use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::DoneClearOn;
use crate::daemon::protocol::v2::HookHealth;
use crate::daemon::protocol::v2::{
    ClientMessage as V2ClientMessage, PROTOCOL_VERSION, ServerMessage as V2ServerMessage, V2Client,
    V2RequestError, V2RequestFailureStage,
};
use crate::daemon::topology::ServerIdentity;
use crate::pane_state::store::{server_guarded_command_args, tmux_command_string};
use crate::pane_state::{
    ClientWitness, DaemonInstanceId, EventId, PaneInstance, PaneState, SourceClientHint, StateId,
    StoredPaneRecord, ViewEvent, ViewHookKind, ViewOccurrence, VisibilitySnapshot,
};
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

pub const HOOK_INDEX: u8 = 70;
pub const HOOK_OWNER: &str = "vde-tmux-pane-state";
pub const HOOK_PROTOCOL: u16 = 2;
pub const HOOK_MONITOR_INTERVAL_SECONDS: u64 = 10;
pub const VIEW_HOOK_DEADLINE_MILLIS: u64 = 500;
pub const VIEW_HOOK_MAX_ATTEMPTS: u8 = 3;
pub const FRESH_VISIBILITY_TIMEOUT: Duration = Duration::from_millis(250);
pub const MAX_CLIENT_WITNESSES: usize = 64;

const HOOKS: [(ViewHookKind, &str); 5] = [
    (ViewHookKind::WindowPaneChanged, "window-pane-changed"),
    (ViewHookKind::SessionWindowChanged, "session-window-changed"),
    (ViewHookKind::ClientSessionChanged, "client-session-changed"),
    (ViewHookKind::ClientAttached, "client-attached"),
    (ViewHookKind::ClientDetached, "client-detached"),
];
const HOOK_IDENTITY_PREFIX: &str = "__vde_hook_identity__";
const HOOK_SERVER_MISMATCH_SENTINEL: &str = "__vde_hook_server_mismatch__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSlotState {
    Missing,
    Owned,
    Foreign(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookInspection {
    pub slots: BTreeMap<String, HookSlotState>,
}

impl HookInspection {
    pub fn health(&self) -> HookHealth {
        if self
            .slots
            .values()
            .all(|slot| matches!(slot, HookSlotState::Owned))
        {
            HookHealth::Healthy
        } else {
            HookHealth::Degraded
        }
    }

    pub fn foreign(&self) -> Option<(&str, &str)> {
        self.slots.iter().find_map(|(hook, state)| match state {
            HookSlotState::Foreign(command) => Some((hook.as_str(), command.as_str())),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookError {
    InvalidQuery(String),
    Collision { hook: String, command: String },
    VerificationFailed(String),
    Tmux(String),
    ServerMismatch,
}

impl fmt::Display for HookError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuery(message)
            | Self::VerificationFailed(message)
            | Self::Tmux(message) => formatter.write_str(message),
            Self::ServerMismatch => formatter.write_str("tmux server incarnation changed"),
            Self::Collision { hook, command } => {
                write!(formatter, "hook collision at {hook}: {command}")
            }
        }
    }
}

impl std::error::Error for HookError {}

pub fn indexed_hook_name(kind: ViewHookKind) -> String {
    let name = HOOKS
        .iter()
        .find_map(|(candidate, name)| (*candidate == kind).then_some(*name))
        .expect("all view hook kinds have a fixed name");
    format!("{name}[{HOOK_INDEX}]")
}

pub fn install_command(kind: ViewHookKind) -> String {
    let name = hook_kind_arg(kind);
    format!(
        "run-shell 'vt hooks pane-state-view {name} --owner {HOOK_OWNER} --protocol {HOOK_PROTOCOL}'"
    )
}

pub fn verified_command(kind: ViewHookKind) -> String {
    let name = hook_kind_arg(kind);
    format!(
        "run-shell \"vt hooks pane-state-view {name} --owner {HOOK_OWNER} --protocol {HOOK_PROTOCOL}\""
    )
}

pub fn hook_query_args() -> Vec<String> {
    let mut args = vec![
        "display-message".to_string(),
        "-p".to_string(),
        format!("{HOOK_IDENTITY_PREFIX}#{{pid}}:#{{start_time}}"),
        ";".to_string(),
    ];
    for (index, (kind, _)) in HOOKS.iter().enumerate() {
        if index > 0 {
            args.push(";".to_string());
        }
        args.extend([
            "show-hooks".to_string(),
            "-g".to_string(),
            indexed_hook_name(*kind),
        ]);
    }
    args
}

pub fn hook_install_args() -> Vec<String> {
    let mut args = Vec::new();
    for (index, (kind, _)) in HOOKS.iter().enumerate() {
        if index > 0 {
            args.push(";".to_string());
        }
        args.extend([
            "set-hook".to_string(),
            "-g".to_string(),
            indexed_hook_name(*kind),
            install_command(*kind),
        ]);
    }
    args
}

pub fn hook_uninstall_args(inspection: &HookInspection) -> Vec<String> {
    let mut args = Vec::new();
    for (kind, _) in HOOKS {
        let hook = indexed_hook_name(kind);
        if !matches!(inspection.slots.get(&hook), Some(HookSlotState::Owned)) {
            continue;
        }
        if !args.is_empty() {
            args.push(";".to_string());
        }
        args.extend(["set-hook".to_string(), "-gu".to_string(), hook]);
    }
    args
}

pub fn inspect_hook_output(output: &str) -> Result<HookInspection, HookError> {
    let lines = output.lines().collect::<Vec<_>>();
    if lines.len() != HOOKS.len() {
        return Err(HookError::InvalidQuery(format!(
            "hook query returned {} lines, expected {}",
            lines.len(),
            HOOKS.len()
        )));
    }
    let mut slots = BTreeMap::new();
    for ((kind, _), line) in HOOKS.iter().zip(lines) {
        let hook = indexed_hook_name(*kind);
        let Some(command) = line.strip_prefix(&hook) else {
            return Err(HookError::InvalidQuery(format!(
                "hook query line does not start with {hook:?}"
            )));
        };
        if !command.is_empty() && !command.starts_with(char::is_whitespace) {
            return Err(HookError::InvalidQuery(format!(
                "hook query line has an invalid command separator for {hook}"
            )));
        }
        let command = command.trim();
        let state = if command.is_empty() {
            HookSlotState::Missing
        } else if command == verified_command(*kind) {
            HookSlotState::Owned
        } else {
            HookSlotState::Foreign(command.to_string())
        };
        slots.insert(hook, state);
    }
    Ok(HookInspection { slots })
}

pub fn preflight_hooks(
    runner: &dyn TmuxRunner,
    expected_identity: &ServerIdentity,
) -> Result<HookInspection, HookError> {
    let inspection = query_hooks(runner, expected_identity)?;
    reject_foreign(&inspection)?;
    Ok(inspection)
}

pub fn install_hooks(
    runner: &dyn TmuxRunner,
    expected_identity: &ServerIdentity,
) -> Result<(), HookError> {
    preflight_hooks(runner, expected_identity)?;
    let guarded = server_guarded_command_args(
        expected_identity.pid,
        expected_identity.start_time,
        tmux_command_string(&hook_install_args()),
        HOOK_SERVER_MISMATCH_SENTINEL,
    );
    let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
    let output = runner
        .run(&refs)
        .map_err(|error| HookError::Tmux(error.to_string()))?;
    if output.trim() == HOOK_SERVER_MISMATCH_SENTINEL {
        return Err(HookError::ServerMismatch);
    }
    let inspection = query_hooks(runner, expected_identity)?;
    if inspection.health() != HookHealth::Healthy {
        return Err(HookError::VerificationFailed(
            "owned view hooks failed post-install verification".to_string(),
        ));
    }
    Ok(())
}

pub fn monitor_hooks(
    runner: &dyn TmuxRunner,
    expected_identity: &ServerIdentity,
) -> Result<HookHealth, HookError> {
    Ok(query_hooks(runner, expected_identity)?.health())
}

pub fn uninstall_hooks(
    runner: &dyn TmuxRunner,
    expected_identity: &ServerIdentity,
) -> Result<(), HookError> {
    let inspection = query_hooks(runner, expected_identity)?;
    let foreign = inspection
        .foreign()
        .map(|(hook, command)| (hook.to_string(), command.to_string()));
    let args = hook_uninstall_args(&inspection);
    if !args.is_empty() {
        let guarded = server_guarded_command_args(
            expected_identity.pid,
            expected_identity.start_time,
            tmux_command_string(&args),
            HOOK_SERVER_MISMATCH_SENTINEL,
        );
        let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
        let output = runner
            .run(&refs)
            .map_err(|error| HookError::Tmux(error.to_string()))?;
        if output.trim() == HOOK_SERVER_MISMATCH_SENTINEL {
            return Err(HookError::ServerMismatch);
        }
    }
    let after = query_hooks(runner, expected_identity)?;
    for (hook, before) in &inspection.slots {
        if matches!(before, HookSlotState::Owned)
            && !matches!(after.slots.get(hook), Some(HookSlotState::Missing))
        {
            return Err(HookError::VerificationFailed(format!(
                "owned view hook {hook} failed uninstall verification"
            )));
        }
    }
    if let Some((hook, command)) = foreign {
        return Err(HookError::Collision { hook, command });
    }
    Ok(())
}

fn query_hooks(
    runner: &dyn TmuxRunner,
    expected_identity: &ServerIdentity,
) -> Result<HookInspection, HookError> {
    let args = hook_query_args();
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = runner
        .run(&refs)
        .map_err(|error| HookError::Tmux(error.to_string()))?;
    let (identity, hooks) = output
        .split_once('\n')
        .ok_or_else(|| HookError::InvalidQuery("missing hook identity header".to_string()))?;
    let actual = identity
        .strip_suffix('\r')
        .unwrap_or(identity)
        .strip_prefix(HOOK_IDENTITY_PREFIX)
        .and_then(|value| value.split_once(':'))
        .and_then(|(pid, start_time)| {
            Some(ServerIdentity {
                pid: pid.parse().ok()?,
                start_time: start_time.parse().ok()?,
            })
        })
        .ok_or_else(|| HookError::InvalidQuery("invalid hook identity header".to_string()))?;
    if &actual != expected_identity {
        return Err(HookError::ServerMismatch);
    }
    inspect_hook_output(hooks)
}

fn reject_foreign(inspection: &HookInspection) -> Result<(), HookError> {
    if let Some((hook, command)) = inspection.foreign() {
        Err(HookError::Collision {
            hook: hook.to_string(),
            command: command.to_string(),
        })
    } else {
        Ok(())
    }
}

fn hook_kind_arg(kind: ViewHookKind) -> &'static str {
    match kind {
        ViewHookKind::WindowPaneChanged => "window-pane-changed",
        ViewHookKind::SessionWindowChanged => "session-window-changed",
        ViewHookKind::ClientSessionChanged => "client-session-changed",
        ViewHookKind::ClientAttached => "client-attached",
        ViewHookKind::ClientDetached => "client-detached",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewRegistry {
    clients: BTreeMap<u32, ClientWitness>,
    windows: BTreeMap<String, WindowView>,
    sessions: BTreeMap<String, (String, PaneInstance)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowView {
    pub active_pane: PaneInstance,
    pub observed_panes: Vec<PaneInstance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedViewRefresh {
    QueryFailed,
    Window {
        window_id: String,
        active_pane: PaneInstance,
        observed_panes: Vec<PaneInstance>,
    },
    Session {
        session_id: String,
        window_id: String,
        active_pane: PaneInstance,
        observed_panes: Vec<PaneInstance>,
    },
    Client {
        witness: ClientWitness,
        observed_panes: Vec<PaneInstance>,
    },
    ClientAbsent {
        client_pid: u32,
    },
}

impl ViewRegistry {
    pub fn clients(&self) -> &BTreeMap<u32, ClientWitness> {
        &self.clients
    }

    pub fn windows(&self) -> &BTreeMap<String, WindowView> {
        &self.windows
    }

    pub fn sessions(&self) -> &BTreeMap<String, (String, PaneInstance)> {
        &self.sessions
    }

    pub fn reconcile(
        &mut self,
        witnesses: &[ClientWitness],
        window_panes: &BTreeMap<String, Vec<PaneInstance>>,
    ) -> Result<bool, ViewError> {
        validate_witnesses(witnesses)?;
        let next_clients = witnesses
            .iter()
            .cloned()
            .map(|witness| (witness.client_pid, witness))
            .collect::<BTreeMap<_, _>>();
        let mut next_windows = BTreeMap::new();
        let mut next_sessions = BTreeMap::new();
        for witness in witnesses {
            let observed_panes = window_panes.get(&witness.window_id).ok_or_else(|| {
                ViewError::InvalidEvent("client window is missing from topology".to_string())
            })?;
            validate_window_view(&witness.active_pane, observed_panes)?;
            let window_view = WindowView {
                active_pane: witness.active_pane.clone(),
                observed_panes: observed_panes.clone(),
            };
            if next_windows
                .insert(witness.window_id.clone(), window_view.clone())
                .is_some_and(|view| view != window_view)
            {
                return Err(ViewError::InvalidEvent(
                    "clients disagree about a window active pane".to_string(),
                ));
            }
            if next_sessions
                .insert(
                    witness.session_id.clone(),
                    (witness.window_id.clone(), witness.active_pane.clone()),
                )
                .is_some_and(|value| {
                    value != (witness.window_id.clone(), witness.active_pane.clone())
                })
            {
                return Err(ViewError::InvalidEvent(
                    "clients disagree about a session active window".to_string(),
                ));
            }
        }
        if self.clients == next_clients
            && self.windows == next_windows
            && self.sessions == next_sessions
        {
            Ok(false)
        } else {
            self.clients = next_clients;
            self.windows = next_windows;
            self.sessions = next_sessions;
            Ok(true)
        }
    }

    pub fn apply_scoped_refresh(
        &mut self,
        kind: ViewHookKind,
        refreshed: ScopedViewRefresh,
    ) -> Result<bool, ViewError> {
        match (kind, refreshed) {
            (_, ScopedViewRefresh::QueryFailed) => Ok(false),
            (
                ViewHookKind::WindowPaneChanged,
                ScopedViewRefresh::Window {
                    window_id,
                    active_pane,
                    observed_panes,
                },
            ) => {
                validate_tmux_id(&window_id, '@', "window")?;
                validate_window_view(&active_pane, &observed_panes)?;
                let before = self.clone();
                self.windows.insert(
                    window_id.clone(),
                    WindowView {
                        active_pane: active_pane.clone(),
                        observed_panes,
                    },
                );
                for witness in self
                    .clients
                    .values_mut()
                    .filter(|witness| witness.window_id == window_id)
                {
                    witness.active_pane = active_pane.clone();
                }
                for (session_window, session_pane) in self.sessions.values_mut() {
                    if session_window == &window_id {
                        *session_pane = active_pane.clone();
                    }
                }
                Ok(*self != before)
            }
            (
                ViewHookKind::SessionWindowChanged,
                ScopedViewRefresh::Session {
                    session_id,
                    window_id,
                    active_pane,
                    observed_panes,
                },
            ) => {
                validate_tmux_id(&session_id, '$', "session")?;
                validate_tmux_id(&window_id, '@', "window")?;
                validate_window_view(&active_pane, &observed_panes)?;
                let before = self.clone();
                let next = (window_id.clone(), active_pane.clone());
                self.sessions.insert(session_id.clone(), next);
                self.windows.insert(
                    window_id.clone(),
                    WindowView {
                        active_pane: active_pane.clone(),
                        observed_panes,
                    },
                );
                for (session_window, session_pane) in self.sessions.values_mut() {
                    if session_window == &window_id {
                        *session_pane = active_pane.clone();
                    }
                }
                for witness in self.clients.values_mut().filter(|witness| {
                    witness.session_id == session_id || witness.window_id == window_id
                }) {
                    if witness.session_id == session_id {
                        witness.window_id = window_id.clone();
                    }
                    witness.active_pane = active_pane.clone();
                }
                Ok(*self != before)
            }
            (
                ViewHookKind::ClientSessionChanged | ViewHookKind::ClientAttached,
                ScopedViewRefresh::Client {
                    witness,
                    observed_panes,
                },
            )
            | (
                ViewHookKind::ClientDetached,
                ScopedViewRefresh::Client {
                    witness,
                    observed_panes,
                },
            ) => {
                validate_witnesses(std::slice::from_ref(&witness))?;
                validate_window_view(&witness.active_pane, &observed_panes)?;
                let before = self.clone();
                self.windows.insert(
                    witness.window_id.clone(),
                    WindowView {
                        active_pane: witness.active_pane.clone(),
                        observed_panes,
                    },
                );
                self.sessions.insert(
                    witness.session_id.clone(),
                    (witness.window_id.clone(), witness.active_pane.clone()),
                );
                for (session_window, session_pane) in self.sessions.values_mut() {
                    if session_window == &witness.window_id {
                        *session_pane = witness.active_pane.clone();
                    }
                }
                for related in self
                    .clients
                    .values_mut()
                    .filter(|related| related.window_id == witness.window_id)
                {
                    related.active_pane = witness.active_pane.clone();
                }
                self.clients.insert(witness.client_pid, witness);
                Ok(*self != before)
            }
            (ViewHookKind::ClientDetached, ScopedViewRefresh::ClientAbsent { client_pid }) => {
                Ok(self.clients.remove(&client_pid).is_some())
            }
            _ => Err(ViewError::InvalidEvent(
                "scoped refresh does not match hook kind".to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcknowledgementIntent {
    pub pane_instance: PaneInstance,
    pub expected_state_id: StateId,
    pub expected_agent_epoch: u64,
    pub through_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewProcessingResult {
    pub registry_changed: bool,
    pub acknowledgements: Vec<crate::pane_state::PaneEventEnvelope>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewError {
    InvalidEvent(String),
    UnverifiedOccurrence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltForegroundViewEvent {
    pub event: ViewEvent,
    pub unverified_occurrence: bool,
}

pub fn build_foreground_view_event(
    daemon_instance_id: DaemonInstanceId,
    event_id: EventId,
    hook_kind: ViewHookKind,
    occurrence: Option<ViewOccurrence>,
    source_client: Option<SourceClientHint>,
    witnesses: Vec<ClientWitness>,
    done_clear_on: DoneClearOn,
) -> Result<BuiltForegroundViewEvent, ViewError> {
    validate_witnesses(&witnesses)?;
    let mut event = ViewEvent {
        daemon_instance_id,
        event_id,
        hook_kind,
        occurrence,
        source_client,
        witnesses,
    };
    event
        .validate()
        .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    if let Some(occurrence) = &event.occurrence {
        validate_occurrence(occurrence)?;
    }
    let occurrence_verified = event.occurrence.as_ref().is_some_and(|occurrence| {
        validate_occurrence(occurrence).is_ok()
            && event
                .witnesses
                .iter()
                .filter(|witness| witness.is_eligible())
                .any(|witness| match done_clear_on {
                    DoneClearOn::Pane => {
                        witness.window_id == occurrence.window_id
                            && witness.active_pane == occurrence.active_pane
                    }
                    DoneClearOn::Window => {
                        witness.window_id == occurrence.window_id
                            && occurrence.observed_panes.contains(&witness.active_pane)
                    }
                })
    });
    let unverified_occurrence = event.occurrence.is_some() && !occurrence_verified;
    if !occurrence_verified {
        event.occurrence = None;
    }
    Ok(BuiltForegroundViewEvent {
        event,
        unverified_occurrence,
    })
}

impl fmt::Display for ViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEvent(message) => formatter.write_str(message),
            Self::UnverifiedOccurrence => formatter.write_str("unverified view occurrence"),
        }
    }
}

impl std::error::Error for ViewError {}

pub fn acknowledgement_intents(
    event: &ViewEvent,
    done_clear_on: DoneClearOn,
    records: &BTreeMap<PaneInstance, StoredPaneRecord>,
) -> Result<Vec<AcknowledgementIntent>, ViewError> {
    event
        .validate()
        .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    validate_witnesses(&event.witnesses)?;
    let Some(occurrence) = event.occurrence.as_ref() else {
        return Ok(Vec::new());
    };
    validate_occurrence(occurrence)?;
    if event.witnesses.iter().any(|witness| {
        witness.window_id == occurrence.window_id
            && !occurrence.observed_panes.contains(&witness.active_pane)
    }) {
        return Err(ViewError::InvalidEvent(
            "client witness active pane is not a member of the declared window".to_string(),
        ));
    }
    let verified = event
        .witnesses
        .iter()
        .filter(|witness| witness.is_eligible())
        .any(|witness| match done_clear_on {
            DoneClearOn::Pane => {
                witness.window_id == occurrence.window_id
                    && witness.active_pane == occurrence.active_pane
            }
            DoneClearOn::Window => witness.window_id == occurrence.window_id,
        });
    if !verified {
        return Err(ViewError::UnverifiedOccurrence);
    }
    let targets = match done_clear_on {
        DoneClearOn::Pane => std::slice::from_ref(&occurrence.active_pane),
        DoneClearOn::Window => occurrence.observed_panes.as_slice(),
    };
    let mut intents = Vec::new();
    for pane in targets {
        let Some(StoredPaneRecord::Active(state)) = records.get(pane) else {
            continue;
        };
        intents.push(intent_for_state(pane, state));
    }
    intents.sort_by(|left, right| left.pane_instance.cmp(&right.pane_instance));
    Ok(intents)
}

pub fn process_view_event(
    registry: &mut ViewRegistry,
    event: &ViewEvent,
    scoped_refresh: ScopedViewRefresh,
    done_clear_on: DoneClearOn,
    records: &BTreeMap<PaneInstance, StoredPaneRecord>,
) -> Result<ViewProcessingResult, ViewError> {
    event
        .validate()
        .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    let query_failed = matches!(scoped_refresh, ScopedViewRefresh::QueryFailed);
    let registry_changed = registry.apply_scoped_refresh(event.hook_kind, scoped_refresh)?;
    let mut diagnostics = Vec::new();
    if query_failed {
        diagnostics.push("view_registry_scoped_query_failed".to_string());
    }
    if event.occurrence.is_none() && event.hook_kind != ViewHookKind::ClientDetached {
        diagnostics.push("unverified_view_occurrence".to_string());
    }
    let intents = match acknowledgement_intents(event, done_clear_on, records) {
        Ok(intents) => intents,
        Err(ViewError::UnverifiedOccurrence) => {
            diagnostics.push("unverified_view_occurrence".to_string());
            Vec::new()
        }
        Err(error) => return Err(error),
    };
    diagnostics.sort();
    diagnostics.dedup();
    Ok(ViewProcessingResult {
        registry_changed,
        acknowledgements: acknowledgement_envelopes(
            &event.daemon_instance_id,
            &event.event_id,
            intents,
        ),
        diagnostics,
    })
}

pub fn reconcile_current_views(
    registry: &mut ViewRegistry,
    daemon_instance_id: &DaemonInstanceId,
    witnesses: &[ClientWitness],
    window_panes: &BTreeMap<String, Vec<PaneInstance>>,
    done_clear_on: DoneClearOn,
    records: &BTreeMap<PaneInstance, StoredPaneRecord>,
) -> Result<ViewProcessingResult, ViewError> {
    let registry_changed = registry.reconcile(witnesses, window_panes)?;
    let event_id =
        EventId::generate().map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    let mut intents = BTreeMap::<PaneInstance, AcknowledgementIntent>::new();
    for witness in witnesses.iter().filter(|witness| witness.is_eligible()) {
        let observed_panes = window_panes.get(&witness.window_id).ok_or_else(|| {
            ViewError::InvalidEvent("client window is missing from topology".to_string())
        })?;
        let event = ViewEvent {
            daemon_instance_id: daemon_instance_id.clone(),
            event_id: event_id.clone(),
            hook_kind: ViewHookKind::WindowPaneChanged,
            occurrence: Some(ViewOccurrence {
                session_id: witness.session_id.clone(),
                window_id: witness.window_id.clone(),
                active_pane: witness.active_pane.clone(),
                observed_panes: observed_panes.clone(),
            }),
            source_client: Some(SourceClientHint {
                client_pid: witness.client_pid,
            }),
            witnesses: witnesses.to_vec(),
        };
        for intent in acknowledgement_intents(&event, done_clear_on, records)? {
            intents
                .entry(intent.pane_instance.clone())
                .or_insert(intent);
        }
    }
    Ok(ViewProcessingResult {
        registry_changed,
        acknowledgements: acknowledgement_envelopes(
            daemon_instance_id,
            &event_id,
            intents.into_values().collect(),
        ),
        diagnostics: Vec::new(),
    })
}

fn acknowledgement_envelopes(
    daemon_instance_id: &DaemonInstanceId,
    event_id: &EventId,
    intents: Vec<AcknowledgementIntent>,
) -> Vec<crate::pane_state::PaneEventEnvelope> {
    intents
        .into_iter()
        .map(|intent| crate::pane_state::PaneEventEnvelope {
            daemon_instance_id: daemon_instance_id.clone(),
            event_id: event_id.clone(),
            pane_instance: intent.pane_instance,
            agent: None,
            agent_session_id: None,
            event: crate::pane_state::PaneEvent::AcknowledgeView {
                expected_state_id: intent.expected_state_id,
                expected_agent_epoch: intent.expected_agent_epoch,
                through_seq: intent.through_seq,
            },
        })
        .collect()
}

pub fn visibility_snapshot(pane: &PaneInstance, witnesses: &[ClientWitness]) -> VisibilitySnapshot {
    VisibilitySnapshot {
        pane_visible_to_eligible_client: witnesses
            .iter()
            .any(|witness| witness.is_eligible() && witness.active_pane == *pane),
    }
}

fn intent_for_state(pane: &PaneInstance, state: &PaneState) -> AcknowledgementIntent {
    AcknowledgementIntent {
        pane_instance: pane.clone(),
        expected_state_id: state.state_id.clone(),
        expected_agent_epoch: state.agent_epoch,
        through_seq: state.completed_seq,
    }
}

fn validate_witnesses(witnesses: &[ClientWitness]) -> Result<(), ViewError> {
    if witnesses.len() > crate::pane_state::MAX_VIEW_WITNESSES {
        return Err(ViewError::InvalidEvent(
            "too many client witnesses".to_string(),
        ));
    }
    let mut pids = BTreeSet::new();
    for witness in witnesses {
        if witness.client_pid == 0 || !pids.insert(witness.client_pid) {
            return Err(ViewError::InvalidEvent(
                "invalid or duplicate client witness".to_string(),
            ));
        }
        validate_tmux_id(&witness.session_id, '$', "witness session")?;
        validate_tmux_id(&witness.window_id, '@', "witness window")?;
        witness
            .active_pane
            .validate()
            .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    }
    Ok(())
}

fn validate_occurrence(occurrence: &ViewOccurrence) -> Result<(), ViewError> {
    validate_tmux_id(&occurrence.session_id, '$', "occurrence session")?;
    validate_tmux_id(&occurrence.window_id, '@', "occurrence window")?;
    Ok(())
}

fn validate_window_view(
    active_pane: &PaneInstance,
    observed_panes: &[PaneInstance],
) -> Result<(), ViewError> {
    if observed_panes.len() > crate::pane_state::MAX_VIEW_PANES {
        return Err(ViewError::InvalidEvent(
            "too many panes in window view".to_string(),
        ));
    }
    active_pane
        .validate()
        .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    for pane in observed_panes {
        pane.validate()
            .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    }
    let panes = observed_panes.iter().collect::<BTreeSet<_>>();
    if panes.len() != observed_panes.len() || !panes.contains(active_pane) {
        return Err(ViewError::InvalidEvent(
            "invalid panes in window view".to_string(),
        ));
    }
    Ok(())
}

fn validate_tmux_id(value: &str, prefix: char, field: &str) -> Result<(), ViewError> {
    if value.strip_prefix(prefix).is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(())
    } else {
        Err(ViewError::InvalidEvent(format!("invalid {field} ID")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFailureStage {
    BeforeFullWrite,
    AfterFullWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewDeliveryContract {
    attempts: u8,
    full_write_completed: bool,
}

impl ViewDeliveryContract {
    pub fn begin_attempt(&mut self) -> bool {
        if self.full_write_completed || self.attempts >= VIEW_HOOK_MAX_ATTEMPTS {
            return false;
        }
        self.attempts += 1;
        true
    }

    pub fn record_full_write(&mut self) {
        self.full_write_completed = true;
    }

    pub fn may_retry(&self, stage: DeliveryFailureStage) -> bool {
        stage == DeliveryFailureStage::BeforeFullWrite
            && !self.full_write_completed
            && self.attempts < VIEW_HOOK_MAX_ATTEMPTS
    }
}

pub trait ViewEventSender {
    fn send(
        &mut self,
        event: &ViewEvent,
        deadline: Instant,
    ) -> std::result::Result<V2ServerMessage, V2RequestError>;
}

pub struct SocketViewEventSender<'a> {
    pub socket: &'a Path,
    pub server_identity: &'a str,
}

impl ViewEventSender for SocketViewEventSender<'_> {
    fn send(
        &mut self,
        event: &ViewEvent,
        deadline: Instant,
    ) -> std::result::Result<V2ServerMessage, V2RequestError> {
        let mut client =
            V2Client::connect_with_deadline(self.socket, self.server_identity, deadline).map_err(
                |error| V2RequestError {
                    stage: V2RequestFailureStage::BeforeFullWrite,
                    message: error.to_string(),
                },
            )?;
        if client.daemon_instance_id() != &event.daemon_instance_id {
            return Err(V2RequestError {
                stage: V2RequestFailureStage::BeforeFullWrite,
                message: "view event targets a stale daemon instance".to_string(),
            });
        }
        client.request_with_stage(&V2ClientMessage::SubmitViewEvent {
            proto: PROTOCOL_VERSION,
            event: event.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewDeliveryFailureStage {
    BeforeFullWrite,
    AfterFullWrite,
    Deadline,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDeliveryError {
    pub event_id: EventId,
    pub stage: ViewDeliveryFailureStage,
    pub message: String,
}

impl fmt::Display for ViewDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "view event {} failed at {:?}: {}",
            self.event_id.as_str(),
            self.stage,
            self.message
        )
    }
}

impl std::error::Error for ViewDeliveryError {}

pub fn deliver_view_event(
    socket: &Path,
    server_identity: &str,
    event: &ViewEvent,
    deadline: Instant,
) -> Result<V2ServerMessage, ViewDeliveryError> {
    deliver_view_event_with(
        &mut SocketViewEventSender {
            socket,
            server_identity,
        },
        event,
        deadline,
    )
}

pub fn deliver_view_event_with(
    sender: &mut dyn ViewEventSender,
    event: &ViewEvent,
    deadline: Instant,
) -> Result<V2ServerMessage, ViewDeliveryError> {
    let mut delivery = ViewDeliveryContract::default();
    while delivery.begin_attempt() {
        if Instant::now() >= deadline {
            return Err(ViewDeliveryError {
                event_id: event.event_id.clone(),
                stage: ViewDeliveryFailureStage::Deadline,
                message: "view hook 500ms deadline exceeded".to_string(),
            });
        }
        match sender.send(event, deadline) {
            Ok(response @ V2ServerMessage::ViewQueued { .. })
            | Ok(response @ V2ServerMessage::ViewResult { .. }) => {
                let response_event_id = match &response {
                    V2ServerMessage::ViewQueued { event_id, .. }
                    | V2ServerMessage::ViewResult { event_id, .. } => event_id,
                    _ => unreachable!("view response pattern already matched"),
                };
                if response_event_id == &event.event_id {
                    return Ok(response);
                }
                return Err(ViewDeliveryError {
                    event_id: event.event_id.clone(),
                    stage: ViewDeliveryFailureStage::Rejected,
                    message: "view response event_id mismatch".to_string(),
                });
            }
            Ok(V2ServerMessage::Error { code, message, .. }) => {
                return Err(ViewDeliveryError {
                    event_id: event.event_id.clone(),
                    stage: ViewDeliveryFailureStage::Rejected,
                    message: format!("{code:?}: {message}"),
                });
            }
            Ok(other) => {
                return Err(ViewDeliveryError {
                    event_id: event.event_id.clone(),
                    stage: ViewDeliveryFailureStage::Rejected,
                    message: format!("unexpected view response: {other:?}"),
                });
            }
            Err(error) if error.stage == V2RequestFailureStage::AfterFullWrite => {
                delivery.record_full_write();
                return Err(ViewDeliveryError {
                    event_id: event.event_id.clone(),
                    stage: ViewDeliveryFailureStage::AfterFullWrite,
                    message: format!("ambiguous_view_delivery: {}", error.message),
                });
            }
            Err(error) => {
                if !delivery.may_retry(DeliveryFailureStage::BeforeFullWrite) {
                    return Err(ViewDeliveryError {
                        event_id: event.event_id.clone(),
                        stage: ViewDeliveryFailureStage::BeforeFullWrite,
                        message: error.message,
                    });
                }
            }
        }
    }
    Err(ViewDeliveryError {
        event_id: event.event_id.clone(),
        stage: ViewDeliveryFailureStage::BeforeFullWrite,
        message: "view hook delivery attempts exhausted".to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshVisibilityError {
    Query(String),
    Parse(String),
    IdentityMismatch {
        expected: ServerIdentity,
        actual: ServerIdentity,
    },
}

impl FreshVisibilityError {
    pub fn requires_daemon_exit(&self) -> bool {
        matches!(self, Self::IdentityMismatch { .. })
    }
}

impl fmt::Display for FreshVisibilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(message) | Self::Parse(message) => formatter.write_str(message),
            Self::IdentityMismatch { expected, actual } => write!(
                formatter,
                "tmux server identity mismatch: expected {}:{}, received {}:{}",
                expected.pid, expected.start_time, actual.pid, actual.start_time
            ),
        }
    }
}

impl std::error::Error for FreshVisibilityError {}

pub trait FreshVisibilityIo {
    fn query_witnesses(
        &self,
        timeout: Duration,
    ) -> Result<Vec<ClientWitness>, FreshVisibilityError>;
}

#[derive(Debug, Clone)]
pub struct SystemFreshVisibilityIo {
    socket_name: Option<String>,
    expected_identity: ServerIdentity,
}

impl SystemFreshVisibilityIo {
    pub fn new(socket_name: Option<String>, expected_identity: ServerIdentity) -> Self {
        Self {
            socket_name,
            expected_identity,
        }
    }
}

impl FreshVisibilityIo for SystemFreshVisibilityIo {
    fn query_witnesses(
        &self,
        timeout: Duration,
    ) -> Result<Vec<ClientWitness>, FreshVisibilityError> {
        let token = EventId::generate()
            .map_err(|error| FreshVisibilityError::Query(error.to_string()))?
            .as_str()
            .to_string();
        let args = client_view_query_args(&token);
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let runner = match &self.socket_name {
            Some(socket_name) => {
                SystemTmuxRunner::with_socket_name(socket_name.clone(), Some(timeout))
            }
            None => SystemTmuxRunner::with_timeout(timeout),
        };
        let output = runner
            .run(&refs)
            .map_err(|error| FreshVisibilityError::Query(error.to_string()))?;
        parse_client_view_query(&output, &token, &self.expected_identity)
    }
}

pub fn query_fresh_visibility(
    io: &dyn FreshVisibilityIo,
    pane: &PaneInstance,
) -> Result<VisibilitySnapshot, FreshVisibilityError> {
    let witnesses = io.query_witnesses(FRESH_VISIBILITY_TIMEOUT)?;
    validate_witnesses(&witnesses)
        .map_err(|error| FreshVisibilityError::Parse(error.to_string()))?;
    Ok(visibility_snapshot(pane, &witnesses))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionVisibility {
    pub snapshot: VisibilitySnapshot,
    pub diagnostic: Option<String>,
}

pub fn completion_visibility(
    io: &dyn FreshVisibilityIo,
    pane: &PaneInstance,
) -> Result<CompletionVisibility, FreshVisibilityError> {
    match query_fresh_visibility(io, pane) {
        Ok(snapshot) => Ok(CompletionVisibility {
            snapshot,
            diagnostic: None,
        }),
        Err(error) if error.requires_daemon_exit() => Err(error),
        Err(error) => Ok(CompletionVisibility {
            snapshot: VisibilitySnapshot::default(),
            diagnostic: Some(format!("fresh_visibility_unavailable: {error}")),
        }),
    }
}

pub fn client_view_query_args(token: &str) -> Vec<String> {
    let field = format!("__vde_client_field_{token}__");
    let row = format!("__vde_client_row_{token}__");
    vec![
        "display-message".to_string(),
        "-p".to_string(),
        format!("__vde_client_identity_{token}__#{{pid}}:#{{start_time}}"),
        ";".to_string(),
        "list-clients".to_string(),
        "-F".to_string(),
        format!(
            "#{{client_pid}}{field}#{{session_id}}{field}#{{window_id}}{field}#{{pane_id}}{field}#{{pane_pid}}{field}#{{client_control_mode}}{field}#{{client_flags}}{row}"
        ),
    ]
}

pub fn parse_client_view_query(
    output: &str,
    token: &str,
    expected_identity: &ServerIdentity,
) -> Result<Vec<ClientWitness>, FreshVisibilityError> {
    let (identity_line, body) = output.split_once('\n').ok_or_else(|| {
        FreshVisibilityError::Parse("missing client query identity header".to_string())
    })?;
    let identity_prefix = format!("__vde_client_identity_{token}__");
    let actual = identity_line
        .strip_suffix('\r')
        .unwrap_or(identity_line)
        .strip_prefix(&identity_prefix)
        .and_then(|value| value.split_once(':'))
        .and_then(|(pid, start_time)| {
            Some(ServerIdentity {
                pid: pid.parse().ok()?,
                start_time: start_time.parse().ok()?,
            })
        })
        .ok_or_else(|| FreshVisibilityError::Parse("invalid client query identity".to_string()))?;
    if &actual != expected_identity {
        return Err(FreshVisibilityError::IdentityMismatch {
            expected: expected_identity.clone(),
            actual,
        });
    }
    let field = format!("__vde_client_field_{token}__");
    let row = format!("__vde_client_row_{token}__");
    let rows = parse_client_rows(body, &row)?;
    if rows.len() > MAX_CLIENT_WITNESSES {
        return Err(FreshVisibilityError::Parse(
            "too many client witnesses".to_string(),
        ));
    }
    let mut witnesses = Vec::with_capacity(rows.len());
    for row_value in rows {
        let values = row_value.split(&field).collect::<Vec<_>>();
        if values.len() != 7 {
            return Err(FreshVisibilityError::Parse(
                "client query row has an invalid field count".to_string(),
            ));
        }
        let client_pid = values[0]
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| FreshVisibilityError::Parse("invalid client PID".to_string()))?;
        let pane_pid = values[4]
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| FreshVisibilityError::Parse("invalid client pane PID".to_string()))?;
        let control_mode = match values[5] {
            "0" => false,
            "1" => true,
            _ => {
                return Err(FreshVisibilityError::Parse(
                    "invalid client control mode".to_string(),
                ));
            }
        };
        witnesses.push(ClientWitness {
            client_pid,
            session_id: values[1].to_string(),
            window_id: values[2].to_string(),
            active_pane: PaneInstance {
                pane_id: values[3].to_string(),
                pane_pid,
            },
            control_mode,
            active_pane_flag: values[6].split(',').any(|flag| flag == "active-pane"),
        });
    }
    validate_witnesses(&witnesses)
        .map_err(|error| FreshVisibilityError::Parse(error.to_string()))?;
    Ok(witnesses)
}

fn parse_client_rows<'a>(
    mut body: &'a str,
    row_separator: &str,
) -> Result<Vec<&'a str>, FreshVisibilityError> {
    let mut rows = Vec::new();
    while !body.is_empty() {
        let Some(index) = body.find(row_separator) else {
            return Err(FreshVisibilityError::Parse(
                "unterminated client query row".to_string(),
            ));
        };
        rows.push(&body[..index]);
        body = &body[index + row_separator.len()..];
        if let Some(rest) = body.strip_prefix("\r\n") {
            body = rest;
        } else if let Some(rest) = body.strip_prefix('\n') {
            body = rest;
        } else if !body.is_empty() {
            return Err(FreshVisibilityError::Parse(
                "invalid client query row terminator".to_string(),
            ));
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_state::{
        AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneState, StateId, TaskState,
    };
    use crate::tmux::mock::MockTmuxRunner;
    use std::collections::VecDeque;

    struct MockViewSender {
        responses: VecDeque<std::result::Result<V2ServerMessage, V2RequestError>>,
        event_ids: Vec<EventId>,
    }

    impl ViewEventSender for MockViewSender {
        fn send(
            &mut self,
            event: &ViewEvent,
            _deadline: Instant,
        ) -> std::result::Result<V2ServerMessage, V2RequestError> {
            self.event_ids.push(event.event_id.clone());
            self.responses.pop_front().unwrap()
        }
    }

    struct MockFreshVisibility {
        witnesses: Result<Vec<ClientWitness>, FreshVisibilityError>,
    }

    impl FreshVisibilityIo for MockFreshVisibility {
        fn query_witnesses(
            &self,
            timeout: Duration,
        ) -> Result<Vec<ClientWitness>, FreshVisibilityError> {
            assert_eq!(timeout, FRESH_VISIBILITY_TIMEOUT);
            self.witnesses.clone()
        }
    }

    #[derive(Default)]
    struct MemoryStoreIo;

    impl crate::pane_state::PaneStateStoreIo for MemoryStoreIo {
        fn write_candidate(
            &mut self,
            _pane: &PaneInstance,
            candidate: &str,
        ) -> crate::pane_state::WriteAttempt {
            crate::pane_state::WriteAttempt::ReadBack(Some(candidate.to_string()))
        }

        fn read_independent(&mut self, _pane: &PaneInstance) -> crate::pane_state::IndependentRead {
            crate::pane_state::IndependentRead::Unavailable("unused".to_string())
        }
    }

    #[derive(Default)]
    struct MemoryClock;

    impl crate::pane_state::RecoveryClock for MemoryClock {
        fn elapsed(&self) -> Duration {
            Duration::ZERO
        }
    }

    fn hook_output(state: impl Fn(ViewHookKind) -> Option<String>) -> String {
        HOOKS
            .iter()
            .map(|(kind, _)| {
                format!(
                    "{} {}",
                    indexed_hook_name(*kind),
                    state(*kind).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn hook_identity() -> ServerIdentity {
        ServerIdentity {
            pid: 42,
            start_time: 99,
        }
    }

    fn hook_query_output(state: impl Fn(ViewHookKind) -> Option<String>) -> String {
        format!(
            "{HOOK_IDENTITY_PREFIX}{}:{}\n{}",
            hook_identity().pid,
            hook_identity().start_time,
            hook_output(state)
        )
    }

    fn pane(id: &str, pid: u32) -> PaneInstance {
        PaneInstance {
            pane_id: id.to_string(),
            pane_pid: pid,
        }
    }

    fn state(pane_instance: PaneInstance, completed: u64, acknowledged: u64) -> PaneState {
        PaneState {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
            revision: 3,
            pane_instance,
            agent: AgentKind::parse("codex").unwrap(),
            agent_session_id: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Idle,
            run_seq: completed,
            completed_seq: completed,
            acknowledged_seq: acknowledged,
            started_at: Some(1),
            completed_at: Some(2),
            prompt: None,
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
        }
    }

    fn event(occurrence: ViewOccurrence, witnesses: Vec<ClientWitness>) -> ViewEvent {
        ViewEvent {
            daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                "ffeeddccbbaa99887766554433221100",
            )
            .unwrap(),
            event_id: crate::pane_state::EventId::parse("102132435465768798a9bacbdcedfe0f")
                .unwrap(),
            hook_kind: ViewHookKind::WindowPaneChanged,
            occurrence: Some(occurrence),
            source_client: Some(SourceClientHint { client_pid: 10 }),
            witnesses,
        }
    }

    #[test]
    fn hook_preflight_rejects_foreign_without_mutation() {
        let mock = MockTmuxRunner::new();
        let query = hook_query_args();
        let query_refs = query.iter().map(String::as_str).collect::<Vec<_>>();
        mock.stub(
            &query_refs,
            &hook_query_output(|kind| {
                (kind == ViewHookKind::WindowPaneChanged)
                    .then(|| "run-shell \"foreign\"".to_string())
            }),
        );
        let error = install_hooks(&mock, &hook_identity()).unwrap_err();
        assert!(matches!(error, HookError::Collision { .. }));
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn hook_identity_mismatch_stops_before_mutation() {
        let mock = MockTmuxRunner::new();
        let query = hook_query_args();
        let query_refs = query.iter().map(String::as_str).collect::<Vec<_>>();
        mock.stub(
            &query_refs,
            &format!("{HOOK_IDENTITY_PREFIX}43:99\n{}", hook_output(|_| None)),
        );
        assert_eq!(
            install_hooks(&mock, &hook_identity()).unwrap_err(),
            HookError::ServerMismatch
        );
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn install_post_verifies_all_five_owned_hooks() {
        let mock = MockTmuxRunner::new();
        let query = hook_query_args();
        let query_refs = query.iter().map(String::as_str).collect::<Vec<_>>();
        mock.stub(&query_refs, &hook_query_output(|_| None));
        let install = hook_install_args();
        let guarded_install = server_guarded_command_args(
            hook_identity().pid,
            hook_identity().start_time,
            tmux_command_string(&install),
            HOOK_SERVER_MISMATCH_SENTINEL,
        );
        let install_refs = guarded_install
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        mock.stub(&install_refs, "");
        mock.stub(
            &query_refs,
            &hook_query_output(|kind| Some(verified_command(kind))),
        );
        install_hooks(&mock, &hook_identity()).unwrap();
        assert_eq!(mock.calls().len(), 3);
        assert!(install.iter().any(|arg| arg.contains("--owner")));
        assert!(install.iter().any(|arg| arg.contains("--protocol 2")));
    }

    #[test]
    fn monitor_degrades_without_repair_and_recovers_when_owned() {
        let mock = MockTmuxRunner::new();
        let query = hook_query_args();
        let refs = query.iter().map(String::as_str).collect::<Vec<_>>();
        mock.stub(&refs, &hook_query_output(|_| None));
        assert_eq!(
            monitor_hooks(&mock, &hook_identity()).unwrap(),
            HookHealth::Degraded
        );
        mock.stub(
            &refs,
            &hook_query_output(|kind| Some(verified_command(kind))),
        );
        assert_eq!(
            monitor_hooks(&mock, &hook_identity()).unwrap(),
            HookHealth::Healthy
        );
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn uninstall_removes_only_owned_slots_and_rejects_foreign() {
        let owned = inspect_hook_output(&hook_output(|kind| {
            (kind == ViewHookKind::WindowPaneChanged).then(|| verified_command(kind))
        }))
        .unwrap();
        let args = hook_uninstall_args(&owned);
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == "set-hook").count(),
            1
        );
        assert!(args.contains(&indexed_hook_name(ViewHookKind::WindowPaneChanged)));

        let foreign = inspect_hook_output(&hook_output(|kind| {
            (kind == ViewHookKind::ClientAttached).then(|| "run-shell \"foreign\"".to_string())
        }))
        .unwrap();
        assert!(reject_foreign(&foreign).is_err());
    }

    #[test]
    fn pane_and_window_ack_scope_use_immutable_occurrence() {
        let first = pane("%1", 11);
        let second = pane("%2", 22);
        let occurrence = ViewOccurrence {
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: first.clone(),
            observed_panes: vec![first.clone(), second.clone()],
        };
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: first.clone(),
            control_mode: false,
            active_pane_flag: false,
        };
        let event = event(occurrence, vec![witness]);
        let records = BTreeMap::from([
            (
                first.clone(),
                StoredPaneRecord::Active(state(first.clone(), 1, 0)),
            ),
            (
                second.clone(),
                StoredPaneRecord::Active(state(second.clone(), 2, 0)),
            ),
        ]);
        let pane_intents = acknowledgement_intents(&event, DoneClearOn::Pane, &records).unwrap();
        assert_eq!(pane_intents.len(), 1);
        assert_eq!(pane_intents[0].pane_instance, first);
        assert_eq!(pane_intents[0].through_seq, 1);

        let window_intents =
            acknowledgement_intents(&event, DoneClearOn::Window, &records).unwrap();
        assert_eq!(window_intents.len(), 2);
        assert_eq!(window_intents[1].pane_instance, second);
        assert_eq!(window_intents[1].through_seq, 2);
    }

    #[test]
    fn detached_or_control_clients_do_not_acknowledge() {
        let target = pane("%1", 11);
        let occurrence = ViewOccurrence {
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: target.clone(),
            observed_panes: vec![target.clone()],
        };
        let control = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: target.clone(),
            control_mode: true,
            active_pane_flag: false,
        };
        let records = BTreeMap::from([(
            target.clone(),
            StoredPaneRecord::Active(state(target, 1, 0)),
        )]);
        assert_eq!(
            acknowledgement_intents(
                &event(occurrence.clone(), Vec::new()),
                DoneClearOn::Pane,
                &records,
            ),
            Err(ViewError::UnverifiedOccurrence)
        );
        assert_eq!(
            acknowledgement_intents(
                &event(occurrence, vec![control]),
                DoneClearOn::Pane,
                &records,
            ),
            Err(ViewError::UnverifiedOccurrence)
        );
    }

    #[test]
    fn delivery_retries_only_before_first_full_write() {
        let mut delivery = ViewDeliveryContract::default();
        assert!(delivery.begin_attempt());
        assert!(delivery.may_retry(DeliveryFailureStage::BeforeFullWrite));
        assert!(delivery.begin_attempt());
        delivery.record_full_write();
        assert!(!delivery.may_retry(DeliveryFailureStage::AfterFullWrite));
        assert!(!delivery.begin_attempt());
    }

    #[test]
    fn foreground_delivery_retries_before_write_with_same_id_and_never_after_write() {
        let target = pane("%1", 11);
        let view = event(
            ViewOccurrence {
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: target.clone(),
                observed_panes: vec![target],
            },
            Vec::new(),
        );
        let before = || V2RequestError {
            stage: V2RequestFailureStage::BeforeFullWrite,
            message: "connect failed".to_string(),
        };
        let mut sender = MockViewSender {
            responses: VecDeque::from([
                Err(before()),
                Err(before()),
                Ok(V2ServerMessage::ViewQueued {
                    event_id: view.event_id.clone(),
                    accepted_seq: 1,
                }),
            ]),
            event_ids: Vec::new(),
        };
        deliver_view_event_with(
            &mut sender,
            &view,
            Instant::now() + Duration::from_millis(500),
        )
        .unwrap();
        assert_eq!(sender.event_ids, vec![view.event_id.clone(); 3]);

        let mut exhausted = MockViewSender {
            responses: VecDeque::from([Err(before()), Err(before()), Err(before())]),
            event_ids: Vec::new(),
        };
        let exhausted_error = deliver_view_event_with(
            &mut exhausted,
            &view,
            Instant::now() + Duration::from_millis(500),
        )
        .unwrap_err();
        assert_eq!(
            exhausted_error.stage,
            ViewDeliveryFailureStage::BeforeFullWrite
        );
        assert_eq!(exhausted_error.event_id, view.event_id);
        assert_eq!(exhausted.event_ids.len(), 3);

        let mut ambiguous = MockViewSender {
            responses: VecDeque::from([Err(V2RequestError {
                stage: V2RequestFailureStage::AfterFullWrite,
                message: "response lost".to_string(),
            })]),
            event_ids: Vec::new(),
        };
        let error = deliver_view_event_with(
            &mut ambiguous,
            &view,
            Instant::now() + Duration::from_millis(500),
        )
        .unwrap_err();
        assert_eq!(error.event_id, view.event_id);
        assert_eq!(error.stage, ViewDeliveryFailureStage::AfterFullWrite);
        assert_eq!(ambiguous.event_ids.len(), 1);
    }

    #[test]
    fn fresh_visibility_is_bounded_and_failure_does_not_auto_acknowledge() {
        let target = pane("%1", 11);
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: target.clone(),
            control_mode: false,
            active_pane_flag: false,
        };
        let visible = query_fresh_visibility(
            &MockFreshVisibility {
                witnesses: Ok(vec![witness]),
            },
            &target,
        )
        .unwrap();
        assert!(visible.pane_visible_to_eligible_client);
        assert!(
            query_fresh_visibility(
                &MockFreshVisibility {
                    witnesses: Err(FreshVisibilityError::Query("timeout".to_string())),
                },
                &target,
            )
            .is_err()
        );
        assert!(!VisibilitySnapshot::default().pane_visible_to_eligible_client);
        let unavailable = completion_visibility(
            &MockFreshVisibility {
                witnesses: Err(FreshVisibilityError::Query("timeout".to_string())),
            },
            &target,
        )
        .unwrap();
        assert!(!unavailable.snapshot.pane_visible_to_eligible_client);
        assert!(unavailable.diagnostic.is_some());
    }

    #[test]
    fn client_view_query_parses_flags_and_rejects_identity_mismatch() {
        let token = "00112233445566778899aabbccddeeff";
        let field = format!("__vde_client_field_{token}__");
        let row = format!("__vde_client_row_{token}__");
        let identity = ServerIdentity {
            pid: 42,
            start_time: 99,
        };
        let output = format!(
            "__vde_client_identity_{token}__42:99\n10{field}$1{field}@2{field}%3{field}30{field}0{field}attached,read-only{row}\n20{field}$1{field}@2{field}%3{field}30{field}1{field}attached,active-pane,control-mode{row}\n"
        );
        let witnesses = parse_client_view_query(&output, token, &identity).unwrap();
        assert_eq!(witnesses.len(), 2);
        assert!(witnesses[0].is_eligible());
        assert!(!witnesses[1].is_eligible());
        assert!(matches!(
            parse_client_view_query(
                &output,
                token,
                &ServerIdentity {
                    pid: 43,
                    start_time: 99,
                },
            ),
            Err(FreshVisibilityError::IdentityMismatch { .. })
        ));
    }

    #[test]
    fn view_processing_acks_immutable_occurrence_after_registry_moves() {
        let first = pane("%1", 11);
        let second = pane("%2", 22);
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: first.clone(),
            control_mode: false,
            active_pane_flag: false,
        };
        let view = event(
            ViewOccurrence {
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: first.clone(),
                observed_panes: vec![first.clone()],
            },
            vec![witness],
        );
        let records = BTreeMap::from([(
            first.clone(),
            StoredPaneRecord::Active(state(first.clone(), 1, 0)),
        )]);
        let result = process_view_event(
            &mut ViewRegistry::default(),
            &view,
            ScopedViewRefresh::Window {
                window_id: "@1".to_string(),
                active_pane: second.clone(),
                observed_panes: vec![first.clone(), second],
            },
            DoneClearOn::Pane,
            &records,
        )
        .unwrap();
        assert_eq!(result.acknowledgements.len(), 1);
        assert_eq!(result.acknowledgements[0].pane_instance, first);
    }

    #[test]
    fn periodic_reconciliation_recovers_only_while_view_remains_visible() {
        let first = pane("%1", 11);
        let second = pane("%2", 22);
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: first.clone(),
            control_mode: false,
            active_pane_flag: false,
        };
        let windows = BTreeMap::from([("@1".to_string(), vec![first.clone(), second.clone()])]);
        let records = BTreeMap::from([
            (
                first.clone(),
                StoredPaneRecord::Active(state(first.clone(), 1, 0)),
            ),
            (
                second.clone(),
                StoredPaneRecord::Active(state(second.clone(), 1, 0)),
            ),
        ]);
        let daemon = DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap();
        let mut registry = ViewRegistry::default();
        let pane_scope = reconcile_current_views(
            &mut registry,
            &daemon,
            std::slice::from_ref(&witness),
            &windows,
            DoneClearOn::Pane,
            &records,
        )
        .unwrap();
        assert_eq!(pane_scope.acknowledgements.len(), 1);
        assert_eq!(pane_scope.acknowledgements[0].pane_instance, first);

        let linked_witness = ClientWitness {
            client_pid: 20,
            session_id: "$2".to_string(),
            ..witness.clone()
        };
        let window_scope = reconcile_current_views(
            &mut registry,
            &daemon,
            &[witness.clone(), linked_witness],
            &windows,
            DoneClearOn::Window,
            &records,
        )
        .unwrap();
        assert_eq!(window_scope.acknowledgements.len(), 2);
        assert_eq!(registry.sessions().len(), 2);

        let focused_out = reconcile_current_views(
            &mut registry,
            &daemon,
            &[],
            &windows,
            DoneClearOn::Window,
            &records,
        )
        .unwrap();
        assert!(focused_out.acknowledgements.is_empty());
    }

    #[test]
    fn reconciliation_acknowledgements_apply_as_one_store_batch() {
        let daemon = DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap();
        let first = pane("%1", 11);
        let second = pane("%2", 22);
        let mut runtime = crate::pane_state::CanonicalStateRuntime::default();
        let mut io = MemoryStoreIo;
        let mut clock = MemoryClock;
        for pane_instance in [&first, &second] {
            for event in [
                crate::pane_state::PaneEvent::BeginRun {
                    started_at: 1,
                    prompt: None,
                },
                crate::pane_state::PaneEvent::CompleteRun { completed_at: 2 },
            ] {
                runtime
                    .apply_event(
                        &mut io,
                        &mut clock,
                        &crate::pane_state::PaneEventEnvelope {
                            daemon_instance_id: daemon.clone(),
                            event_id: EventId::generate().unwrap(),
                            pane_instance: pane_instance.clone(),
                            agent: Some(AgentKind::parse("codex").unwrap()),
                            agent_session_id: Some(
                                crate::pane_state::AgentSessionId::parse("session").unwrap(),
                            ),
                            event,
                        },
                        &VisibilitySnapshot::default(),
                        DoneClearOn::Window,
                    )
                    .unwrap();
            }
        }
        let records = BTreeMap::from([
            (first.clone(), runtime.record(&first).unwrap().clone()),
            (second.clone(), runtime.record(&second).unwrap().clone()),
        ]);
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: first.clone(),
            control_mode: false,
            active_pane_flag: false,
        };
        let windows = BTreeMap::from([("@1".to_string(), vec![first.clone(), second.clone()])]);
        let reconciliation = reconcile_current_views(
            &mut ViewRegistry::default(),
            &daemon,
            &[witness],
            &windows,
            DoneClearOn::Window,
            &records,
        )
        .unwrap();
        let crate::pane_state::ViewBatchProgress::Complete(result) = runtime
            .apply_view_acknowledgement_batch(
                &mut io,
                &mut clock,
                &reconciliation.acknowledgements,
                DoneClearOn::Window,
            )
        else {
            panic!("expected completed reconciliation batch");
        };
        assert_eq!(result.committed, 2);
        assert!(result.failed.is_empty());
        for pane_instance in [&first, &second] {
            let StoredPaneRecord::Active(state) = runtime.record(pane_instance).unwrap() else {
                unreachable!();
            };
            assert_eq!(state.acknowledged_seq, state.completed_seq);
        }
    }

    #[test]
    fn short_focus_delivery_registry_move_and_persist_preserve_old_occurrence() {
        let daemon = DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap();
        let first = pane("%1", 11);
        let second = pane("%2", 22);
        let mut runtime = crate::pane_state::CanonicalStateRuntime::default();
        let mut io = MemoryStoreIo;
        let mut clock = MemoryClock;
        for event in [
            crate::pane_state::PaneEvent::BeginRun {
                started_at: 1,
                prompt: None,
            },
            crate::pane_state::PaneEvent::CompleteRun { completed_at: 2 },
        ] {
            runtime
                .apply_event(
                    &mut io,
                    &mut clock,
                    &crate::pane_state::PaneEventEnvelope {
                        daemon_instance_id: daemon.clone(),
                        event_id: EventId::generate().unwrap(),
                        pane_instance: first.clone(),
                        agent: Some(AgentKind::parse("codex").unwrap()),
                        agent_session_id: Some(
                            crate::pane_state::AgentSessionId::parse("session").unwrap(),
                        ),
                        event,
                    },
                    &VisibilitySnapshot::default(),
                    DoneClearOn::Pane,
                )
                .unwrap();
        }
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: first.clone(),
            control_mode: false,
            active_pane_flag: false,
        };
        let view = event(
            ViewOccurrence {
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: first.clone(),
                observed_panes: vec![first.clone()],
            },
            vec![witness],
        );
        let mut sender = MockViewSender {
            responses: VecDeque::from([
                Err(V2RequestError {
                    stage: V2RequestFailureStage::BeforeFullWrite,
                    message: "connect race".to_string(),
                }),
                Ok(V2ServerMessage::ViewQueued {
                    event_id: view.event_id.clone(),
                    accepted_seq: 7,
                }),
            ]),
            event_ids: Vec::new(),
        };
        let response = deliver_view_event_with(
            &mut sender,
            &view,
            Instant::now() + Duration::from_millis(500),
        )
        .unwrap();
        assert!(matches!(
            response,
            V2ServerMessage::ViewQueued {
                accepted_seq: 7,
                ..
            }
        ));
        assert_eq!(sender.event_ids, vec![view.event_id.clone(); 2]);

        let records = BTreeMap::from([(first.clone(), runtime.record(&first).unwrap().clone())]);
        let processed = process_view_event(
            &mut ViewRegistry::default(),
            &view,
            ScopedViewRefresh::Window {
                window_id: "@1".to_string(),
                active_pane: second.clone(),
                observed_panes: vec![first.clone(), second],
            },
            DoneClearOn::Pane,
            &records,
        )
        .unwrap();
        let crate::pane_state::ViewBatchProgress::Complete(result) = runtime
            .apply_view_acknowledgement_batch(
                &mut io,
                &mut clock,
                &processed.acknowledgements,
                DoneClearOn::Pane,
            )
        else {
            panic!("expected completed short-focus batch");
        };
        assert_eq!(result.committed, 1);
        let StoredPaneRecord::Active(state) = runtime.record(&first).unwrap() else {
            unreachable!();
        };
        assert_eq!(state.acknowledged_seq, state.completed_seq);
    }

    #[test]
    fn view_limits_are_all_or_nothing() {
        let active = pane("%1", 1);
        let panes = (1..=crate::pane_state::MAX_VIEW_PANES)
            .map(|index| pane(&format!("%{index}"), index as u32))
            .collect::<Vec<_>>();
        let witnesses = (1..=crate::pane_state::MAX_VIEW_WITNESSES)
            .map(|index| ClientWitness {
                client_pid: index as u32,
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: active.clone(),
                control_mode: false,
                active_pane_flag: false,
            })
            .collect::<Vec<_>>();
        let valid = event(
            ViewOccurrence {
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: active.clone(),
                observed_panes: panes.clone(),
            },
            witnesses.clone(),
        );
        valid.validate().unwrap();

        let mut too_many_panes = valid.clone();
        too_many_panes
            .occurrence
            .as_mut()
            .unwrap()
            .observed_panes
            .push(pane("%513", 513));
        assert!(too_many_panes.validate().is_err());

        let mut too_many_witnesses = valid;
        too_many_witnesses.witnesses.push(ClientWitness {
            client_pid: 65,
            ..witnesses[0].clone()
        });
        assert!(too_many_witnesses.validate().is_err());
        let mut registry = ViewRegistry::default();
        assert!(
            registry
                .reconcile(
                    &too_many_witnesses.witnesses,
                    &BTreeMap::from([("@1".to_string(), panes)]),
                )
                .is_err()
        );
        assert!(registry.clients().is_empty());
    }

    #[test]
    fn foreground_builder_drops_unverified_occurrence_but_keeps_registry_witnesses() {
        let target = pane("%1", 11);
        let occurrence = ViewOccurrence {
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: target.clone(),
            observed_panes: vec![target.clone()],
        };
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$2".to_string(),
            window_id: "@2".to_string(),
            active_pane: pane("%2", 22),
            control_mode: false,
            active_pane_flag: false,
        };
        let built = build_foreground_view_event(
            crate::pane_state::DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            crate::pane_state::EventId::parse("102132435465768798a9bacbdcedfe0f").unwrap(),
            ViewHookKind::WindowPaneChanged,
            Some(occurrence),
            Some(SourceClientHint { client_pid: 10 }),
            vec![witness],
            DoneClearOn::Pane,
        )
        .unwrap();
        assert!(built.unverified_occurrence);
        assert!(built.event.occurrence.is_none());
        assert_eq!(built.event.witnesses.len(), 1);
    }

    #[test]
    fn registry_scoped_failure_keeps_current_state_and_detach_removes_client() {
        let mut registry = ViewRegistry::default();
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: pane("%1", 11),
            control_mode: false,
            active_pane_flag: false,
        };
        registry
            .reconcile(
                std::slice::from_ref(&witness),
                &BTreeMap::from([("@1".to_string(), vec![pane("%1", 11)])]),
            )
            .unwrap();
        assert!(
            !registry
                .apply_scoped_refresh(
                    ViewHookKind::ClientSessionChanged,
                    ScopedViewRefresh::QueryFailed,
                )
                .unwrap()
        );
        assert_eq!(registry.clients().get(&10), Some(&witness));
        assert!(
            registry
                .apply_scoped_refresh(
                    ViewHookKind::ClientDetached,
                    ScopedViewRefresh::ClientAbsent { client_pid: 10 },
                )
                .unwrap()
        );
        assert!(registry.clients().is_empty());
    }

    #[test]
    fn registry_scoped_window_and_session_refresh_update_related_clients() {
        let mut registry = ViewRegistry::default();
        let witnesses = [
            ClientWitness {
                client_pid: 10,
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: pane("%1", 11),
                control_mode: false,
                active_pane_flag: false,
            },
            ClientWitness {
                client_pid: 20,
                session_id: "$2".to_string(),
                window_id: "@1".to_string(),
                active_pane: pane("%1", 11),
                control_mode: false,
                active_pane_flag: false,
            },
        ];
        registry
            .reconcile(
                &witnesses,
                &BTreeMap::from([("@1".to_string(), vec![pane("%1", 11)])]),
            )
            .unwrap();
        registry
            .apply_scoped_refresh(
                ViewHookKind::WindowPaneChanged,
                ScopedViewRefresh::Window {
                    window_id: "@1".to_string(),
                    active_pane: pane("%2", 22),
                    observed_panes: vec![pane("%1", 11), pane("%2", 22)],
                },
            )
            .unwrap();
        assert!(
            registry
                .clients()
                .values()
                .all(|witness| witness.active_pane == pane("%2", 22))
        );
        registry
            .apply_scoped_refresh(
                ViewHookKind::SessionWindowChanged,
                ScopedViewRefresh::Session {
                    session_id: "$1".to_string(),
                    window_id: "@2".to_string(),
                    active_pane: pane("%3", 33),
                    observed_panes: vec![pane("%3", 33)],
                },
            )
            .unwrap();
        assert_eq!(registry.clients()[&10].window_id, "@2");
        assert_eq!(registry.clients()[&10].active_pane, pane("%3", 33));
        assert_eq!(registry.clients()[&20].window_id, "@1");
    }

    #[test]
    fn detached_refresh_keeps_client_when_same_pid_still_exists() {
        let mut registry = ViewRegistry::default();
        let witness = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: pane("%1", 11),
            control_mode: false,
            active_pane_flag: false,
        };
        registry
            .reconcile(
                std::slice::from_ref(&witness),
                &BTreeMap::from([("@1".to_string(), vec![pane("%1", 11)])]),
            )
            .unwrap();
        assert!(
            !registry
                .apply_scoped_refresh(
                    ViewHookKind::ClientDetached,
                    ScopedViewRefresh::Client {
                        witness: witness.clone(),
                        observed_panes: vec![pane("%1", 11)],
                    },
                )
                .unwrap()
        );
        assert_eq!(registry.clients().get(&10), Some(&witness));
    }

    #[test]
    fn client_refresh_propagates_linked_window_active_pane() {
        let first = pane("%1", 11);
        let second = pane("%2", 22);
        let mut registry = ViewRegistry::default();
        let witnesses = [
            ClientWitness {
                client_pid: 10,
                session_id: "$1".to_string(),
                window_id: "@1".to_string(),
                active_pane: first.clone(),
                control_mode: false,
                active_pane_flag: false,
            },
            ClientWitness {
                client_pid: 20,
                session_id: "$2".to_string(),
                window_id: "@1".to_string(),
                active_pane: first.clone(),
                control_mode: false,
                active_pane_flag: false,
            },
        ];
        registry
            .reconcile(
                &witnesses,
                &BTreeMap::from([("@1".to_string(), vec![first.clone(), second.clone()])]),
            )
            .unwrap();
        let moved = ClientWitness {
            active_pane: second.clone(),
            ..witnesses[0].clone()
        };
        assert!(
            registry
                .apply_scoped_refresh(
                    ViewHookKind::ClientSessionChanged,
                    ScopedViewRefresh::Client {
                        witness: moved,
                        observed_panes: vec![first, second.clone()],
                    },
                )
                .unwrap()
        );
        assert!(
            registry
                .clients()
                .values()
                .all(|witness| witness.active_pane == second)
        );
        assert!(
            registry
                .sessions()
                .values()
                .all(|(_, active_pane)| active_pane == &second)
        );
    }
}

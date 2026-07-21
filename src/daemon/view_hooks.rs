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
    ClientWitness, DaemonInstanceId, EventId, PaneInstance, PaneState, StateId, ViewEvent,
    ViewHookKind, ViewVisibilityProof, VisibilitySnapshot,
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

const HOOK_PANE_FIELD: &str = "__vde_hook_pane_field_v2__";
const HOOK_PANE_ROW: &str = "__vde_hook_pane_row_v2__";
const HOOK_CLIENT_FIELD: &str = "__vde_hook_client_field_v2__";
const HOOK_CLIENT_ROW: &str = "__vde_hook_client_row_v2__";
const MAX_HOOK_PANE_FRAME_BYTES: usize = 64 * 1024;
const MAX_HOOK_CLIENT_FRAME_BYTES: usize = 64 * 1024;
const MAX_HOOK_CLIENT_FLAGS_BYTES: usize = 256;

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
    verified_command(kind)
}

pub fn verified_command(kind: ViewHookKind) -> String {
    let name = hook_kind_arg(kind);
    let panes = hook_pane_loop_format();
    let clients = hook_client_loop_format();
    format!(
        "run-shell \"vt hooks pane-state-view {name} --owner {HOOK_OWNER} --protocol {HOOK_PROTOCOL} --hook-session='#{{hook_session}}' --hook-window='#{{hook_window}}' --snapshot-session='#{{session_id}}' --snapshot-window='#{{window_id}}' --snapshot-pane='#{{pane_id}}' --snapshot-pane-pid='#{{pane_pid}}' --snapshot-panes='{panes}' --snapshot-clients='{clients}'\""
    )
}

fn hook_pane_loop_format() -> String {
    let row = format!("#{{pane_id}}{HOOK_PANE_FIELD}#{{pane_pid}}{HOOK_PANE_ROW}");
    format!("#{{P:{row},{row}}}")
}

fn hook_client_loop_format() -> String {
    let row = format!(
        "#{{client_pid}}{HOOK_CLIENT_FIELD}#{{session_id}}{HOOK_CLIENT_FIELD}#{{window_id}}{HOOK_CLIENT_FIELD}#{{pane_id}}{HOOK_CLIENT_FIELD}#{{pane_pid}}{HOOK_CLIENT_FIELD}#{{client_control_mode}}{HOOK_CLIENT_FIELD}#{{client_flags}}{HOOK_CLIENT_ROW}"
    );
    let session_matches_client = "#{==:#{client_session},#{session_name}}";
    let selected_session = format!("#{{?{session_matches_client},{row},}}");
    let session_join = format!("#{{S:{selected_session}}}");
    format!("#{{L:{session_join}}}")
}

#[derive(Debug, Clone, Copy)]
pub struct HookViewSnapshotFrame<'a> {
    pub hook_session: &'a str,
    pub hook_window: &'a str,
    pub session_id: &'a str,
    pub window_id: &'a str,
    pub pane_id: &'a str,
    pub pane_pid: &'a str,
    pub panes: &'a str,
    pub clients: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHookViewSnapshot {
    pub active_pane: Option<PaneInstance>,
    pub window_panes: Vec<PaneInstance>,
    pub visibility: ViewVisibilityProof,
}

pub fn parse_hook_view_snapshot(
    kind: ViewHookKind,
    frame: HookViewSnapshotFrame<'_>,
) -> Result<ParsedHookViewSnapshot, ViewError> {
    if frame.panes.len() > MAX_HOOK_PANE_FRAME_BYTES
        || frame.clients.len() > MAX_HOOK_CLIENT_FRAME_BYTES
    {
        return Err(ViewError::InvalidEvent(
            "hook snapshot frame exceeds byte limit".to_string(),
        ));
    }
    let witnesses = parse_hook_client_rows(frame.clients)?;
    let panes = parse_hook_pane_rows(frame.panes)?;
    if kind == ViewHookKind::ClientDetached {
        return Ok(ParsedHookViewSnapshot {
            active_pane: None,
            window_panes: Vec::new(),
            visibility: ViewVisibilityProof {
                pane_visible: false,
                window_visible: false,
            },
        });
    }

    let active_pane = PaneInstance {
        pane_id: frame.pane_id.to_string(),
        pane_pid: parse_positive_pid(frame.pane_pid, "snapshot pane PID")?,
    };
    active_pane
        .validate()
        .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    let (session_id, window_id) = occurrence_context(kind, frame)?;
    validate_tmux_id(&session_id, '$', "snapshot session")?;
    validate_tmux_id(&window_id, '@', "snapshot window")?;
    validate_window_view(&active_pane, &panes)?;
    if witnesses
        .iter()
        .any(|witness| witness.window_id == window_id && !panes.contains(&witness.active_pane))
    {
        return Err(ViewError::InvalidEvent(
            "hook client pane is not a member of the declared window".to_string(),
        ));
    }
    let mut eligible = witnesses.iter().filter(|witness| witness.is_eligible());
    let pane_visible = eligible
        .clone()
        .any(|witness| witness.window_id == window_id && witness.active_pane == active_pane);
    let window_visible = eligible.any(|witness| witness.window_id == window_id);
    Ok(ParsedHookViewSnapshot {
        active_pane: Some(active_pane),
        window_panes: panes,
        visibility: ViewVisibilityProof {
            pane_visible,
            window_visible,
        },
    })
}

fn occurrence_context(
    kind: ViewHookKind,
    frame: HookViewSnapshotFrame<'_>,
) -> Result<(String, String), ViewError> {
    let (session_id, window_id) = match kind {
        ViewHookKind::WindowPaneChanged => {
            require_matching_context(frame.window_id, frame.hook_window, "hook window")?;
            (frame.session_id, frame.hook_window)
        }
        ViewHookKind::SessionWindowChanged => {
            require_matching_context(frame.session_id, frame.hook_session, "hook session")?;
            (frame.hook_session, frame.window_id)
        }
        ViewHookKind::ClientSessionChanged | ViewHookKind::ClientAttached => {
            (frame.session_id, frame.window_id)
        }
        ViewHookKind::ClientDetached => unreachable!("detached hooks have no occurrence"),
    };
    validate_tmux_id(session_id, '$', "snapshot session")?;
    validate_tmux_id(window_id, '@', "snapshot window")?;
    Ok((session_id.to_string(), window_id.to_string()))
}

fn require_matching_context(direct: &str, hook: &str, field: &str) -> Result<(), ViewError> {
    if direct.is_empty() || hook.is_empty() || direct != hook {
        return Err(ViewError::InvalidEvent(format!(
            "{field} does not match direct snapshot context"
        )));
    }
    Ok(())
}

fn parse_hook_pane_rows(raw: &str) -> Result<Vec<PaneInstance>, ViewError> {
    let rows = parse_hook_rows(
        raw,
        HOOK_PANE_ROW,
        crate::pane_state::MAX_VIEW_PANES,
        "panes",
    )?;
    let mut panes = Vec::with_capacity(rows.len());
    let mut pane_ids = BTreeSet::new();
    let mut pane_pids = BTreeSet::new();
    for row in rows {
        let fields = row.split(HOOK_PANE_FIELD).collect::<Vec<_>>();
        if fields.len() != 2 {
            return Err(ViewError::InvalidEvent(
                "hook pane row has an invalid field count".to_string(),
            ));
        }
        let pane = PaneInstance {
            pane_id: fields[0].to_string(),
            pane_pid: parse_positive_pid(fields[1], "hook pane PID")?,
        };
        pane.validate()
            .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
        if !pane_ids.insert(pane.pane_id.clone()) || !pane_pids.insert(pane.pane_pid) {
            return Err(ViewError::InvalidEvent(
                "duplicate pane in hook snapshot".to_string(),
            ));
        }
        panes.push(pane);
    }
    Ok(panes)
}

fn parse_hook_client_rows(raw: &str) -> Result<Vec<ClientWitness>, ViewError> {
    let rows = parse_hook_rows(raw, HOOK_CLIENT_ROW, MAX_CLIENT_WITNESSES, "clients")?;
    let mut witnesses = Vec::with_capacity(rows.len());
    for row in rows {
        let fields = row.split(HOOK_CLIENT_FIELD).collect::<Vec<_>>();
        if fields.len() != 7 {
            return Err(ViewError::InvalidEvent(
                "hook client row has an invalid field count".to_string(),
            ));
        }
        validate_tmux_id(fields[1], '$', "hook client session")?;
        validate_tmux_id(fields[2], '@', "hook client window")?;
        let active_pane = PaneInstance {
            pane_id: fields[3].to_string(),
            pane_pid: parse_positive_pid(fields[4], "hook client pane PID")?,
        };
        active_pane
            .validate()
            .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
        let control_mode = match fields[5] {
            "0" => false,
            "1" => true,
            _ => {
                return Err(ViewError::InvalidEvent(
                    "invalid hook client control mode".to_string(),
                ));
            }
        };
        let flags = parse_hook_client_flags(fields[6])?;
        witnesses.push(ClientWitness {
            client_pid: parse_positive_pid(fields[0], "hook client PID")?,
            session_id: fields[1].to_string(),
            window_id: fields[2].to_string(),
            active_pane,
            control_mode,
            active_pane_flag: flags.contains("active-pane"),
        });
    }
    validate_witnesses(&witnesses)?;
    Ok(witnesses)
}

fn parse_hook_client_flags(raw: &str) -> Result<BTreeSet<&str>, ViewError> {
    if raw.is_empty() {
        return Ok(BTreeSet::new());
    }
    if raw.len() > MAX_HOOK_CLIENT_FLAGS_BYTES {
        return Err(ViewError::InvalidEvent(
            "hook client flags exceed byte limit".to_string(),
        ));
    }
    let mut flags = BTreeSet::new();
    for flag in raw.split(',') {
        let plain = !flag.is_empty()
            && flag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        let pause_after = flag.strip_prefix("pause-after=").is_some_and(|seconds| {
            !seconds.is_empty() && seconds.bytes().all(|byte| byte.is_ascii_digit())
        });
        if (!plain && !pause_after) || !flags.insert(flag) {
            return Err(ViewError::InvalidEvent(
                "invalid hook client flags".to_string(),
            ));
        }
    }
    Ok(flags)
}

fn parse_hook_rows<'a>(
    mut raw: &'a str,
    separator: &str,
    maximum: usize,
    label: &str,
) -> Result<Vec<&'a str>, ViewError> {
    let mut rows = Vec::new();
    while !raw.is_empty() {
        let Some(index) = raw.find(separator) else {
            return Err(ViewError::InvalidEvent(format!(
                "unterminated hook {label} row"
            )));
        };
        if rows.len() == maximum {
            return Err(ViewError::InvalidEvent(format!("too many hook {label}")));
        }
        rows.push(&raw[..index]);
        raw = &raw[index + separator.len()..];
    }
    Ok(rows)
}

fn parse_positive_pid(raw: &str, field: &str) -> Result<u32, ViewError> {
    raw.parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| ViewError::InvalidEvent(format!("invalid {field}")))
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
pub struct CurrentClientViews {
    clients: BTreeMap<u32, ClientWitness>,
    windows: BTreeMap<String, WindowView>,
    sessions: BTreeMap<String, (String, PaneInstance)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowView {
    pub active_pane: PaneInstance,
    pub observed_panes: Vec<PaneInstance>,
}

impl CurrentClientViews {
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
    pub acknowledgements: Vec<crate::pane_state::PaneEventEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewError {
    InvalidEvent(String),
    UnverifiedOccurrence,
}

pub fn build_foreground_view_event(
    daemon_instance_id: DaemonInstanceId,
    event_id: EventId,
    hook_kind: ViewHookKind,
    active_pane: Option<PaneInstance>,
    window_panes: Vec<PaneInstance>,
    visibility: ViewVisibilityProof,
) -> Result<ViewEvent, ViewError> {
    let event = ViewEvent {
        daemon_instance_id,
        event_id,
        hook_kind,
        active_pane,
        window_panes,
        visibility,
    };
    event
        .validate()
        .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    Ok(event)
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
    records: &BTreeMap<PaneInstance, PaneState>,
) -> Result<Vec<AcknowledgementIntent>, ViewError> {
    event
        .validate()
        .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    let Some(active_pane) = event.active_pane.as_ref() else {
        return Ok(Vec::new());
    };
    let verified = match done_clear_on {
        DoneClearOn::Pane => event.visibility.pane_visible,
        DoneClearOn::Window => event.visibility.window_visible,
    };
    if !verified {
        return Err(ViewError::UnverifiedOccurrence);
    }
    let targets = match done_clear_on {
        DoneClearOn::Pane => std::slice::from_ref(active_pane),
        DoneClearOn::Window => event.window_panes.as_slice(),
    };
    let mut intents = Vec::new();
    for pane in targets {
        let Some(state) = records.get(pane) else {
            continue;
        };
        intents.push(intent_for_state(pane, state));
    }
    intents.sort_by(|left, right| left.pane_instance.cmp(&right.pane_instance));
    Ok(intents)
}

pub fn process_view_event(
    event: &ViewEvent,
    done_clear_on: DoneClearOn,
    records: &BTreeMap<PaneInstance, PaneState>,
) -> Result<ViewProcessingResult, ViewError> {
    event
        .validate()
        .map_err(|error| ViewError::InvalidEvent(error.to_string()))?;
    let intents = match acknowledgement_intents(event, done_clear_on, records) {
        Ok(intents) => intents,
        Err(ViewError::UnverifiedOccurrence) => Vec::new(),
        Err(error) => return Err(error),
    };
    Ok(ViewProcessingResult {
        acknowledgements: acknowledgement_envelopes(
            &event.daemon_instance_id,
            &event.event_id,
            intents,
        ),
    })
}

pub fn reconcile_current_views(
    registry: &mut CurrentClientViews,
    witnesses: &[ClientWitness],
    window_panes: &BTreeMap<String, Vec<PaneInstance>>,
) -> Result<bool, ViewError> {
    registry.reconcile(witnesses, window_panes)
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

pub fn visibility_snapshot(
    pane: &PaneInstance,
    window_id: Option<&str>,
    witnesses: &[ClientWitness],
) -> VisibilitySnapshot {
    VisibilitySnapshot {
        pane_visible_to_eligible_client: witnesses
            .iter()
            .any(|witness| witness.is_eligible() && witness.active_pane == *pane),
        window_visible_to_eligible_client: window_id.is_some_and(|window_id| {
            witnesses
                .iter()
                .any(|witness| witness.is_eligible() && witness.window_id == window_id)
        }),
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
    pub initial_client: Option<V2Client>,
}

impl ViewEventSender for SocketViewEventSender<'_> {
    fn send(
        &mut self,
        event: &ViewEvent,
        deadline: Instant,
    ) -> std::result::Result<V2ServerMessage, V2RequestError> {
        let mut client = match self.initial_client.take() {
            Some(client) => client,
            None => V2Client::connect_with_deadline(self.socket, self.server_identity, deadline)
                .map_err(|error| V2RequestError {
                    stage: V2RequestFailureStage::BeforeFullWrite,
                    message: error.to_string(),
                })?,
        };
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
            initial_client: None,
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
    deliver_view_event_with_contract(
        sender,
        event,
        deadline,
        ViewDeliveryContract::default(),
        false,
    )
}

pub fn deliver_view_event_with_active_attempt(
    sender: &mut dyn ViewEventSender,
    event: &ViewEvent,
    deadline: Instant,
    delivery: ViewDeliveryContract,
) -> Result<V2ServerMessage, ViewDeliveryError> {
    deliver_view_event_with_contract(sender, event, deadline, delivery, true)
}

fn deliver_view_event_with_contract(
    sender: &mut dyn ViewEventSender,
    event: &ViewEvent,
    deadline: Instant,
    mut delivery: ViewDeliveryContract,
    mut attempt_active: bool,
) -> Result<V2ServerMessage, ViewDeliveryError> {
    loop {
        if !attempt_active && !delivery.begin_attempt() {
            break;
        }
        attempt_active = false;
        if Instant::now() >= deadline {
            return Err(ViewDeliveryError {
                event_id: event.event_id.clone(),
                stage: ViewDeliveryFailureStage::Deadline,
                message: "view hook 500ms deadline exceeded".to_string(),
            });
        }
        match sender.send(event, deadline) {
            Ok(response @ V2ServerMessage::ViewQueued { .. }) => {
                let V2ServerMessage::ViewQueued {
                    event_id: response_event_id,
                    ..
                } = &response
                else {
                    unreachable!("view response pattern already matched")
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
    window_id: Option<&str>,
) -> Result<VisibilitySnapshot, FreshVisibilityError> {
    let witnesses = io.query_witnesses(FRESH_VISIBILITY_TIMEOUT)?;
    validate_witnesses(&witnesses)
        .map_err(|error| FreshVisibilityError::Parse(error.to_string()))?;
    Ok(visibility_snapshot(pane, window_id, &witnesses))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionVisibility {
    pub snapshot: VisibilitySnapshot,
    pub diagnostic: Option<String>,
}

pub fn completion_visibility(
    io: &dyn FreshVisibilityIo,
    pane: &PaneInstance,
    window_id: Option<&str>,
) -> Result<CompletionVisibility, FreshVisibilityError> {
    match query_fresh_visibility(io, pane, window_id) {
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

pub fn guarded_client_view_query_args(token: &str) -> Vec<String> {
    let mut args = client_view_query_args(token);
    debug_assert_eq!(args.get(3).map(String::as_str), Some(";"));
    debug_assert_eq!(args.get(4).map(String::as_str), Some("list-clients"));
    let list_clients = args.split_off(4);
    args.extend([
        "if-shell".to_string(),
        "-F".to_string(),
        "#{>:#{server_sessions},0}".to_string(),
        tmux_command_string(&list_clients),
    ]);
    args
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
    use crate::pane_state::{AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, TaskState};

    fn pane(id: &str, pid: u32) -> PaneInstance {
        PaneInstance {
            pane_id: id.to_string(),
            pane_pid: pid,
        }
    }

    fn pane_rows(panes: &[PaneInstance]) -> String {
        panes
            .iter()
            .map(|pane| {
                format!(
                    "{}{HOOK_PANE_FIELD}{}{HOOK_PANE_ROW}",
                    pane.pane_id, pane.pane_pid
                )
            })
            .collect()
    }

    fn client_row(
        client_pid: u32,
        session_id: &str,
        window_id: &str,
        active_pane: &PaneInstance,
        control_mode: bool,
        flags: &str,
    ) -> String {
        format!(
            "{client_pid}{HOOK_CLIENT_FIELD}{session_id}{HOOK_CLIENT_FIELD}{window_id}{HOOK_CLIENT_FIELD}{}{HOOK_CLIENT_FIELD}{}{HOOK_CLIENT_FIELD}{}{HOOK_CLIENT_FIELD}{flags}{HOOK_CLIENT_ROW}",
            active_pane.pane_id,
            active_pane.pane_pid,
            u8::from(control_mode),
        )
    }

    fn frame<'a>(panes: &'a str, clients: &'a str) -> HookViewSnapshotFrame<'a> {
        HookViewSnapshotFrame {
            hook_session: "",
            hook_window: "@2",
            session_id: "$1",
            window_id: "@2",
            pane_id: "%1",
            pane_pid: "101",
            panes,
            clients,
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

    fn event(
        active_pane: PaneInstance,
        window_panes: Vec<PaneInstance>,
        pane_visible: bool,
        window_visible: bool,
    ) -> ViewEvent {
        ViewEvent {
            daemon_instance_id: DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100")
                .unwrap(),
            event_id: EventId::parse("102132435465768798a9bacbdcedfe0f").unwrap(),
            hook_kind: ViewHookKind::WindowPaneChanged,
            active_pane: Some(active_pane),
            window_panes,
            visibility: ViewVisibilityProof {
                pane_visible,
                window_visible,
            },
        }
    }

    #[test]
    fn hook_snapshot_builds_small_immutable_visibility_proof() {
        let first = pane("%1", 101);
        let second = pane("%2", 202);
        let panes = pane_rows(&[first.clone(), second.clone()]);
        let clients = [
            client_row(10, "$1", "@2", &first, false, ""),
            client_row(11, "$1", "@2", &second, true, ""),
            client_row(12, "$1", "@2", &second, false, "active-pane"),
        ]
        .concat();

        let parsed =
            parse_hook_view_snapshot(ViewHookKind::WindowPaneChanged, frame(&panes, &clients))
                .unwrap();

        assert_eq!(parsed.active_pane, Some(first));
        assert_eq!(parsed.window_panes, vec![pane("%1", 101), second]);
        assert_eq!(
            parsed.visibility,
            ViewVisibilityProof {
                pane_visible: true,
                window_visible: true,
            }
        );
    }

    #[test]
    fn hook_snapshot_keeps_window_proof_when_another_pane_is_visible() {
        let first = pane("%1", 101);
        let second = pane("%2", 202);
        let panes = pane_rows(&[first.clone(), second.clone()]);
        let clients = client_row(10, "$1", "@2", &second, false, "");

        let parsed =
            parse_hook_view_snapshot(ViewHookKind::WindowPaneChanged, frame(&panes, &clients))
                .unwrap();

        assert!(!parsed.visibility.pane_visible);
        assert!(parsed.visibility.window_visible);
    }

    #[test]
    fn hook_snapshot_rejects_duplicates_invalid_rows_and_oversize_frames() {
        let first = pane("%1", 101);
        let duplicate = pane_rows(&[first.clone(), first]);
        assert!(
            parse_hook_view_snapshot(ViewHookKind::WindowPaneChanged, frame(&duplicate, ""))
                .is_err()
        );
        let invalid_client =
            format!("10{HOOK_CLIENT_FIELD}$1{HOOK_CLIENT_FIELD}@2{HOOK_CLIENT_ROW}");
        let panes = pane_rows(&[pane("%1", 101)]);
        assert!(
            parse_hook_view_snapshot(
                ViewHookKind::WindowPaneChanged,
                frame(&panes, &invalid_client),
            )
            .is_err()
        );
        let oversized = "x".repeat(MAX_HOOK_PANE_FRAME_BYTES + 1);
        assert!(
            parse_hook_view_snapshot(ViewHookKind::WindowPaneChanged, frame(&oversized, ""))
                .is_err()
        );
    }

    #[test]
    fn detached_hook_has_no_transient_visibility_target() {
        let parsed = parse_hook_view_snapshot(ViewHookKind::ClientDetached, frame("", "")).unwrap();
        assert_eq!(parsed.active_pane, None);
        assert!(parsed.window_panes.is_empty());
        assert!(!parsed.visibility.pane_visible);
        assert!(!parsed.visibility.window_visible);
    }

    #[test]
    fn daemon_policy_selects_pane_or_window_from_same_proof() {
        let first = pane("%1", 101);
        let second = pane("%2", 202);
        let event = event(
            first.clone(),
            vec![first.clone(), second.clone()],
            false,
            true,
        );
        let records = [
            (first.clone(), state(first.clone(), 1, 0)),
            (second.clone(), state(second.clone(), 1, 0)),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            acknowledgement_intents(&event, DoneClearOn::Pane, &records).unwrap_err(),
            ViewError::UnverifiedOccurrence
        );
        let window = acknowledgement_intents(&event, DoneClearOn::Window, &records).unwrap();
        assert_eq!(
            window
                .iter()
                .map(|intent| intent.pane_instance.clone())
                .collect::<Vec<_>>(),
            vec![first, second]
        );
    }

    #[test]
    fn sequencer_position_freezes_completion_upper_bound() {
        let target = pane("%1", 101);
        let event = event(target.clone(), vec![target.clone()], true, true);
        let records = [(target.clone(), state(target, 4, 2))]
            .into_iter()
            .collect();
        let intent = acknowledgement_intents(&event, DoneClearOn::Pane, &records).unwrap();

        assert_eq!(intent.len(), 1);
        assert_eq!(intent[0].through_seq, 4);
        assert_eq!(intent[0].expected_agent_epoch, 1);
        assert_eq!(
            intent[0].expected_state_id.as_str(),
            "00112233445566778899aabbccddeeff"
        );
    }

    #[test]
    fn current_client_views_are_full_replacements() {
        let first = pane("%1", 101);
        let second = pane("%2", 202);
        let mut views = CurrentClientViews::default();
        let windows = [
            ("@1".to_string(), vec![first.clone()]),
            ("@2".to_string(), vec![second.clone()]),
        ]
        .into_iter()
        .collect();
        let initial = ClientWitness {
            client_pid: 10,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: first,
            control_mode: false,
            active_pane_flag: false,
        };
        assert!(views.reconcile(&[initial], &windows).unwrap());

        let replacement = ClientWitness {
            client_pid: 11,
            session_id: "$2".to_string(),
            window_id: "@2".to_string(),
            active_pane: second,
            control_mode: false,
            active_pane_flag: false,
        };
        assert!(views.reconcile(&[replacement], &windows).unwrap());
        assert_eq!(
            views.clients().keys().copied().collect::<Vec<_>>(),
            vec![11]
        );
        assert_eq!(
            views.sessions().keys().cloned().collect::<Vec<_>>(),
            vec!["$2"]
        );
        assert_eq!(
            views.windows().keys().cloned().collect::<Vec<_>>(),
            vec!["@2"]
        );
    }

    #[test]
    fn hook_installation_keeps_five_owned_indexed_hooks() {
        let args = hook_install_args();
        for (kind, _) in HOOKS {
            assert!(args.contains(&indexed_hook_name(kind)));
            assert!(args.contains(&install_command(kind)));
        }
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == "set-hook").count(),
            HOOKS.len()
        );
    }

    #[test]
    fn completion_visibility_keeps_fresh_query_contract() {
        struct Fresh {
            witnesses: Vec<ClientWitness>,
        }
        impl FreshVisibilityIo for Fresh {
            fn query_witnesses(
                &self,
                timeout: Duration,
            ) -> Result<Vec<ClientWitness>, FreshVisibilityError> {
                assert_eq!(timeout, FRESH_VISIBILITY_TIMEOUT);
                Ok(self.witnesses.clone())
            }
        }
        let target = pane("%1", 101);
        let visibility = completion_visibility(
            &Fresh {
                witnesses: vec![ClientWitness {
                    client_pid: 10,
                    session_id: "$1".to_string(),
                    window_id: "@1".to_string(),
                    active_pane: target.clone(),
                    control_mode: false,
                    active_pane_flag: false,
                }],
            },
            &target,
            Some("@1"),
        )
        .unwrap();
        assert!(visibility.snapshot.pane_visible_to_eligible_client);
        assert!(visibility.snapshot.window_visible_to_eligible_client);
        assert_eq!(visibility.diagnostic, None);
    }

    #[test]
    fn hook_command_contains_hook_time_loops() {
        let command = install_command(ViewHookKind::WindowPaneChanged);
        assert!(command.contains("#{P:"));
        assert!(command.contains("#{L:#{S:"));
    }
}

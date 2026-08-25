use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;

use crate::config::Config;
use crate::daemon::protocol::v2::ResolvedSnapshot;
use crate::pane_state::PaneInstance;
use crate::sidebar::client::SubscriptionUpdate;
use crate::sidebar::render::SidebarRenderTheme;
use crate::sidebar::state::{PresentationMode, SidebarState};
use crate::sidebar::tree::chat_row_id;
use crate::tmux::TmuxRunner;

use super::effects::{
    CategoryIntentRequest, PanePinRequest, PreferenceIntentRequest, queue_reorder,
};
use super::mouse::queue_pane_pin_for_selection;
use super::projection::{
    PeekSource, ViewportMove, adjacent_agent_target, apply_local_sidebar_key,
    jump_latest_unread_from_control, move_viewport_selection, peek_from_control, project_view,
    read_current_from_control, select_context_agent, set_sidebar_context,
};
use super::types::{DrawnFrame, MarkCompleteUi, Notice, NoticeLevel};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) enum ConnectionState {
    #[default]
    Connecting,
    Connected,
    ConfigChanged(String),
    Degraded(String),
    Disconnected,
}

impl ConnectionState {
    fn label(&self) -> Option<&str> {
        match self {
            Self::Connecting => Some("connecting"),
            Self::Connected => None,
            Self::ConfigChanged(_) => Some("reloading config"),
            Self::Degraded(_) => Some("degraded"),
            Self::Disconnected => Some("disconnected · reconnecting"),
        }
    }

    pub(super) fn notice(&self) -> Option<Notice<'_>> {
        self.label().map(|message| Notice {
            message,
            level: match self {
                Self::Connecting | Self::ConfigChanged(_) => NoticeLevel::Progress,
                Self::Degraded(_) | Self::Disconnected => NoticeLevel::Failure,
                Self::Connected => NoticeLevel::Success,
            },
        })
    }
}

/// Decides whether a run-loop iteration projects the sidebar view and draws a frame.
/// The gate stays clean until a visible change is reported; elapsed-time labels in a
/// sidebar visible to an attached client are refreshed once per second boundary.
#[derive(Debug)]
pub(super) struct RenderGate {
    dirty: bool,
    last_elapsed_second: Option<i64>,
    last_toast: Option<(String, NoticeLevel)>,
}

impl RenderGate {
    pub(super) fn new() -> Self {
        Self {
            dirty: true,
            last_elapsed_second: None,
            last_toast: None,
        }
    }

    pub(super) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(super) fn mark_dirty_if(&mut self, changed: bool) {
        if changed {
            self.dirty = true;
        }
    }

    pub(super) fn note_toast(&mut self, notice: Option<Notice<'_>>) {
        let current = notice.map(|notice| (notice.message.to_string(), notice.level));
        if self.last_toast != current {
            self.last_toast = current;
            self.dirty = true;
        }
    }

    pub(super) fn take_draw_decision(
        &mut self,
        now_epoch_secs: i64,
        elapsed_clock_visible: bool,
    ) -> bool {
        if elapsed_clock_visible && self.last_elapsed_second != Some(now_epoch_secs) {
            self.last_elapsed_second = Some(now_epoch_secs);
            self.dirty = true;
        }
        std::mem::take(&mut self.dirty)
    }
}

pub(super) fn sidebar_elapsed_clock_visible(
    snapshot: &ResolvedSnapshot,
    sidebar_instance: &PaneInstance,
) -> bool {
    snapshot
        .panes
        .iter()
        .find(|pane| &pane.pane_instance == sidebar_instance)
        .is_some_and(|pane| {
            pane.session_links.iter().any(|link| {
                link.window_active
                    && snapshot
                        .sidebar_model
                        .active_sessions
                        .contains(&link.session_id)
            })
        })
}

/// Geometry and row mapping of the most recently drawn frame. Click hit-testing
/// must use exactly what was drawn, so the run loop records it on every draw.
#[derive(Debug, Clone)]
pub(super) struct ControlMessageContext<'a> {
    pub(super) snapshot: Option<&'a ResolvedSnapshot>,
    pub(super) config: &'a Config,
    pub(super) daemon_connected: bool,
    pub(super) socket: &'a Path,
    pub(super) server_identity: &'a str,
    pub(super) frame: Option<&'a DrawnFrame>,
    pub(super) theme: &'a SidebarRenderTheme,
}

pub(super) fn drain_control_messages(
    control: &crate::sidebar::control::ControlListener,
    state: &mut SidebarState,
    preference_tx: &mpsc::Sender<PreferenceIntentRequest>,
    category_tx: &mpsc::Sender<CategoryIntentRequest>,
    pin_tx: &mpsc::Sender<PanePinRequest>,
    ui: &mut MarkCompleteUi,
    context: ControlMessageContext<'_>,
) -> Result<bool> {
    let ControlMessageContext {
        snapshot,
        config,
        daemon_connected,
        socket,
        server_identity,
        frame,
        theme,
    } = context;
    let mut before: Option<SidebarState> = None;
    while let Some(message) = control.try_recv()? {
        if before.is_none() {
            before = Some(state.clone());
        }
        match message {
            crate::sidebar::control::ControlMessage::Input {
                key,
                source_pane,
                session_id,
            } => {
                if let Some(snapshot) = snapshot {
                    set_sidebar_context(snapshot, state, &source_pane, &session_id);
                    let sidebar = project_view(snapshot, config, state);
                    match crate::sidebar::input::parse_key(&key) {
                        Some(crate::sidebar::input::SidebarInputAction::AgentNext)
                        | Some(crate::sidebar::input::SidebarInputAction::AgentPrevious)
                        | Some(crate::sidebar::input::SidebarInputAction::ReadCurrent)
                        | Some(crate::sidebar::input::SidebarInputAction::UnreadLatest)
                        | Some(crate::sidebar::input::SidebarInputAction::TogglePanePin)
                            if !daemon_connected =>
                        {
                            ui.set_toast(
                                "jump unavailable while sidebar is degraded".to_string(),
                                NoticeLevel::Warning,
                                Duration::from_secs(5),
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::AgentNext) => {
                            ui.set_toast(
                                "agent-next requires --client-pid".to_string(),
                                NoticeLevel::Failure,
                                Duration::from_secs(5),
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::AgentPrevious) => {
                            ui.set_toast(
                                "agent-prev requires --client-pid".to_string(),
                                NoticeLevel::Failure,
                                Duration::from_secs(5),
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::ReadCurrent) => {
                            ui.set_toast(
                                "read-current requires --client-pid".to_string(),
                                NoticeLevel::Failure,
                                Duration::from_secs(5),
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::UnreadLatest) => {
                            jump_latest_unread_from_control(
                                socket,
                                server_identity,
                                &source_pane,
                                ui,
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::TogglePanePin) => {
                            queue_pane_pin_for_selection(snapshot, &sidebar, pin_tx, ui);
                        }
                        Some(crate::sidebar::input::SidebarInputAction::MoveFirst) => {
                            move_viewport_selection(
                                snapshot,
                                config,
                                state,
                                frame,
                                ViewportMove::First,
                                theme,
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::MoveLast) => {
                            move_viewport_selection(
                                snapshot,
                                config,
                                state,
                                frame,
                                ViewportMove::Last,
                                theme,
                            );
                        }
                        Some(
                            action @ (crate::sidebar::input::SidebarInputAction::HalfPageDown
                            | crate::sidebar::input::SidebarInputAction::HalfPageUp
                            | crate::sidebar::input::SidebarInputAction::PageDown
                            | crate::sidebar::input::SidebarInputAction::PageUp),
                        ) => {
                            let movement = match action {
                                crate::sidebar::input::SidebarInputAction::HalfPageDown => {
                                    ViewportMove::HalfPageDown
                                }
                                crate::sidebar::input::SidebarInputAction::HalfPageUp => {
                                    ViewportMove::HalfPageUp
                                }
                                crate::sidebar::input::SidebarInputAction::PageDown => {
                                    ViewportMove::PageDown
                                }
                                crate::sidebar::input::SidebarInputAction::PageUp => {
                                    ViewportMove::PageUp
                                }
                                _ => unreachable!(),
                            };
                            move_viewport_selection(
                                snapshot, config, state, frame, movement, theme,
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::ReorderUp)
                            if daemon_connected =>
                        {
                            queue_reorder(&sidebar, true, preference_tx, category_tx, ui);
                        }
                        Some(crate::sidebar::input::SidebarInputAction::ReorderDown)
                            if daemon_connected =>
                        {
                            queue_reorder(&sidebar, false, preference_tx, category_tx, ui);
                        }
                        _ => apply_local_sidebar_key(state, &sidebar, &key),
                    }
                } else if matches!(
                    crate::sidebar::input::parse_key(&key),
                    Some(crate::sidebar::input::SidebarInputAction::AgentNext)
                        | Some(crate::sidebar::input::SidebarInputAction::AgentPrevious)
                        | Some(crate::sidebar::input::SidebarInputAction::ReadCurrent)
                        | Some(crate::sidebar::input::SidebarInputAction::UnreadLatest)
                        | Some(crate::sidebar::input::SidebarInputAction::TogglePanePin)
                ) {
                    ui.set_toast(
                        "jump unavailable before the first snapshot".to_string(),
                        NoticeLevel::Warning,
                        Duration::from_secs(5),
                    );
                }
            }
            crate::sidebar::control::ControlMessage::PeekInput {
                key,
                source_pane,
                session_id,
                client_pid,
            } => {
                let action = crate::sidebar::input::parse_key(&key);
                if !matches!(
                    action,
                    Some(crate::sidebar::input::SidebarInputAction::AgentNext)
                        | Some(crate::sidebar::input::SidebarInputAction::AgentPrevious)
                        | Some(crate::sidebar::input::SidebarInputAction::ReadCurrent)
                ) {
                    ui.set_toast(
                        "invalid peek control input".to_string(),
                        NoticeLevel::Failure,
                        Duration::from_secs(5),
                    );
                    continue;
                }
                let Some(snapshot) = snapshot else {
                    ui.set_toast(
                        "peek unavailable before the first snapshot".to_string(),
                        NoticeLevel::Warning,
                        Duration::from_secs(5),
                    );
                    continue;
                };
                if !daemon_connected {
                    ui.set_toast(
                        "peek unavailable while sidebar is degraded".to_string(),
                        NoticeLevel::Warning,
                        Duration::from_secs(5),
                    );
                    continue;
                }
                set_sidebar_context(snapshot, state, &source_pane, &session_id);
                let priority_navigation = matches!(
                    action,
                    Some(crate::sidebar::input::SidebarInputAction::AgentNext)
                        | Some(crate::sidebar::input::SidebarInputAction::AgentPrevious)
                );
                if priority_navigation && state.presentation_mode != PresentationMode::Priority {
                    ui.set_toast(
                        "peek navigation requires Priority view".to_string(),
                        NoticeLevel::Warning,
                        Duration::from_secs(5),
                    );
                    continue;
                }
                let sidebar = project_view(snapshot, config, state);
                match action {
                    Some(crate::sidebar::input::SidebarInputAction::AgentNext)
                    | Some(crate::sidebar::input::SidebarInputAction::AgentPrevious) => {
                        let forward = matches!(
                            action,
                            Some(crate::sidebar::input::SidebarInputAction::AgentNext)
                        );
                        let anchor = chat_row_id(&source_pane);
                        let target =
                            adjacent_agent_target(Some(anchor.as_str()), &sidebar.rows, forward);
                        peek_from_control(
                            socket,
                            server_identity,
                            snapshot,
                            PeekSource {
                                pane: &source_pane,
                                client_pid,
                            },
                            target,
                            state,
                            ui,
                        );
                    }
                    Some(crate::sidebar::input::SidebarInputAction::ReadCurrent) => {
                        let rows = if state.presentation_mode == PresentationMode::Priority {
                            sidebar.rows.as_slice()
                        } else {
                            &[]
                        };
                        read_current_from_control(
                            socket,
                            server_identity,
                            &source_pane,
                            client_pid,
                            rows,
                            state,
                            ui,
                        );
                    }
                    _ => unreachable!("peek input was validated"),
                }
            }
            crate::sidebar::control::ControlMessage::Focus {
                pane_instance,
                session_id,
            } => {
                let Some(snapshot) = snapshot else {
                    continue;
                };
                apply_focus_message(snapshot, config, state, pane_instance, &session_id);
            }
        }
    }
    Ok(before.is_some_and(|before| before != *state))
}

fn apply_focus_message(
    snapshot: &ResolvedSnapshot,
    config: &Config,
    state: &mut SidebarState,
    pane_instance: PaneInstance,
    session_id: &str,
) -> bool {
    let Some(pane) = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance == pane_instance)
    else {
        return false;
    };
    if !session_id.is_empty()
        && !pane
            .session_links
            .iter()
            .any(|link| link.session_id == session_id)
    {
        return false;
    }
    set_sidebar_context(snapshot, state, &pane_instance, session_id);
    let changed = select_context_agent(
        snapshot,
        config,
        state,
        Some(&pane_instance),
        Some(session_id),
    );
    if changed {
        true
    } else {
        state.version = state.version.saturating_add(1);
        true
    }
}

pub(super) fn drain_snapshot_updates(
    rx: &mpsc::Receiver<SubscriptionUpdate>,
    current: &mut Option<ResolvedSnapshot>,
    connection: &mut ConnectionState,
) -> bool {
    let mut changed = false;
    let set_connection = |connection: &mut ConnectionState, next: ConnectionState| {
        if *connection != next {
            *connection = next;
            true
        } else {
            false
        }
    };
    loop {
        match rx.try_recv() {
            Ok(SubscriptionUpdate::Connecting) => {
                changed |= set_connection(connection, ConnectionState::Connecting);
            }
            Ok(SubscriptionUpdate::Connected(snapshot)) => {
                let next = match snapshot_degraded_message(&snapshot) {
                    Some(message) => ConnectionState::Degraded(message),
                    None => ConnectionState::Connected,
                };
                let revision_changed = current.as_ref().is_none_or(|previous| {
                    previous.snapshot_revision != snapshot.snapshot_revision
                });
                *current = Some(*snapshot);
                changed |= revision_changed;
                changed |= set_connection(connection, next);
            }
            Ok(SubscriptionUpdate::ConfigChanged { active_config_hash }) => {
                changed |= set_connection(
                    connection,
                    ConnectionState::ConfigChanged(active_config_hash),
                );
                return changed;
            }
            Ok(SubscriptionUpdate::Degraded(error)) => {
                changed |= set_connection(connection, ConnectionState::Degraded(error));
            }
            Ok(SubscriptionUpdate::Disconnected) => {
                changed |= set_connection(connection, ConnectionState::Disconnected);
            }
            Err(mpsc::TryRecvError::Empty) => return changed,
            Err(mpsc::TryRecvError::Disconnected) => {
                changed |= set_connection(connection, ConnectionState::Disconnected);
                return changed;
            }
        }
    }
}

fn snapshot_degraded_message(snapshot: &ResolvedSnapshot) -> Option<String> {
    crate::sidebar::current_degraded_message(snapshot)
}

pub(super) fn resolve_current_window_id(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let mut args = vec!["display-message", "-p"];
    if let Some(pane) = env
        .get("TMUX_PANE")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        args.extend(["-t", pane]);
    }
    args.extend(["-F", "#{window_id}"]);
    let window = runner.run(&args)?.trim().to_string();
    if window.is_empty() {
        anyhow::bail!("failed to resolve current sidebar window");
    }
    Ok(window)
}

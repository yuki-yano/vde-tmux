use serde::{Deserialize, Serialize};

use crate::daemon::session_badge::{BadgeState, BadgeStateCounts};
use crate::daemon::{SidebarFrame, TransitionEvent};
use crate::pane_state::{
    DaemonInstanceId, EventId, MAX_REQUEST_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES,
    PaneEventEnvelope, PaneInstance, PaneStateLoadError, ResolvedPaneState, StateVersion,
    StoredStateDescriptor, ViewEvent,
};

pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DaemonPhase {
    InstallingHooks,
    Hydrating,
    Serving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum HookHealth {
    Healthy,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StatusContext {
    Global,
    Session { session_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLinkPresentation {
    pub session_id: String,
    pub session_name: String,
    pub window_index: i64,
    pub window_active: bool,
    pub window_last: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanePresentation {
    pub pane_instance: PaneInstance,
    pub session_links: Vec<SessionLinkPresentation>,
    pub window_id: String,
    pub window_name: String,
    pub current_path: String,
    pub current_command: String,
    pub active: bool,
    pub stored: Option<StoredStateDescriptor>,
    pub resolved: Option<ResolvedPaneState>,
    pub diagnostic: Option<PaneStateLoadError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionEntry {
    pub pane_instance: PaneInstance,
    pub session_name: String,
    pub badge: BadgeState,
    pub reason: Option<String>,
    pub elapsed_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonDiagnostic {
    pub code: ErrorCode,
    pub message: String,
    pub pane_instance: Option<PaneInstance>,
    pub event_id: Option<EventId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedSnapshot {
    pub snapshot_revision: u64,
    pub panes: Vec<PanePresentation>,
    pub sidebar: SidebarFrame,
    pub attention: Vec<AttentionEntry>,
    pub events: Vec<TransitionEvent>,
    pub diagnostics: Vec<DaemonDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusPresentation {
    pub session_id: String,
    pub session_name: String,
    pub counts: BadgeStateCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowStatusPresentation {
    pub window_id: String,
    pub window_name: String,
    pub counts: BadgeStateCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryStatusPresentation {
    pub category: String,
    pub counts: BadgeStateCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusSnapshot {
    pub snapshot_revision: u64,
    pub context: StatusContext,
    pub summary: BadgeStateCounts,
    pub sessions: Vec<SessionStatusPresentation>,
    pub windows: Vec<WindowStatusPresentation>,
    pub categories: Vec<CategoryStatusPresentation>,
    pub attention: Vec<AttentionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SidebarCommand {
    Key {
        key: String,
    },
    JumpPane {
        pane_id: String,
    },
    MarkDone {
        pane_instance: PaneInstance,
        expected: StateVersion,
    },
    SelectContext {
        pane_id: Option<String>,
        session_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum ClientMessage {
    Hello {
        proto: u16,
    },
    QueryResolvedSnapshot {
        proto: u16,
    },
    QueryStatusSnapshot {
        proto: u16,
        context: StatusContext,
    },
    QueryPane {
        proto: u16,
        pane_id: String,
    },
    Subscribe {
        proto: u16,
    },
    SubmitPaneEvent {
        proto: u16,
        envelope: PaneEventEnvelope,
    },
    SubmitViewEvent {
        proto: u16,
        event: ViewEvent,
    },
    SidebarCommand {
        proto: u16,
        daemon_instance_id: DaemonInstanceId,
        event_id: EventId,
        command: SidebarCommand,
    },
    RefreshPanes {
        proto: u16,
        daemon_instance_id: DaemonInstanceId,
        event_id: EventId,
    },
    RefreshTopology {
        proto: u16,
        daemon_instance_id: DaemonInstanceId,
        event_id: EventId,
    },
    ResetPaneState {
        proto: u16,
        daemon_instance_id: DaemonInstanceId,
        event_id: EventId,
        pane_instance: PaneInstance,
        expected: StoredStateDescriptor,
    },
    CleanupLegacyState {
        proto: u16,
        daemon_instance_id: DaemonInstanceId,
        event_id: EventId,
    },
    UninstallHooks {
        proto: u16,
        daemon_instance_id: DaemonInstanceId,
        event_id: EventId,
    },
    Shutdown {
        proto: u16,
        daemon_instance_id: DaemonInstanceId,
        event_id: EventId,
    },
}

impl ClientMessage {
    pub fn proto(&self) -> u16 {
        match self {
            Self::Hello { proto }
            | Self::QueryResolvedSnapshot { proto }
            | Self::QueryStatusSnapshot { proto, .. }
            | Self::QueryPane { proto, .. }
            | Self::Subscribe { proto }
            | Self::SubmitPaneEvent { proto, .. }
            | Self::SubmitViewEvent { proto, .. }
            | Self::SidebarCommand { proto, .. }
            | Self::RefreshPanes { proto, .. }
            | Self::RefreshTopology { proto, .. }
            | Self::ResetPaneState { proto, .. }
            | Self::CleanupLegacyState { proto, .. }
            | Self::UninstallHooks { proto, .. }
            | Self::Shutdown { proto, .. } => *proto,
        }
    }

    pub fn mutation_instance_id(&self) -> Option<&DaemonInstanceId> {
        match self {
            Self::SubmitPaneEvent { envelope, .. } => Some(&envelope.daemon_instance_id),
            Self::SubmitViewEvent { event, .. } => Some(&event.daemon_instance_id),
            Self::SidebarCommand {
                daemon_instance_id, ..
            }
            | Self::RefreshPanes {
                daemon_instance_id, ..
            }
            | Self::RefreshTopology {
                daemon_instance_id, ..
            }
            | Self::ResetPaneState {
                daemon_instance_id, ..
            }
            | Self::CleanupLegacyState {
                daemon_instance_id, ..
            }
            | Self::UninstallHooks {
                daemon_instance_id, ..
            }
            | Self::Shutdown {
                daemon_instance_id, ..
            } => Some(daemon_instance_id),
            _ => None,
        }
    }

    pub fn event_id(&self) -> Option<&EventId> {
        match self {
            Self::SubmitPaneEvent { envelope, .. } => Some(&envelope.event_id),
            Self::SubmitViewEvent { event, .. } => Some(&event.event_id),
            Self::SidebarCommand { event_id, .. }
            | Self::RefreshPanes { event_id, .. }
            | Self::RefreshTopology { event_id, .. }
            | Self::ResetPaneState { event_id, .. }
            | Self::CleanupLegacyState { event_id, .. }
            | Self::UninstallHooks { event_id, .. }
            | Self::Shutdown { event_id, .. } => Some(event_id),
            _ => None,
        }
    }

    pub fn is_mutation(&self) -> bool {
        self.mutation_instance_id().is_some()
    }

    pub fn is_query(&self) -> bool {
        matches!(
            self,
            Self::QueryResolvedSnapshot { .. }
                | Self::QueryStatusSnapshot { .. }
                | Self::QueryPane { .. }
                | Self::Subscribe { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PaneApplyOutcome {
    Noop,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResetOutcome {
    Replaced,
    AlreadyReset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneMutationFailure {
    pub pane_instance: PaneInstance,
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ViewApplyResult {
    Noop {
        snapshot_revision: u64,
    },
    TopologyOnly {
        snapshot_revision: u64,
    },
    Committed {
        snapshot_revision: u64,
        panes: usize,
    },
    Partial {
        snapshot_revision: u64,
        committed: usize,
        failed: Vec<PaneMutationFailure>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyCleanupFailure {
    pub scope: String,
    pub target: String,
    pub option: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ErrorCode {
    UnsupportedProtocol,
    NotReady,
    InvalidRequest,
    InvalidPaneInstance,
    PaneNotFound,
    StaleStateIdentity,
    StaleSelection,
    StaleAgentEvent,
    StaleDaemonInstance,
    InvalidProgressOperation,
    StateInvariantViolation,
    StateTooLarge,
    StateLoadError,
    PersistFailed,
    HookCollision,
    WriterLeaseHeld,
    QueueFull,
    FrameTooLarge,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorDetails {
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum ServerMessage {
    HelloAck {
        proto: u16,
        daemon_instance_id: DaemonInstanceId,
        server_identity: String,
        phase: DaemonPhase,
        hook_health: HookHealth,
    },
    ResolvedSnapshotResult {
        snapshot_revision: u64,
        snapshot: ResolvedSnapshot,
    },
    StatusSnapshotResult {
        snapshot_revision: u64,
        snapshot: StatusSnapshot,
    },
    PaneResult {
        snapshot_revision: u64,
        pane: PanePresentation,
    },
    PaneEventResult {
        event_id: EventId,
        accepted_seq: u64,
        state_version: Option<StateVersion>,
        snapshot_revision: u64,
        outcome: PaneApplyOutcome,
    },
    ViewQueued {
        event_id: EventId,
        accepted_seq: u64,
    },
    ViewResult {
        event_id: EventId,
        accepted_seq: u64,
        result: ViewApplyResult,
    },
    ResetResult {
        event_id: EventId,
        accepted_seq: u64,
        previous: StoredStateDescriptor,
        current: StoredStateDescriptor,
        outcome: ResetOutcome,
        snapshot_revision: u64,
    },
    CleanupLegacyResult {
        event_id: EventId,
        accepted_seq: u64,
        attempted: u64,
        removed: u64,
        failed: Vec<LegacyCleanupFailure>,
        snapshot_revision: u64,
    },
    HooksUninstalled {
        event_id: EventId,
        accepted_seq: u64,
    },
    ShutdownAccepted {
        event_id: EventId,
        accepted_seq: u64,
    },
    SnapshotAck {
        event_id: EventId,
        accepted_seq: u64,
        snapshot_revision: u64,
    },
    Error {
        code: ErrorCode,
        message: String,
        event_id: Option<EventId>,
        details: Option<ErrorDetails>,
    },
}

impl ServerMessage {
    pub fn error(code: ErrorCode, message: impl Into<String>, event_id: Option<EventId>) -> Self {
        Self::Error {
            code,
            message: message.into(),
            event_id,
            details: None,
        }
    }
}

#[allow(clippy::result_large_err)]
pub fn decode_request_frame(frame: &[u8]) -> Result<ClientMessage, ServerMessage> {
    if frame.len() > MAX_REQUEST_FRAME_BYTES {
        return Err(ServerMessage::error(
            ErrorCode::FrameTooLarge,
            "request frame exceeds 1 MiB",
            None,
        ));
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(frame)
        && value
            .get("proto")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|proto| proto != u64::from(PROTOCOL_VERSION))
    {
        return Err(ServerMessage::error(
            ErrorCode::UnsupportedProtocol,
            "protocol version 2 is required",
            None,
        ));
    }
    serde_json::from_slice(frame)
        .map_err(|error| ServerMessage::error(ErrorCode::InvalidRequest, error.to_string(), None))
}

#[allow(clippy::result_large_err)]
pub fn encode_response_frame(message: &ServerMessage) -> Result<Vec<u8>, ServerMessage> {
    let mut frame = serde_json::to_vec(message)
        .map_err(|error| ServerMessage::error(ErrorCode::InternalError, error.to_string(), None))?;
    if frame.len().saturating_add(1) > MAX_RESPONSE_FRAME_BYTES {
        return Err(ServerMessage::error(
            ErrorCode::FrameTooLarge,
            "response frame exceeds 16 MiB",
            None,
        ));
    }
    frame.push(b'\n');
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane_state::{AgentKind, AgentSessionId, PaneEvent, StateId};

    const EVENT_ID: &str = "102132435465768798a9bacbdcedfe0f";
    const DAEMON_ID: &str = "ffeeddccbbaa99887766554433221100";

    fn daemon_id() -> DaemonInstanceId {
        DaemonInstanceId::parse(DAEMON_ID).unwrap()
    }

    fn event_id() -> EventId {
        EventId::parse(EVENT_ID).unwrap()
    }

    fn pane() -> PaneInstance {
        PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        }
    }

    fn pane_event() -> ClientMessage {
        ClientMessage::SubmitPaneEvent {
            proto: PROTOCOL_VERSION,
            envelope: PaneEventEnvelope {
                daemon_instance_id: daemon_id(),
                event_id: event_id(),
                pane_instance: pane(),
                agent: Some(AgentKind::parse("codex").unwrap()),
                agent_session_id: Some(AgentSessionId::parse("session").unwrap()),
                event: PaneEvent::BeginRun {
                    started_at: 1,
                    prompt: None,
                },
            },
        }
    }

    #[test]
    fn every_client_message_roundtrips() {
        let state_id = StateId::parse("00112233445566778899aabbccddeeff").unwrap();
        let messages = vec![
            ClientMessage::Hello { proto: 2 },
            ClientMessage::QueryResolvedSnapshot { proto: 2 },
            ClientMessage::QueryStatusSnapshot {
                proto: 2,
                context: StatusContext::Global,
            },
            ClientMessage::QueryPane {
                proto: 2,
                pane_id: "%1".to_string(),
            },
            ClientMessage::Subscribe { proto: 2 },
            pane_event(),
            ClientMessage::SubmitViewEvent {
                proto: 2,
                event: ViewEvent {
                    daemon_instance_id: daemon_id(),
                    event_id: event_id(),
                    hook_kind: crate::pane_state::ViewHookKind::ClientDetached,
                    occurrence: None,
                    source_client: None,
                    witnesses: Vec::new(),
                },
            },
            ClientMessage::SidebarCommand {
                proto: 2,
                daemon_instance_id: daemon_id(),
                event_id: event_id(),
                command: SidebarCommand::MarkDone {
                    pane_instance: pane(),
                    expected: StateVersion {
                        state_id,
                        agent_epoch: 1,
                        revision: 1,
                    },
                },
            },
            ClientMessage::RefreshPanes {
                proto: 2,
                daemon_instance_id: daemon_id(),
                event_id: event_id(),
            },
            ClientMessage::RefreshTopology {
                proto: 2,
                daemon_instance_id: daemon_id(),
                event_id: event_id(),
            },
            ClientMessage::ResetPaneState {
                proto: 2,
                daemon_instance_id: daemon_id(),
                event_id: event_id(),
                pane_instance: pane(),
                expected: StoredStateDescriptor::Quarantined {
                    quarantine_id: "hash".to_string(),
                },
            },
            ClientMessage::CleanupLegacyState {
                proto: 2,
                daemon_instance_id: daemon_id(),
                event_id: event_id(),
            },
            ClientMessage::UninstallHooks {
                proto: 2,
                daemon_instance_id: daemon_id(),
                event_id: event_id(),
            },
            ClientMessage::Shutdown {
                proto: 2,
                daemon_instance_id: daemon_id(),
                event_id: event_id(),
            },
        ];
        for message in messages {
            let json = serde_json::to_vec(&message).unwrap();
            assert_eq!(decode_request_frame(&json).unwrap(), message);
        }
    }

    #[test]
    fn unknown_fields_and_oversized_frames_are_rejected() {
        let json = br#"{"op":"hello","proto":2,"unknown":true}"#;
        assert!(matches!(
            decode_request_frame(json),
            Err(ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));
        let oversized = vec![b'x'; MAX_REQUEST_FRAME_BYTES + 1];
        assert!(matches!(
            decode_request_frame(&oversized),
            Err(ServerMessage::Error {
                code: ErrorCode::FrameTooLarge,
                ..
            })
        ));
    }

    #[test]
    fn v1_shape_does_not_deserialize_as_v2() {
        let v1 = br#"{"op":"query","proto":1,"what":"summary"}"#;
        assert!(matches!(
            decode_request_frame(v1),
            Err(ServerMessage::Error {
                code: ErrorCode::UnsupportedProtocol,
                ..
            })
        ));
    }

    #[test]
    fn server_error_response_roundtrips_with_newline_frame() {
        let message = ServerMessage::error(
            ErrorCode::UnsupportedProtocol,
            "unsupported",
            Some(event_id()),
        );
        let frame = encode_response_frame(&message).unwrap();
        assert_eq!(frame.last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<ServerMessage>(&frame[..frame.len() - 1]).unwrap(),
            message
        );
    }

    #[test]
    fn every_server_message_roundtrips() {
        let pane_presentation = PanePresentation {
            pane_instance: pane(),
            session_links: Vec::new(),
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            current_path: "/tmp".to_string(),
            current_command: "zsh".to_string(),
            active: true,
            stored: None,
            resolved: None,
            diagnostic: None,
        };
        let sidebar = SidebarFrame {
            state: crate::sidebar::state::SidebarState::default(),
            counts: crate::sidebar::tree::BadgeCounts::default(),
            rows: Vec::new(),
        };
        let resolved = ResolvedSnapshot {
            snapshot_revision: 1,
            panes: vec![pane_presentation.clone()],
            sidebar,
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        };
        let status = StatusSnapshot {
            snapshot_revision: 1,
            context: StatusContext::Global,
            summary: BadgeStateCounts::default(),
            sessions: Vec::new(),
            windows: Vec::new(),
            categories: Vec::new(),
            attention: Vec::new(),
        };
        let descriptor = StoredStateDescriptor::Quarantined {
            quarantine_id: "hash".to_string(),
        };
        let messages = vec![
            ServerMessage::HelloAck {
                proto: 2,
                daemon_instance_id: daemon_id(),
                server_identity: "server".to_string(),
                phase: DaemonPhase::Serving,
                hook_health: HookHealth::Healthy,
            },
            ServerMessage::ResolvedSnapshotResult {
                snapshot_revision: 1,
                snapshot: resolved,
            },
            ServerMessage::StatusSnapshotResult {
                snapshot_revision: 1,
                snapshot: status,
            },
            ServerMessage::PaneResult {
                snapshot_revision: 1,
                pane: pane_presentation,
            },
            ServerMessage::PaneEventResult {
                event_id: event_id(),
                accepted_seq: 1,
                state_version: None,
                snapshot_revision: 1,
                outcome: PaneApplyOutcome::Noop,
            },
            ServerMessage::ViewQueued {
                event_id: event_id(),
                accepted_seq: 2,
            },
            ServerMessage::ViewResult {
                event_id: event_id(),
                accepted_seq: 2,
                result: ViewApplyResult::Noop {
                    snapshot_revision: 1,
                },
            },
            ServerMessage::ResetResult {
                event_id: event_id(),
                accepted_seq: 3,
                previous: descriptor.clone(),
                current: descriptor,
                outcome: ResetOutcome::AlreadyReset,
                snapshot_revision: 1,
            },
            ServerMessage::CleanupLegacyResult {
                event_id: event_id(),
                accepted_seq: 4,
                attempted: 0,
                removed: 0,
                failed: Vec::new(),
                snapshot_revision: 1,
            },
            ServerMessage::HooksUninstalled {
                event_id: event_id(),
                accepted_seq: 5,
            },
            ServerMessage::ShutdownAccepted {
                event_id: event_id(),
                accepted_seq: 6,
            },
            ServerMessage::SnapshotAck {
                event_id: event_id(),
                accepted_seq: 7,
                snapshot_revision: 1,
            },
            ServerMessage::error(ErrorCode::InternalError, "error", Some(event_id())),
        ];
        for message in messages {
            let encoded = serde_json::to_vec(&message).unwrap();
            assert_eq!(
                serde_json::from_slice::<ServerMessage>(&encoded).unwrap(),
                message
            );
        }
    }
}

use std::collections::{BTreeMap, BTreeSet};

use crate::daemon::protocol::v2::{ClientMessage, DaemonPhase, HookHealth, ServerMessage};
use crate::pane_state::{DaemonInstanceId, PaneEventEnvelope, PaneInstance};

use super::V2_BOOTSTRAP_FIFO_CAPACITY;
use super::contracts::SidebarEffectCompletion;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Default)]
pub(super) struct V2ConnectionState {
    pub(super) hello_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct V2SequencedMutation {
    pub(super) accepted_seq: u64,
    pub(super) mutation: V2AcceptedMutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub(super) enum V2AcceptedMutation {
    External(ClientMessage),
    Internal(V2InternalMutation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum V2InternalMutation {
    PaneEvent(Box<PaneEventEnvelope>),
    ObservationBatch(Box<ObservationBatchPayload>),
    RefreshTopology,
    TargetedPaneRefresh {
        pane_id: String,
    },
    ReconcileViews,
    CurrentViewsReplacement {
        observation_seq: u64,
        witnesses: Vec<crate::pane_state::ClientWitness>,
        through_unread_order: u64,
    },
    GitProjection {
        badges: std::collections::BTreeMap<String, crate::git::GitBadge>,
        worktrees: std::collections::BTreeMap<String, crate::git::WorktreeInfo>,
        repo_identities: std::collections::BTreeMap<String, crate::category::RepoIdentity>,
    },
    DiagnosticProjection {
        pane_instance: Option<PaneInstance>,
        message: String,
    },
    FrameTooLargeProjection {
        rejected_revision: u64,
    },
    HookHealthProjection {
        health: HookHealth,
        diagnostic: Option<String>,
    },
    AgentPromptTimeouts {
        observed_at: i64,
    },
    TaskSummaryCompleted(crate::daemon::task_summary::TaskSummaryCompletion),
    SidebarEffectCompleted(SidebarEffectCompletion),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservationPollProjection {
    pub(super) observation_seq: u64,
    pub(super) topology: crate::daemon::topology::TopologySnapshot,
    pub(super) status_metadata: crate::daemon::runtime::StatusProjectionMetadata,
    pub(super) witnesses: Vec<crate::pane_state::ClientWitness>,
    pub(super) observation_bases:
        BTreeMap<PaneInstance, Option<crate::pane_state::StoredStateDescriptor>>,
    pub(super) view_base: crate::daemon::view_hooks::CurrentClientViews,
    pub(super) through_unread_order: u64,
}

/// One successful observation poll as a single sequenced mutation. Application
/// order is fixed: projection, observation pane events, pane removals,
/// diagnostics, then a trailing triage pass; the snapshot is published once
/// after the whole batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservationBatchPayload {
    pub(super) projection: Box<ObservationPollProjection>,
    pub(super) observations: Vec<PaneEventEnvelope>,
    pub(super) removals: Vec<PaneEventEnvelope>,
    pub(super) diagnostics: Vec<(Option<PaneInstance>, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub(super) enum V2Route {
    Response(ServerMessage),
    Fatal(ServerMessage),
    Query(ClientMessage),
    Mutation(V2SequencedMutation),
    Queued { accepted_seq: u64 },
    DroppedInternal,
}

#[derive(Debug, Clone)]
pub(super) struct V2Router {
    pub(super) daemon_instance_id: DaemonInstanceId,
    pub(super) server_identity: String,
    pub(super) phase: DaemonPhase,
    pub(super) hook_health: HookHealth,
    pub(super) next_accepted_seq: u64,
    pub(super) bootstrap_fifo: std::collections::VecDeque<V2SequencedMutation>,
    pub(super) fatal: bool,
}

impl V2Router {
    pub(super) fn new(
        daemon_instance_id: DaemonInstanceId,
        server_identity: impl Into<String>,
    ) -> Self {
        Self {
            daemon_instance_id,
            server_identity: server_identity.into(),
            phase: DaemonPhase::InstallingHooks,
            hook_health: HookHealth::Healthy,
            next_accepted_seq: 1,
            bootstrap_fifo: std::collections::VecDeque::new(),
            fatal: false,
        }
    }

    pub(super) fn phase(&self) -> DaemonPhase {
        self.phase
    }

    pub(super) fn daemon_instance_id(&self) -> &DaemonInstanceId {
        &self.daemon_instance_id
    }

    #[cfg(test)]
    pub(super) fn set_phase(&mut self, phase: DaemonPhase) {
        self.phase = phase;
    }

    pub(super) fn begin_hydration(&mut self) -> Result<(), &'static str> {
        if self.phase != DaemonPhase::InstallingHooks {
            return Err("daemon may enter hydration only after hook installation");
        }
        self.phase = DaemonPhase::Hydrating;
        Ok(())
    }

    pub(super) fn set_hook_health(&mut self, health: HookHealth) {
        self.hook_health = health;
    }

    pub(super) fn hook_health(&self) -> HookHealth {
        self.hook_health
    }

    #[cfg(test)]
    pub(super) fn is_fatal(&self) -> bool {
        self.fatal
    }

    pub(super) fn mark_fatal(&mut self) {
        self.fatal = true;
    }

    pub(super) fn route(
        &mut self,
        connection: &mut V2ConnectionState,
        message: ClientMessage,
    ) -> V2Route {
        use crate::daemon::protocol::v2::{
            ClientMessage as V2ClientMessage, ErrorCode, PROTOCOL_VERSION,
            ServerMessage as V2ServerMessage,
        };

        if self.fatal {
            return V2Route::Fatal(V2ServerMessage::error(
                ErrorCode::InternalError,
                "daemon router is fail-stopped",
                message.event_id().cloned(),
            ));
        }

        if !connection.hello_complete {
            return match message {
                V2ClientMessage::Hello { proto } if proto == PROTOCOL_VERSION => {
                    connection.hello_complete = true;
                    V2Route::Response(V2ServerMessage::HelloAck {
                        proto: PROTOCOL_VERSION,
                        daemon_instance_id: self.daemon_instance_id.clone(),
                        server_identity: self.server_identity.clone(),
                        phase: self.phase,
                        hook_health: self.hook_health,
                    })
                }
                V2ClientMessage::Hello { .. } => V2Route::Response(V2ServerMessage::error(
                    ErrorCode::UnsupportedProtocol,
                    crate::daemon::protocol::v2::protocol_requirement_message(),
                    None,
                )),
                _ => V2Route::Response(V2ServerMessage::error(
                    ErrorCode::InvalidRequest,
                    "Hello must be the first message on a connection",
                    None,
                )),
            };
        }

        if message.proto() != PROTOCOL_VERSION {
            return V2Route::Response(V2ServerMessage::error(
                ErrorCode::UnsupportedProtocol,
                crate::daemon::protocol::v2::protocol_requirement_message(),
                message.event_id().cloned(),
            ));
        }
        if matches!(message, V2ClientMessage::Hello { .. }) {
            return V2Route::Response(V2ServerMessage::error(
                ErrorCode::InvalidRequest,
                "Hello may only be sent once",
                None,
            ));
        }
        if let Some(instance_id) = message.mutation_instance_id()
            && instance_id != &self.daemon_instance_id
        {
            return V2Route::Response(V2ServerMessage::error(
                ErrorCode::StaleDaemonInstance,
                "mutation targets a stale daemon instance",
                message.event_id().cloned(),
            ));
        }
        if let Err(error) = validate_v2_origin(&message) {
            return V2Route::Response(error);
        }

        if message.is_query() {
            if self.phase != DaemonPhase::Serving {
                return V2Route::Response(V2ServerMessage::error(
                    ErrorCode::NotReady,
                    format!("daemon phase is {:?}", self.phase),
                    None,
                ));
            }
            return V2Route::Query(message);
        }
        if !message.is_mutation() {
            return V2Route::Response(V2ServerMessage::error(
                ErrorCode::InvalidRequest,
                "unsupported message",
                None,
            ));
        }
        if self.phase != DaemonPhase::Serving
            && self.bootstrap_fifo.len() >= V2_BOOTSTRAP_FIFO_CAPACITY
        {
            return V2Route::Response(V2ServerMessage::error(
                ErrorCode::QueueFull,
                "bootstrap FIFO is full",
                message.event_id().cloned(),
            ));
        }

        let accepted_seq = match self.allocate_accepted_seq() {
            Some(accepted_seq) => accepted_seq,
            None => {
                return V2Route::Fatal(V2ServerMessage::error(
                    ErrorCode::InternalError,
                    "accepted sequence overflow",
                    message.event_id().cloned(),
                ));
            }
        };
        let event_id = message.event_id().cloned();
        let is_view = matches!(message, V2ClientMessage::SubmitViewEvent { .. });
        let mutation = V2SequencedMutation {
            accepted_seq,
            mutation: V2AcceptedMutation::External(message),
        };
        if self.phase == DaemonPhase::Serving {
            return V2Route::Mutation(mutation);
        }
        self.bootstrap_fifo.push_back(mutation);
        if is_view {
            V2Route::Response(V2ServerMessage::ViewQueued {
                event_id: event_id.expect("view mutation has event ID"),
                accepted_seq,
            })
        } else {
            V2Route::Queued { accepted_seq }
        }
    }

    #[cfg(test)]
    pub(super) fn finish_bootstrap<E>(
        &mut self,
        apply_fifo_and_reconcile: impl FnOnce(Vec<V2SequencedMutation>) -> Result<(), E>,
    ) -> Result<(), E> {
        assert_eq!(
            self.phase,
            DaemonPhase::Hydrating,
            "bootstrap may finish only from Hydrating"
        );
        let queued = self.bootstrap_fifo.drain(..).collect();
        apply_fifo_and_reconcile(queued)?;
        self.phase = DaemonPhase::Serving;
        Ok(())
    }

    pub(super) fn take_bootstrap_fifo(&mut self) -> Vec<V2SequencedMutation> {
        assert_ne!(
            self.phase,
            DaemonPhase::Serving,
            "Serving router has no bootstrap FIFO"
        );
        self.bootstrap_fifo.drain(..).collect()
    }

    pub(super) fn enter_serving_if_bootstrap_empty(&mut self) -> bool {
        if self.phase == DaemonPhase::Hydrating && self.bootstrap_fifo.is_empty() {
            self.phase = DaemonPhase::Serving;
            true
        } else {
            false
        }
    }

    pub(super) fn accept_internal(&mut self, mutation: V2InternalMutation) -> V2Route {
        use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

        if self.fatal {
            return V2Route::Fatal(ServerMessage::error(
                ErrorCode::InternalError,
                "daemon router is fail-stopped",
                None,
            ));
        }
        if self.phase != DaemonPhase::Serving
            && self.bootstrap_fifo.len() >= V2_BOOTSTRAP_FIFO_CAPACITY
        {
            return V2Route::DroppedInternal;
        }
        let accepted_seq = match self.allocate_accepted_seq() {
            Some(accepted_seq) => accepted_seq,
            None => {
                return V2Route::Fatal(ServerMessage::error(
                    ErrorCode::InternalError,
                    "accepted sequence overflow",
                    None,
                ));
            }
        };
        let mutation = V2SequencedMutation {
            accepted_seq,
            mutation: V2AcceptedMutation::Internal(mutation),
        };
        if self.phase == DaemonPhase::Serving {
            V2Route::Mutation(mutation)
        } else {
            self.bootstrap_fifo.push_back(mutation);
            V2Route::Queued { accepted_seq }
        }
    }

    fn allocate_accepted_seq(&mut self) -> Option<u64> {
        match self.next_accepted_seq.checked_add(1) {
            Some(next) => {
                let accepted = self.next_accepted_seq;
                self.next_accepted_seq = next;
                Some(accepted)
            }
            None => {
                self.fatal = true;
                None
            }
        }
    }

    #[cfg(test)]
    pub(super) fn set_next_accepted_seq(&mut self, value: u64) {
        self.next_accepted_seq = value;
    }
}

#[allow(clippy::result_large_err)]
pub(super) fn validate_v2_origin(
    message: &ClientMessage,
) -> std::result::Result<(), ServerMessage> {
    use crate::daemon::protocol::v2::{ClientMessage, ErrorCode, ServerMessage};
    match message {
        ClientMessage::SubmitPaneEvent { envelope, .. } if !envelope.event.is_external() => {
            Err(ServerMessage::error(
                ErrorCode::InvalidRequest,
                "pane event variant is internal-only",
                Some(envelope.event_id.clone()),
            ))
        }
        ClientMessage::SubmitProviderEvent {
            envelope,
            observation,
            ..
        } => {
            if !envelope.event.is_external() {
                return Err(ServerMessage::error(
                    ErrorCode::InvalidRequest,
                    "provider pane event variant is internal-only",
                    Some(envelope.event_id.clone()),
                ));
            }
            observation.validate().map_err(|error| {
                ServerMessage::error(
                    ErrorCode::InvalidRequest,
                    error.to_string(),
                    Some(envelope.event_id.clone()),
                )
            })?;
            if observation.ingress_request_id != envelope.event_id
                || envelope.agent.as_ref() != Some(&observation.provider)
                || envelope.agent_session_id.as_ref() != Some(&observation.session_id)
            {
                return Err(ServerMessage::error(
                    ErrorCode::InvalidRequest,
                    "provider observation identity does not match its pane event envelope",
                    Some(envelope.event_id.clone()),
                ));
            }
            Ok(())
        }
        ClientMessage::SubmitViewEvent { event, .. } => event.validate().map_err(|error| {
            ServerMessage::error(
                ErrorCode::InvalidRequest,
                error.to_string(),
                Some(event.event_id.clone()),
            )
        }),
        ClientMessage::SidebarCommand {
            event_id,
            command:
                crate::daemon::protocol::v2::SidebarCommand::PeekPane {
                    pane_instance,
                    source_pane,
                    client_pid,
                },
            ..
        } => {
            if *client_pid == 0 {
                return Err(ServerMessage::error(
                    ErrorCode::InvalidRequest,
                    "peek client PID must be positive",
                    Some(event_id.clone()),
                ));
            }
            for pane in [pane_instance, source_pane] {
                if let Err(error) = pane.validate() {
                    return Err(ServerMessage::error(
                        ErrorCode::InvalidPaneInstance,
                        error.to_string(),
                        Some(event_id.clone()),
                    ));
                }
            }
            Ok(())
        }
        ClientMessage::SidebarCommand {
            event_id,
            command:
                crate::daemon::protocol::v2::SidebarCommand::ReadPeek {
                    source_pane,
                    client_pid,
                    advance_candidates,
                },
            ..
        } => {
            if *client_pid == 0 || advance_candidates.len() > crate::pane_state::MAX_VIEW_PANES {
                return Err(ServerMessage::error(
                    ErrorCode::InvalidRequest,
                    "read-current client PID or advance candidate list is invalid",
                    Some(event_id.clone()),
                ));
            }
            if let Err(error) = source_pane.validate() {
                return Err(ServerMessage::error(
                    ErrorCode::InvalidPaneInstance,
                    error.to_string(),
                    Some(event_id.clone()),
                ));
            }
            let mut seen = BTreeSet::new();
            for pane in advance_candidates {
                if let Err(error) = pane.validate() {
                    return Err(ServerMessage::error(
                        ErrorCode::InvalidPaneInstance,
                        error.to_string(),
                        Some(event_id.clone()),
                    ));
                }
                if !seen.insert(pane) {
                    return Err(ServerMessage::error(
                        ErrorCode::InvalidRequest,
                        "read-current advance candidates must be unique",
                        Some(event_id.clone()),
                    ));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

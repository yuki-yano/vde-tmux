use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine as _;

use crate::daemon::protocol::v2::{
    ClientMessage, DaemonPhase, ErrorCode, HookHealth, ServerMessage,
};
use crate::pane_state::{DaemonInstanceId, EventId, PaneEvent, PaneEventEnvelope, PaneInstance};
use crate::tmux::TmuxRunner;

static SHUTDOWN_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

const V2_BOOTSTRAP_FIFO_CAPACITY: usize = 64;
const V2_MUTATION_QUEUE_CAPACITY: usize = 1024;
// Bound all socket-owned threads, including clients stalled during the
// handshake. Long-lived subscriptions have a lower ceiling so routine hook,
// mutation, and query traffic retains capacity under a subscriber burst.
const V2_CONNECTION_THREAD_CAPACITY: usize = 64;
const V2_RESERVED_NON_STREAMING_CONNECTION_CAPACITY: usize = 16;
pub const V2_FRAME_START_TIMEOUT: Duration = Duration::from_secs(2);
pub const V2_FRAME_BODY_TIMEOUT: Duration = Duration::from_millis(100);
pub const V2_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const V2_OVERLOAD_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_millis(25);
const TMUX_SERVER_LIVENESS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CURRENT_VIEW_REFRESH_DEBOUNCE: Duration = Duration::from_millis(100);
const PANE_SWITCH_REQUEST_SEPARATOR: &str = "__vde_pane_switch_request__";

#[derive(Default)]
struct V2ConnectionThreadCounts {
    active: usize,
    streaming: usize,
}

struct V2ConnectionThreadLimiter {
    capacity: usize,
    streaming_capacity: usize,
    counts: Mutex<V2ConnectionThreadCounts>,
}

impl V2ConnectionThreadLimiter {
    fn new(capacity: usize, reserved_non_streaming: usize) -> Self {
        assert!(reserved_non_streaming <= capacity);
        Self {
            capacity,
            streaming_capacity: capacity - reserved_non_streaming,
            counts: Mutex::new(V2ConnectionThreadCounts::default()),
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<V2ConnectionThreadPermit> {
        let mut counts = self
            .counts
            .lock()
            .expect("v2 connection thread limiter lock poisoned");
        if counts.active >= self.capacity {
            return None;
        }
        counts.active += 1;
        Some(V2ConnectionThreadPermit {
            limiter: self.clone(),
            streaming: false,
        })
    }
}

struct V2ConnectionThreadPermit {
    limiter: Arc<V2ConnectionThreadLimiter>,
    streaming: bool,
}

impl V2ConnectionThreadPermit {
    fn try_mark_streaming(&mut self) -> bool {
        if self.streaming {
            return true;
        }
        let mut counts = self
            .limiter
            .counts
            .lock()
            .expect("v2 connection thread limiter lock poisoned");
        if counts.streaming >= self.limiter.streaming_capacity {
            return false;
        }
        counts.streaming += 1;
        self.streaming = true;
        true
    }
}

impl Drop for V2ConnectionThreadPermit {
    fn drop(&mut self) {
        let mut counts = self
            .limiter
            .counts
            .lock()
            .expect("v2 connection thread limiter lock poisoned");
        counts.active = counts
            .active
            .checked_sub(1)
            .expect("v2 connection thread permit released once");
        if self.streaming {
            counts.streaming = counts
                .streaming
                .checked_sub(1)
                .expect("v2 streaming connection permit released once");
        }
    }
}

pub struct V2FrameReader {
    reader: BufReader<UnixStream>,
}

impl V2FrameReader {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            reader: BufReader::new(stream),
        }
    }

    pub fn stream_mut(&mut self) -> &mut UnixStream {
        self.reader.get_mut()
    }

    pub fn into_stream(self) -> UnixStream {
        self.reader.into_inner()
    }
}

#[allow(clippy::result_large_err)]
pub fn read_v2_request_frame(
    connection: &mut V2FrameReader,
) -> std::result::Result<Vec<u8>, ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    use crate::pane_state::MAX_REQUEST_FRAME_BYTES;

    connection
        .reader
        .get_mut()
        .set_read_timeout(Some(V2_FRAME_START_TIMEOUT))
        .map_err(|error| ServerMessage::error(ErrorCode::InternalError, error.to_string(), None))?;
    let mut frame = Vec::new();
    let mut body_deadline: Option<std::time::Instant> = None;
    loop {
        if let Some(deadline) = body_deadline {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(ServerMessage::error(
                    ErrorCode::InvalidRequest,
                    "request frame body deadline exceeded",
                    None,
                ));
            };
            connection
                .reader
                .get_mut()
                .set_read_timeout(Some(remaining))
                .map_err(|error| {
                    ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
                })?;
        }
        let available = connection.reader.fill_buf().map_err(|error| {
            let stage = if body_deadline.is_some() {
                "body"
            } else {
                "start"
            };
            ServerMessage::error(
                ErrorCode::InvalidRequest,
                format!("request frame {stage} deadline exceeded: {error}"),
                None,
            )
        })?;
        if available.is_empty() {
            return Err(ServerMessage::error(
                ErrorCode::InvalidRequest,
                "connection closed before request frame completed",
                None,
            ));
        }
        if body_deadline.is_none() {
            body_deadline = Some(std::time::Instant::now() + V2_FRAME_BODY_TIMEOUT);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        let body_bytes = request_frame_body_bytes(frame.len(), take, newline.is_some());
        if body_bytes > MAX_REQUEST_FRAME_BYTES {
            return Err(ServerMessage::error(
                ErrorCode::FrameTooLarge,
                "request frame exceeds 1 MiB",
                None,
            ));
        }
        frame.extend_from_slice(&available[..take]);
        connection.reader.consume(take);
        if newline.is_some() {
            frame.pop();
            return Ok(frame);
        }
    }
}

fn request_frame_body_bytes(buffered: usize, take: usize, newline_terminated: bool) -> usize {
    buffered
        .saturating_add(take)
        .saturating_sub(usize::from(newline_terminated))
}

#[allow(clippy::result_large_err)]
pub fn write_v2_response(
    stream: &mut UnixStream,
    message: &ServerMessage,
) -> std::result::Result<(), ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage, encode_response_frame};

    let frame = match encode_response_frame(message) {
        Ok(frame) => frame,
        Err(
            error @ ServerMessage::Error {
                code: ErrorCode::FrameTooLarge,
                ..
            },
        ) => encode_response_frame(&error)?,
        Err(error) => return Err(error),
    };
    write_v2_frame(stream, &frame)
}

#[allow(clippy::result_large_err)]
fn write_v2_frame(stream: &mut UnixStream, frame: &[u8]) -> std::result::Result<(), ServerMessage> {
    write_v2_frame_with_timeout(stream, frame, V2_RESPONSE_WRITE_TIMEOUT)
}

#[allow(clippy::result_large_err)]
fn write_v2_frame_with_timeout(
    stream: &mut UnixStream,
    frame: &[u8],
    timeout: Duration,
) -> std::result::Result<(), ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let deadline = std::time::Instant::now() + timeout;
    let mut written = 0;
    while written < frame.len() {
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return Err(ServerMessage::error(
                ErrorCode::InternalError,
                "response write deadline exceeded",
                None,
            ));
        };
        let timeout = bounded_write_timeout(remaining);
        stream.set_write_timeout(Some(timeout)).map_err(|error| {
            ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
        })?;
        let count = stream.write(&frame[written..]).map_err(|error| {
            ServerMessage::error(
                ErrorCode::InternalError,
                format!("response write failed: {error}"),
                None,
            )
        })?;
        if count == 0 {
            return Err(ServerMessage::error(
                ErrorCode::InternalError,
                "response stream closed before frame completed",
                None,
            ));
        }
        written += count;
    }
    Ok(())
}

fn write_v2_overload_response(stream: &mut UnixStream) {
    let response = ServerMessage::error(
        ErrorCode::QueueFull,
        "daemon connection capacity is full",
        None,
    );
    if let Ok(frame) = crate::daemon::protocol::v2::encode_response_frame(&response) {
        let _ = write_v2_frame_with_timeout(stream, &frame, V2_OVERLOAD_RESPONSE_WRITE_TIMEOUT);
    }
}

fn bounded_write_timeout(remaining: Duration) -> Duration {
    remaining.max(Duration::from_millis(1))
}

#[derive(Debug, Clone, Default)]
pub struct V2ConnectionState {
    hello_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V2SequencedMutation {
    pub accepted_seq: u64,
    pub mutation: V2AcceptedMutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum V2AcceptedMutation {
    External(ClientMessage),
    Internal(V2InternalMutation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum V2InternalMutation {
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
pub(crate) struct ObservationPollProjection {
    observation_seq: u64,
    topology: crate::daemon::topology::TopologySnapshot,
    status_metadata: super::runtime::StatusProjectionMetadata,
    witnesses: Vec<crate::pane_state::ClientWitness>,
    observation_bases: BTreeMap<PaneInstance, Option<crate::pane_state::StoredStateDescriptor>>,
    view_base: crate::daemon::view_hooks::CurrentClientViews,
    through_unread_order: u64,
}

/// One successful observation poll as a single sequenced mutation. Application
/// order is fixed: projection, observation pane events, pane removals,
/// diagnostics, then a trailing triage pass; the snapshot is published once
/// after the whole batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservationBatchPayload {
    projection: Box<ObservationPollProjection>,
    observations: Vec<PaneEventEnvelope>,
    removals: Vec<PaneEventEnvelope>,
    diagnostics: Vec<(Option<PaneInstance>, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum V2Route {
    Response(ServerMessage),
    Fatal(ServerMessage),
    Query(ClientMessage),
    Mutation(V2SequencedMutation),
    Queued { accepted_seq: u64 },
    DroppedInternal,
}

#[derive(Debug, Clone)]
pub struct V2Router {
    daemon_instance_id: DaemonInstanceId,
    server_identity: String,
    phase: DaemonPhase,
    hook_health: HookHealth,
    next_accepted_seq: u64,
    bootstrap_fifo: std::collections::VecDeque<V2SequencedMutation>,
    fatal: bool,
}

impl V2Router {
    pub fn new(daemon_instance_id: DaemonInstanceId, server_identity: impl Into<String>) -> Self {
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

    pub fn phase(&self) -> DaemonPhase {
        self.phase
    }

    pub fn daemon_instance_id(&self) -> &DaemonInstanceId {
        &self.daemon_instance_id
    }

    #[cfg(test)]
    pub fn set_phase(&mut self, phase: DaemonPhase) {
        self.phase = phase;
    }

    pub fn begin_hydration(&mut self) -> Result<(), &'static str> {
        if self.phase != DaemonPhase::InstallingHooks {
            return Err("daemon may enter hydration only after hook installation");
        }
        self.phase = DaemonPhase::Hydrating;
        Ok(())
    }

    pub fn set_hook_health(&mut self, health: HookHealth) {
        self.hook_health = health;
    }

    pub fn hook_health(&self) -> HookHealth {
        self.hook_health
    }

    pub fn is_fatal(&self) -> bool {
        self.fatal
    }

    pub fn mark_fatal(&mut self) {
        self.fatal = true;
    }

    pub(crate) fn route(
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
    pub(crate) fn finish_bootstrap<E>(
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

    pub(crate) fn take_bootstrap_fifo(&mut self) -> Vec<V2SequencedMutation> {
        assert_ne!(
            self.phase,
            DaemonPhase::Serving,
            "Serving router has no bootstrap FIFO"
        );
        self.bootstrap_fifo.drain(..).collect()
    }

    pub(crate) fn enter_serving_if_bootstrap_empty(&mut self) -> bool {
        if self.phase == DaemonPhase::Hydrating && self.bootstrap_fifo.is_empty() {
            self.phase = DaemonPhase::Serving;
            true
        } else {
            false
        }
    }

    pub(crate) fn accept_internal(&mut self, mutation: V2InternalMutation) -> V2Route {
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
    fn set_next_accepted_seq(&mut self, value: u64) {
        self.next_accepted_seq = value;
    }
}

#[allow(clippy::result_large_err)]
fn validate_v2_origin(message: &ClientMessage) -> std::result::Result<(), ServerMessage> {
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

#[derive(Debug)]
struct ProductionMutation {
    sequenced: V2SequencedMutation,
    raw_frame_bytes: usize,
}

#[derive(Debug)]
struct NotificationWorkerJob {
    pane_id: String,
    agent: String,
}

struct SidebarTmuxJob {
    effect: super::runtime::CanonicalSidebarEffect,
    original_accepted_seq: u64,
    event_id: EventId,
    snapshot_revision: u64,
}

const NVIM_PROCESS_PID_OPTION: &str = "@vde_nvim_process_pid";

#[derive(Debug, Clone, PartialEq, Eq)]
struct NvimPaneMarker {
    pane_id: String,
    pane_pid: u32,
    process_pid: u32,
}

fn parse_nvim_pane_markers(output: &str) -> Vec<NvimPaneMarker> {
    let mut markers = BTreeMap::new();
    for line in output.lines() {
        let mut fields = line.split('\u{1f}');
        let (Some(pane_id), Some(pane_pid), Some(process_pid), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(pane_pid) = pane_pid.parse::<u32>().ok().filter(|pid| *pid > 0) else {
            continue;
        };
        let Some(process_pid) = process_pid.parse::<u32>().ok().filter(|pid| *pid > 0) else {
            continue;
        };
        if crate::daemon::topology::validate_pane_id(pane_id).is_err() {
            continue;
        }
        let marker = NvimPaneMarker {
            pane_id: pane_id.to_string(),
            pane_pid,
            process_pid,
        };
        markers.entry(marker.pane_id.clone()).or_insert(marker);
    }
    markers.into_values().collect()
}

fn stale_nvim_marker_cleanup_command(
    expected_server_pid: u32,
    markers: &[NvimPaneMarker],
) -> Option<String> {
    let mut arguments = Vec::new();
    for marker in markers {
        if !arguments.is_empty() {
            arguments.push(";".to_string());
        }
        let guard = format!(
            "#{{&&:#{{==:#{{pid}},{expected_server_pid}}},#{{&&:#{{==:#{{pane_pid}},{}}},#{{==:#{{{NVIM_PROCESS_PID_OPTION}}},{}}}}}}}",
            marker.pane_pid, marker.process_pid
        );
        let unset = crate::pane_state::store::tmux_command_string(&[
            "set-option".to_string(),
            "-pu".to_string(),
            "-t".to_string(),
            marker.pane_id.clone(),
            NVIM_PROCESS_PID_OPTION.to_string(),
        ]);
        arguments.extend([
            "if-shell".to_string(),
            "-F".to_string(),
            "-t".to_string(),
            marker.pane_id.clone(),
            guard,
            unset,
        ]);
    }
    (!arguments.is_empty()).then(|| crate::pane_state::store::tmux_command_string(&arguments))
}

fn enqueue_sidebar_tmux_job(
    tx: &SyncSender<SidebarTmuxJob>,
    deferred_responses: &Mutex<BTreeSet<u64>>,
    job: SidebarTmuxJob,
) -> std::result::Result<(), ErrorCode> {
    let original_accepted_seq = job.original_accepted_seq;
    tx.try_send(job).map_err(|error| match error {
        TrySendError::Full(_) => ErrorCode::QueueFull,
        TrySendError::Disconnected(_) => ErrorCode::InternalError,
    })?;
    deferred_responses
        .lock()
        .expect("deferred response lock poisoned")
        .insert(original_accepted_seq);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SidebarEffectCompletion {
    original_accepted_seq: u64,
    event_id: EventId,
    snapshot_revision: u64,
    witness_observation_floor: u64,
    result: SidebarEffectResult,
    effect: super::runtime::CanonicalSidebarEffect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SidebarEffectResult {
    Succeeded(PaneInstance),
    ServerIncarnationMismatch,
    PaneInstanceMismatch,
    NoAvailablePane,
    SourceClientMismatch,
    Failed(String),
}

#[derive(Debug, Default)]
struct ProductionQueue {
    items: VecDeque<ProductionMutation>,
    in_flight: bool,
}

#[derive(Debug, Clone)]
struct PublishedResolvedSnapshot {
    revision: u64,
    frame: Arc<Vec<u8>>,
    message: Arc<ServerMessage>,
    terminal: bool,
}

/// Outcome of waiting on the snapshot stream: either a newer published
/// snapshot or a keepalive deadline with the latest published revision.
#[derive(Debug, Clone)]
enum SnapshotWaitOutcome {
    Published(PublishedResolvedSnapshot),
    HeartbeatDue { snapshot_revision: u64 },
}

/// Interval without new content after which a subscription stream emits a
/// small `Heartbeat` frame instead of re-sending the full snapshot.
const V2_STREAM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
enum StatusPushTrigger {
    Snapshot,
    Flush,
}

struct ProductionV2Coordinator {
    router: Mutex<V2Router>,
    state: Mutex<Option<super::runtime::CanonicalCoordinatorState>>,
    agent_runtime: Mutex<Option<crate::agent_state::runtime::AgentRuntime>>,
    queue: Mutex<ProductionQueue>,
    queue_ready: Condvar,
    snapshot_cache: Mutex<Option<PublishedResolvedSnapshot>>,
    snapshot_changed: Condvar,
    waiters: Mutex<BTreeMap<u64, Sender<ServerMessage>>>,
    deferred_responses: Mutex<BTreeSet<u64>>,
    shutdown: AtomicBool,
    shutdown_ready: AtomicBool,
    incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
    env: std::collections::BTreeMap<String, String>,
    notification_tx: Option<SyncSender<NotificationWorkerJob>>,
    notification_shutdown: Arc<AtomicBool>,
    notification_process_lock: Arc<Mutex<()>>,
    task_summary_tx: Option<SyncSender<crate::daemon::task_summary::TaskSummaryJob>>,
    task_summary_completion_rx:
        Mutex<Option<mpsc::Receiver<crate::daemon::task_summary::TaskSummaryCompletion>>>,
    sidebar_tmux_tx: SyncSender<SidebarTmuxJob>,
    sidebar_completion_rx: Mutex<Option<mpsc::Receiver<SidebarEffectCompletion>>>,
    tmux_control: Mutex<Option<crate::daemon::tmux_control::TmuxControlHandle>>,
    pane_switch_trigger_shutdown: Arc<AtomicBool>,
    status_push: Mutex<crate::daemon::status_push::StatusPushState>,
    status_push_driver: Mutex<()>,
    status_push_started: Instant,
    config_hash: Mutex<String>,
    witness_observation_seq: Arc<AtomicU64>,
    current_view_refresh_generation: AtomicU64,
    current_view_refresh_running: AtomicBool,
}

#[cfg(test)]
fn start_notification_worker(command: String) -> SyncSender<NotificationWorkerJob> {
    start_notification_worker_with_timeout_and_log(command, Duration::from_secs(2), None)
}

fn start_sidebar_tmux_worker(
    env: &BTreeMap<String, String>,
    expected_server: crate::daemon::topology::ServerIdentity,
    witness_observation_seq: Arc<AtomicU64>,
) -> (
    SyncSender<SidebarTmuxJob>,
    mpsc::Receiver<SidebarEffectCompletion>,
) {
    let (tx, rx) = mpsc::sync_channel::<SidebarTmuxJob>(64);
    let (completion_tx, completion_rx) = mpsc::channel::<SidebarEffectCompletion>();
    let socket_name = env
        .get("VDE_TMUX_SOCKET_NAME")
        .cloned()
        .filter(|value| !value.trim().is_empty());
    thread::spawn(move || {
        use crate::daemon::workers::WorkerIo as _;

        while let Ok(job) = rx.recv() {
            let io = crate::daemon::workers::SystemWorkerIo::new(
                socket_name.clone(),
                expected_server.clone(),
            );
            let (candidates, client_pid, source_pane) = match &job.effect {
                super::runtime::CanonicalSidebarEffect::JumpPane {
                    pane_instance,
                    client_pid,
                    source_pane,
                } => (
                    vec![pane_instance.clone()],
                    *client_pid,
                    source_pane.clone(),
                ),
                super::runtime::CanonicalSidebarEffect::JumpLatestUnread {
                    candidates,
                    client_pid,
                    source_pane,
                }
                | super::runtime::CanonicalSidebarEffect::ReadPeekAdvance {
                    candidates,
                    client_pid,
                    source_pane,
                    ..
                } => (candidates.clone(), *client_pid, source_pane.clone()),
                super::runtime::CanonicalSidebarEffect::PeekPane {
                    pane_instance,
                    client_pid,
                    source_pane,
                } => (
                    vec![pane_instance.clone()],
                    *client_pid,
                    source_pane.clone(),
                ),
            };
            let result = io.jump_to_first_available_pane(&candidates, client_pid, &source_pane);
            let result = match result {
                Ok(pane_instance) => SidebarEffectResult::Succeeded(pane_instance),
                Err(crate::daemon::workers::SidebarTmuxError::ServerIncarnationMismatch) => {
                    SidebarEffectResult::ServerIncarnationMismatch
                }
                Err(crate::daemon::workers::SidebarTmuxError::PaneInstanceMismatch(_)) => {
                    SidebarEffectResult::PaneInstanceMismatch
                }
                Err(crate::daemon::workers::SidebarTmuxError::NoAvailablePane) => {
                    SidebarEffectResult::NoAvailablePane
                }
                Err(crate::daemon::workers::SidebarTmuxError::SourceClientMismatch) => {
                    SidebarEffectResult::SourceClientMismatch
                }
                Err(error) => SidebarEffectResult::Failed(error.to_string()),
            };
            let witness_observation_floor = witness_observation_seq.load(Ordering::SeqCst);
            let _ = completion_tx.send(SidebarEffectCompletion {
                original_accepted_seq: job.original_accepted_seq,
                event_id: job.event_id,
                snapshot_revision: job.snapshot_revision,
                witness_observation_floor,
                result,
                effect: job.effect,
            });
        }
    });
    (tx, completion_rx)
}

#[cfg(test)]
fn start_notification_worker_with_timeout_and_log(
    command: String,
    timeout: Duration,
    log_context: Option<(std::collections::BTreeMap<String, String>, String)>,
) -> SyncSender<NotificationWorkerJob> {
    start_notification_worker_with_control(
        command,
        timeout,
        log_context,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(())),
    )
}

fn start_notification_worker_with_control(
    command: String,
    timeout: Duration,
    log_context: Option<(std::collections::BTreeMap<String, String>, String)>,
    shutdown: Arc<AtomicBool>,
    process_lock: Arc<Mutex<()>>,
) -> SyncSender<NotificationWorkerJob> {
    let (sender, receiver) = mpsc::sync_channel::<NotificationWorkerJob>(64);
    thread::spawn(move || {
        while let Ok(job) = receiver.recv() {
            let process_guard = process_lock
                .lock()
                .expect("notification process lock poisoned");
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let mut process = Command::new("/bin/sh");
            process
                .arg("-c")
                .arg(&command)
                .env("VDE_PANE_ID", &job.pane_id)
                .env("VDE_AGENT", &job.agent)
                .env("VDE_BADGE_STATE", "Blocked")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            unsafe {
                process.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = process.spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(error) => {
                    log_notification_failure(
                        log_context.as_ref(),
                        &format!(
                            "notification command spawn failed for pane {}: {error}",
                            job.pane_id
                        ),
                    );
                    continue;
                }
            };
            let notification_identity =
                match crate::daemon::lifecycle::process_start_token(child.id()) {
                    Ok(start_token) => crate::daemon::lifecycle::NotificationProcessIdentity {
                        process_group_id: child.id() as i32,
                        leader_start_token: start_token,
                    },
                    Err(error) => {
                        terminate_notification_process_group(&mut child);
                        log_notification_failure(
                            log_context.as_ref(),
                            &format!(
                                "notification process identity failed for pane {}: {error}",
                                job.pane_id
                            ),
                        );
                        continue;
                    }
                };
            if let Err(error) = record_active_notification(
                log_context.as_ref(),
                Some(notification_identity.clone()),
            ) {
                terminate_notification_process_group(&mut child);
                log_notification_failure(
                    log_context.as_ref(),
                    &format!(
                        "notification process identity persistence failed for pane {}: {error}",
                        job.pane_id
                    ),
                );
                continue;
            }
            drop(process_guard);
            let deadline = Instant::now() + timeout;
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    terminate_notification_process_group(&mut child);
                    break;
                }
                match try_wait_notification_process_group(&mut child) {
                    Ok(Some(status)) => {
                        if !status.success() {
                            log_notification_failure(
                                log_context.as_ref(),
                                &format!(
                                    "notification command exited with status {status} for pane {}",
                                    job.pane_id
                                ),
                            );
                        }
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        terminate_notification_process_group(&mut child);
                        log_notification_failure(
                            log_context.as_ref(),
                            &format!(
                                "notification command timed out after {timeout:?} for pane {}",
                                job.pane_id
                            ),
                        );
                        break;
                    }
                    Err(error) => {
                        terminate_notification_process_group(&mut child);
                        log_notification_failure(
                            log_context.as_ref(),
                            &format!(
                                "notification command wait failed for pane {}: {error}",
                                job.pane_id
                            ),
                        );
                        break;
                    }
                }
            }
            clear_active_notification(log_context.as_ref(), &notification_identity);
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
        }
    });
    sender
}

fn record_active_notification(
    context: Option<&(std::collections::BTreeMap<String, String>, String)>,
    identity: Option<crate::daemon::lifecycle::NotificationProcessIdentity>,
) -> Result<()> {
    let Some((env, incarnation_hash)) = context else {
        return Ok(());
    };
    crate::daemon::lifecycle::update_lifecycle_record(env, incarnation_hash, |record| {
        record.active_notification = identity;
        Ok(())
    })
}

fn clear_active_notification(
    context: Option<&(std::collections::BTreeMap<String, String>, String)>,
    identity: &crate::daemon::lifecycle::NotificationProcessIdentity,
) {
    let Some((env, incarnation_hash)) = context else {
        return;
    };
    let _ = crate::daemon::lifecycle::update_lifecycle_record(env, incarnation_hash, |record| {
        if record.active_notification.as_ref() == Some(identity) {
            record.active_notification = None;
        }
        Ok(())
    });
}

fn log_notification_failure(
    context: Option<&(std::collections::BTreeMap<String, String>, String)>,
    message: &str,
) {
    let Some((env, incarnation_hash)) = context else {
        eprintln!("[vde-tmux] {message}");
        return;
    };
    if crate::daemon::lifecycle::append_daemon_log(
        env,
        incarnation_hash,
        &format!("notification: {message}"),
    )
    .is_err()
    {
        eprintln!("[vde-tmux] {message}");
    }
}

fn terminate_notification_process_group(child: &mut std::process::Child) {
    let process_group = -(child.id() as i32);
    let _ = unsafe { libc::kill(process_group, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

fn try_wait_notification_process_group(
    child: &mut std::process::Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    child.wait().map(Some)
}

impl ProductionV2Coordinator {
    #[cfg(test)]
    fn new(
        incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
        env: std::collections::BTreeMap<String, String>,
        notification_command: Option<String>,
    ) -> Result<Self> {
        Self::new_with_task_summary(
            incarnation,
            env,
            notification_command,
            crate::config::SidebarTaskSummaryConfig::default(),
        )
    }

    fn new_with_task_summary(
        incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
        env: std::collections::BTreeMap<String, String>,
        notification_command: Option<String>,
        task_summary_config: crate::config::SidebarTaskSummaryConfig,
    ) -> Result<Self> {
        let notification_shutdown = Arc::new(AtomicBool::new(false));
        let notification_process_lock = Arc::new(Mutex::new(()));
        let notification_tx = notification_command.map(|command| {
            start_notification_worker_with_control(
                command,
                Duration::from_secs(2),
                Some((env.clone(), incarnation.hash.clone())),
                notification_shutdown.clone(),
                notification_process_lock.clone(),
            )
        });
        let task_summary_worker = task_summary_config
            .enabled
            .then(|| crate::daemon::task_summary::start_worker(task_summary_config));
        let (task_summary_tx, task_summary_completion_rx) = task_summary_worker
            .map_or((None, None), |worker| {
                (Some(worker.sender), Some(worker.completions))
            });
        let witness_observation_seq = Arc::new(AtomicU64::new(0));
        let (sidebar_tmux_tx, sidebar_completion_rx) = start_sidebar_tmux_worker(
            &env,
            incarnation.identity.clone(),
            witness_observation_seq.clone(),
        );
        let status_push = crate::daemon::status_push::StatusPushState::new(
            incarnation.identity.clone(),
            Duration::ZERO,
        )
        .map_err(|error| anyhow::anyhow!("failed to initialize status push state: {error}"))?;
        Ok(Self {
            router: Mutex::new(V2Router::new(
                DaemonInstanceId::generate()?,
                incarnation.hash.clone(),
            )),
            state: Mutex::new(None),
            agent_runtime: Mutex::new(None),
            queue: Mutex::new(ProductionQueue::default()),
            queue_ready: Condvar::new(),
            snapshot_cache: Mutex::new(None),
            snapshot_changed: Condvar::new(),
            waiters: Mutex::new(BTreeMap::new()),
            deferred_responses: Mutex::new(BTreeSet::new()),
            shutdown: AtomicBool::new(false),
            shutdown_ready: AtomicBool::new(false),
            incarnation,
            env,
            notification_tx,
            notification_shutdown,
            notification_process_lock,
            task_summary_tx,
            task_summary_completion_rx: Mutex::new(task_summary_completion_rx),
            sidebar_tmux_tx,
            sidebar_completion_rx: Mutex::new(Some(sidebar_completion_rx)),
            tmux_control: Mutex::new(None),
            pane_switch_trigger_shutdown: Arc::new(AtomicBool::new(false)),
            status_push: Mutex::new(status_push),
            status_push_driver: Mutex::new(()),
            status_push_started: Instant::now(),
            config_hash: Mutex::new(crate::daemon::lifecycle::config_hash(
                &crate::config::Config::default(),
            )),
            witness_observation_seq,
            current_view_refresh_generation: AtomicU64::new(0),
            current_view_refresh_running: AtomicBool::new(false),
        })
    }

    fn schedule_task_summary(&self, state: &crate::pane_state::PaneState) {
        let Some(sender) = &self.task_summary_tx else {
            return;
        };
        if !matches!(state.agent.as_str(), "codex" | "claude") {
            return;
        }
        let Some(fingerprint) = state.task_context.context_fingerprint() else {
            return;
        };
        if state
            .task_context
            .summary
            .as_ref()
            .is_some_and(|summary| summary.context_fingerprint == fingerprint)
        {
            return;
        }
        let job = crate::daemon::task_summary::TaskSummaryJob {
            pane_instance: state.pane_instance.clone(),
            state_id: state.state_id.clone(),
            agent_epoch: state.agent_epoch,
            agent: state.agent.clone(),
            task_context: state.task_context.clone(),
        };
        if let Err(error) = sender.try_send(job) {
            self.log_daemon_error(&format!(
                "task summary dispatch failed for pane {}: {error}",
                state.pane_instance.pane_id
            ));
        }
    }

    fn start_tmux_control(&self) {
        use std::os::unix::fs::FileTypeExt as _;

        let mut control = self
            .tmux_control
            .lock()
            .expect("tmux control lock poisoned");
        if control.is_some() {
            return;
        }
        *control = Some(
            if std::fs::metadata(&self.incarnation.socket_path)
                .is_ok_and(|metadata| metadata.file_type().is_socket())
            {
                crate::daemon::tmux_control::TmuxControlHandle::start(
                    self.incarnation.socket_path.clone(),
                )
            } else {
                crate::daemon::tmux_control::TmuxControlHandle::unavailable()
            },
        );
    }

    fn control_health(&self) -> crate::daemon::protocol::v2::ControlHealth {
        self.tmux_control
            .lock()
            .expect("tmux control lock poisoned")
            .as_ref()
            .map_or(
                crate::daemon::protocol::v2::ControlHealth::Starting,
                |control| control.health(),
            )
    }

    fn query_nvim_pane_markers(&self) -> Option<Vec<NvimPaneMarker>> {
        let control = self
            .tmux_control
            .lock()
            .expect("tmux control lock poisoned")
            .as_ref()
            .cloned()?;
        let format = format!("#{{pane_id}}\u{1f}#{{pane_pid}}\u{1f}#{{{NVIM_PROCESS_PID_OPTION}}}");
        let command = crate::pane_state::store::tmux_command_string(&[
            "list-panes".to_string(),
            "-a".to_string(),
            "-F".to_string(),
            format,
        ]);
        control
            .execute_until(command, Instant::now() + Duration::from_millis(250))
            .ok()
            .map(|output| parse_nvim_pane_markers(&output))
    }

    fn cleanup_stale_nvim_pane_markers(
        &self,
        markers: &[NvimPaneMarker],
        processes: &crate::daemon::workers::AgentProcessSnapshot,
    ) {
        let stale = markers
            .iter()
            .filter(|marker| {
                processes.contains_nvim_process(marker.pane_pid, marker.process_pid) == Some(false)
            })
            .cloned()
            .collect::<Vec<_>>();
        let Some(command) =
            stale_nvim_marker_cleanup_command(self.incarnation.identity.pid, &stale)
        else {
            return;
        };
        let control = self
            .tmux_control
            .lock()
            .expect("tmux control lock poisoned")
            .as_ref()
            .cloned();
        if let Some(control) = control {
            let _ = control.execute_until(command, Instant::now() + Duration::from_millis(250));
        }
    }

    fn shutdown_tmux_control(&self) {
        if let Some(control) = self
            .tmux_control
            .lock()
            .expect("tmux control lock poisoned")
            .as_ref()
        {
            control.shutdown();
        }
    }

    fn pane_switch_channel(&self) -> String {
        "vde-pane-switch".to_string()
    }

    fn stop_pane_switch_trigger(&self) {
        if self
            .pane_switch_trigger_shutdown
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let command = crate::pane_state::store::tmux_command_string(&[
            "wait-for".to_string(),
            "-S".to_string(),
            self.pane_switch_channel(),
        ]);
        if let Some(control) = self
            .tmux_control
            .lock()
            .expect("tmux control lock poisoned")
            .as_ref()
        {
            let _ = control.execute_until(command, Instant::now() + Duration::from_millis(250));
        }
    }

    fn handle_pane_switch_trigger(&self) {
        if self.shutdown.load(Ordering::SeqCst)
            || self.pane_switch_trigger_shutdown.load(Ordering::SeqCst)
        {
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        let command = crate::pane_state::store::tmux_command_string(&[
            "show-options".to_string(),
            "-gqv".to_string(),
            crate::daemon::lifecycle::PANE_SWITCH_REQUEST_OPTION.to_string(),
        ]);
        let request = {
            let control = self
                .tmux_control
                .lock()
                .expect("tmux control lock poisoned");
            let Some(control) = control.as_ref() else {
                return;
            };
            let Ok(output) = control.execute_until(command, deadline) else {
                return;
            };
            output.trim_end_matches(['\r', '\n']).to_string()
        };
        let fields = request
            .split(PANE_SWITCH_REQUEST_SEPARATOR)
            .collect::<Vec<_>>();
        if fields.len() != 4 || fields[0] != self.incarnation.hash {
            return;
        }
        let direction = match fields[1] {
            "left" => crate::cli::pane_switch::PaneSwitchDirection::Left,
            "right" => crate::cli::pane_switch::PaneSwitchDirection::Right,
            "up" => crate::cli::pane_switch::PaneSwitchDirection::Up,
            "down" => crate::cli::pane_switch::PaneSwitchDirection::Down,
            _ => return,
        };
        let Ok(pane_pid) = fields[3].parse::<u32>() else {
            return;
        };
        let _ = self.pane_switch(
            direction,
            PaneInstance {
                pane_id: fields[2].to_string(),
                pane_pid,
            },
        );
    }

    fn configure_health(&self, config: &crate::config::Config) {
        *self.config_hash.lock().expect("config hash lock poisoned") =
            crate::daemon::lifecycle::config_hash(config);
    }

    fn begin_witness_observation(&self) -> u64 {
        self.witness_observation_seq
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1)
    }

    fn schedule_current_view_refresh(self: &Arc<Self>) {
        self.current_view_refresh_generation
            .fetch_add(1, Ordering::SeqCst);
        if self
            .current_view_refresh_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let coordinator = self.clone();
        thread::spawn(move || {
            loop {
                let generation = coordinator
                    .current_view_refresh_generation
                    .load(Ordering::SeqCst);
                thread::sleep(CURRENT_VIEW_REFRESH_DEBOUNCE);
                if coordinator
                    .current_view_refresh_generation
                    .load(Ordering::SeqCst)
                    != generation
                {
                    continue;
                }
                let through_unread_order = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned")
                    .as_ref()
                    .map_or(0, |state| state.leased.runtime.latest_unread_order());
                match query_client_witnesses(&coordinator, Duration::from_millis(100)) {
                    Ok(observation) => {
                        let _ = coordinator.enqueue_internal(
                            V2InternalMutation::CurrentViewsReplacement {
                                observation_seq: observation.seq,
                                witnesses: observation.witnesses,
                                through_unread_order,
                            },
                        );
                    }
                    Err(error) if error.requires_daemon_exit() => {
                        coordinator.fail_stop(error.to_string());
                        coordinator
                            .current_view_refresh_running
                            .store(false, Ordering::SeqCst);
                        return;
                    }
                    Err(error) => coordinator
                        .log_daemon_error(&format!("current view refresh failed: {error}")),
                }
                if coordinator
                    .current_view_refresh_generation
                    .load(Ordering::SeqCst)
                    != generation
                {
                    continue;
                }
                coordinator
                    .current_view_refresh_running
                    .store(false, Ordering::SeqCst);
                if coordinator
                    .current_view_refresh_generation
                    .load(Ordering::SeqCst)
                    == generation
                    || coordinator
                        .current_view_refresh_running
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_err()
                {
                    return;
                }
            }
        });
    }

    fn route_external(
        &self,
        connection: &mut V2ConnectionState,
        message: ClientMessage,
        raw_frame_bytes: usize,
    ) -> ServerMessage {
        use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

        if self.shutdown.load(Ordering::SeqCst) {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is shutting down",
                message.event_id().cloned(),
            );
        }
        let mut router = self.router.lock().expect("v2 router lock poisoned");
        if self.shutdown.load(Ordering::SeqCst) {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is shutting down",
                message.event_id().cloned(),
            );
        }
        if router.phase() == DaemonPhase::Serving && message.is_mutation() {
            let queue = self.queue.lock().expect("v2 queue lock poisoned");
            if queue.items.len() + usize::from(queue.in_flight) >= V2_MUTATION_QUEUE_CAPACITY {
                return ServerMessage::error(
                    ErrorCode::QueueFull,
                    "sequenced mutation queue is full",
                    message.event_id().cloned(),
                );
            }
        }
        match router.route(connection, message) {
            V2Route::Response(response) => response,
            V2Route::Fatal(response) => {
                drop(router);
                self.fail_stop("v2 router entered fatal state");
                response
            }
            V2Route::Query(query) => {
                drop(router);
                self.query(query)
            }
            V2Route::Mutation(sequenced) => {
                let view = matches!(
                    sequenced.mutation,
                    V2AcceptedMutation::External(ClientMessage::SubmitViewEvent { .. })
                );
                let accepted_seq = sequenced.accepted_seq;
                let event_id = match &sequenced.mutation {
                    V2AcceptedMutation::External(message) => message.event_id().cloned(),
                    V2AcceptedMutation::Internal(_) => None,
                };
                if view {
                    self.enqueue_without_waiter_locked(sequenced, raw_frame_bytes);
                    drop(router);
                    return ServerMessage::ViewQueued {
                        event_id: event_id.expect("view event has an event ID"),
                        accepted_seq,
                    };
                }
                let receiver = self.enqueue_locked(sequenced, raw_frame_bytes);
                drop(router);
                receiver.recv().unwrap_or_else(|error| {
                    ServerMessage::error(
                        ErrorCode::InternalError,
                        format!("mutation response unavailable: {error}"),
                        event_id,
                    )
                })
            }
            V2Route::Queued { accepted_seq } => {
                let (sender, receiver) = mpsc::channel();
                self.waiters
                    .lock()
                    .expect("v2 waiter lock poisoned")
                    .insert(accepted_seq, sender);
                drop(router);
                receiver.recv().unwrap_or_else(|error| {
                    ServerMessage::error(
                        ErrorCode::InternalError,
                        format!("bootstrap mutation response unavailable: {error}"),
                        None,
                    )
                })
            }
            V2Route::DroppedInternal => {
                ServerMessage::error(ErrorCode::QueueFull, "internal mutation was dropped", None)
            }
        }
    }

    #[allow(clippy::result_large_err)] // The typed protocol error is intentionally returned intact.
    fn route_subscription(
        &self,
        connection: &mut V2ConnectionState,
        message: ClientMessage,
    ) -> Result<PublishedResolvedSnapshot, ServerMessage> {
        use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

        if self.shutdown.load(Ordering::SeqCst) {
            return Err(ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is shutting down",
                None,
            ));
        }
        let mut router = self.router.lock().expect("v2 router lock poisoned");
        let route = router.route(connection, message);
        match route {
            V2Route::Query(ClientMessage::Subscribe { .. }) => {
                drop(router);
                if self
                    .state
                    .lock()
                    .expect("canonical state lock poisoned")
                    .is_none()
                {
                    return Err(ServerMessage::error(
                        ErrorCode::NotReady,
                        "daemon is hydrating",
                        None,
                    ));
                }
                self.publish_resolved_snapshot().map_err(|error| {
                    ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
                })
            }
            V2Route::Response(response) => Err(response),
            V2Route::Fatal(response) => {
                drop(router);
                self.fail_stop("v2 subscription route entered fatal state");
                Err(response)
            }
            _ => Err(ServerMessage::error(
                ErrorCode::InvalidRequest,
                "expected a Subscribe query",
                None,
            )),
        }
    }

    fn enqueue_locked(
        &self,
        sequenced: V2SequencedMutation,
        raw_frame_bytes: usize,
    ) -> mpsc::Receiver<ServerMessage> {
        let (sender, receiver) = mpsc::channel();
        self.waiters
            .lock()
            .expect("v2 waiter lock poisoned")
            .insert(sequenced.accepted_seq, sender);
        self.queue
            .lock()
            .expect("v2 queue lock poisoned")
            .items
            .push_back(ProductionMutation {
                sequenced,
                raw_frame_bytes,
            });
        self.queue_ready.notify_one();
        receiver
    }

    fn enqueue_without_waiter_locked(
        &self,
        sequenced: V2SequencedMutation,
        raw_frame_bytes: usize,
    ) {
        self.queue
            .lock()
            .expect("v2 queue lock poisoned")
            .items
            .push_back(ProductionMutation {
                sequenced,
                raw_frame_bytes,
            });
        self.queue_ready.notify_one();
    }

    fn query(&self, message: ClientMessage) -> ServerMessage {
        use crate::daemon::protocol::v2::{ClientMessage, ErrorCode, ServerMessage};

        match message {
            ClientMessage::QueryResolvedSnapshot { .. } | ClientMessage::Subscribe { .. } => {
                if self
                    .state
                    .lock()
                    .expect("canonical state lock poisoned")
                    .is_none()
                {
                    return ServerMessage::error(ErrorCode::NotReady, "daemon is hydrating", None);
                }
                match self.publish_resolved_snapshot() {
                    Ok(published) => (*published.message).clone(),
                    Err(error) => {
                        ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
                    }
                }
            }
            ClientMessage::QueryPane { pane_id, .. } => {
                if let Err(error) = crate::daemon::topology::validate_pane_id(&pane_id) {
                    return ServerMessage::error(
                        ErrorCode::InvalidRequest,
                        error.to_string(),
                        None,
                    );
                }
                {
                    let state = self.state.lock().expect("canonical state lock poisoned");
                    let Some(state) = state.as_ref() else {
                        return ServerMessage::error(
                            ErrorCode::NotReady,
                            "daemon is hydrating",
                            None,
                        );
                    };
                    if let Some(pane) = state.pane_presentation(&pane_id) {
                        return ServerMessage::PaneResult {
                            snapshot_revision: state.leased.runtime.snapshot_revision(),
                            pane,
                        };
                    }
                }
                self.enqueue_internal_and_wait(V2InternalMutation::TargetedPaneRefresh { pane_id })
            }
            ClientMessage::QueryStatusSnapshot { context, .. } => {
                let state = self.state.lock().expect("canonical state lock poisoned");
                let Some(state) = state.as_ref() else {
                    return ServerMessage::error(ErrorCode::NotReady, "daemon is hydrating", None);
                };
                let snapshot = state.status_snapshot(context);
                ServerMessage::StatusSnapshotResult {
                    snapshot_revision: snapshot.snapshot_revision,
                    snapshot,
                }
            }
            ClientMessage::QueryRuntimeInfo { .. } => ServerMessage::RuntimeInfoResult {
                info: crate::daemon::protocol::v2::RuntimeInfo {
                    config_hash: self
                        .config_hash
                        .lock()
                        .expect("config hash lock poisoned")
                        .clone(),
                    control_health: self.control_health(),
                },
            },
            ClientMessage::QueryAgentRun { run_ref, .. } => {
                let reference = match crate::agent_state::RunRef::decode(&run_ref) {
                    Ok(reference) => reference,
                    Err(error) => {
                        return ServerMessage::error(
                            ErrorCode::InvalidRequest,
                            error.to_string(),
                            None,
                        );
                    }
                };
                let runtime = self
                    .agent_runtime
                    .lock()
                    .expect("agent runtime lock poisoned");
                let Some(runtime) = runtime.as_ref() else {
                    return ServerMessage::error(
                        ErrorCode::NotReady,
                        "agent runtime is hydrating",
                        None,
                    );
                };
                match runtime.get_run(&reference) {
                    Ok(run) => ServerMessage::AgentRunResult {
                        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                        run_ref,
                        run,
                    },
                    Err(error) => agent_state_query_error(error),
                }
            }
            ClientMessage::QueryCurrentAgentRuns { bindings, .. } => {
                if bindings.len() > 4096 {
                    return ServerMessage::error(
                        ErrorCode::InvalidRequest,
                        "current run batch exceeds 4096 Agent Bindings",
                        None,
                    );
                }
                let live_bindings = {
                    let state = self.state.lock().expect("canonical state lock poisoned");
                    let Some(state) = state.as_ref() else {
                        return ServerMessage::error(
                            ErrorCode::NotReady,
                            "daemon is hydrating",
                            None,
                        );
                    };
                    bindings
                        .into_iter()
                        .filter(|binding| {
                            binding.server_identity == self.incarnation.identity
                                && state
                                    .leased
                                    .runtime
                                    .record(&binding.pane_instance)
                                    .is_some_and(|record| {
                                        record.agent_present
                                            && record.state_id == binding.pane_state_id
                                            && record.agent_epoch == binding.agent_epoch
                                            && record.agent_process.as_ref()
                                                == Some(&binding.process)
                                    })
                        })
                        .collect::<Vec<_>>()
                };
                let runtime = self
                    .agent_runtime
                    .lock()
                    .expect("agent runtime lock poisoned");
                let Some(runtime) = runtime.as_ref() else {
                    return ServerMessage::error(
                        ErrorCode::NotReady,
                        "agent runtime is hydrating",
                        None,
                    );
                };
                let mut runs = Vec::with_capacity(live_bindings.len());
                for binding in live_bindings {
                    let run = match runtime.current_run_for_binding(&binding) {
                        Ok(Some(run)) => run,
                        Ok(None) => continue,
                        Err(error) => return agent_state_query_error(error),
                    };
                    let run_ref = match runtime.run_ref(run.run_id.clone()).encode() {
                        Ok(run_ref) => run_ref,
                        Err(error) => {
                            return ServerMessage::error(
                                ErrorCode::InternalError,
                                error.to_string(),
                                None,
                            );
                        }
                    };
                    runs.push(crate::daemon::protocol::v2::CurrentAgentRun {
                        binding,
                        run_ref,
                        execution_phase: run.execution_phase,
                        semantic_outcome: run.semantic_outcome,
                    });
                }
                ServerMessage::CurrentAgentRunsResult {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                    runs,
                }
            }
            ClientMessage::QueryAgentOperation { operation_ref, .. } => {
                let reference = match crate::agent_state::OperationRef::decode(&operation_ref) {
                    Ok(reference) => reference,
                    Err(error) => {
                        return ServerMessage::error(
                            ErrorCode::InvalidRequest,
                            error.to_string(),
                            None,
                        );
                    }
                };
                let runtime = self
                    .agent_runtime
                    .lock()
                    .expect("agent runtime lock poisoned");
                let Some(runtime) = runtime.as_ref() else {
                    return ServerMessage::error(
                        ErrorCode::NotReady,
                        "agent runtime is hydrating",
                        None,
                    );
                };
                match runtime.get_operation(&reference) {
                    Ok(operation) => ServerMessage::AgentOperationResult {
                        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                        operation_ref,
                        operation,
                    },
                    Err(error) => agent_state_query_error(error),
                }
            }
            ClientMessage::QueryAgentResponse { run_ref, .. } => {
                let reference = match crate::agent_state::RunRef::decode(&run_ref) {
                    Ok(reference) => reference,
                    Err(error) => {
                        return ServerMessage::error(
                            ErrorCode::InvalidRequest,
                            error.to_string(),
                            None,
                        );
                    }
                };
                let runtime = self
                    .agent_runtime
                    .lock()
                    .expect("agent runtime lock poisoned");
                let Some(runtime) = runtime.as_ref() else {
                    return ServerMessage::error(
                        ErrorCode::NotReady,
                        "agent runtime is hydrating",
                        None,
                    );
                };
                let run = match runtime.get_run(&reference) {
                    Ok(run) => run,
                    Err(error) => return agent_state_query_error(error),
                };
                let Some(metadata) = run.artifact else {
                    let (code, message) = if run.semantic_outcome
                        == crate::agent_state::SemanticOutcome::Unresolved
                    {
                        (ErrorCode::RunUnresolved, "run is not completed")
                    } else {
                        (
                            ErrorCode::ArtifactUnavailable,
                            "completed run has no available response artifact",
                        )
                    };
                    return ServerMessage::error(code, message, None);
                };
                match runtime.read_response(&reference) {
                    Ok(body) => ServerMessage::AgentResponseResult {
                        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                        run_ref,
                        metadata,
                        body_base64: base64::engine::general_purpose::STANDARD
                            .encode(body.as_bytes()),
                    },
                    Err(error) => agent_state_query_error(error),
                }
            }
            ClientMessage::QueryAgentStorage { .. } => {
                let runtime = self
                    .agent_runtime
                    .lock()
                    .expect("agent runtime lock poisoned");
                let Some(runtime) = runtime.as_ref() else {
                    return ServerMessage::error(
                        ErrorCode::NotReady,
                        "agent runtime is hydrating",
                        None,
                    );
                };
                match runtime.store().usage() {
                    Ok(usage) => ServerMessage::AgentStorageResult {
                        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                        usage,
                    },
                    Err(error) => agent_state_query_error(error),
                }
            }
            ClientMessage::PaneSwitch {
                direction,
                source_pane,
                ..
            } => self.pane_switch(direction, source_pane),
            _ => ServerMessage::error(ErrorCode::InvalidRequest, "unsupported query", None),
        }
    }

    fn pane_switch(
        &self,
        direction: crate::cli::pane_switch::PaneSwitchDirection,
        source_pane: PaneInstance,
    ) -> ServerMessage {
        use crate::cli::pane_switch::{
            PaneSwitchAction, PaneSwitchOutcome, SELECTION_CHANGED_SENTINEL,
        };
        use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

        if let Err(error) = source_pane.validate() {
            return ServerMessage::error(ErrorCode::InvalidPaneInstance, error.to_string(), None);
        }
        const SNAPSHOT_IDENTITY_SEPARATOR: &str = "__vde_pane_switch_identity__";
        let deadline = Instant::now() + Duration::from_millis(500);
        let control_guard = self
            .tmux_control
            .lock()
            .expect("tmux control lock poisoned");
        let Some(control) = control_guard.as_ref() else {
            return ServerMessage::error(
                ErrorCode::ControlUnavailable,
                "tmux control client is starting",
                None,
            );
        };
        let snapshot_format = format!(
            "#{{pid}}{SNAPSHOT_IDENTITY_SEPARATOR}#{{start_time}}{SNAPSHOT_IDENTITY_SEPARATOR}{}",
            crate::cli::pane_switch::pane_snapshot_format()
        );
        let snapshot_command = crate::pane_state::store::tmux_command_string(&[
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            source_pane.pane_id.clone(),
            snapshot_format,
        ]);
        let snapshot_output = match control.execute_until(snapshot_command, deadline) {
            Ok(output) => output,
            Err(crate::daemon::tmux_control::ControlError::QueueFull) => {
                return ServerMessage::error(
                    ErrorCode::QueueFull,
                    "tmux control command queue is full",
                    None,
                );
            }
            Err(crate::daemon::tmux_control::ControlError::CommandFailed(message)) => {
                return ServerMessage::error(
                    ErrorCode::StaleSelection,
                    format!("source pane could not be captured: {message}"),
                    None,
                );
            }
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::ControlUnavailable,
                    error.to_string(),
                    None,
                );
            }
        };
        let mut snapshot_fields = snapshot_output
            .trim_end_matches(['\r', '\n'])
            .splitn(3, SNAPSHOT_IDENTITY_SEPARATOR);
        let snapshot_identity = snapshot_fields
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
            .zip(
                snapshot_fields
                    .next()
                    .and_then(|start_time| start_time.parse::<i64>().ok()),
            );
        let pane_snapshot = snapshot_fields.next();
        if snapshot_identity
            != Some((
                self.incarnation.identity.pid,
                self.incarnation.identity.start_time,
            ))
            || pane_snapshot.is_none()
        {
            drop(control_guard);
            self.fail_stop("tmux server incarnation changed during pane snapshot capture");
            return ServerMessage::error(
                ErrorCode::InternalError,
                "tmux server incarnation changed during pane navigation",
                None,
            );
        }
        let action = match crate::cli::pane_switch::prepare(
            direction,
            &source_pane.pane_id,
            source_pane.pane_pid,
            pane_snapshot.expect("pane snapshot checked above"),
        ) {
            Ok(PaneSwitchAction::NoTarget) => {
                return ServerMessage::PaneSwitchResult {
                    outcome: PaneSwitchOutcome::NoTarget,
                };
            }
            Ok(PaneSwitchAction::Apply(command)) => command,
            Err(error) => {
                let message = error.to_string();
                let code = if message.contains("source pane") {
                    ErrorCode::StaleSelection
                } else {
                    ErrorCode::InvalidRequest
                };
                return ServerMessage::error(code, message, None);
            }
        };
        const SERVER_CHANGED: &str = "__vde_pane_switch_server_changed__";
        let guarded = crate::pane_state::store::server_guarded_command_args(
            self.incarnation.identity.pid,
            self.incarnation.identity.start_time,
            action,
            SERVER_CHANGED,
        );
        let command = crate::pane_state::store::tmux_command_string(&guarded);
        let result = control.execute_until(command, deadline);
        drop(control_guard);
        match result {
            Ok(output) if output.lines().any(|line| line.trim() == SERVER_CHANGED) => {
                self.fail_stop("tmux server incarnation changed during pane navigation");
                ServerMessage::error(
                    ErrorCode::InternalError,
                    "tmux server incarnation changed during pane navigation",
                    None,
                )
            }
            Ok(output)
                if output
                    .lines()
                    .any(|line| line.trim() == SELECTION_CHANGED_SENTINEL) =>
            {
                ServerMessage::error(
                    ErrorCode::StaleSelection,
                    "pane selection changed before the switch could be applied",
                    None,
                )
            }
            Ok(_) => ServerMessage::PaneSwitchResult {
                outcome: PaneSwitchOutcome::Applied,
            },
            Err(crate::daemon::tmux_control::ControlError::QueueFull) => ServerMessage::error(
                ErrorCode::QueueFull,
                "tmux control command queue is full",
                None,
            ),
            Err(error) => {
                ServerMessage::error(ErrorCode::ControlUnavailable, error.to_string(), None)
            }
        }
    }

    fn enqueue_internal_and_wait(&self, mutation: V2InternalMutation) -> ServerMessage {
        use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

        if self.shutdown.load(Ordering::SeqCst) {
            return ServerMessage::error(ErrorCode::NotReady, "daemon is shutting down", None);
        }
        let mut router = self.router.lock().expect("v2 router lock poisoned");
        if self.shutdown.load(Ordering::SeqCst) {
            return ServerMessage::error(ErrorCode::NotReady, "daemon is shutting down", None);
        }
        let queue = self.queue.lock().expect("v2 queue lock poisoned");
        if queue.items.len() + usize::from(queue.in_flight) >= V2_MUTATION_QUEUE_CAPACITY {
            return ServerMessage::error(
                ErrorCode::QueueFull,
                "sequenced mutation queue is full",
                None,
            );
        }
        drop(queue);
        match router.accept_internal(mutation) {
            V2Route::Mutation(sequenced) => {
                let receiver = self.enqueue_locked(sequenced, 0);
                drop(router);
                receiver
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_else(|error| {
                        ServerMessage::error(
                            ErrorCode::InternalError,
                            format!("internal mutation response unavailable: {error}"),
                            None,
                        )
                    })
            }
            V2Route::Fatal(response) => {
                drop(router);
                self.fail_stop("v2 internal route entered fatal state");
                response
            }
            V2Route::Response(response) => response,
            V2Route::DroppedInternal | V2Route::Queued { .. } => ServerMessage::error(
                ErrorCode::QueueFull,
                "internal mutation was not accepted",
                None,
            ),
            V2Route::Query(_) => unreachable!("internal mutation cannot become a query"),
        }
    }

    fn enqueue_internal(&self, mutation: V2InternalMutation) -> bool {
        if self.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let mut router = self.router.lock().expect("v2 router lock poisoned");
        if self.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let queue = self.queue.lock().expect("v2 queue lock poisoned");
        if queue.items.len() + usize::from(queue.in_flight) >= V2_MUTATION_QUEUE_CAPACITY {
            drop(queue);
            drop(router);
            self.note_mutation_queue_drop();
            return false;
        }
        drop(queue);
        match router.accept_internal(mutation) {
            V2Route::Mutation(sequenced) => {
                self.queue
                    .lock()
                    .expect("v2 queue lock poisoned")
                    .items
                    .push_back(ProductionMutation {
                        sequenced,
                        raw_frame_bytes: 0,
                    });
                self.queue_ready.notify_one();
                true
            }
            V2Route::Queued { .. } => true,
            V2Route::Fatal(_) => {
                drop(router);
                self.fail_stop("v2 internal route entered fatal state");
                false
            }
            V2Route::DroppedInternal | V2Route::Response(_) | V2Route::Query(_) => false,
        }
    }

    fn note_mutation_queue_drop(&self) {
        self.log_daemon_error(&format!(
            "sequenced mutation queue full: dropped internal mutation (capacity={V2_MUTATION_QUEUE_CAPACITY})"
        ));
    }

    fn complete(&self, accepted_seq: u64, response: ServerMessage) {
        if let Some(waiter) = self
            .waiters
            .lock()
            .expect("v2 waiter lock poisoned")
            .remove(&accepted_seq)
            && waiter.send(response).is_err()
        {
            let _ = self.enqueue_internal(V2InternalMutation::DiagnosticProjection {
                pane_instance: None,
                message: format!("mutation_response_disconnected: accepted_seq={accepted_seq}"),
            });
        }
    }

    fn publish_resolved_snapshot(&self) -> Result<PublishedResolvedSnapshot> {
        // Fast path: compare the cheap current revision against the published
        // cache before building a full checked snapshot. Lock order stays
        // state -> release -> snapshot_cache on every path.
        let current_revision = {
            let state = self.state.lock().expect("canonical state lock poisoned");
            let state = state
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("canonical state is not initialized"))?;
            state.leased.runtime.snapshot_revision()
        };
        {
            let cache = self
                .snapshot_cache
                .lock()
                .expect("snapshot cache lock poisoned");
            if let Some(published) = cache.as_ref()
                && published.revision >= current_revision
            {
                return Ok(published.clone());
            }
        }
        let snapshot = {
            let state = self.state.lock().expect("canonical state lock poisoned");
            let state = state
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("canonical state is not initialized"))?;
            state.checked_resolved_snapshot()?
        };
        let revision = snapshot.snapshot_revision;
        let mut cache = self
            .snapshot_cache
            .lock()
            .expect("snapshot cache lock poisoned");
        if let Some(published) = cache.as_ref()
            && published.revision >= revision
        {
            return Ok(published.clone());
        }
        let candidate = ServerMessage::ResolvedSnapshotResult {
            snapshot_revision: revision,
            snapshot,
        };
        let (message, frame, terminal) =
            match crate::daemon::protocol::v2::encode_response_frame(&candidate) {
                Ok(frame) => (candidate, frame, false),
                Err(
                    error @ ServerMessage::Error {
                        code: ErrorCode::FrameTooLarge,
                        ..
                    },
                ) => {
                    let frame = crate::daemon::protocol::v2::encode_response_frame(&error)
                        .map_err(|nested| {
                            anyhow::anyhow!(
                                "failed to serialize FrameTooLarge response: {nested:?}"
                            )
                        })?;
                    (error, frame, true)
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "failed to serialize resolved snapshot: {error:?}"
                    ));
                }
            };
        let published = PublishedResolvedSnapshot {
            revision,
            frame: Arc::new(frame),
            message: Arc::new(message),
            terminal,
        };
        *cache = Some(published.clone());
        drop(cache);
        self.snapshot_changed.notify_all();
        if terminal {
            let _ = self.enqueue_internal(V2InternalMutation::FrameTooLargeProjection {
                rejected_revision: revision,
            });
        }
        Ok(published)
    }

    /// Waits until a snapshot newer than `revision` is published. When
    /// `heartbeat_after` elapses without one, the subscriber gets a
    /// `HeartbeatDue` outcome instead of a re-sent full snapshot.
    fn wait_for_snapshot_after(
        &self,
        revision: u64,
        heartbeat_after: Duration,
    ) -> Option<SnapshotWaitOutcome> {
        let mut cache = self
            .snapshot_cache
            .lock()
            .expect("snapshot cache lock poisoned");
        loop {
            if let Some(published) = cache.as_ref()
                && published.revision > revision
            {
                return Some(SnapshotWaitOutcome::Published(published.clone()));
            }
            if self.shutdown.load(Ordering::SeqCst) {
                return None;
            }
            let (next, timeout) = self
                .snapshot_changed
                .wait_timeout(cache, heartbeat_after)
                .expect("snapshot cache lock poisoned while waiting");
            cache = next;
            if timeout.timed_out() {
                let snapshot_revision = cache
                    .as_ref()
                    .map_or(revision, |published| published.revision);
                return Some(SnapshotWaitOutcome::HeartbeatDue { snapshot_revision });
            }
        }
    }

    fn drive_status_push(&self, trigger: StatusPushTrigger) -> Result<()> {
        use crate::daemon::status_push::build_display_frame;

        let _driver = self
            .status_push_driver
            .lock()
            .expect("status push driver lock poisoned");
        let now = self.status_push_started.elapsed();
        let decision = match trigger {
            StatusPushTrigger::Flush => self
                .status_push
                .lock()
                .expect("status push lock poisoned")
                .flush_coalesced(now)
                .map_err(anyhow::Error::new)?,
            StatusPushTrigger::Snapshot => {
                let (global, sessions, panes, config) = {
                    let state = self.state.lock().expect("canonical state lock poisoned");
                    let state = state
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("canonical state is not initialized"))?;
                    if matches!(trigger, StatusPushTrigger::Snapshot)
                        && self
                            .status_push
                            .lock()
                            .expect("status push lock poisoned")
                            .last_snapshot_revision()
                            == Some(state.leased.runtime.snapshot_revision())
                    {
                        return Ok(());
                    }
                    let _ = state.checked_resolved_snapshot()?;
                    let (global, sessions, panes) = state.display_projection();
                    (global, sessions, panes, state.projection_config.clone())
                };
                let frame = build_display_frame(&config, &global, &sessions, &panes)
                    .map_err(anyhow::Error::new)?;
                let mut push = self.status_push.lock().expect("status push lock poisoned");
                match trigger {
                    StatusPushTrigger::Snapshot => {
                        push.on_snapshot_revision(global.snapshot_revision, now, frame)
                    }
                    StatusPushTrigger::Flush => unreachable!(),
                }
                .map_err(anyhow::Error::new)?
            }
        };
        self.execute_status_push_decision(decision)
    }

    fn execute_status_push_decision(
        &self,
        decision: crate::daemon::status_push::StatusPushDecision,
    ) -> Result<()> {
        use crate::daemon::status_push::{
            BatchExecution, StatusPushDecision, SystemDisplayBatchIo,
        };

        let StatusPushDecision::Batch(prepared) = decision else {
            return Ok(());
        };
        let runner = self.status_push_runner(Duration::from_secs(1));
        let batch_dir = crate::daemon::daemon_socket_path_for_incarnation(
            &self.env,
            None,
            &self.incarnation.hash,
        )
        .with_extension("status-batches");
        let mut io = SystemDisplayBatchIo::new(&runner, &batch_dir);
        let result = self
            .status_push
            .lock()
            .expect("status push lock poisoned")
            .execute_prepared(&prepared, &mut io)
            .map_err(anyhow::Error::new)?;
        match result {
            BatchExecution::Committed => Ok(()),
            BatchExecution::Failed(error) => {
                self.log_status_push_error(&format!("status display batch failed: {error}"));
                Ok(())
            }
            BatchExecution::PaneInstanceMismatch(pane) => {
                self.status_push
                    .lock()
                    .expect("status push lock poisoned")
                    .pane_removed(&pane);
                self.log_status_push_error(&format!(
                    "status display pane instance changed: {}:{}",
                    pane.pane_id, pane.pane_pid
                ));
                Ok(())
            }
            BatchExecution::ServerIncarnationMismatch => {
                self.fail_stop("tmux server incarnation changed during status display write");
                bail!("tmux server incarnation changed during status display write")
            }
        }
    }

    fn write_status_shutdown_projection(&self) {
        use crate::daemon::status_push::StatusPushDecision;

        if self
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .is_none()
        {
            return;
        }
        let _driver = self
            .status_push_driver
            .lock()
            .expect("status push driver lock poisoned");
        let started = Instant::now();
        let first = self
            .status_push
            .lock()
            .expect("status push lock poisoned")
            .request_shutdown(
                self.status_push_started.elapsed(),
                "#[fg=yellow]vde daemon stopped#[default]".to_string(),
            );
        let mut decision = match first {
            Ok(decision) => decision,
            Err(error) => {
                self.log_status_push_error(&format!(
                    "failed to prepare status shutdown projection: {error}"
                ));
                return;
            }
        };
        loop {
            if started.elapsed() >= Duration::from_secs(2) {
                self.log_status_push_error("status shutdown projection exceeded 2 second budget");
                return;
            }
            match decision {
                StatusPushDecision::Coalesced { ready_at } => {
                    let now = self.status_push_started.elapsed();
                    if ready_at > now {
                        thread::sleep(
                            (ready_at - now)
                                .min(Duration::from_millis(100))
                                .min(Duration::from_secs(2).saturating_sub(started.elapsed())),
                        );
                    }
                }
                StatusPushDecision::Batch(prepared) => {
                    if let Err(error) =
                        self.execute_status_push_decision(StatusPushDecision::Batch(prepared))
                    {
                        self.log_status_push_error(&format!(
                            "failed to write status shutdown projection: {error:#}"
                        ));
                    }
                }
                StatusPushDecision::WaitingForInFlight => {
                    thread::sleep(Duration::from_millis(10));
                }
                StatusPushDecision::Ignored | StatusPushDecision::NoChanges => return,
            }
            decision = match self
                .status_push
                .lock()
                .expect("status push lock poisoned")
                .flush_coalesced(self.status_push_started.elapsed())
            {
                Ok(decision) => decision,
                Err(error) => {
                    self.log_status_push_error(&format!(
                        "failed to flush status shutdown projection: {error}"
                    ));
                    return;
                }
            };
        }
    }

    fn status_push_runner(&self, timeout: Duration) -> crate::tmux::SystemTmuxRunner {
        self.env
            .get("VDE_TMUX_SOCKET_NAME")
            .filter(|name| !name.trim().is_empty())
            .map(|name| crate::tmux::SystemTmuxRunner::with_socket_name(name, Some(timeout)))
            .unwrap_or_else(|| crate::tmux::SystemTmuxRunner::with_timeout(timeout))
    }

    fn sync_status_push_topology_targets_locked(&self) {
        let (sessions, panes) = {
            let state = self.state.lock().expect("canonical state lock poisoned");
            let Some(state) = state.as_ref() else {
                return;
            };
            (
                state
                    .topology
                    .panes
                    .iter()
                    .flat_map(|pane| {
                        pane.session_links
                            .iter()
                            .map(|link| link.session_id.clone())
                    })
                    .collect(),
                state
                    .topology
                    .panes
                    .iter()
                    .map(|pane| pane.pane_instance.clone())
                    .collect(),
            )
        };
        self.status_push
            .lock()
            .expect("status push lock poisoned")
            .retain_topology_targets(&sessions, &panes);
    }

    fn log_status_push_error(&self, message: &str) {
        if crate::daemon::lifecycle::append_daemon_log(
            &self.env,
            &self.incarnation.hash,
            &format!("status_push: {message}"),
        )
        .is_err()
        {
            eprintln!("[vde-tmux] {message}");
        }
    }

    fn log_daemon_error(&self, message: &str) {
        if crate::daemon::lifecycle::append_daemon_log(&self.env, &self.incarnation.hash, message)
            .is_err()
        {
            eprintln!("[vde-tmux] {message}");
        }
    }

    fn schedule_sidebar_effect(
        &self,
        effect: super::runtime::CanonicalSidebarEffect,
        original_accepted_seq: u64,
        event_id: EventId,
        snapshot_revision: u64,
    ) -> std::result::Result<(), ErrorCode> {
        let expected_pane = match &effect {
            super::runtime::CanonicalSidebarEffect::JumpPane { pane_instance, .. }
            | super::runtime::CanonicalSidebarEffect::PeekPane { pane_instance, .. } => {
                Some(pane_instance.clone())
            }
            super::runtime::CanonicalSidebarEffect::JumpLatestUnread { candidates, .. }
            | super::runtime::CanonicalSidebarEffect::ReadPeekAdvance { candidates, .. } => {
                if candidates.is_empty() {
                    return Err(ErrorCode::StaleSelection);
                }
                None
            }
        };
        let exists = expected_pane.as_ref().is_none_or(|expected_pane| {
            self.state
                .lock()
                .expect("canonical state lock poisoned")
                .as_ref()
                .is_some_and(|state| state.contains_pane(expected_pane))
        });
        if !exists {
            return Err(ErrorCode::StaleSelection);
        }
        enqueue_sidebar_tmux_job(
            &self.sidebar_tmux_tx,
            &self.deferred_responses,
            SidebarTmuxJob {
                effect,
                original_accepted_seq,
                event_id,
                snapshot_revision,
            },
        )
    }

    fn is_deferred_response(&self, accepted_seq: u64) -> bool {
        self.deferred_responses
            .lock()
            .expect("deferred response lock poisoned")
            .contains(&accepted_seq)
    }

    fn finish_deferred_response(&self, accepted_seq: u64) {
        self.deferred_responses
            .lock()
            .expect("deferred response lock poisoned")
            .remove(&accepted_seq);
    }

    fn fail_stop(&self, message: impl Into<String>) {
        let message = message.into();
        let snapshot_cache = self
            .snapshot_cache
            .lock()
            .expect("snapshot cache lock poisoned");
        let first_shutdown = !self.shutdown.swap(true, Ordering::SeqCst);
        self.shutdown_ready.store(true, Ordering::SeqCst);
        self.snapshot_changed.notify_all();
        drop(snapshot_cache);
        if first_shutdown {
            self.stop_pane_switch_trigger();
            self.shutdown_tmux_control();
            self.stop_notification_worker();
            self.log_daemon_error(&format!("canonical daemon fail-stop: {message}"));
        }
        self.router
            .lock()
            .expect("v2 router lock poisoned")
            .mark_fatal();
        self.queue
            .lock()
            .expect("v2 queue lock poisoned")
            .items
            .clear();
        let waiters = std::mem::take(&mut *self.waiters.lock().expect("v2 waiter lock poisoned"));
        for (_, waiter) in waiters {
            let _ = waiter.send(ServerMessage::error(
                ErrorCode::InternalError,
                format!("daemon fail-stopped: {message}"),
                None,
            ));
        }
        self.queue_ready.notify_all();
    }

    fn begin_graceful_shutdown(&self, current_accepted_seq: u64) {
        self.begin_shutdown(Some(current_accepted_seq));
    }

    fn begin_signal_shutdown(&self) {
        self.begin_shutdown(None);
        self.mark_shutdown_ready();
    }

    fn begin_shutdown(&self, current_accepted_seq: Option<u64>) {
        self.stop_pane_switch_trigger();
        self.shutdown_tmux_control();
        self.stop_notification_worker();
        self.write_status_shutdown_projection();
        let snapshot_cache = self
            .snapshot_cache
            .lock()
            .expect("snapshot cache lock poisoned");
        self.shutdown.store(true, Ordering::SeqCst);
        self.snapshot_changed.notify_all();
        drop(snapshot_cache);
        self.router
            .lock()
            .expect("v2 router lock poisoned")
            .mark_fatal();
        self.queue
            .lock()
            .expect("v2 queue lock poisoned")
            .items
            .clear();
        let mut waiters = self.waiters.lock().expect("v2 waiter lock poisoned");
        let current = current_accepted_seq.and_then(|accepted_seq| {
            waiters
                .remove(&accepted_seq)
                .map(|waiter| (accepted_seq, waiter))
        });
        let abandoned = std::mem::take(&mut *waiters);
        if let Some((accepted_seq, current)) = current {
            waiters.insert(accepted_seq, current);
        }
        drop(waiters);
        for (_, waiter) in abandoned {
            let _ = waiter.send(ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is shutting down",
                None,
            ));
        }
        self.queue_ready.notify_all();
    }

    fn stop_notification_worker(&self) {
        self.notification_shutdown.store(true, Ordering::SeqCst);
        let _process_guard = self
            .notification_process_lock
            .lock()
            .expect("notification process lock poisoned during shutdown");
        if let Err(error) = crate::daemon::lifecycle::terminate_active_notification(
            &self.env,
            &self.incarnation.hash,
        ) {
            self.log_daemon_error(&format!(
                "failed to terminate active notification during daemon shutdown: {error:#}"
            ));
        }
    }

    fn mark_shutdown_ready(&self) {
        let snapshot_cache = self
            .snapshot_cache
            .lock()
            .expect("snapshot cache lock poisoned");
        self.shutdown_ready.store(true, Ordering::SeqCst);
        self.snapshot_changed.notify_all();
        drop(snapshot_cache);
    }

    fn wait_for_shutdown(&self) {
        let mut snapshot_cache = self
            .snapshot_cache
            .lock()
            .expect("snapshot cache lock poisoned");
        while !self.shutdown_ready.load(Ordering::SeqCst) {
            snapshot_cache = self
                .snapshot_changed
                .wait(snapshot_cache)
                .expect("snapshot cache lock poisoned while waiting for shutdown");
        }
    }
}

fn handle_v2_runtime_stream(
    coordinator: Arc<ProductionV2Coordinator>,
    stream: UnixStream,
    connection_permit: &mut V2ConnectionThreadPermit,
) -> Result<()> {
    let mut connection = V2FrameReader::new(stream);
    let frame = match read_v2_request_frame(&mut connection) {
        Ok(frame) => frame,
        Err(response) => {
            let _ = write_v2_response(connection.stream_mut(), &response);
            return Ok(());
        }
    };
    let mut connection_state = V2ConnectionState::default();
    let message = match crate::daemon::protocol::v2::decode_request_frame(&frame) {
        Ok(message) => message,
        Err(response) => {
            let _ = write_v2_response(connection.stream_mut(), &response);
            return Ok(());
        }
    };
    let refresh_current_views = matches!(&message, ClientMessage::SubmitViewEvent { .. });
    let response = coordinator.route_external(&mut connection_state, message, frame.len());
    write_v2_response(connection.stream_mut(), &response)
        .map_err(|error| anyhow::anyhow!("failed to write v2 handshake: {error:?}"))?;
    if refresh_current_views && matches!(response, ServerMessage::ViewQueued { .. }) {
        coordinator.schedule_current_view_refresh();
    }
    if !matches!(response, ServerMessage::HelloAck { .. }) {
        return Ok(());
    }

    let frame = match read_v2_request_frame(&mut connection) {
        Ok(frame) => frame,
        Err(response) => {
            let _ = write_v2_response(connection.stream_mut(), &response);
            return Ok(());
        }
    };
    let message = match crate::daemon::protocol::v2::decode_request_frame(&frame) {
        Ok(message) => message,
        Err(response) => {
            let _ = write_v2_response(connection.stream_mut(), &response);
            return Ok(());
        }
    };
    let subscribe = matches!(&message, ClientMessage::Subscribe { .. });
    if subscribe {
        if !connection_permit.try_mark_streaming() {
            let response = ServerMessage::error(
                ErrorCode::QueueFull,
                "daemon streaming connection capacity is full",
                None,
            );
            let _ = write_v2_response(connection.stream_mut(), &response);
            return Ok(());
        }
        let published = match coordinator.route_subscription(&mut connection_state, message) {
            Ok(published) => published,
            Err(response) => {
                let _ = write_v2_response(connection.stream_mut(), &response);
                return Ok(());
            }
        };
        if let Err(error) = write_v2_frame(connection.stream_mut(), &published.frame) {
            let _ = coordinator.enqueue_internal(V2InternalMutation::DiagnosticProjection {
                pane_instance: None,
                message: format!("subscriber_initial_write_failed: {error:?}"),
            });
            return Ok(());
        }
        if published.terminal {
            return Ok(());
        }
        return stream_v2_subscription(coordinator, connection.into_stream(), published.revision);
    }
    let refresh_current_views = matches!(&message, ClientMessage::SubmitViewEvent { .. });
    let response = coordinator.route_external(&mut connection_state, message, frame.len());
    if write_v2_response(connection.stream_mut(), &response).is_ok()
        && refresh_current_views
        && matches!(response, ServerMessage::ViewQueued { .. })
    {
        coordinator.schedule_current_view_refresh();
    }
    Ok(())
}

fn stream_v2_subscription(
    coordinator: Arc<ProductionV2Coordinator>,
    stream: UnixStream,
    last_revision: u64,
) -> Result<()> {
    stream_v2_subscription_with_heartbeat_interval(
        coordinator,
        stream,
        last_revision,
        V2_STREAM_HEARTBEAT_INTERVAL,
    )
}

fn stream_v2_subscription_with_heartbeat_interval(
    coordinator: Arc<ProductionV2Coordinator>,
    mut stream: UnixStream,
    mut last_revision: u64,
    heartbeat_interval: Duration,
) -> Result<()> {
    let daemon_instance_id = coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .daemon_instance_id()
        .clone();
    while let Some(outcome) = coordinator.wait_for_snapshot_after(last_revision, heartbeat_interval)
    {
        match outcome {
            SnapshotWaitOutcome::Published(published) => {
                if let Err(error) = write_v2_frame(&mut stream, &published.frame) {
                    let _ =
                        coordinator.enqueue_internal(V2InternalMutation::DiagnosticProjection {
                            pane_instance: None,
                            message: format!(
                                "subscriber_write_failed: after_revision={last_revision} error={error:?}"
                            ),
                        });
                    break;
                }
                last_revision = published.revision;
                if published.terminal {
                    break;
                }
            }
            SnapshotWaitOutcome::HeartbeatDue { snapshot_revision } => {
                let heartbeat = ServerMessage::Heartbeat {
                    daemon_instance_id: daemon_instance_id.clone(),
                    snapshot_revision,
                };
                let Ok(frame) = crate::daemon::protocol::v2::encode_response_frame(&heartbeat)
                else {
                    break;
                };
                if write_v2_frame(&mut stream, &frame).is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn start_v2_mutation_worker(coordinator: Arc<ProductionV2Coordinator>) {
    thread::spawn(move || {
        loop {
            let mutation = {
                let mut queue = coordinator.queue.lock().expect("v2 queue lock poisoned");
                while queue.items.is_empty() && !coordinator.shutdown.load(Ordering::SeqCst) {
                    queue = coordinator
                        .queue_ready
                        .wait(queue)
                        .expect("v2 queue lock poisoned while waiting");
                }
                if coordinator.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                queue.in_flight = true;
                queue.items.pop_front()
            };
            let Some(mutation) = mutation else {
                continue;
            };
            debug_assert!(mutation.raw_frame_bytes <= crate::pane_state::MAX_REQUEST_FRAME_BYTES);
            let accepted_seq = mutation.sequenced.accepted_seq;
            let graceful_shutdown = matches!(
                &mutation.sequenced.mutation,
                V2AcceptedMutation::External(ClientMessage::Shutdown { .. })
            );
            let changes_topology_targets = mutation_changes_topology_targets(&mutation.sequenced);
            let status_driver = changes_topology_targets.then(|| {
                coordinator
                    .status_push_driver
                    .lock()
                    .expect("status push driver lock poisoned")
            });
            let response = apply_production_mutation(&coordinator, mutation.sequenced);
            if changes_topology_targets {
                coordinator.sync_status_push_topology_targets_locked();
            }
            drop(status_driver);
            if let Err(error) = coordinator.publish_resolved_snapshot() {
                coordinator.fail_stop(error.to_string());
            }
            if !coordinator.is_deferred_response(accepted_seq) {
                coordinator.complete(accepted_seq, response);
            }
            if graceful_shutdown {
                coordinator.mark_shutdown_ready();
            }
            let mut queue = coordinator.queue.lock().expect("v2 queue lock poisoned");
            queue.in_flight = false;
            coordinator.queue_ready.notify_all();
        }
    });
}

fn start_sidebar_completion_forwarder(coordinator: Arc<ProductionV2Coordinator>) {
    let receiver = coordinator
        .sidebar_completion_rx
        .lock()
        .expect("sidebar completion receiver lock poisoned")
        .take()
        .expect("sidebar completion forwarder started once");
    thread::spawn(move || {
        while let Ok(completion) = receiver.recv() {
            if coordinator.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !coordinator.enqueue_internal(V2InternalMutation::SidebarEffectCompleted(completion))
            {
                coordinator
                    .fail_stop("sidebar completion could not enter sequenced mutation queue");
                break;
            }
        }
    });
}

fn start_task_summary_completion_forwarder(coordinator: Arc<ProductionV2Coordinator>) {
    let Some(receiver) = coordinator
        .task_summary_completion_rx
        .lock()
        .expect("task summary completion receiver lock poisoned")
        .take()
    else {
        return;
    };
    thread::spawn(move || {
        while let Ok(completion) = receiver.recv() {
            if coordinator.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !coordinator.enqueue_internal(V2InternalMutation::TaskSummaryCompleted(completion)) {
                coordinator.log_daemon_error(
                    "task summary completion could not enter sequenced mutation queue",
                );
            }
        }
    });
}

fn start_agent_prompt_timeout_worker(coordinator: Arc<ProductionV2Coordinator>) {
    thread::spawn(move || {
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(250)
                && !coordinator.shutdown.load(Ordering::SeqCst)
            {
                thread::sleep(Duration::from_millis(25));
            }
            if coordinator.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let observed_at = epoch_seconds();
            let expired = coordinator
                .agent_runtime
                .lock()
                .expect("agent runtime lock poisoned")
                .as_ref()
                .is_some_and(|runtime| runtime.has_expired_dispatch(observed_at).unwrap_or(true));
            if !expired {
                continue;
            }
            if !coordinator
                .enqueue_internal(V2InternalMutation::AgentPromptTimeouts { observed_at })
            {
                break;
            }
        }
    });
}

fn mutation_changes_topology_targets(mutation: &V2SequencedMutation) -> bool {
    match &mutation.mutation {
        V2AcceptedMutation::External(ClientMessage::RefreshTopology { .. })
        | V2AcceptedMutation::Internal(
            V2InternalMutation::ObservationBatch(_)
            | V2InternalMutation::RefreshTopology
            | V2InternalMutation::TargetedPaneRefresh { .. },
        ) => true,
        V2AcceptedMutation::External(ClientMessage::SubmitPaneEvent { envelope, .. }) => {
            matches!(envelope.event, PaneEvent::PaneRemoved { .. })
        }
        V2AcceptedMutation::Internal(V2InternalMutation::PaneEvent(envelope)) => {
            matches!(envelope.event, PaneEvent::PaneRemoved { .. })
        }
        _ => false,
    }
}

fn start_canonical_observation_worker(
    coordinator: Arc<ProductionV2Coordinator>,
    poll: Duration,
    capture: crate::daemon::workers::CaptureCoordinatorHandle,
) {
    thread::spawn(move || {
        let mut last_hook_check = Instant::now();
        let mut last_port_scan = None;
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let (dispatch, view_base, through_unread_order) = {
                let state_guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                let Some(state) = state_guard.as_ref() else {
                    thread::sleep(poll);
                    continue;
                };
                let mut panes = state
                    .topology
                    .panes
                    .iter()
                    .map(|pane| pane.pane_instance.clone())
                    .collect::<Vec<_>>();
                panes.extend(state.leased.runtime.tracked_panes());
                panes.sort();
                panes.dedup();
                (
                    state.leased.runtime.freeze_observation_dispatch(panes),
                    state.views.clone(),
                    state.leased.runtime.latest_unread_order(),
                )
            };
            let daemon_instance_id = coordinator
                .router
                .lock()
                .expect("v2 router lock poisoned")
                .daemon_instance_id()
                .clone();
            let mut projection =
                match query_observation_poll_projection(&coordinator, Duration::from_secs(1)) {
                    Ok(projection) => projection,
                    Err(error) if error.requires_daemon_exit() => {
                        coordinator.fail_stop(error.to_string());
                        break;
                    }
                    Err(error) => {
                        for snapshot in &dispatch {
                            match crate::daemon::workers::observation_envelope(
                                daemon_instance_id.clone(),
                                snapshot.pane_instance.clone(),
                                snapshot.base.clone(),
                                &snapshot.tracker,
                                crate::daemon::workers::ObservationSample {
                                    observed_at: epoch_seconds(),
                                    presence: crate::pane_state::AgentPresenceObservation::Unknown,
                                    capture: None,
                                    process: None,
                                },
                            ) {
                                Ok(envelope) => {
                                    let _ = coordinator.enqueue_internal(
                                        V2InternalMutation::PaneEvent(Box::new(envelope)),
                                    );
                                }
                                Err(build_error) => {
                                    coordinator.fail_stop(build_error.to_string());
                                    return;
                                }
                            }
                        }
                        let pane = dispatch
                            .first()
                            .map(|snapshot| snapshot.pane_instance.clone());
                        let _ = coordinator.enqueue_internal(
                            V2InternalMutation::DiagnosticProjection {
                                pane_instance: pane,
                                message: format!("observation_projection_failed: {error}"),
                            },
                        );
                        thread::sleep(poll);
                        continue;
                    }
                };
            projection.observation_bases = dispatch
                .iter()
                .map(|snapshot| (snapshot.pane_instance.clone(), snapshot.base.clone()))
                .collect();
            projection.view_base = view_base;
            projection.through_unread_order = through_unread_order;
            let nvim_markers = coordinator.query_nvim_pane_markers();
            let scan_ports = last_port_scan
                .is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(10));
            let processes = crate::daemon::workers::read_agent_process_snapshot(
                Duration::from_secs(1),
                scan_ports,
            );
            if scan_ports {
                last_port_scan = Some(Instant::now());
            }
            if let Some(markers) = nvim_markers {
                coordinator.cleanup_stale_nvim_pane_markers(&markers, &processes);
            }
            let poll_result = crate::daemon::workers::run_observation_poll(
                &capture,
                &dispatch,
                &processes,
                &daemon_instance_id,
                epoch_seconds(),
            );
            match poll_result {
                Ok(result) => {
                    let current = projection
                        .topology
                        .panes
                        .iter()
                        .map(|pane| pane.pane_instance.clone())
                        .collect::<std::collections::BTreeSet<_>>();
                    let first_pane = dispatch
                        .first()
                        .map(|snapshot| snapshot.pane_instance.clone());
                    let mut diagnostics = Vec::new();
                    let removals = match crate::daemon::workers::pane_removal_envelopes(
                        &daemon_instance_id,
                        &dispatch,
                        &current,
                        true,
                    ) {
                        Ok(removals) => removals,
                        Err(error) => {
                            diagnostics.push((
                                first_pane.clone(),
                                format!("pane_removal_build_failed: {error}"),
                            ));
                            Vec::new()
                        }
                    };
                    diagnostics.extend(
                        result
                            .diagnostics
                            .into_iter()
                            .map(|message| (first_pane.clone(), message)),
                    );
                    let _ = coordinator.enqueue_internal(V2InternalMutation::ObservationBatch(
                        Box::new(ObservationBatchPayload {
                            projection: Box::new(projection),
                            observations: result.envelopes,
                            removals,
                            diagnostics,
                        }),
                    ));
                }
                Err(error) if error.requires_daemon_exit() => {
                    coordinator.fail_stop(error.to_string());
                    break;
                }
                Err(error) => {
                    let _ =
                        coordinator.enqueue_internal(V2InternalMutation::DiagnosticProjection {
                            pane_instance: dispatch
                                .first()
                                .map(|snapshot| snapshot.pane_instance.clone()),
                            message: format!("observation_poll_failed: {error}"),
                        });
                }
            }
            if last_hook_check.elapsed() >= Duration::from_secs(10) {
                let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(1));
                match crate::daemon::view_hooks::monitor_hooks(
                    &runner,
                    &coordinator.incarnation.identity,
                ) {
                    Ok(health) => {
                        let _ = coordinator.enqueue_internal(
                            V2InternalMutation::HookHealthProjection {
                                health,
                                diagnostic: None,
                            },
                        );
                    }
                    Err(crate::daemon::view_hooks::HookError::ServerMismatch) => {
                        coordinator
                            .fail_stop("tmux server incarnation changed during hook monitor");
                        break;
                    }
                    Err(error) => {
                        let _ = coordinator.enqueue_internal(
                            V2InternalMutation::HookHealthProjection {
                                health: HookHealth::Degraded,
                                diagnostic: Some(format!("hook_health_degraded: {error}")),
                            },
                        );
                    }
                }
                last_hook_check = Instant::now();
            }
            thread::sleep(poll);
        }
    });
}

fn start_canonical_git_worker(
    coordinator: Arc<ProductionV2Coordinator>,
    poll: Duration,
    git_timeout: Duration,
) {
    thread::spawn(move || {
        let git = crate::daemon::workers::system_git_runner(git_timeout);
        let mut poller = crate::git::GitPoller::new();
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let paths = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned")
                .as_ref()
                .map(|state| state.git_polling_paths())
                .unwrap_or_default();
            let (badges, worktrees, repo_identities) =
                poller.poll_with_identities(&git, paths.iter().map(String::as_str), Instant::now());
            let _ = coordinator.enqueue_internal(V2InternalMutation::GitProjection {
                badges,
                worktrees,
                repo_identities,
            });
            let started = Instant::now();
            while started.elapsed() < poll && !coordinator.shutdown.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100).min(poll));
            }
        }
    });
}

fn start_status_push_worker(coordinator: Arc<ProductionV2Coordinator>) {
    thread::spawn(move || {
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            for trigger in [StatusPushTrigger::Snapshot, StatusPushTrigger::Flush] {
                if let Err(error) = coordinator.drive_status_push(trigger) {
                    if coordinator.shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    coordinator.log_status_push_error(&format!(
                        "status display projection failed: {error:#}"
                    ));
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

fn start_tmux_server_liveness_monitor(coordinator: Arc<ProductionV2Coordinator>) -> Result<()> {
    let server_pid = coordinator.incarnation.identity.pid;
    let expected_start_token = crate::daemon::lifecycle::process_start_token(server_pid)
        .with_context(|| {
            format!("failed to identify tmux server process before starting liveness monitor: {server_pid}")
        })?;
    thread::spawn(move || {
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let started = Instant::now();
            while started.elapsed() < TMUX_SERVER_LIVENESS_POLL_INTERVAL
                && !coordinator.shutdown.load(Ordering::SeqCst)
            {
                thread::sleep(
                    Duration::from_millis(50)
                        .min(TMUX_SERVER_LIVENESS_POLL_INTERVAL.saturating_sub(started.elapsed())),
                );
            }
            if coordinator.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !crate::daemon::lifecycle::process_start_token(server_pid)
                .is_ok_and(|actual| actual == expected_start_token)
            {
                coordinator.fail_stop(format!("tmux server process exited: pid={server_pid}"));
                break;
            }
        }
    });
    Ok(())
}

fn apply_production_mutation(
    coordinator: &ProductionV2Coordinator,
    sequenced: V2SequencedMutation,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ClientMessage, ErrorCode, ServerMessage};

    let accepted_seq = sequenced.accepted_seq;
    let response = match sequenced.mutation {
        V2AcceptedMutation::External(ClientMessage::SubmitPaneEvent { envelope, .. }) => {
            apply_external_pane_event(coordinator, accepted_seq, envelope)
        }
        V2AcceptedMutation::External(ClientMessage::SubmitProviderEvent {
            envelope,
            observation,
            ..
        }) => apply_external_provider_event(coordinator, accepted_seq, envelope, observation),
        V2AcceptedMutation::External(ClientMessage::StartAgentPrompt {
            event_id,
            target_agent_ref,
            operation_id,
            prompt_base64,
            prompt_digest,
            dispatch_option,
            observed_at,
            ..
        }) => apply_start_agent_prompt(
            coordinator,
            event_id,
            target_agent_ref,
            operation_id,
            prompt_base64,
            prompt_digest,
            dispatch_option,
            observed_at,
        ),
        V2AcceptedMutation::External(ClientMessage::ResolveAgentRun {
            event_id,
            run_ref,
            outcome,
            precondition,
            resolution_id,
            reason,
            actor_pid,
            ..
        }) => apply_resolve_agent_run(
            coordinator,
            event_id,
            run_ref,
            outcome,
            precondition,
            resolution_id,
            reason,
            actor_pid,
        ),
        V2AcceptedMutation::External(ClientMessage::SubmitViewEvent { event, .. }) => {
            apply_external_view_event(coordinator, accepted_seq, event)
        }
        V2AcceptedMutation::External(ClientMessage::RefreshTopology { event_id, .. }) => {
            match refresh_full_topology(coordinator) {
                Ok(revision) => ServerMessage::SnapshotAck {
                    event_id,
                    accepted_seq,
                    snapshot_revision: revision,
                },
                Err(error) => production_store_error_response(coordinator, error, Some(event_id)),
            }
        }
        V2AcceptedMutation::External(ClientMessage::SidebarCommand {
            event_id, command, ..
        }) => match command {
            crate::daemon::protocol::v2::SidebarCommand::MarkComplete {
                pane_instance,
                expected,
            } => {
                let envelope = PaneEventEnvelope {
                    daemon_instance_id: coordinator
                        .router
                        .lock()
                        .expect("v2 router lock poisoned")
                        .daemon_instance_id()
                        .clone(),
                    event_id,
                    pane_instance,
                    agent: None,
                    agent_session_id: None,
                    event: PaneEvent::MarkDone {
                        expected,
                        completed_at: epoch_seconds(),
                    },
                };
                apply_external_pane_event(coordinator, accepted_seq, envelope)
            }
            crate::daemon::protocol::v2::SidebarCommand::JumpPane {
                pane_instance,
                source_pane,
            } => {
                let (revision, clients) = {
                    let guard = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned");
                    let state = guard
                        .as_ref()
                        .expect("state initialized before sidebar command");
                    (
                        state.leased.runtime.snapshot_revision(),
                        unique_eligible_client_pid(&state.views, &source_pane),
                    )
                };
                let client_pid = match clients {
                    Ok(client_pid) => client_pid,
                    Err(count) => {
                        return ServerMessage::error(
                            ErrorCode::StaleSelection,
                            format!(
                                "source pane must identify exactly one eligible tmux client: {}:{} matched {}",
                                source_pane.pane_id, source_pane.pane_pid, count
                            ),
                            Some(event_id),
                        );
                    }
                };
                let effect = super::runtime::CanonicalSidebarEffect::JumpPane {
                    pane_instance: pane_instance.clone(),
                    client_pid,
                    source_pane,
                };
                if let Err(code) = coordinator.schedule_sidebar_effect(
                    effect,
                    accepted_seq,
                    event_id.clone(),
                    revision,
                ) {
                    ServerMessage::error(
                        code,
                        format!(
                            "sidebar pane selection is stale: {}:{}",
                            pane_instance.pane_id, pane_instance.pane_pid
                        ),
                        Some(event_id),
                    )
                } else {
                    ServerMessage::SnapshotAck {
                        event_id,
                        accepted_seq,
                        snapshot_revision: revision,
                    }
                }
            }
            crate::daemon::protocol::v2::SidebarCommand::JumpLatestUnread { source_pane } => {
                let (revision, clients, candidates) = {
                    let guard = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned");
                    let state = guard
                        .as_ref()
                        .expect("state initialized before sidebar command");
                    (
                        state.leased.runtime.snapshot_revision(),
                        unique_eligible_client_pid(&state.views, &source_pane),
                        state.latest_unread_candidates(),
                    )
                };
                let client_pid = match clients {
                    Ok(client_pid) => client_pid,
                    Err(count) => {
                        return ServerMessage::error(
                            ErrorCode::StaleSelection,
                            format!(
                                "source pane must identify exactly one eligible tmux client: {}:{} matched {}",
                                source_pane.pane_id, source_pane.pane_pid, count
                            ),
                            Some(event_id),
                        );
                    }
                };
                if candidates.is_empty() {
                    return ServerMessage::error(
                        ErrorCode::StaleSelection,
                        "no eligible unread pane",
                        Some(event_id),
                    );
                }
                let effect = super::runtime::CanonicalSidebarEffect::JumpLatestUnread {
                    candidates,
                    client_pid,
                    source_pane,
                };
                if let Err(code) = coordinator.schedule_sidebar_effect(
                    effect,
                    accepted_seq,
                    event_id.clone(),
                    revision,
                ) {
                    ServerMessage::error(code, "unread pane selection is stale", Some(event_id))
                } else {
                    ServerMessage::SnapshotAck {
                        event_id,
                        accepted_seq,
                        snapshot_revision: revision,
                    }
                }
            }
            crate::daemon::protocol::v2::SidebarCommand::PeekPane {
                pane_instance,
                source_pane,
                client_pid,
            } => {
                let observation =
                    match query_client_witnesses(coordinator, Duration::from_millis(250)) {
                        Ok(observation) => observation,
                        Err(error) if error.requires_daemon_exit() => {
                            coordinator.fail_stop(error.to_string());
                            return ServerMessage::error(
                                ErrorCode::InternalError,
                                error.to_string(),
                                Some(event_id),
                            );
                        }
                        Err(error) => {
                            return ServerMessage::error(
                                ErrorCode::StaleSelection,
                                format!("peek client witness is unavailable: {error}"),
                                Some(event_id),
                            );
                        }
                    };
                let revision = {
                    let mut guard = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned");
                    let state = guard
                        .as_mut()
                        .expect("state initialized before sidebar command");
                    state.reconcile_peek_leases(&observation.witnesses, observation.seq);
                    if !eligible_witness_matches(&observation.witnesses, client_pid, &source_pane) {
                        return ServerMessage::error(
                            ErrorCode::StaleSelection,
                            "peek source does not match the requested tmux client",
                            Some(event_id),
                        );
                    }
                    if !state.contains_pane(&pane_instance) {
                        return ServerMessage::error(
                            ErrorCode::StaleSelection,
                            "peek target is stale",
                            Some(event_id),
                        );
                    }
                    if !state.begin_peek(
                        client_pid,
                        source_pane.clone(),
                        [pane_instance.clone()],
                        accepted_seq,
                    ) {
                        return ServerMessage::error(
                            ErrorCode::QueueFull,
                            "a peek operation is already in flight for this client",
                            Some(event_id),
                        );
                    }
                    state.leased.runtime.snapshot_revision()
                };
                let effect = super::runtime::CanonicalSidebarEffect::PeekPane {
                    pane_instance,
                    client_pid,
                    source_pane,
                };
                if let Err(code) = coordinator.schedule_sidebar_effect(
                    effect,
                    accepted_seq,
                    event_id.clone(),
                    revision,
                ) {
                    if let Some(state) = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned")
                        .as_mut()
                    {
                        state.restore_peek_after_failure(
                            client_pid,
                            accepted_seq,
                            &observation.witnesses,
                            observation.seq,
                        );
                    }
                    ServerMessage::error(code, "peek target is stale", Some(event_id))
                } else {
                    ServerMessage::SnapshotAck {
                        event_id,
                        accepted_seq,
                        snapshot_revision: revision,
                    }
                }
            }
            crate::daemon::protocol::v2::SidebarCommand::ReadPeek {
                source_pane,
                client_pid,
                advance_candidates,
            } => {
                let observation =
                    match query_client_witnesses(coordinator, Duration::from_millis(250)) {
                        Ok(observation) => observation,
                        Err(error) if error.requires_daemon_exit() => {
                            coordinator.fail_stop(error.to_string());
                            return ServerMessage::error(
                                ErrorCode::InternalError,
                                error.to_string(),
                                Some(event_id),
                            );
                        }
                        Err(error) => {
                            return ServerMessage::error(
                                ErrorCode::StaleSelection,
                                format!("read-current client witness is unavailable: {error}"),
                                Some(event_id),
                            );
                        }
                    };
                let daemon_instance_id = coordinator
                    .router
                    .lock()
                    .expect("v2 router lock poisoned")
                    .daemon_instance_id()
                    .clone();
                let (revision, read_outcome, candidates) = {
                    let mut guard = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned");
                    let state = guard
                        .as_mut()
                        .expect("state initialized before sidebar command");
                    state.reconcile_peek_leases(&observation.witnesses, observation.seq);
                    if !eligible_witness_matches(&observation.witnesses, client_pid, &source_pane) {
                        return ServerMessage::error(
                            ErrorCode::StaleSelection,
                            "read source does not match the requested tmux client",
                            Some(event_id),
                        );
                    }
                    let Some(target) = state.active_peek_target(client_pid).cloned() else {
                        return ServerMessage::error(
                            ErrorCode::StaleSelection,
                            "read-current requires an active peek lease",
                            Some(event_id),
                        );
                    };
                    if target != source_pane {
                        return ServerMessage::error(
                            ErrorCode::StaleSelection,
                            "active peek target no longer matches the client focus",
                            Some(event_id),
                        );
                    }
                    let mut io = pane_snapshot_store(coordinator);
                    match commit_read_peek_state(
                        state,
                        &mut io,
                        &daemon_instance_id,
                        &event_id,
                        &target,
                        client_pid,
                        advance_candidates,
                        accepted_seq,
                    ) {
                        Ok(result) => {
                            if result.candidates.is_empty() {
                                let witness_observation_floor =
                                    coordinator.witness_observation_seq.load(Ordering::SeqCst);
                                let renewed = state.renew_active_peek(
                                    client_pid,
                                    &target,
                                    witness_observation_floor,
                                );
                                debug_assert!(
                                    renewed,
                                    "read-current without advance starts a new active lease interval"
                                );
                            }
                            (result.revision, result.read_outcome, result.candidates)
                        }
                        Err(error) => {
                            return production_store_error_response(
                                coordinator,
                                error,
                                Some(event_id),
                            );
                        }
                    }
                };
                if candidates.is_empty() {
                    ServerMessage::SidebarReadPeekResult {
                        event_id,
                        accepted_seq,
                        snapshot_revision: revision,
                        read_outcome,
                        advance_outcome: crate::daemon::protocol::v2::PeekAdvanceOutcome::Stayed,
                    }
                } else {
                    let effect = super::runtime::CanonicalSidebarEffect::ReadPeekAdvance {
                        candidates,
                        client_pid,
                        source_pane,
                        read_outcome,
                    };
                    if coordinator
                        .schedule_sidebar_effect(effect, accepted_seq, event_id.clone(), revision)
                        .is_err()
                    {
                        if let Some(state) = coordinator
                            .state
                            .lock()
                            .expect("canonical state lock poisoned")
                            .as_mut()
                        {
                            state.restore_peek_after_failure(
                                client_pid,
                                accepted_seq,
                                &observation.witnesses,
                                observation.seq,
                            );
                        }
                        ServerMessage::SidebarReadPeekResult {
                            event_id,
                            accepted_seq,
                            snapshot_revision: revision,
                            read_outcome,
                            advance_outcome:
                                crate::daemon::protocol::v2::PeekAdvanceOutcome::Failed,
                        }
                    } else {
                        ServerMessage::SnapshotAck {
                            event_id,
                            accepted_seq,
                            snapshot_revision: revision,
                        }
                    }
                }
            }
            crate::daemon::protocol::v2::SidebarCommand::PreferenceIntent { intent } => {
                apply_sidebar_preference_intent(coordinator, accepted_seq, event_id, intent)
            }
            crate::daemon::protocol::v2::SidebarCommand::CategoryIntent { intent } => {
                apply_category_intent(coordinator, accepted_seq, event_id, intent)
            }
            crate::daemon::protocol::v2::SidebarCommand::SetNavigation {
                selection,
                scroll,
                manual_scroll,
            } => apply_sidebar_navigation(
                coordinator,
                accepted_seq,
                event_id,
                selection,
                scroll,
                manual_scroll,
            ),
        },
        V2AcceptedMutation::External(ClientMessage::Shutdown { event_id, .. }) => {
            coordinator.begin_graceful_shutdown(accepted_seq);
            ServerMessage::ShutdownAccepted {
                event_id,
                accepted_seq,
            }
        }
        V2AcceptedMutation::External(
            ClientMessage::Hello { .. }
            | ClientMessage::QueryResolvedSnapshot { .. }
            | ClientMessage::QueryStatusSnapshot { .. }
            | ClientMessage::QueryPane { .. }
            | ClientMessage::QueryRuntimeInfo { .. }
            | ClientMessage::QueryAgentRun { .. }
            | ClientMessage::QueryCurrentAgentRuns { .. }
            | ClientMessage::QueryAgentOperation { .. }
            | ClientMessage::QueryAgentResponse { .. }
            | ClientMessage::QueryAgentStorage { .. }
            | ClientMessage::PaneSwitch { .. }
            | ClientMessage::Subscribe { .. },
        ) => unreachable!("v2 router cannot sequence a read-only request"),
        V2AcceptedMutation::Internal(V2InternalMutation::TargetedPaneRefresh { pane_id }) => {
            targeted_pane_refresh_response(coordinator, &pane_id)
        }
        V2AcceptedMutation::Internal(V2InternalMutation::RefreshTopology) => {
            match refresh_full_topology(coordinator) {
                Ok(revision) => ServerMessage::SnapshotAck {
                    event_id: EventId::generate()
                        .expect("OS random source failed after daemon startup"),
                    accepted_seq,
                    snapshot_revision: revision,
                },
                Err(error) => production_store_error_response(coordinator, error, None),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::ReconcileViews) => {
            match initial_view_reconciliation(coordinator) {
                Ok(()) => {
                    let revision = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned")
                        .as_ref()
                        .map_or(0, |state| state.leased.runtime.snapshot_revision());
                    ServerMessage::SnapshotAck {
                        event_id: EventId::generate()
                            .expect("OS random source failed after daemon startup"),
                        accepted_seq,
                        snapshot_revision: revision,
                    }
                }
                Err(error) => observation_poll_error_response(coordinator, error),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::CurrentViewsReplacement {
            observation_seq,
            witnesses,
            through_unread_order,
        }) => {
            match reconcile_views_with_witnesses(
                coordinator,
                observation_seq,
                &witnesses,
                through_unread_order,
                None,
                None,
            ) {
                Ok(()) => {
                    let revision = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned")
                        .as_ref()
                        .map_or(0, |state| state.leased.runtime.snapshot_revision());
                    ServerMessage::SnapshotAck {
                        event_id: EventId::generate()
                            .expect("OS random source failed after daemon startup"),
                        accepted_seq,
                        snapshot_revision: revision,
                    }
                }
                Err(error) => observation_poll_error_response(coordinator, error),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::PaneEvent(envelope)) => {
            apply_external_pane_event(coordinator, accepted_seq, *envelope)
        }
        V2AcceptedMutation::Internal(V2InternalMutation::DiagnosticProjection {
            pane_instance,
            message,
        }) => match apply_diagnostic_projection(coordinator, pane_instance, message) {
            Ok(revision) => ServerMessage::SnapshotAck {
                event_id: EventId::generate()
                    .expect("OS random source failed after daemon startup"),
                accepted_seq,
                snapshot_revision: revision,
            },
            Err(error) => production_store_error_response(coordinator, error, None),
        },
        V2AcceptedMutation::Internal(V2InternalMutation::ObservationBatch(payload)) => {
            apply_observation_batch(coordinator, accepted_seq, *payload)
        }
        V2AcceptedMutation::Internal(V2InternalMutation::FrameTooLargeProjection {
            rejected_revision,
        }) => {
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized before frame-size diagnostic");
            if let Err(error) = state.record_frame_too_large_diagnostic(rejected_revision) {
                return production_store_error_response(coordinator, error, None);
            }
            ServerMessage::SnapshotAck {
                event_id: EventId::generate()
                    .expect("OS random source failed after daemon startup"),
                accepted_seq,
                snapshot_revision: state.leased.runtime.snapshot_revision(),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::HookHealthProjection {
            health,
            diagnostic,
        }) => {
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized before hook health projection");
            if let Err(error) = state.set_hook_health(health, diagnostic) {
                return production_store_error_response(coordinator, error, None);
            }
            coordinator
                .router
                .lock()
                .expect("v2 router lock poisoned")
                .set_hook_health(health);
            ServerMessage::SnapshotAck {
                event_id: EventId::generate()
                    .expect("OS random source failed after daemon startup"),
                accepted_seq,
                snapshot_revision: state.leased.runtime.snapshot_revision(),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::AgentPromptTimeouts { observed_at }) => {
            let settled = coordinator
                .agent_runtime
                .lock()
                .expect("agent runtime lock poisoned")
                .as_mut()
                .expect("agent runtime initialized before timeout worker")
                .settle_expired_dispatches(observed_at);
            match settled {
                Ok(_) => ServerMessage::SnapshotAck {
                    event_id: EventId::generate()
                        .expect("OS random source failed after daemon startup"),
                    accepted_seq,
                    snapshot_revision: coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned")
                        .as_ref()
                        .map_or(0, |state| state.leased.runtime.snapshot_revision()),
                },
                Err(error) => agent_state_query_error(error),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::TaskSummaryCompleted(completion)) => {
            let summary = match completion.result {
                Ok(summary) => summary,
                Err(error) => {
                    coordinator.log_daemon_error(&format!(
                        "task summary generation failed for pane {}: {error}",
                        completion.pane_instance.pane_id
                    ));
                    crate::pane_state::TaskSummaryState {
                        text: None,
                        context_fingerprint: completion.context_fingerprint,
                        generated_at: epoch_seconds(),
                        outcome: crate::pane_state::TaskSummaryOutcome::Failed,
                        failure_code: Some(task_summary_failure_code(&error).to_string()),
                    }
                }
            };
            let envelope = PaneEventEnvelope {
                daemon_instance_id: coordinator
                    .router
                    .lock()
                    .expect("v2 router lock poisoned")
                    .daemon_instance_id()
                    .clone(),
                event_id: EventId::generate()
                    .expect("OS random source failed after daemon startup"),
                pane_instance: completion.pane_instance,
                agent: None,
                agent_session_id: None,
                event: PaneEvent::TaskSummaryGenerated {
                    expected_state_id: completion.state_id,
                    expected_agent_epoch: completion.agent_epoch,
                    summary,
                },
            };
            apply_external_pane_event(coordinator, accepted_seq, envelope)
        }
        V2AcceptedMutation::Internal(V2InternalMutation::SidebarEffectCompleted(completion)) => {
            let restores_previous_peek = matches!(
                &completion.effect,
                super::runtime::CanonicalSidebarEffect::PeekPane { .. }
                    | super::runtime::CanonicalSidebarEffect::ReadPeekAdvance { .. }
            ) && !matches!(
                &completion.result,
                SidebarEffectResult::Succeeded(_)
                    | SidebarEffectResult::SourceClientMismatch
                    | SidebarEffectResult::ServerIncarnationMismatch
            );
            let failure_observation = if restores_previous_peek {
                match query_client_witnesses(coordinator, Duration::from_millis(250)) {
                    Ok(observation) => Some(observation),
                    Err(error) => {
                        if error.requires_daemon_exit() {
                            coordinator.fail_stop(error.to_string());
                        }
                        None
                    }
                }
            } else {
                None
            };
            let fail_stop = matches!(
                &completion.result,
                SidebarEffectResult::ServerIncarnationMismatch
            );
            let succeeded_target = match &completion.result {
                SidebarEffectResult::Succeeded(target) => Some(target.clone()),
                _ => None,
            };
            let mut reconcile_after_success = false;
            {
                let mut guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                if let Some(state) = guard.as_mut() {
                    match (&completion.effect, succeeded_target.as_ref()) {
                        (
                            super::runtime::CanonicalSidebarEffect::PeekPane { client_pid, .. }
                            | super::runtime::CanonicalSidebarEffect::ReadPeekAdvance {
                                client_pid,
                                ..
                            },
                            Some(target),
                        ) => state.activate_peek(
                            *client_pid,
                            completion.original_accepted_seq,
                            target.clone(),
                            completion.witness_observation_floor,
                        ),
                        (
                            super::runtime::CanonicalSidebarEffect::PeekPane { client_pid, .. }
                            | super::runtime::CanonicalSidebarEffect::ReadPeekAdvance {
                                client_pid,
                                ..
                            },
                            None,
                        ) => state.restore_peek_after_failure(
                            *client_pid,
                            completion.original_accepted_seq,
                            failure_observation
                                .as_ref()
                                .map_or(&[], |observation| observation.witnesses.as_slice()),
                            failure_observation
                                .as_ref()
                                .map_or(0, |observation| observation.seq),
                        ),
                        (
                            super::runtime::CanonicalSidebarEffect::JumpPane { client_pid, .. }
                            | super::runtime::CanonicalSidebarEffect::JumpLatestUnread {
                                client_pid,
                                ..
                            },
                            Some(_),
                        ) => {
                            state.clear_peek(*client_pid);
                            reconcile_after_success = true;
                        }
                        _ => {}
                    }
                }
            }
            if reconcile_after_success {
                let _ = coordinator.enqueue_internal(V2InternalMutation::ReconcileViews);
            }
            let original_response = match (&completion.effect, &completion.result) {
                (
                    super::runtime::CanonicalSidebarEffect::PeekPane { .. },
                    SidebarEffectResult::Succeeded(pane_instance),
                ) => ServerMessage::SidebarPeekResult {
                    event_id: completion.event_id.clone(),
                    accepted_seq: completion.original_accepted_seq,
                    snapshot_revision: completion.snapshot_revision,
                    pane_instance: pane_instance.clone(),
                },
                (
                    super::runtime::CanonicalSidebarEffect::ReadPeekAdvance {
                        read_outcome, ..
                    },
                    result,
                ) => ServerMessage::SidebarReadPeekResult {
                    event_id: completion.event_id.clone(),
                    accepted_seq: completion.original_accepted_seq,
                    snapshot_revision: completion.snapshot_revision,
                    read_outcome: *read_outcome,
                    advance_outcome: read_peek_advance_outcome(result),
                },
                (_, SidebarEffectResult::Succeeded(_)) => ServerMessage::SnapshotAck {
                    event_id: completion.event_id.clone(),
                    accepted_seq: completion.original_accepted_seq,
                    snapshot_revision: completion.snapshot_revision,
                },
                (_, SidebarEffectResult::PaneInstanceMismatch) => ServerMessage::error(
                    ErrorCode::StaleSelection,
                    "sidebar pane selection became stale before tmux mutation",
                    Some(completion.event_id.clone()),
                ),
                (_, SidebarEffectResult::NoAvailablePane) => ServerMessage::error(
                    ErrorCode::StaleSelection,
                    "no unread pane remained available before tmux mutation",
                    Some(completion.event_id.clone()),
                ),
                (_, SidebarEffectResult::SourceClientMismatch) => ServerMessage::error(
                    ErrorCode::StaleSelection,
                    "source sidebar focus changed before tmux mutation",
                    Some(completion.event_id.clone()),
                ),
                (_, SidebarEffectResult::ServerIncarnationMismatch) => ServerMessage::error(
                    ErrorCode::InternalError,
                    "tmux server incarnation changed during sidebar command",
                    Some(completion.event_id.clone()),
                ),
                (_, SidebarEffectResult::Failed(message)) => {
                    eprintln!("[vde-tmux] sidebar tmux command failed: {message}");
                    ServerMessage::error(
                        ErrorCode::InternalError,
                        message,
                        Some(completion.event_id.clone()),
                    )
                }
            };
            coordinator.finish_deferred_response(completion.original_accepted_seq);
            coordinator.complete(completion.original_accepted_seq, original_response);
            if fail_stop {
                coordinator.fail_stop("tmux server incarnation changed during sidebar command");
            }
            let snapshot_revision = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned")
                .as_ref()
                .map_or(0, |state| state.leased.runtime.snapshot_revision());
            ServerMessage::SnapshotAck {
                event_id: completion.event_id,
                accepted_seq,
                snapshot_revision,
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::GitProjection {
            badges,
            worktrees,
            repo_identities,
        }) => {
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized before git projection");
            if let Err(error) =
                state.replace_git_projection_with_identities(badges, worktrees, repo_identities)
            {
                return production_store_error_response(coordinator, error, None);
            }
            ServerMessage::SnapshotAck {
                event_id: EventId::generate()
                    .expect("OS random source failed after daemon startup"),
                accepted_seq,
                snapshot_revision: state.leased.runtime.snapshot_revision(),
            }
        }
    };
    if let ServerMessage::Error {
        code: ErrorCode::InternalError,
        message,
        ..
    } = &response
        && coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_ref()
            .is_some_and(|state| state.leased.runtime.is_fail_stopped())
    {
        coordinator.fail_stop(message.clone());
    }
    response
}

fn task_summary_failure_code(error: &str) -> &'static str {
    if error.contains("timed out") || error.contains("timeout") {
        "timeout"
    } else if error.contains("failed to start") || error.contains("No such file") {
        "process_start"
    } else if error.contains("queue") {
        "queue_full"
    } else if error.contains("invalid") || error.contains("exceeded") || error.contains("empty") {
        "invalid_output"
    } else {
        "process_failed"
    }
}

fn unique_eligible_client_pid(
    views: &crate::daemon::view_hooks::CurrentClientViews,
    source_pane: &PaneInstance,
) -> std::result::Result<u32, usize> {
    let clients = views
        .clients()
        .values()
        .filter(|witness| witness.is_eligible() && &witness.active_pane == source_pane)
        .map(|witness| witness.client_pid)
        .collect::<BTreeSet<_>>();
    if clients.len() == 1 {
        Ok(*clients.iter().next().expect("one client was verified"))
    } else {
        Err(clients.len())
    }
}

fn eligible_witness_matches(
    witnesses: &[crate::pane_state::ClientWitness],
    client_pid: u32,
    source_pane: &PaneInstance,
) -> bool {
    witnesses.iter().any(|witness| {
        witness.client_pid == client_pid
            && witness.is_eligible()
            && &witness.active_pane == source_pane
    })
}

fn read_peek_advance_outcome(
    result: &SidebarEffectResult,
) -> crate::daemon::protocol::v2::PeekAdvanceOutcome {
    match result {
        SidebarEffectResult::Succeeded(pane_instance) => {
            crate::daemon::protocol::v2::PeekAdvanceOutcome::Jumped {
                pane_instance: pane_instance.clone(),
            }
        }
        SidebarEffectResult::NoAvailablePane => {
            crate::daemon::protocol::v2::PeekAdvanceOutcome::Stayed
        }
        _ => crate::daemon::protocol::v2::PeekAdvanceOutcome::Failed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadPeekCommitResult {
    revision: u64,
    read_outcome: crate::daemon::protocol::v2::PaneApplyOutcome,
    candidates: Vec<PaneInstance>,
}

#[allow(clippy::too_many_arguments)]
fn commit_read_peek_state(
    state: &mut super::runtime::CanonicalCoordinatorState,
    io: &mut dyn crate::pane_state::snapshot::PaneSnapshotStoreIo,
    daemon_instance_id: &DaemonInstanceId,
    event_id: &EventId,
    target: &PaneInstance,
    client_pid: u32,
    advance_candidates: Vec<PaneInstance>,
    accepted_seq: u64,
) -> Result<ReadPeekCommitResult, crate::pane_state::store::StoreError> {
    let through_order = state
        .leased
        .runtime
        .record(target)
        .and_then(|pane| pane.unread.latest_unread())
        .map(|occurrence| occurrence.order);
    let read_outcome = if let Some(through_order) = through_order {
        let envelope = PaneEventEnvelope {
            daemon_instance_id: daemon_instance_id.clone(),
            event_id: event_id.clone(),
            pane_instance: target.clone(),
            agent: None,
            agent_session_id: None,
            event: PaneEvent::MarkPaneRead { through_order },
        };
        let result = state.leased.runtime.apply_pane_reads(io, &[envelope])?;
        if result.committed > 0 {
            crate::daemon::protocol::v2::PaneApplyOutcome::Committed
        } else {
            crate::daemon::protocol::v2::PaneApplyOutcome::Noop
        }
    } else {
        crate::daemon::protocol::v2::PaneApplyOutcome::Noop
    };

    state.clear_peeks_for_read_panes_except(&BTreeSet::from([target.clone()]), Some(client_pid));
    let mut seen = BTreeSet::new();
    let candidates = advance_candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.clone()))
        .filter(|candidate| {
            state.contains_pane(candidate)
                && state
                    .leased
                    .runtime
                    .record(candidate)
                    .is_some_and(|pane| pane.unread.is_unread())
        })
        .collect::<Vec<_>>();
    let revision = state.leased.runtime.snapshot_revision();
    if !candidates.is_empty() {
        let began = state.begin_peek(
            client_pid,
            target.clone(),
            candidates.iter().cloned(),
            accepted_seq,
        );
        debug_assert!(began, "read-current starts from an active lease");
    }
    Ok(ReadPeekCommitResult {
        revision,
        read_outcome,
        candidates,
    })
}

fn apply_sidebar_preference_intent(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    intent: crate::sidebar::state::SidebarPreferenceIntent,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before sidebar preference intent");
    if !state.sidebar_intent_dedupe.accept(event_id.clone()) {
        return ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
    }
    let snapshot = state.resolved_snapshot();
    let projection = crate::sidebar::tree::project_sidebar(
        &state.projection_config,
        &snapshot.panes,
        &snapshot.sidebar_model,
        &snapshot.events,
        &crate::sidebar::state::SidebarState {
            category_scope: crate::sidebar::state::CategoryScope::All,
            ..crate::sidebar::state::SidebarState::default()
        },
        crate::sidebar::tree::now_epoch_secs(),
    );
    let known_rows = projection
        .rows
        .into_iter()
        .map(|row| row.id)
        .collect::<BTreeSet<_>>();
    let mut candidate = state.sidebar_preferences.clone();
    if !candidate.apply_intent(&intent, &known_rows) {
        return ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
    }
    let path =
        crate::sidebar::store::state_path(&coordinator.env, &coordinator.incarnation.socket_path);
    if let Err(error) = crate::sidebar::store::save_state(&path, &candidate) {
        let message = format!("sidebar preference persistence failed: {error:#}");
        coordinator.log_daemon_error(&message);
        let _ = state.add_global_diagnostic(ErrorCode::PersistFailed, message.clone());
        return ServerMessage::error(ErrorCode::PersistFailed, message, Some(event_id));
    }
    if let Err(error) = state.replace_sidebar_preferences(candidate) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::SnapshotAck {
        event_id,
        accepted_seq,
        snapshot_revision: state.leased.runtime.snapshot_revision(),
    }
}

fn persist_pruned_sidebar_pins(
    coordinator: &ProductionV2Coordinator,
    state: &mut super::runtime::CanonicalCoordinatorState,
) -> Result<bool, crate::pane_state::store::StoreError> {
    let present = state
        .topology
        .panes
        .iter()
        .map(|pane| pane.pane_instance.clone())
        .collect::<BTreeSet<_>>();
    let mut candidate = state.sidebar_preferences.clone();
    if !candidate.retain_panes(&present) {
        return Ok(false);
    }
    let path =
        crate::sidebar::store::state_path(&coordinator.env, &coordinator.incarnation.socket_path);
    crate::sidebar::store::save_state(&path, &candidate).map_err(|error| {
        crate::pane_state::store::StoreError::PersistFailed(format!(
            "sidebar pin cleanup persistence failed: {error:#}"
        ))
    })?;
    state.replace_sidebar_preferences(candidate)
}

fn apply_category_intent(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    intent: crate::category::CategoryIntent,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before category intent");
    if !state.sidebar_intent_dedupe.accept(event_id.clone()) {
        return ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
    }
    let model = state.effective_category_model();
    let mut candidate = state.category_state.clone();
    let changed = match candidate.apply_intent(&state.projection_config, &intent, &model) {
        Ok(changed) => changed,
        Err(message) => {
            return ServerMessage::error(ErrorCode::InvalidRequest, message, Some(event_id));
        }
    };
    if !changed {
        return ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
    }
    let path =
        crate::category::store::state_path(&coordinator.env, &coordinator.incarnation.socket_path);
    if let Err(error) = crate::category::store::save_state(&path, &candidate) {
        let message = format!("category state persistence failed: {error:#}");
        coordinator.log_daemon_error(&message);
        let _ = state.add_global_diagnostic(ErrorCode::PersistFailed, message.clone());
        return ServerMessage::error(ErrorCode::PersistFailed, message, Some(event_id));
    }
    if let Err(error) = state.replace_category_state(candidate) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    let model = state.effective_category_model();
    let mirrors = state
        .status_metadata
        .sessions
        .values()
        .map(|session| {
            let category = state
                .repo_identities
                .get(&session.project_path)
                .and_then(|identity| model.placements.get(&identity.key))
                .map(|placement| placement.category.to_string())
                .unwrap_or_else(|| crate::category::UNCATEGORIZED.to_string());
            (session.session_name.clone(), category)
        })
        .collect::<Vec<_>>();
    let snapshot_revision = state.leased.runtime.snapshot_revision();
    drop(state_guard);
    let runner = coordinator.status_push_runner(Duration::from_secs(1));
    for (session_name, category) in mirrors {
        if let Err(error) = crate::options::set_session_option(
            &runner,
            &session_name,
            crate::options::KEY_CATEGORY,
            &category,
        ) {
            coordinator.log_daemon_error(&format!(
                "failed to update category mirror for {session_name}: {error:#}"
            ));
        }
    }
    ServerMessage::SnapshotAck {
        event_id,
        accepted_seq,
        snapshot_revision,
    }
}

fn apply_sidebar_navigation(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    selection: Option<String>,
    scroll: usize,
    manual_scroll: bool,
) -> ServerMessage {
    use crate::daemon::protocol::v2::ServerMessage;
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before sidebar navigation");
    if !state.sidebar_intent_dedupe.accept(event_id.clone()) {
        return ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
    }
    if let Err(error) = state.replace_sidebar_navigation(selection, scroll, manual_scroll) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::SnapshotAck {
        event_id,
        accepted_seq,
        snapshot_revision: state.leased.runtime.snapshot_revision(),
    }
}

fn apply_external_pane_event(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: PaneEventEnvelope,
) -> ServerMessage {
    apply_pane_event_mutation(coordinator, accepted_seq, envelope, false, None)
}

fn apply_external_provider_event(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: PaneEventEnvelope,
    observation: crate::hook::provider::ProviderObservation,
) -> ServerMessage {
    let runner = coordinator.status_push_runner(Duration::from_secs(1));
    apply_external_provider_event_with_runner(
        coordinator,
        accepted_seq,
        envelope,
        observation,
        &runner,
    )
}

fn apply_external_provider_event_with_runner(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    mut envelope: PaneEventEnvelope,
    mut observation: crate::hook::provider::ProviderObservation,
    process_runner: &dyn crate::tmux::TmuxRunner,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    use crate::hook::provider::ProviderHookKind;

    if observation.provider.as_str() != "codex" {
        return ServerMessage::error(
            ErrorCode::UnsupportedProvider,
            "durable provider observations are enabled only for the authenticated Codex adapter",
            Some(envelope.event_id),
        );
    }
    if envelope.agent.as_ref() != Some(&observation.provider)
        || envelope.agent_session_id.as_ref() != Some(&observation.session_id)
    {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "provider observation identity does not match the pane event envelope",
            Some(envelope.event_id),
        );
    }
    if !provider_event_matches_pane_event(&observation, &envelope.event) {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "provider hook kind does not match the pane event transition",
            Some(envelope.event_id),
        );
    }

    let received_at = epoch_seconds();
    observation.observed_at = received_at;
    normalize_provider_pane_event(&mut envelope.event, received_at);

    if observation.hook_kind == ProviderHookKind::SessionStart {
        // A resumed prompt cannot be attributed to a durable Run, so keep it
        // outside the public Pane snapshot to preserve guarded-dispatch privacy.
        redact_private_provider_prompt(&mut envelope.event, true);
        return apply_external_pane_event(coordinator, accepted_seq, envelope);
    }

    let duplicate = {
        let runtime_guard = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let Some(runtime) = runtime_guard.as_ref() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "agent runtime is hydrating",
                Some(envelope.event_id),
            );
        };
        match runtime.provider_event_run(&observation) {
            Ok(value) => value,
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::PersistFailed,
                    error.to_string(),
                    Some(envelope.event_id),
                );
            }
        }
    };

    let (binding, run_seq) = if let Some(run) = duplicate {
        (run.binding, run.run_seq)
    } else {
        let record = match provider_binding_record(coordinator, &envelope, &observation) {
            Ok(record) => record,
            Err(response) => return response,
        };
        let record = if record.agent_process.is_none() {
            match refresh_provider_process_identity(
                coordinator,
                accepted_seq,
                &envelope,
                &observation,
                process_runner,
            ) {
                Ok(record) => record,
                Err(response) => return response,
            }
        } else {
            record
        };
        let Some(process) = record.agent_process.clone() else {
            return ServerMessage::error(
                ErrorCode::StaleAgentEvent,
                "provider event has no exact agent process identity after a fresh process scan",
                Some(envelope.event_id),
            );
        };
        let run_seq = if observation.hook_kind == ProviderHookKind::UserPromptSubmit {
            match record.run_seq.checked_add(1) {
                Some(value) => value,
                None => {
                    return ServerMessage::error(
                        ErrorCode::StateInvariantViolation,
                        "agent run sequence overflow",
                        Some(envelope.event_id),
                    );
                }
            }
        } else {
            record.run_seq
        };
        (
            crate::agent_state::AgentBinding {
                server_identity: coordinator.incarnation.identity.clone(),
                pane_instance: record.pane_instance,
                pane_state_id: record.state_id,
                agent_epoch: record.agent_epoch,
                agent_kind: record.agent,
                provider_session_id: observation.session_id.clone(),
                process,
            },
            run_seq,
        )
    };

    let apply_result = coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned")
        .as_mut()
        .expect("agent runtime checked above")
        .apply_provider_observation(binding, run_seq, &observation);
    let apply_result = match apply_result {
        Ok(result) => result,
        Err(error) => {
            if matches!(error, crate::agent_state::StoreError::NotFound(_)) {
                let message = format!("provider_attribution_unresolved: {error}");
                let snapshot_revision = {
                    let mut state = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned");
                    let state = state
                        .as_mut()
                        .expect("state initialized before provider event");
                    match state.add_global_diagnostic(ErrorCode::StaleAgentEvent, message) {
                        Ok(revision) => revision,
                        Err(store_error) => {
                            return production_store_error_response(
                                coordinator,
                                store_error,
                                Some(envelope.event_id),
                            );
                        }
                    }
                };
                return ServerMessage::SnapshotAck {
                    event_id: envelope.event_id,
                    accepted_seq,
                    snapshot_revision,
                };
            }
            return agent_state_query_error_with_event(error, Some(envelope.event_id));
        }
    };

    // Provider adapters already reduce human-entered prompts and responses to
    // bounded, single-line UI previews. Keep those previews for the sidebar,
    // but never project the prompt of a guarded dispatch into PaneState.
    let private_prompt = apply_result.run.as_ref().is_some_and(|run| {
        run.operation_id.is_some()
            && apply_result.operation.as_ref().is_none_or(|operation| {
                observation.prompt_digest.as_deref() == Some(operation.prompt_digest.as_str())
            })
    });
    redact_private_provider_prompt(&mut envelope.event, private_prompt);

    if apply_result.disposition == crate::agent_state::reducer::ApplyDisposition::Duplicate
        && let Some(run) = apply_result.run.as_ref()
    {
        let projection_check = {
            let state = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let Some(state) = state.as_ref() else {
                return ServerMessage::error(
                    ErrorCode::NotReady,
                    "daemon is hydrating",
                    Some(envelope.event_id),
                );
            };
            let pane = state.leased.runtime.record(&envelope.pane_instance);
            pane.map_or(Ok(false), |pane| {
                pane_needs_durable_run_projection(pane, run)
            })
            .map(|needed| {
                (
                    needed,
                    pane.map(crate::pane_state::PaneState::version),
                    state.leased.runtime.snapshot_revision(),
                )
            })
        };
        let (projection_is_current, state_version, snapshot_revision) = match projection_check {
            Ok(result) => result,
            Err(message) => {
                coordinator.fail_stop(message.clone());
                return ServerMessage::error(
                    ErrorCode::StateInvariantViolation,
                    message,
                    Some(envelope.event_id),
                );
            }
        };
        if !projection_is_current {
            return ServerMessage::PaneEventResult {
                event_id: envelope.event_id,
                accepted_seq,
                state_version,
                snapshot_revision,
                outcome: crate::daemon::protocol::v2::PaneApplyOutcome::Noop,
            };
        }
    }

    if apply_result.disposition == crate::agent_state::reducer::ApplyDisposition::EvidenceOnly
        && let Some(run) = apply_result.run.as_ref()
    {
        return project_provider_run_evidence_only(
            coordinator,
            accepted_seq,
            envelope.event_id,
            &envelope.pane_instance,
            run,
        );
    }

    apply_pane_event_mutation(coordinator, accepted_seq, envelope, false, apply_result.run)
}

fn project_provider_run_evidence_only(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    pane_instance: &PaneInstance,
    run: &crate::agent_state::RunRecord,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{PaneApplyOutcome, ServerMessage};
    use crate::pane_state::reducer::ReductionOutcome;

    let result = (|| -> Result<
        crate::pane_state::store::ApplyResult,
        crate::pane_state::store::StoreError,
    > {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard.as_mut().ok_or_else(|| {
            crate::pane_state::store::StoreError::PersistFailed("daemon is hydrating".to_string())
        })?;
        let revision_before = state.leased.runtime.snapshot_revision();
        let mut io = pane_snapshot_store(coordinator);
        let changed = if state
            .leased
            .runtime
            .record(pane_instance)
            .is_some_and(|pane| pane_belongs_to_run_epoch(pane, run))
        {
            state.leased.runtime.project_current_run(
                &mut io,
                pane_instance,
                crate::pane_state::CurrentDurableRunProjection {
                    run_id: run.run_id.as_str().to_string(),
                    run_seq: run.run_seq,
                    run_revision: run.revision,
                },
                run.execution_active(),
                run.updated_at,
            )?
        } else {
            false
        };
        let result = crate::pane_state::store::ApplyResult {
            outcome: if changed {
                ReductionOutcome::CanonicalChanged
            } else {
                ReductionOutcome::Noop
            },
            state_version: state
                .leased
                .runtime
                .record(pane_instance)
                .map(crate::pane_state::PaneState::version),
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        };
        finish_pane_event_projection(
            coordinator,
            state,
            pane_instance,
            None,
            revision_before,
            result,
            false,
        )
    })();

    match result {
        Ok(result) => ServerMessage::PaneEventResult {
            event_id,
            accepted_seq,
            state_version: result.state_version,
            snapshot_revision: result.snapshot_revision,
            outcome: if result.outcome == ReductionOutcome::CanonicalChanged {
                PaneApplyOutcome::Committed
            } else {
                PaneApplyOutcome::Noop
            },
        },
        Err(error) => production_store_error_response(coordinator, error, Some(event_id)),
    }
}

#[allow(clippy::result_large_err)]
fn provider_binding_record(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
    observation: &crate::hook::provider::ProviderObservation,
) -> std::result::Result<crate::pane_state::PaneState, ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let record = {
        let state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let Some(state) = state_guard.as_ref() else {
            return Err(ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is hydrating",
                Some(envelope.event_id.clone()),
            ));
        };
        state
            .leased
            .runtime
            .record(&envelope.pane_instance)
            .cloned()
    };
    let Some(record) = record else {
        return Err(ServerMessage::error(
            ErrorCode::PaneNotFound,
            "provider event has no canonical pane state",
            Some(envelope.event_id.clone()),
        ));
    };
    let provider_session_matches = match record.agent_session_id.as_ref() {
        Some(session) => session == &observation.session_id,
        None => observation.hook_kind == crate::hook::provider::ProviderHookKind::UserPromptSubmit,
    };
    if record.agent != observation.provider || !provider_session_matches || !record.agent_present {
        return Err(ServerMessage::error(
            ErrorCode::StaleAgentEvent,
            "provider event no longer matches the live Agent Binding",
            Some(envelope.event_id.clone()),
        ));
    }
    Ok(record)
}

#[allow(clippy::result_large_err)]
fn refresh_provider_process_identity(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: &PaneEventEnvelope,
    observation: &crate::hook::provider::ProviderObservation,
    runner: &dyn crate::tmux::TmuxRunner,
) -> std::result::Result<crate::pane_state::PaneState, ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    const RESOLVE_ATTEMPTS: usize = 4;
    const RETRY_DELAY: Duration = Duration::from_millis(20);

    let mut last_error = None;
    let mut process = None;
    for attempt in 0..RESOLVE_ATTEMPTS {
        match runner.resolve_agent_process(envelope.pane_instance.pane_pid, &observation.provider) {
            Ok(Some(resolved)) => {
                process = Some(resolved);
                break;
            }
            Ok(None) => last_error = None,
            Err(error) => last_error = Some(error.to_string()),
        }
        if attempt + 1 < RESOLVE_ATTEMPTS {
            thread::sleep(RETRY_DELAY);
        }
    }
    let Some(process) = process else {
        let message = last_error.map_or_else(
            || {
                format!(
                    "fresh pane process scans found no exact provider process identity after {RESOLVE_ATTEMPTS} attempts"
                )
            },
            |error| {
                format!(
                    "fresh pane process scans could not verify provider identity after {RESOLVE_ATTEMPTS} attempts: {error}"
                )
            },
        );
        return Err(ServerMessage::error(
            ErrorCode::StaleAgentEvent,
            message,
            Some(envelope.event_id.clone()),
        ));
    };
    let dispatch = {
        let state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let Some(state) = state_guard.as_ref() else {
            return Err(ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is hydrating",
                Some(envelope.event_id.clone()),
            ));
        };
        state
            .leased
            .runtime
            .freeze_observation_dispatch([envelope.pane_instance.clone()])
            .into_iter()
            .next()
            .expect("one requested pane produces one observation dispatch snapshot")
    };
    let daemon_instance_id = coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .daemon_instance_id()
        .clone();
    let process_envelope = crate::daemon::workers::observation_envelope(
        daemon_instance_id,
        envelope.pane_instance.clone(),
        dispatch.base,
        &dispatch.tracker,
        crate::daemon::workers::ObservationSample {
            observed_at: epoch_seconds(),
            presence: crate::pane_state::AgentPresenceObservation::Present(
                observation.provider.clone(),
            ),
            capture: None,
            process: Some(crate::pane_state::ProcessObservation {
                agent_process_checked: true,
                agent_process: Some(process.clone()),
                background_process_alive: None,
                listening_ports: None,
            }),
        },
    )
    .map_err(|error| {
        ServerMessage::error(
            ErrorCode::InternalError,
            format!("could not build provider process observation: {error:#}"),
            Some(envelope.event_id.clone()),
        )
    })?;
    match apply_pane_event_mutation(coordinator, accepted_seq, process_envelope, false, None) {
        ServerMessage::PaneEventResult { .. } => {}
        ServerMessage::Error { code, message, .. } => {
            return Err(ServerMessage::error(
                code,
                message,
                Some(envelope.event_id.clone()),
            ));
        }
        response => {
            return Err(ServerMessage::error(
                ErrorCode::InternalError,
                format!("unexpected provider process refresh response: {response:?}"),
                Some(envelope.event_id.clone()),
            ));
        }
    }
    let record = provider_binding_record(coordinator, envelope, observation)?;
    if record.agent_process.as_ref() != Some(&process) || !record.scan_verified {
        return Err(ServerMessage::error(
            ErrorCode::StaleAgentEvent,
            "fresh pane process identity did not become the live Agent Binding",
            Some(envelope.event_id.clone()),
        ));
    }
    Ok(record)
}

fn normalize_provider_pane_event(event: &mut PaneEvent, observed_at: i64) {
    match event {
        PaneEvent::AgentSessionStarted {
            observed_at: event_at,
            ..
        }
        | PaneEvent::ActivityObserved {
            observed_at: event_at,
        }
        | PaneEvent::ActivityAndProgressObserved {
            observed_at: event_at,
            ..
        }
        | PaneEvent::WaitRequested {
            observed_at: event_at,
            ..
        }
        | PaneEvent::FailRun {
            observed_at: event_at,
            ..
        }
        | PaneEvent::ProgressUpdated {
            observed_at: event_at,
            ..
        } => *event_at = observed_at,
        PaneEvent::BeginRun { started_at, .. } => *started_at = observed_at,
        PaneEvent::CompleteRun { completed_at }
        | PaneEvent::ResponseAndCompleteRun { completed_at, .. }
        | PaneEvent::MarkDone { completed_at, .. } => *completed_at = observed_at,
        PaneEvent::ExplicitStateReported { report } => report.observed_at = observed_at,
        PaneEvent::ObservationBatch { .. }
        | PaneEvent::MarkPaneRead { .. }
        | PaneEvent::TaskSummaryGenerated { .. }
        | PaneEvent::PaneRemoved { .. } => {}
    }
}

fn redact_private_provider_prompt(event: &mut PaneEvent, private_prompt: bool) {
    if !private_prompt {
        return;
    }
    match event {
        PaneEvent::AgentSessionStarted { resumed_prompt, .. } => *resumed_prompt = None,
        PaneEvent::BeginRun { prompt, .. } => *prompt = None,
        PaneEvent::ActivityAndProgressObserved { operations, .. }
        | PaneEvent::ProgressUpdated { operations, .. } => {
            operations.retain(|operation| {
                !matches!(
                    operation,
                    crate::pane_state::ProgressOperation::SetPrompt(_)
                )
            });
        }
        PaneEvent::ExplicitStateReported { report } => report.prompt = None,
        _ => {}
    }
}

fn provider_event_matches_pane_event(
    observation: &crate::hook::provider::ProviderObservation,
    event: &PaneEvent,
) -> bool {
    use crate::hook::provider::ProviderHookKind;

    match (observation.hook_kind, event) {
        (ProviderHookKind::SessionStart, PaneEvent::AgentSessionStarted { .. }) => true,
        (
            ProviderHookKind::UserPromptSubmit,
            PaneEvent::BeginRun {
                prompt:
                    Some(crate::pane_state::PromptState {
                        digest: Some(digest),
                        ..
                    }),
                ..
            },
        ) => observation.prompt_digest.as_deref() == Some(digest),
        (
            ProviderHookKind::Activity,
            PaneEvent::ActivityObserved { .. } | PaneEvent::ActivityAndProgressObserved { .. },
        )
        | (ProviderHookKind::Waiting, PaneEvent::WaitRequested { .. }) => true,
        (ProviderHookKind::Stop, PaneEvent::ResponseAndCompleteRun { .. }) => {
            observation.response.is_some()
        }
        (ProviderHookKind::Stop, PaneEvent::CompleteRun { .. }) => observation.response.is_none(),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_start_agent_prompt(
    coordinator: &ProductionV2Coordinator,
    event_id: EventId,
    target_agent_ref: String,
    operation_id: crate::agent_state::OperationId,
    prompt_base64: String,
    prompt_digest: crate::agent_state::Sha256Digest,
    dispatch_option: String,
    observed_at: i64,
) -> ServerMessage {
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    apply_start_agent_prompt_with_runner(
        coordinator,
        &runner,
        event_id,
        target_agent_ref,
        operation_id,
        prompt_base64,
        prompt_digest,
        dispatch_option,
        observed_at,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_start_agent_prompt_with_runner(
    coordinator: &ProductionV2Coordinator,
    runner: &dyn crate::tmux::TmuxRunner,
    event_id: EventId,
    target_agent_ref: String,
    operation_id: crate::agent_state::OperationId,
    prompt_base64: String,
    prompt_digest: crate::agent_state::Sha256Digest,
    dispatch_option: String,
    observed_at: i64,
) -> ServerMessage {
    use crate::agent_state::runtime::PrepareOperationResult;
    use crate::daemon::agent_dispatch::DispatchOutcome;
    use crate::daemon::protocol::v2::{ErrorCode, PROTOCOL_VERSION, ServerMessage};

    let operation_result = |runtime: &crate::agent_state::runtime::AgentRuntime,
                            operation: crate::agent_state::OperationRecord|
     -> ServerMessage {
        let reference = runtime.operation_ref(operation.operation_id.clone());
        match reference.encode() {
            Ok(operation_ref) => ServerMessage::AgentPromptResult {
                proto: PROTOCOL_VERSION,
                operation_ref,
                operation,
            },
            Err(error) => ServerMessage::error(
                ErrorCode::InternalError,
                error.to_string(),
                Some(event_id.clone()),
            ),
        }
    };

    if observed_at < 0 || dispatch_option != "paste_enter" {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "agent prompt requires a non-negative timestamp and dispatch_option=paste_enter",
            Some(event_id),
        );
    }
    let observed_at = epoch_seconds();
    let prompt = match base64::engine::general_purpose::STANDARD.decode(&prompt_base64) {
        Ok(prompt)
            if !prompt.is_empty()
                && prompt.len() <= crate::agent_state::PROMPT_BODY_MAX_BYTES
                && !prompt.contains(&0)
                && std::str::from_utf8(&prompt).is_ok() =>
        {
            prompt
        }
        _ => {
            return ServerMessage::error(
                ErrorCode::InvalidRequest,
                "agent prompt must be non-empty UTF-8 without NUL and at most 65,536 bytes",
                Some(event_id),
            );
        }
    };
    let decoded_prompt = std::str::from_utf8(&prompt).expect("prompt UTF-8 checked above");
    let observed_digest = crate::agent_state::Sha256Digest::parse(
        crate::pane_state::PromptState::digest_decoded_prompt(decoded_prompt),
    )
    .expect("PromptState emits a valid SHA-256 digest");
    if observed_digest != prompt_digest {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "agent prompt body does not match prompt_digest",
            Some(event_id),
        );
    }

    let request_fingerprint = crate::agent_state::runtime::AgentRuntime::request_fingerprint(
        &target_agent_ref,
        &prompt_digest,
        &dispatch_option,
    );
    let existing_prepared = {
        let runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let Some(runtime) = runtime.as_ref() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "agent runtime is hydrating",
                Some(event_id),
            );
        };
        match runtime.lookup_operation_request(&operation_id, &request_fingerprint) {
            Ok(Some(existing))
                if existing.dispatch_state == crate::agent_state::DispatchState::Prepared =>
            {
                Some(existing)
            }
            Ok(Some(existing)) => return operation_result(runtime, existing),
            Ok(None) => None,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    };

    if let Some(existing) = existing_prepared.as_ref() {
        let expired = {
            let mut runtime = coordinator
                .agent_runtime
                .lock()
                .expect("agent runtime lock poisoned");
            let runtime = runtime.as_mut().expect("agent runtime checked above");
            runtime.reject_prepared_retry_if_expired(&existing.operation_id, observed_at)
        };
        match expired {
            Ok(Some(operation)) => {
                let runtime = coordinator
                    .agent_runtime
                    .lock()
                    .expect("agent runtime lock poisoned");
                let runtime = runtime.as_ref().expect("agent runtime checked above");
                return operation_result(runtime, operation);
            }
            Ok(None) => {}
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    }

    if coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .hook_health()
        != HookHealth::Healthy
    {
        return ServerMessage::error(
            ErrorCode::HookCollision,
            "hook health is degraded; prompt was not staged or sent",
            Some(event_id),
        );
    }

    let (binding, expected_run_seq, pane, expected_pane_version, expected_current_run) =
        match resolve_agent_prompt_target(coordinator, &target_agent_ref) {
            Ok(value) => value,
            Err(message) => {
                if let Some(rejection_code) =
                    prepared_target_rejection_code(existing_prepared.is_some(), None)
                {
                    let mut runtime = coordinator
                        .agent_runtime
                        .lock()
                        .expect("agent runtime lock poisoned");
                    let runtime = runtime.as_mut().expect("agent runtime checked above");
                    return match runtime.settle_dispatch(
                        &operation_id,
                        crate::agent_state::DispatchState::Rejected,
                        rejection_code,
                        observed_at,
                    ) {
                        Ok(operation) => operation_result(runtime, operation),
                        Err(error) => agent_state_query_error_with_event(error, Some(event_id)),
                    };
                }
                let code = if message.starts_with("unsupported provider:") {
                    ErrorCode::UnsupportedProvider
                } else {
                    ErrorCode::StaleAgentEvent
                };
                return ServerMessage::error(code, message, Some(event_id));
            }
        };
    let operation = if let Some(existing) = existing_prepared {
        let target_matches = prepared_operation_matches_target(
            &existing,
            &binding,
            expected_pane_version,
            expected_current_run.as_ref(),
            expected_run_seq,
        );
        if let Some(rejection_code) = prepared_target_rejection_code(true, Some(target_matches)) {
            let mut runtime = coordinator
                .agent_runtime
                .lock()
                .expect("agent runtime lock poisoned");
            let runtime = runtime.as_mut().expect("agent runtime checked above");
            return match runtime.settle_dispatch(
                &operation_id,
                crate::agent_state::DispatchState::Rejected,
                rejection_code,
                observed_at,
            ) {
                Ok(operation) => operation_result(runtime, operation),
                Err(error) => agent_state_query_error_with_event(error, Some(event_id)),
            };
        }
        existing
    } else {
        let mut runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let runtime = runtime.as_mut().expect("agent runtime checked above");
        match runtime.prepare_operation(
            operation_id.clone(),
            target_agent_ref,
            &prompt,
            prompt_digest,
            dispatch_option,
            binding.clone(),
            expected_pane_version,
            expected_current_run,
            expected_run_seq,
            observed_at,
        ) {
            Ok(PrepareOperationResult::Existing(existing)) => {
                return operation_result(runtime, existing);
            }
            Ok(PrepareOperationResult::Created(created)) => created,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    };
    maybe_crash_agent_operation("after_prepared", &operation.operation_id);

    let reject_pre_dispatch = |code: &str, message: String| -> ServerMessage {
        let mut runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let runtime = runtime.as_mut().expect("agent runtime initialized");
        match runtime.settle_dispatch(
            &operation.operation_id,
            crate::agent_state::DispatchState::Rejected,
            code,
            observed_at,
        ) {
            Ok(operation) => operation_result(runtime, operation),
            Err(error) => ServerMessage::error(
                ErrorCode::PersistFailed,
                format!("{message}; failed to persist rejection: {error}"),
                Some(event_id.clone()),
            ),
        }
    };

    let dispatch_lock =
        match acquire_agent_prompt_dispatch_lock(&coordinator.incarnation.identity, &pane) {
            Ok(lock) => lock,
            Err(rejection) => return reject_pre_dispatch(rejection.code, rejection.message),
        };

    if let Err(rejection) = verify_agent_prompt_process_and_owner(runner, &pane, &binding) {
        return reject_pre_dispatch(rejection.code, rejection.message);
    }
    if let Err(message) = verify_agent_prompt_precondition(coordinator, &operation) {
        return reject_pre_dispatch("pane_precondition_changed", message);
    }
    let staged = {
        let mut runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let runtime = runtime.as_mut().expect("agent runtime initialized");
        if let Err(error) = runtime.mark_dispatch_started(&operation_id, observed_at) {
            return agent_state_query_error_with_event(error, Some(event_id));
        }
        match runtime.store().read_prompt(&operation_id) {
            Ok(prompt) => prompt,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    };
    let dispatch = crate::daemon::agent_dispatch::dispatch_prompt_guarded(
        runner,
        &coordinator.incarnation,
        &pane,
        &staged,
        operation_id.as_str(),
    );
    if matches!(dispatch, DispatchOutcome::Submitted) {
        maybe_crash_agent_operation("after_dispatch_submitted", &operation_id);
    }
    drop(dispatch_lock);

    let mut runtime = coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned");
    let runtime = runtime.as_mut().expect("agent runtime initialized");
    let operation = match dispatch {
        DispatchOutcome::Submitted => match runtime.store().load_operation(&operation_id) {
            Ok(Some(operation)) => operation,
            Ok(None) => {
                return ServerMessage::error(
                    ErrorCode::InternalError,
                    "submitted operation disappeared",
                    Some(event_id),
                );
            }
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        },
        DispatchOutcome::Rejected(message) => match runtime.settle_dispatch(
            &operation_id,
            crate::agent_state::DispatchState::Rejected,
            "guarded_dispatch_rejected",
            observed_at,
        ) {
            Ok(operation) => operation,
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::PersistFailed,
                    format!("{message}; failed to persist rejection: {error}"),
                    Some(event_id),
                );
            }
        },
        DispatchOutcome::DeliveryUnknown(message) => match runtime.settle_dispatch(
            &operation_id,
            crate::agent_state::DispatchState::DeliveryUnknown,
            "guarded_dispatch_ambiguous",
            observed_at,
        ) {
            Ok(operation) => operation,
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::PersistFailed,
                    format!("{message}; failed to persist ambiguity: {error}"),
                    Some(event_id),
                );
            }
        },
    };
    operation_result(runtime, operation)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentPromptPreDispatchRejection {
    code: &'static str,
    message: String,
}

fn acquire_agent_prompt_dispatch_lock(
    server_identity: &crate::daemon::topology::ServerIdentity,
    pane: &crate::pane_state::PaneInstance,
) -> std::result::Result<crate::runtime_dir::PaneDispatchLock, AgentPromptPreDispatchRejection> {
    match crate::runtime_dir::try_acquire_pane_dispatch_lock(
        server_identity,
        &pane.pane_id,
        pane.pane_pid,
    ) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(AgentPromptPreDispatchRejection {
            code: "dispatch_lock_busy",
            message: "another guarded dispatch owns this pane".to_string(),
        }),
        Err(error) => Err(AgentPromptPreDispatchRejection {
            code: "dispatch_lock_error",
            message: format!("could not acquire guarded dispatch lock: {error:#}"),
        }),
    }
}

fn verify_agent_prompt_process_and_owner(
    runner: &dyn crate::tmux::TmuxRunner,
    pane: &crate::pane_state::PaneInstance,
    binding: &crate::agent_state::OperationBinding,
) -> std::result::Result<(), AgentPromptPreDispatchRejection> {
    match runner.resolve_agent_process(pane.pane_pid, &binding.agent_kind) {
        Ok(Some(process)) if process == binding.process => {}
        Ok(Some(_)) => {
            return Err(AgentPromptPreDispatchRejection {
                code: "target_process_replaced",
                message: "exact agent process identity changed before guarded dispatch".to_string(),
            });
        }
        Ok(None) => {
            return Err(AgentPromptPreDispatchRejection {
                code: "target_process_absent",
                message: "exact agent process disappeared before guarded dispatch".to_string(),
            });
        }
        Err(error) => {
            return Err(AgentPromptPreDispatchRejection {
                code: "target_process_unverifiable",
                message: format!(
                    "could not re-resolve exact agent process before dispatch: {error:#}"
                ),
            });
        }
    }
    runner
        .verify_agent_input_owner(pane.pane_pid, binding.process.pid)
        .map_err(|error| AgentPromptPreDispatchRejection {
            code: "agent_not_input_owner",
            message: format!("exact agent process is not the foreground input owner: {error:#}"),
        })
}

fn prepared_operation_matches_target(
    existing: &crate::agent_state::OperationRecord,
    binding: &crate::agent_state::OperationBinding,
    expected_pane_version: crate::pane_state::StateVersion,
    expected_current_run: Option<&crate::pane_state::CurrentDurableRunProjection>,
    expected_run_seq: u64,
) -> bool {
    existing.binding == *binding
        && existing.expected_pane_version == expected_pane_version
        && existing.expected_current_run.as_ref() == expected_current_run
        && existing.expected_run_seq == expected_run_seq
}

fn prepared_target_rejection_code(
    has_existing_prepared: bool,
    resolved_target_matches: Option<bool>,
) -> Option<&'static str> {
    match (has_existing_prepared, resolved_target_matches) {
        (true, None) => Some("target_no_longer_current"),
        (true, Some(false)) => Some("binding_changed_before_dispatch"),
        (true, Some(true)) | (false, _) => None,
    }
}

#[cfg(debug_assertions)]
fn maybe_crash_agent_operation(point: &str, operation_id: &crate::agent_state::OperationId) {
    let Some(root) = std::env::var_os("VDE_TMUX_TEST_AGENT_OPERATION_FAULT_DIR") else {
        return;
    };
    let marker = PathBuf::from(root).join(format!("{}.{}", operation_id.as_str(), point));
    if fs::remove_file(marker).is_ok() {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
fn maybe_crash_agent_operation(_point: &str, _operation_id: &crate::agent_state::OperationId) {}

fn resolve_agent_prompt_target(
    coordinator: &ProductionV2Coordinator,
    target_agent_ref: &str,
) -> std::result::Result<
    (
        crate::agent_state::OperationBinding,
        u64,
        crate::pane_state::PaneInstance,
        crate::pane_state::StateVersion,
        Option<crate::pane_state::CurrentDurableRunProjection>,
    ),
    String,
> {
    use sha2::{Digest as _, Sha256};

    let parts = target_agent_ref.split(':').collect::<Vec<_>>();
    if parts.len() != 8 || parts[0] != "vta1" || parts[1] != coordinator.incarnation.hash {
        return Err("invalid or stale exact agent_ref".to_string());
    }
    let pane = crate::pane_state::PaneInstance {
        pane_id: format!("%{}", parts[2]),
        pane_pid: parts[3]
            .parse::<u32>()
            .map_err(|_| "invalid agent_ref pane PID".to_string())?,
    };
    pane.validate().map_err(|error| error.to_string())?;
    let expected_epoch = parts[5]
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "invalid agent_ref epoch".to_string())?;
    let expected_process_pid = parts[6]
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "invalid agent_ref process PID".to_string())?;
    if parts[7].len() != 64
        || !parts[7]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("invalid agent_ref process start token digest".to_string());
    }
    let record = {
        let state = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state
            .as_ref()
            .ok_or_else(|| "daemon is hydrating".to_string())?;
        state
            .leased
            .runtime
            .record(&pane)
            .cloned()
            .ok_or_else(|| "agent pane is not retained".to_string())?
    };
    let process = record
        .agent_process
        .clone()
        .ok_or_else(|| "agent process identity is unavailable".to_string())?;
    let process_digest = format!("{:x}", Sha256::digest(process.start_token.as_bytes()));
    if record.state_id.as_str() != parts[4]
        || record.agent_epoch != expected_epoch
        || process.pid != expected_process_pid
        || process_digest != parts[7]
        || !record.agent_present
    {
        return Err("agent_ref was replaced before dispatch".to_string());
    }
    if record.agent.as_str() != "codex" {
        return Err(format!(
            "unsupported provider: durable guarded prompt dispatch is enabled only for Codex, not {}",
            record.agent.as_str()
        ));
    }
    if !matches!(record.lifecycle, crate::pane_state::LifecycleState::Idle) {
        return Err("agent is busy or blocked".to_string());
    }
    let provider_session_id = record.agent_session_id.clone();
    let expected_run_seq = record
        .run_seq
        .checked_add(1)
        .ok_or_else(|| "agent run sequence overflow".to_string())?;
    let expected_pane_version = record.version();
    let expected_current_run = record.current_run.clone();
    Ok((
        crate::agent_state::OperationBinding {
            server_identity: coordinator.incarnation.identity.clone(),
            pane_instance: pane.clone(),
            pane_state_id: record.state_id,
            agent_epoch: record.agent_epoch,
            agent_kind: record.agent,
            provider_session_id,
            process,
        },
        expected_run_seq,
        pane,
        expected_pane_version,
        expected_current_run,
    ))
}

fn verify_agent_prompt_precondition(
    coordinator: &ProductionV2Coordinator,
    operation: &crate::agent_state::OperationRecord,
) -> std::result::Result<(), String> {
    let state = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state
        .as_ref()
        .ok_or_else(|| "daemon is hydrating".to_string())?;
    let record = state
        .leased
        .runtime
        .record(&operation.binding.pane_instance)
        .ok_or_else(|| "agent pane is no longer retained".to_string())?;
    let exact_binding_matches = agent_prompt_precondition_matches(record, operation);
    if exact_binding_matches {
        Ok(())
    } else {
        Err(
            "pane revision, lifecycle, current run, session, or process changed before dispatch"
                .to_string(),
        )
    }
}

fn agent_prompt_precondition_matches(
    record: &crate::pane_state::PaneState,
    operation: &crate::agent_state::OperationRecord,
) -> bool {
    record.agent_present
        && record.version() == operation.expected_pane_version
        && record.current_run == operation.expected_current_run
        && record.agent == operation.binding.agent_kind
        && record.agent_session_id == operation.binding.provider_session_id
        && record.agent_process.as_ref() == Some(&operation.binding.process)
        && record.run_seq.checked_add(1) == Some(operation.expected_run_seq)
        && matches!(record.lifecycle, crate::pane_state::LifecycleState::Idle)
}

#[allow(clippy::too_many_arguments)]
fn apply_resolve_agent_run(
    coordinator: &ProductionV2Coordinator,
    event_id: EventId,
    run_ref: String,
    outcome: String,
    precondition: crate::agent_state::RecoveryPrecondition,
    resolution_id: crate::agent_state::ResolutionId,
    reason: String,
    actor_pid: u32,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, PROTOCOL_VERSION, ServerMessage};
    use crate::tmux::TmuxRunner as _;

    if outcome != "completed" || actor_pid == 0 {
        return ServerMessage::error(
            ErrorCode::InvalidRequest,
            "agent run resolve requires outcome=completed and a positive actor PID",
            Some(event_id),
        );
    }
    let observed_at = epoch_seconds();
    let reference = match crate::agent_state::RunRef::decode(&run_ref) {
        Ok(reference) => reference,
        Err(error) => {
            return ServerMessage::error(
                ErrorCode::InvalidRequest,
                error.to_string(),
                Some(event_id),
            );
        }
    };
    let (run, already_resolved) = {
        let runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let Some(runtime) = runtime.as_ref() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "agent runtime is hydrating",
                Some(event_id),
            );
        };
        let existing = match runtime.lookup_operator_completion(&reference, &resolution_id, &reason)
        {
            Ok(existing) => existing,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        };
        if let Some(run) = existing {
            (run, true)
        } else {
            match runtime.get_run(&reference) {
                Ok(run) => (run, false),
                Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
            }
        }
    };
    if already_resolved {
        if let Err(error) = project_operator_completed_run(coordinator, &run) {
            return production_store_error_response(coordinator, error, Some(event_id));
        }
        return ServerMessage::AgentRunResolved {
            proto: PROTOCOL_VERSION,
            run_ref,
            run,
        };
    }
    let runner = coordinator.status_push_runner(Duration::from_secs(3));
    let first_process = match runner
        .resolve_agent_process(run.binding.pane_instance.pane_pid, &run.binding.agent_kind)
    {
        Ok(process) => process,
        Err(error) => {
            return ServerMessage::error(
                ErrorCode::StalePrecondition,
                format!("fresh process observation failed: {error}"),
                Some(event_id),
            );
        }
    };
    let fresh_viewport_fingerprint =
        if let crate::agent_state::RecoveryProcessExpectation::ExactPresentStable {
            process: expected,
        } = &precondition.process_expectation
        {
            if first_process.as_ref() != Some(expected) || expected != &run.binding.process {
                return ServerMessage::error(
                    ErrorCode::StalePrecondition,
                    "the exact bound process is no longer present",
                    Some(event_id),
                );
            }
            if let Err(error) = crate::api::verify_recovery_foreground_owner(
                &runner,
                &run.binding.pane_instance,
                &run.binding.process,
            ) {
                return ServerMessage::error(
                    ErrorCode::StalePrecondition,
                    format!("the exact bound process is no longer the foreground owner: {error}"),
                    Some(event_id),
                );
            }
            let fingerprint = match crate::api::capture_visible_viewport_fingerprint(
                &runner,
                &run.binding.pane_instance,
            ) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    return ServerMessage::error(
                        ErrorCode::StalePrecondition,
                        format!("fresh viewport capture failed: {error:#}"),
                        Some(event_id),
                    );
                }
            };
            let second_process = match runner
                .resolve_agent_process(run.binding.pane_instance.pane_pid, &run.binding.agent_kind)
            {
                Ok(process) => process,
                Err(error) => {
                    return ServerMessage::error(
                        ErrorCode::StalePrecondition,
                        format!("second fresh process observation failed: {error}"),
                        Some(event_id),
                    );
                }
            };
            if second_process != first_process
                || crate::api::verify_recovery_foreground_owner(
                    &runner,
                    &run.binding.pane_instance,
                    &run.binding.process,
                )
                .is_err()
            {
                return ServerMessage::error(
                    ErrorCode::StalePrecondition,
                    "process identity or foreground ownership changed during viewport capture",
                    Some(event_id),
                );
            }
            Some(fingerprint)
        } else {
            None
        };
    let fresh_pane = match recovery_pane_fence_for_run(coordinator, &run) {
        Ok(pane) => pane,
        Err(message) => {
            return ServerMessage::error(ErrorCode::StalePrecondition, message, Some(event_id));
        }
    };
    let resolved = {
        let mut runtime = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned");
        let Some(runtime) = runtime.as_mut() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "agent runtime is hydrating",
                Some(event_id),
            );
        };
        match runtime.resolve_operator_completed(
            &reference,
            &precondition,
            resolution_id,
            reason,
            // The daemon socket is private to this user's runtime directory. Record the
            // effective daemon UID and the caller-reported PID in the durable audit.
            unsafe { libc::geteuid() },
            actor_pid,
            observed_at,
            &fresh_pane,
            first_process,
            fresh_viewport_fingerprint.as_ref(),
        ) {
            Ok(run) => run,
            Err(error) => return agent_state_query_error_with_event(error, Some(event_id)),
        }
    };
    if let Err(error) = project_operator_completed_run(coordinator, &resolved) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::AgentRunResolved {
        proto: PROTOCOL_VERSION,
        run_ref,
        run: resolved,
    }
}

fn project_operator_completed_run(
    coordinator: &ProductionV2Coordinator,
    run: &crate::agent_state::RunRecord,
) -> std::result::Result<(), crate::pane_state::store::StoreError> {
    let mut state = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = state.as_mut() else {
        return Err(crate::pane_state::store::StoreError::PersistFailed(
            "canonical pane state is hydrating".to_string(),
        ));
    };
    if !state
        .leased
        .runtime
        .record(&run.binding.pane_instance)
        .is_some_and(|pane| pane_belongs_to_run_epoch(pane, run))
    {
        // A replacement pane must never receive a historical run projection. The
        // durable Run is already complete, so there is nothing to repair here.
        return Ok(());
    }
    let projection = crate::pane_state::CurrentDurableRunProjection {
        run_id: run.run_id.as_str().to_string(),
        run_seq: run.run_seq,
        run_revision: run.revision,
    };
    let mut io = pane_snapshot_store(coordinator);
    if state.leased.runtime.project_current_run(
        &mut io,
        &run.binding.pane_instance,
        projection,
        false,
        run.updated_at,
    )? {
        let _ = state.checked_resolved_snapshot()?;
    }
    Ok(())
}

fn recovery_pane_fence_for_run(
    coordinator: &ProductionV2Coordinator,
    run: &crate::agent_state::RunRecord,
) -> std::result::Result<crate::agent_state::RecoveryPaneFence, String> {
    let state = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let canonical = state
        .as_ref()
        .and_then(|state| state.leased.runtime.record(&run.binding.pane_instance))
        .ok_or_else(|| "the pane bound to the run has no canonical state".to_string())?;
    if canonical.state_id != run.binding.pane_state_id
        || canonical.agent_epoch != run.binding.agent_epoch
        || canonical.agent != run.binding.agent_kind
        || canonical.agent_session_id.as_ref() != Some(&run.binding.provider_session_id)
        || canonical.pane_instance != run.binding.pane_instance
    {
        return Err("the pane no longer has the complete binding recorded by the run".to_string());
    }
    let current_run = canonical
        .current_run
        .clone()
        .ok_or_else(|| "the pane no longer points at a durable run".to_string())?;
    let subagent_count = u32::try_from(canonical.subagents.len())
        .map_err(|_| "pane subagent count overflow".to_string())?;
    Ok(crate::agent_state::RecoveryPaneFence {
        state_id: canonical.state_id.clone(),
        revision: canonical.revision,
        current_run,
        lifecycle: canonical.lifecycle.clone(),
        subagent_count,
    })
}

fn agent_state_query_error(error: crate::agent_state::StoreError) -> ServerMessage {
    agent_state_query_error_with_event(error, None)
}

fn agent_state_query_error_with_event(
    error: crate::agent_state::StoreError,
    event_id: Option<EventId>,
) -> ServerMessage {
    use crate::agent_state::StoreError;
    let code = match error {
        StoreError::StalePrecondition(_) => ErrorCode::StalePrecondition,
        StoreError::RecoveryNotAllowed(_) => ErrorCode::RecoveryNotAllowed,
        StoreError::ResolutionConflict(_) => ErrorCode::ResolutionConflict,
        StoreError::RunAlreadyResolved(_) => ErrorCode::RunAlreadyResolved,
        StoreError::PromptDispatchBusy(_) => ErrorCode::PromptDispatchBusy,
        StoreError::OperationConflict(_) => ErrorCode::OperationConflict,
        StoreError::OperationStoreFull(_) => ErrorCode::OperationStoreFull,
        StoreError::OperationNotFound(_) => ErrorCode::OperationNotFound,
        StoreError::OperationGenerationReplaced(_) => ErrorCode::OperationGenerationReplaced,
        StoreError::RunNotFound(_) => ErrorCode::RunNotFound,
        StoreError::RunGenerationReplaced(_) => ErrorCode::RunGenerationReplaced,
        StoreError::ProviderEventConflict(_) => ErrorCode::ProviderEventConflict,
        StoreError::ArtifactUnavailable => ErrorCode::ArtifactUnavailable,
        StoreError::ArtifactExpired => ErrorCode::ArtifactExpired,
        StoreError::StateUninitialized => ErrorCode::StateUninitialized,
        StoreError::Capacity(_) => ErrorCode::StorageCapacityExceeded,
        StoreError::NotFound(_) => ErrorCode::RunNotFound,
        StoreError::Invalid(_) | StoreError::Conflict(_) => ErrorCode::InvalidRequest,
        StoreError::Io(_) | StoreError::Corrupt(_) => ErrorCode::PersistFailed,
    };
    ServerMessage::error(code, error.to_string(), event_id)
}

fn apply_diagnostic_projection(
    coordinator: &ProductionV2Coordinator,
    pane_instance: Option<PaneInstance>,
    message: String,
) -> Result<u64, crate::pane_state::store::StoreError> {
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before diagnostic");
    if let Some(pane) = pane_instance {
        state.leased.runtime.add_diagnostic(pane, message)?;
    } else {
        state.add_global_diagnostic(ErrorCode::InternalError, message)?;
    }
    Ok(state.leased.runtime.snapshot_revision())
}

fn apply_triage_projection(
    coordinator: &ProductionV2Coordinator,
) -> Result<u64, crate::pane_state::store::StoreError> {
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before triage projection");
    state.leased.runtime.advance_poll_projection()?;
    Ok(state.leased.runtime.snapshot_revision())
}

/// Applies one observation poll as a single sequenced mutation. Every stage
/// reuses the standalone-mutation helpers, so reducer, persist, and read-back
/// contracts are identical to the previous one-mutation-per-event queue; only
/// the snapshot publish moves to the end of the batch.
fn apply_observation_batch(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    payload: ObservationBatchPayload,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let ObservationBatchPayload {
        projection,
        observations,
        removals,
        diagnostics,
    } = payload;
    if let Err(error) = apply_observation_poll_projection(coordinator, *projection) {
        return observation_poll_error_response(coordinator, error);
    }
    for envelope in observations.into_iter().chain(removals) {
        // Nonfatal per-pane failures keep processing the remaining panes, same
        // as the standalone mutation queue did; fail-stop conditions raise the
        // shutdown flag inside the helper and abort the rest of the batch.
        let _ = apply_pane_event_mutation(coordinator, accepted_seq, envelope, true, None);
        if coordinator.shutdown.load(Ordering::SeqCst) {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "daemon failed stop during observation batch",
                None,
            );
        }
    }
    for (pane_instance, message) in diagnostics {
        if let Err(error) = apply_diagnostic_projection(coordinator, pane_instance, message) {
            return production_store_error_response(coordinator, error, None);
        }
    }
    match apply_triage_projection(coordinator) {
        Ok(revision) => ServerMessage::SnapshotAck {
            event_id: EventId::generate().expect("OS random source failed after daemon startup"),
            accepted_seq,
            snapshot_revision: revision,
        },
        Err(error) => production_store_error_response(coordinator, error, None),
    }
}

fn apply_pane_event_mutation(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: PaneEventEnvelope,
    defer_full_preflight: bool,
    durable_run: Option<crate::agent_state::RunRecord>,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{PaneApplyOutcome, ServerMessage};

    let event_id = envelope.event_id.clone();
    let durable_run = match durable_run {
        Some(run) => Some(run),
        None => match reconcile_run_for_pane_event(coordinator, &envelope) {
            Ok(run) => run,
            Err(error) => {
                return ServerMessage::error(
                    ErrorCode::PersistFailed,
                    error.to_string(),
                    Some(event_id),
                );
            }
        },
    };
    if let PaneEvent::PaneRemoved { expected } = &envelope.event {
        return apply_pane_removal(
            coordinator,
            accepted_seq,
            event_id,
            envelope.pane_instance,
            expected.clone(),
        );
    }
    let (visibility, visibility_diagnostic) =
        match unread_visibility_for_event(coordinator, &envelope) {
            Ok(value) => value,
            Err(error) => {
                coordinator.fail_stop(error.to_string());
                return production_store_error_response(coordinator, error, Some(event_id));
            }
        };
    let result = {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let Some(state) = state_guard.as_mut() else {
            return ServerMessage::error(
                ErrorCode::NotReady,
                "daemon is hydrating",
                Some(event_id),
            );
        };
        let mut io = pane_snapshot_store(coordinator);
        let revision_before = state.leased.runtime.snapshot_revision();
        state
            .leased
            .runtime
            .apply_event(&mut io, &envelope, &visibility)
            .and_then(|mut result| {
                if let Some(run) = durable_run.as_ref()
                    && state
                        .leased
                        .runtime
                        .record(&envelope.pane_instance)
                        .is_some_and(|pane| pane_belongs_to_run_epoch(pane, run))
                {
                    let projection = crate::pane_state::CurrentDurableRunProjection {
                        run_id: run.run_id.as_str().to_string(),
                        run_seq: run.run_seq,
                        run_revision: run.revision,
                    };
                    if state.leased.runtime.project_current_run(
                        &mut io,
                        &envelope.pane_instance,
                        projection,
                        run.execution_active(),
                        run.updated_at,
                    )? {
                        result.state_version = state
                            .leased
                            .runtime
                            .record(&envelope.pane_instance)
                            .map(crate::pane_state::PaneState::version);
                        result.snapshot_revision = state.leased.runtime.snapshot_revision();
                    }
                }
                let result = finish_pane_event_projection(
                    coordinator,
                    state,
                    &envelope.pane_instance,
                    visibility_diagnostic.as_deref(),
                    revision_before,
                    result,
                    defer_full_preflight,
                )?;
                if matches!(envelope.event, PaneEvent::BeginRun { .. })
                    && let Some(record) = state.leased.runtime.record(&envelope.pane_instance)
                {
                    coordinator.schedule_task_summary(record);
                }
                Ok(result)
            })
    };
    match result {
        Ok(result) => ServerMessage::PaneEventResult {
            event_id,
            accepted_seq,
            state_version: result.state_version,
            snapshot_revision: result.snapshot_revision,
            outcome: if result.outcome
                == crate::pane_state::reducer::ReductionOutcome::CanonicalChanged
            {
                PaneApplyOutcome::Committed
            } else {
                PaneApplyOutcome::Noop
            },
        },
        Err(error) => {
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            production_store_error_response(coordinator, error, Some(event_id))
        }
    }
}

fn reconcile_run_for_pane_event(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
) -> Result<Option<crate::agent_state::RunRecord>, crate::agent_state::StoreError> {
    let (checked, process, observed_at) = match &envelope.event {
        PaneEvent::ObservationBatch {
            process: Some(process),
            observed_at,
            ..
        } => (
            process.agent_process_checked,
            process.agent_process.as_ref(),
            *observed_at,
        ),
        PaneEvent::PaneRemoved { .. } => (true, None, epoch_seconds()),
        _ => return Ok(None),
    };
    coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned")
        .as_mut()
        .map_or(Ok(None), |runtime| {
            runtime.reconcile_process_for_pane(
                &envelope.pane_instance,
                checked,
                process,
                observed_at,
            )
        })
}

fn pane_belongs_to_run_epoch(
    pane: &crate::pane_state::PaneState,
    run: &crate::agent_state::RunRecord,
) -> bool {
    pane.pane_instance == run.binding.pane_instance
        && pane.state_id == run.binding.pane_state_id
        && pane.agent_epoch == run.binding.agent_epoch
        && pane.agent == run.binding.agent_kind
        && pane.agent_session_id.as_ref() == Some(&run.binding.provider_session_id)
}

fn pane_needs_durable_run_projection(
    pane: &crate::pane_state::PaneState,
    run: &crate::agent_state::RunRecord,
) -> std::result::Result<bool, String> {
    if !pane_belongs_to_run_epoch(pane, run) {
        return Ok(false);
    }
    match pane.current_run.as_ref() {
        Some(current) if current.run_id == run.run_id.as_str() => Ok(true),
        Some(_) if pane.run_seq == run.run_seq => Err(
            "Pane current durable run identity conflicts with a duplicate provider Run".to_string(),
        ),
        Some(_) => Ok(pane.run_seq < run.run_seq),
        None => Ok(pane.run_seq <= run.run_seq),
    }
}

fn pane_snapshot_store(
    coordinator: &ProductionV2Coordinator,
) -> crate::pane_state::snapshot::FilePaneSnapshotStore {
    crate::pane_state::snapshot::FilePaneSnapshotStore::new(
        crate::pane_state::snapshot::snapshot_path(&coordinator.env, &coordinator.incarnation.hash),
        coordinator.incarnation.identity.clone(),
    )
}

/// `defer_full_preflight` is set on the observation-batch path: the per-pane
/// persist/read-back contract still runs here, while the full resolved-snapshot
/// preflight happens once when the batch publishes.
fn finish_pane_event_projection(
    coordinator: &ProductionV2Coordinator,
    state: &mut super::runtime::CanonicalCoordinatorState,
    pane: &PaneInstance,
    visibility_diagnostic: Option<&str>,
    revision_before: u64,
    mut result: crate::pane_state::store::ApplyResult,
    defer_full_preflight: bool,
) -> Result<crate::pane_state::store::ApplyResult, crate::pane_state::store::StoreError> {
    let mut messages = visibility_diagnostic
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for notification in state.leased.runtime.drain_notification_jobs() {
        let agent = match state.leased.runtime.record(&notification.pane_instance) {
            Some(active) if active.version() == notification.state_version => {
                active.agent.as_str().to_string()
            }
            _ => {
                messages.push(format!(
                    "notification_target_missing: pane={} state={:?}",
                    notification.pane_instance.pane_id, notification.state_version
                ));
                continue;
            }
        };
        let Some(sender) = coordinator.notification_tx.as_ref() else {
            continue;
        };
        let job = NotificationWorkerJob {
            pane_id: notification.pane_instance.pane_id.clone(),
            agent,
        };
        if let Err(error) = sender.try_send(job) {
            let reason = match error {
                TrySendError::Full(_) => "queue_full",
                TrySendError::Disconnected(_) => "worker_disconnected",
            };
            messages.push(format!(
                "notification_dispatch_failed: pane={} reason={reason}",
                notification.pane_instance.pane_id
            ));
            log_notification_failure(
                Some(&(
                    coordinator.env.clone(),
                    coordinator.incarnation.hash.clone(),
                )),
                &format!(
                    "notification dispatch failed for pane {}: {reason}",
                    notification.pane_instance.pane_id
                ),
            );
        }
    }
    result.snapshot_revision = state.leased.runtime.finish_sequenced_projection(
        Some(pane),
        messages,
        false,
        revision_before,
    )?;
    if !defer_full_preflight {
        let _ = state.checked_resolved_snapshot()?;
    }
    Ok(result)
}

fn apply_pane_removal(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    pane: PaneInstance,
    expected: Option<crate::pane_state::StoredStateDescriptor>,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{PaneApplyOutcome, ServerMessage};
    let topology = match query_full_topology(coordinator, Duration::from_millis(100)) {
        Ok(topology) => topology,
        Err(error) => {
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            return ServerMessage::error(
                ErrorCode::InternalError,
                error.to_string(),
                Some(event_id),
            );
        }
    };
    let still_present = topology
        .panes
        .iter()
        .any(|current| current.pane_instance == pane);
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before pane removal");
    let topology_changed = state.topology != topology;
    state.topology = topology;
    if let Err(error) = persist_pruned_sidebar_pins(coordinator, state) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    if still_present {
        if topology_changed && let Err(error) = state.leased.runtime.mark_projection_changed() {
            return production_store_error_response(coordinator, error, Some(event_id));
        }
        return ServerMessage::PaneEventResult {
            event_id,
            accepted_seq,
            state_version: state
                .leased
                .runtime
                .record(&pane)
                .map(|state| state.version()),
            snapshot_revision: state.leased.runtime.snapshot_revision(),
            outcome: PaneApplyOutcome::Noop,
        };
    }
    let mut io = pane_snapshot_store(coordinator);
    let removed = match state
        .leased
        .runtime
        .remove_absent_pane(&mut io, &pane, expected.as_ref())
    {
        Ok(removed) => removed,
        Err(error) => {
            return production_store_error_response(coordinator, error, Some(event_id));
        }
    };
    if topology_changed
        && !removed
        && let Err(error) = state.leased.runtime.mark_projection_changed()
    {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::PaneEventResult {
        event_id,
        accepted_seq,
        state_version: None,
        snapshot_revision: state.leased.runtime.snapshot_revision(),
        outcome: if removed {
            PaneApplyOutcome::Committed
        } else {
            PaneApplyOutcome::Noop
        },
    }
}

fn unread_visibility_for_event(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
) -> Result<
    (crate::pane_state::VisibilitySnapshot, Option<String>),
    crate::pane_state::store::StoreError,
> {
    use crate::pane_state::{PaneEvent, ReportedLifecycle};

    let (current, tracker, focus_equivalent_panes) = {
        let state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard.as_ref();
        let current = state
            .and_then(|state| state.leased.runtime.record(&envelope.pane_instance))
            .cloned();
        let tracker = state
            .map(|state| state.leased.runtime.tracker(&envelope.pane_instance))
            .unwrap_or_default();
        let focus_equivalent_panes = state.map_or_else(
            || BTreeSet::from([envelope.pane_instance.clone()]),
            |state| state.focus_equivalent_panes(&envelope.pane_instance),
        );
        (current, tracker, focus_equivalent_panes)
    };
    let may_create_unread = match &envelope.event {
        PaneEvent::WaitRequested { .. } | PaneEvent::FailRun { .. } => true,
        PaneEvent::CompleteRun { .. } | PaneEvent::ResponseAndCompleteRun { .. } => {
            current.as_ref().is_none_or(|state| {
                state.run_seq > state.completed_seq || state.synthetic_completion_armed
            })
        }
        PaneEvent::ExplicitStateReported { report }
            if matches!(
                report.lifecycle,
                Some(ReportedLifecycle::Waiting { .. } | ReportedLifecycle::Error { .. })
            ) =>
        {
            true
        }
        PaneEvent::ExplicitStateReported { report }
            if matches!(report.lifecycle, Some(ReportedLifecycle::Idle)) =>
        {
            current
                .as_ref()
                .map_or(report.completed_at.is_some() || report.attention, |state| {
                    state.run_seq > state.completed_seq
                        || (state.synthetic_completion_armed
                            && (report.completed_at.is_some() || report.attention))
                })
        }
        PaneEvent::ObservationBatch {
            presence, capture, ..
        } => current.as_ref().is_some_and(|state| {
            observation_may_create_unread(state, &tracker, presence, capture.as_ref())
        }),
        _ => false,
    };
    if !may_create_unread {
        return Ok((crate::pane_state::VisibilitySnapshot::default(), None));
    }
    let io = crate::daemon::view_hooks::SystemFreshVisibilityIo::new(
        coordinator
            .env
            .get("VDE_TMUX_SOCKET_NAME")
            .cloned()
            .filter(|value| !value.trim().is_empty()),
        coordinator.incarnation.identity.clone(),
    );
    use crate::daemon::view_hooks::FreshVisibilityIo as _;
    let observation_seq = coordinator.begin_witness_observation();
    match io.query_witnesses(crate::daemon::view_hooks::FRESH_VISIBILITY_TIMEOUT) {
        Ok(witnesses) => {
            let pane_visible_to_eligible_client = {
                let mut guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                guard.as_mut().is_some_and(|state| {
                    state.reconcile_peek_leases(&witnesses, observation_seq);
                    let authorized =
                        state.has_read_authority_for(&witnesses, &focus_equivalent_panes);
                    if authorized {
                        state.clear_peeks_for_read_panes(&focus_equivalent_panes);
                    }
                    authorized
                })
            };
            Ok((
                crate::pane_state::VisibilitySnapshot {
                    pane_visible_to_eligible_client,
                },
                None,
            ))
        }
        Err(error) if error.requires_daemon_exit() => Err(
            crate::pane_state::store::StoreError::FailStop(error.to_string()),
        ),
        Err(error) => Ok((
            crate::pane_state::VisibilitySnapshot::default(),
            Some(format!("fresh_visibility_unavailable: {error}")),
        )),
    }
}

fn observation_may_create_unread(
    state: &crate::pane_state::PaneState,
    tracker: &crate::pane_state::CaptureTrackerSnapshot,
    presence: &crate::pane_state::AgentPresenceObservation,
    capture: Option<&crate::pane_state::CaptureObservation>,
) -> bool {
    use crate::pane_state::{AgentPresenceObservation, CaptureInference, LifecycleState};

    let absence_evidence = match presence {
        AgentPresenceObservation::Absent => true,
        AgentPresenceObservation::Present(kind) => kind != &state.agent,
        AgentPresenceObservation::Unknown => false,
    };
    let confirmed_absence_can_complete = absence_evidence
        && tracker.absence_count >= 1
        && state.scan_verified
        && !matches!(state.lifecycle, LifecycleState::Idle);
    let capture_is_applied = state.agent_present
        && matches!(presence, AgentPresenceObservation::Present(kind) if kind == &state.agent);
    let stale_capture_can_complete = capture_is_applied
        && matches!(
            capture,
            Some(crate::pane_state::CaptureObservation {
                inference: CaptureInference::StaleRunCompleted,
                ..
            })
        )
        && matches!(state.lifecycle, LifecycleState::Running);
    let permission_wait_can_create = capture_is_applied
        && matches!(
            capture,
            Some(crate::pane_state::CaptureObservation {
                inference: CaptureInference::PermissionWait { .. },
                ..
            })
        )
        && !matches!(state.lifecycle, LifecycleState::Waiting { .. });
    confirmed_absence_can_complete || stale_capture_can_complete || permission_wait_can_create
}

fn apply_external_view_event(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event: crate::pane_state::ViewEvent,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let event_id = event.event_id.clone();
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = state_guard.as_mut() else {
        return ServerMessage::error(ErrorCode::NotReady, "daemon is hydrating", Some(event_id));
    };
    let revision_before = state.leased.runtime.snapshot_revision();
    if let Err(error) = event.validate() {
        return ServerMessage::error(ErrorCode::InvalidRequest, error.to_string(), Some(event_id));
    }

    let diagnostic_pane = event.active_pane.as_ref().cloned().or_else(|| {
        state
            .topology
            .panes
            .first()
            .map(|pane| pane.pane_instance.clone())
    });

    if let Err(error) = state.leased.runtime.finish_sequenced_projection(
        diagnostic_pane.as_ref(),
        std::iter::empty(),
        false,
        revision_before,
    ) {
        return production_store_error_response(coordinator, error, Some(event_id));
    }
    ServerMessage::ViewQueued {
        event_id,
        accepted_seq,
    }
}

fn query_full_topology(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
) -> Result<crate::daemon::topology::TopologySnapshot, crate::daemon::topology::TopologyError> {
    let framing = crate::daemon::topology::QueryFraming::generate()?;
    let args = crate::daemon::topology::poll_query_args(&framing);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout)
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
    let output = runner
        .run(&refs)
        .map_err(|error| crate::daemon::topology::TopologyError::Query(error.to_string()))?;
    crate::daemon::topology::parse_topology(&output, &framing, &coordinator.incarnation.identity)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservationPollFraming {
    query: crate::daemon::topology::QueryFraming,
    topology_end: String,
    status_end: String,
    client_end: String,
    final_end: String,
}

impl ObservationPollFraming {
    fn generate() -> Result<Self, crate::daemon::topology::TopologyError> {
        Self::from_query(crate::daemon::topology::QueryFraming::generate()?)
    }

    fn from_query(
        query: crate::daemon::topology::QueryFraming,
    ) -> Result<Self, crate::daemon::topology::TopologyError> {
        let token = query.token();
        if token.is_empty() {
            return Err(crate::daemon::topology::TopologyError::InvalidFraming(
                "observation poll query token is empty".to_string(),
            ));
        }
        Ok(Self {
            topology_end: format!("__vde_poll_topology_end_{token}__"),
            status_end: format!("__vde_poll_status_end_{token}__"),
            client_end: format!("__vde_poll_client_end_{token}__"),
            final_end: format!("__vde_poll_final_end_{token}__"),
            query,
        })
    }

    fn query_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        append_tmux_command(
            &mut args,
            crate::daemon::topology::guarded_poll_query_args(&self.query),
        );
        append_tmux_display_marker(&mut args, &self.topology_end);
        append_tmux_command(
            &mut args,
            crate::daemon::topology::status_metadata_query_args(&self.query),
        );
        append_tmux_display_marker(&mut args, &self.status_end);
        append_tmux_command(
            &mut args,
            crate::daemon::view_hooks::guarded_client_view_query_args(self.query.token()),
        );
        append_tmux_display_marker(&mut args, &self.client_end);
        append_tmux_display_marker(&mut args, &self.final_end);
        args
    }
}

fn append_tmux_command(args: &mut Vec<String>, command: Vec<String>) {
    if !args.is_empty() {
        args.push(";".to_string());
    }
    args.extend(command);
}

fn append_tmux_display_marker(args: &mut Vec<String>, marker: &str) {
    append_tmux_command(
        args,
        vec![
            "display-message".to_string(),
            "-p".to_string(),
            marker.to_string(),
        ],
    );
}

#[derive(Debug)]
enum ObservationPollQueryError {
    Framing(String),
    Topology(crate::daemon::topology::TopologyError),
    Client(crate::daemon::view_hooks::FreshVisibilityError),
}

impl ObservationPollQueryError {
    fn requires_daemon_exit(&self) -> bool {
        match self {
            Self::Framing(_) => false,
            Self::Topology(error) => error.requires_daemon_exit(),
            Self::Client(error) => error.requires_daemon_exit(),
        }
    }
}

impl std::fmt::Display for ObservationPollQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Framing(message) => formatter.write_str(message),
            Self::Topology(error) => write!(formatter, "{error}"),
            Self::Client(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ObservationPollQueryError {}

fn query_observation_poll_projection(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
) -> Result<ObservationPollProjection, ObservationPollQueryError> {
    let observation_seq = coordinator.begin_witness_observation();
    let framing =
        ObservationPollFraming::generate().map_err(ObservationPollQueryError::Topology)?;
    let args = framing.query_args();
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout)
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
    let output = runner.run(&refs).map_err(|error| {
        ObservationPollQueryError::Topology(crate::daemon::topology::TopologyError::Query(
            error.to_string(),
        ))
    })?;
    let mut projection =
        parse_observation_poll_projection(&output, &framing, &coordinator.incarnation.identity)?;
    projection.observation_seq = observation_seq;
    Ok(projection)
}

fn parse_observation_poll_projection(
    output: &str,
    framing: &ObservationPollFraming,
    expected_identity: &crate::daemon::topology::ServerIdentity,
) -> Result<ObservationPollProjection, ObservationPollQueryError> {
    crate::daemon::topology::ensure_query_output_size(output)
        .map_err(ObservationPollQueryError::Topology)?;
    let (topology_frame, remainder) =
        split_observation_poll_frame(output, &framing.topology_end, "topology")?;
    let (status_frame, remainder) =
        split_observation_poll_frame(remainder, &framing.status_end, "status")?;
    let (client_frame, remainder) =
        split_observation_poll_frame(remainder, &framing.client_end, "client")?;
    let expected_final = format!("{}\n", framing.final_end);
    if remainder != expected_final {
        return Err(ObservationPollQueryError::Framing(
            "observation poll final marker is missing or not final".to_string(),
        ));
    }

    let topology_frame = format!("{topology_frame}\n");
    let status_frame = format!("{status_frame}\n");
    let client_frame = format!("{client_frame}\n");
    let topology =
        crate::daemon::topology::parse_topology(&topology_frame, &framing.query, expected_identity)
            .map_err(ObservationPollQueryError::Topology)?;
    let status = crate::daemon::topology::parse_status_metadata(
        &status_frame,
        &framing.query,
        expected_identity,
    )
    .map_err(ObservationPollQueryError::Topology)?;
    let witnesses = crate::daemon::view_hooks::parse_client_view_query(
        &client_frame,
        framing.query.token(),
        expected_identity,
    )
    .map_err(ObservationPollQueryError::Client)?;

    Ok(ObservationPollProjection {
        observation_seq: 0,
        topology,
        status_metadata: status_projection_metadata(status, &witnesses),
        witnesses,
        observation_bases: BTreeMap::new(),
        view_base: crate::daemon::view_hooks::CurrentClientViews::default(),
        through_unread_order: 0,
    })
}

fn split_observation_poll_frame<'a>(
    output: &'a str,
    marker: &str,
    section: &str,
) -> Result<(&'a str, &'a str), ObservationPollQueryError> {
    let delimiter = format!("\n{marker}\n");
    let Some((frame, remainder)) = output.split_once(&delimiter) else {
        return Err(ObservationPollQueryError::Framing(format!(
            "observation poll {section} marker is missing"
        )));
    };
    if remainder.starts_with(&format!("{marker}\n")) || remainder.contains(&delimiter) {
        return Err(ObservationPollQueryError::Framing(format!(
            "observation poll {section} marker is duplicated"
        )));
    }
    Ok((frame, remainder))
}

fn query_status_projection_metadata(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
    witnesses: &[crate::pane_state::ClientWitness],
) -> Result<super::runtime::StatusProjectionMetadata, crate::daemon::topology::TopologyError> {
    let framing = crate::daemon::topology::QueryFraming::generate()?;
    let args = crate::daemon::topology::status_metadata_query_args(&framing);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout)
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
    let output = runner
        .run(&refs)
        .map_err(|error| crate::daemon::topology::TopologyError::Query(error.to_string()))?;
    let snapshot = crate::daemon::topology::parse_status_metadata(
        &output,
        &framing,
        &coordinator.incarnation.identity,
    )?;
    Ok(status_projection_metadata(snapshot, witnesses))
}

fn status_projection_metadata(
    snapshot: crate::daemon::topology::StatusMetadataSnapshot,
    witnesses: &[crate::pane_state::ClientWitness],
) -> super::runtime::StatusProjectionMetadata {
    let attached_sessions = crate::session::regular_client_session_ids(witnesses);
    let mut metadata = super::runtime::StatusProjectionMetadata::default();
    for session in snapshot.sessions {
        let attached = attached_sessions.contains(&session.session_id);
        metadata.sessions.insert(
            session.session_id,
            super::runtime::SessionProjectionMetadata {
                session_name: session.session_name,
                project_path: session.project_path,
                attached: Some(attached),
                created_at: Some(session.created_at),
            },
        );
    }
    for window in snapshot.windows {
        metadata.windows.insert(
            window.window_id,
            super::runtime::WindowProjectionMetadata {
                bell: Some(window.bell),
                activity: Some(window.activity),
                silence: Some(window.silence),
            },
        );
    }
    metadata
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WitnessObservation {
    seq: u64,
    witnesses: Vec<crate::pane_state::ClientWitness>,
}

fn query_client_witnesses(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
) -> Result<WitnessObservation, crate::daemon::view_hooks::FreshVisibilityError> {
    let seq = coordinator.begin_witness_observation();
    let token = EventId::generate()
        .map_err(|error| crate::daemon::view_hooks::FreshVisibilityError::Query(error.to_string()))?
        .as_str()
        .to_string();
    let args = crate::daemon::view_hooks::client_view_query_args(&token);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout);
    let output = runner.run(&refs).map_err(|error| {
        crate::daemon::view_hooks::FreshVisibilityError::Query(error.to_string())
    })?;
    let witnesses = crate::daemon::view_hooks::parse_client_view_query(
        &output,
        &token,
        &coordinator.incarnation.identity,
    )?;
    Ok(WitnessObservation { seq, witnesses })
}

fn refresh_full_topology(
    coordinator: &ProductionV2Coordinator,
) -> Result<u64, crate::pane_state::store::StoreError> {
    let topology = query_full_topology(coordinator, Duration::from_secs(1)).map_err(|error| {
        if error.requires_daemon_exit() {
            crate::pane_state::store::StoreError::FailStop(error.to_string())
        } else {
            crate::pane_state::store::StoreError::PersistFailed(error.to_string())
        }
    })?;
    let observation_floor = coordinator.witness_observation_seq.load(Ordering::SeqCst);
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard.as_mut().ok_or_else(|| {
        crate::pane_state::store::StoreError::PersistFailed("daemon is hydrating".to_string())
    })?;
    state.replace_topology_and_fence_observations(topology, observation_floor)?;
    persist_pruned_sidebar_pins(coordinator, state)?;
    Ok(state.leased.runtime.snapshot_revision())
}

fn apply_observation_poll_projection(
    coordinator: &ProductionV2Coordinator,
    projection: ObservationPollProjection,
) -> Result<u64> {
    {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard
            .as_mut()
            .context("state initialized before observation projection")?;
        if state.apply_topology_observation(projection.topology, projection.observation_seq)? {
            persist_pruned_sidebar_pins(coordinator, state)?;
            state.replace_status_metadata(projection.status_metadata)?;
        }
    }
    reconcile_views_with_witnesses(
        coordinator,
        projection.observation_seq,
        &projection.witnesses,
        projection.through_unread_order,
        Some(&projection.observation_bases),
        Some(&projection.view_base),
    )?;
    Ok(coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_ref()
        .map_or(0, |state| state.leased.runtime.snapshot_revision()))
}

fn observation_poll_error_response(
    coordinator: &ProductionV2Coordinator,
    error: anyhow::Error,
) -> ServerMessage {
    match error.downcast::<crate::pane_state::store::StoreError>() {
        Ok(store_error) => production_store_error_response(coordinator, store_error, None),
        Err(error) => ServerMessage::error(ErrorCode::InternalError, error.to_string(), None),
    }
}

fn targeted_pane_refresh_response(
    coordinator: &ProductionV2Coordinator,
    pane_id: &str,
) -> ServerMessage {
    let io = crate::daemon::topology::SystemTargetedRefreshIo::new(
        coordinator
            .env
            .get("VDE_TMUX_SOCKET_NAME")
            .cloned()
            .filter(|value| !value.trim().is_empty()),
    );
    let outcome =
        crate::daemon::topology::targeted_refresh(&io, pane_id, &coordinator.incarnation.identity);
    targeted_pane_refresh_outcome_response(coordinator, pane_id, outcome)
}

fn targeted_pane_refresh_outcome_response(
    coordinator: &ProductionV2Coordinator,
    pane_id: &str,
    outcome: Result<
        crate::daemon::topology::TargetedRefreshOutcome,
        crate::daemon::topology::TopologyError,
    >,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    match outcome {
        Ok(crate::daemon::topology::TargetedRefreshOutcome::NotFound) => {
            ServerMessage::error(ErrorCode::PaneNotFound, "pane was not found", None)
        }
        Ok(crate::daemon::topology::TargetedRefreshOutcome::Found(pane)) => {
            let observation_floor = coordinator.witness_observation_seq.load(Ordering::SeqCst);
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized before targeted refresh");
            let mut topology = state.topology.clone();
            topology
                .panes
                .retain(|existing| existing.pane_instance.pane_id != pane_id);
            topology.panes.push(*pane);
            topology
                .panes
                .sort_by(|left, right| left.pane_instance.cmp(&right.pane_instance));
            if let Err(error) =
                state.replace_topology_and_fence_observations(topology, observation_floor)
            {
                return production_store_error_response(coordinator, error, None);
            }
            match state.pane_presentation(pane_id) {
                Some(pane) => ServerMessage::PaneResult {
                    snapshot_revision: state.leased.runtime.snapshot_revision(),
                    pane,
                },
                None => ServerMessage::error(
                    ErrorCode::InternalError,
                    "targeted refresh did not populate pane cache",
                    None,
                ),
            }
        }
        Err(error) => {
            if matches!(
                &error,
                crate::daemon::topology::TopologyError::Query(_)
                    | crate::daemon::topology::TopologyError::Deadline
            ) {
                let diagnostic_result = {
                    let mut state_guard = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned");
                    state_guard
                        .as_mut()
                        .expect("state initialized before targeted refresh")
                        .add_global_diagnostic(
                            ErrorCode::InternalError,
                            format!("targeted pane refresh for {pane_id} failed: {error}"),
                        )
                };
                if let Err(store_error) = diagnostic_result {
                    return production_store_error_response(coordinator, store_error, None);
                }
            }
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
        }
    }
}

fn store_error_code(error: &crate::pane_state::store::StoreError) -> ErrorCode {
    use crate::pane_state::reducer::ReduceError;
    use crate::pane_state::store::StoreError;
    use ErrorCode;
    match error {
        StoreError::StateTooLarge => ErrorCode::StateTooLarge,
        StoreError::InvalidPaneInstance => ErrorCode::InvalidPaneInstance,
        StoreError::StaleStateIdentity => ErrorCode::StaleStateIdentity,
        StoreError::WriterLeaseHeld => ErrorCode::WriterLeaseHeld,
        StoreError::PersistFailed(_) => ErrorCode::PersistFailed,
        StoreError::FailStop(_) | StoreError::CounterOverflow(_) | StoreError::Random(_) => {
            ErrorCode::InternalError
        }
        StoreError::Reduce(reduce) => match reduce {
            ReduceError::InvalidRequest(_) | ReduceError::MissingStateId => {
                ErrorCode::InvalidRequest
            }
            ReduceError::InvalidPaneInstance => ErrorCode::InvalidPaneInstance,
            ReduceError::StaleStateIdentity => ErrorCode::StaleStateIdentity,
            ReduceError::StaleSelection => ErrorCode::StaleSelection,
            ReduceError::StaleAgentEvent => ErrorCode::StaleAgentEvent,
            ReduceError::InvalidProgressOperation(_) => ErrorCode::InvalidProgressOperation,
            ReduceError::StateInvariantViolation(_) => ErrorCode::StateInvariantViolation,
            ReduceError::CounterOverflow(_) => ErrorCode::InternalError,
        },
    }
}

fn store_error_response(
    error: crate::pane_state::store::StoreError,
    event_id: Option<EventId>,
) -> ServerMessage {
    ServerMessage::error(store_error_code(&error), error.to_string(), event_id)
}

fn production_store_error_response(
    coordinator: &ProductionV2Coordinator,
    error: crate::pane_state::store::StoreError,
    event_id: Option<EventId>,
) -> ServerMessage {
    if error.requires_daemon_exit() {
        coordinator.fail_stop(error.to_string());
    }
    store_error_response(error, event_id)
}

fn epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn run_runtime_daemon_server(
    config: crate::config::Config,
    socket_path: &Path,
    env: &std::collections::BTreeMap<String, String>,
    incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
) -> Result<()> {
    incarnation.verify(
        &crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3)),
        env,
    )?;
    if let Some(parent) = socket_path.parent() {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    let writer_namespace = crate::daemon::writer_lease_namespace(&incarnation.hash);
    if let Some(parent) = writer_namespace.parent() {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    let leased = super::runtime::LeasedCanonicalPaneStateRuntime::acquire(&writer_namespace)
        .map_err(anyhow::Error::new)?;
    let Some((listener, _instance_lock, socket_cleanup)) = bind_daemon_listener(socket_path)?
    else {
        return Ok(());
    };

    let (coordinator, mut runtime_cleanup) = initialize_runtime_daemon_post_bind(
        &config,
        socket_path,
        env,
        incarnation,
        socket_cleanup,
    )?;
    install_shutdown_signal_handler(coordinator.clone())?;
    let listener_coordinator = coordinator.clone();
    let connection_limiter = Arc::new(V2ConnectionThreadLimiter::new(
        V2_CONNECTION_THREAD_CAPACITY,
        V2_RESERVED_NON_STREAMING_CONNECTION_CAPACITY,
    ));
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let Some(mut connection_permit) = connection_limiter.try_acquire() else {
                        // Report overload without creating another connection thread. The
                        // short write deadline keeps overload handling itself bounded.
                        write_v2_overload_response(&mut stream);
                        drop(stream);
                        continue;
                    };
                    let coordinator = listener_coordinator.clone();
                    if let Err(error) = thread::Builder::new().spawn(move || {
                        if let Err(error) = handle_v2_runtime_stream(
                            coordinator.clone(),
                            stream,
                            &mut connection_permit,
                        ) {
                            coordinator
                                .log_daemon_error(&format!("daemon connection error: {error:#}"));
                        }
                    }) {
                        listener_coordinator.log_daemon_error(&format!(
                            "daemon connection thread spawn failed: {error}"
                        ));
                    }
                }
                Err(error) => {
                    listener_coordinator
                        .log_daemon_error(&format!("daemon listener error: {error:#}"));
                    break;
                }
            }
        }
    });

    bootstrap_v2_runtime(&coordinator, leased, env, &config)?;
    start_pane_switch_trigger_worker(coordinator.clone());
    start_tmux_server_liveness_monitor(coordinator.clone())?;
    start_v2_mutation_worker(coordinator.clone());
    start_agent_prompt_timeout_worker(coordinator.clone());
    start_sidebar_completion_forwarder(coordinator.clone());
    start_task_summary_completion_forwarder(coordinator.clone());
    let capture = crate::daemon::workers::start_capture_coordinator(
        Arc::new(crate::daemon::workers::SystemObservationWorkerIo::new(
            coordinator
                .env
                .get("VDE_TMUX_SOCKET_NAME")
                .cloned()
                .filter(|value| !value.trim().is_empty()),
        )),
        coordinator.incarnation.identity.clone(),
    );
    start_canonical_observation_worker(
        coordinator.clone(),
        Duration::from_millis(config.daemon.poll_ms),
        capture.clone(),
    );
    start_canonical_git_worker(
        coordinator.clone(),
        Duration::from_millis(config.daemon.git.poll_interval_ms),
        Duration::from_millis(config.daemon.git.timeout_ms),
    );
    start_status_push_worker(coordinator.clone());

    coordinator.wait_for_shutdown();
    runtime_cleanup.cleanup()?;
    Ok(())
}

fn start_pane_switch_trigger_worker(coordinator: Arc<ProductionV2Coordinator>) {
    const TRIGGER_QUEUE_CAPACITY: usize = 32;
    let (triggers, receiver) = mpsc::sync_channel(TRIGGER_QUEUE_CAPACITY);
    let waiter = coordinator.clone();
    thread::spawn(move || {
        while !waiter.pane_switch_trigger_shutdown.load(Ordering::SeqCst) {
            let status = Command::new("tmux")
                .arg("-S")
                .arg(&waiter.incarnation.socket_path)
                .arg("wait-for")
                .arg(waiter.pane_switch_channel())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if waiter.pane_switch_trigger_shutdown.load(Ordering::SeqCst) {
                break;
            }
            match status {
                Ok(status) if status.success() => {
                    let _ = triggers.try_send(());
                }
                _ => thread::sleep(Duration::from_millis(100)),
            }
        }
    });
    thread::spawn(move || {
        while receiver.recv().is_ok() {
            if coordinator
                .pane_switch_trigger_shutdown
                .load(Ordering::SeqCst)
                || coordinator.shutdown.load(Ordering::SeqCst)
            {
                return;
            }
            coordinator.handle_pane_switch_trigger();
        }
    });
}

fn initialize_runtime_daemon_post_bind(
    config: &crate::config::Config,
    socket_path: &Path,
    env: &std::collections::BTreeMap<String, String>,
    incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
    mut socket_cleanup: BoundDaemonSocketCleanup,
) -> Result<(Arc<ProductionV2Coordinator>, RuntimeDaemonCleanup)> {
    let notification_command = (config.notify.enabled && !config.notify.command.trim().is_empty())
        .then(|| config.notify.command.clone());
    let coordinator = Arc::new(ProductionV2Coordinator::new_with_task_summary(
        incarnation,
        env.clone(),
        notification_command,
        config.sidebar.task_summary.clone(),
    )?);
    coordinator.configure_health(config);
    let daemon_instance_id = coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .daemon_instance_id()
        .clone();
    let process_identity =
        crate::daemon::lifecycle::daemon_process_identity(socket_path, &daemon_instance_id)?;
    socket_cleanup.verify_process_identity(&process_identity)?;
    crate::daemon::lifecycle::update_lifecycle_record(
        env,
        &coordinator.incarnation.hash,
        |record| {
            record.process = Some(process_identity.clone());
            record.health = crate::daemon::lifecycle::LifecycleHealth::Stable;
            record.last_transition_error = None;
            Ok(())
        },
    )?;
    let runtime_cleanup = RuntimeDaemonCleanup::new(
        env,
        &coordinator.incarnation.hash,
        socket_path,
        process_identity,
    );
    socket_cleanup.disarm();
    Ok((coordinator, runtime_cleanup))
}

struct BoundDaemonSocketCleanup {
    socket_path: PathBuf,
    socket_device: u64,
    socket_inode: u64,
    active: bool,
}

impl BoundDaemonSocketCleanup {
    fn new(socket_path: &Path) -> Result<Self> {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

        let metadata = fs::symlink_metadata(socket_path)
            .with_context(|| format!("failed to stat bound socket {}", socket_path.display()))?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            bail!(
                "bound daemon socket identity is invalid: {}",
                socket_path.display()
            );
        }
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
            active: true,
        })
    }

    fn disarm(&mut self) {
        self.active = false;
    }

    fn verify_process_identity(
        &self,
        process_identity: &crate::daemon::lifecycle::DaemonProcessIdentity,
    ) -> Result<()> {
        if process_identity.socket_device != self.socket_device
            || process_identity.socket_inode != self.socket_inode
        {
            bail!(
                "daemon socket identity changed during post-bind initialization: {}",
                self.socket_path.display()
            );
        }
        Ok(())
    }

    fn cleanup(&mut self) {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

        if !self.active {
            return;
        }
        self.active = false;
        let Ok(metadata) = fs::symlink_metadata(&self.socket_path) else {
            return;
        };
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.dev() != self.socket_device
            || metadata.ino() != self.socket_inode
        {
            return;
        }
        if fs::remove_file(&self.socket_path).is_ok()
            && let Some(parent) = self.socket_path.parent()
        {
            let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
        }
    }
}

impl Drop for BoundDaemonSocketCleanup {
    fn drop(&mut self) {
        self.cleanup();
    }
}

struct RuntimeDaemonCleanup {
    env: std::collections::BTreeMap<String, String>,
    incarnation_hash: String,
    socket_path: PathBuf,
    process_identity: crate::daemon::lifecycle::DaemonProcessIdentity,
    active: bool,
}

impl RuntimeDaemonCleanup {
    fn new(
        env: &std::collections::BTreeMap<String, String>,
        incarnation_hash: &str,
        socket_path: &Path,
        process_identity: crate::daemon::lifecycle::DaemonProcessIdentity,
    ) -> Self {
        Self {
            env: env.clone(),
            incarnation_hash: incarnation_hash.to_string(),
            socket_path: socket_path.to_path_buf(),
            process_identity,
            active: true,
        }
    }

    fn cleanup(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        crate::daemon::lifecycle::remove_force_stopped_socket(
            &self.socket_path,
            &self.process_identity,
        )?;
        crate::daemon::lifecycle::update_lifecycle_record(
            &self.env,
            &self.incarnation_hash,
            |record| {
                if record.process.as_ref() == Some(&self.process_identity) {
                    record.process = None;
                }
                Ok(())
            },
        )?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RuntimeDaemonCleanup {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn bootstrap_v2_runtime(
    coordinator: &ProductionV2Coordinator,
    mut leased: super::runtime::LeasedCanonicalPaneStateRuntime,
    env: &std::collections::BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<()> {
    let agent_state_root = crate::agent_state::state_root(env, &coordinator.incarnation.hash);
    let agent_runtime = crate::agent_state::runtime::AgentRuntime::open(
        agent_state_root.clone(),
        coordinator.incarnation.hash.clone(),
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "agent state {} is invalid: {error}; use the explicit agent storage recovery command",
            agent_state_root.display()
        )
    })?;
    *coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned") = Some(agent_runtime);
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3))
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
    crate::daemon::agent_dispatch::cleanup_stale_prompt_buffers(&runner).map_err(|error| {
        anyhow::anyhow!("failed to clean stale guarded prompt buffers: {error}")
    })?;
    crate::daemon::view_hooks::install_hooks(&runner, &coordinator.incarnation.identity)
        .map_err(|error| anyhow::anyhow!("failed to install pane-state hooks: {error}"))?;
    coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .begin_hydration()
        .map_err(anyhow::Error::msg)?;

    let session_framing = crate::daemon::topology::QueryFraming::generate()?;
    let session_args = crate::daemon::topology::targeted_session_query_args(&session_framing);
    let session_refs = session_args.iter().map(String::as_str).collect::<Vec<_>>();
    let session_output = runner.run(&session_refs)?;
    let session_count = crate::daemon::topology::parse_session_count(
        &session_output,
        &session_framing,
        &coordinator.incarnation.identity,
    )?;
    let (topology, witnesses) = if session_count == 0 {
        (
            crate::daemon::topology::TopologySnapshot {
                server_identity: coordinator.incarnation.identity.clone(),
                panes: Vec::new(),
            },
            Vec::new(),
        )
    } else {
        let topology = query_full_topology(coordinator, Duration::from_secs(1))?;
        let observation = query_client_witnesses(coordinator, Duration::from_secs(1))?;
        (topology, observation.witnesses)
    };
    let snapshot_path =
        crate::pane_state::snapshot::snapshot_path(env, &coordinator.incarnation.hash);
    let mut records = crate::pane_state::snapshot::load_snapshot(
        &snapshot_path,
        &coordinator.incarnation.identity,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "pane snapshot {} is invalid: {error}; remove {} to reset all pane state",
            snapshot_path.display(),
            snapshot_path.display()
        )
    })?;
    let record_count = records.len();
    crate::pane_state::snapshot::retain_topology_records(
        &mut records,
        topology.panes.iter().map(|pane| pane.pane_instance.clone()),
    );
    let mut records_changed = records.len() != record_count;
    let reconciled_runs = coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned")
        .as_mut()
        .expect("agent runtime initialized before pane reconciliation")
        .reconcile_panes_after_restart(&records, epoch_seconds())
        .map_err(|error| anyhow::anyhow!("failed to reconcile durable runs at startup: {error}"))?;
    let mut latest_unread_order = records
        .values()
        .filter_map(|pane| pane.unread.latest.as_ref().map(|latest| latest.order))
        .max()
        .unwrap_or_default();
    for pane in records.values_mut() {
        let current = reconciled_runs
            .iter()
            .filter(|run| pane_belongs_to_run_epoch(pane, run))
            .max_by_key(|run| run.run_seq);
        let changed = if let Some(run) = current {
            pane.reconcile_current_run(
                crate::pane_state::CurrentDurableRunProjection {
                    run_id: run.run_id.as_str().to_string(),
                    run_seq: run.run_seq,
                    run_revision: run.revision,
                },
                run.execution_active(),
                run.updated_at,
                latest_unread_order,
            )
        } else {
            pane.clear_current_run()
        }
        .map_err(|error| {
            anyhow::anyhow!("failed to repair pane durable run projection: {error}")
        })?;
        latest_unread_order = latest_unread_order.max(
            pane.unread
                .latest
                .as_ref()
                .map(|latest| latest.order)
                .unwrap_or_default(),
        );
        records_changed |= changed;
    }
    if records_changed {
        crate::pane_state::snapshot::save_snapshot(
            &snapshot_path,
            &coordinator.incarnation.identity,
            &records,
        )?;
    }
    leased.hydrate(records)?;
    let mut views = crate::daemon::view_hooks::CurrentClientViews::default();
    let mut window_panes = BTreeMap::<String, Vec<PaneInstance>>::new();
    for pane in &topology.panes {
        window_panes
            .entry(pane.window_id.clone())
            .or_default()
            .push(pane.pane_instance.clone());
    }
    views
        .reconcile(&witnesses, &window_panes)
        .map_err(|error| anyhow::anyhow!("failed to build initial view registry: {error}"))?;
    let status_metadata =
        query_status_projection_metadata(coordinator, Duration::from_secs(1), &witnesses)?;
    let state_path = crate::sidebar::store::state_path(env, &coordinator.incarnation.socket_path);
    let mut sidebar_preferences = crate::sidebar::store::load_state(&state_path)?;
    let present_panes = topology
        .panes
        .iter()
        .map(|pane| pane.pane_instance.clone())
        .collect::<BTreeSet<_>>();
    if sidebar_preferences.retain_panes(&present_panes) {
        crate::sidebar::store::save_state(&state_path, &sidebar_preferences)?;
    }
    let category_state_path =
        crate::category::store::state_path(env, &coordinator.incarnation.socket_path);
    let category_state = crate::category::store::load_state(&category_state_path)?;
    let mut canonical = super::runtime::CanonicalCoordinatorState::new(
        leased,
        topology,
        views,
        sidebar_preferences,
    );
    canonical.status_metadata = status_metadata;
    canonical.category_state = category_state;
    canonical.projection_config = config.clone();
    *coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned") = Some(canonical);
    let daemon_socket =
        crate::daemon::daemon_socket_path_for_incarnation(env, None, &coordinator.incarnation.hash);
    crate::daemon::lifecycle::publish_runtime_context(
        &runner,
        &coordinator.incarnation,
        &daemon_socket,
    )?;

    let mut initial_reconciliation_queued = false;
    loop {
        let queued = coordinator
            .router
            .lock()
            .expect("v2 router lock poisoned")
            .take_bootstrap_fifo();
        for mutation in queued {
            let accepted_seq = mutation.accepted_seq;
            let response = apply_production_mutation(coordinator, mutation);
            coordinator.publish_resolved_snapshot()?;
            if !coordinator.is_deferred_response(accepted_seq) {
                coordinator.complete(accepted_seq, response);
            }
        }
        if !initial_reconciliation_queued {
            let mut router = coordinator.router.lock().expect("v2 router lock poisoned");
            for mutation in [
                V2InternalMutation::RefreshTopology,
                V2InternalMutation::ReconcileViews,
            ] {
                if matches!(router.accept_internal(mutation), V2Route::Fatal(_)) {
                    bail!("accepted sequence overflow during initial reconciliation");
                }
            }
            initial_reconciliation_queued = true;
            continue;
        }
        coordinator.publish_resolved_snapshot()?;
        coordinator.drive_status_push(StatusPushTrigger::Snapshot)?;
        let mut router = coordinator.router.lock().expect("v2 router lock poisoned");
        if router.enter_serving_if_bootstrap_empty() {
            drop(router);
            // Attaching earlier would let a bootstrap failure emit a client-detached hook that
            // starts another enabled daemon. Serving is the first point at which the control
            // client's lifecycle is allowed to affect tmux hooks.
            coordinator.start_tmux_control();
            break;
        }
    }
    Ok(())
}

fn initial_view_reconciliation(coordinator: &ProductionV2Coordinator) -> Result<()> {
    let through_unread_order = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_ref()
        .map_or(0, |state| state.leased.runtime.latest_unread_order());
    let observation = match query_client_witnesses(coordinator, Duration::from_millis(250)) {
        Ok(observation) => observation,
        Err(error) if error.requires_daemon_exit() => return Err(error.into()),
        Err(error) => {
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            if let Some(state) = state_guard.as_mut()
                && let Some(pane) = state.topology.panes.first()
            {
                state.leased.runtime.add_diagnostic(
                    pane.pane_instance.clone(),
                    format!("initial_view_reconciliation_failed: {error}"),
                )?;
            }
            return Ok(());
        }
    };
    reconcile_views_with_witnesses(
        coordinator,
        observation.seq,
        &observation.witnesses,
        through_unread_order,
        None,
        None,
    )
}

fn reconcile_views_with_witnesses(
    coordinator: &ProductionV2Coordinator,
    observation_seq: u64,
    witnesses: &[crate::pane_state::ClientWitness],
    through_unread_order: u64,
    _observation_bases: Option<
        &BTreeMap<PaneInstance, Option<crate::pane_state::StoredStateDescriptor>>,
    >,
    view_base: Option<&crate::daemon::view_hooks::CurrentClientViews>,
) -> Result<()> {
    let daemon_instance_id = coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .daemon_instance_id()
        .clone();
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before reconciliation");
    if !observation_view_base_matches(&state.views, view_base) {
        return Ok(());
    }
    state.reconcile_peek_leases(witnesses, observation_seq);
    let read_event_id = EventId::generate()?;
    let focused_panes = state.read_authorized_panes(witnesses);
    let pane_reads = crate::daemon::view_hooks::pane_read_envelopes_for_panes(
        &daemon_instance_id,
        &read_event_id,
        &focused_panes,
        through_unread_order,
        &state.records_snapshot(),
    )?;
    let window_panes = state.window_panes();
    let revision_before = state.leased.runtime.snapshot_revision();
    if !pane_reads.is_empty() {
        let read_panes = pane_reads
            .iter()
            .map(|envelope| envelope.pane_instance.clone())
            .collect::<BTreeSet<_>>();
        let mut io = pane_snapshot_store(coordinator);
        state
            .leased
            .runtime
            .apply_pane_reads(&mut io, &pane_reads)?;
        state.clear_peeks_for_read_panes(&read_panes);
    }
    let mut next_views = state.views.clone();
    let registry_changed = crate::daemon::view_hooks::reconcile_current_views(
        &mut next_views,
        witnesses,
        &window_panes,
    )?;
    state.views = next_views;
    state.leased.runtime.finish_sequenced_projection(
        None,
        std::iter::empty(),
        registry_changed,
        revision_before,
    )?;
    Ok(())
}

fn observation_view_base_matches(
    current: &crate::daemon::view_hooks::CurrentClientViews,
    observation_base: Option<&crate::daemon::view_hooks::CurrentClientViews>,
) -> bool {
    observation_base.is_none_or(|base| current == base)
}

fn bind_daemon_listener(
    socket_path: &Path,
) -> Result<
    Option<(
        UnixListener,
        crate::daemon::lifecycle::DaemonFileLock,
        BoundDaemonSocketCleanup,
    )>,
> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    let Some(instance_lock) =
        crate::daemon::lifecycle::try_acquire_daemon_instance_lock(socket_path)?
    else {
        return Ok(None);
    };
    if socket_path.exists() {
        crate::daemon::lifecycle::verify_stale_socket_can_be_removed(
            socket_path,
            Instant::now() + Duration::from_secs(3),
        )?;
        fs::remove_file(socket_path)
            .with_context(|| format!("failed to remove {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    let socket_cleanup = BoundDaemonSocketCleanup::new(socket_path)?;
    Ok(Some((listener, instance_lock, socket_cleanup)))
}

fn install_shutdown_signal_handler(coordinator: Arc<ProductionV2Coordinator>) -> Result<()> {
    let mut fds = [0; 2];
    // SAFETY: `pipe` writes two valid file descriptors into `fds` on success.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!(
            "failed to create shutdown signal pipe: {}",
            std::io::Error::last_os_error()
        );
    }
    SHUTDOWN_SIGNAL_WRITE_FD.store(fds[1], Ordering::SeqCst);
    install_shutdown_signal(libc::SIGTERM)?;
    install_shutdown_signal(libc::SIGINT)?;
    // SAFETY: `fds[0]` is a fresh read end returned by `pipe` and is now owned by `File`.
    let reader = unsafe { fs::File::from_raw_fd(fds[0]) };
    spawn_shutdown_forwarder(reader, coordinator);
    Ok(())
}

fn install_shutdown_signal(signum: libc::c_int) -> Result<()> {
    // SAFETY: zeroed `sigaction` is immediately initialized with a handler, empty mask, and flags.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = shutdown_signal_handler as *const () as usize;
    action.sa_flags = 0;
    // SAFETY: `action.sa_mask` is a valid signal set field to initialize.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    // SAFETY: `sigaction` installs a plain async-signal-safe handler for the given signal.
    if unsafe { libc::sigaction(signum, &action, std::ptr::null_mut()) } != 0 {
        bail!(
            "failed to install signal handler for {signum}: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

extern "C" fn shutdown_signal_handler(_signum: libc::c_int) {
    let fd = SHUTDOWN_SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = [1_u8];
    // SAFETY: `write` is async-signal-safe; fd is the stored pipe write end.
    unsafe {
        let _ = libc::write(fd, byte.as_ptr().cast(), byte.len());
    }
}

fn spawn_shutdown_forwarder<R>(mut reader: R, coordinator: Arc<ProductionV2Coordinator>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut byte = [0_u8; 1];
        if reader.read(&mut byte).is_ok() {
            coordinator.begin_signal_shutdown();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::v2::PROTOCOL_VERSION;

    #[test]
    fn connection_thread_limiter_enforces_cap_and_releases_slots() {
        let limiter = Arc::new(V2ConnectionThreadLimiter::new(2, 1));
        let first = limiter.try_acquire().expect("first connection fits");
        let second = limiter.try_acquire().expect("second connection fits");
        assert!(limiter.try_acquire().is_none());

        drop(first);
        let replacement = limiter.try_acquire();
        assert!(replacement.is_some());

        drop(second);
        drop(replacement);
        assert_eq!(limiter.counts.lock().expect("limiter lock").active, 0);
    }

    #[test]
    fn connection_overload_returns_queue_full_without_a_handler_thread() {
        let (mut server, client) = UnixStream::pair().unwrap();
        write_v2_overload_response(&mut server);

        let mut response = String::new();
        BufReader::new(client).read_line(&mut response).unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(&response).unwrap(),
            ServerMessage::Error {
                code: ErrorCode::QueueFull,
                ..
            }
        ));
    }

    #[test]
    fn connection_thread_permit_releases_during_unwind() {
        let limiter = Arc::new(V2ConnectionThreadLimiter::new(1, 0));
        let permit = limiter.try_acquire().expect("connection fits");

        let result = std::panic::catch_unwind(move || {
            let _permit = permit;
            panic!("simulated connection handler panic");
        });

        assert!(result.is_err());
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn streaming_connections_leave_reserved_non_streaming_capacity() {
        let limiter = Arc::new(V2ConnectionThreadLimiter::new(4, 1));
        let mut streaming = (0..3)
            .map(|_| limiter.try_acquire().expect("streaming connection fits"))
            .collect::<Vec<_>>();
        assert!(
            streaming
                .iter_mut()
                .all(|permit| permit.try_mark_streaming())
        );

        let mut reserved = limiter.try_acquire().expect("reserved connection fits");
        assert!(!reserved.try_mark_streaming());
        assert!(limiter.try_acquire().is_none());

        drop(streaming.pop());
        assert!(reserved.try_mark_streaming());
    }
    const V2_EVENT_ID: &str = "102132435465768798a9bacbdcedfe0f";
    const V2_DAEMON_ID: &str = "ffeeddccbbaa99887766554433221100";
    const POLL_TOKEN: &str = "00112233445566778899aabbccddeeff";

    fn codex_provider_test_event(
        daemon_instance_id: DaemonInstanceId,
        pane_instance: PaneInstance,
        event: &str,
        payload: &str,
        observed_at: i64,
    ) -> (
        PaneEventEnvelope,
        crate::hook::provider::ProviderObservation,
    ) {
        use crate::hook::provider::ProviderHookKind;
        use crate::pane_state::{AgentSessionSource, PromptState, ResponseState};

        let event_id = EventId::generate().unwrap();
        let observation = crate::hook::provider::observation_from_json(
            "codex",
            event,
            payload,
            event_id.clone(),
            observed_at,
        )
        .unwrap()
        .unwrap();
        let payload: serde_json::Value = serde_json::from_str(payload).unwrap();
        let pane_event = match observation.hook_kind {
            ProviderHookKind::SessionStart => PaneEvent::AgentSessionStarted {
                observed_at,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
            ProviderHookKind::UserPromptSubmit => PaneEvent::BeginRun {
                started_at: observed_at,
                prompt: payload
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .map(|text| PromptState {
                        text: text.to_string(),
                        source: "user".to_string(),
                        digest: observation.prompt_digest.clone(),
                    }),
            },
            ProviderHookKind::Stop => PaneEvent::ResponseAndCompleteRun {
                completed_at: observed_at,
                response: ResponseState {
                    text: payload
                        .get("last_assistant_message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    observed_at,
                },
            },
            _ => panic!("unsupported provider test hook {event}"),
        };
        (
            PaneEventEnvelope {
                daemon_instance_id,
                event_id,
                pane_instance,
                agent: Some(observation.provider.clone()),
                agent_session_id: Some(observation.session_id.clone()),
                event: pane_event,
            },
            observation,
        )
    }

    #[test]
    fn provider_hook_kind_must_match_the_pane_transition_before_run_mutation() {
        let observation = crate::hook::provider::observation_from_json(
            "codex",
            "UserPromptSubmit",
            r#"{"session_id":"session","turn_id":"turn","prompt":"hello"}"#,
            EventId::parse(V2_EVENT_ID).unwrap(),
            10,
        )
        .unwrap()
        .unwrap();
        let begin = PaneEvent::BeginRun {
            started_at: 10,
            prompt: Some(crate::pane_state::PromptState {
                text: "hello".to_string(),
                source: "user".to_string(),
                digest: observation.prompt_digest.clone(),
            }),
        };
        assert!(provider_event_matches_pane_event(&observation, &begin));
        assert!(!provider_event_matches_pane_event(
            &observation,
            &PaneEvent::CompleteRun { completed_at: 10 }
        ));

        let stop = crate::hook::provider::observation_from_json(
            "codex",
            "Stop",
            r#"{"session_id":"session","turn_id":"turn","last_assistant_message":"done"}"#,
            EventId::parse(V2_EVENT_ID).unwrap(),
            11,
        )
        .unwrap()
        .unwrap();
        assert!(provider_event_matches_pane_event(
            &stop,
            &PaneEvent::ResponseAndCompleteRun {
                completed_at: 11,
                response: crate::pane_state::ResponseState {
                    text: "done".to_string(),
                    observed_at: 11,
                },
            }
        ));
        assert!(!provider_event_matches_pane_event(
            &stop,
            &PaneEvent::CompleteRun { completed_at: 11 }
        ));
    }

    #[test]
    fn claude_provider_observation_is_rejected_before_mutation() {
        let root = test_root("claude-provider-rejection");
        let coordinator = test_coordinator(&root, "c".repeat(64));
        let observation = crate::hook::provider::observation_from_json(
            "claude",
            "Stop",
            r#"{"session_id":"session","prompt_id":"prompt","last_assistant_message":"private"}"#,
            v2_event_id(),
            1,
        )
        .unwrap()
        .unwrap();
        let envelope = PaneEventEnvelope {
            daemon_instance_id: v2_daemon_id(),
            event_id: v2_event_id(),
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 100,
            },
            agent: Some(crate::pane_state::AgentKind::parse("claude").unwrap()),
            agent_session_id: Some(crate::pane_state::AgentSessionId::parse("session").unwrap()),
            event: PaneEvent::ResponseAndCompleteRun {
                completed_at: 1,
                response: crate::pane_state::ResponseState {
                    text: "private".to_string(),
                    observed_at: 1,
                },
            },
        };

        assert!(matches!(
            apply_external_provider_event(&coordinator, 1, envelope, observation),
            ServerMessage::Error {
                code: ErrorCode::UnsupportedProvider,
                ..
            }
        ));
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn immediate_codex_prompt_retries_transient_process_scan_after_session_start() {
        use crate::agent_state::{ExecutionPhase, SemanticOutcome};
        use crate::pane_state::{AgentProcessIdentity, LifecycleState};

        let root = test_root("codex-session-start-process-race");
        let hash = "codex-session-start-process-race";
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let coordinator =
            ProductionV2Coordinator::new(test_incarnation(&root, hash), env, None).unwrap();
        install_test_state(
            &coordinator,
            &root,
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );
        *coordinator.agent_runtime.lock().unwrap() = Some(
            crate::agent_state::runtime::AgentRuntime::open(
                root.join("agent-state"),
                hash.to_string(),
            )
            .unwrap(),
        );
        let pane = PaneInstance {
            pane_id: "%537".to_string(),
            pane_pid: 53_700,
        };
        let daemon_instance_id = coordinator
            .router
            .lock()
            .unwrap()
            .daemon_instance_id()
            .clone();
        let make_event = |event: &str, payload: &str, observed_at: i64| {
            codex_provider_test_event(
                daemon_instance_id.clone(),
                pane.clone(),
                event,
                payload,
                observed_at,
            )
        };
        let runner = crate::tmux::mock::MockTmuxRunner::new();

        let (session_envelope, session_observation) = make_event(
            "SessionStart",
            r#"{"session_id":"session-537","source":"startup"}"#,
            1,
        );
        assert!(matches!(
            apply_external_provider_event_with_runner(
                &coordinator,
                1,
                session_envelope,
                session_observation,
                &runner,
            ),
            ServerMessage::PaneEventResult { .. }
        ));
        let started = coordinator
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .leased
            .runtime
            .record(&pane)
            .unwrap()
            .clone();
        assert!(started.agent_process.is_none());
        assert!(!started.scan_verified);

        let process = AgentProcessIdentity {
            pid: 53_762,
            start_token: "codex-process-start".to_string(),
        };
        runner.stub_agent_process_sequence(
            pane.pane_pid,
            "codex",
            [
                Ok(None),
                Err("transient process identity race".to_string()),
                Ok(Some(process.clone())),
            ],
        );
        let (prompt_envelope, prompt_observation) = make_event(
            "UserPromptSubmit",
            r#"{"session_id":"session-537","turn_id":"turn-1","prompt":"hello"}"#,
            2,
        );
        assert!(matches!(
            apply_external_provider_event_with_runner(
                &coordinator,
                2,
                prompt_envelope,
                prompt_observation.clone(),
                &runner,
            ),
            ServerMessage::PaneEventResult { .. }
        ));
        let running = coordinator
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .leased
            .runtime
            .record(&pane)
            .unwrap()
            .clone();
        assert_eq!(running.agent_process.as_ref(), Some(&process));
        assert!(running.scan_verified);
        assert_eq!(running.run_seq, 1);
        assert!(matches!(running.lifecycle, LifecycleState::Running));
        assert!(running.current_run.is_some());
        assert_eq!(
            running.prompt.as_ref().map(|prompt| prompt.text.as_str()),
            Some("hello")
        );
        assert_eq!(running.task_context.recent_prompts, ["hello"]);
        let durable_run = coordinator
            .agent_runtime
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .provider_event_run(&prompt_observation)
            .unwrap()
            .unwrap();
        assert_eq!(durable_run.execution_phase, ExecutionPhase::Running);
        assert_eq!(durable_run.semantic_outcome, SemanticOutcome::Unresolved);

        let (steer_envelope, steer_observation) = make_event(
            "UserPromptSubmit",
            r#"{"session_id":"session-537","turn_id":"turn-1","prompt":"review with pane five"}"#,
            3,
        );
        assert!(matches!(
            apply_external_provider_event_with_runner(
                &coordinator,
                3,
                steer_envelope,
                steer_observation.clone(),
                &runner,
            ),
            ServerMessage::PaneEventResult { .. }
        ));
        let steered = coordinator
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .leased
            .runtime
            .record(&pane)
            .unwrap()
            .clone();
        assert_eq!(steered.run_seq, 1);
        assert_eq!(
            steered.prompt.as_ref().map(|prompt| prompt.text.as_str()),
            Some("review with pane five")
        );
        assert_eq!(
            steered.task_context.recent_prompts,
            ["hello", "review with pane five"]
        );
        let steered_run = coordinator
            .agent_runtime
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .provider_event_run(&steer_observation)
            .unwrap()
            .unwrap();
        assert_eq!(steered_run.run_id, durable_run.run_id);
        assert_eq!(steered_run.run_seq, durable_run.run_seq);
        assert_eq!(
            steered_run
                .evidence
                .provider_events
                .last()
                .unwrap()
                .disposition,
            "prompt_updated"
        );

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_provider_process_refresh_remains_fail_closed() {
        let root = test_root("codex-process-refresh-fail-closed");
        let hash = "codex-process-refresh-fail-closed";
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let coordinator =
            ProductionV2Coordinator::new(test_incarnation(&root, hash), env, None).unwrap();
        install_test_state(
            &coordinator,
            &root,
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );
        *coordinator.agent_runtime.lock().unwrap() = Some(
            crate::agent_state::runtime::AgentRuntime::open(
                root.join("agent-state"),
                hash.to_string(),
            )
            .unwrap(),
        );
        let pane = PaneInstance {
            pane_id: "%538".to_string(),
            pane_pid: 53_800,
        };
        let daemon_instance_id = coordinator
            .router
            .lock()
            .unwrap()
            .daemon_instance_id()
            .clone();
        let make_event = |event: &str, payload: &str, observed_at: i64| {
            codex_provider_test_event(
                daemon_instance_id.clone(),
                pane.clone(),
                event,
                payload,
                observed_at,
            )
        };
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let (session_envelope, session_observation) = make_event(
            "SessionStart",
            r#"{"session_id":"session-538","source":"startup"}"#,
            1,
        );
        assert!(matches!(
            apply_external_provider_event_with_runner(
                &coordinator,
                1,
                session_envelope,
                session_observation,
                &runner,
            ),
            ServerMessage::PaneEventResult { .. }
        ));
        runner.stub_agent_process(pane.pane_pid, "codex", None);
        let (prompt_envelope, prompt_observation) = make_event(
            "UserPromptSubmit",
            r#"{"session_id":"session-538","turn_id":"turn-1","prompt":"hello"}"#,
            2,
        );
        assert!(matches!(
            apply_external_provider_event_with_runner(
                &coordinator,
                2,
                prompt_envelope,
                prompt_observation,
                &runner,
            ),
            ServerMessage::Error {
                code: ErrorCode::StaleAgentEvent,
                ..
            }
        ));
        let state = coordinator.state.lock().unwrap();
        let record = state
            .as_ref()
            .unwrap()
            .leased
            .runtime
            .record(&pane)
            .unwrap();
        assert!(record.agent_process.is_none());
        assert_eq!(record.run_seq, 0);
        assert!(record.current_run.is_none());

        drop(state);
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_projection_keeps_ui_previews_but_redacts_guarded_prompts() {
        let begin = PaneEvent::BeginRun {
            started_at: 1,
            prompt: Some(crate::pane_state::PromptState {
                text: "human prompt preview".to_string(),
                source: "user".to_string(),
                digest: Some("a".repeat(64)),
            }),
        };
        let mut public_begin = begin.clone();
        redact_private_provider_prompt(&mut public_begin, false);
        assert_eq!(public_begin, begin);

        let mut guarded_begin = begin;
        redact_private_provider_prompt(&mut guarded_begin, true);
        assert!(matches!(
            guarded_begin,
            PaneEvent::BeginRun { prompt: None, .. }
        ));

        let mut stop = PaneEvent::ResponseAndCompleteRun {
            completed_at: 2,
            response: crate::pane_state::ResponseState {
                text: "response preview".to_string(),
                observed_at: 2,
            },
        };
        let expected_stop = stop.clone();
        redact_private_provider_prompt(&mut stop, true);
        assert_eq!(stop, expected_stop);

        let mut progress = PaneEvent::ProgressUpdated {
            observed_at: 3,
            operations: vec![
                crate::pane_state::ProgressOperation::SetPrompt(crate::pane_state::PromptState {
                    text: "private progress prompt".to_string(),
                    source: "generic_hook".to_string(),
                    digest: None,
                }),
                crate::pane_state::ProgressOperation::TaskCreated,
            ],
        };
        redact_private_provider_prompt(&mut progress, true);
        assert!(matches!(
            progress,
            PaneEvent::ProgressUpdated { operations, .. }
                if operations == vec![crate::pane_state::ProgressOperation::TaskCreated]
        ));

        let mut report = PaneEvent::ExplicitStateReported {
            report: crate::pane_state::ExplicitStateReport {
                observed_at: 4,
                lifecycle: None,
                started_at: None,
                completed_at: None,
                prompt: Some(crate::pane_state::FieldUpdate::Set(
                    crate::pane_state::PromptState {
                        text: "private report prompt".to_string(),
                        source: "generic_hook".to_string(),
                        digest: None,
                    },
                )),
                tasks: None,
                subagents: None,
                attention: false,
            },
        };
        redact_private_provider_prompt(&mut report, true);
        assert!(matches!(
            report,
            PaneEvent::ExplicitStateReported { report } if report.prompt.is_none()
        ));
    }

    fn guarded_prompt_test_binding() -> crate::agent_state::AgentBinding {
        crate::agent_state::AgentBinding {
            server_identity: crate::daemon::topology::ServerIdentity {
                pid: std::process::id(),
                start_time: 424_242,
            },
            pane_instance: crate::pane_state::PaneInstance {
                pane_id: "%4242".to_string(),
                pane_pid: 42_424,
            },
            pane_state_id: crate::pane_state::StateId::parse("1".repeat(32)).unwrap(),
            agent_epoch: 1,
            agent_kind: crate::pane_state::AgentKind::parse("codex").unwrap(),
            provider_session_id: crate::pane_state::AgentSessionId::parse("session-guarded")
                .unwrap(),
            process: crate::pane_state::AgentProcessIdentity {
                pid: 52_424,
                start_token: "guarded-process-start".to_string(),
            },
        }
    }

    fn guarded_prompt_test_operation(
        binding: crate::agent_state::AgentBinding,
    ) -> crate::agent_state::OperationRecord {
        crate::agent_state::OperationRecord {
            state_format_version: crate::agent_state::PRIVATE_STATE_FORMAT_VERSION,
            generation: crate::agent_state::StateGeneration::parse("2".repeat(32)).unwrap(),
            operation_id: crate::agent_state::OperationId::parse("operation_guarded_test").unwrap(),
            revision: 1,
            request_fingerprint: crate::agent_state::Sha256Digest::of(b"request"),
            target_agent_ref: "vta1:guarded-target".to_string(),
            prompt_digest: crate::agent_state::Sha256Digest::of(b"prompt"),
            dispatch_option: "paste_enter".to_string(),
            expected_pane_version: crate::pane_state::StateVersion {
                state_id: binding.pane_state_id.clone(),
                agent_epoch: binding.agent_epoch,
                revision: 7,
            },
            expected_current_run: None,
            expected_run_seq: 4,
            confirmation_deadline_at: 20,
            dispatch_state: crate::agent_state::DispatchState::Prepared,
            run_id: None,
            result_receipt: None,
            created_at: 10,
            updated_at: 10,
            binding: binding.into(),
        }
    }

    fn guarded_prompt_test_pane_state(
        binding: &crate::agent_state::AgentBinding,
    ) -> crate::pane_state::PaneState {
        crate::pane_state::PaneState {
            schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
            state_id: binding.pane_state_id.clone(),
            revision: 7,
            pane_instance: binding.pane_instance.clone(),
            agent: binding.agent_kind.clone(),
            agent_session_id: Some(binding.provider_session_id.clone()),
            agent_process: Some(binding.process.clone()),
            agent_epoch: binding.agent_epoch,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: crate::pane_state::LifecycleState::Idle,
            run_seq: 3,
            current_run: None,
            completed_seq: 0,
            unread: crate::pane_state::UnreadState::default(),
            started_at: None,
            completed_at: None,
            prompt: None,
            latest_response: None,
            task_context: crate::pane_state::TaskContextState::default(),
            tasks: crate::pane_state::TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
        }
    }

    #[test]
    fn guarded_prompt_process_owner_rejections_cover_every_fail_closed_branch() {
        let binding = guarded_prompt_test_binding();
        let operation_binding = crate::agent_state::OperationBinding::from(binding.clone());
        let pane = binding.pane_instance.clone();

        let replaced = crate::tmux::mock::MockTmuxRunner::new();
        let mut other_process = binding.process.clone();
        other_process.pid += 1;
        replaced.stub_agent_process(
            pane.pane_pid,
            binding.agent_kind.as_str(),
            Some(other_process),
        );
        assert_eq!(
            verify_agent_prompt_process_and_owner(&replaced, &pane, &operation_binding)
                .unwrap_err()
                .code,
            "target_process_replaced"
        );

        let absent = crate::tmux::mock::MockTmuxRunner::new();
        absent.stub_agent_process(pane.pane_pid, binding.agent_kind.as_str(), None);
        assert_eq!(
            verify_agent_prompt_process_and_owner(&absent, &pane, &operation_binding)
                .unwrap_err()
                .code,
            "target_process_absent"
        );

        let unverifiable = crate::tmux::mock::MockTmuxRunner::new();
        assert_eq!(
            verify_agent_prompt_process_and_owner(&unverifiable, &pane, &operation_binding)
                .unwrap_err()
                .code,
            "target_process_unverifiable"
        );

        let not_owner = crate::tmux::mock::MockTmuxRunner::new();
        not_owner.stub_agent_process(
            pane.pane_pid,
            binding.agent_kind.as_str(),
            Some(binding.process.clone()),
        );
        not_owner.stub_agent_input_owner(pane.pane_pid, binding.process.pid, false);
        assert_eq!(
            verify_agent_prompt_process_and_owner(&not_owner, &pane, &operation_binding)
                .unwrap_err()
                .code,
            "agent_not_input_owner"
        );

        let accepted = crate::tmux::mock::MockTmuxRunner::new();
        accepted.stub_agent_process(
            pane.pane_pid,
            binding.agent_kind.as_str(),
            Some(binding.process.clone()),
        );
        accepted.stub_agent_input_owner(pane.pane_pid, binding.process.pid, true);
        verify_agent_prompt_process_and_owner(&accepted, &pane, &operation_binding).unwrap();
    }

    #[test]
    fn guarded_prompt_lock_binding_and_pane_preconditions_are_fail_closed() {
        let binding = guarded_prompt_test_binding();
        let pane = binding.pane_instance.clone();
        let first = acquire_agent_prompt_dispatch_lock(&binding.server_identity, &pane).unwrap();
        assert_eq!(
            acquire_agent_prompt_dispatch_lock(&binding.server_identity, &pane)
                .unwrap_err()
                .code,
            "dispatch_lock_busy"
        );
        drop(first);

        let operation = guarded_prompt_test_operation(binding.clone());
        let operation_binding = crate::agent_state::OperationBinding::from(binding.clone());
        assert!(prepared_operation_matches_target(
            &operation,
            &operation_binding,
            operation.expected_pane_version.clone(),
            None,
            operation.expected_run_seq,
        ));
        assert!(!prepared_operation_matches_target(
            &operation,
            &operation_binding,
            operation.expected_pane_version.clone(),
            None,
            operation.expected_run_seq + 1,
        ));
        assert_eq!(
            prepared_target_rejection_code(true, None),
            Some("target_no_longer_current")
        );
        assert_eq!(
            prepared_target_rejection_code(true, Some(false)),
            Some("binding_changed_before_dispatch")
        );
        assert_eq!(prepared_target_rejection_code(true, Some(true)), None);
        assert_eq!(prepared_target_rejection_code(false, None), None);

        let mut pane_state = guarded_prompt_test_pane_state(&binding);
        assert!(agent_prompt_precondition_matches(&pane_state, &operation));
        pane_state.revision += 1;
        assert!(!agent_prompt_precondition_matches(&pane_state, &operation));
    }

    #[test]
    fn nvim_marker_parser_ignores_empty_and_malformed_values() {
        let output = "%6\u{1f}94451\u{1f}68736\n%8\u{1f}95025\u{1f}\ninvalid\n";
        assert_eq!(
            parse_nvim_pane_markers(output),
            vec![NvimPaneMarker {
                pane_id: "%6".to_string(),
                pane_pid: 94451,
                process_pid: 68736,
            }]
        );
    }

    #[test]
    fn nvim_marker_cleanup_is_guarded_by_server_pane_and_marker_identity() {
        let marker = NvimPaneMarker {
            pane_id: "%6".to_string(),
            pane_pid: 94451,
            process_pid: 68736,
        };
        let command = stale_nvim_marker_cleanup_command(74133, &[marker]).unwrap();
        assert!(command.contains("#{==:#{pid},74133}"));
        assert!(command.contains("#{==:#{pane_pid},94451}"));
        assert!(command.contains("#{==:#{@vde_nvim_process_pid},68736}"));
        assert!(command.contains("set-option"));
        assert!(command.contains("-pu"));
        assert!(command.contains("%6"));
        assert!(command.contains(NVIM_PROCESS_PID_OPTION));
        assert!(stale_nvim_marker_cleanup_command(74133, &[]).is_none());
    }

    fn test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "vde-{label}-{}-{}",
            std::process::id(),
            EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_incarnation(
        root: &Path,
        hash: impl Into<String>,
    ) -> crate::daemon::lifecycle::TmuxServerIncarnation {
        crate::daemon::lifecycle::TmuxServerIncarnation {
            socket_path: root.join("tmux.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            },
            hash: hash.into(),
        }
    }

    fn test_coordinator(root: &Path, hash: impl Into<String>) -> ProductionV2Coordinator {
        ProductionV2Coordinator::new(test_incarnation(root, hash), BTreeMap::new(), None).unwrap()
    }

    fn initialized_test_coordinator(
        root: &Path,
        hash: impl Into<String>,
        views: crate::daemon::view_hooks::CurrentClientViews,
    ) -> ProductionV2Coordinator {
        let coordinator = test_coordinator(root, hash);
        install_test_state(&coordinator, root, views);
        coordinator
    }

    fn detached_test_coordinator(hash: impl Into<String>) -> ProductionV2Coordinator {
        test_coordinator(Path::new("/tmp/vde-test"), hash)
    }

    fn install_test_state(
        coordinator: &ProductionV2Coordinator,
        root: &Path,
        views: crate::daemon::view_hooks::CurrentClientViews,
    ) {
        let leased =
            super::super::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
                .unwrap();
        *coordinator.state.lock().unwrap() =
            Some(super::super::runtime::CanonicalCoordinatorState::new(
                leased,
                crate::daemon::topology::TopologySnapshot {
                    server_identity: coordinator.incarnation.identity.clone(),
                    panes: Vec::new(),
                },
                views,
                crate::sidebar::state::SidebarPreferences::default(),
            ));
    }

    fn read_peek_test_pane_state(
        pane_instance: PaneInstance,
        state_id: &str,
        order: u64,
    ) -> crate::pane_state::PaneState {
        crate::pane_state::PaneState {
            schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
            state_id: crate::pane_state::StateId::parse(state_id).unwrap(),
            revision: 1,
            pane_instance,
            agent: crate::pane_state::AgentKind::parse("codex").unwrap(),
            agent_session_id: Some(
                crate::pane_state::AgentSessionId::parse("read-peek-session").unwrap(),
            ),
            agent_process: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: crate::pane_state::LifecycleState::Idle,
            run_seq: 1,
            current_run: None,
            completed_seq: 1,
            unread: crate::pane_state::UnreadState {
                occurrence_seq: 1,
                read_seq: 0,
                latest: Some(crate::pane_state::UnreadOccurrence {
                    seq: 1,
                    order,
                    reason: crate::pane_state::UnreadReason::Completed,
                    occurred_at: 1,
                }),
            },
            started_at: Some(1),
            completed_at: Some(1),
            prompt: None,
            latest_response: None,
            task_context: crate::pane_state::TaskContextState::default(),
            tasks: crate::pane_state::TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
        }
    }

    fn read_peek_test_topology_pane(
        pane_instance: PaneInstance,
        active: bool,
    ) -> crate::daemon::topology::TopologyPane {
        crate::daemon::topology::TopologyPane {
            pane_instance,
            session_links: Vec::new(),
            window_id: "@1".to_string(),
            window_name: "peek".to_string(),
            current_path: "/tmp".to_string(),
            current_command: "codex".to_string(),
            pane_width: 80,
            active,
            editprompt_is_editor: false,
            editprompt_target_panes: Vec::new(),
            editprompt_editor_pane: None,
        }
    }

    fn read_peek_test_state(
        root: &Path,
    ) -> (
        super::super::runtime::CanonicalCoordinatorState,
        PaneInstance,
        PaneInstance,
    ) {
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let candidate = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        let mut leased =
            super::super::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
                .unwrap();
        leased
            .hydrate(BTreeMap::from([
                (
                    target.clone(),
                    read_peek_test_pane_state(
                        target.clone(),
                        "11111111111111111111111111111111",
                        1,
                    ),
                ),
                (
                    candidate.clone(),
                    read_peek_test_pane_state(
                        candidate.clone(),
                        "22222222222222222222222222222222",
                        2,
                    ),
                ),
            ]))
            .unwrap();
        let mut state = super::super::runtime::CanonicalCoordinatorState::new(
            leased,
            crate::daemon::topology::TopologySnapshot {
                server_identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                panes: vec![
                    read_peek_test_topology_pane(target.clone(), true),
                    read_peek_test_topology_pane(candidate.clone(), false),
                ],
            },
            crate::daemon::view_hooks::CurrentClientViews::default(),
            crate::sidebar::state::SidebarPreferences::default(),
        );
        assert!(state.begin_peek(10, target.clone(), [target.clone()], 1));
        state.activate_peek(10, 1, target.clone(), 0);
        (state, target, candidate)
    }

    fn read_peek_test_witness(
        client_pid: u32,
        pane: &PaneInstance,
    ) -> crate::pane_state::ClientWitness {
        crate::pane_state::ClientWitness {
            client_pid,
            session_id: format!("${client_pid}"),
            window_id: "@1".to_string(),
            active_pane: pane.clone(),
            control_mode: false,
            active_pane_flag: false,
        }
    }

    struct ReadPeekStoreIo {
        fail: bool,
    }

    impl crate::pane_state::snapshot::PaneSnapshotStoreIo for ReadPeekStoreIo {
        fn save(
            &mut self,
            _records: &BTreeMap<PaneInstance, crate::pane_state::PaneState>,
        ) -> Result<(), crate::pane_state::store::StoreError> {
            if self.fail {
                Err(crate::pane_state::store::StoreError::PersistFailed(
                    "injected read failure".to_string(),
                ))
            } else {
                Ok(())
            }
        }
    }

    fn emit_read_peek_waiting_occurrence(
        state: &mut super::super::runtime::CanonicalCoordinatorState,
        target: &PaneInstance,
    ) {
        for event in [
            PaneEvent::BeginRun {
                started_at: 2,
                prompt: None,
            },
            PaneEvent::WaitRequested {
                observed_at: 3,
                reason: crate::pane_state::WaitReason::PermissionPrompt,
            },
        ] {
            apply_read_peek_event(
                state,
                target,
                event,
                &crate::pane_state::VisibilitySnapshot::default(),
            );
        }
    }

    fn apply_read_peek_event(
        state: &mut super::super::runtime::CanonicalCoordinatorState,
        target: &PaneInstance,
        event: PaneEvent,
        visibility: &crate::pane_state::VisibilitySnapshot,
    ) {
        state
            .leased
            .runtime
            .apply_event(
                &mut ReadPeekStoreIo { fail: false },
                &PaneEventEnvelope {
                    daemon_instance_id: v2_daemon_id(),
                    event_id: EventId::generate().unwrap(),
                    pane_instance: target.clone(),
                    agent: Some(crate::pane_state::AgentKind::parse("codex").unwrap()),
                    agent_session_id: Some(
                        crate::pane_state::AgentSessionId::parse("read-peek-session").unwrap(),
                    ),
                    event,
                },
                visibility,
            )
            .unwrap();
    }

    #[test]
    fn duplicate_operator_resolution_repairs_a_failed_completed_pane_projection() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::agent_state::{
            AgentBinding, ExecutionPhase, PRIVATE_STATE_FORMAT_VERSION, ResolutionId,
            ResolutionKind, RunEvidenceSummary, RunRecord, RunResolution, SemanticOutcome,
            StableRunId, StateGeneration,
        };
        use crate::pane_state::{
            AgentKind, AgentProcessIdentity, AgentSessionId, CurrentDurableRunProjection,
            LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneState, StateId, TaskContextState,
            TaskState, UnreadState,
        };

        let root = test_root("operator-projection-repair");
        let hash = "operator-projection-repair-hash";
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let coordinator =
            ProductionV2Coordinator::new(test_incarnation(&root, hash), env.clone(), None).unwrap();
        let pane = PaneInstance {
            pane_id: "%7".to_string(),
            pane_pid: 77,
        };
        let state_id = StateId::parse("1".repeat(32)).unwrap();
        let run_id = StableRunId::parse("2".repeat(32)).unwrap();
        let agent = AgentKind::parse("codex").unwrap();
        let session_id = AgentSessionId::parse("session-projection-repair").unwrap();
        let process = AgentProcessIdentity {
            pid: 88,
            start_token: "process-start-token".to_string(),
        };
        let pane_state = PaneState {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            state_id: state_id.clone(),
            revision: 1,
            pane_instance: pane.clone(),
            agent: agent.clone(),
            agent_session_id: Some(session_id.clone()),
            agent_process: Some(process.clone()),
            agent_epoch: 1,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Running,
            run_seq: 1,
            current_run: Some(CurrentDurableRunProjection {
                run_id: run_id.as_str().to_string(),
                run_seq: 1,
                run_revision: 1,
            }),
            completed_seq: 0,
            unread: UnreadState::default(),
            started_at: Some(1),
            completed_at: None,
            prompt: None,
            latest_response: None,
            task_context: TaskContextState::default(),
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
        };
        let mut leased =
            super::super::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
                .unwrap();
        leased
            .hydrate(BTreeMap::from([(pane.clone(), pane_state.clone())]))
            .unwrap();
        *coordinator.state.lock().unwrap() =
            Some(super::super::runtime::CanonicalCoordinatorState::new(
                leased,
                crate::daemon::topology::TopologySnapshot {
                    server_identity: coordinator.incarnation.identity.clone(),
                    panes: Vec::new(),
                },
                crate::daemon::view_hooks::CurrentClientViews::default(),
                crate::sidebar::state::SidebarPreferences::default(),
            ));
        let run = RunRecord {
            state_format_version: PRIVATE_STATE_FORMAT_VERSION,
            generation: StateGeneration::parse("3".repeat(32)).unwrap(),
            run_id,
            run_seq: 1,
            revision: 2,
            binding: AgentBinding {
                server_identity: coordinator.incarnation.identity.clone(),
                pane_instance: pane.clone(),
                pane_state_id: state_id,
                agent_epoch: 1,
                agent_kind: agent,
                provider_session_id: session_id,
                process,
            },
            provider_turn_key: Some("turn-projection-repair".to_string()),
            operation_id: None,
            execution_phase: ExecutionPhase::Ended,
            semantic_outcome: SemanticOutcome::Completed,
            evidence: RunEvidenceSummary::default(),
            resolution: Some(RunResolution {
                resolution_id: ResolutionId::parse("resolution_projection_repair").unwrap(),
                kind: ResolutionKind::ProviderCompleted,
                resolved_at: 2,
                operator_audit: None,
            }),
            artifact: None,
            created_at: 1,
            updated_at: 2,
        };
        run.validate().unwrap();
        assert!(pane_needs_durable_run_projection(&pane_state, &run).unwrap());
        let mut newer_pane_run = pane_state.clone();
        newer_pane_run.run_seq = 2;
        newer_pane_run.current_run = Some(CurrentDurableRunProjection {
            run_id: "4".repeat(32),
            run_seq: 2,
            run_revision: 1,
        });
        assert!(!pane_needs_durable_run_projection(&newer_pane_run, &run).unwrap());
        let mut lagging_projection = pane_state.clone();
        lagging_projection.run_seq = 0;
        lagging_projection.current_run = None;
        assert!(pane_needs_durable_run_projection(&lagging_projection, &run).unwrap());
        let mut equal_sequence_without_pointer = pane_state.clone();
        equal_sequence_without_pointer.current_run = None;
        assert!(pane_needs_durable_run_projection(&equal_sequence_without_pointer, &run).unwrap());
        let mut conflicting_projection = pane_state.clone();
        conflicting_projection.current_run = Some(CurrentDurableRunProjection {
            run_id: "4".repeat(32),
            run_seq: 1,
            run_revision: 1,
        });
        assert!(pane_needs_durable_run_projection(&conflicting_projection, &run).is_err());

        let snapshot_dir = crate::daemon::lifecycle::incarnation_log_directory(&env, hash);
        std::fs::create_dir_all(snapshot_dir.parent().unwrap()).unwrap();
        std::fs::write(&snapshot_dir, b"block snapshot directory").unwrap();
        assert!(project_operator_completed_run(&coordinator, &run).is_err());
        {
            let state = coordinator.state.lock().unwrap();
            let unchanged = state
                .as_ref()
                .unwrap()
                .leased
                .runtime
                .record(&pane)
                .unwrap();
            assert!(matches!(unchanged.lifecycle, LifecycleState::Running));
            assert_eq!(unchanged.current_run.as_ref().unwrap().run_revision, 1);
        }

        std::fs::remove_file(&snapshot_dir).unwrap();
        std::fs::create_dir_all(&snapshot_dir).unwrap();
        std::fs::set_permissions(&snapshot_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        project_operator_completed_run(&coordinator, &run).unwrap();
        let repaired_revision = {
            let state = coordinator.state.lock().unwrap();
            let repaired = state
                .as_ref()
                .unwrap()
                .leased
                .runtime
                .record(&pane)
                .unwrap();
            assert!(matches!(repaired.lifecycle, LifecycleState::Idle));
            assert_eq!(repaired.current_run.as_ref().unwrap().run_revision, 2);
            repaired.revision
        };
        project_operator_completed_run(&coordinator, &run).unwrap();
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .leased
                .runtime
                .record(&pane)
                .unwrap()
                .revision,
            repaired_revision
        );

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_cleanup_removes_owned_socket_and_process_record_on_early_return() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let event_id = EventId::generate().unwrap();
        let root = PathBuf::from(format!(
            "/tmp/vrc-{}-{}",
            std::process::id(),
            &event_id.as_str()[..8]
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let metadata = std::fs::symlink_metadata(&socket).unwrap();
        let identity = crate::daemon::lifecycle::DaemonProcessIdentity {
            pid: std::process::id(),
            start_token: crate::daemon::lifecycle::process_start_token(std::process::id()).unwrap(),
            daemon_instance_id: V2_DAEMON_ID.to_string(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
        };
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let incarnation_hash = "runtime-cleanup-test";
        crate::daemon::lifecycle::update_lifecycle_record(&env, incarnation_hash, |record| {
            record.process = Some(identity.clone());
            Ok(())
        })
        .unwrap();

        drop(RuntimeDaemonCleanup::new(
            &env,
            incarnation_hash,
            &socket,
            identity,
        ));

        assert!(!socket.exists());
        assert!(
            crate::daemon::lifecycle::read_lifecycle_record(&env, incarnation_hash)
                .unwrap()
                .process
                .is_none()
        );
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn post_bind_initialization_failure_removes_socket_and_releases_instance_lock() {
        use std::os::unix::fs::PermissionsExt as _;

        let event_id = EventId::generate().unwrap();
        let root = PathBuf::from(format!(
            "/tmp/vpb-{}-{}",
            std::process::id(),
            &event_id.as_str()[..8]
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("daemon.sock");
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let incarnation_hash = "c".repeat(64);
        crate::daemon::lifecycle::update_lifecycle_record(&env, &incarnation_hash, |_| Ok(()))
            .unwrap();
        let lifecycle_path =
            crate::daemon::lifecycle::lifecycle_record_path(&env, &incarnation_hash);
        let malformed_record = b"{malformed lifecycle record\n";
        std::fs::write(&lifecycle_path, malformed_record).unwrap();

        let Some((listener, instance_lock, socket_cleanup)) =
            bind_daemon_listener(&socket).unwrap()
        else {
            panic!("test must acquire the daemon instance lock");
        };
        assert!(
            crate::daemon::lifecycle::try_acquire_daemon_instance_lock(&socket)
                .unwrap()
                .is_none()
        );

        let result = initialize_runtime_daemon_post_bind(
            &crate::config::Config::default(),
            &socket,
            &env,
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: root.join("tmux.sock"),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: incarnation_hash,
            },
            socket_cleanup,
        );
        let error = match result {
            Ok(_) => panic!("malformed lifecycle record must fail post-bind initialization"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("invalid lifecycle record"));
        assert!(!socket.exists());
        assert_eq!(std::fs::read(&lifecycle_path).unwrap(), malformed_record);

        drop(listener);
        drop(instance_lock);
        let reacquired = crate::daemon::lifecycle::try_acquire_daemon_instance_lock(&socket)
            .unwrap()
            .expect("instance lock must be released after the early return");
        drop(reacquired);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bound_socket_cleanup_preserves_replacement_socket() {
        let event_id = EventId::generate().unwrap();
        let root = PathBuf::from(format!(
            "/tmp/vbs-{}-{}",
            std::process::id(),
            &event_id.as_str()[..8]
        ));
        std::fs::create_dir(&root).unwrap();
        let socket = root.join("daemon.sock");
        let original_listener = UnixListener::bind(&socket).unwrap();
        let cleanup = BoundDaemonSocketCleanup::new(&socket).unwrap();
        std::fs::remove_file(&socket).unwrap();
        let replacement_listener = UnixListener::bind(&socket).unwrap();
        let replacement_identity = crate::daemon::lifecycle::daemon_process_identity(
            &socket,
            &DaemonInstanceId::parse(V2_DAEMON_ID.to_string()).unwrap(),
        )
        .unwrap();

        assert!(
            cleanup
                .verify_process_identity(&replacement_identity)
                .is_err()
        );

        drop(cleanup);

        assert!(socket.exists());
        drop(replacement_listener);
        std::fs::remove_file(&socket).unwrap();
        drop(original_listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn observation_poll_framing() -> ObservationPollFraming {
        ObservationPollFraming::from_query(
            crate::daemon::topology::QueryFraming::from_token(POLL_TOKEN).unwrap(),
        )
        .unwrap()
    }

    fn observation_poll_output(framing: &ObservationPollFraming) -> String {
        let field = framing.query.field_separator();
        let row = framing.query.row_separator();
        let identity = framing
            .query
            .identity_format()
            .replace("#{pid}", "123")
            .replace("#{start_time}", "456");
        let topology = [
            "$1", "main", "@1", "0", "1", "0", "window", "%1", "100", "/tmp", "zsh", "80", "1", "",
            "", "",
        ]
        .join(field);
        let status_session = [
            "__vde_sm_00112233445566778899aabbccddeeff__",
            "$1",
            "main",
            "work",
            "/tmp",
            "",
            "1",
            "10",
        ]
        .join(field);
        let status_window = [
            "__vde_wm_00112233445566778899aabbccddeeff__",
            "@1",
            "0",
            "1",
            "0",
        ]
        .join(field);
        let client = ["99", "$1", "@1", "%1", "100", "0", ""]
            .join(&format!("__vde_client_field_{POLL_TOKEN}__"));
        format!(
            "{identity}\n{topology}{row}\n{}\n{identity}\n{status_session}{row}\n{status_window}{row}\n{}\n__vde_client_identity_{POLL_TOKEN}__123:456\n{client}__vde_client_row_{POLL_TOKEN}__\n{}\n{}\n",
            framing.topology_end, framing.status_end, framing.client_end, framing.final_end
        )
    }

    fn empty_observation_poll_output(framing: &ObservationPollFraming) -> String {
        let identity = framing
            .query
            .identity_format()
            .replace("#{pid}", "123")
            .replace("#{start_time}", "456");
        format!(
            "{identity}\n{}\n{identity}\n{}\n__vde_client_identity_{POLL_TOKEN}__123:456\n{}\n{}\n",
            framing.topology_end, framing.status_end, framing.client_end, framing.final_end
        )
    }

    #[test]
    fn observation_poll_query_is_one_guarded_command_group() {
        let framing = observation_poll_framing();
        let args = framing.query_args();
        let rendered = args.join(" ");

        assert!(rendered.contains("list-panes"));
        assert!(rendered.contains("list-sessions"));
        assert!(rendered.contains("list-windows"));
        assert!(rendered.contains("list-clients"));
        assert_eq!(rendered.matches("#{>:#{server_sessions},0}").count(), 3);
        assert!(rendered.contains(&framing.topology_end));
        assert!(rendered.contains(&framing.status_end));
        assert!(rendered.contains(&framing.client_end));
        assert!(rendered.contains(&framing.final_end));
        assert!(!rendered.contains("capture-pane"));
    }

    #[test]
    fn observation_poll_parser_is_all_or_nothing() {
        let framing = observation_poll_framing();
        let identity = crate::daemon::topology::ServerIdentity {
            pid: 123,
            start_time: 456,
        };
        let output = observation_poll_output(&framing);
        let projection = parse_observation_poll_projection(&output, &framing, &identity).unwrap();

        assert_eq!(projection.topology.panes.len(), 1);
        assert_eq!(projection.status_metadata.sessions.len(), 1);
        assert_eq!(projection.status_metadata.windows.len(), 1);
        assert_eq!(projection.witnesses.len(), 1);

        let truncated = output.replace(&format!("{}\n", framing.final_end), "");
        assert!(matches!(
            parse_observation_poll_projection(&truncated, &framing, &identity),
            Err(ObservationPollQueryError::Framing(_))
        ));
        let malformed = output.replace("$1__vde_f_", "$1__broken_f_");
        assert!(parse_observation_poll_projection(&malformed, &framing, &identity).is_err());

        let empty = parse_observation_poll_projection(
            &empty_observation_poll_output(&framing),
            &framing,
            &identity,
        )
        .unwrap();
        assert!(empty.topology.panes.is_empty());
        assert!(empty.status_metadata.sessions.is_empty());
        assert!(empty.status_metadata.windows.is_empty());
        assert!(empty.witnesses.is_empty());

        let duplicated = output.replacen(
            &format!("{}\n", framing.topology_end),
            &format!("{}\n{}\n", framing.topology_end, framing.topology_end),
            1,
        );
        assert!(matches!(
            parse_observation_poll_projection(&duplicated, &framing, &identity),
            Err(ObservationPollQueryError::Framing(message))
                if message.contains("duplicated")
        ));
    }

    #[test]
    fn observation_poll_parser_rejects_oversized_combined_output() {
        let framing = observation_poll_framing();
        let identity = crate::daemon::topology::ServerIdentity {
            pid: 123,
            start_time: 456,
        };
        let mut output = observation_poll_output(&framing);
        output.push_str(
            &"x".repeat(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES - output.len() + 1),
        );

        assert!(matches!(
            parse_observation_poll_projection(&output, &framing, &identity),
            Err(ObservationPollQueryError::Topology(
                crate::daemon::topology::TopologyError::OutputTooLarge { .. }
            ))
        ));
    }

    #[test]
    fn stale_poll_view_base_blocks_full_replacement() {
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let view_base = crate::daemon::view_hooks::CurrentClientViews::default();
        let mut current = view_base.clone();
        current
            .reconcile(
                &[crate::pane_state::ClientWitness {
                    client_pid: 10,
                    session_id: "$1".to_string(),
                    window_id: "@1".to_string(),
                    active_pane: pane.clone(),
                    control_mode: false,
                    active_pane_flag: false,
                }],
                &BTreeMap::from([("@1".to_string(), vec![pane])]),
            )
            .unwrap();
        assert!(!observation_view_base_matches(&current, Some(&view_base)));
    }

    fn v2_daemon_id() -> DaemonInstanceId {
        DaemonInstanceId::parse(V2_DAEMON_ID).unwrap()
    }

    fn v2_event_id() -> EventId {
        EventId::parse(V2_EVENT_ID).unwrap()
    }

    fn v2_sidebar_command(command: crate::daemon::protocol::v2::SidebarCommand) -> ClientMessage {
        ClientMessage::SidebarCommand {
            proto: PROTOCOL_VERSION,
            daemon_instance_id: v2_daemon_id(),
            event_id: v2_event_id(),
            command,
        }
    }

    #[test]
    fn peek_protocol_origin_rejects_invalid_identity_candidates_and_bounds() {
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let invalid = PaneInstance {
            pane_id: "1".to_string(),
            pane_pid: 0,
        };
        let peek_with_zero_client =
            v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::PeekPane {
                pane_instance: pane.clone(),
                source_pane: pane.clone(),
                client_pid: 0,
            });
        assert!(matches!(
            validate_v2_origin(&peek_with_zero_client),
            Err(ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        let peek_with_invalid_pane =
            v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::PeekPane {
                pane_instance: invalid.clone(),
                source_pane: pane.clone(),
                client_pid: 10,
            });
        assert!(matches!(
            validate_v2_origin(&peek_with_invalid_pane),
            Err(ServerMessage::Error {
                code: ErrorCode::InvalidPaneInstance,
                ..
            })
        ));

        let read_with_duplicate =
            v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::ReadPeek {
                source_pane: pane.clone(),
                client_pid: 10,
                advance_candidates: vec![pane.clone(), pane.clone()],
            });
        assert!(matches!(
            validate_v2_origin(&read_with_duplicate),
            Err(ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));

        let read_with_invalid =
            v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::ReadPeek {
                source_pane: pane.clone(),
                client_pid: 10,
                advance_candidates: vec![invalid],
            });
        assert!(matches!(
            validate_v2_origin(&read_with_invalid),
            Err(ServerMessage::Error {
                code: ErrorCode::InvalidPaneInstance,
                ..
            })
        ));

        let read_above_bound =
            v2_sidebar_command(crate::daemon::protocol::v2::SidebarCommand::ReadPeek {
                source_pane: pane.clone(),
                client_pid: 10,
                advance_candidates: vec![pane; crate::pane_state::MAX_VIEW_PANES + 1],
            });
        assert!(matches!(
            validate_v2_origin(&read_above_bound),
            Err(ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));
    }

    fn v2_pane_event(event: PaneEvent) -> ClientMessage {
        ClientMessage::SubmitPaneEvent {
            proto: PROTOCOL_VERSION,
            envelope: PaneEventEnvelope {
                daemon_instance_id: v2_daemon_id(),
                event_id: v2_event_id(),
                pane_instance: PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 100,
                },
                agent: Some(crate::pane_state::AgentKind::parse("codex").unwrap()),
                agent_session_id: Some(
                    crate::pane_state::AgentSessionId::parse("session").unwrap(),
                ),
                event,
            },
        }
    }

    fn v2_begin() -> ClientMessage {
        v2_pane_event(PaneEvent::BeginRun {
            started_at: 1,
            prompt: None,
        })
    }

    fn v2_handshake(router: &mut V2Router, connection: &mut V2ConnectionState) {
        let route = router.route(
            connection,
            ClientMessage::Hello {
                proto: PROTOCOL_VERSION,
            },
        );
        assert!(matches!(
            route,
            V2Route::Response(ServerMessage::HelloAck {
                proto: PROTOCOL_VERSION,
                ..
            })
        ));
    }

    #[test]
    fn canonical_notification_worker_exports_blocked_environment() {
        let root = test_root("notification-worker");
        let output = root.join("env.txt");
        let command = format!(
            "printf '%s|%s|%s' \"$VDE_PANE_ID\" \"$VDE_AGENT\" \"$VDE_BADGE_STATE\" > '{}'",
            output.display()
        );
        let sender = start_notification_worker(command);
        sender
            .try_send(NotificationWorkerJob {
                pane_id: "%7".to_string(),
                agent: "codex".to_string(),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !std::fs::read_to_string(&output).is_ok_and(|contents| contents == "%7|codex|Blocked")
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "%7|codex|Blocked"
        );
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_notification_timeout_kills_descendant_processes() {
        let root = test_root("notification-timeout");
        let pid_file = root.join("child.pid");
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let sender = start_notification_worker_with_timeout_and_log(
            command,
            Duration::from_millis(100),
            None,
        );
        sender
            .try_send(NotificationWorkerJob {
                pane_id: "%7".to_string(),
                agent: "codex".to_string(),
            })
            .unwrap();
        let file_deadline = Instant::now() + Duration::from_secs(1);
        let pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < file_deadline,
                "notification descendant PID was not written"
            );
            thread::sleep(Duration::from_millis(10));
        };
        let exit_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let exists = unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if !exists {
                break;
            }
            assert!(
                Instant::now() < exit_deadline,
                "notification descendant survived timeout"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_notification_successful_leader_exit_kills_background_descendants() {
        let root = test_root("notification-background");
        let pid_file = root.join("child.pid");
        let command = format!("sleep 30 & echo $! > '{}'", pid_file.display());
        let sender =
            start_notification_worker_with_timeout_and_log(command, Duration::from_secs(2), None);
        sender
            .try_send(NotificationWorkerJob {
                pane_id: "%7".to_string(),
                agent: "codex".to_string(),
            })
            .unwrap();
        let file_deadline = Instant::now() + Duration::from_secs(1);
        let pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < file_deadline,
                "notification descendant PID was not written"
            );
            thread::sleep(Duration::from_millis(10));
        };
        let exit_deadline = Instant::now() + Duration::from_secs(1);
        while crate::daemon::lifecycle::process_start_token(pid).is_ok() {
            assert!(
                Instant::now() < exit_deadline,
                "notification descendant survived successful leader exit"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_notification_failure_is_written_to_private_incarnation_log() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("notification-log");
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "c".repeat(64);
        let sender = start_notification_worker_with_timeout_and_log(
            "exit 7".to_string(),
            Duration::from_secs(1),
            Some((env.clone(), hash.clone())),
        );
        sender
            .try_send(NotificationWorkerJob {
                pane_id: "%7".to_string(),
                agent: "codex".to_string(),
            })
            .unwrap();
        let path = crate::daemon::lifecycle::daemon_log_path(&env, &hash);
        let deadline = Instant::now() + Duration::from_secs(1);
        let contents = loop {
            if let Ok(contents) = std::fs::read_to_string(&path)
                && contents.contains("notification:")
            {
                break contents;
            }
            assert!(
                Instant::now() < deadline,
                "notification failure was not written"
            );
            thread::sleep(Duration::from_millis(10));
        };

        assert!(contents.contains("notification:"));
        assert!(contents.contains("exited with status"));
        assert!(contents.contains("pane %7"));
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let clear_deadline = Instant::now() + Duration::from_secs(1);
        while crate::daemon::lifecycle::read_lifecycle_record(&env, &hash)
            .is_ok_and(|record| record.active_notification.is_some())
        {
            assert!(
                Instant::now() < clear_deadline,
                "notification identity was not cleared"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_push_failure_uses_the_shared_daemon_log() {
        let root = test_root("status-push-log");
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "d".repeat(64);
        let coordinator =
            ProductionV2Coordinator::new(test_incarnation(&root, hash.clone()), env.clone(), None)
                .unwrap();

        coordinator.log_status_push_error("test failure");

        let contents =
            std::fs::read_to_string(crate::daemon::lifecycle::daemon_log_path(&env, &hash))
                .unwrap();
        assert!(contents.contains("status_push: test failure"));
        for dedicated in ["notification.log", "status-push.log", "pane-state-hook.log"] {
            assert!(!root.join("vde-tmux").join(&hash).join(dedicated).exists());
        }
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn production_fail_stop_marks_router_and_releases_waiters() {
        let coordinator = detached_test_coordinator("a".repeat(64));
        let (sender, receiver) = mpsc::channel();
        coordinator.waiters.lock().unwrap().insert(1, sender);

        coordinator.fail_stop("counter overflow");

        assert!(coordinator.router.lock().unwrap().is_fatal());
        assert!(coordinator.shutdown.load(Ordering::SeqCst));
        assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)).unwrap(),
            ServerMessage::Error {
                code: ErrorCode::InternalError,
                ..
            }
        ));
    }

    #[test]
    fn tmux_server_liveness_monitor_fail_stops_after_server_process_exits() {
        let root = test_root("tmux-server-liveness");
        let mut server = Command::new("sleep").arg("30").spawn().unwrap();
        let server_pid = server.id();
        let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation {
            socket_path: root.join("tmux.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: server_pid,
                start_time: 1,
            },
            hash: "b".repeat(64),
        };
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let coordinator = Arc::new(ProductionV2Coordinator::new(incarnation, env, None).unwrap());
        start_tmux_server_liveness_monitor(coordinator.clone()).unwrap();

        server.kill().unwrap();
        server.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "liveness monitor did not stop the daemon"
            );
            thread::sleep(Duration::from_millis(10));
        }

        assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
        let cleanup_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match std::fs::remove_dir_all(&root) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    assert!(
                        Instant::now() < cleanup_deadline,
                        "liveness monitor did not finish writing its fail-stop log"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to remove test state: {error}"),
            }
        }
    }

    #[test]
    fn observation_poll_store_fail_stop_reaches_coordinator() {
        let coordinator = detached_test_coordinator("b".repeat(64));

        let response = observation_poll_error_response(
            &coordinator,
            anyhow::Error::new(crate::pane_state::store::StoreError::FailStop(
                "projection invariant failed".to_string(),
            )),
        );

        assert!(coordinator.router.lock().unwrap().is_fatal());
        assert!(matches!(
            response,
            ServerMessage::Error {
                code: ErrorCode::InternalError,
                ..
            }
        ));
    }

    #[test]
    fn repeated_permission_wait_observation_skips_fresh_visibility_query() {
        use crate::pane_state::{
            AgentKind, AgentPresenceObservation, CaptureInference, CaptureObservation,
            CaptureTrackerSnapshot, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneInstance,
            PaneState, StateId, TaskState, UnreadState, WaitReason,
        };

        let agent = AgentKind::parse("codex").unwrap();
        let mut state = PaneState {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
            revision: 1,
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 101,
            },
            agent: agent.clone(),
            agent_session_id: None,
            agent_process: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Waiting {
                reason: WaitReason::PermissionPrompt,
            },
            run_seq: 1,
            current_run: None,
            completed_seq: 0,
            unread: UnreadState::default(),
            started_at: Some(1),
            completed_at: None,
            prompt: None,
            latest_response: None,
            task_context: crate::pane_state::TaskContextState::default(),
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
        };
        let permission_wait = CaptureObservation {
            inference: CaptureInference::PermissionWait {
                reason: WaitReason::PermissionPrompt,
            },
            observed_fingerprint: Some([1; 32]),
        };
        let tracker = CaptureTrackerSnapshot::default();
        let present = AgentPresenceObservation::Present(agent);

        assert!(!observation_may_create_unread(
            &state,
            &tracker,
            &present,
            Some(&permission_wait),
        ));

        state.lifecycle = LifecycleState::Error { reason: None };
        assert!(observation_may_create_unread(
            &state,
            &tracker,
            &present,
            Some(&permission_wait),
        ));

        state.lifecycle = LifecycleState::Waiting {
            reason: WaitReason::PermissionPrompt,
        };
        let absence_tracker = CaptureTrackerSnapshot {
            absence_count: 1,
            ..CaptureTrackerSnapshot::default()
        };
        assert!(observation_may_create_unread(
            &state,
            &absence_tracker,
            &AgentPresenceObservation::Absent,
            Some(&permission_wait),
        ));
    }

    #[test]
    fn disconnected_mutation_waiter_enqueues_sequenced_diagnostic() {
        let coordinator = detached_test_coordinator("d".repeat(64));
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(DaemonPhase::Serving);
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        coordinator.waiters.lock().unwrap().insert(3, sender);

        coordinator.complete(
            3,
            ServerMessage::SnapshotAck {
                event_id: v2_event_id(),
                accepted_seq: 3,
                snapshot_revision: 0,
            },
        );

        let queue = coordinator.queue.lock().unwrap();
        assert!(matches!(
            queue.items.front().map(|item| &item.sequenced.mutation),
            Some(V2AcceptedMutation::Internal(
                V2InternalMutation::DiagnosticProjection {
                    pane_instance: None,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn graceful_shutdown_releases_later_waiters_and_keeps_current_response() {
        let coordinator = detached_test_coordinator("b".repeat(64));
        let (current_tx, current_rx) = mpsc::channel();
        let (later_tx, later_rx) = mpsc::channel();
        coordinator
            .waiters
            .lock()
            .unwrap()
            .extend([(4, current_tx), (5, later_tx)]);

        coordinator.begin_graceful_shutdown(4);
        assert!(!coordinator.shutdown_ready.load(Ordering::SeqCst));

        assert!(matches!(
            later_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            ServerMessage::Error {
                code: ErrorCode::NotReady,
                ..
            }
        ));
        assert!(current_rx.try_recv().is_err());
        let response = coordinator.route_external(
            &mut V2ConnectionState::default(),
            ClientMessage::Hello {
                proto: PROTOCOL_VERSION,
            },
            0,
        );
        assert!(matches!(
            response,
            ServerMessage::Error {
                code: ErrorCode::NotReady,
                ..
            }
        ));
        assert!(!coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
        assert!(current_rx.try_recv().is_err());
        coordinator.complete(
            4,
            ServerMessage::ShutdownAccepted {
                event_id: v2_event_id(),
                accepted_seq: 4,
            },
        );
        assert!(matches!(
            current_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            ServerMessage::ShutdownAccepted {
                accepted_seq: 4,
                ..
            }
        ));
        coordinator.mark_shutdown_ready();
        assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
    }

    #[test]
    fn mutation_queue_capacity_logs_each_internal_drop() {
        let root = test_root("mutation-queue-capacity");
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let hash = "mutation-queue-capacity";
        let coordinator =
            ProductionV2Coordinator::new(test_incarnation(&root, hash), env.clone(), None).unwrap();
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(DaemonPhase::Serving);

        for _ in 0..V2_MUTATION_QUEUE_CAPACITY {
            assert!(coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
        }
        assert!(!coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
        assert!(!coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
        let log =
            std::fs::read_to_string(crate::daemon::lifecycle::daemon_log_path(&env, hash)).unwrap();
        assert_eq!(
            log.matches("sequenced mutation queue full: dropped internal mutation")
                .count(),
            2
        );

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observation_poll_burst_enqueues_one_batch_mutation() {
        for pane_count in [63, 256, 512] {
            let root = test_root(&format!("observation-burst-{pane_count}"));
            let server_identity = crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            };
            let coordinator = test_coordinator(&root, format!("observation-burst-{pane_count}"));
            coordinator
                .router
                .lock()
                .unwrap()
                .set_phase(DaemonPhase::Serving);
            let daemon_instance_id = coordinator
                .router
                .lock()
                .unwrap()
                .daemon_instance_id()
                .clone();

            let observations = (0..pane_count)
                .map(|index| PaneEventEnvelope {
                    daemon_instance_id: daemon_instance_id.clone(),
                    event_id: EventId::generate().unwrap(),
                    pane_instance: PaneInstance {
                        pane_id: format!("%{index}"),
                        pane_pid: 10_000 + index as u32,
                    },
                    agent: None,
                    agent_session_id: None,
                    event: PaneEvent::ObservationBatch {
                        base: None,
                        tracker_generation: 0,
                        observed_at: 1,
                        presence: crate::pane_state::AgentPresenceObservation::Unknown,
                        capture: None,
                        process: None,
                    },
                })
                .collect::<Vec<_>>();
            assert!(
                coordinator.enqueue_internal(V2InternalMutation::ObservationBatch(Box::new(
                    ObservationBatchPayload {
                        projection: Box::new(ObservationPollProjection {
                            observation_seq: 1,
                            topology: crate::daemon::topology::TopologySnapshot {
                                server_identity,
                                panes: Vec::new(),
                            },
                            status_metadata:
                                super::super::runtime::StatusProjectionMetadata::default(),
                            witnesses: Vec::new(),
                            observation_bases: BTreeMap::new(),
                            view_base: crate::daemon::view_hooks::CurrentClientViews::default(),
                            through_unread_order: 0,
                        }),
                        observations,
                        removals: Vec::new(),
                        diagnostics: Vec::new(),
                    }
                ),))
            );

            // One successful poll is exactly one queued mutation regardless of
            // pane count, and the payload preserves the per-pane events.
            let queue = coordinator.queue.lock().unwrap();
            assert_eq!(queue.items.len(), 1);
            match queue.items.front().map(|item| &item.sequenced.mutation) {
                Some(V2AcceptedMutation::Internal(V2InternalMutation::ObservationBatch(
                    payload,
                ))) => {
                    assert_eq!(payload.observations.len(), pane_count);
                    assert!(payload.removals.is_empty());
                    assert!(payload.diagnostics.is_empty());
                }
                other => panic!("expected one observation batch, found {other:?}"),
            }
            drop(queue);

            drop(coordinator);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn observation_batch_applies_all_stages_and_publishes_one_snapshot_build() {
        for pane_count in [0usize, 1, 62] {
            let root = test_root(&format!("batch-apply-{pane_count}"));
            let server_identity = crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            };
            let coordinator = test_coordinator(&root, format!("batch-apply-{pane_count:0>52}"));
            coordinator
                .router
                .lock()
                .unwrap()
                .set_phase(DaemonPhase::Serving);
            let daemon_instance_id = coordinator
                .router
                .lock()
                .unwrap()
                .daemon_instance_id()
                .clone();
            let leased = super::super::runtime::LeasedCanonicalPaneStateRuntime::acquire(
                &root.join("writer"),
            )
            .unwrap();
            *coordinator.state.lock().unwrap() =
                Some(super::super::runtime::CanonicalCoordinatorState::new(
                    leased,
                    crate::daemon::topology::TopologySnapshot {
                        server_identity: server_identity.clone(),
                        panes: Vec::new(),
                    },
                    crate::daemon::view_hooks::CurrentClientViews::default(),
                    crate::sidebar::state::SidebarPreferences::default(),
                ));

            let observations = (0..pane_count)
                .map(|index| PaneEventEnvelope {
                    daemon_instance_id: daemon_instance_id.clone(),
                    event_id: EventId::generate().unwrap(),
                    pane_instance: PaneInstance {
                        pane_id: format!("%{index}"),
                        pane_pid: 10_000 + index as u32,
                    },
                    agent: None,
                    agent_session_id: None,
                    event: PaneEvent::ObservationBatch {
                        base: None,
                        tracker_generation: 0,
                        observed_at: 1,
                        presence: crate::pane_state::AgentPresenceObservation::Unknown,
                        capture: None,
                        process: None,
                    },
                })
                .collect::<Vec<_>>();
            let response = apply_production_mutation(
                &coordinator,
                V2SequencedMutation {
                    accepted_seq: 1,
                    mutation: V2AcceptedMutation::Internal(V2InternalMutation::ObservationBatch(
                        Box::new(ObservationBatchPayload {
                            projection: Box::new(ObservationPollProjection {
                                observation_seq: 1,
                                topology: crate::daemon::topology::TopologySnapshot {
                                    server_identity: server_identity.clone(),
                                    panes: Vec::new(),
                                },
                                status_metadata:
                                    super::super::runtime::StatusProjectionMetadata::default(),
                                witnesses: Vec::new(),
                                observation_bases: BTreeMap::new(),
                                view_base: crate::daemon::view_hooks::CurrentClientViews::default(),
                                through_unread_order: 0,
                            }),
                            observations,
                            removals: Vec::new(),
                            diagnostics: vec![(None, "poll diagnostic".to_string())],
                        }),
                    )),
                },
            );
            let ServerMessage::SnapshotAck {
                snapshot_revision, ..
            } = response
            else {
                panic!("batch response for {pane_count} panes: {response:?}");
            };
            assert!(!coordinator.shutdown.load(Ordering::SeqCst));

            let published = coordinator.publish_resolved_snapshot().unwrap();
            assert_eq!(published.revision, snapshot_revision);

            drop(coordinator);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn sidebar_preference_intents_commit_serially_and_dedupe_event_ids() {
        let root = test_root("sidebar-intents");
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let socket_path = root.join("tmux.sock");
        let server_identity = crate::daemon::topology::ServerIdentity {
            pid: 1,
            start_time: 2,
        };
        let coordinator = ProductionV2Coordinator::new(
            test_incarnation(&root, "sidebar-intents"),
            env.clone(),
            None,
        )
        .unwrap();
        let leased =
            super::super::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
                .unwrap();
        *coordinator.state.lock().unwrap() =
            Some(super::super::runtime::CanonicalCoordinatorState::new(
                leased,
                crate::daemon::topology::TopologySnapshot {
                    server_identity,
                    panes: Vec::new(),
                },
                crate::daemon::view_hooks::CurrentClientViews::default(),
                crate::sidebar::state::SidebarPreferences::default(),
            ));
        let first_event = EventId::generate().unwrap();
        let second_event = EventId::generate().unwrap();

        let first = apply_sidebar_preference_intent(
            &coordinator,
            1,
            first_event.clone(),
            crate::sidebar::state::SidebarPreferenceIntent::SetDefaultFilter {
                filter: crate::sidebar::state::StatusFilter::DoneOnly,
            },
        );
        let second = apply_sidebar_preference_intent(
            &coordinator,
            2,
            second_event,
            crate::sidebar::state::SidebarPreferenceIntent::SetDefaultPresentationMode {
                presentation_mode: crate::sidebar::state::PresentationMode::Flat,
            },
        );
        let duplicate = apply_sidebar_preference_intent(
            &coordinator,
            3,
            first_event,
            crate::sidebar::state::SidebarPreferenceIntent::SetDefaultFilter {
                filter: crate::sidebar::state::StatusFilter::All,
            },
        );

        assert!(matches!(
            first,
            ServerMessage::SnapshotAck {
                snapshot_revision: 1,
                ..
            }
        ));
        assert!(matches!(
            second,
            ServerMessage::SnapshotAck {
                snapshot_revision: 2,
                ..
            }
        ));
        assert!(matches!(
            duplicate,
            ServerMessage::SnapshotAck {
                snapshot_revision: 2,
                ..
            }
        ));
        let persisted = crate::sidebar::store::load_state(&crate::sidebar::store::state_path(
            &env,
            &socket_path,
        ))
        .unwrap();
        assert_eq!(
            persisted.filter,
            crate::sidebar::state::StatusFilter::DoneOnly
        );
        assert_eq!(
            persisted.presentation_mode,
            crate::sidebar::state::PresentationMode::Flat
        );

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_pin_persists_outside_canonical_state_and_prunes_with_topology() {
        let root = test_root("sidebar-pane-pin");
        let coordinator = initialized_test_coordinator(
            &root,
            "pane-pin",
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        {
            let mut guard = coordinator.state.lock().unwrap();
            let state = guard.as_mut().unwrap();
            state
                .replace_topology(crate::daemon::topology::TopologySnapshot {
                    server_identity: coordinator.incarnation.identity.clone(),
                    panes: vec![crate::daemon::topology::TopologyPane {
                        pane_instance: target.clone(),
                        session_links: Vec::new(),
                        window_id: "@1".to_string(),
                        window_name: "main".to_string(),
                        current_path: "/tmp/app".to_string(),
                        current_command: "codex".to_string(),
                        pane_width: 80,
                        active: false,
                        editprompt_is_editor: false,
                        editprompt_target_panes: Vec::new(),
                        editprompt_editor_pane: None,
                    }],
                })
                .unwrap();
            let daemon_instance_id = coordinator
                .router
                .lock()
                .unwrap()
                .daemon_instance_id()
                .clone();
            state
                .leased
                .runtime
                .apply_event(
                    &mut pane_snapshot_store(&coordinator),
                    &PaneEventEnvelope {
                        daemon_instance_id,
                        event_id: EventId::generate().unwrap(),
                        pane_instance: target.clone(),
                        agent: Some(crate::pane_state::AgentKind::parse("codex").unwrap()),
                        agent_session_id: Some(
                            crate::pane_state::AgentSessionId::parse("pin-session").unwrap(),
                        ),
                        event: PaneEvent::BeginRun {
                            started_at: 1,
                            prompt: None,
                        },
                    },
                    &crate::pane_state::VisibilitySnapshot::default(),
                )
                .unwrap();
        }

        let response = apply_sidebar_preference_intent(
            &coordinator,
            1,
            EventId::generate().unwrap(),
            crate::sidebar::state::SidebarPreferenceIntent::SetPanePinned {
                pane_instance: target.clone(),
                pinned: true,
            },
        );
        assert!(matches!(response, ServerMessage::SnapshotAck { .. }));
        let state_path = crate::sidebar::store::state_path(
            &coordinator.env,
            &coordinator.incarnation.socket_path,
        );
        assert!(
            crate::sidebar::store::load_state(&state_path)
                .unwrap()
                .pinned_panes
                .contains(&target)
        );
        {
            let mut guard = coordinator.state.lock().unwrap();
            let state = guard.as_mut().unwrap();
            assert!(state.sidebar_preferences.pinned_panes.contains(&target));
            assert!(
                serde_json::to_value(state.leased.runtime.record(&target).unwrap())
                    .unwrap()["unread"]
                    .get("pinned")
                    .is_none()
            );
            state.topology.panes.clear();
            assert!(persist_pruned_sidebar_pins(&coordinator, state).unwrap());
            assert!(state.sidebar_preferences.pinned_panes.is_empty());
        }
        assert!(
            crate::sidebar::store::load_state(&state_path)
                .unwrap()
                .pinned_panes
                .is_empty()
        );

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sidebar_navigation_is_shared_in_snapshots_without_persisting_preferences() {
        let root = test_root("sidebar-navigation");
        let coordinator = initialized_test_coordinator(
            &root,
            "sidebar-navigation",
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );
        let selection = Some("chat::%1::101".to_string());

        let first = apply_sidebar_navigation(
            &coordinator,
            1,
            EventId::generate().unwrap(),
            selection.clone(),
            12,
            true,
        );
        let first_revision = match first {
            ServerMessage::SnapshotAck {
                snapshot_revision, ..
            } => snapshot_revision,
            other => panic!("unexpected navigation response: {other:?}"),
        };
        let snapshot = coordinator
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .resolved_snapshot();
        assert_eq!(snapshot.sidebar_model.navigation.revision, 1);
        assert_eq!(snapshot.sidebar_model.navigation.selection, selection);
        assert_eq!(snapshot.sidebar_model.navigation.scroll, 12);
        assert!(snapshot.sidebar_model.navigation.manual_scroll);

        let duplicate = apply_sidebar_navigation(
            &coordinator,
            2,
            EventId::generate().unwrap(),
            snapshot.sidebar_model.navigation.selection.clone(),
            12,
            true,
        );
        assert!(matches!(
            duplicate,
            ServerMessage::SnapshotAck {
                snapshot_revision,
                ..
            } if snapshot_revision == first_revision
        ));

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any())]
    fn live_scheduler_dedupes_targets_into_one_capture_and_fans_out() {
        let root = test_root("live-dedupe");
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        let target_a = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let target_b = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 200,
        };
        let coordinator = initialized_test_coordinator(
            &root,
            "live-dedupe".to_string(),
            live_views_showing(&source),
        );
        let io = MockLiveCaptureIo::new(coordinator.incarnation.identity.clone());

        let (_, mailbox_a1) =
            coordinator
                .live
                .register(source.clone(), target_a.clone(), Duration::from_secs(2));
        let (_, mailbox_a2) =
            coordinator
                .live
                .register(source.clone(), target_a.clone(), Duration::from_secs(2));
        let (_, mailbox_b) =
            coordinator
                .live
                .register(source.clone(), target_b.clone(), Duration::from_secs(2));

        run_live_preview_tick(&coordinator, &io, Instant::now());

        // Two distinct targets shared by three subscribers produce exactly one
        // capture request with one section per deduplicated target.
        assert_eq!(io.call_count(), 1);
        assert_eq!(io.calls.lock().unwrap()[0].len(), 2);
        let a1 = mailbox_a1.wait().expect("first subscriber receives a body");
        let a2 = mailbox_a2
            .wait()
            .expect("second subscriber receives a body");
        match (&a1, &a2) {
            (
                LivePush::Result {
                    live_revision: r1,
                    body: b1,
                    ..
                },
                LivePush::Result {
                    live_revision: r2,
                    body: b2,
                    ..
                },
            ) => {
                assert_eq!(r1, r2);
                assert_eq!(b1, b2);
            }
            other => panic!("expected two results, found {other:?}"),
        }
        assert!(matches!(mailbox_b.wait(), Some(LivePush::Result { .. })));

        // Nothing is due immediately after the tick, so no further capture runs.
        run_live_preview_tick(&coordinator, &io, Instant::now());
        assert_eq!(io.call_count(), 1);

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any())]
    fn live_scheduler_does_not_capture_hidden_sources() {
        let root = test_root("live-hidden");
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        // The registry views show a different pane, so the source is hidden.
        let other = PaneInstance {
            pane_id: "%8".to_string(),
            pane_pid: 800,
        };
        let coordinator =
            live_test_coordinator(&root, "live-hidden".to_string(), live_views_showing(&other));
        let io = MockLiveCaptureIo::new(coordinator.incarnation.identity.clone());

        let (_, mailbox) = coordinator
            .live
            .register(source, target, Duration::from_secs(2));

        run_live_preview_tick(&coordinator, &io, Instant::now());

        assert_eq!(io.call_count(), 0);
        assert!(matches!(
            mailbox.wait(),
            Some(LivePush::Unavailable {
                reason: LiveUnavailableReason::HiddenSource,
            })
        ));

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any())]
    fn live_pane_instance_mismatch_delivers_no_body() {
        let root = test_root("live-mismatch");
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let coordinator = live_test_coordinator(
            &root,
            "live-mismatch".to_string(),
            live_views_showing(&source),
        );
        let mut io = MockLiveCaptureIo::new(coordinator.incarnation.identity.clone());
        io.mismatch_targets.insert("%1".to_string());

        let (_, mailbox) = coordinator
            .live
            .register(source, target, Duration::from_secs(2));

        run_live_preview_tick(&coordinator, &io, Instant::now());

        assert!(matches!(
            mailbox.wait(),
            Some(LivePush::Unavailable {
                reason: LiveUnavailableReason::PaneInstanceMismatch,
            })
        ));
        assert!(coordinator.live.bodies.lock().unwrap().is_empty());

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any())]
    fn observation_piggyback_consumes_due_live_demand_before_the_scheduler_tick() {
        let root = test_root("live-piggyback");
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let coordinator = live_test_coordinator(
            &root,
            "live-piggyback".to_string(),
            live_views_showing(&source),
        );
        let io = MockLiveCaptureIo::new(coordinator.incarnation.identity.clone());
        let (_, mailbox) =
            coordinator
                .live
                .register(source, target.clone(), Duration::from_secs(2));

        // The observation worker consumes the due demand ahead of its own
        // capture, exactly as it does before a poll.
        let now = Instant::now();
        let targets = collect_due_live_targets(
            &coordinator,
            now,
            crate::daemon::workers::CAPTURE_COALESCE_WINDOW,
        );
        assert_eq!(targets, vec![target]);
        deliver_live_result(
            &coordinator,
            &targets,
            Ok(vec![crate::daemon::workers::LiveCaptureSection::Body(
                "from-observation\n".to_string(),
            )]),
        );
        assert!(matches!(mailbox.wait(), Some(LivePush::Result { .. })));

        // In the aligned default configuration the scheduler tick that follows
        // finds no remaining due demand, so live preview needs no extra tmux
        // process of its own.
        run_live_preview_tick(&coordinator, &io, now + Duration::from_millis(5));
        assert_eq!(io.call_count(), 0);

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any())]
    fn live_subscription_rejects_interval_below_minimum() {
        let root = test_root("live-interval");
        let coordinator = Arc::new(initialized_test_coordinator(
            &root,
            "live-interval".to_string(),
            crate::daemon::view_hooks::CurrentClientViews::default(),
        ));
        let (server, client) = UnixStream::pair().unwrap();
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };

        handle_v2_live_subscription(coordinator.clone(), server, source, target, 99).unwrap();

        let mut response = String::new();
        std::io::BufReader::new(client)
            .read_line(&mut response)
            .unwrap();
        let message: ServerMessage = serde_json::from_str(response.trim()).unwrap();
        assert!(matches!(
            message,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ));
        assert!(coordinator.live.entries.lock().unwrap().is_empty());

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any())]
    fn live_subscription_guard_removes_registry_entry_after_write_failure() {
        let root = test_root("live-guard");
        let coordinator = Arc::new(live_test_coordinator(
            &root,
            "live-guard".to_string(),
            crate::daemon::view_hooks::CurrentClientViews::default(),
        ));
        let (server, client) = UnixStream::pair().unwrap();
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let handler = {
            let coordinator = coordinator.clone();
            thread::spawn(move || {
                handle_v2_live_subscription(coordinator, server, source, target, 2000)
            })
        };
        // Wait until the handler registered its demand, then sever the client.
        let deadline = Instant::now() + Duration::from_secs(2);
        while coordinator.live.entries.lock().unwrap().is_empty() {
            assert!(
                Instant::now() < deadline,
                "subscription was never registered"
            );
            thread::sleep(Duration::from_millis(5));
        }
        drop(client);
        coordinator
            .live
            .entries
            .lock()
            .unwrap()
            .values()
            .for_each(|entry| {
                entry.mailbox.push(LivePush::Unavailable {
                    reason: LiveUnavailableReason::TargetMissing,
                });
            });

        handler.join().unwrap().unwrap();
        assert!(coordinator.live.entries.lock().unwrap().is_empty());

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_wait_timeout_yields_heartbeat_instead_of_full_snapshot() {
        let root = test_root("heartbeat-wait");
        let coordinator = initialized_test_coordinator(
            &root,
            "heartbeat-wait".to_string(),
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );
        let published = coordinator.publish_resolved_snapshot().unwrap();

        // With no newer revision, ten seconds of waiting yields only heartbeat
        // outcomes; the cached full snapshot is never re-sent.
        for _ in 0..5 {
            let outcome = coordinator
                .wait_for_snapshot_after(published.revision, Duration::from_millis(10))
                .expect("wait must not observe a shutdown");
            assert!(matches!(
                outcome,
                SnapshotWaitOutcome::HeartbeatDue { snapshot_revision }
                    if snapshot_revision == published.revision
            ));
        }

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn idle_subscription_stream_sends_heartbeats_and_new_revisions_still_flow() {
        let root = test_root("heartbeat-stream");
        let coordinator = Arc::new(initialized_test_coordinator(
            &root,
            "heartbeat-stream".to_string(),
            crate::daemon::view_hooks::CurrentClientViews::default(),
        ));
        let published = coordinator.publish_resolved_snapshot().unwrap();
        let daemon_instance_id = coordinator
            .router
            .lock()
            .unwrap()
            .daemon_instance_id()
            .clone();
        let (server, client) = UnixStream::pair().unwrap();
        let stream_worker = {
            let coordinator = coordinator.clone();
            let last_revision = published.revision;
            thread::spawn(move || {
                stream_v2_subscription_with_heartbeat_interval(
                    coordinator,
                    server,
                    last_revision,
                    Duration::from_millis(10),
                )
            })
        };
        let mut reader = std::io::BufReader::new(client.try_clone().unwrap());

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let first: ServerMessage = serde_json::from_str(line.trim()).unwrap();
        match first {
            ServerMessage::Heartbeat {
                daemon_instance_id: heartbeat_instance,
                snapshot_revision,
            } => {
                assert_eq!(heartbeat_instance, daemon_instance_id);
                assert_eq!(snapshot_revision, published.revision);
            }
            other => panic!("expected a heartbeat, received {other:?}"),
        }

        // A real revision bump still flows through as a full snapshot frame.
        coordinator
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .leased
            .runtime
            .mark_projection_changed()
            .unwrap();
        let newer = coordinator.publish_resolved_snapshot().unwrap();
        assert_eq!(newer.revision, published.revision + 1);
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let message: ServerMessage = serde_json::from_str(line.trim()).unwrap();
            match message {
                ServerMessage::Heartbeat { .. } => continue,
                ServerMessage::ResolvedSnapshotResult {
                    snapshot_revision, ..
                } => {
                    assert_eq!(snapshot_revision, newer.revision);
                    break;
                }
                other => panic!("unexpected subscription frame: {other:?}"),
            }
        }

        drop(reader);
        drop(client);
        stream_worker.join().unwrap().unwrap();
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any())]
    fn live_mailbox_delivers_latest_only_and_close_ends_the_stream() {
        let mailbox = LiveMailbox::default();
        mailbox.push(LivePush::Unavailable {
            reason: LiveUnavailableReason::TargetMissing,
        });
        mailbox.push(LivePush::Unavailable {
            reason: LiveUnavailableReason::CaptureFailed,
        });

        // A slow subscriber only ever sees the newest undelivered message.
        assert!(matches!(
            mailbox.wait(),
            Some(LivePush::Unavailable {
                reason: LiveUnavailableReason::CaptureFailed,
            })
        ));

        mailbox.close();
        assert!(mailbox.wait().is_none());
    }

    #[test]
    #[cfg(any())]
    fn live_registry_prunes_demand_for_missing_panes() {
        let registry = LiveRegistry::default();
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let (_, mailbox) =
            registry.register(source.clone(), target.clone(), Duration::from_secs(2));
        registry.bodies.lock().unwrap().insert(
            target.clone(),
            LiveBodyEntry {
                live_revision: 1,
                captured_at_epoch_millis: 0,
                body: Arc::new("body".to_string()),
            },
        );

        // Both panes still present: demand survives.
        registry.prune_missing_panes(&BTreeSet::from([source.clone(), target.clone()]));
        assert_eq!(registry.entries.lock().unwrap().len(), 1);

        // The target pane disappeared: demand and cached body are dropped and
        // the subscriber stream is closed.
        registry.prune_missing_panes(&BTreeSet::from([source]));
        assert!(registry.entries.lock().unwrap().is_empty());
        assert!(registry.bodies.lock().unwrap().is_empty());
        assert!(mailbox.wait().is_none());
    }

    #[test]
    fn observation_batch_keeps_sequence_order_with_following_mutations() {
        let root = test_root("batch-sequence");
        let server_identity = crate::daemon::topology::ServerIdentity {
            pid: 1,
            start_time: 2,
        };
        let coordinator = test_coordinator(&root, "a1".repeat(32));
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(DaemonPhase::Serving);

        assert!(
            coordinator.enqueue_internal(V2InternalMutation::ObservationBatch(Box::new(
                ObservationBatchPayload {
                    projection: Box::new(ObservationPollProjection {
                        observation_seq: 1,
                        topology: crate::daemon::topology::TopologySnapshot {
                            server_identity,
                            panes: Vec::new(),
                        },
                        status_metadata: super::super::runtime::StatusProjectionMetadata::default(),
                        witnesses: Vec::new(),
                        observation_bases: BTreeMap::new(),
                        view_base: crate::daemon::view_hooks::CurrentClientViews::default(),
                        through_unread_order: 0,
                    }),
                    observations: Vec::new(),
                    removals: Vec::new(),
                    diagnostics: Vec::new(),
                }
            ),))
        );
        assert!(
            coordinator.enqueue_internal(V2InternalMutation::DiagnosticProjection {
                pane_instance: None,
                message: "after-batch".to_string(),
            })
        );

        let queue = coordinator.queue.lock().unwrap();
        assert_eq!(queue.items.len(), 2);
        let sequences = queue
            .items
            .iter()
            .map(|item| item.sequenced.accepted_seq)
            .collect::<Vec<_>>();
        assert!(sequences[0] < sequences[1]);
        assert!(matches!(
            queue.items.front().map(|item| &item.sequenced.mutation),
            Some(V2AcceptedMutation::Internal(
                V2InternalMutation::ObservationBatch(_)
            ))
        ));
        assert!(matches!(
            queue.items.back().map(|item| &item.sequenced.mutation),
            Some(V2AcceptedMutation::Internal(
                V2InternalMutation::DiagnosticProjection { .. }
            ))
        ));
        drop(queue);

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_waiter_cannot_miss_shutdown_notification() {
        let coordinator = Arc::new(detached_test_coordinator("e".repeat(64)));
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let coordinator = coordinator.clone();
            thread::spawn(move || {
                started_tx.send(()).unwrap();
                let result = coordinator.wait_for_snapshot_after(0, Duration::from_secs(2));
                done_tx.send(result).unwrap();
            })
        };
        started_rx.recv().unwrap();

        coordinator.begin_graceful_shutdown(u64::MAX);

        assert!(
            done_rx
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
        waiter.join().unwrap();
    }

    #[test]
    fn published_snapshot_frame_is_shared_and_replaced_only_for_new_revision() {
        let root = test_root("published-snapshot");
        let coordinator = test_coordinator(&root, "c".repeat(64));
        install_test_state(
            &coordinator,
            &root,
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );

        let first = coordinator.publish_resolved_snapshot().unwrap();
        assert!(matches!(
            coordinator.query(ClientMessage::QueryStatusSnapshot {
                proto: PROTOCOL_VERSION,
                context: crate::daemon::protocol::v2::StatusContext::Global,
            }),
            ServerMessage::StatusSnapshotResult {
                snapshot_revision: 0,
                ..
            }
        ));
        let same = coordinator.publish_resolved_snapshot().unwrap();
        assert!(Arc::ptr_eq(&first.frame, &same.frame));
        assert!(Arc::ptr_eq(&first.message, &same.message));
        coordinator
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .leased
            .runtime
            .mark_projection_changed()
            .unwrap();
        let changed = coordinator.publish_resolved_snapshot().unwrap();
        assert_eq!(changed.revision, first.revision + 1);
        assert!(!Arc::ptr_eq(&first.frame, &changed.frame));
        coordinator
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .leased
            .runtime
            .set_snapshot_revision_for_test(first.revision);
        let stale_publisher = coordinator.publish_resolved_snapshot().unwrap();
        assert_eq!(stale_publisher.revision, changed.revision);
        assert!(Arc::ptr_eq(&stale_publisher.frame, &changed.frame));

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publish_resolved_snapshot_skips_rebuild_for_unchanged_revision() {
        let root = test_root("publish-rebuild-baseline");
        let coordinator = test_coordinator(&root, "f".repeat(64));
        install_test_state(
            &coordinator,
            &root,
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );

        let first = coordinator.publish_resolved_snapshot().unwrap();
        let cached = coordinator.publish_resolved_snapshot().unwrap();

        // The revision fast path serves the cached frame without rebuilding the
        // checked snapshot when the revision has not changed.
        assert!(Arc::ptr_eq(&first.frame, &cached.frame));

        coordinator
            .state
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .leased
            .runtime
            .mark_projection_changed()
            .unwrap();
        let changed = coordinator.publish_resolved_snapshot().unwrap();
        assert!(!Arc::ptr_eq(&first.frame, &changed.frame));

        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn query_pane_cache_miss_with_refresh_outcome(
        outcome: Result<
            crate::daemon::topology::TargetedRefreshOutcome,
            crate::daemon::topology::TopologyError,
        >,
    ) -> (
        ServerMessage,
        Vec<crate::daemon::protocol::v2::DaemonDiagnostic>,
    ) {
        let root = test_root("query-pane-refresh");
        let coordinator = Arc::new(test_coordinator(&root, "9".repeat(64)));
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(DaemonPhase::Serving);
        install_test_state(
            &coordinator,
            &root,
            crate::daemon::view_hooks::CurrentClientViews::default(),
        );

        let (result_tx, result_rx) = mpsc::channel();
        let query_coordinator = coordinator.clone();
        let query = thread::spawn(move || {
            result_tx
                .send(query_coordinator.query(ClientMessage::QueryPane {
                    proto: PROTOCOL_VERSION,
                    pane_id: "%7".to_string(),
                }))
                .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        let queued = loop {
            if let Some(queued) = coordinator.queue.lock().unwrap().items.pop_front() {
                break queued;
            }
            assert!(
                Instant::now() < deadline,
                "QueryPane refresh was not queued"
            );
            thread::yield_now();
        };
        assert!(matches!(
            &queued.sequenced.mutation,
            V2AcceptedMutation::Internal(V2InternalMutation::TargetedPaneRefresh { pane_id })
                if pane_id == "%7"
        ));
        let refresh_response = targeted_pane_refresh_outcome_response(&coordinator, "%7", outcome);
        coordinator.complete(queued.sequenced.accepted_seq, refresh_response);

        let response = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        query.join().unwrap();
        let diagnostics = coordinator
            .state
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .global_diagnostics
            .iter()
            .cloned()
            .collect();
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
        (response, diagnostics)
    }

    #[test]
    fn query_pane_cache_miss_waits_for_targeted_refresh_and_returns_found() {
        let (response, diagnostics) = query_pane_cache_miss_with_refresh_outcome(Ok(
            crate::daemon::topology::TargetedRefreshOutcome::Found(Box::new(
                crate::daemon::topology::TopologyPane {
                    pane_instance: PaneInstance {
                        pane_id: "%7".to_string(),
                        pane_pid: 700,
                    },
                    session_links: Vec::new(),
                    window_id: "@1".to_string(),
                    window_name: "main".to_string(),
                    current_path: "/tmp".to_string(),
                    current_command: "zsh".to_string(),
                    pane_width: 80,
                    active: true,
                    editprompt_is_editor: false,
                    editprompt_target_panes: Vec::new(),
                    editprompt_editor_pane: None,
                },
            )),
        ));
        assert!(diagnostics.is_empty());
        assert!(matches!(
            response,
            ServerMessage::PaneResult {
                pane: crate::daemon::protocol::v2::PanePresentation {
                    pane_instance: PaneInstance {
                        pane_id,
                        pane_pid: 700,
                    },
                    ..
                },
                ..
            } if pane_id == "%7"
        ));
    }

    #[test]
    fn query_pane_cache_miss_returns_pane_not_found_after_fresh_absence() {
        assert!(matches!(
            query_pane_cache_miss_with_refresh_outcome(Ok(
                crate::daemon::topology::TargetedRefreshOutcome::NotFound,
            ))
            .0,
            ServerMessage::Error {
                code: ErrorCode::PaneNotFound,
                ..
            }
        ));
    }

    #[test]
    fn query_pane_cache_miss_returns_internal_error_after_refresh_failure() {
        let failure =
            crate::daemon::topology::TopologyError::Query("tmux query failed".to_string());
        let (response, diagnostics) = query_pane_cache_miss_with_refresh_outcome(Err(failure));
        assert!(matches!(
            response,
            ServerMessage::Error {
                code: ErrorCode::InternalError,
                ..
            }
        ));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, ErrorCode::InternalError);
        assert!(diagnostics[0].message.contains("tmux query failed"));
    }

    #[test]
    fn query_pane_cache_miss_records_refresh_timeout_diagnostic() {
        let (response, diagnostics) = query_pane_cache_miss_with_refresh_outcome(Err(
            crate::daemon::topology::TopologyError::Deadline,
        ));
        assert!(matches!(
            response,
            ServerMessage::Error {
                code: ErrorCode::InternalError,
                ..
            }
        ));
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("deadline exceeded"));
    }

    #[test]
    fn oversized_resolved_snapshot_commits_and_queues_frame_too_large_diagnostic() {
        let root = test_root("frame-too-large");
        let coordinator = test_coordinator(&root, "f".repeat(64));
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(DaemonPhase::Serving);
        let leased =
            super::super::runtime::LeasedCanonicalPaneStateRuntime::acquire(&root.join("writer"))
                .unwrap();
        let mut state = super::super::runtime::CanonicalCoordinatorState::new(
            leased,
            crate::daemon::topology::TopologySnapshot {
                server_identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                panes: Vec::new(),
            },
            crate::daemon::view_hooks::CurrentClientViews::default(),
            crate::sidebar::state::SidebarPreferences::default(),
        );
        state
            .replace_topology(crate::daemon::topology::TopologySnapshot {
                server_identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                panes: vec![crate::daemon::topology::TopologyPane {
                    pane_instance: PaneInstance {
                        pane_id: "%1".to_string(),
                        pane_pid: 101,
                    },
                    session_links: Vec::new(),
                    window_id: "@1".to_string(),
                    window_name: "x".repeat(crate::pane_state::MAX_RESPONSE_FRAME_BYTES),
                    current_path: "/tmp".to_string(),
                    current_command: "zsh".to_string(),
                    pane_width: 80,
                    active: false,
                    editprompt_is_editor: false,
                    editprompt_target_panes: Vec::new(),
                    editprompt_editor_pane: None,
                }],
            })
            .unwrap();
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);
        *coordinator.state.lock().unwrap() = Some(state);

        let published = coordinator.publish_resolved_snapshot().unwrap();
        assert!(published.terminal);
        assert!(matches!(
            published.message.as_ref(),
            ServerMessage::Error {
                code: ErrorCode::FrameTooLarge,
                ..
            }
        ));
        let mutation = coordinator.queue.lock().unwrap().items.pop_front().unwrap();
        assert!(matches!(
            &mutation.sequenced.mutation,
            V2AcceptedMutation::Internal(V2InternalMutation::FrameTooLargeProjection {
                rejected_revision: 1
            })
        ));
        let response = apply_production_mutation(&coordinator, mutation.sequenced);
        assert!(matches!(
            response,
            ServerMessage::SnapshotAck {
                snapshot_revision: 2,
                ..
            }
        ));
        let state = coordinator.state.lock().unwrap();
        assert!(
            state
                .as_ref()
                .unwrap()
                .global_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == ErrorCode::FrameTooLarge)
        );

        drop(state);
        drop(coordinator);
        match std::fs::remove_dir_all(root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("failed to remove test directory: {error}"),
        }
    }

    #[test]
    fn terminal_subscription_frame_is_written_once_then_stream_closes() {
        let coordinator = Arc::new(detached_test_coordinator("a".repeat(64)));
        let message = ServerMessage::error(ErrorCode::FrameTooLarge, "too large", None);
        let frame = crate::daemon::protocol::v2::encode_response_frame(&message).unwrap();
        *coordinator.snapshot_cache.lock().unwrap() = Some(PublishedResolvedSnapshot {
            revision: 2,
            frame: Arc::new(frame),
            message: Arc::new(message),
            terminal: true,
        });
        let (mut client, server) = UnixStream::pair().unwrap();
        let handle = {
            let coordinator = coordinator.clone();
            thread::spawn(move || stream_v2_subscription(coordinator, server, 1).unwrap())
        };

        let mut raw = String::new();
        client.read_to_string(&mut raw).unwrap();
        handle.join().unwrap();
        let frames = raw.lines().collect::<Vec<_>>();
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(frames[0]).unwrap(),
            ServerMessage::Error {
                code: ErrorCode::FrameTooLarge,
                ..
            }
        ));
    }

    #[test]
    fn v2_requires_hello_and_rejects_v1_before_side_effects() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        let mut connection = V2ConnectionState::default();
        assert!(matches!(
            router.route(&mut connection, v2_begin()),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));
        assert!(matches!(
            router.route(&mut connection, ClientMessage::Hello { proto: 1 },),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::UnsupportedProtocol,
                ..
            })
        ));
        v2_handshake(&mut router, &mut connection);
        assert!(matches!(
            router.route(
                &mut connection,
                ClientMessage::QueryResolvedSnapshot { proto: 1 },
            ),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::UnsupportedProtocol,
                ..
            })
        ));
    }

    #[test]
    fn v2_read_only_query_does_not_consume_accepted_sequence() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        assert!(matches!(
            router.route(
                &mut connection,
                ClientMessage::QueryResolvedSnapshot {
                    proto: PROTOCOL_VERSION,
                },
            ),
            V2Route::Query(_)
        ));
        assert!(matches!(
            router.route(
                &mut connection,
                ClientMessage::QueryRuntimeInfo {
                    proto: PROTOCOL_VERSION,
                },
            ),
            V2Route::Query(_)
        ));
        let V2Route::Mutation(mutation) = router.route(&mut connection, v2_begin()) else {
            panic!("expected mutation");
        };
        assert_eq!(mutation.accepted_seq, 1);
    }

    #[test]
    fn v2_internal_and_external_mutations_share_one_accepted_sequence() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let V2Route::Mutation(external) = router.route(&mut connection, v2_begin()) else {
            panic!("expected external mutation");
        };
        let V2Route::Mutation(internal) =
            router.accept_internal(V2InternalMutation::RefreshTopology)
        else {
            panic!("expected internal mutation");
        };
        let V2Route::Mutation(next_external) = router.route(&mut connection, v2_begin()) else {
            panic!("expected external mutation");
        };
        assert_eq!(
            (
                external.accepted_seq,
                internal.accepted_seq,
                next_external.accepted_seq,
            ),
            (1, 2, 3)
        );
        assert!(matches!(
            internal.mutation,
            V2AcceptedMutation::Internal(V2InternalMutation::RefreshTopology)
        ));
    }

    #[test]
    fn v2_serving_with_degraded_hooks_continues_queries_and_canonical_mutations() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Serving);
        router.set_hook_health(HookHealth::Degraded);
        let mut connection = V2ConnectionState::default();

        let hello = router.route(
            &mut connection,
            ClientMessage::Hello {
                proto: PROTOCOL_VERSION,
            },
        );
        assert!(matches!(
            hello,
            V2Route::Response(ServerMessage::HelloAck {
                phase: DaemonPhase::Serving,
                hook_health: HookHealth::Degraded,
                ..
            })
        ));
        assert!(matches!(
            router.route(
                &mut connection,
                ClientMessage::QueryResolvedSnapshot {
                    proto: PROTOCOL_VERSION,
                },
            ),
            V2Route::Query(ClientMessage::QueryResolvedSnapshot { .. })
        ));
        assert!(matches!(
            router.route(&mut connection, v2_begin()),
            V2Route::Mutation(V2SequencedMutation {
                accepted_seq: 1,
                mutation: V2AcceptedMutation::External(ClientMessage::SubmitPaneEvent { .. }),
            })
        ));
    }

    #[test]
    fn read_peek_commit_fences_the_old_occurrence_and_protects_the_source_during_advance() {
        let root = test_root("read-peek-fence");
        let (mut state, target, candidate) = read_peek_test_state(&root);
        assert!(state.begin_peek(20, target.clone(), [target.clone()], 3));
        state.activate_peek(20, 3, target.clone(), 0);

        let result = commit_read_peek_state(
            &mut state,
            &mut ReadPeekStoreIo { fail: false },
            &v2_daemon_id(),
            &v2_event_id(),
            &target,
            10,
            vec![candidate.clone()],
            2,
        )
        .unwrap();

        assert_eq!(
            result.read_outcome,
            crate::daemon::protocol::v2::PaneApplyOutcome::Committed
        );
        assert_eq!(result.candidates, vec![candidate.clone()]);
        assert!(
            !state
                .leased
                .runtime
                .record(&target)
                .unwrap()
                .unread
                .is_unread()
        );
        assert!(matches!(
            state.peek_leases.get(&10),
            Some(super::super::runtime::PeekLease::Pending {
                operation_seq: 2,
                previous_target: Some(previous),
                candidates,
                ..
            }) if previous == &target && candidates == &BTreeSet::from([candidate])
        ));
        assert!(state.active_peek_target(20).is_none());

        let owner = read_peek_test_witness(10, &target);
        assert!(
            !state
                .read_authorized_panes(std::slice::from_ref(&owner))
                .contains(&target)
        );
        emit_read_peek_waiting_occurrence(&mut state, &target);
        let unread = &state.leased.runtime.record(&target).unwrap().unread;
        assert!(unread.is_unread());
        assert!(unread.latest_unread().unwrap().order > 1);

        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_peek_without_an_advance_candidate_keeps_a_new_active_source_lease() {
        let root = test_root("read-peek-stayed");
        let (mut state, target, _) = read_peek_test_state(&root);
        let result = commit_read_peek_state(
            &mut state,
            &mut ReadPeekStoreIo { fail: false },
            &v2_daemon_id(),
            &v2_event_id(),
            &target,
            10,
            Vec::new(),
            2,
        )
        .unwrap();

        assert!(result.candidates.is_empty());
        assert_eq!(state.active_peek_target(10), Some(&target));
        let owner = read_peek_test_witness(10, &target);
        assert!(
            !state
                .read_authorized_panes(std::slice::from_ref(&owner))
                .contains(&target)
        );
        emit_read_peek_waiting_occurrence(&mut state, &target);
        assert!(
            state
                .leased
                .runtime
                .record(&target)
                .unwrap()
                .unread
                .is_unread()
        );

        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_peek_advance_failure_restores_the_source_without_rolling_back_read() {
        let root = test_root("read-peek-advance-failure");
        let (mut state, target, candidate) = read_peek_test_state(&root);
        commit_read_peek_state(
            &mut state,
            &mut ReadPeekStoreIo { fail: false },
            &v2_daemon_id(),
            &v2_event_id(),
            &target,
            10,
            vec![candidate],
            2,
        )
        .unwrap();
        state.restore_peek_after_failure(10, 2, &[read_peek_test_witness(10, &target)], 3);

        assert_eq!(state.active_peek_target(10), Some(&target));
        assert!(
            !state
                .leased
                .runtime
                .record(&target)
                .unwrap()
                .unread
                .is_unread()
        );

        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_peek_persist_failure_preserves_the_unread_occurrence_and_active_lease() {
        let root = test_root("read-peek-persist-failure");
        let (mut state, target, candidate) = read_peek_test_state(&root);
        let result = commit_read_peek_state(
            &mut state,
            &mut ReadPeekStoreIo { fail: true },
            &v2_daemon_id(),
            &v2_event_id(),
            &target,
            10,
            vec![candidate],
            2,
        );

        assert!(matches!(
            result,
            Err(crate::pane_state::store::StoreError::PersistFailed(_))
        ));
        assert_eq!(state.active_peek_target(10), Some(&target));
        assert!(
            state
                .leased
                .runtime
                .record(&target)
                .unwrap()
                .unread
                .is_unread()
        );

        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_peek_terminal_occurrences_use_client_scoped_born_read_authority() {
        let scenarios = [
            (
                "waiting",
                PaneEvent::WaitRequested {
                    observed_at: 3,
                    reason: crate::pane_state::WaitReason::PermissionPrompt,
                },
                crate::pane_state::UnreadReason::Waiting,
            ),
            (
                "error",
                PaneEvent::FailRun {
                    observed_at: 3,
                    reason: Some("failed".to_string()),
                },
                crate::pane_state::UnreadReason::Error,
            ),
            (
                "completed",
                PaneEvent::CompleteRun { completed_at: 3 },
                crate::pane_state::UnreadReason::Completed,
            ),
        ];

        for (label, terminal_event, expected_reason) in scenarios {
            for observer_visible in [false, true] {
                let root = test_root(&format!(
                    "read-peek-born-{label}-{}",
                    if observer_visible {
                        "observer"
                    } else {
                        "owner"
                    }
                ));
                let (mut state, target, _) = read_peek_test_state(&root);
                commit_read_peek_state(
                    &mut state,
                    &mut ReadPeekStoreIo { fail: false },
                    &v2_daemon_id(),
                    &v2_event_id(),
                    &target,
                    10,
                    Vec::new(),
                    2,
                )
                .unwrap();
                apply_read_peek_event(
                    &mut state,
                    &target,
                    PaneEvent::BeginRun {
                        started_at: 2,
                        prompt: None,
                    },
                    &crate::pane_state::VisibilitySnapshot::default(),
                );

                let owner = read_peek_test_witness(10, &target);
                let mut witnesses = vec![owner];
                if observer_visible {
                    witnesses.push(read_peek_test_witness(20, &target));
                }
                let panes = BTreeSet::from([target.clone()]);
                let authorized = state.has_read_authority_for(&witnesses, &panes);
                assert_eq!(authorized, observer_visible, "{label}");
                if authorized {
                    state.clear_peeks_for_read_panes(&panes);
                }
                apply_read_peek_event(
                    &mut state,
                    &target,
                    terminal_event.clone(),
                    &crate::pane_state::VisibilitySnapshot {
                        pane_visible_to_eligible_client: authorized,
                    },
                );

                let unread = &state.leased.runtime.record(&target).unwrap().unread;
                if observer_visible {
                    assert!(!unread.is_unread(), "{label}");
                    assert!(state.active_peek_target(10).is_none(), "{label}");
                } else {
                    assert_eq!(
                        unread.latest_unread().map(|occurrence| occurrence.reason),
                        Some(expected_reason),
                        "{label}"
                    );
                    assert_eq!(state.active_peek_target(10), Some(&target), "{label}");
                }

                drop(state);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn peek_observations_are_causal_across_effect_completion_orderings() {
        for observation_before_completion in [false, true] {
            let root = test_root(if observation_before_completion {
                "peek-observation-before-completion"
            } else {
                "peek-completion-before-observation"
            });
            let (mut state, source, target) = read_peek_test_state(&root);
            assert!(state.begin_peek(10, source.clone(), [target.clone()], 2));
            let source_witness = read_peek_test_witness(10, &source);
            let target_witness = read_peek_test_witness(10, &target);

            if observation_before_completion {
                state.reconcile_peek_leases(std::slice::from_ref(&target_witness), 4);
                assert!(matches!(
                    state.peek_leases.get(&10),
                    Some(super::super::runtime::PeekLease::Pending {
                        operation_seq: 2,
                        ..
                    })
                ));
            }
            let coordinator = test_coordinator(&root, "causal-completion");
            *coordinator.state.lock().unwrap() = Some(state);
            let response = apply_production_mutation(
                &coordinator,
                V2SequencedMutation {
                    accepted_seq: 3,
                    mutation: V2AcceptedMutation::Internal(
                        V2InternalMutation::SidebarEffectCompleted(SidebarEffectCompletion {
                            original_accepted_seq: 2,
                            event_id: v2_event_id(),
                            snapshot_revision: 7,
                            witness_observation_floor: 5,
                            result: SidebarEffectResult::Succeeded(target.clone()),
                            effect: super::super::runtime::CanonicalSidebarEffect::PeekPane {
                                pane_instance: target.clone(),
                                client_pid: 10,
                                source_pane: source.clone(),
                            },
                        }),
                    ),
                },
            );
            assert!(matches!(
                response,
                ServerMessage::SnapshotAck {
                    accepted_seq: 3,
                    ..
                }
            ));

            {
                let mut guard = coordinator.state.lock().unwrap();
                let state = guard.as_mut().unwrap();
                state.reconcile_peek_leases(std::slice::from_ref(&source_witness), 4);
                assert_eq!(state.active_peek_target(10), Some(&target));
                state.reconcile_peek_leases(std::slice::from_ref(&target_witness), 6);
                assert_eq!(state.active_peek_target(10), Some(&target));
                state.reconcile_peek_leases(std::slice::from_ref(&source_witness), 4);
                assert_eq!(state.active_peek_target(10), Some(&target));

                state.reconcile_peek_leases(std::slice::from_ref(&source_witness), 7);
                assert!(state.active_peek_target(10).is_none());
            }
            drop(coordinator);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn read_peek_stale_advance_is_stayed_and_other_failures_remain_failed() {
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        assert_eq!(
            read_peek_advance_outcome(&SidebarEffectResult::Succeeded(pane.clone())),
            crate::daemon::protocol::v2::PeekAdvanceOutcome::Jumped {
                pane_instance: pane,
            }
        );
        assert_eq!(
            read_peek_advance_outcome(&SidebarEffectResult::NoAvailablePane),
            crate::daemon::protocol::v2::PeekAdvanceOutcome::Stayed
        );
        assert_eq!(
            read_peek_advance_outcome(&SidebarEffectResult::SourceClientMismatch),
            crate::daemon::protocol::v2::PeekAdvanceOutcome::Failed
        );
    }

    #[test]
    fn sidebar_jump_requires_one_eligible_client_for_source_pane() {
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 909,
        };
        let mut views = crate::daemon::view_hooks::CurrentClientViews::default();
        assert_eq!(unique_eligible_client_pid(&views, &source), Err(0));

        let witness = |client_pid| crate::pane_state::ClientWitness {
            client_pid,
            session_id: format!("${client_pid}"),
            window_id: "@1".to_string(),
            active_pane: source.clone(),
            control_mode: false,
            active_pane_flag: false,
        };
        views
            .reconcile(
                &[witness(10)],
                &BTreeMap::from([("@1".to_string(), vec![source.clone()])]),
            )
            .unwrap();
        assert_eq!(unique_eligible_client_pid(&views, &source), Ok(10));

        views
            .reconcile(
                &[witness(10), witness(20)],
                &BTreeMap::from([("@1".to_string(), vec![source.clone()])]),
            )
            .unwrap();
        assert_eq!(unique_eligible_client_pid(&views, &source), Err(2));
    }

    #[test]
    fn sidebar_worker_completion_reenters_the_shared_sequence_after_external_command() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let command = ClientMessage::SidebarCommand {
            proto: PROTOCOL_VERSION,
            daemon_instance_id: v2_daemon_id(),
            event_id: v2_event_id(),
            command: crate::daemon::protocol::v2::SidebarCommand::JumpPane {
                pane_instance: PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
                source_pane: PaneInstance {
                    pane_id: "%9".to_string(),
                    pane_pid: 909,
                },
            },
        };
        let V2Route::Mutation(external) = router.route(&mut connection, command) else {
            panic!("expected sidebar mutation");
        };
        let completion = SidebarEffectCompletion {
            original_accepted_seq: external.accepted_seq,
            event_id: v2_event_id(),
            snapshot_revision: 7,
            witness_observation_floor: 0,
            result: SidebarEffectResult::Succeeded(PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 101,
            }),
            effect: super::super::runtime::CanonicalSidebarEffect::JumpPane {
                pane_instance: PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
                client_pid: 10,
                source_pane: PaneInstance {
                    pane_id: "%9".to_string(),
                    pane_pid: 909,
                },
            },
        };
        let V2Route::Mutation(internal) =
            router.accept_internal(V2InternalMutation::SidebarEffectCompleted(completion))
        else {
            panic!("expected sequenced sidebar completion");
        };

        assert_eq!(external.accepted_seq, 1);
        assert_eq!(internal.accepted_seq, 2);
        assert!(matches!(
            internal.mutation,
            V2AcceptedMutation::Internal(V2InternalMutation::SidebarEffectCompleted(
                SidebarEffectCompletion {
                    original_accepted_seq: 1,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn sidebar_dispatch_returns_before_worker_completion_and_releases_original_waiter_after_event()
    {
        let (job_tx, job_rx) = mpsc::sync_channel(1);
        let deferred = Mutex::new(BTreeSet::new());
        enqueue_sidebar_tmux_job(
            &job_tx,
            &deferred,
            SidebarTmuxJob {
                effect: super::super::runtime::CanonicalSidebarEffect::JumpPane {
                    pane_instance: PaneInstance {
                        pane_id: "%1".to_string(),
                        pane_pid: 101,
                    },
                    client_pid: 10,
                    source_pane: PaneInstance {
                        pane_id: "%9".to_string(),
                        pane_pid: 909,
                    },
                },
                original_accepted_seq: 1,
                event_id: v2_event_id(),
                snapshot_revision: 7,
            },
        )
        .unwrap();
        assert!(deferred.lock().unwrap().contains(&1));
        let pending = job_rx.try_recv().expect("job is queued without waiting");

        let coordinator = detached_test_coordinator("0".repeat(64));
        coordinator
            .deferred_responses
            .lock()
            .unwrap()
            .insert(pending.original_accepted_seq);
        let (waiter_tx, waiter_rx) = mpsc::channel();
        coordinator.waiters.lock().unwrap().insert(1, waiter_tx);
        assert!(waiter_rx.try_recv().is_err());

        let internal_response = apply_production_mutation(
            &coordinator,
            V2SequencedMutation {
                accepted_seq: 2,
                mutation: V2AcceptedMutation::Internal(V2InternalMutation::SidebarEffectCompleted(
                    SidebarEffectCompletion {
                        original_accepted_seq: pending.original_accepted_seq,
                        event_id: pending.event_id,
                        snapshot_revision: pending.snapshot_revision,
                        witness_observation_floor: 0,
                        result: SidebarEffectResult::Succeeded(PaneInstance {
                            pane_id: "%1".to_string(),
                            pane_pid: 101,
                        }),
                        effect: pending.effect,
                    },
                )),
            },
        );

        assert!(matches!(
            internal_response,
            ServerMessage::SnapshotAck {
                accepted_seq: 2,
                ..
            }
        ));
        assert!(matches!(
            waiter_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            ServerMessage::SnapshotAck {
                accepted_seq: 1,
                snapshot_revision: 7,
                ..
            }
        ));
        assert!(!coordinator.is_deferred_response(1));
    }

    #[test]
    fn waiterless_view_queue_completion_does_not_emit_disconnected_diagnostic() {
        let coordinator = detached_test_coordinator("f".repeat(64));
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        assert!(matches!(
            coordinator.route_external(
                &mut connection,
                ClientMessage::Hello {
                    proto: PROTOCOL_VERSION,
                },
                0,
            ),
            ServerMessage::HelloAck { .. }
        ));
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let daemon_instance_id = coordinator
            .router
            .lock()
            .unwrap()
            .daemon_instance_id()
            .clone();
        let response = coordinator.route_external(
            &mut connection,
            ClientMessage::SubmitViewEvent {
                proto: PROTOCOL_VERSION,
                event: crate::pane_state::ViewEvent {
                    daemon_instance_id,
                    event_id: v2_event_id(),
                    hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
                    active_pane: Some(pane.clone()),
                    window_panes: vec![pane],
                    visibility: crate::pane_state::ViewVisibilityProof {
                        pane_visible: false,
                        window_visible: false,
                    },
                },
            },
            128,
        );
        assert!(matches!(
            response,
            ServerMessage::ViewQueued {
                accepted_seq: 1,
                ..
            }
        ));
        assert!(coordinator.waiters.lock().unwrap().is_empty());
        let queued = coordinator.queue.lock().unwrap().items.pop_front().unwrap();
        assert_eq!(queued.sequenced.accepted_seq, 1);
        assert!(matches!(
            queued.sequenced.mutation,
            V2AcceptedMutation::External(ClientMessage::SubmitViewEvent { .. })
        ));

        coordinator.complete(
            1,
            ServerMessage::SnapshotAck {
                event_id: v2_event_id(),
                accepted_seq: 1,
                snapshot_revision: 0,
            },
        );

        assert!(coordinator.waiters.lock().unwrap().is_empty());
        assert!(coordinator.queue.lock().unwrap().items.is_empty());
    }

    #[test]
    fn v2_bootstrap_fifo_preserves_order_and_rejects_overflow_without_consuming_seq() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Hydrating);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        for expected in 1..=V2_BOOTSTRAP_FIFO_CAPACITY as u64 {
            assert_eq!(
                router.route(&mut connection, v2_begin()),
                V2Route::Queued {
                    accepted_seq: expected
                }
            );
        }
        assert!(matches!(
            router.route(&mut connection, v2_begin()),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::QueueFull,
                ..
            })
        ));
        assert_eq!(
            router.accept_internal(V2InternalMutation::ReconcileViews),
            V2Route::DroppedInternal
        );
        let mut queued = Vec::new();
        router
            .finish_bootstrap::<()>(|mutations| {
                queued = mutations;
                Ok(())
            })
            .unwrap();
        assert_eq!(queued.len(), V2_BOOTSTRAP_FIFO_CAPACITY);
        assert!(
            queued
                .windows(2)
                .all(|window| window[0].accepted_seq < window[1].accepted_seq)
        );
        let V2Route::Mutation(next) = router.route(&mut connection, v2_begin()) else {
            panic!("expected mutation");
        };
        assert_eq!(next.accepted_seq, 65);
    }

    #[test]
    fn restart_owned_hook_view_event_keeps_fifo_order_during_bootstrap() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Hydrating);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let owned_hook_event = ClientMessage::SubmitViewEvent {
            proto: PROTOCOL_VERSION,
            event: crate::pane_state::ViewEvent {
                daemon_instance_id: v2_daemon_id(),
                event_id: v2_event_id(),
                hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
                active_pane: Some(pane.clone()),
                window_panes: vec![pane],
                visibility: crate::pane_state::ViewVisibilityProof {
                    pane_visible: false,
                    window_visible: false,
                },
            },
        };
        assert!(matches!(
            router.route(&mut connection, owned_hook_event),
            V2Route::Response(ServerMessage::ViewQueued {
                accepted_seq: 1,
                ..
            })
        ));
        assert_eq!(
            router.route(&mut connection, v2_begin()),
            V2Route::Queued { accepted_seq: 2 }
        );

        let queued = router.take_bootstrap_fifo();
        assert_eq!(
            queued
                .iter()
                .map(|mutation| mutation.accepted_seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(matches!(
            queued[0].mutation,
            V2AcceptedMutation::External(ClientMessage::SubmitViewEvent { .. })
        ));
        assert!(matches!(
            queued[1].mutation,
            V2AcceptedMutation::External(ClientMessage::SubmitPaneEvent { .. })
        ));
    }

    #[test]
    fn v2_bootstrap_failure_keeps_hydrating_and_never_serves_queries() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.begin_hydration().unwrap();
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        assert!(matches!(
            router.route(&mut connection, v2_begin()),
            V2Route::Queued { accepted_seq: 1 }
        ));
        let result = router.finish_bootstrap(|queued| {
            assert_eq!(queued.len(), 1);
            Err("initial reconciliation failed")
        });
        assert_eq!(result, Err("initial reconciliation failed"));
        assert_eq!(router.phase(), DaemonPhase::Hydrating);
        assert!(matches!(
            router.route(
                &mut connection,
                ClientMessage::QueryResolvedSnapshot {
                    proto: PROTOCOL_VERSION,
                },
            ),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::NotReady,
                ..
            })
        ));
    }

    #[test]
    fn v2_rejects_stale_instance_and_internal_event_origins() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let mut stale = v2_begin();
        let ClientMessage::SubmitPaneEvent { envelope, .. } = &mut stale else {
            unreachable!();
        };
        envelope.daemon_instance_id =
            DaemonInstanceId::parse("00112233445566778899aabbccddeeff").unwrap();
        assert!(matches!(
            router.route(&mut connection, stale),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::StaleDaemonInstance,
                ..
            })
        ));
        let internal_events = [
            PaneEvent::MarkPaneRead { through_order: 1 },
            PaneEvent::ObservationBatch {
                base: None,
                tracker_generation: 0,
                observed_at: 1,
                presence: crate::pane_state::AgentPresenceObservation::Unknown,
                capture: None,
                process: None,
            },
            PaneEvent::PaneRemoved { expected: None },
        ];
        for internal_event in internal_events {
            assert!(matches!(
                router.route(&mut connection, v2_pane_event(internal_event)),
                V2Route::Response(ServerMessage::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                })
            ));
        }
    }

    #[test]
    fn v2_rejects_invalid_view_before_consuming_accepted_sequence() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let invalid = ClientMessage::SubmitViewEvent {
            proto: PROTOCOL_VERSION,
            event: crate::pane_state::ViewEvent {
                daemon_instance_id: v2_daemon_id(),
                event_id: v2_event_id(),
                hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
                active_pane: Some(pane.clone()),
                window_panes: vec![
                    pane,
                    PaneInstance {
                        pane_id: "invalid".to_string(),
                        pane_pid: 0,
                    },
                ],
                visibility: crate::pane_state::ViewVisibilityProof {
                    pane_visible: false,
                    window_visible: false,
                },
            },
        };
        assert!(matches!(
            router.route(&mut connection, invalid),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));
        let detached_pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let detached_with_occurrence = ClientMessage::SubmitViewEvent {
            proto: PROTOCOL_VERSION,
            event: crate::pane_state::ViewEvent {
                daemon_instance_id: v2_daemon_id(),
                event_id: v2_event_id(),
                hook_kind: crate::pane_state::ViewHookKind::ClientDetached,
                active_pane: Some(detached_pane.clone()),
                window_panes: vec![detached_pane],
                visibility: crate::pane_state::ViewVisibilityProof {
                    pane_visible: false,
                    window_visible: false,
                },
            },
        };
        assert!(matches!(
            router.route(&mut connection, detached_with_occurrence),
            V2Route::Response(ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            })
        ));
        let V2Route::Mutation(mutation) = router.route(&mut connection, v2_begin()) else {
            panic!("expected mutation after rejected view");
        };
        assert_eq!(mutation.accepted_seq, 1);
    }

    #[test]
    fn v2_accepted_sequence_overflow_is_internal_error() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(DaemonPhase::Serving);
        router.set_next_accepted_seq(u64::MAX);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        assert!(matches!(
            router.route(&mut connection, v2_begin()),
            V2Route::Fatal(ServerMessage::Error {
                code: ErrorCode::InternalError,
                ..
            })
        ));
        assert!(router.is_fatal());
    }

    #[test]
    fn v2_frame_body_deadline_is_typed_and_bounded() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut reader = V2FrameReader::new(server);
        client.write_all(b"{").unwrap();
        let started = std::time::Instant::now();
        let error = read_v2_request_frame(&mut reader).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            error,
            ServerMessage::Error {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ));
    }

    #[test]
    fn v2_frame_reader_and_writer_use_newline_framing() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut reader = V2FrameReader::new(server);
        write!(
            client,
            "{{\"op\":\"hello\",\"proto\":{PROTOCOL_VERSION}}}\n\
             {{\"op\":\"query_resolved_snapshot\",\"proto\":{PROTOCOL_VERSION}}}\n"
        )
        .unwrap();
        let frame = read_v2_request_frame(&mut reader).unwrap();
        assert_eq!(
            crate::daemon::protocol::v2::decode_request_frame(&frame).unwrap(),
            ClientMessage::Hello {
                proto: PROTOCOL_VERSION,
            }
        );
        let second = read_v2_request_frame(&mut reader).unwrap();
        assert_eq!(
            crate::daemon::protocol::v2::decode_request_frame(&second).unwrap(),
            ClientMessage::QueryResolvedSnapshot {
                proto: PROTOCOL_VERSION,
            }
        );
        let response = ServerMessage::error(ErrorCode::NotReady, "not ready", None);
        write_v2_response(reader.stream_mut(), &response).unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerMessage>(line.trim()).unwrap(),
            response
        );
    }

    #[test]
    fn v2_request_frame_limit_counts_newline_only_when_present() {
        assert_eq!(
            request_frame_body_bytes(crate::pane_state::MAX_REQUEST_FRAME_BYTES, 1, true),
            crate::pane_state::MAX_REQUEST_FRAME_BYTES
        );
        assert_eq!(
            request_frame_body_bytes(crate::pane_state::MAX_REQUEST_FRAME_BYTES, 1, false),
            crate::pane_state::MAX_REQUEST_FRAME_BYTES + 1
        );
    }

    #[test]
    fn v2_oversized_response_writes_typed_error_on_same_stream() {
        let (mut server, client) = UnixStream::pair().unwrap();
        let oversized = ServerMessage::error(
            ErrorCode::InternalError,
            "x".repeat(crate::pane_state::MAX_RESPONSE_FRAME_BYTES),
            None,
        );
        write_v2_response(&mut server, &oversized).unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(line.trim()).unwrap(),
            ServerMessage::Error {
                code: ErrorCode::FrameTooLarge,
                ..
            }
        ));
    }

    #[test]
    fn response_write_timeout_has_one_millisecond_floor() {
        assert_eq!(
            bounded_write_timeout(Duration::from_nanos(1)),
            Duration::from_millis(1)
        );
        assert_eq!(
            bounded_write_timeout(Duration::from_millis(2)),
            Duration::from_millis(2)
        );
    }

    #[test]
    fn shutdown_forwarder_stops_v2_coordinator() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let (mut signal_writer, signal_reader) = UnixStream::pair().unwrap();
        let coordinator = Arc::new(detached_test_coordinator("1".repeat(64)));
        spawn_shutdown_forwarder(signal_reader, coordinator.clone());
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let coordinator = coordinator.clone();
            thread::spawn(move || {
                coordinator.wait_for_shutdown();
                done_tx.send(()).unwrap();
            })
        };

        signal_writer.write_all(b"x").unwrap();

        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        waiter.join().unwrap();
        assert!(coordinator.shutdown.load(Ordering::SeqCst));
        assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
        assert!(coordinator.router.lock().unwrap().is_fatal());
    }
}

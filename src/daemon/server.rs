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

use crate::tmux::TmuxRunner;

static SHUTDOWN_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

const V2_BOOTSTRAP_FIFO_CAPACITY: usize = 64;
const V2_MUTATION_QUEUE_CAPACITY: usize = 64;
pub const V2_FRAME_START_TIMEOUT: Duration = Duration::from_secs(2);
pub const V2_FRAME_BODY_TIMEOUT: Duration = Duration::from_millis(100);
pub const V2_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

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
) -> std::result::Result<Vec<u8>, crate::daemon::protocol::v2::ServerMessage> {
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
    message: &crate::daemon::protocol::v2::ServerMessage,
) -> std::result::Result<(), crate::daemon::protocol::v2::ServerMessage> {
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
fn write_v2_frame(
    stream: &mut UnixStream,
    frame: &[u8],
) -> std::result::Result<(), crate::daemon::protocol::v2::ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let deadline = std::time::Instant::now() + V2_RESPONSE_WRITE_TIMEOUT;
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
    External(crate::daemon::protocol::v2::ClientMessage),
    Internal(V2InternalMutation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum V2InternalMutation {
    PaneEvent(Box<crate::pane_state::PaneEventEnvelope>),
    ObservationPollProjection(Box<ObservationPollProjection>),
    RefreshTopology,
    TargetedPaneRefresh {
        pane_id: String,
    },
    ReconcileViews,
    GitProjection {
        badges: std::collections::BTreeMap<String, crate::git::GitBadge>,
        worktrees: std::collections::BTreeMap<String, crate::git::WorktreeInfo>,
    },
    TriageProjection,
    DiagnosticProjection {
        pane_instance: Option<crate::pane_state::PaneInstance>,
        message: String,
    },
    FrameTooLargeProjection {
        rejected_revision: u64,
    },
    HookHealthProjection {
        health: crate::daemon::protocol::v2::HookHealth,
        diagnostic: Option<String>,
    },
    SidebarEffectCompleted(SidebarEffectCompletion),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservationPollProjection {
    topology: crate::daemon::topology::TopologySnapshot,
    status_metadata: super::runtime::StatusProjectionMetadata,
    witnesses: Vec<crate::pane_state::ClientWitness>,
    observation_bases:
        BTreeMap<crate::pane_state::PaneInstance, Option<crate::pane_state::StoredStateDescriptor>>,
    view_base: crate::daemon::view_hooks::ViewRegistry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum V2Route {
    Response(crate::daemon::protocol::v2::ServerMessage),
    Fatal(crate::daemon::protocol::v2::ServerMessage),
    Query(crate::daemon::protocol::v2::ClientMessage),
    Mutation(V2SequencedMutation),
    Queued { accepted_seq: u64 },
    DroppedInternal,
}

#[derive(Debug, Clone)]
pub struct V2Router {
    daemon_instance_id: crate::pane_state::DaemonInstanceId,
    server_identity: String,
    phase: crate::daemon::protocol::v2::DaemonPhase,
    hook_health: crate::daemon::protocol::v2::HookHealth,
    next_accepted_seq: u64,
    bootstrap_fifo: std::collections::VecDeque<V2SequencedMutation>,
    fatal: bool,
}

impl V2Router {
    pub fn new(
        daemon_instance_id: crate::pane_state::DaemonInstanceId,
        server_identity: impl Into<String>,
    ) -> Self {
        Self {
            daemon_instance_id,
            server_identity: server_identity.into(),
            phase: crate::daemon::protocol::v2::DaemonPhase::InstallingHooks,
            hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
            next_accepted_seq: 1,
            bootstrap_fifo: std::collections::VecDeque::new(),
            fatal: false,
        }
    }

    pub fn phase(&self) -> crate::daemon::protocol::v2::DaemonPhase {
        self.phase
    }

    pub fn daemon_instance_id(&self) -> &crate::pane_state::DaemonInstanceId {
        &self.daemon_instance_id
    }

    #[cfg(test)]
    pub fn set_phase(&mut self, phase: crate::daemon::protocol::v2::DaemonPhase) {
        self.phase = phase;
    }

    pub fn begin_hydration(&mut self) -> Result<(), &'static str> {
        if self.phase != crate::daemon::protocol::v2::DaemonPhase::InstallingHooks {
            return Err("daemon may enter hydration only after hook installation");
        }
        self.phase = crate::daemon::protocol::v2::DaemonPhase::Hydrating;
        Ok(())
    }

    pub fn set_hook_health(&mut self, health: crate::daemon::protocol::v2::HookHealth) {
        self.hook_health = health;
    }

    pub fn hook_health(&self) -> crate::daemon::protocol::v2::HookHealth {
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
        message: crate::daemon::protocol::v2::ClientMessage,
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
            if self.phase != crate::daemon::protocol::v2::DaemonPhase::Serving {
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
        if self.phase != crate::daemon::protocol::v2::DaemonPhase::Serving
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
        if self.phase == crate::daemon::protocol::v2::DaemonPhase::Serving {
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
            crate::daemon::protocol::v2::DaemonPhase::Hydrating,
            "bootstrap may finish only from Hydrating"
        );
        let queued = self.bootstrap_fifo.drain(..).collect();
        apply_fifo_and_reconcile(queued)?;
        self.phase = crate::daemon::protocol::v2::DaemonPhase::Serving;
        Ok(())
    }

    pub(crate) fn take_bootstrap_fifo(&mut self) -> Vec<V2SequencedMutation> {
        assert_ne!(
            self.phase,
            crate::daemon::protocol::v2::DaemonPhase::Serving,
            "Serving router has no bootstrap FIFO"
        );
        self.bootstrap_fifo.drain(..).collect()
    }

    pub(crate) fn enter_serving_if_bootstrap_empty(&mut self) -> bool {
        if self.phase == crate::daemon::protocol::v2::DaemonPhase::Hydrating
            && self.bootstrap_fifo.is_empty()
        {
            self.phase = crate::daemon::protocol::v2::DaemonPhase::Serving;
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
        if self.phase != crate::daemon::protocol::v2::DaemonPhase::Serving
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
        if self.phase == crate::daemon::protocol::v2::DaemonPhase::Serving {
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
fn validate_v2_origin(
    message: &crate::daemon::protocol::v2::ClientMessage,
) -> std::result::Result<(), crate::daemon::protocol::v2::ServerMessage> {
    use crate::daemon::protocol::v2::{ClientMessage, ErrorCode, ServerMessage};
    match message {
        ClientMessage::SubmitPaneEvent { envelope, .. } if !envelope.event.is_external() => {
            Err(ServerMessage::error(
                ErrorCode::InvalidRequest,
                "pane event variant is internal-only",
                Some(envelope.event_id.clone()),
            ))
        }
        ClientMessage::SubmitViewEvent { event, .. } => event.validate().map_err(|error| {
            ServerMessage::error(
                ErrorCode::InvalidRequest,
                error.to_string(),
                Some(event.event_id.clone()),
            )
        }),
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

#[derive(Debug, Default)]
struct NotificationHealthCounters {
    failures: AtomicU64,
    degraded: AtomicBool,
    last_error_code: Mutex<Option<String>>,
}

struct SidebarTmuxJob {
    effect: super::runtime::CanonicalSidebarEffect,
    expected_pane: crate::pane_state::PaneInstance,
    original_accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    snapshot_revision: u64,
}

fn enqueue_sidebar_tmux_job(
    tx: &SyncSender<SidebarTmuxJob>,
    deferred_responses: &Mutex<BTreeSet<u64>>,
    job: SidebarTmuxJob,
) -> std::result::Result<(), crate::daemon::protocol::v2::ErrorCode> {
    let original_accepted_seq = job.original_accepted_seq;
    tx.try_send(job).map_err(|error| match error {
        TrySendError::Full(_) => crate::daemon::protocol::v2::ErrorCode::QueueFull,
        TrySendError::Disconnected(_) => crate::daemon::protocol::v2::ErrorCode::InternalError,
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
    event_id: crate::pane_state::EventId,
    snapshot_revision: u64,
    result: SidebarEffectResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SidebarEffectResult {
    Succeeded,
    ServerIncarnationMismatch,
    PaneInstanceMismatch,
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
    message: Arc<crate::daemon::protocol::v2::ServerMessage>,
    terminal: bool,
}

#[derive(Debug, Clone, Copy)]
enum StatusPushTrigger {
    Snapshot,
    RenderClock,
    Flush,
}

struct ProductionV2Coordinator {
    router: Mutex<V2Router>,
    state: Mutex<Option<super::runtime::CanonicalCoordinatorState>>,
    queue: Mutex<ProductionQueue>,
    queue_ready: Condvar,
    snapshot_cache: Mutex<Option<PublishedResolvedSnapshot>>,
    snapshot_changed: Condvar,
    waiters: Mutex<BTreeMap<u64, Sender<crate::daemon::protocol::v2::ServerMessage>>>,
    deferred_responses: Mutex<BTreeSet<u64>>,
    shutdown: AtomicBool,
    shutdown_ready: AtomicBool,
    incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
    env: std::collections::BTreeMap<String, String>,
    done_clear_on: crate::config::DoneClearOn,
    notification_tx: Option<SyncSender<NotificationWorkerJob>>,
    notification_shutdown: Arc<AtomicBool>,
    notification_process_lock: Arc<Mutex<()>>,
    notification_health: Arc<NotificationHealthCounters>,
    notification_queue_drops: AtomicU64,
    notification_internal_drops_reported: AtomicU64,
    recent_error_code: Mutex<Option<crate::daemon::protocol::v2::ErrorCode>>,
    quarantine_base_total: u64,
    quarantine_runtime_baseline: AtomicU64,
    quarantine_persisted_total: AtomicU64,
    sidebar_tmux_tx: SyncSender<SidebarTmuxJob>,
    sidebar_completion_rx: Mutex<Option<mpsc::Receiver<SidebarEffectCompletion>>>,
    status_push: Mutex<crate::daemon::status_push::StatusPushState>,
    status_push_driver: Mutex<()>,
    status_push_log: Mutex<()>,
    status_push_started: Instant,
    config_hash: Mutex<String>,
    projection_updated_at_epoch_seconds: AtomicU64,
    notification_enabled: bool,
}

#[cfg(test)]
fn start_notification_worker(command: String) -> SyncSender<NotificationWorkerJob> {
    start_notification_worker_with_timeout_and_log(command, Duration::from_secs(2), None, None)
}

fn start_sidebar_tmux_worker(
    env: &BTreeMap<String, String>,
    expected_server: crate::daemon::topology::ServerIdentity,
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
            let result = match job.effect {
                super::runtime::CanonicalSidebarEffect::JumpPane {
                    pane_instance,
                    client_pid,
                    source_pane,
                } => {
                    debug_assert_eq!(pane_instance, job.expected_pane);
                    io.jump_to_pane(&job.expected_pane, client_pid, &source_pane)
                }
            };
            let result = match result {
                Ok(()) => SidebarEffectResult::Succeeded,
                Err(crate::daemon::workers::SidebarTmuxError::ServerIncarnationMismatch) => {
                    SidebarEffectResult::ServerIncarnationMismatch
                }
                Err(crate::daemon::workers::SidebarTmuxError::PaneInstanceMismatch(_)) => {
                    SidebarEffectResult::PaneInstanceMismatch
                }
                Err(crate::daemon::workers::SidebarTmuxError::SourceClientMismatch) => {
                    SidebarEffectResult::SourceClientMismatch
                }
                Err(error) => SidebarEffectResult::Failed(error.to_string()),
            };
            let _ = completion_tx.send(SidebarEffectCompletion {
                original_accepted_seq: job.original_accepted_seq,
                event_id: job.event_id,
                snapshot_revision: job.snapshot_revision,
                result,
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
    health: Option<Arc<NotificationHealthCounters>>,
) -> SyncSender<NotificationWorkerJob> {
    start_notification_worker_with_control(
        command,
        timeout,
        log_context,
        health,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(())),
    )
}

fn start_notification_worker_with_control(
    command: String,
    timeout: Duration,
    log_context: Option<(std::collections::BTreeMap<String, String>, String)>,
    health: Option<Arc<NotificationHealthCounters>>,
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
                    note_notification_failure(health.as_ref(), "spawn_failed");
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
                        note_notification_failure(health.as_ref(), "identity_failed");
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
                note_notification_failure(health.as_ref(), "identity_persist_failed");
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
                            note_notification_failure(health.as_ref(), "nonzero_exit");
                            log_notification_failure(
                                log_context.as_ref(),
                                &format!(
                                    "notification command exited with status {status} for pane {}",
                                    job.pane_id
                                ),
                            );
                        } else if let Some(health) = &health {
                            health.degraded.store(false, Ordering::SeqCst);
                        }
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        terminate_notification_process_group(&mut child);
                        note_notification_failure(health.as_ref(), "timeout");
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
                        note_notification_failure(health.as_ref(), "wait_failed");
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

fn note_notification_failure(health: Option<&Arc<NotificationHealthCounters>>, code: &str) {
    let Some(health) = health else {
        return;
    };
    health.failures.fetch_add(1, Ordering::SeqCst);
    health.degraded.store(true, Ordering::SeqCst);
    *health
        .last_error_code
        .lock()
        .expect("notification health lock poisoned") = Some(code.to_string());
}

fn log_notification_failure(
    context: Option<&(std::collections::BTreeMap<String, String>, String)>,
    message: &str,
) {
    let Some((env, incarnation_hash)) = context else {
        eprintln!("[vde-tmux] {message}");
        return;
    };
    if crate::daemon::lifecycle::append_incarnation_log(
        env,
        incarnation_hash,
        "notification.log",
        message,
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
    fn new(
        incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
        env: std::collections::BTreeMap<String, String>,
        done_clear_on: crate::config::DoneClearOn,
        notification_command: Option<String>,
    ) -> Result<Self> {
        let notification_enabled = notification_command.is_some();
        let quarantine_base_total =
            crate::daemon::lifecycle::read_lifecycle_record(&env, &incarnation.hash)?
                .quarantine_observed_total;
        let notification_health = Arc::new(NotificationHealthCounters::default());
        let notification_shutdown = Arc::new(AtomicBool::new(false));
        let notification_process_lock = Arc::new(Mutex::new(()));
        let notification_tx = notification_command.map(|command| {
            start_notification_worker_with_control(
                command,
                Duration::from_secs(2),
                Some((env.clone(), incarnation.hash.clone())),
                Some(notification_health.clone()),
                notification_shutdown.clone(),
                notification_process_lock.clone(),
            )
        });
        let (sidebar_tmux_tx, sidebar_completion_rx) =
            start_sidebar_tmux_worker(&env, incarnation.identity.clone());
        let status_push = crate::daemon::status_push::StatusPushState::new(
            incarnation.identity.clone(),
            Duration::ZERO,
        )
        .map_err(|error| anyhow::anyhow!("failed to initialize status push state: {error}"))?;
        Ok(Self {
            router: Mutex::new(V2Router::new(
                crate::pane_state::DaemonInstanceId::generate()?,
                incarnation.hash.clone(),
            )),
            state: Mutex::new(None),
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
            done_clear_on,
            notification_tx,
            notification_shutdown,
            notification_process_lock,
            notification_health,
            notification_queue_drops: AtomicU64::new(0),
            notification_internal_drops_reported: AtomicU64::new(0),
            recent_error_code: Mutex::new(None),
            quarantine_base_total,
            quarantine_runtime_baseline: AtomicU64::new(0),
            quarantine_persisted_total: AtomicU64::new(quarantine_base_total),
            sidebar_tmux_tx,
            sidebar_completion_rx: Mutex::new(Some(sidebar_completion_rx)),
            status_push: Mutex::new(status_push),
            status_push_driver: Mutex::new(()),
            status_push_log: Mutex::new(()),
            status_push_started: Instant::now(),
            config_hash: Mutex::new(crate::daemon::lifecycle::config_hash(
                &crate::config::Config::default(),
            )),
            projection_updated_at_epoch_seconds: AtomicU64::new(epoch_seconds() as u64),
            notification_enabled,
        })
    }

    fn configure_health(&self, config: &crate::config::Config) {
        *self.config_hash.lock().expect("config hash lock poisoned") =
            crate::daemon::lifecycle::config_hash(config);
    }

    fn note_error_response(&self, response: &crate::daemon::protocol::v2::ServerMessage) {
        if let crate::daemon::protocol::v2::ServerMessage::Error { code, .. } = response {
            *self
                .recent_error_code
                .lock()
                .expect("recent error code lock poisoned") = Some(code.clone());
        }
    }

    fn sync_quarantine_summary(&self) {
        let runtime_observed = self
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_ref()
            .map_or(0, |state| state.leased.runtime.quarantine_observed_total());
        let observed = cumulative_quarantine_total(
            self.quarantine_base_total,
            self.quarantine_runtime_baseline.load(Ordering::SeqCst),
            runtime_observed,
        );
        let previous = self
            .quarantine_persisted_total
            .swap(observed, Ordering::SeqCst);
        if observed == previous {
            return;
        }
        if crate::daemon::lifecycle::update_lifecycle_record(
            &self.env,
            &self.incarnation.hash,
            |record| {
                record.quarantine_observed_total = observed;
                Ok(())
            },
        )
        .is_err()
        {
            self.quarantine_persisted_total
                .store(previous, Ordering::SeqCst);
        }
    }

    fn establish_quarantine_baseline(&self) {
        let runtime_observed = self
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_ref()
            .map_or(0, |state| state.leased.runtime.quarantine_observed_total());
        self.quarantine_runtime_baseline
            .store(runtime_observed, Ordering::SeqCst);
    }

    fn route_external(
        &self,
        connection: &mut V2ConnectionState,
        message: crate::daemon::protocol::v2::ClientMessage,
        raw_frame_bytes: usize,
    ) -> crate::daemon::protocol::v2::ServerMessage {
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
        if router.phase() == crate::daemon::protocol::v2::DaemonPhase::Serving
            && message.is_mutation()
        {
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
                    V2AcceptedMutation::External(
                        crate::daemon::protocol::v2::ClientMessage::SubmitViewEvent { .. }
                    )
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
        message: crate::daemon::protocol::v2::ClientMessage,
    ) -> Result<PublishedResolvedSnapshot, crate::daemon::protocol::v2::ServerMessage> {
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
            V2Route::Query(crate::daemon::protocol::v2::ClientMessage::Subscribe { .. }) => {
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
    ) -> mpsc::Receiver<crate::daemon::protocol::v2::ServerMessage> {
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

    fn query(
        &self,
        message: crate::daemon::protocol::v2::ClientMessage,
    ) -> crate::daemon::protocol::v2::ServerMessage {
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
                    if state.leased.runtime.sequenced_mutations_paused() {
                        return ServerMessage::error(
                            ErrorCode::NotReady,
                            "pane refresh is paused while persistence is recovering",
                            None,
                        );
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
            ClientMessage::QueryHealth { .. } => {
                let (revision, quarantine_count, internal_notification_drops) = self
                    .state
                    .lock()
                    .expect("canonical state lock poisoned")
                    .as_ref()
                    .map_or((0, 0, 0), |state| {
                        (
                            state.leased.runtime.snapshot_revision(),
                            state.leased.runtime.quarantine_count() as u64,
                            state.leased.runtime.notification_queue_drops(),
                        )
                    });
                let lifecycle = match crate::daemon::lifecycle::read_lifecycle_record(
                    &self.env,
                    &self.incarnation.hash,
                ) {
                    Ok(record) => record,
                    Err(error) => {
                        return ServerMessage::error(
                            ErrorCode::InternalError,
                            format!("failed to read lifecycle health: {error}"),
                            None,
                        );
                    }
                };
                ServerMessage::HealthResult {
                    health: crate::daemon::protocol::v2::DaemonHealth {
                        config_hash: self
                            .config_hash
                            .lock()
                            .expect("config hash lock poisoned")
                            .clone(),
                        projection_revision: revision,
                        projection_updated_at_epoch_seconds: self
                            .projection_updated_at_epoch_seconds
                            .load(Ordering::SeqCst),
                        notification_enabled: self.notification_enabled,
                        notification_failures: self
                            .notification_health
                            .failures
                            .load(Ordering::SeqCst),
                        notification_queue_drops: self
                            .notification_queue_drops
                            .load(Ordering::SeqCst)
                            .saturating_add(internal_notification_drops),
                        notification_degraded: self
                            .notification_health
                            .degraded
                            .load(Ordering::SeqCst),
                        last_notification_error_code: self
                            .notification_health
                            .last_error_code
                            .lock()
                            .expect("notification health lock poisoned")
                            .clone(),
                        current_quarantine_count: quarantine_count,
                        quarantine_observed_total: lifecycle.quarantine_observed_total,
                        recent_error_code: self
                            .recent_error_code
                            .lock()
                            .expect("recent error code lock poisoned")
                            .clone()
                            .or_else(|| {
                                (lifecycle.quarantine_observed_total > 0)
                                    .then_some(ErrorCode::StateLoadError)
                            }),
                        hook_delivery_failures: lifecycle.hook_delivery_failures,
                        hook_delivery_degraded: lifecycle.hook_delivery_degraded,
                        last_hook_error_code: lifecycle.last_hook_error_code,
                        status_push_failures: lifecycle.status_push_failures,
                        status_push_degraded: lifecycle.status_push_degraded,
                        last_status_push_error: lifecycle.last_status_push_error,
                        last_status_push_error_at_epoch_seconds: lifecycle
                            .last_status_push_error_at_epoch_seconds,
                    },
                }
            }
            _ => ServerMessage::error(ErrorCode::InvalidRequest, "unsupported query", None),
        }
    }

    fn enqueue_internal_and_wait(
        &self,
        mutation: V2InternalMutation,
    ) -> crate::daemon::protocol::v2::ServerMessage {
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

    fn complete(&self, accepted_seq: u64, response: crate::daemon::protocol::v2::ServerMessage) {
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
        let candidate = crate::daemon::protocol::v2::ServerMessage::ResolvedSnapshotResult {
            snapshot_revision: revision,
            snapshot,
        };
        let (message, frame, terminal) =
            match crate::daemon::protocol::v2::encode_response_frame(&candidate) {
                Ok(frame) => (candidate, frame, false),
                Err(
                    error @ crate::daemon::protocol::v2::ServerMessage::Error {
                        code: crate::daemon::protocol::v2::ErrorCode::FrameTooLarge,
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
        self.projection_updated_at_epoch_seconds
            .store(epoch_seconds() as u64, Ordering::SeqCst);
        drop(cache);
        self.snapshot_changed.notify_all();
        if terminal {
            let _ = self.enqueue_internal(V2InternalMutation::FrameTooLargeProjection {
                rejected_revision: revision,
            });
        }
        Ok(published)
    }

    fn wait_for_snapshot_after(&self, revision: u64) -> Option<PublishedResolvedSnapshot> {
        let mut cache = self
            .snapshot_cache
            .lock()
            .expect("snapshot cache lock poisoned");
        loop {
            if let Some(published) = cache.as_ref()
                && published.revision > revision
            {
                return Some(published.clone());
            }
            if self.shutdown.load(Ordering::SeqCst) {
                return None;
            }
            let (next, timeout) = self
                .snapshot_changed
                .wait_timeout(cache, Duration::from_secs(2))
                .expect("snapshot cache lock poisoned while waiting");
            cache = next;
            if timeout.timed_out()
                && let Some(published) = cache.as_ref()
            {
                return Some(published.clone());
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
            StatusPushTrigger::Snapshot | StatusPushTrigger::RenderClock => {
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
                let frame =
                    build_display_frame(&config, &global, &sessions, &panes, epoch_seconds())
                        .map_err(anyhow::Error::new)?;
                let mut push = self.status_push.lock().expect("status push lock poisoned");
                match trigger {
                    StatusPushTrigger::Snapshot => {
                        push.on_snapshot_revision(global.snapshot_revision, now, frame)
                    }
                    StatusPushTrigger::RenderClock => push.on_render_clock(now, frame),
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
            BatchExecution::Committed => {
                let _ = crate::daemon::lifecycle::record_status_push_recovered(
                    &self.env,
                    &self.incarnation.hash,
                );
                Ok(())
            }
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
        let _log = self
            .status_push_log
            .lock()
            .expect("status push log lock poisoned");
        let _ = crate::daemon::lifecycle::record_status_push_failure(
            &self.env,
            &self.incarnation.hash,
            message,
        );
        if crate::daemon::lifecycle::append_incarnation_log(
            &self.env,
            &self.incarnation.hash,
            "status-push.log",
            message,
        )
        .is_err()
        {
            eprintln!("[vde-tmux] {message}");
        }
    }

    fn log_daemon_error(&self, message: &str) {
        if crate::daemon::lifecycle::append_incarnation_log(
            &self.env,
            &self.incarnation.hash,
            "daemon.log",
            message,
        )
        .is_err()
        {
            eprintln!("[vde-tmux] {message}");
        }
    }

    fn schedule_sidebar_effect(
        &self,
        effect: super::runtime::CanonicalSidebarEffect,
        original_accepted_seq: u64,
        event_id: crate::pane_state::EventId,
        snapshot_revision: u64,
    ) -> std::result::Result<(), crate::daemon::protocol::v2::ErrorCode> {
        let expected_pane = match &effect {
            super::runtime::CanonicalSidebarEffect::JumpPane { pane_instance, .. } => {
                pane_instance.clone()
            }
        };
        let exists = self
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_ref()
            .is_some_and(|state| {
                state
                    .resolved_snapshot()
                    .panes
                    .iter()
                    .any(|pane| pane.pane_instance == expected_pane)
            });
        if !exists {
            return Err(crate::daemon::protocol::v2::ErrorCode::StaleSelection);
        }
        enqueue_sidebar_tmux_job(
            &self.sidebar_tmux_tx,
            &self.deferred_responses,
            SidebarTmuxJob {
                effect,
                expected_pane,
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
            let _ = waiter.send(crate::daemon::protocol::v2::ServerMessage::error(
                crate::daemon::protocol::v2::ErrorCode::InternalError,
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
            let _ = waiter.send(crate::daemon::protocol::v2::ServerMessage::error(
                crate::daemon::protocol::v2::ErrorCode::NotReady,
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

fn cumulative_quarantine_total(base: u64, runtime_baseline: u64, runtime_observed: u64) -> u64 {
    base.saturating_add(runtime_observed.saturating_sub(runtime_baseline))
}

fn handle_v2_runtime_stream(
    coordinator: Arc<ProductionV2Coordinator>,
    stream: UnixStream,
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
    let track_error = message.is_mutation();
    let response = coordinator.route_external(&mut connection_state, message, frame.len());
    if track_error {
        coordinator.note_error_response(&response);
    }
    write_v2_response(connection.stream_mut(), &response)
        .map_err(|error| anyhow::anyhow!("failed to write v2 handshake: {error:?}"))?;
    if !matches!(
        response,
        crate::daemon::protocol::v2::ServerMessage::HelloAck { .. }
    ) {
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
    let subscribe = matches!(
        &message,
        crate::daemon::protocol::v2::ClientMessage::Subscribe { .. }
    );
    if subscribe {
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
    let track_error = message.is_mutation();
    let response = coordinator.route_external(&mut connection_state, message, frame.len());
    if track_error {
        coordinator.note_error_response(&response);
    }
    let _ = write_v2_response(connection.stream_mut(), &response);
    Ok(())
}

fn stream_v2_subscription(
    coordinator: Arc<ProductionV2Coordinator>,
    mut stream: UnixStream,
    mut last_revision: u64,
) -> Result<()> {
    while let Some(published) = coordinator.wait_for_snapshot_after(last_revision) {
        if let Err(error) = write_v2_frame(&mut stream, &published.frame) {
            let _ = coordinator.enqueue_internal(V2InternalMutation::DiagnosticProjection {
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
                V2AcceptedMutation::External(
                    crate::daemon::protocol::v2::ClientMessage::Shutdown { .. }
                )
            );
            let changes_topology_targets = mutation_changes_topology_targets(&mutation.sequenced);
            let status_driver = changes_topology_targets.then(|| {
                coordinator
                    .status_push_driver
                    .lock()
                    .expect("status push driver lock poisoned")
            });
            let response = apply_production_mutation(&coordinator, mutation.sequenced);
            coordinator.sync_quarantine_summary();
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

fn mutation_changes_topology_targets(mutation: &V2SequencedMutation) -> bool {
    match &mutation.mutation {
        V2AcceptedMutation::External(
            crate::daemon::protocol::v2::ClientMessage::RefreshPanes { .. }
            | crate::daemon::protocol::v2::ClientMessage::RefreshTopology { .. },
        )
        | V2AcceptedMutation::Internal(
            V2InternalMutation::ObservationPollProjection(_)
            | V2InternalMutation::RefreshTopology
            | V2InternalMutation::TargetedPaneRefresh { .. },
        ) => true,
        V2AcceptedMutation::External(
            crate::daemon::protocol::v2::ClientMessage::SubmitPaneEvent { envelope, .. },
        ) => {
            matches!(
                envelope.event,
                crate::pane_state::PaneEvent::PaneRemoved { .. }
            )
        }
        V2AcceptedMutation::Internal(V2InternalMutation::PaneEvent(envelope)) => matches!(
            envelope.event,
            crate::pane_state::PaneEvent::PaneRemoved { .. }
        ),
        _ => false,
    }
}

fn start_canonical_observation_worker(coordinator: Arc<ProductionV2Coordinator>, poll: Duration) {
    thread::spawn(move || {
        let capture_io = crate::daemon::workers::SystemObservationWorkerIo::new(
            coordinator
                .env
                .get("VDE_TMUX_SOCKET_NAME")
                .cloned()
                .filter(|value| !value.trim().is_empty()),
        );
        let mut last_hook_check = Instant::now();
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let (dispatch, view_base) = {
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
                                epoch_seconds(),
                                crate::pane_state::AgentPresenceObservation::Unknown,
                                None,
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
            let processes =
                crate::daemon::workers::read_agent_process_snapshot(Duration::from_secs(1));
            match crate::daemon::workers::run_observation_poll(
                &capture_io,
                &dispatch,
                &processes,
                &daemon_instance_id,
                &coordinator.incarnation.identity,
                epoch_seconds(),
            ) {
                Ok(result) => {
                    let current = projection
                        .topology
                        .panes
                        .iter()
                        .map(|pane| pane.pane_instance.clone())
                        .collect::<std::collections::BTreeSet<_>>();
                    let _ = coordinator.enqueue_internal(
                        V2InternalMutation::ObservationPollProjection(Box::new(projection)),
                    );
                    for envelope in result.envelopes {
                        if !coordinator
                            .enqueue_internal(V2InternalMutation::PaneEvent(Box::new(envelope)))
                        {
                            break;
                        }
                    }
                    match crate::daemon::workers::pane_removal_envelopes(
                        &daemon_instance_id,
                        &dispatch,
                        &current,
                        true,
                    ) {
                        Ok(removals) => {
                            for envelope in removals {
                                if !coordinator.enqueue_internal(V2InternalMutation::PaneEvent(
                                    Box::new(envelope),
                                )) {
                                    break;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = coordinator.enqueue_internal(
                                V2InternalMutation::DiagnosticProjection {
                                    pane_instance: dispatch
                                        .first()
                                        .map(|snapshot| snapshot.pane_instance.clone()),
                                    message: format!("pane_removal_build_failed: {error}"),
                                },
                            );
                        }
                    }
                    for message in result.diagnostics {
                        let _ = coordinator.enqueue_internal(
                            V2InternalMutation::DiagnosticProjection {
                                pane_instance: dispatch
                                    .first()
                                    .map(|snapshot| snapshot.pane_instance.clone()),
                                message,
                            },
                        );
                    }
                    let _ = coordinator.enqueue_internal(V2InternalMutation::TriageProjection);
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
                        if health == crate::daemon::protocol::v2::HookHealth::Healthy {
                            let _ = crate::daemon::lifecycle::record_hook_delivery_recovered(
                                &coordinator.env,
                                &coordinator.incarnation.hash,
                            );
                        }
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
                                health: crate::daemon::protocol::v2::HookHealth::Degraded,
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

fn start_canonical_git_worker(coordinator: Arc<ProductionV2Coordinator>, poll: Duration) {
    thread::spawn(move || {
        let git = crate::daemon::workers::system_git_runner(Duration::from_millis(500));
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let paths = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned")
                .as_ref()
                .map(|state| {
                    state
                        .resolved_snapshot()
                        .panes
                        .into_iter()
                        .filter(|pane| pane.resolved.is_some())
                        .map(|pane| pane.current_path)
                        .filter(|path| !path.trim().is_empty())
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            let badges =
                crate::git::collect_git_badges_for_paths(&git, paths.iter().map(String::as_str));
            let worktrees = crate::git::collect_worktree_infos_for_paths(
                &git,
                paths.iter().map(String::as_str),
            );
            let _ = coordinator
                .enqueue_internal(V2InternalMutation::GitProjection { badges, worktrees });
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
            for trigger in [
                StatusPushTrigger::Snapshot,
                StatusPushTrigger::RenderClock,
                StatusPushTrigger::Flush,
            ] {
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

fn apply_production_mutation(
    coordinator: &ProductionV2Coordinator,
    sequenced: V2SequencedMutation,
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::{ClientMessage, ErrorCode, ServerMessage};

    let accepted_seq = sequenced.accepted_seq;
    let response = match sequenced.mutation {
        V2AcceptedMutation::External(ClientMessage::SubmitPaneEvent { envelope, .. }) => {
            apply_external_pane_event(coordinator, accepted_seq, envelope)
        }
        V2AcceptedMutation::External(ClientMessage::SubmitViewEvent { event, .. }) => {
            apply_external_view_event(coordinator, accepted_seq, event)
        }
        V2AcceptedMutation::External(ClientMessage::RefreshPanes { event_id, .. })
        | V2AcceptedMutation::External(ClientMessage::RefreshTopology { event_id, .. }) => {
            match refresh_full_topology(coordinator) {
                Ok(revision) => ServerMessage::SnapshotAck {
                    event_id,
                    accepted_seq,
                    snapshot_revision: revision,
                },
                Err(error) => production_store_error_response(coordinator, error, Some(event_id)),
            }
        }
        V2AcceptedMutation::External(ClientMessage::ResetPaneState {
            event_id,
            pane_instance,
            expected,
            ..
        }) => apply_reset(coordinator, accepted_seq, event_id, pane_instance, expected),
        V2AcceptedMutation::External(ClientMessage::CleanupLegacyState {
            event_id,
            dry_run,
            ..
        }) => apply_legacy_cleanup(coordinator, accepted_seq, event_id, dry_run),
        V2AcceptedMutation::External(ClientMessage::SidebarCommand {
            event_id, command, ..
        }) => match command {
            crate::daemon::protocol::v2::SidebarCommand::MarkComplete {
                pane_instance,
                expected,
            } => {
                let envelope = crate::pane_state::PaneEventEnvelope {
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
                    event: crate::pane_state::PaneEvent::MarkDone {
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
            crate::daemon::protocol::v2::SidebarCommand::UpdateManualOrder {
                expected_version,
                manual_order,
                manual_chat_order,
            } => apply_sidebar_order_command(
                coordinator,
                accepted_seq,
                event_id,
                expected_version,
                manual_order,
                manual_chat_order,
            ),
            crate::daemon::protocol::v2::SidebarCommand::UpdateViewPreferences {
                expected_version,
                view_mode,
                filter,
            } => apply_sidebar_view_preferences_command(
                coordinator,
                accepted_seq,
                event_id,
                expected_version,
                view_mode,
                filter,
            ),
            crate::daemon::protocol::v2::SidebarCommand::SetExpansionOverride {
                expected_version,
                row_id,
                overridden,
            } => apply_sidebar_expansion_command(
                coordinator,
                accepted_seq,
                event_id,
                expected_version,
                row_id,
                overridden,
            ),
        },
        V2AcceptedMutation::External(ClientMessage::UninstallHooks { event_id, .. }) => {
            let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
            match crate::daemon::view_hooks::uninstall_hooks(
                &runner,
                &coordinator.incarnation.identity,
            ) {
                Ok(()) => ServerMessage::HooksUninstalled {
                    event_id,
                    accepted_seq,
                },
                Err(error) => ServerMessage::error(
                    ErrorCode::HookCollision,
                    error.to_string(),
                    Some(event_id),
                ),
            }
        }
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
            | ClientMessage::QueryHealth { .. }
            | ClientMessage::Subscribe { .. },
        ) => unreachable!("v2 router cannot sequence a read-only request"),
        V2AcceptedMutation::Internal(V2InternalMutation::TargetedPaneRefresh { pane_id }) => {
            targeted_pane_refresh_response(coordinator, &pane_id)
        }
        V2AcceptedMutation::Internal(V2InternalMutation::ObservationPollProjection(projection)) => {
            match apply_observation_poll_projection(coordinator, *projection) {
                Ok(revision) => {
                    coordinator
                        .projection_updated_at_epoch_seconds
                        .store(epoch_seconds() as u64, Ordering::SeqCst);
                    ServerMessage::SnapshotAck {
                        event_id: crate::pane_state::EventId::generate()
                            .expect("OS random source failed after daemon startup"),
                        accepted_seq,
                        snapshot_revision: revision,
                    }
                }
                Err(error) => observation_poll_error_response(coordinator, error),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::RefreshTopology) => {
            match refresh_full_topology(coordinator) {
                Ok(revision) => ServerMessage::SnapshotAck {
                    event_id: crate::pane_state::EventId::generate()
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
                        event_id: crate::pane_state::EventId::generate()
                            .expect("OS random source failed after daemon startup"),
                        accepted_seq,
                        snapshot_revision: revision,
                    }
                }
                Err(error) => {
                    ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
                }
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::PaneEvent(envelope)) => {
            apply_external_pane_event(coordinator, accepted_seq, *envelope)
        }
        V2AcceptedMutation::Internal(V2InternalMutation::DiagnosticProjection {
            pane_instance,
            message,
        }) => {
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized before diagnostic");
            let result = if let Some(pane) = pane_instance {
                state.leased.runtime.add_diagnostic(pane, message)
            } else {
                state
                    .add_global_diagnostic(ErrorCode::InternalError, message)
                    .map(|_| ())
            };
            if let Err(error) = result {
                return production_store_error_response(coordinator, error, None);
            }
            ServerMessage::SnapshotAck {
                event_id: crate::pane_state::EventId::generate()
                    .expect("OS random source failed after daemon startup"),
                accepted_seq,
                snapshot_revision: state.leased.runtime.snapshot_revision(),
            }
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
                event_id: crate::pane_state::EventId::generate()
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
                event_id: crate::pane_state::EventId::generate()
                    .expect("OS random source failed after daemon startup"),
                accepted_seq,
                snapshot_revision: state.leased.runtime.snapshot_revision(),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::SidebarEffectCompleted(completion)) => {
            let fail_stop = matches!(
                completion.result,
                SidebarEffectResult::ServerIncarnationMismatch
            );
            let original_response = match completion.result {
                SidebarEffectResult::Succeeded => ServerMessage::SnapshotAck {
                    event_id: completion.event_id.clone(),
                    accepted_seq: completion.original_accepted_seq,
                    snapshot_revision: completion.snapshot_revision,
                },
                SidebarEffectResult::PaneInstanceMismatch => ServerMessage::error(
                    ErrorCode::StaleSelection,
                    "sidebar pane selection became stale before tmux mutation",
                    Some(completion.event_id.clone()),
                ),
                SidebarEffectResult::SourceClientMismatch => ServerMessage::error(
                    ErrorCode::StaleSelection,
                    "source sidebar focus changed before tmux mutation",
                    Some(completion.event_id.clone()),
                ),
                SidebarEffectResult::ServerIncarnationMismatch => ServerMessage::error(
                    ErrorCode::InternalError,
                    "tmux server incarnation changed during sidebar command",
                    Some(completion.event_id.clone()),
                ),
                SidebarEffectResult::Failed(message) => {
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
        V2AcceptedMutation::Internal(V2InternalMutation::TriageProjection) => {
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized before triage projection");
            if let Err(error) = state.leased.runtime.advance_poll_projection() {
                return production_store_error_response(coordinator, error, None);
            }
            ServerMessage::SnapshotAck {
                event_id: crate::pane_state::EventId::generate()
                    .expect("OS random source failed after daemon startup"),
                accepted_seq,
                snapshot_revision: state.leased.runtime.snapshot_revision(),
            }
        }
        V2AcceptedMutation::Internal(V2InternalMutation::GitProjection { badges, worktrees }) => {
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized before git projection");
            if let Err(error) = state.replace_git_projection(badges, worktrees) {
                return production_store_error_response(coordinator, error, None);
            }
            ServerMessage::SnapshotAck {
                event_id: crate::pane_state::EventId::generate()
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

fn unique_eligible_client_pid(
    views: &crate::daemon::view_hooks::ViewRegistry,
    source_pane: &crate::pane_state::PaneInstance,
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

fn apply_sidebar_order_command(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    expected_version: u64,
    manual_order: Vec<crate::sidebar::state::RepoId>,
    manual_chat_order: Vec<String>,
) -> crate::daemon::protocol::v2::ServerMessage {
    let path = crate::sidebar::store::state_path(&coordinator.env);
    apply_sidebar_preferences_result(
        coordinator,
        accepted_seq,
        event_id,
        expected_version,
        crate::sidebar::store::compare_and_swap_order(
            &path,
            expected_version,
            manual_order,
            manual_chat_order,
        ),
    )
}

fn apply_sidebar_view_preferences_command(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    expected_version: u64,
    view_mode: crate::sidebar::state::ViewMode,
    filter: crate::sidebar::state::StatusFilter,
) -> crate::daemon::protocol::v2::ServerMessage {
    let path = crate::sidebar::store::state_path(&coordinator.env);
    apply_sidebar_preferences_result(
        coordinator,
        accepted_seq,
        event_id,
        expected_version,
        crate::sidebar::store::compare_and_swap_view_preferences(
            &path,
            expected_version,
            view_mode,
            filter,
        ),
    )
}

fn apply_sidebar_expansion_command(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    expected_version: u64,
    row_id: String,
    overridden: bool,
) -> crate::daemon::protocol::v2::ServerMessage {
    let path = crate::sidebar::store::expansion_state_path(&coordinator.env);
    apply_sidebar_expansion_result(
        coordinator,
        accepted_seq,
        event_id,
        expected_version,
        crate::sidebar::store::compare_and_swap_expansion_override(
            &path,
            expected_version,
            row_id,
            overridden,
        ),
    )
}

fn apply_sidebar_expansion_result(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    expected_version: u64,
    result: std::result::Result<
        crate::sidebar::state::SidebarExpansionPreferences,
        crate::sidebar::store::OrderUpdateError,
    >,
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    let path = crate::sidebar::store::expansion_state_path(&coordinator.env);
    let candidate = match result {
        Ok(candidate) => candidate,
        Err(crate::sidebar::store::OrderUpdateError::Busy) => {
            return ServerMessage::error(
                ErrorCode::QueueFull,
                "sidebar expansion state is being updated by another tmux server",
                Some(event_id),
            );
        }
        Err(crate::sidebar::store::OrderUpdateError::Stale { current_version }) => {
            if let Ok(persisted) = crate::sidebar::store::load_expansion_state(&path) {
                let mut state_guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                if let Some(state) = state_guard.as_mut()
                    && let Err(error) = state.replace_sidebar_expansion_preferences(persisted)
                {
                    return production_store_error_response(coordinator, error, Some(event_id));
                }
            }
            return ServerMessage::error(
                ErrorCode::StaleStateIdentity,
                format!(
                    "sidebar expansion version is stale: expected {expected_version}, current {current_version}"
                ),
                Some(event_id),
            );
        }
        Err(crate::sidebar::store::OrderUpdateError::Storage(error)) => {
            let message = format!("sidebar expansion persistence failed: {error:#}");
            coordinator.log_daemon_error(&message);
            return ServerMessage::error(ErrorCode::PersistFailed, message, Some(event_id));
        }
    };
    let snapshot_revision = {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard
            .as_mut()
            .expect("state initialized before sidebar expansion command");
        if let Err(error) = state.replace_sidebar_expansion_preferences(candidate) {
            return production_store_error_response(coordinator, error, Some(event_id));
        }
        state.leased.runtime.snapshot_revision()
    };
    ServerMessage::SnapshotAck {
        event_id,
        accepted_seq,
        snapshot_revision,
    }
}

fn apply_sidebar_preferences_result(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    expected_version: u64,
    result: std::result::Result<
        crate::sidebar::state::SidebarOrderPreferences,
        crate::sidebar::store::OrderUpdateError,
    >,
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    let path = crate::sidebar::store::state_path(&coordinator.env);
    let candidate = match result {
        Ok(candidate) => candidate,
        Err(crate::sidebar::store::OrderUpdateError::Busy) => {
            return ServerMessage::error(
                ErrorCode::QueueFull,
                "sidebar preferences are being updated by another tmux server",
                Some(event_id),
            );
        }
        Err(crate::sidebar::store::OrderUpdateError::Stale { current_version }) => {
            if let Ok(persisted) = crate::sidebar::store::load_state(&path) {
                let mut state_guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                if let Some(state) = state_guard.as_mut()
                    && let Err(error) = state.replace_sidebar_order_preferences(persisted)
                {
                    return production_store_error_response(coordinator, error, Some(event_id));
                }
            }
            return ServerMessage::error(
                ErrorCode::StaleStateIdentity,
                format!(
                    "sidebar preference version is stale: expected {expected_version}, current {current_version}"
                ),
                Some(event_id),
            );
        }
        Err(crate::sidebar::store::OrderUpdateError::Storage(error)) => {
            let message = format!("sidebar preference persistence failed: {error:#}");
            coordinator.log_daemon_error(&message);
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            if let Some(state) = state_guard.as_mut() {
                let _ = state.add_global_diagnostic(ErrorCode::PersistFailed, message.clone());
            }
            return ServerMessage::error(ErrorCode::PersistFailed, message, Some(event_id));
        }
    };
    let snapshot_revision = {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard
            .as_mut()
            .expect("state initialized before sidebar command");
        if let Err(error) = state.replace_sidebar_order_preferences(candidate) {
            return production_store_error_response(coordinator, error, Some(event_id));
        }
        state.leased.runtime.snapshot_revision()
    };
    ServerMessage::SnapshotAck {
        event_id,
        accepted_seq,
        snapshot_revision,
    }
}

fn apply_external_pane_event(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    envelope: crate::pane_state::PaneEventEnvelope,
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::{PaneApplyOutcome, ServerMessage};
    use crate::pane_state::store::{PendingResolution, StoreError};

    let event_id = envelope.event_id.clone();
    if let crate::pane_state::PaneEvent::PaneRemoved { expected } = &envelope.event {
        return apply_pane_removal(
            coordinator,
            accepted_seq,
            event_id,
            envelope.pane_instance,
            expected.clone(),
        );
    }
    let (visibility, visibility_diagnostic) =
        match completion_visibility_for_event(coordinator, &envelope) {
            Ok(value) => value,
            Err(error) => {
                coordinator.fail_stop(error.to_string());
                return production_store_error_response(coordinator, error, Some(event_id));
            }
        };
    let mut clock = crate::pane_state::store::SystemRecoveryClock::start();
    let (initial, revision_before) = {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let Some(state) = state_guard.as_mut() else {
            return crate::daemon::protocol::v2::ServerMessage::error(
                crate::daemon::protocol::v2::ErrorCode::NotReady,
                "daemon is hydrating",
                Some(event_id),
            );
        };
        let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
        let mut io = crate::pane_state::store::TmuxPaneStateStoreIo::new(
            &runner,
            coordinator.incarnation.identity.pid,
            coordinator.incarnation.identity.start_time,
        );
        let revision_before = state.leased.runtime.snapshot_revision();
        let initial = state.leased.runtime.apply_event(
            &mut io,
            &mut clock,
            &envelope,
            &visibility,
            coordinator.done_clear_on,
        );
        let initial = initial.and_then(|result| {
            finish_pane_event_projection(
                coordinator,
                state,
                &envelope.pane_instance,
                visibility_diagnostic.as_deref(),
                revision_before,
                result,
            )
        });
        (initial, revision_before)
    };
    let result = match initial {
        Ok(result) => Ok(result),
        Err(StoreError::PersistPending) => loop {
            thread::sleep(crate::pane_state::store::STORE_RECOVERY_RETRY_INTERVAL);
            let resolved = {
                let mut state_guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                let state = state_guard
                    .as_mut()
                    .expect("state initialized before recovery");
                let timeout = bounded_recovery_timeout(
                    state.leased.runtime.pending_recovery_remaining(&clock),
                );
                let runner = crate::tmux::SystemTmuxRunner::from_env(timeout);
                let mut io = crate::pane_state::store::TmuxPaneStateStoreIo::new(
                    &runner,
                    coordinator.incarnation.identity.pid,
                    coordinator.incarnation.identity.start_time,
                );
                match state.leased.runtime.resolve_pending(&mut io, &clock) {
                    Ok(PendingResolution::Applied(result)) => finish_pane_event_projection(
                        coordinator,
                        state,
                        &envelope.pane_instance,
                        visibility_diagnostic.as_deref(),
                        revision_before,
                        result,
                    )
                    .map(PendingResolution::Applied),
                    other => other,
                }
            };
            match resolved {
                Ok(PendingResolution::StillPending) => continue,
                Ok(PendingResolution::Applied(result)) => break Ok(result),
                Ok(PendingResolution::Reset(_)) => unreachable!("pane event cannot resolve reset"),
                Err(error) => break Err(error),
            }
        },
        Err(error) => Err(error),
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

fn finish_pane_event_projection(
    coordinator: &ProductionV2Coordinator,
    state: &mut super::runtime::CanonicalCoordinatorState,
    pane: &crate::pane_state::PaneInstance,
    visibility_diagnostic: Option<&str>,
    revision_before: u64,
    mut result: crate::pane_state::store::ApplyResult,
) -> Result<crate::pane_state::store::ApplyResult, crate::pane_state::store::StoreError> {
    let mut messages = visibility_diagnostic
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let internal_drops = state.leased.runtime.notification_queue_drops();
    let reported = coordinator
        .notification_internal_drops_reported
        .swap(internal_drops, Ordering::SeqCst);
    if internal_drops > reported {
        coordinator.log_daemon_error(&format!(
            "notification queue overflow dropped {} oldest job(s)",
            internal_drops - reported
        ));
    }
    for notification in state.leased.runtime.drain_notification_jobs() {
        let agent = match state.leased.runtime.record(&notification.pane_instance) {
            Some(crate::pane_state::StoredPaneRecord::Active(active))
                if active.version() == notification.state_version =>
            {
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
            coordinator
                .notification_queue_drops
                .fetch_add(1, Ordering::SeqCst);
            let reason = match error {
                TrySendError::Full(_) => "queue_full",
                TrySendError::Disconnected(_) => "worker_disconnected",
            };
            note_notification_failure(Some(&coordinator.notification_health), reason);
            messages.push(format!(
                "notification_dispatch_failed: pane={} reason={reason}",
                notification.pane_instance.pane_id
            ));
            coordinator.log_daemon_error(&format!(
                "notification dispatch failed for pane {}: {reason}",
                notification.pane_instance.pane_id
            ));
        }
    }
    result.snapshot_revision = state.leased.runtime.finish_sequenced_projection(
        Some(pane),
        messages,
        false,
        revision_before,
    )?;
    let _ = state.checked_resolved_snapshot()?;
    Ok(result)
}

fn apply_pane_removal(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    pane: crate::pane_state::PaneInstance,
    expected: Option<crate::pane_state::StoredStateDescriptor>,
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::{PaneApplyOutcome, ServerMessage};
    let topology = match query_full_topology(coordinator, Duration::from_millis(100)) {
        Ok(topology) => topology,
        Err(error) => {
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            return ServerMessage::error(
                crate::daemon::protocol::v2::ErrorCode::InternalError,
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
                .and_then(|record| match record {
                    crate::pane_state::StoredPaneRecord::Active(state) => Some(state.version()),
                    crate::pane_state::StoredPaneRecord::Reset(_) => None,
                }),
            snapshot_revision: state.leased.runtime.snapshot_revision(),
            outcome: PaneApplyOutcome::Noop,
        };
    }
    let removed = match state
        .leased
        .runtime
        .remove_absent_pane(&pane, expected.as_ref())
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

fn completion_visibility_for_event(
    coordinator: &ProductionV2Coordinator,
    envelope: &crate::pane_state::PaneEventEnvelope,
) -> Result<
    (crate::pane_state::VisibilitySnapshot, Option<String>),
    crate::pane_state::store::StoreError,
> {
    use crate::pane_state::{
        AgentPresenceObservation, CaptureInference, LifecycleState, PaneEvent, ReportedLifecycle,
        StoredPaneRecord,
    };

    let (current, tracker) = {
        let state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard.as_ref();
        let current = state
            .and_then(|state| state.leased.runtime.record(&envelope.pane_instance))
            .and_then(|record| match record {
                StoredPaneRecord::Active(state) => Some(state.clone()),
                StoredPaneRecord::Reset(_) => None,
            });
        let tracker = state
            .map(|state| state.leased.runtime.tracker(&envelope.pane_instance))
            .unwrap_or_default();
        (current, tracker)
    };
    let may_complete = match &envelope.event {
        PaneEvent::CompleteRun { .. } => current.as_ref().is_none_or(|state| {
            state.run_seq > state.completed_seq || state.synthetic_completion_armed
        }),
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
            let absence_evidence = match presence {
                AgentPresenceObservation::Absent => true,
                AgentPresenceObservation::Present(kind) => kind != &state.agent,
                AgentPresenceObservation::Unknown => false,
            };
            let confirmed_absence_can_complete = absence_evidence
                && tracker.absence_count >= 1
                && state.scan_verified
                && !matches!(state.lifecycle, LifecycleState::Idle);
            let stale_capture_can_complete =
                matches!(
                    capture,
                    Some(crate::pane_state::CaptureObservation {
                        inference: CaptureInference::StaleRunCompleted,
                        ..
                    })
                ) && matches!(state.lifecycle, LifecycleState::Running);
            confirmed_absence_can_complete || stale_capture_can_complete
        }),
        _ => false,
    };
    if !may_complete {
        return Ok((crate::pane_state::VisibilitySnapshot::default(), None));
    }
    let mut diagnostic = None;
    let window_id = if coordinator.done_clear_on == crate::config::DoneClearOn::Window {
        match query_full_topology(
            coordinator,
            crate::daemon::view_hooks::FRESH_VISIBILITY_TIMEOUT,
        ) {
            Ok(topology) => match completion_window_id(&topology, &envelope.pane_instance) {
                Some(window_id) => Some(window_id.to_string()),
                None => {
                    diagnostic = Some("fresh_visibility_target_missing".to_string());
                    None
                }
            },
            Err(error) if error.requires_daemon_exit() => {
                return Err(crate::pane_state::store::StoreError::FailStop(
                    error.to_string(),
                ));
            }
            Err(error) => {
                diagnostic = Some(format!("fresh_visibility_topology_unavailable: {error}"));
                None
            }
        }
    } else {
        None
    };
    let io = crate::daemon::view_hooks::SystemFreshVisibilityIo::new(
        coordinator
            .env
            .get("VDE_TMUX_SOCKET_NAME")
            .cloned()
            .filter(|value| !value.trim().is_empty()),
        coordinator.incarnation.identity.clone(),
    );
    match crate::daemon::view_hooks::completion_visibility(
        &io,
        &envelope.pane_instance,
        window_id.as_deref(),
    ) {
        Ok(result) => Ok((result.snapshot, result.diagnostic.or(diagnostic))),
        Err(error) => Err(crate::pane_state::store::StoreError::FailStop(
            error.to_string(),
        )),
    }
}

fn completion_window_id<'a>(
    topology: &'a crate::daemon::topology::TopologySnapshot,
    pane: &crate::pane_state::PaneInstance,
) -> Option<&'a str> {
    topology
        .panes
        .iter()
        .find(|candidate| candidate.pane_instance == *pane)
        .map(|candidate| candidate.window_id.as_str())
}

fn apply_external_view_event(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event: crate::pane_state::ViewEvent,
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::{
        ErrorCode, PaneMutationFailure, ServerMessage, ViewApplyResult,
    };
    use crate::pane_state::store::{ViewBatchApplyResult, ViewBatchProgress};

    let event_id = event.event_id.clone();
    let scoped_refresh = match scoped_view_refresh(coordinator, &event) {
        Ok(refresh) => refresh,
        Err(error) if error.1 => {
            coordinator.fail_stop(error.0.clone());
            return ServerMessage::error(ErrorCode::InternalError, error.0, Some(event_id));
        }
        Err(_) => crate::daemon::view_hooks::ScopedViewRefresh::QueryFailed,
    };
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = state_guard.as_mut() else {
        return ServerMessage::error(ErrorCode::NotReady, "daemon is hydrating", Some(event_id));
    };
    let records = state.records_snapshot();
    let revision_before = state.leased.runtime.snapshot_revision();
    let mut next_views = state.views.clone();
    let processing = match crate::daemon::view_hooks::process_view_event(
        &mut next_views,
        &event,
        scoped_refresh,
        coordinator.done_clear_on,
        &records,
    ) {
        Ok(result) => result,
        Err(error) => {
            return ServerMessage::error(
                ErrorCode::InvalidRequest,
                error.to_string(),
                Some(event_id),
            );
        }
    };
    let diagnostic_pane = event
        .occurrence
        .as_ref()
        .map(|occurrence| occurrence.active_pane.clone())
        .or_else(|| {
            state
                .topology
                .panes
                .first()
                .map(|pane| pane.pane_instance.clone())
        });
    let registry_changed = processing.registry_changed;
    let diagnostics = processing.diagnostics;
    if processing.acknowledgements.is_empty() {
        state.views = next_views;
        let revision = match state.leased.runtime.finish_sequenced_projection(
            diagnostic_pane.as_ref(),
            diagnostics,
            registry_changed,
            revision_before,
        ) {
            Ok(revision) => revision,
            Err(error) => {
                return production_store_error_response(coordinator, error, Some(event_id));
            }
        };
        let result = if revision == revision_before {
            ViewApplyResult::Noop {
                snapshot_revision: revision,
            }
        } else {
            ViewApplyResult::TopologyOnly {
                snapshot_revision: revision,
            }
        };
        return ServerMessage::ViewResult {
            event_id,
            accepted_seq,
            result,
        };
    }
    let mut clock = crate::pane_state::store::SystemRecoveryClock::start();
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    let mut io = crate::pane_state::store::TmuxPaneStateStoreIo::new(
        &runner,
        coordinator.incarnation.identity.pid,
        coordinator.incarnation.identity.start_time,
    );
    let mut progress = state.leased.runtime.apply_view_acknowledgement_batch(
        &mut io,
        &mut clock,
        &processing.acknowledgements,
        coordinator.done_clear_on,
    );
    loop {
        match progress {
            ViewBatchProgress::Complete(mut result) => {
                let state = state_guard
                    .as_mut()
                    .expect("state initialized after view batch");
                state.views = next_views;
                result.snapshot_revision = match state.leased.runtime.finish_sequenced_projection(
                    diagnostic_pane.as_ref(),
                    diagnostics,
                    registry_changed,
                    revision_before,
                ) {
                    Ok(revision) => revision,
                    Err(error) => {
                        return production_store_error_response(coordinator, error, Some(event_id));
                    }
                };
                return ServerMessage::ViewResult {
                    event_id,
                    accepted_seq,
                    result: view_result(result),
                };
            }
            ViewBatchProgress::Pending(continuation) => {
                drop(state_guard);
                thread::sleep(crate::pane_state::store::STORE_RECOVERY_RETRY_INTERVAL);
                state_guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                let state = state_guard
                    .as_mut()
                    .expect("state initialized during view recovery");
                let timeout =
                    bounded_recovery_timeout(continuation.pending_recovery_remaining(&clock));
                let runner = crate::tmux::SystemTmuxRunner::from_env(timeout);
                let mut io = crate::pane_state::store::TmuxPaneStateStoreIo::new(
                    &runner,
                    coordinator.incarnation.identity.pid,
                    coordinator.incarnation.identity.start_time,
                );
                progress = state.leased.runtime.resume_view_acknowledgement_batch(
                    &mut io,
                    &mut clock,
                    continuation,
                );
            }
            ViewBatchProgress::Blocked(error) => {
                return production_store_error_response(coordinator, error, Some(event_id));
            }
            ViewBatchProgress::Fatal(error) => {
                coordinator.fail_stop(error.to_string());
                return production_store_error_response(coordinator, error, Some(event_id));
            }
        }
    }

    fn view_result(result: ViewBatchApplyResult) -> ViewApplyResult {
        if result.failed.is_empty() {
            if result.committed == 0 {
                ViewApplyResult::Noop {
                    snapshot_revision: result.snapshot_revision,
                }
            } else {
                ViewApplyResult::Committed {
                    snapshot_revision: result.snapshot_revision,
                    panes: result.committed,
                }
            }
        } else {
            ViewApplyResult::Partial {
                snapshot_revision: result.snapshot_revision,
                committed: result.committed,
                failed: result
                    .failed
                    .into_iter()
                    .map(|failure| PaneMutationFailure {
                        pane_instance: failure.pane_instance,
                        code: store_error_code(&failure.error),
                        message: failure.error.to_string(),
                    })
                    .collect(),
            }
        }
    }
}

fn scoped_view_refresh(
    coordinator: &ProductionV2Coordinator,
    event: &crate::pane_state::ViewEvent,
) -> std::result::Result<crate::daemon::view_hooks::ScopedViewRefresh, (String, bool)> {
    use crate::daemon::view_hooks::ScopedViewRefresh;
    use crate::pane_state::ViewHookKind;

    let deadline = Instant::now() + Duration::from_millis(100);
    let topology = query_full_topology(coordinator, scoped_view_refresh_remaining(deadline)?)
        .map_err(|error| (error.to_string(), error.requires_daemon_exit()))?;
    let witnesses = query_client_witnesses(coordinator, scoped_view_refresh_remaining(deadline)?)
        .map_err(|error| (error.to_string(), error.requires_daemon_exit()))?;
    let occurrence = event.occurrence.as_ref();
    let window_id = occurrence.map(|value| value.window_id.as_str());
    let window = window_id.and_then(|window_id| {
        let panes = topology
            .panes
            .iter()
            .filter(|pane| pane.window_id == window_id)
            .map(|pane| pane.pane_instance.clone())
            .collect::<Vec<_>>();
        let active = topology
            .panes
            .iter()
            .find(|pane| pane.window_id == window_id && pane.active)
            .map(|pane| pane.pane_instance.clone());
        active.map(|active_pane| (window_id.to_string(), active_pane, panes))
    });
    match event.hook_kind {
        ViewHookKind::WindowPaneChanged => window
            .map(
                |(window_id, active_pane, observed_panes)| ScopedViewRefresh::Window {
                    window_id,
                    active_pane,
                    observed_panes,
                },
            )
            .ok_or_else(|| ("view window is no longer present".to_string(), false)),
        ViewHookKind::SessionWindowChanged => {
            let session_id = occurrence
                .map(|value| value.session_id.clone())
                .ok_or_else(|| ("view session occurrence is missing".to_string(), false))?;
            let current_window = topology.panes.iter().find_map(|pane| {
                pane.session_links
                    .iter()
                    .any(|link| link.session_id == session_id && link.window_active)
                    .then(|| pane.window_id.clone())
            });
            let current_window = current_window
                .ok_or_else(|| ("view session is no longer present".to_string(), false))?;
            let observed_panes = topology
                .panes
                .iter()
                .filter(|pane| pane.window_id == current_window)
                .map(|pane| pane.pane_instance.clone())
                .collect::<Vec<_>>();
            let active_pane = topology
                .panes
                .iter()
                .find(|pane| pane.window_id == current_window && pane.active)
                .map(|pane| pane.pane_instance.clone())
                .ok_or_else(|| ("view session active pane is missing".to_string(), false))?;
            Ok(ScopedViewRefresh::Session {
                session_id,
                window_id: current_window,
                active_pane,
                observed_panes,
            })
        }
        ViewHookKind::ClientSessionChanged | ViewHookKind::ClientAttached => {
            let client_pid = event
                .source_client
                .as_ref()
                .map(|source| source.client_pid)
                .ok_or_else(|| ("view source client is missing".to_string(), false))?;
            let witness = witnesses
                .into_iter()
                .find(|witness| witness.client_pid == client_pid)
                .ok_or_else(|| ("view source client is no longer present".to_string(), false))?;
            let observed_panes = topology
                .panes
                .iter()
                .filter(|pane| pane.window_id == witness.window_id)
                .map(|pane| pane.pane_instance.clone())
                .collect();
            Ok(ScopedViewRefresh::Client {
                witness,
                observed_panes,
            })
        }
        ViewHookKind::ClientDetached => {
            let client_pid = event
                .source_client
                .as_ref()
                .map(|source| source.client_pid)
                .ok_or_else(|| ("detached client PID is missing".to_string(), false))?;
            if let Some(witness) = witnesses
                .into_iter()
                .find(|value| value.client_pid == client_pid)
            {
                let observed_panes = topology
                    .panes
                    .iter()
                    .filter(|pane| pane.window_id == witness.window_id)
                    .map(|pane| pane.pane_instance.clone())
                    .collect();
                Ok(ScopedViewRefresh::Client {
                    witness,
                    observed_panes,
                })
            } else {
                Ok(ScopedViewRefresh::ClientAbsent { client_pid })
            }
        }
    }
}

fn scoped_view_refresh_remaining(
    deadline: Instant,
) -> std::result::Result<Duration, (String, bool)> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| ("scoped view refresh deadline exceeded".to_string(), false))
}

fn apply_reset(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    pane: crate::pane_state::PaneInstance,
    expected: crate::pane_state::StoredStateDescriptor,
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::{ResetOutcome, ServerMessage};
    use crate::pane_state::store::{PendingResolution, StoreError};

    let previous = expected.clone();
    let tombstone_id = match crate::pane_state::ResetTombstoneId::generate() {
        Ok(value) => value,
        Err(error) => {
            return production_store_error_response(
                coordinator,
                StoreError::Random(error.to_string()),
                Some(event_id),
            );
        }
    };
    let mut clock = crate::pane_state::store::SystemRecoveryClock::start();
    let initial = {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let Some(state) = state_guard.as_mut() else {
            return crate::daemon::protocol::v2::ServerMessage::error(
                crate::daemon::protocol::v2::ErrorCode::NotReady,
                "daemon is hydrating",
                Some(event_id),
            );
        };
        let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
        let mut io = crate::pane_state::store::TmuxPaneStateStoreIo::new(
            &runner,
            coordinator.incarnation.identity.pid,
            coordinator.incarnation.identity.start_time,
        );
        state.leased.runtime.reset(
            &mut io,
            &mut clock,
            &pane,
            &expected,
            epoch_seconds(),
            tombstone_id,
        )
    };
    let current = match initial {
        Ok(current) => Ok(current),
        Err(StoreError::PersistPending) => loop {
            thread::sleep(crate::pane_state::store::STORE_RECOVERY_RETRY_INTERVAL);
            let resolution = {
                let mut state_guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                let state = state_guard
                    .as_mut()
                    .expect("state initialized during reset recovery");
                let timeout = bounded_recovery_timeout(
                    state.leased.runtime.pending_recovery_remaining(&clock),
                );
                let runner = crate::tmux::SystemTmuxRunner::from_env(timeout);
                let mut io = crate::pane_state::store::TmuxPaneStateStoreIo::new(
                    &runner,
                    coordinator.incarnation.identity.pid,
                    coordinator.incarnation.identity.start_time,
                );
                state.leased.runtime.resolve_pending(&mut io, &clock)
            };
            match resolution {
                Ok(PendingResolution::StillPending) => continue,
                Ok(PendingResolution::Reset(current)) => break Ok(current),
                Ok(PendingResolution::Applied(_)) => {
                    unreachable!("reset cannot resolve pane event")
                }
                Err(error) => break Err(error),
            }
        },
        Err(error) => Err(error),
    };
    match current {
        Ok(current) => {
            let revision = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned")
                .as_ref()
                .expect("state initialized after reset")
                .leased
                .runtime
                .snapshot_revision();
            ServerMessage::ResetResult {
                event_id,
                accepted_seq,
                previous: previous.clone(),
                current: current.clone(),
                outcome: if current == previous {
                    ResetOutcome::AlreadyReset
                } else {
                    ResetOutcome::Replaced
                },
                snapshot_revision: revision,
            }
        }
        Err(error) => {
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            production_store_error_response(coordinator, error, Some(event_id))
        }
    }
}

const LEGACY_CLEANUP_SERVER_MISMATCH_SENTINEL: &str = "__vde_legacy_cleanup_server_mismatch__";
const LEGACY_CLEANUP_PANE_MISMATCH_PREFIX: &str = "__vde_legacy_cleanup_pane_mismatch__";

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacyCleanupItem {
    scope: &'static str,
    target: String,
    option: &'static str,
    pane_pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacyCleanupOutcome {
    attempted: u64,
    removed: u64,
    failed: Vec<crate::daemon::protocol::v2::LegacyCleanupFailure>,
    server_mismatch: bool,
}

fn legacy_cleanup_items(
    topology: &crate::daemon::topology::TopologySnapshot,
) -> Vec<LegacyCleanupItem> {
    use crate::options::{
        LEGACY_PANE_OPTION_KEYS, LEGACY_SESSION_OPTION_KEYS, LEGACY_WINDOW_OPTION_KEYS,
    };

    let mut panes = topology
        .panes
        .iter()
        .map(|pane| pane.pane_instance.clone())
        .collect::<Vec<_>>();
    panes.sort();
    panes.dedup();
    let sessions = topology
        .panes
        .iter()
        .flat_map(|pane| {
            pane.session_links
                .iter()
                .map(|link| link.session_id.clone())
        })
        .collect::<BTreeSet<_>>();
    let windows = topology
        .panes
        .iter()
        .map(|pane| pane.window_id.clone())
        .collect::<BTreeSet<_>>();

    panes
        .into_iter()
        .flat_map(|pane| {
            LEGACY_PANE_OPTION_KEYS
                .iter()
                .copied()
                .map(move |option| LegacyCleanupItem {
                    scope: "pane",
                    target: pane.pane_id.clone(),
                    option,
                    pane_pid: Some(pane.pane_pid),
                })
        })
        .chain(sessions.into_iter().flat_map(|target| {
            LEGACY_SESSION_OPTION_KEYS
                .iter()
                .copied()
                .map(move |option| LegacyCleanupItem {
                    scope: "session",
                    target: target.clone(),
                    option,
                    pane_pid: None,
                })
        }))
        .chain(windows.into_iter().flat_map(|target| {
            LEGACY_WINDOW_OPTION_KEYS
                .iter()
                .copied()
                .map(move |option| LegacyCleanupItem {
                    scope: "window",
                    target: target.clone(),
                    option,
                    pane_pid: None,
                })
        }))
        .collect()
}

fn inspect_existing_legacy_cleanup_items(
    runner: &dyn crate::tmux::TmuxRunner,
    candidates: &[LegacyCleanupItem],
) -> (
    Vec<LegacyCleanupItem>,
    Vec<crate::daemon::protocol::v2::LegacyCleanupFailure>,
) {
    let mut existing = Vec::new();
    let mut failed = Vec::new();
    let mut groups = BTreeMap::<(&str, &str), Vec<&LegacyCleanupItem>>::new();
    for item in candidates {
        groups
            .entry((item.scope, item.target.as_str()))
            .or_default()
            .push(item);
    }
    for ((scope, target), items) in groups {
        let mut args = vec!["show-option"];
        match scope {
            "pane" => args.push("-pq"),
            "window" => args.push("-wq"),
            "session" => args.push("-q"),
            _ => unreachable!("legacy cleanup scope is fixed"),
        }
        args.extend(["-t", target]);
        match runner.run(&args) {
            Ok(output) => {
                let present = output
                    .lines()
                    .filter_map(|line| line.split_whitespace().next())
                    .collect::<BTreeSet<_>>();
                existing.extend(
                    items
                        .into_iter()
                        .filter(|item| present.contains(item.option))
                        .cloned(),
                );
            }
            Err(error) => {
                let message = format!(
                    "legacy option inspection failed: {}",
                    error.to_string().chars().take(256).collect::<String>()
                );
                failed.extend(items.into_iter().map(|item| {
                    crate::daemon::protocol::v2::LegacyCleanupFailure {
                        scope: item.scope.to_string(),
                        target: item.target.clone(),
                        option: item.option.to_string(),
                        message: message.clone(),
                    }
                }));
            }
        }
    }
    (existing, failed)
}

fn legacy_cleanup_scope_counts(
    items: &[LegacyCleanupItem],
) -> crate::daemon::protocol::v2::LegacyCleanupScopeCounts {
    let mut counts = crate::daemon::protocol::v2::LegacyCleanupScopeCounts::default();
    for item in items {
        match item.scope {
            "pane" => counts.pane = counts.pane.saturating_add(1),
            "window" => counts.window = counts.window.saturating_add(1),
            "session" => counts.session = counts.session.saturating_add(1),
            _ => unreachable!("legacy cleanup scope is fixed"),
        }
    }
    counts
}

fn bound_legacy_cleanup_failures(
    mut failed: Vec<crate::daemon::protocol::v2::LegacyCleanupFailure>,
) -> (
    Vec<crate::daemon::protocol::v2::LegacyCleanupFailure>,
    u64,
    u64,
) {
    const MAX_REPORTED_CLEANUP_FAILURES: usize = 256;
    let total = failed.len() as u64;
    failed.truncate(MAX_REPORTED_CLEANUP_FAILURES);
    let omitted = total.saturating_sub(failed.len() as u64);
    (failed, total, omitted)
}

fn legacy_cleanup_command(item: &LegacyCleanupItem, index: usize) -> String {
    let mut unset = vec!["set-option".to_string()];
    match item.scope {
        "pane" => unset.extend(["-p".to_string(), "-u".to_string()]),
        "session" => unset.push("-u".to_string()),
        "window" => unset.extend(["-w".to_string(), "-u".to_string()]),
        _ => unreachable!("legacy cleanup scope is fixed"),
    }
    unset.extend([
        "-t".to_string(),
        item.target.clone(),
        item.option.to_string(),
    ]);
    let unset = crate::pane_state::store::tmux_command_string(&unset);
    let Some(pane_pid) = item.pane_pid else {
        return unset;
    };
    crate::pane_state::store::tmux_command_string(&[
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        item.target.clone(),
        format!("#{{==:#{{pane_pid}},{pane_pid}}}"),
        unset,
        format!("display-message -p '{LEGACY_CLEANUP_PANE_MISMATCH_PREFIX}:{index}'"),
    ])
}

fn execute_legacy_cleanup(
    runner: &dyn crate::tmux::TmuxRunner,
    expected_server: &crate::daemon::topology::ServerIdentity,
    items: &[LegacyCleanupItem],
) -> LegacyCleanupOutcome {
    let attempted = u64::try_from(items.len()).unwrap_or(u64::MAX);
    if items.is_empty() {
        return LegacyCleanupOutcome {
            attempted,
            removed: 0,
            failed: Vec::new(),
            server_mismatch: false,
        };
    }
    let command = items
        .iter()
        .enumerate()
        .map(|(index, item)| legacy_cleanup_command(item, index))
        .collect::<Vec<_>>()
        .join(" ; ");
    let guarded = crate::pane_state::store::server_guarded_command_args(
        expected_server.pid,
        expected_server.start_time,
        command,
        LEGACY_CLEANUP_SERVER_MISMATCH_SENTINEL,
    );
    let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
    match runner.run(&refs) {
        Ok(output)
            if output
                .lines()
                .any(|line| line.trim() == LEGACY_CLEANUP_SERVER_MISMATCH_SENTINEL) =>
        {
            LegacyCleanupOutcome {
                attempted,
                removed: 0,
                failed: Vec::new(),
                server_mismatch: true,
            }
        }
        Ok(output) => {
            let mismatches = output
                .lines()
                .filter_map(|line| {
                    line.trim()
                        .strip_prefix(&format!("{LEGACY_CLEANUP_PANE_MISMATCH_PREFIX}:"))
                        .and_then(|index| index.parse::<usize>().ok())
                })
                .collect::<BTreeSet<_>>();
            let failed = mismatches
                .into_iter()
                .filter_map(|index| items.get(index))
                .map(|item| crate::daemon::protocol::v2::LegacyCleanupFailure {
                    scope: item.scope.to_string(),
                    target: item.target.clone(),
                    option: item.option.to_string(),
                    message: "pane instance changed before cleanup".to_string(),
                })
                .collect::<Vec<_>>();
            LegacyCleanupOutcome {
                attempted,
                removed: attempted.saturating_sub(failed.len() as u64),
                failed,
                server_mismatch: false,
            }
        }
        Err(error) => {
            let detail = error.to_string().chars().take(256).collect::<String>();
            let message = format!("tmux legacy cleanup batch failed: {detail}");
            LegacyCleanupOutcome {
                attempted,
                removed: 0,
                failed: items
                    .iter()
                    .map(|item| crate::daemon::protocol::v2::LegacyCleanupFailure {
                        scope: item.scope.to_string(),
                        target: item.target.clone(),
                        option: item.option.to_string(),
                        message: message.clone(),
                    })
                    .collect(),
                server_mismatch: false,
            }
        }
    }
}

fn apply_legacy_cleanup(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: crate::pane_state::EventId,
    dry_run: bool,
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::ServerMessage;

    let topology = match query_full_topology(coordinator, Duration::from_secs(1)) {
        Ok(topology) => topology,
        Err(error) => {
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            return ServerMessage::error(
                crate::daemon::protocol::v2::ErrorCode::InternalError,
                error.to_string(),
                Some(event_id),
            );
        }
    };
    let candidates = legacy_cleanup_items(&topology);
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    if let Err(error) = coordinator.incarnation.verify(&runner, &coordinator.env) {
        coordinator.fail_stop("tmux server incarnation changed before legacy cleanup inspection");
        return ServerMessage::error(
            crate::daemon::protocol::v2::ErrorCode::InternalError,
            error.to_string(),
            Some(event_id),
        );
    }
    let (items, mut inspection_failures) =
        inspect_existing_legacy_cleanup_items(&runner, &candidates);
    if let Err(error) = coordinator.incarnation.verify(&runner, &coordinator.env) {
        coordinator.fail_stop("tmux server incarnation changed during legacy cleanup inspection");
        return ServerMessage::error(
            crate::daemon::protocol::v2::ErrorCode::InternalError,
            error.to_string(),
            Some(event_id),
        );
    }
    let mut outcome = if dry_run {
        LegacyCleanupOutcome {
            attempted: items.len() as u64,
            removed: 0,
            failed: std::mem::take(&mut inspection_failures),
            server_mismatch: false,
        }
    } else {
        execute_legacy_cleanup(&runner, &coordinator.incarnation.identity, &items)
    };
    if !dry_run {
        outcome.failed.extend(inspection_failures);
    }
    if outcome.server_mismatch {
        coordinator.fail_stop("tmux server incarnation changed during legacy cleanup");
        return ServerMessage::error(
            crate::daemon::protocol::v2::ErrorCode::InternalError,
            "tmux server incarnation changed during legacy cleanup",
            Some(event_id),
        );
    }
    let remaining_items = if dry_run {
        items.clone()
    } else {
        let (remaining, post_inspection_failures) =
            inspect_existing_legacy_cleanup_items(&runner, &items);
        outcome.failed.extend(post_inspection_failures);
        if let Err(error) = coordinator.incarnation.verify(&runner, &coordinator.env) {
            coordinator
                .fail_stop("tmux server incarnation changed during legacy cleanup verification");
            return ServerMessage::error(
                crate::daemon::protocol::v2::ErrorCode::InternalError,
                error.to_string(),
                Some(event_id),
            );
        }
        let already_failed = outcome
            .failed
            .iter()
            .map(|failure| {
                (
                    failure.scope.clone(),
                    failure.target.clone(),
                    failure.option.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        outcome.failed.extend(remaining.iter().filter_map(|item| {
            let key = (
                item.scope.to_string(),
                item.target.clone(),
                item.option.to_string(),
            );
            (!already_failed.contains(&key)).then(|| {
                crate::daemon::protocol::v2::LegacyCleanupFailure {
                    scope: key.0,
                    target: key.1,
                    option: key.2,
                    message: "legacy option remained after cleanup".to_string(),
                }
            })
        }));
        outcome.removed = (items.len() as u64).saturating_sub(remaining.len() as u64);
        remaining
    };
    let snapshot_revision = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_ref()
        .map_or(0, |state| state.leased.runtime.snapshot_revision());
    let (failed, failed_total, failed_omitted) = bound_legacy_cleanup_failures(outcome.failed);
    ServerMessage::CleanupLegacyResult {
        event_id,
        accepted_seq,
        dry_run,
        attempted: outcome.attempted,
        removed: outcome.removed,
        remaining: remaining_items.len() as u64,
        scope_counts: legacy_cleanup_scope_counts(&items),
        remaining_scope_counts: legacy_cleanup_scope_counts(&remaining_items),
        failed,
        failed_total,
        failed_omitted,
        snapshot_revision,
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
    parse_observation_poll_projection(&output, &framing, &coordinator.incarnation.identity)
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
        topology,
        status_metadata: status_projection_metadata(status),
        witnesses,
        observation_bases: BTreeMap::new(),
        view_base: crate::daemon::view_hooks::ViewRegistry::default(),
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
    Ok(status_projection_metadata(snapshot))
}

fn status_projection_metadata(
    snapshot: crate::daemon::topology::StatusMetadataSnapshot,
) -> super::runtime::StatusProjectionMetadata {
    let mut metadata = super::runtime::StatusProjectionMetadata::default();
    for session in snapshot.sessions {
        if let Some(category) = session.category.clone() {
            metadata.categories.insert(category);
        }
        metadata.sessions.insert(
            session.session_id,
            super::runtime::SessionProjectionMetadata {
                session_name: session.session_name,
                stored_category: session.category,
                project_path: session.project_path,
                category_override: session.category_override,
                attached: Some(session.attached),
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

fn query_client_witnesses(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
) -> Result<Vec<crate::pane_state::ClientWitness>, crate::daemon::view_hooks::FreshVisibilityError>
{
    let token = crate::pane_state::EventId::generate()
        .map_err(|error| crate::daemon::view_hooks::FreshVisibilityError::Query(error.to_string()))?
        .as_str()
        .to_string();
    let args = crate::daemon::view_hooks::client_view_query_args(&token);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout);
    let output = runner.run(&refs).map_err(|error| {
        crate::daemon::view_hooks::FreshVisibilityError::Query(error.to_string())
    })?;
    crate::daemon::view_hooks::parse_client_view_query(
        &output,
        &token,
        &coordinator.incarnation.identity,
    )
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
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard.as_mut().ok_or_else(|| {
        crate::pane_state::store::StoreError::PersistFailed("daemon is hydrating".to_string())
    })?;
    state.replace_topology(topology)?;
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
        state.replace_topology(projection.topology)?;
        state.replace_status_metadata(projection.status_metadata)?;
    }
    reconcile_views_with_witnesses(
        coordinator,
        &projection.witnesses,
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
) -> crate::daemon::protocol::v2::ServerMessage {
    match error.downcast::<crate::pane_state::store::StoreError>() {
        Ok(store_error) => production_store_error_response(coordinator, store_error, None),
        Err(error) => crate::daemon::protocol::v2::ServerMessage::error(
            crate::daemon::protocol::v2::ErrorCode::InternalError,
            error.to_string(),
            None,
        ),
    }
}

fn targeted_pane_refresh_response(
    coordinator: &ProductionV2Coordinator,
    pane_id: &str,
) -> crate::daemon::protocol::v2::ServerMessage {
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
) -> crate::daemon::protocol::v2::ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    match outcome {
        Ok(crate::daemon::topology::TargetedRefreshOutcome::NotFound) => {
            ServerMessage::error(ErrorCode::PaneNotFound, "pane was not found", None)
        }
        Ok(crate::daemon::topology::TargetedRefreshOutcome::Found(pane)) => {
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
            topology.panes.push(pane);
            topology
                .panes
                .sort_by(|left, right| left.pane_instance.cmp(&right.pane_instance));
            if let Err(error) = state.replace_topology(topology) {
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

fn store_error_code(
    error: &crate::pane_state::store::StoreError,
) -> crate::daemon::protocol::v2::ErrorCode {
    use crate::daemon::protocol::v2::ErrorCode;
    use crate::pane_state::reducer::ReduceError;
    use crate::pane_state::store::StoreError;
    match error {
        StoreError::StateTooLarge => ErrorCode::StateTooLarge,
        StoreError::StateLoad(_) | StoreError::ExternalWriter(_) => ErrorCode::StateLoadError,
        StoreError::InvalidPaneInstance => ErrorCode::InvalidPaneInstance,
        StoreError::StaleStateIdentity => ErrorCode::StaleStateIdentity,
        StoreError::WriterLeaseHeld => ErrorCode::WriterLeaseHeld,
        StoreError::PersistPending => ErrorCode::NotReady,
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
    event_id: Option<crate::pane_state::EventId>,
) -> crate::daemon::protocol::v2::ServerMessage {
    crate::daemon::protocol::v2::ServerMessage::error(
        store_error_code(&error),
        error.to_string(),
        event_id,
    )
}

fn production_store_error_response(
    coordinator: &ProductionV2Coordinator,
    error: crate::pane_state::store::StoreError,
    event_id: Option<crate::pane_state::EventId>,
) -> crate::daemon::protocol::v2::ServerMessage {
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

fn bounded_recovery_timeout(remaining: Option<Duration>) -> Duration {
    remaining
        .unwrap_or(Duration::from_secs(3))
        .min(Duration::from_secs(3))
        .max(Duration::from_millis(1))
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
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let coordinator = listener_coordinator.clone();
                    thread::spawn(move || {
                        if let Err(error) = handle_v2_runtime_stream(coordinator.clone(), stream) {
                            coordinator
                                .log_daemon_error(&format!("daemon connection error: {error:#}"));
                        }
                    });
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
    start_v2_mutation_worker(coordinator.clone());
    start_sidebar_completion_forwarder(coordinator.clone());
    start_canonical_observation_worker(
        coordinator.clone(),
        Duration::from_millis(config.daemon.poll_ms),
    );
    start_canonical_git_worker(
        coordinator.clone(),
        Duration::from_millis(config.daemon.git.poll_interval_ms),
    );
    start_status_push_worker(coordinator.clone());

    coordinator.wait_for_shutdown();
    runtime_cleanup.cleanup()?;
    Ok(())
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
    let coordinator = Arc::new(ProductionV2Coordinator::new(
        incarnation,
        env.clone(),
        config.daemon.done_clear_on,
        notification_command,
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
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3))
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
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
    let (records, topology, witnesses) = if session_count == 0 {
        (
            Vec::new(),
            crate::daemon::topology::TopologySnapshot {
                server_identity: coordinator.incarnation.identity.clone(),
                panes: Vec::new(),
            },
            Vec::new(),
        )
    } else {
        let hydrate_framing = crate::daemon::topology::QueryFraming::generate()?;
        let hydrate_args = crate::daemon::topology::hydrate_query_args(&hydrate_framing);
        let hydrate_refs = hydrate_args.iter().map(String::as_str).collect::<Vec<_>>();
        let hydrate_output = runner.run(&hydrate_refs)?;
        let records = crate::daemon::topology::parse_hydrate_records(
            &hydrate_output,
            &hydrate_framing,
            &coordinator.incarnation.identity,
        )?;
        let topology = query_full_topology(coordinator, Duration::from_secs(1))?;
        let witnesses = query_client_witnesses(coordinator, Duration::from_secs(1))?;
        (records, topology, witnesses)
    };
    leased.hydrate(records);
    let mut views = crate::daemon::view_hooks::ViewRegistry::default();
    let mut window_panes = BTreeMap::<String, Vec<crate::pane_state::PaneInstance>>::new();
    for pane in &topology.panes {
        window_panes
            .entry(pane.window_id.clone())
            .or_default()
            .push(pane.pane_instance.clone());
    }
    views
        .reconcile(&witnesses, &window_panes)
        .map_err(|error| anyhow::anyhow!("failed to build initial view registry: {error}"))?;
    let status_metadata = query_status_projection_metadata(coordinator, Duration::from_secs(1))?;
    let state_path = crate::sidebar::store::state_path(env);
    let sidebar_order = crate::sidebar::store::load_state(&state_path)?;
    let expansion_path = crate::sidebar::store::expansion_state_path(env);
    let sidebar_expansion = crate::sidebar::store::load_expansion_state(&expansion_path)?;
    let mut canonical =
        super::runtime::CanonicalCoordinatorState::new(leased, topology, views, sidebar_order);
    canonical.sidebar_expansion = sidebar_expansion;
    canonical.status_metadata = status_metadata;
    canonical.projection_config = config.clone();
    *coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned") = Some(canonical);
    coordinator.establish_quarantine_baseline();

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
            break;
        }
    }
    Ok(())
}

fn initial_view_reconciliation(coordinator: &ProductionV2Coordinator) -> Result<()> {
    let witnesses = match query_client_witnesses(coordinator, Duration::from_millis(250)) {
        Ok(witnesses) => witnesses,
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
    reconcile_views_with_witnesses(coordinator, &witnesses, None, None)
}

fn reconcile_views_with_witnesses(
    coordinator: &ProductionV2Coordinator,
    witnesses: &[crate::pane_state::ClientWitness],
    observation_bases: Option<
        &BTreeMap<
            crate::pane_state::PaneInstance,
            Option<crate::pane_state::StoredStateDescriptor>,
        >,
    >,
    view_base: Option<&crate::daemon::view_hooks::ViewRegistry>,
) -> Result<()> {
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
    let window_panes = state.window_panes();
    let records = records_at_observation_base(state.records_snapshot(), observation_bases);
    let revision_before = state.leased.runtime.snapshot_revision();
    let mut next_views = state.views.clone();
    let result = crate::daemon::view_hooks::reconcile_current_views(
        &mut next_views,
        coordinator
            .router
            .lock()
            .expect("v2 router lock poisoned")
            .daemon_instance_id(),
        witnesses,
        &window_panes,
        coordinator.done_clear_on,
        &records,
    )?;
    let registry_changed = result.registry_changed;
    if result.acknowledgements.is_empty() {
        state.views = next_views;
        state.leased.runtime.finish_sequenced_projection(
            None,
            std::iter::empty(),
            registry_changed,
            revision_before,
        )?;
        return Ok(());
    }
    let mut clock = crate::pane_state::store::SystemRecoveryClock::start();
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    let mut io = crate::pane_state::store::TmuxPaneStateStoreIo::new(
        &runner,
        coordinator.incarnation.identity.pid,
        coordinator.incarnation.identity.start_time,
    );
    let progress = state.leased.runtime.apply_view_acknowledgement_batch(
        &mut io,
        &mut clock,
        &result.acknowledgements,
        coordinator.done_clear_on,
    );
    match progress {
        crate::pane_state::store::ViewBatchProgress::Complete(_) => {
            let state = state_guard
                .as_mut()
                .expect("state initialized after reconciliation");
            state.views = next_views;
            state.leased.runtime.finish_sequenced_projection(
                None,
                std::iter::empty(),
                registry_changed,
                revision_before,
            )?;
            Ok(())
        }
        crate::pane_state::store::ViewBatchProgress::Pending(mut continuation) => loop {
            drop(state_guard);
            thread::sleep(crate::pane_state::store::STORE_RECOVERY_RETRY_INTERVAL);
            state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized during reconciliation");
            let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
            let mut io = crate::pane_state::store::TmuxPaneStateStoreIo::new(
                &runner,
                coordinator.incarnation.identity.pid,
                coordinator.incarnation.identity.start_time,
            );
            match state.leased.runtime.resume_view_acknowledgement_batch(
                &mut io,
                &mut clock,
                continuation,
            ) {
                crate::pane_state::store::ViewBatchProgress::Complete(_) => {
                    state.views = next_views;
                    state.leased.runtime.finish_sequenced_projection(
                        None,
                        std::iter::empty(),
                        registry_changed,
                        revision_before,
                    )?;
                    break Ok(());
                }
                crate::pane_state::store::ViewBatchProgress::Pending(next) => continuation = next,
                crate::pane_state::store::ViewBatchProgress::Blocked(error)
                | crate::pane_state::store::ViewBatchProgress::Fatal(error) => {
                    break Err(anyhow::Error::new(error));
                }
            }
        },
        crate::pane_state::store::ViewBatchProgress::Blocked(error)
        | crate::pane_state::store::ViewBatchProgress::Fatal(error) => {
            Err(anyhow::Error::new(error))
        }
    }
}

fn observation_view_base_matches(
    current: &crate::daemon::view_hooks::ViewRegistry,
    observation_base: Option<&crate::daemon::view_hooks::ViewRegistry>,
) -> bool {
    observation_base.is_none_or(|base| current == base)
}

fn records_at_observation_base(
    mut records: BTreeMap<crate::pane_state::PaneInstance, crate::pane_state::StoredPaneRecord>,
    observation_bases: Option<
        &BTreeMap<
            crate::pane_state::PaneInstance,
            Option<crate::pane_state::StoredStateDescriptor>,
        >,
    >,
) -> BTreeMap<crate::pane_state::PaneInstance, crate::pane_state::StoredPaneRecord> {
    if let Some(observation_bases) = observation_bases {
        records.retain(|pane, record| {
            observation_bases
                .get(pane)
                .is_some_and(|base| base.as_ref() == Some(&record.descriptor()))
        });
    }
    records
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
    const V2_EVENT_ID: &str = "102132435465768798a9bacbdcedfe0f";
    const V2_DAEMON_ID: &str = "ffeeddccbbaa99887766554433221100";
    const POLL_TOKEN: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn runtime_cleanup_removes_owned_socket_and_process_record_on_early_return() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let event_id = crate::pane_state::EventId::generate().unwrap();
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

        let event_id = crate::pane_state::EventId::generate().unwrap();
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
        let event_id = crate::pane_state::EventId::generate().unwrap();
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
            &crate::pane_state::DaemonInstanceId::parse(V2_DAEMON_ID.to_string()).unwrap(),
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
            "$1", "main", "@1", "0", "1", "0", "window", "%1", "100", "/tmp", "zsh", "80", "1",
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
    fn completion_window_lookup_requires_full_pane_instance() {
        let framing = observation_poll_framing();
        let projection = parse_observation_poll_projection(
            &observation_poll_output(&framing),
            &framing,
            &crate::daemon::topology::ServerIdentity {
                pid: 123,
                start_time: 456,
            },
        )
        .unwrap();
        assert_eq!(
            completion_window_id(
                &projection.topology,
                &crate::pane_state::PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 100,
                },
            ),
            Some("@1")
        );
        assert_eq!(
            completion_window_id(
                &projection.topology,
                &crate::pane_state::PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
            ),
            None
        );
    }

    #[test]
    fn stale_poll_view_and_state_bases_block_reconciliation_inputs() {
        let pane = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let state = |revision, completed_seq| {
            crate::pane_state::StoredPaneRecord::Active(crate::pane_state::PaneState {
                schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
                state_id: crate::pane_state::StateId::parse(POLL_TOKEN).unwrap(),
                revision,
                pane_instance: pane.clone(),
                agent: crate::pane_state::AgentKind::parse("codex").unwrap(),
                agent_session_id: None,
                agent_epoch: 1,
                agent_present: true,
                scan_verified: true,
                synthetic_completion_armed: false,
                lifecycle: crate::pane_state::LifecycleState::Idle,
                run_seq: completed_seq,
                completed_seq,
                acknowledged_seq: 0,
                started_at: Some(1),
                completed_at: Some(2),
                prompt: None,
                tasks: crate::pane_state::TaskState::default(),
                subagents: Vec::new(),
                worktree_activity: None,
            })
        };
        let observed = state(3, 1);
        let newer = state(4, 2);
        let bases = BTreeMap::from([(pane.clone(), Some(observed.descriptor()))]);

        assert_eq!(
            records_at_observation_base(BTreeMap::from([(pane.clone(), observed)]), Some(&bases),)
                .len(),
            1
        );
        assert!(
            records_at_observation_base(BTreeMap::from([(pane.clone(), newer)]), Some(&bases))
                .is_empty()
        );

        let view_base = crate::daemon::view_hooks::ViewRegistry::default();
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

    fn v2_daemon_id() -> crate::pane_state::DaemonInstanceId {
        crate::pane_state::DaemonInstanceId::parse(V2_DAEMON_ID).unwrap()
    }

    fn v2_event_id() -> crate::pane_state::EventId {
        crate::pane_state::EventId::parse(V2_EVENT_ID).unwrap()
    }

    fn v2_pane_event(
        event: crate::pane_state::PaneEvent,
    ) -> crate::daemon::protocol::v2::ClientMessage {
        crate::daemon::protocol::v2::ClientMessage::SubmitPaneEvent {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            envelope: crate::pane_state::PaneEventEnvelope {
                daemon_instance_id: v2_daemon_id(),
                event_id: v2_event_id(),
                pane_instance: crate::pane_state::PaneInstance {
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

    fn v2_begin() -> crate::daemon::protocol::v2::ClientMessage {
        v2_pane_event(crate::pane_state::PaneEvent::BeginRun {
            started_at: 1,
            prompt: None,
        })
    }

    fn v2_handshake(router: &mut V2Router, connection: &mut V2ConnectionState) {
        let route = router.route(
            connection,
            crate::daemon::protocol::v2::ClientMessage::Hello {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            },
        );
        assert!(matches!(
            route,
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::HelloAck {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                ..
            })
        ));
    }

    #[test]
    fn canonical_notification_worker_exports_blocked_environment() {
        let root = std::env::temp_dir().join(format!(
            "vde-notification-worker-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
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
        let root = std::env::temp_dir().join(format!(
            "vde-notification-timeout-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("child.pid");
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let health = Arc::new(NotificationHealthCounters::default());
        let sender = start_notification_worker_with_timeout_and_log(
            command,
            Duration::from_millis(100),
            None,
            Some(health.clone()),
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
        assert_eq!(health.failures.load(Ordering::SeqCst), 1);
        assert!(health.degraded.load(Ordering::SeqCst));
        assert_eq!(
            health.last_error_code.lock().unwrap().as_deref(),
            Some("timeout")
        );
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_notification_successful_leader_exit_kills_background_descendants() {
        let root = std::env::temp_dir().join(format!(
            "vde-notification-background-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("child.pid");
        let command = format!("sleep 30 & echo $! > '{}'", pid_file.display());
        let health = Arc::new(NotificationHealthCounters::default());
        let sender = start_notification_worker_with_timeout_and_log(
            command,
            Duration::from_secs(2),
            None,
            Some(health.clone()),
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
        assert_eq!(health.failures.load(Ordering::SeqCst), 0);
        assert!(!health.degraded.load(Ordering::SeqCst));
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_notification_failure_is_written_to_private_incarnation_log() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "vde-notification-log-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "c".repeat(64);
        let health = Arc::new(NotificationHealthCounters::default());
        let sender = start_notification_worker_with_timeout_and_log(
            "exit 7".to_string(),
            Duration::from_secs(1),
            Some((env.clone(), hash.clone())),
            Some(health.clone()),
        );
        sender
            .try_send(NotificationWorkerJob {
                pane_id: "%7".to_string(),
                agent: "codex".to_string(),
            })
            .unwrap();
        let path = crate::daemon::lifecycle::incarnation_log_path(&env, &hash, "notification.log");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("exited with status"));
        assert!(contents.contains("pane %7"));
        assert_eq!(health.failures.load(Ordering::SeqCst), 1);
        assert!(health.degraded.load(Ordering::SeqCst));
        assert_eq!(
            health.last_error_code.lock().unwrap().as_deref(),
            Some("nonzero_exit")
        );
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
    fn production_fail_stop_marks_router_and_releases_waiters() {
        let coordinator = ProductionV2Coordinator::new(
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: "/tmp/vde-test-tmux.sock".into(),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: "a".repeat(64),
            },
            BTreeMap::new(),
            crate::config::DoneClearOn::Pane,
            None,
        )
        .unwrap();
        let (sender, receiver) = mpsc::channel();
        coordinator.waiters.lock().unwrap().insert(1, sender);

        coordinator.fail_stop("counter overflow");

        assert!(coordinator.router.lock().unwrap().is_fatal());
        assert!(coordinator.shutdown.load(Ordering::SeqCst));
        assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)).unwrap(),
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InternalError,
                ..
            }
        ));
    }

    #[test]
    fn observation_poll_store_fail_stop_reaches_coordinator() {
        let coordinator = ProductionV2Coordinator::new(
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: "/tmp/vde-test-tmux.sock".into(),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: "b".repeat(64),
            },
            BTreeMap::new(),
            crate::config::DoneClearOn::Pane,
            None,
        )
        .unwrap();

        let response = observation_poll_error_response(
            &coordinator,
            anyhow::Error::new(crate::pane_state::store::StoreError::FailStop(
                "projection invariant failed".to_string(),
            )),
        );

        assert!(coordinator.router.lock().unwrap().is_fatal());
        assert!(matches!(
            response,
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InternalError,
                ..
            }
        ));
    }

    #[test]
    fn disconnected_mutation_waiter_enqueues_sequenced_diagnostic() {
        let coordinator = ProductionV2Coordinator::new(
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: "/tmp/vde-test-tmux.sock".into(),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: "d".repeat(64),
            },
            BTreeMap::new(),
            crate::config::DoneClearOn::Pane,
            None,
        )
        .unwrap();
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        coordinator.waiters.lock().unwrap().insert(3, sender);

        coordinator.complete(
            3,
            crate::daemon::protocol::v2::ServerMessage::SnapshotAck {
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
        let coordinator = ProductionV2Coordinator::new(
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: "/tmp/vde-test-tmux.sock".into(),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: "b".repeat(64),
            },
            BTreeMap::new(),
            crate::config::DoneClearOn::Pane,
            None,
        )
        .unwrap();
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
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::NotReady,
                ..
            }
        ));
        assert!(current_rx.try_recv().is_err());
        let response = coordinator.route_external(
            &mut V2ConnectionState::default(),
            crate::daemon::protocol::v2::ClientMessage::Hello {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            },
            0,
        );
        assert!(matches!(
            response,
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::NotReady,
                ..
            }
        ));
        assert!(!coordinator.enqueue_internal(V2InternalMutation::RefreshTopology));
        assert!(current_rx.try_recv().is_err());
        coordinator.complete(
            4,
            crate::daemon::protocol::v2::ServerMessage::ShutdownAccepted {
                event_id: v2_event_id(),
                accepted_seq: 4,
            },
        );
        assert!(matches!(
            current_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            crate::daemon::protocol::v2::ServerMessage::ShutdownAccepted {
                accepted_seq: 4,
                ..
            }
        ));
        coordinator.mark_shutdown_ready();
        assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
    }

    #[test]
    fn snapshot_waiter_cannot_miss_shutdown_notification() {
        let coordinator = Arc::new(
            ProductionV2Coordinator::new(
                crate::daemon::lifecycle::TmuxServerIncarnation {
                    socket_path: "/tmp/vde-test-tmux.sock".into(),
                    identity: crate::daemon::topology::ServerIdentity {
                        pid: 1,
                        start_time: 2,
                    },
                    hash: "e".repeat(64),
                },
                BTreeMap::new(),
                crate::config::DoneClearOn::Pane,
                None,
            )
            .unwrap(),
        );
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let coordinator = coordinator.clone();
            thread::spawn(move || {
                started_tx.send(()).unwrap();
                let result = coordinator.wait_for_snapshot_after(0);
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
        let root = std::env::temp_dir().join(format!(
            "vde-published-snapshot-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let coordinator = ProductionV2Coordinator::new(
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: root.join("tmux.sock"),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: "c".repeat(64),
            },
            BTreeMap::new(),
            crate::config::DoneClearOn::Pane,
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
                    server_identity: crate::daemon::topology::ServerIdentity {
                        pid: 1,
                        start_time: 2,
                    },
                    panes: Vec::new(),
                },
                crate::daemon::view_hooks::ViewRegistry::default(),
                crate::sidebar::state::SidebarOrderPreferences::default(),
            ));

        let first = coordinator.publish_resolved_snapshot().unwrap();
        assert!(matches!(
            coordinator.query(
                crate::daemon::protocol::v2::ClientMessage::QueryStatusSnapshot {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                    context: crate::daemon::protocol::v2::StatusContext::Global,
                }
            ),
            crate::daemon::protocol::v2::ServerMessage::StatusSnapshotResult {
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

    fn query_pane_cache_miss_with_refresh_outcome(
        outcome: Result<
            crate::daemon::topology::TargetedRefreshOutcome,
            crate::daemon::topology::TopologyError,
        >,
    ) -> (
        crate::daemon::protocol::v2::ServerMessage,
        Vec<crate::daemon::protocol::v2::DaemonDiagnostic>,
    ) {
        let root = std::env::temp_dir().join(format!(
            "vde-query-pane-refresh-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let coordinator = Arc::new(
            ProductionV2Coordinator::new(
                crate::daemon::lifecycle::TmuxServerIncarnation {
                    socket_path: root.join("tmux.sock"),
                    identity: crate::daemon::topology::ServerIdentity {
                        pid: 1,
                        start_time: 2,
                    },
                    hash: "9".repeat(64),
                },
                BTreeMap::new(),
                crate::config::DoneClearOn::Pane,
                None,
            )
            .unwrap(),
        );
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
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
                crate::daemon::view_hooks::ViewRegistry::default(),
                crate::sidebar::state::SidebarOrderPreferences::default(),
            ));

        let (result_tx, result_rx) = mpsc::channel();
        let query_coordinator = coordinator.clone();
        let query = thread::spawn(move || {
            result_tx
                .send(query_coordinator.query(
                    crate::daemon::protocol::v2::ClientMessage::QueryPane {
                        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                        pane_id: "%7".to_string(),
                    },
                ))
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
            crate::daemon::topology::TargetedRefreshOutcome::Found(
                crate::daemon::topology::TopologyPane {
                    pane_instance: crate::pane_state::PaneInstance {
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
                },
            ),
        ));
        assert!(diagnostics.is_empty());
        assert!(matches!(
            response,
            crate::daemon::protocol::v2::ServerMessage::PaneResult {
                pane: crate::daemon::protocol::v2::PanePresentation {
                    pane_instance: crate::pane_state::PaneInstance {
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
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::PaneNotFound,
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
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InternalError,
                ..
            }
        ));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].code,
            crate::daemon::protocol::v2::ErrorCode::InternalError
        );
        assert!(diagnostics[0].message.contains("tmux query failed"));
    }

    #[test]
    fn query_pane_cache_miss_records_refresh_timeout_diagnostic() {
        let (response, diagnostics) = query_pane_cache_miss_with_refresh_outcome(Err(
            crate::daemon::topology::TopologyError::Deadline,
        ));
        assert!(matches!(
            response,
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InternalError,
                ..
            }
        ));
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("deadline exceeded"));
    }

    #[test]
    fn oversized_resolved_snapshot_commits_and_queues_frame_too_large_diagnostic() {
        let root = std::env::temp_dir().join(format!(
            "vde-frame-too-large-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let coordinator = ProductionV2Coordinator::new(
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: root.join("tmux.sock"),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: "f".repeat(64),
            },
            BTreeMap::new(),
            crate::config::DoneClearOn::Pane,
            None,
        )
        .unwrap();
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
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
            crate::daemon::view_hooks::ViewRegistry::default(),
            crate::sidebar::state::SidebarOrderPreferences::default(),
        );
        state
            .replace_topology(crate::daemon::topology::TopologySnapshot {
                server_identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                panes: vec![crate::daemon::topology::TopologyPane {
                    pane_instance: crate::pane_state::PaneInstance {
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
                }],
            })
            .unwrap();
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);
        *coordinator.state.lock().unwrap() = Some(state);

        let published = coordinator.publish_resolved_snapshot().unwrap();
        assert!(published.terminal);
        assert!(matches!(
            published.message.as_ref(),
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::FrameTooLarge,
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
            crate::daemon::protocol::v2::ServerMessage::SnapshotAck {
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
                .any(|diagnostic| diagnostic.code
                    == crate::daemon::protocol::v2::ErrorCode::FrameTooLarge)
        );

        drop(state);
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_subscription_frame_is_written_once_then_stream_closes() {
        let coordinator = Arc::new(
            ProductionV2Coordinator::new(
                crate::daemon::lifecycle::TmuxServerIncarnation {
                    socket_path: "/tmp/vde-test-tmux.sock".into(),
                    identity: crate::daemon::topology::ServerIdentity {
                        pid: 1,
                        start_time: 2,
                    },
                    hash: "a".repeat(64),
                },
                BTreeMap::new(),
                crate::config::DoneClearOn::Pane,
                None,
            )
            .unwrap(),
        );
        let message = crate::daemon::protocol::v2::ServerMessage::error(
            crate::daemon::protocol::v2::ErrorCode::FrameTooLarge,
            "too large",
            None,
        );
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
            serde_json::from_str::<crate::daemon::protocol::v2::ServerMessage>(frames[0]).unwrap(),
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::FrameTooLarge,
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
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InvalidRequest,
                ..
            })
        ));
        assert!(matches!(
            router.route(
                &mut connection,
                crate::daemon::protocol::v2::ClientMessage::Hello { proto: 1 },
            ),
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::UnsupportedProtocol,
                ..
            })
        ));
        v2_handshake(&mut router, &mut connection);
        assert!(matches!(
            router.route(
                &mut connection,
                crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot { proto: 1 },
            ),
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::UnsupportedProtocol,
                ..
            })
        ));
    }

    #[test]
    fn v2_read_only_query_does_not_consume_accepted_sequence() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        assert!(matches!(
            router.route(
                &mut connection,
                crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                },
            ),
            V2Route::Query(_)
        ));
        assert!(matches!(
            router.route(
                &mut connection,
                crate::daemon::protocol::v2::ClientMessage::QueryHealth {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
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
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
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
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        router.set_hook_health(crate::daemon::protocol::v2::HookHealth::Degraded);
        let mut connection = V2ConnectionState::default();

        let hello = router.route(
            &mut connection,
            crate::daemon::protocol::v2::ClientMessage::Hello {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            },
        );
        assert!(matches!(
            hello,
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::HelloAck {
                phase: crate::daemon::protocol::v2::DaemonPhase::Serving,
                hook_health: crate::daemon::protocol::v2::HookHealth::Degraded,
                ..
            })
        ));
        assert!(matches!(
            router.route(
                &mut connection,
                crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                },
            ),
            V2Route::Query(
                crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot { .. }
            )
        ));
        assert!(matches!(
            router.route(&mut connection, v2_begin()),
            V2Route::Mutation(V2SequencedMutation {
                accepted_seq: 1,
                mutation: V2AcceptedMutation::External(
                    crate::daemon::protocol::v2::ClientMessage::SubmitPaneEvent { .. }
                ),
            })
        ));
    }

    #[test]
    fn sidebar_jump_requires_one_eligible_client_for_source_pane() {
        let source = crate::pane_state::PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 909,
        };
        let mut views = crate::daemon::view_hooks::ViewRegistry::default();
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
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let command = crate::daemon::protocol::v2::ClientMessage::SidebarCommand {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            daemon_instance_id: v2_daemon_id(),
            event_id: v2_event_id(),
            command: crate::daemon::protocol::v2::SidebarCommand::JumpPane {
                pane_instance: crate::pane_state::PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
                source_pane: crate::pane_state::PaneInstance {
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
            result: SidebarEffectResult::Succeeded,
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
                    pane_instance: crate::pane_state::PaneInstance {
                        pane_id: "%1".to_string(),
                        pane_pid: 101,
                    },
                    client_pid: 10,
                    source_pane: crate::pane_state::PaneInstance {
                        pane_id: "%9".to_string(),
                        pane_pid: 909,
                    },
                },
                expected_pane: crate::pane_state::PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 100,
                },
                original_accepted_seq: 1,
                event_id: v2_event_id(),
                snapshot_revision: 7,
            },
        )
        .unwrap();
        assert!(deferred.lock().unwrap().contains(&1));
        let pending = job_rx.try_recv().expect("job is queued without waiting");

        let coordinator = ProductionV2Coordinator::new(
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: "/tmp/vde-test-tmux.sock".into(),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: "0".repeat(64),
            },
            BTreeMap::new(),
            crate::config::DoneClearOn::Pane,
            None,
        )
        .unwrap();
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
                        result: SidebarEffectResult::Succeeded,
                    },
                )),
            },
        );

        assert!(matches!(
            internal_response,
            crate::daemon::protocol::v2::ServerMessage::SnapshotAck {
                accepted_seq: 2,
                ..
            }
        ));
        assert!(matches!(
            waiter_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            crate::daemon::protocol::v2::ServerMessage::SnapshotAck {
                accepted_seq: 1,
                snapshot_revision: 7,
                ..
            }
        ));
        assert!(!coordinator.is_deferred_response(1));
    }

    #[test]
    fn waiterless_view_queue_completion_does_not_emit_disconnected_diagnostic() {
        let coordinator = ProductionV2Coordinator::new(
            crate::daemon::lifecycle::TmuxServerIncarnation {
                socket_path: "/tmp/vde-test-tmux.sock".into(),
                identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                hash: "f".repeat(64),
            },
            BTreeMap::new(),
            crate::config::DoneClearOn::Pane,
            None,
        )
        .unwrap();
        coordinator
            .router
            .lock()
            .unwrap()
            .set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        assert!(matches!(
            coordinator.route_external(
                &mut connection,
                crate::daemon::protocol::v2::ClientMessage::Hello {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                },
                0,
            ),
            crate::daemon::protocol::v2::ServerMessage::HelloAck { .. }
        ));
        let pane = crate::pane_state::PaneInstance {
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
            crate::daemon::protocol::v2::ClientMessage::SubmitViewEvent {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                event: crate::pane_state::ViewEvent {
                    daemon_instance_id,
                    event_id: v2_event_id(),
                    hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
                    occurrence: Some(crate::pane_state::ViewOccurrence {
                        session_id: "$1".to_string(),
                        window_id: "@1".to_string(),
                        active_pane: pane.clone(),
                        observed_panes: vec![pane],
                    }),
                    source_client: None,
                    witnesses: Vec::new(),
                },
            },
            128,
        );
        assert!(matches!(
            response,
            crate::daemon::protocol::v2::ServerMessage::ViewQueued {
                accepted_seq: 1,
                ..
            }
        ));
        assert!(coordinator.waiters.lock().unwrap().is_empty());
        let queued = coordinator.queue.lock().unwrap().items.pop_front().unwrap();
        assert_eq!(queued.sequenced.accepted_seq, 1);
        assert!(matches!(
            queued.sequenced.mutation,
            V2AcceptedMutation::External(
                crate::daemon::protocol::v2::ClientMessage::SubmitViewEvent { .. }
            )
        ));

        coordinator.complete(
            1,
            crate::daemon::protocol::v2::ServerMessage::SnapshotAck {
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
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Hydrating);
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
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::QueueFull,
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
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Hydrating);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let pane = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let owned_hook_event = crate::daemon::protocol::v2::ClientMessage::SubmitViewEvent {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            event: crate::pane_state::ViewEvent {
                daemon_instance_id: v2_daemon_id(),
                event_id: v2_event_id(),
                hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
                occurrence: Some(crate::pane_state::ViewOccurrence {
                    session_id: "$1".to_string(),
                    window_id: "@1".to_string(),
                    active_pane: pane.clone(),
                    observed_panes: vec![pane],
                }),
                source_client: None,
                witnesses: Vec::new(),
            },
        };
        assert!(matches!(
            router.route(&mut connection, owned_hook_event),
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::ViewQueued {
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
            V2AcceptedMutation::External(
                crate::daemon::protocol::v2::ClientMessage::SubmitViewEvent { .. }
            )
        ));
        assert!(matches!(
            queued[1].mutation,
            V2AcceptedMutation::External(
                crate::daemon::protocol::v2::ClientMessage::SubmitPaneEvent { .. }
            )
        ));
    }

    #[test]
    fn restart_hydration_reads_canonical_state_without_legacy_attention() {
        let framing =
            crate::daemon::topology::QueryFraming::from_token("00112233445566778899aabbccddeeff")
                .unwrap();
        let args = crate::daemon::topology::hydrate_query_args(&framing);
        assert!(args.iter().any(|arg| arg.contains("@vde_pane_state")));
        assert!(args.iter().all(|arg| !arg.contains("@vde_attention")));
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
        assert_eq!(
            router.phase(),
            crate::daemon::protocol::v2::DaemonPhase::Hydrating
        );
        assert!(matches!(
            router.route(
                &mut connection,
                crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                },
            ),
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::NotReady,
                ..
            })
        ));
    }

    #[test]
    fn v2_rejects_stale_instance_and_internal_event_origins() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let mut stale = v2_begin();
        let crate::daemon::protocol::v2::ClientMessage::SubmitPaneEvent { envelope, .. } =
            &mut stale
        else {
            unreachable!();
        };
        envelope.daemon_instance_id =
            crate::pane_state::DaemonInstanceId::parse("00112233445566778899aabbccddeeff").unwrap();
        assert!(matches!(
            router.route(&mut connection, stale),
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::StaleDaemonInstance,
                ..
            })
        ));
        let internal_events = [
            crate::pane_state::PaneEvent::AcknowledgeView {
                expected_state_id: crate::pane_state::StateId::parse(
                    "00112233445566778899aabbccddeeff",
                )
                .unwrap(),
                expected_agent_epoch: 1,
                through_seq: 1,
            },
            crate::pane_state::PaneEvent::ObservationBatch {
                base: None,
                tracker_generation: 0,
                observed_at: 1,
                presence: crate::pane_state::AgentPresenceObservation::Unknown,
                capture: None,
            },
            crate::pane_state::PaneEvent::PaneRemoved { expected: None },
        ];
        for internal_event in internal_events {
            assert!(matches!(
                router.route(&mut connection, v2_pane_event(internal_event)),
                V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                    code: crate::daemon::protocol::v2::ErrorCode::InvalidRequest,
                    ..
                })
            ));
        }
    }

    #[test]
    fn v2_rejects_invalid_view_before_consuming_accepted_sequence() {
        let mut router = V2Router::new(v2_daemon_id(), "server");
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        let pane = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let invalid = crate::daemon::protocol::v2::ClientMessage::SubmitViewEvent {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            event: crate::pane_state::ViewEvent {
                daemon_instance_id: v2_daemon_id(),
                event_id: v2_event_id(),
                hook_kind: crate::pane_state::ViewHookKind::WindowPaneChanged,
                occurrence: Some(crate::pane_state::ViewOccurrence {
                    session_id: "$1".to_string(),
                    window_id: "@1".to_string(),
                    active_pane: pane.clone(),
                    observed_panes: vec![
                        pane,
                        crate::pane_state::PaneInstance {
                            pane_id: "invalid".to_string(),
                            pane_pid: 0,
                        },
                    ],
                }),
                source_client: Some(crate::pane_state::SourceClientHint { client_pid: 10 }),
                witnesses: Vec::new(),
            },
        };
        assert!(matches!(
            router.route(&mut connection, invalid),
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InvalidRequest,
                ..
            })
        ));
        let detached_pane = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let detached_with_occurrence =
            crate::daemon::protocol::v2::ClientMessage::SubmitViewEvent {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                event: crate::pane_state::ViewEvent {
                    daemon_instance_id: v2_daemon_id(),
                    event_id: v2_event_id(),
                    hook_kind: crate::pane_state::ViewHookKind::ClientDetached,
                    occurrence: Some(crate::pane_state::ViewOccurrence {
                        session_id: "$1".to_string(),
                        window_id: "@1".to_string(),
                        active_pane: detached_pane.clone(),
                        observed_panes: vec![detached_pane],
                    }),
                    source_client: Some(crate::pane_state::SourceClientHint { client_pid: 10 }),
                    witnesses: Vec::new(),
                },
            };
        assert!(matches!(
            router.route(&mut connection, detached_with_occurrence),
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InvalidRequest,
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
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        router.set_next_accepted_seq(u64::MAX);
        let mut connection = V2ConnectionState::default();
        v2_handshake(&mut router, &mut connection);
        assert!(matches!(
            router.route(&mut connection, v2_begin()),
            V2Route::Fatal(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InternalError,
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
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InvalidRequest,
                ..
            }
        ));
    }

    #[test]
    fn v2_frame_reader_and_writer_use_newline_framing() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut reader = V2FrameReader::new(server);
        client
            .write_all(
                b"{\"op\":\"hello\",\"proto\":3}\n{\"op\":\"query_resolved_snapshot\",\"proto\":3}\n",
            )
            .unwrap();
        let frame = read_v2_request_frame(&mut reader).unwrap();
        assert_eq!(
            crate::daemon::protocol::v2::decode_request_frame(&frame).unwrap(),
            crate::daemon::protocol::v2::ClientMessage::Hello {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            }
        );
        let second = read_v2_request_frame(&mut reader).unwrap();
        assert_eq!(
            crate::daemon::protocol::v2::decode_request_frame(&second).unwrap(),
            crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot {
                proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            }
        );
        let response = crate::daemon::protocol::v2::ServerMessage::error(
            crate::daemon::protocol::v2::ErrorCode::NotReady,
            "not ready",
            None,
        );
        write_v2_response(reader.stream_mut(), &response).unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<crate::daemon::protocol::v2::ServerMessage>(line.trim())
                .unwrap(),
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
        let oversized = crate::daemon::protocol::v2::ServerMessage::error(
            crate::daemon::protocol::v2::ErrorCode::InternalError,
            "x".repeat(crate::pane_state::MAX_RESPONSE_FRAME_BYTES),
            None,
        );
        write_v2_response(&mut server, &oversized).unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<crate::daemon::protocol::v2::ServerMessage>(line.trim())
                .unwrap(),
            crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::FrameTooLarge,
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
    fn scoped_view_refresh_uses_one_shared_deadline() {
        let deadline = Instant::now() + Duration::from_millis(100);
        let first = scoped_view_refresh_remaining(deadline).unwrap();
        let second = scoped_view_refresh_remaining(deadline).unwrap();
        assert!(second <= first);
        assert!(second <= Duration::from_millis(100));
        assert!(scoped_view_refresh_remaining(Instant::now() - Duration::from_millis(1)).is_err());
    }

    #[test]
    fn quarantine_total_does_not_double_count_restart_hydration() {
        assert_eq!(cumulative_quarantine_total(1, 1, 1), 1);
        assert_eq!(cumulative_quarantine_total(1, 1, 2), 2);
        assert_eq!(cumulative_quarantine_total(2, 1, 1), 2);
    }

    fn legacy_cleanup_topology() -> crate::daemon::topology::TopologySnapshot {
        let link = crate::daemon::protocol::v2::SessionLinkPresentation {
            session_id: "$1".to_string(),
            session_name: "main".to_string(),
            window_index: 1,
            window_active: true,
            window_last: false,
        };
        crate::daemon::topology::TopologySnapshot {
            server_identity: crate::daemon::topology::ServerIdentity {
                pid: 123,
                start_time: 456,
            },
            panes: [
                crate::pane_state::PaneInstance {
                    pane_id: "%2".to_string(),
                    pane_pid: 102,
                },
                crate::pane_state::PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
            ]
            .into_iter()
            .map(|pane_instance| crate::daemon::topology::TopologyPane {
                pane_instance,
                session_links: vec![link.clone()],
                window_id: "@1".to_string(),
                window_name: "main".to_string(),
                current_path: "/tmp".to_string(),
                current_command: "zsh".to_string(),
                pane_width: 80,
                active: true,
            })
            .collect(),
        }
    }

    #[test]
    fn legacy_cleanup_uses_only_fixed_keys_and_deduplicates_linked_targets() {
        let items = legacy_cleanup_items(&legacy_cleanup_topology());

        assert_eq!(items.len(), 32);
        assert_eq!(items.iter().filter(|item| item.scope == "pane").count(), 26);
        assert_eq!(
            items.iter().filter(|item| item.scope == "session").count(),
            3
        );
        assert_eq!(
            items.iter().filter(|item| item.scope == "window").count(),
            3
        );
        let options = items
            .iter()
            .map(|item| item.option)
            .collect::<BTreeSet<_>>();
        assert_eq!(options.len(), 19);
        assert!(!options.contains(crate::options::KEY_PANE_STATE));
        assert!(!options.contains(crate::options::KEY_STATUS_PANE));
        assert!(!options.contains(crate::options::KEY_SIDEBAR_MARKER));
    }

    #[derive(Default)]
    struct LegacyCleanupRunner {
        output: String,
        fail: bool,
        calls: std::cell::RefCell<Vec<Vec<String>>>,
    }

    impl crate::tmux::TmuxRunner for LegacyCleanupRunner {
        fn run(&self, args: &[&str]) -> anyhow::Result<String> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            if self.fail {
                anyhow::bail!("tmux cleanup failed");
            }
            Ok(self.output.clone())
        }
    }

    #[test]
    fn legacy_cleanup_inspection_counts_present_empty_value_and_scope() {
        let topology = legacy_cleanup_topology();
        let candidates = legacy_cleanup_items(&topology)
            .into_iter()
            .filter(|item| item.scope == "pane" && item.option == "@vde_agent")
            .collect::<Vec<_>>();
        let runner = LegacyCleanupRunner {
            output: "@vde_agent \n".to_string(),
            ..LegacyCleanupRunner::default()
        };

        let (existing, failed) = inspect_existing_legacy_cleanup_items(&runner, &candidates);
        let counts = legacy_cleanup_scope_counts(&existing);

        assert_eq!(existing.len(), candidates.len());
        assert!(failed.is_empty());
        assert_eq!(counts.pane, 2);
        assert_eq!(counts.window, 0);
        assert_eq!(counts.session, 0);
    }

    #[test]
    fn legacy_cleanup_failure_report_is_bounded_with_omitted_count() {
        let failures = (0..300)
            .map(|index| crate::daemon::protocol::v2::LegacyCleanupFailure {
                scope: "pane".to_string(),
                target: format!("%{index}"),
                option: "@vde_agent".to_string(),
                message: "remained".to_string(),
            })
            .collect();

        let (reported, total, omitted) = bound_legacy_cleanup_failures(failures);

        assert_eq!(reported.len(), 256);
        assert_eq!(total, 300);
        assert_eq!(omitted, 44);
    }

    #[test]
    fn legacy_cleanup_is_one_server_guarded_batch_with_per_pane_pid_guards() {
        let topology = legacy_cleanup_topology();
        let items = legacy_cleanup_items(&topology);
        let runner = LegacyCleanupRunner::default();

        let outcome = execute_legacy_cleanup(&runner, &topology.server_identity, &items);

        assert_eq!(outcome.attempted, 32);
        assert_eq!(outcome.removed, 32);
        assert!(outcome.failed.is_empty());
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "if-shell");
        assert!(calls[0][2].contains("#{pid},123"));
        assert!(calls[0][2].contains("#{start_time},456"));
        assert!(calls[0][3].contains("#{pane_pid},101"));
        assert!(calls[0][3].contains("#{pane_pid},102"));
        for option in crate::options::LEGACY_PANE_OPTION_KEYS
            .iter()
            .chain(crate::options::LEGACY_SESSION_OPTION_KEYS)
            .chain(crate::options::LEGACY_WINDOW_OPTION_KEYS)
        {
            assert!(calls[0][3].contains(option), "missing {option}");
        }
    }

    #[test]
    fn legacy_cleanup_reports_pane_instance_mismatch_without_counting_removal() {
        let topology = legacy_cleanup_topology();
        let items = legacy_cleanup_items(&topology);
        let runner = LegacyCleanupRunner {
            output: format!("{LEGACY_CLEANUP_PANE_MISMATCH_PREFIX}:0\n"),
            ..LegacyCleanupRunner::default()
        };

        let outcome = execute_legacy_cleanup(&runner, &topology.server_identity, &items);

        assert_eq!(outcome.attempted, 32);
        assert_eq!(outcome.removed, 31);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].scope, "pane");
        assert_eq!(outcome.failed[0].target, "%1");
        assert_eq!(outcome.failed[0].option, "@vde_agent");
    }

    #[test]
    fn legacy_cleanup_server_mismatch_is_terminal_and_removes_nothing() {
        let topology = legacy_cleanup_topology();
        let items = legacy_cleanup_items(&topology);
        let runner = LegacyCleanupRunner {
            output: format!("{LEGACY_CLEANUP_SERVER_MISMATCH_SENTINEL}\n"),
            ..LegacyCleanupRunner::default()
        };

        let outcome = execute_legacy_cleanup(&runner, &topology.server_identity, &items);

        assert!(outcome.server_mismatch);
        assert_eq!(outcome.removed, 0);
        assert!(outcome.failed.is_empty());
    }

    #[test]
    fn legacy_cleanup_empty_topology_is_a_zero_attempt_noop() {
        let topology = crate::daemon::topology::TopologySnapshot {
            server_identity: crate::daemon::topology::ServerIdentity {
                pid: 123,
                start_time: 456,
            },
            panes: Vec::new(),
        };
        let runner = LegacyCleanupRunner::default();

        let outcome = execute_legacy_cleanup(
            &runner,
            &topology.server_identity,
            &legacy_cleanup_items(&topology),
        );

        assert_eq!(outcome.attempted, 0);
        assert_eq!(outcome.removed, 0);
        assert!(outcome.failed.is_empty());
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn legacy_cleanup_batch_failure_is_typed_for_each_attempt() {
        let topology = legacy_cleanup_topology();
        let items = legacy_cleanup_items(&topology);
        let runner = LegacyCleanupRunner {
            fail: true,
            ..LegacyCleanupRunner::default()
        };

        let outcome = execute_legacy_cleanup(&runner, &topology.server_identity, &items);

        assert_eq!(outcome.attempted, 32);
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.failed.len(), 32);
        assert!(
            outcome.failed.iter().all(|failure| failure.message
                == "tmux legacy cleanup batch failed: tmux cleanup failed")
        );
    }

    #[test]
    fn shutdown_forwarder_stops_v2_coordinator() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let (mut signal_writer, signal_reader) = UnixStream::pair().unwrap();
        let coordinator = Arc::new(
            ProductionV2Coordinator::new(
                crate::daemon::lifecycle::TmuxServerIncarnation {
                    socket_path: "/tmp/vde-test-tmux.sock".into(),
                    identity: crate::daemon::topology::ServerIdentity {
                        pid: 1,
                        start_time: 2,
                    },
                    hash: "1".repeat(64),
                },
                BTreeMap::new(),
                crate::config::DoneClearOn::Pane,
                None,
            )
            .unwrap(),
        );
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

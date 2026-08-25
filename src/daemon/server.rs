use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(test)]
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use base64::Engine as _;

#[cfg(test)]
use crate::daemon::protocol::v2::HookHealth;
use crate::daemon::protocol::v2::{ClientMessage, DaemonPhase, ErrorCode, ServerMessage};
use crate::pane_state::{DaemonInstanceId, EventId, PaneEvent, PaneEventEnvelope, PaneInstance};

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

mod bootstrap;
mod contracts;
mod effects;
mod framing;
mod mutations;
mod observation;
mod router;
mod state_helpers;

pub(crate) use contracts::SidebarEffectCompletion;
use contracts::SidebarEffectResult;
use effects::{
    NVIM_PROCESS_PID_OPTION, NotificationWorkerJob, NvimPaneMarker, SidebarTmuxJob,
    enqueue_sidebar_tmux_job, parse_nvim_pane_markers, stale_nvim_marker_cleanup_command,
    start_notification_worker_with_control, start_sidebar_tmux_worker,
};
use framing::{V2ConnectionThreadPermit, write_v2_frame};
#[cfg(test)]
use observation::{ObservationPollFraming, targeted_pane_refresh_outcome_response};
use observation::{
    observation_poll_error_response, query_client_witnesses, refresh_full_topology,
    targeted_pane_refresh_response,
};
use state_helpers::{pane_snapshot_store, production_store_error_response};

pub use bootstrap::run_runtime_daemon_server;
pub use framing::{V2FrameReader, read_v2_request_frame, write_v2_response};
// Preserve the existing crate-visible server paths while production children
// depend on the router owner directly.
#[allow(unused_imports)]
pub(crate) use router::{
    ObservationBatchPayload, ObservationPollProjection, V2AcceptedMutation, V2InternalMutation,
    V2Route, V2SequencedMutation,
};
pub use router::{V2ConnectionState, V2Router};

use bootstrap::initial_view_reconciliation;
use mutations::agent::{
    agent_state_query_error, apply_resolve_agent_run, apply_start_agent_prompt,
};
use mutations::pane::{
    apply_diagnostic_projection, apply_external_view_event, apply_observation_batch,
};
use mutations::provider::{apply_external_pane_event, apply_external_provider_event};
use mutations::sidebar::{
    apply_category_intent, apply_sidebar_navigation, apply_sidebar_preference_intent,
    commit_read_peek_state, eligible_witness_matches, read_peek_advance_outcome,
    task_summary_failure_code, unique_eligible_client_pid,
};
use observation::reconcile_views_with_witnesses;

#[derive(Debug)]
struct ProductionMutation {
    sequenced: V2SequencedMutation,
    raw_frame_bytes: usize,
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

fn epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
include!("server/test_support.rs");

#[cfg(test)]
mod tests;

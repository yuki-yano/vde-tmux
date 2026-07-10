use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::protocol::{ClientMessage, QueryTarget, ServerMessage};
use super::runtime::{ClientId, DaemonEvent, LatestSlot, RuntimeEffect, RuntimeState};
use crate::config::Config;
use crate::daemon::{build_snapshot, statusline_attention_fallback, statusline_summary_fallback};
use crate::options::snapshot::read_all_panes;
use crate::tmux::TmuxRunner;

const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
static SHUTDOWN_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

const V2_BOOTSTRAP_FIFO_CAPACITY: usize = 64;
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
        if frame.len().saturating_add(take).saturating_sub(1) > MAX_REQUEST_FRAME_BYTES {
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

#[allow(clippy::result_large_err)]
pub fn write_v2_response(
    stream: &mut UnixStream,
    message: &crate::daemon::protocol::v2::ServerMessage,
) -> std::result::Result<(), crate::daemon::protocol::v2::ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage, encode_response_frame};

    let frame = encode_response_frame(message)?;
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
        stream.set_write_timeout(Some(remaining)).map_err(|error| {
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

#[derive(Debug, Clone, Default)]
pub struct V2ConnectionState {
    hello_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2SequencedMutation {
    pub accepted_seq: u64,
    pub message: crate::daemon::protocol::v2::ClientMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum V2Route {
    Response(crate::daemon::protocol::v2::ServerMessage),
    Fatal(crate::daemon::protocol::v2::ServerMessage),
    Query(crate::daemon::protocol::v2::ClientMessage),
    Mutation(V2SequencedMutation),
    Queued { accepted_seq: u64 },
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

    pub fn set_phase(&mut self, phase: crate::daemon::protocol::v2::DaemonPhase) {
        self.phase = phase;
    }

    pub fn set_hook_health(&mut self, health: crate::daemon::protocol::v2::HookHealth) {
        self.hook_health = health;
    }

    pub fn is_fatal(&self) -> bool {
        self.fatal
    }

    pub fn route(
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
                    "protocol version 2 is required",
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
                "protocol version 2 is required",
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

        let accepted_seq = match self.next_accepted_seq.checked_add(1) {
            Some(next) => {
                let accepted = self.next_accepted_seq;
                self.next_accepted_seq = next;
                accepted
            }
            None => {
                self.fatal = true;
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
            message,
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

    pub fn drain_bootstrap_fifo(&mut self) -> Vec<V2SequencedMutation> {
        if self.phase != crate::daemon::protocol::v2::DaemonPhase::Serving {
            return Vec::new();
        }
        self.bootstrap_fifo.drain(..).collect()
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
    use crate::pane_state::PaneEvent;

    match message {
        ClientMessage::SubmitPaneEvent { envelope, .. }
            if !matches!(
                envelope.event,
                PaneEvent::AgentSessionStarted { .. }
                    | PaneEvent::BeginRun { .. }
                    | PaneEvent::ActivityObserved { .. }
                    | PaneEvent::WaitRequested { .. }
                    | PaneEvent::CompleteRun { .. }
                    | PaneEvent::FailRun { .. }
                    | PaneEvent::ProgressUpdated { .. }
                    | PaneEvent::ExplicitStateReported { .. }
            ) =>
        {
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

pub fn handle_message(
    runner: &dyn TmuxRunner,
    config: &Config,
    message: ClientMessage,
) -> Result<ServerMessage> {
    match message {
        ClientMessage::Query {
            proto: _,
            what: QueryTarget::Summary,
        } => {
            let text = statusline_summary_fallback(runner, config)?;
            Ok(ServerMessage::Summary { text })
        }
        ClientMessage::Query {
            proto: _,
            what: QueryTarget::Attention,
        } => {
            let text = statusline_attention_fallback(runner)?;
            Ok(ServerMessage::Attention { text })
        }
        ClientMessage::Subscribe { proto: _ } => {
            let panes = read_all_panes(runner)?;
            Ok(ServerMessage::Snapshot {
                snapshot: build_snapshot(&panes),
            })
        }
        ClientMessage::SidebarEvent { .. } => Ok(ServerMessage::Error {
            message: "sidebar events require runtime daemon".to_string(),
        }),
        ClientMessage::RefreshPanes { .. } => Ok(ServerMessage::Error {
            message: "refresh_panes requires runtime daemon".to_string(),
        }),
        ClientMessage::Shutdown { .. } => Ok(ServerMessage::Error {
            message: "shutdown requires runtime daemon".to_string(),
        }),
    }
}

pub fn handle_stream(
    runner: &dyn TmuxRunner,
    config: &Config,
    mut stream: UnixStream,
) -> Result<()> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader.read_line(&mut line)?;
    }
    let response = match serde_json::from_str::<ClientMessage>(line.trim()) {
        Ok(message) => handle_message(runner, config, message)?,
        Err(error) => ServerMessage::Error {
            message: error.to_string(),
        },
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

pub fn handle_stream_with_runtime(
    tx: Sender<DaemonEvent>,
    client_id: ClientId,
    mut stream: UnixStream,
) -> Result<()> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader.read_line(&mut line)?;
    }
    let message = serde_json::from_str::<ClientMessage>(line.trim())?;
    match message {
        ClientMessage::Subscribe { proto: _ } => {
            let slot = Arc::new(LatestSlot::new());
            tx.send(DaemonEvent::Connect {
                client_id,
                slot: slot.clone(),
            })?;
            spawn_client_writer(client_id, stream, slot, tx);
        }
        ClientMessage::SidebarEvent { proto: _, event } => {
            tx.send(DaemonEvent::Client { client_id, event })?;
            write_server_message(&mut stream, &ServerMessage::Ack)?;
        }
        ClientMessage::RefreshPanes { proto: _ } => {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(DaemonEvent::RefreshPanes { reply: reply_tx })?;
            let response = reply_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or_else(|error| ServerMessage::Error {
                    message: error.to_string(),
                });
            write_server_message(&mut stream, &response)?;
        }
        ClientMessage::Shutdown { proto: _ } => {
            tx.send(DaemonEvent::Shutdown)?;
            write_server_message(&mut stream, &ServerMessage::Ack)?;
        }
        ClientMessage::Query {
            proto: _,
            what: QueryTarget::Summary,
        } => {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(DaemonEvent::QuerySummary { reply: reply_tx })?;
            let response = reply_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or_else(|error| ServerMessage::Error {
                    message: error.to_string(),
                });
            write_server_message(&mut stream, &response)?;
        }
        ClientMessage::Query {
            proto: _,
            what: QueryTarget::Attention,
        } => {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(DaemonEvent::QueryAttention { reply: reply_tx })?;
            let response = reply_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or_else(|error| ServerMessage::Error {
                    message: error.to_string(),
                });
            write_server_message(&mut stream, &response)?;
        }
    }
    Ok(())
}

fn spawn_client_writer(
    client_id: ClientId,
    mut stream: UnixStream,
    slot: Arc<LatestSlot<ServerMessage>>,
    tx: Sender<DaemonEvent>,
) {
    if let Err(error) = configure_client_writer_stream(&stream) {
        eprintln!("[vde-tmux] daemon client writer setup error: {error:#}");
        let _ = tx.send(DaemonEvent::Disconnect { client_id });
        return;
    }
    thread::spawn(move || {
        while let Some(message) = slot.wait_for_update() {
            if let Err(error) = write_server_message(&mut stream, &message) {
                eprintln!("[vde-tmux] daemon client writer error: {error:#}");
                let _ = tx.send(DaemonEvent::Disconnect { client_id });
                break;
            }
        }
    });
}

fn configure_client_writer_stream(stream: &UnixStream) -> Result<()> {
    stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
    Ok(())
}

fn write_server_message(stream: &mut UnixStream, message: &ServerMessage) -> Result<()> {
    serde_json::to_writer(&mut *stream, message)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

pub fn run_daemon_server(
    runner: &dyn TmuxRunner,
    config: &Config,
    socket_path: &Path,
) -> Result<()> {
    let Some((listener, _instance_lock)) = bind_daemon_listener(socket_path)? else {
        return Ok(());
    };
    for stream in listener.incoming() {
        let stream = stream?;
        if let Err(error) = handle_stream(runner, config, stream) {
            eprintln!("[vde-tmux] daemon connection error: {error:#}");
        }
    }
    Ok(())
}

pub fn run_runtime_daemon_server(
    config: crate::config::Config,
    socket_path: &Path,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let Some((listener, _instance_lock)) = bind_daemon_listener(socket_path)? else {
        return Ok(());
    };

    let (tx, rx) = mpsc::channel();
    install_shutdown_signal_handler(tx.clone())?;
    let latest_panes = Arc::new(crate::daemon::workers::LatestPanes::default());
    let capture_activity = Arc::new(crate::daemon::workers::SharedCaptureActivity::new());
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    let worker_io = Arc::new(crate::daemon::workers::SystemWorkerIo::new(runner));
    crate::daemon::workers::start_tmux_worker(
        worker_io.clone(),
        latest_panes.clone(),
        capture_activity.clone(),
        tx.clone(),
        Duration::from_millis(config.daemon.poll_ms),
        300,
    );
    crate::daemon::workers::start_git_worker(
        Arc::new(crate::daemon::workers::system_git_runner(
            Duration::from_millis(config.daemon.git.timeout_ms),
        )),
        latest_panes,
        tx.clone(),
        Duration::from_millis(config.daemon.git.poll_interval_ms),
    );

    let listener_tx = tx.clone();
    thread::spawn(move || {
        let mut next_client_id = 1_u64;
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let client_id = ClientId(next_client_id);
                    next_client_id += 1;
                    if let Err(error) =
                        handle_stream_with_runtime(listener_tx.clone(), client_id, stream)
                    {
                        eprintln!("[vde-tmux] daemon connection error: {error:#}");
                    }
                }
                Err(error) => {
                    eprintln!("[vde-tmux] daemon listener error: {error:#}");
                    break;
                }
            }
        }
    });

    let state_path = crate::sidebar::store::state_path(env);
    let ui_state = crate::sidebar::store::load_state(&state_path)?;
    run_runtime_loop(
        RuntimeState::new(config, ui_state),
        rx,
        Some(state_path),
        worker_io,
        capture_activity,
    )
}

fn bind_daemon_listener(
    socket_path: &Path,
) -> Result<Option<(UnixListener, crate::daemon::lifecycle::DaemonFileLock)>> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    if crate::daemon::lifecycle::daemon_socket_responds(socket_path) {
        return Ok(None);
    }
    let Some(instance_lock) =
        crate::daemon::lifecycle::try_acquire_daemon_instance_lock(socket_path)?
    else {
        if crate::daemon::lifecycle::wait_for_daemon_socket(socket_path, Duration::from_secs(5)) {
            return Ok(None);
        }
        bail!(
            "daemon instance lock is already held for {}",
            socket_path.display()
        );
    };
    if crate::daemon::lifecycle::daemon_socket_responds(socket_path) {
        return Ok(None);
    }
    if socket_path.exists() {
        fs::remove_file(socket_path)
            .with_context(|| format!("failed to remove {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    Ok(Some((listener, instance_lock)))
}

pub fn install_shutdown_signal_handler(tx: Sender<DaemonEvent>) -> Result<()> {
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
    spawn_shutdown_forwarder(reader, tx);
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

fn spawn_shutdown_forwarder<R>(mut reader: R, tx: Sender<DaemonEvent>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut byte = [0_u8; 1];
        if reader.read(&mut byte).is_ok() {
            let _ = tx.send(DaemonEvent::Shutdown);
        }
    });
}

pub fn run_runtime_loop(
    mut state: RuntimeState,
    rx: mpsc::Receiver<DaemonEvent>,
    state_path: Option<std::path::PathBuf>,
    worker_io: Arc<dyn crate::daemon::workers::WorkerIo>,
    capture_activity: Arc<crate::daemon::workers::SharedCaptureActivity>,
) -> Result<()> {
    let notify_command = state.notify_command().map(str::to_string);
    while state.is_running() {
        let effects = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(DaemonEvent::RefreshPanes { reply }) => refresh_panes_once(
                &mut state,
                worker_io.as_ref(),
                capture_activity.as_ref(),
                reply,
            ),
            Ok(event) => state.apply_event(event),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                state.apply_event(DaemonEvent::DebounceCheck(std::time::Instant::now()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        handle_runtime_effects(
            effects,
            state_path.as_deref(),
            worker_io.as_ref(),
            notify_command.as_deref(),
        )?;
    }
    handle_runtime_effects(
        state.apply_event(DaemonEvent::DebounceCheck(
            std::time::Instant::now() + Duration::from_secs(1),
        )),
        state_path.as_deref(),
        worker_io.as_ref(),
        notify_command.as_deref(),
    )?;
    Ok(())
}

fn refresh_panes_once(
    state: &mut RuntimeState,
    worker_io: &dyn crate::daemon::workers::WorkerIo,
    capture_activity: &crate::daemon::workers::SharedCaptureActivity,
    reply: Sender<ServerMessage>,
) -> Vec<RuntimeEffect> {
    match crate::daemon::workers::read_panes_with_shared_capture_activity(
        worker_io,
        300,
        capture_activity,
    ) {
        Ok(panes) => {
            let effects = state.apply_event(DaemonEvent::PanesUpdated(panes));
            let _ = reply.send(ServerMessage::Ack);
            effects
        }
        Err(error) => {
            let _ = reply.send(ServerMessage::Error {
                message: error.to_string(),
            });
            Vec::new()
        }
    }
}

fn handle_runtime_effects(
    effects: Vec<RuntimeEffect>,
    state_path: Option<&Path>,
    worker_io: &dyn crate::daemon::workers::WorkerIo,
    notify_command: Option<&str>,
) -> Result<()> {
    for effect in effects {
        match effect {
            RuntimeEffect::JumpPane(pane_id) => {
                if let Err(error) = worker_io.jump_to_pane(&pane_id) {
                    eprintln!("[vde-tmux] daemon jump error: {error:#}");
                }
            }
            RuntimeEffect::PreviewPane {
                pane_id,
                history_lines,
            } => {
                if let Err(error) = worker_io.preview_pane(&pane_id, history_lines) {
                    eprintln!("[vde-tmux] daemon preview error: {error:#}");
                }
            }
            RuntimeEffect::SaveState(state) => {
                if let Some(path) = state_path {
                    crate::sidebar::store::save_state(path, &state)?;
                }
            }
            RuntimeEffect::SetSessionBadge {
                session,
                value,
                state,
            } => {
                if let Err(error) = worker_io.set_session_option(
                    &session,
                    crate::options::KEY_SESSION_STATUS,
                    &value,
                ) {
                    eprintln!("[vde-tmux] session badge set failed: {error:#}");
                }
                if let Err(error) = worker_io.set_session_option(
                    &session,
                    crate::options::KEY_SESSION_STATE,
                    &state,
                ) {
                    eprintln!("[vde-tmux] session state set failed: {error:#}");
                }
            }
            RuntimeEffect::SetSessionProjectPath { session, path } => {
                if let Err(error) =
                    worker_io.set_session_option(&session, crate::options::KEY_PROJECT_PATH, &path)
                {
                    eprintln!("[vde-tmux] session project path set failed: {error:#}");
                }
            }
            RuntimeEffect::SetSessionCategory { session, category } => {
                if let Err(error) =
                    worker_io.set_session_option(&session, crate::options::KEY_CATEGORY, &category)
                {
                    eprintln!("[vde-tmux] session category set failed: {error:#}");
                }
            }
            RuntimeEffect::SetSessionAgentCounts { session, counts } => {
                if let Err(error) = worker_io.set_session_option(
                    &session,
                    crate::options::KEY_SESSION_AGENT_COUNTS,
                    &counts,
                ) {
                    eprintln!("[vde-tmux] session agent counts set failed: {error:#}");
                }
            }
            RuntimeEffect::ClearSessionBadge { session } => {
                if let Err(error) =
                    worker_io.unset_session_option(&session, crate::options::KEY_SESSION_STATUS)
                {
                    eprintln!("[vde-tmux] session badge clear failed: {error:#}");
                }
                if let Err(error) =
                    worker_io.unset_session_option(&session, crate::options::KEY_SESSION_STATE)
                {
                    eprintln!("[vde-tmux] session state clear failed: {error:#}");
                }
            }
            RuntimeEffect::ClearSessionAgentCounts { session } => {
                if let Err(error) = worker_io
                    .unset_session_option(&session, crate::options::KEY_SESSION_AGENT_COUNTS)
                {
                    eprintln!("[vde-tmux] session agent counts clear failed: {error:#}");
                }
            }
            RuntimeEffect::SetWindowBadge {
                window,
                value,
                state,
                counts,
            } => {
                if let Err(error) =
                    worker_io.set_window_option(&window, crate::options::KEY_WINDOW_STATUS, &value)
                {
                    eprintln!("[vde-tmux] window badge set failed: {error:#}");
                }
                if let Err(error) =
                    worker_io.set_window_option(&window, crate::options::KEY_WINDOW_STATE, &state)
                {
                    eprintln!("[vde-tmux] window state set failed: {error:#}");
                }
                if let Err(error) = worker_io.set_window_option(
                    &window,
                    crate::options::KEY_WINDOW_AGENT_COUNTS,
                    &counts,
                ) {
                    eprintln!("[vde-tmux] window agent counts set failed: {error:#}");
                }
            }
            RuntimeEffect::ClearWindowBadge { window } => {
                if let Err(error) =
                    worker_io.unset_window_option(&window, crate::options::KEY_WINDOW_STATUS)
                {
                    eprintln!("[vde-tmux] window badge clear failed: {error:#}");
                }
                if let Err(error) =
                    worker_io.unset_window_option(&window, crate::options::KEY_WINDOW_STATE)
                {
                    eprintln!("[vde-tmux] window state clear failed: {error:#}");
                }
                if let Err(error) =
                    worker_io.unset_window_option(&window, crate::options::KEY_WINDOW_AGENT_COUNTS)
                {
                    eprintln!("[vde-tmux] window agent counts clear failed: {error:#}");
                }
            }
            RuntimeEffect::ClearPaneState { pane_id } => {
                for key in crate::options::PANE_STATE_KEYS {
                    if let Err(error) = worker_io.unset_pane_option(&pane_id, key) {
                        eprintln!("[vde-tmux] pane state clear failed: {pane_id} {key}: {error:#}");
                    }
                }
            }
            RuntimeEffect::MarkPaneDone {
                pane_id,
                completed_at,
            } => {
                if let Err(error) =
                    worker_io.set_pane_option(&pane_id, crate::options::KEY_STATUS, "idle")
                {
                    eprintln!("[vde-tmux] mark done status write failed: {pane_id}: {error:#}");
                }
                if let Err(error) =
                    worker_io.set_pane_option(&pane_id, crate::options::KEY_ATTENTION, "1")
                {
                    eprintln!("[vde-tmux] mark done attention write failed: {pane_id}: {error:#}");
                }
                if let Err(error) = worker_io.set_pane_option(
                    &pane_id,
                    crate::options::KEY_COMPLETED_AT,
                    &completed_at.to_string(),
                ) {
                    eprintln!(
                        "[vde-tmux] mark done completed_at write failed: {pane_id}: {error:#}"
                    );
                }
                for key in [
                    crate::options::KEY_WAIT_REASON,
                    crate::options::KEY_TASKS,
                    crate::options::KEY_TASK_ITEMS,
                    crate::options::KEY_TASK_ITEM_IDS,
                    crate::options::KEY_SUBAGENTS,
                ] {
                    if let Err(error) = worker_io.unset_pane_option(&pane_id, key) {
                        eprintln!("[vde-tmux] mark done unset failed: {pane_id} {key}: {error:#}");
                    }
                }
            }
            RuntimeEffect::Notify {
                pane_id,
                agent,
                state,
            } => {
                if let Some(command) = notify_command
                    && let Err(error) =
                        worker_io.run_notify(command, &pane_id, &agent, &format!("{state:?}"))
                {
                    eprintln!("[vde-tmux] notify command failed: {error:#}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{ClientMessage, ServerMessage};
    use crate::options::snapshot::snapshot_format;
    use crate::tmux::mock::MockTmuxRunner;

    const V2_EVENT_ID: &str = "102132435465768798a9bacbdcedfe0f";
    const V2_DAEMON_ID: &str = "ffeeddccbbaa99887766554433221100";

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
            proto: 2,
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
            crate::daemon::protocol::v2::ClientMessage::Hello { proto: 2 },
        );
        assert!(matches!(
            route,
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::HelloAck {
                proto: 2,
                ..
            })
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
                crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot { proto: 2 },
            ),
            V2Route::Query(_)
        ));
        let V2Route::Mutation(mutation) = router.route(&mut connection, v2_begin()) else {
            panic!("expected mutation");
        };
        assert_eq!(mutation.accepted_seq, 1);
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
        router.set_phase(crate::daemon::protocol::v2::DaemonPhase::Serving);
        let queued = router.drain_bootstrap_fifo();
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
        let internal = v2_pane_event(crate::pane_state::PaneEvent::AcknowledgeView {
            expected_state_id: crate::pane_state::StateId::parse(
                "00112233445566778899aabbccddeeff",
            )
            .unwrap(),
            expected_agent_epoch: 1,
            through_seq: 1,
        });
        assert!(matches!(
            router.route(&mut connection, internal),
            V2Route::Response(crate::daemon::protocol::v2::ServerMessage::Error {
                code: crate::daemon::protocol::v2::ErrorCode::InvalidRequest,
                ..
            })
        ));
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
                b"{\"op\":\"hello\",\"proto\":2}\n{\"op\":\"query_resolved_snapshot\",\"proto\":2}\n",
            )
            .unwrap();
        let frame = read_v2_request_frame(&mut reader).unwrap();
        assert_eq!(
            crate::daemon::protocol::v2::decode_request_frame(&frame).unwrap(),
            crate::daemon::protocol::v2::ClientMessage::Hello { proto: 2 }
        );
        let second = read_v2_request_frame(&mut reader).unwrap();
        assert_eq!(
            crate::daemon::protocol::v2::decode_request_frame(&second).unwrap(),
            crate::daemon::protocol::v2::ClientMessage::QueryResolvedSnapshot { proto: 2 }
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

    fn pane_line(agent: &str, status: &str, wait_reason: &str) -> String {
        [
            "main",
            "@1",
            "%1",
            "/tmp",
            agent,
            "/dev/ttys001",
            "123",
            "0",
            "0",
            "0",
            "",
            "",
            "",
            "",
            agent,
            status,
            "",
            "",
            wait_reason,
            "",
            if status == "running" { "100" } else { "" },
            "",
            "",
            "",
            "",
            "",
        ]
        .join("\u{1f}")
    }

    #[test]
    fn handle_query_returns_summary_payload() {
        let mock = MockTmuxRunner::new();
        let format = snapshot_format();
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            &format!("{}\n", pane_line("codex", "running", "")),
        );
        let response = handle_message(
            &mock,
            &Config::default(),
            ClientMessage::Query {
                proto: 1,
                what: crate::daemon::protocol::QueryTarget::Summary,
            },
        )
        .unwrap();
        assert_eq!(
            response,
            ServerMessage::Summary {
                text: "#[fg=#4fd08a]●1#[default]".to_string()
            }
        );
    }

    #[test]
    fn handle_query_summary_uses_supplied_config() {
        let mock = MockTmuxRunner::new();
        let format = snapshot_format();
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            &format!("{}\n", pane_line("codex", "running", "")),
        );
        let mut config = crate::config::Config::default();
        config.statusline.summary.enabled = false;

        let response = handle_message(
            &mock,
            &config,
            ClientMessage::Query {
                proto: 1,
                what: crate::daemon::protocol::QueryTarget::Summary,
            },
        )
        .unwrap();

        assert_eq!(
            response,
            ServerMessage::Summary {
                text: String::new()
            }
        );
    }

    #[test]
    fn handle_subscribe_returns_snapshot() {
        let mock = MockTmuxRunner::new();
        let format = snapshot_format();
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            &format!("{}\n", pane_line("codex", "running", "")),
        );
        let response = handle_message(
            &mock,
            &Config::default(),
            ClientMessage::Subscribe { proto: 1 },
        )
        .unwrap();
        let ServerMessage::Snapshot { snapshot } = response else {
            panic!("expected snapshot response");
        };
        assert_eq!(snapshot.agent_count, 1);
    }

    #[test]
    fn handle_subscribe_keeps_connection_and_pushes_snapshot() {
        use crate::daemon::protocol::ServerMessage;
        use crate::daemon::runtime::{ClientId, DaemonEvent};
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;

        let (mut client, server) = UnixStream::pair().unwrap();
        serde_json::to_writer(&mut client, &ClientMessage::Subscribe { proto: 1 }).unwrap();
        client.write_all(b"\n").unwrap();

        let (tx, rx) = mpsc::channel();
        handle_stream_with_runtime(tx, ClientId(1), server).unwrap();
        let DaemonEvent::Connect { slot, .. } = rx.recv().unwrap() else {
            panic!("expected connect event");
        };
        slot.publish(ServerMessage::Ack);

        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), r#"{"type":"ack"}"#);
    }

    #[test]
    fn client_writer_stream_has_write_timeout() {
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let (_client, server) = UnixStream::pair().unwrap();

        configure_client_writer_stream(&server).unwrap();

        assert_eq!(
            server.write_timeout().unwrap(),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn bind_daemon_listener_keeps_existing_responsive_socket() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        use std::thread;

        let dir = std::path::PathBuf::from(format!(
            "/tmp/vt-srv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            serde_json::to_writer(
                &mut stream,
                &ServerMessage::Summary {
                    text: String::new(),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let listener = bind_daemon_listener(&socket).unwrap();

        assert!(listener.is_none());
        handle.join().unwrap();
        assert!(socket.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn shutdown_forwarder_sends_shutdown_event() {
        use crate::daemon::runtime::DaemonEvent;
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;
        use std::time::Duration;

        let (mut signal_writer, signal_reader) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        spawn_shutdown_forwarder(signal_reader, tx);

        signal_writer.write_all(b"x").unwrap();

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            DaemonEvent::Shutdown
        ));
    }

    #[test]
    fn handle_runtime_shutdown_sends_shutdown_event_and_ack() {
        use crate::daemon::runtime::{ClientId, DaemonEvent};
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;
        use std::time::Duration;

        let (mut client, server) = UnixStream::pair().unwrap();
        serde_json::to_writer(&mut client, &ClientMessage::Shutdown { proto: 1 }).unwrap();
        client.write_all(b"\n").unwrap();

        let (tx, rx) = mpsc::channel();
        handle_stream_with_runtime(tx, ClientId(1), server).unwrap();

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            DaemonEvent::Shutdown
        ));

        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), r#"{"type":"ack"}"#);
    }

    #[derive(Default)]
    struct LoopWorkerIo {
        panes: std::sync::Mutex<Vec<crate::options::snapshot::PaneSnapshot>>,
        captures: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
        jumps: std::sync::Mutex<Vec<String>>,
        previews: std::sync::Mutex<Vec<(String, u32)>>,
        pane_options: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
        session_options: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
        window_options: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
        notify_calls: std::sync::Mutex<Vec<(String, String, String, String)>>,
        fail_jump: bool,
    }

    impl crate::daemon::workers::WorkerIo for LoopWorkerIo {
        fn read_panes(&self) -> anyhow::Result<Vec<crate::options::snapshot::PaneSnapshot>> {
            Ok(self.panes.lock().unwrap().clone())
        }

        fn capture_tail(&self, pane_id: &str) -> anyhow::Result<String> {
            Ok(self
                .captures
                .lock()
                .unwrap()
                .get(pane_id)
                .cloned()
                .unwrap_or_default())
        }

        fn jump_to_pane(&self, pane_id: &str) -> anyhow::Result<()> {
            if self.fail_jump {
                anyhow::bail!("jump failed");
            }
            self.jumps.lock().unwrap().push(pane_id.to_string());
            Ok(())
        }

        fn preview_pane(&self, pane_id: &str, history_lines: u32) -> anyhow::Result<()> {
            self.previews
                .lock()
                .unwrap()
                .push((pane_id.to_string(), history_lines));
            Ok(())
        }

        fn set_pane_option(&self, pane_id: &str, key: &str, value: &str) -> anyhow::Result<()> {
            self.pane_options.lock().unwrap().push((
                pane_id.to_string(),
                key.to_string(),
                Some(value.to_string()),
            ));
            Ok(())
        }

        fn unset_pane_option(&self, pane_id: &str, key: &str) -> anyhow::Result<()> {
            self.pane_options
                .lock()
                .unwrap()
                .push((pane_id.to_string(), key.to_string(), None));
            Ok(())
        }

        fn set_session_option(&self, session: &str, key: &str, value: &str) -> anyhow::Result<()> {
            self.session_options.lock().unwrap().push((
                session.to_string(),
                key.to_string(),
                Some(value.to_string()),
            ));
            Ok(())
        }

        fn unset_session_option(&self, session: &str, key: &str) -> anyhow::Result<()> {
            self.session_options
                .lock()
                .unwrap()
                .push((session.to_string(), key.to_string(), None));
            Ok(())
        }

        fn set_window_option(&self, window: &str, key: &str, value: &str) -> anyhow::Result<()> {
            self.window_options.lock().unwrap().push((
                window.to_string(),
                key.to_string(),
                Some(value.to_string()),
            ));
            Ok(())
        }

        fn unset_window_option(&self, window: &str, key: &str) -> anyhow::Result<()> {
            self.window_options
                .lock()
                .unwrap()
                .push((window.to_string(), key.to_string(), None));
            Ok(())
        }

        fn run_notify(
            &self,
            command: &str,
            pane_id: &str,
            agent: &str,
            state: &str,
        ) -> anyhow::Result<()> {
            self.notify_calls.lock().unwrap().push((
                command.to_string(),
                pane_id.to_string(),
                agent.to_string(),
                state.to_string(),
            ));
            Ok(())
        }
    }

    fn test_agent_pane(
        session: &str,
        pane_id: &str,
        status: &str,
    ) -> crate::options::snapshot::PaneSnapshot {
        crate::options::snapshot::PaneSnapshot {
            session: session.to_string(),
            window_id: "@1".to_string(),
            pane_id: pane_id.to_string(),
            current_path: "/tmp/app".to_string(),
            current_command: "codex".to_string(),
            agent: "codex".to_string(),
            status: status.to_string(),
            ..crate::options::snapshot::PaneSnapshot::default()
        }
    }

    fn shared_capture_activity() -> Arc<crate::daemon::workers::SharedCaptureActivity> {
        Arc::new(crate::daemon::workers::SharedCaptureActivity::new())
    }

    #[test]
    fn runtime_effects_write_session_category_metadata() {
        let io = LoopWorkerIo::default();
        handle_runtime_effects(
            vec![
                RuntimeEffect::SetSessionProjectPath {
                    session: "main".to_string(),
                    path: "/tmp/repo".to_string(),
                },
                RuntimeEffect::SetSessionCategory {
                    session: "main".to_string(),
                    category: "work".to_string(),
                },
            ],
            None,
            &io,
            None,
        )
        .unwrap();

        let options = io.session_options.lock().unwrap().clone();
        assert!(options.contains(&(
            "main".to_string(),
            crate::options::KEY_PROJECT_PATH.to_string(),
            Some("/tmp/repo".to_string()),
        )));
        assert!(options.contains(&(
            "main".to_string(),
            crate::options::KEY_CATEGORY.to_string(),
            Some("work".to_string()),
        )));
    }

    #[test]
    fn runtime_loop_answers_query_and_handles_jump_effect() {
        use crate::daemon::protocol::SidebarClientEvent;
        use crate::daemon::runtime::{ClientId, DaemonEvent, RuntimeState};
        use crate::options::snapshot::PaneSnapshot;
        use crate::sidebar::state::SidebarState;
        use std::sync::{Arc, mpsc};

        let io = Arc::new(LoopWorkerIo::default());
        let (tx, rx) = mpsc::channel();
        tx.send(DaemonEvent::PanesUpdated(vec![PaneSnapshot {
            session: "main".to_string(),
            window_id: "@1".to_string(),
            pane_id: "%1".to_string(),
            current_path: "/tmp/app".to_string(),
            current_command: "codex".to_string(),
            agent: "codex".to_string(),
            status: "running".to_string(),
            ..PaneSnapshot::default()
        }]))
        .unwrap();
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(DaemonEvent::QuerySummary { reply: reply_tx })
            .unwrap();
        tx.send(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::JumpPane {
                pane: "%1".to_string(),
            },
        })
        .unwrap();
        drop(tx);

        run_runtime_loop(
            RuntimeState::new(crate::config::Config::default(), SidebarState::default()),
            rx,
            None,
            io.clone(),
            shared_capture_activity(),
        )
        .unwrap();

        assert_eq!(
            reply_rx.recv().unwrap(),
            ServerMessage::Summary {
                text: "#[fg=#4fd08a]●1#[default]".to_string()
            }
        );
        assert_eq!(io.jumps.lock().unwrap().as_slice(), ["%1"]);
    }

    #[test]
    fn runtime_loop_refresh_panes_reads_tmux_and_updates_snapshot() {
        use crate::daemon::runtime::{DaemonEvent, RuntimeState};
        use crate::sidebar::state::SidebarState;
        use std::sync::{Arc, mpsc};

        let io = Arc::new(LoopWorkerIo::default());
        let mut pane = test_agent_pane("main", "%1", "waiting");
        pane.wait_reason = "permission_prompt".to_string();
        io.panes.lock().unwrap().push(pane);
        let (tx, rx) = mpsc::channel();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        tx.send(DaemonEvent::RefreshPanes { reply: refresh_tx })
            .unwrap();
        let (summary_tx, summary_rx) = mpsc::channel();
        tx.send(DaemonEvent::QuerySummary { reply: summary_tx })
            .unwrap();
        drop(tx);

        run_runtime_loop(
            RuntimeState::new(crate::config::Config::default(), SidebarState::default()),
            rx,
            None,
            io,
            shared_capture_activity(),
        )
        .unwrap();

        assert_eq!(refresh_rx.recv().unwrap(), ServerMessage::Ack);
        assert_eq!(
            summary_rx.recv().unwrap(),
            ServerMessage::Summary {
                text: "#[fg=#ff6b6b]▲1#[default]".to_string()
            }
        );
    }

    #[test]
    fn refresh_panes_reuses_shared_capture_activity() {
        use crate::daemon::runtime::RuntimeState;
        use crate::sidebar::state::SidebarState;
        use std::sync::{Arc, mpsc};

        let io = Arc::new(LoopWorkerIo::default());
        let mut pane = test_agent_pane("main", "%1", "running");
        pane.started_at = (crate::sidebar::tree::now_epoch_secs() - 1_000).to_string();
        io.panes.lock().unwrap().push(pane);
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Working (1s)\n".to_string());
        let capture_activity = shared_capture_activity();
        let initial = crate::daemon::workers::read_panes_with_shared_capture_activity(
            io.as_ref(),
            300,
            capture_activity.as_ref(),
        )
        .unwrap();
        assert_eq!(initial[0].status, "idle");
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Working (2s)\n".to_string());

        let mut state =
            RuntimeState::new(crate::config::Config::default(), SidebarState::default());
        let (reply_tx, reply_rx) = mpsc::channel();
        let effects =
            refresh_panes_once(&mut state, io.as_ref(), capture_activity.as_ref(), reply_tx);

        assert_eq!(reply_rx.recv().unwrap(), ServerMessage::Ack);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::SetSessionBadge { session, state, .. }
                if session == "main" && state == "working"
        )));
    }

    #[test]
    fn runtime_loop_keeps_running_when_jump_effect_fails() {
        use crate::daemon::protocol::SidebarClientEvent;
        use crate::daemon::runtime::{ClientId, DaemonEvent, RuntimeState};
        use crate::sidebar::state::SidebarState;
        use std::sync::{Arc, mpsc};

        let io = Arc::new(LoopWorkerIo {
            fail_jump: true,
            ..LoopWorkerIo::default()
        });
        let (tx, rx) = mpsc::channel();
        tx.send(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::JumpPane {
                pane: "%1".to_string(),
            },
        })
        .unwrap();
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(DaemonEvent::QuerySummary { reply: reply_tx })
            .unwrap();
        drop(tx);

        run_runtime_loop(
            RuntimeState::new(crate::config::Config::default(), SidebarState::default()),
            rx,
            None,
            io,
            shared_capture_activity(),
        )
        .unwrap();

        assert_eq!(
            reply_rx.recv().unwrap(),
            ServerMessage::Summary {
                text: String::new()
            }
        );
    }

    #[test]
    fn runtime_loop_flushes_dirty_state_on_shutdown() {
        use crate::daemon::protocol::SidebarClientEvent;
        use crate::daemon::runtime::{ClientId, DaemonEvent, RuntimeState};
        use crate::options::snapshot::PaneSnapshot;
        use crate::sidebar::state::SidebarState;
        use std::sync::{Arc, mpsc};

        let dir = std::env::temp_dir().join(format!(
            "vde-runtime-shutdown-flush-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state_path = dir.join("state.json");
        let io = Arc::new(LoopWorkerIo::default());
        let (tx, rx) = mpsc::channel();
        tx.send(DaemonEvent::PanesUpdated(vec![PaneSnapshot {
            session: "main".to_string(),
            window_id: "@1".to_string(),
            pane_id: "%1".to_string(),
            current_path: "/tmp/app".to_string(),
            current_command: "codex".to_string(),
            agent: "codex".to_string(),
            status: "running".to_string(),
            ..PaneSnapshot::default()
        }]))
        .unwrap();
        tx.send(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "j".to_string(),
            },
        })
        .unwrap();
        tx.send(DaemonEvent::Shutdown).unwrap();

        run_runtime_loop(
            RuntimeState::new(crate::config::Config::default(), SidebarState::default()),
            rx,
            Some(state_path.clone()),
            io,
            shared_capture_activity(),
        )
        .unwrap();

        let saved = crate::sidebar::store::load_state(&state_path).unwrap();
        assert_eq!(saved.selection.as_deref(), Some("repo::misc::app"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runtime_loop_executes_session_badge_effects() {
        use crate::daemon::runtime::{DaemonEvent, RuntimeState};
        use crate::sidebar::state::SidebarState;
        use std::sync::{Arc, mpsc};
        use std::thread;

        let io = Arc::new(LoopWorkerIo::default());
        let (tx, rx) = mpsc::channel();
        let state = RuntimeState::new(crate::config::Config::default(), SidebarState::default());
        let handle = {
            let io = io.clone();
            thread::spawn(move || run_runtime_loop(state, rx, None, io, shared_capture_activity()))
        };
        tx.send(DaemonEvent::PanesUpdated(vec![test_agent_pane(
            "main", "%1", "running",
        )]))
        .unwrap();
        tx.send(DaemonEvent::Shutdown).unwrap();
        handle.join().unwrap().unwrap();

        let calls = io.session_options.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                (
                    "main".to_string(),
                    "@vde_session_status".to_string(),
                    Some("●".to_string()),
                ),
                (
                    "main".to_string(),
                    "@vde_session_state".to_string(),
                    Some("working".to_string()),
                ),
                ("main".to_string(), "@vde_session_status".to_string(), None),
                ("main".to_string(), "@vde_session_state".to_string(), None),
            ]
        );
    }

    #[test]
    fn notify_effect_runs_command_via_worker_io() {
        let io = LoopWorkerIo::default();

        handle_runtime_effects(
            vec![RuntimeEffect::Notify {
                pane_id: "%1".to_string(),
                agent: "codex".to_string(),
                state: crate::daemon::session_badge::BadgeState::Blocked,
            }],
            None,
            &io,
            Some("true"),
        )
        .unwrap();

        let calls = io.notify_calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(
                "true".to_string(),
                "%1".to_string(),
                "codex".to_string(),
                "Blocked".to_string()
            )]
        );
    }

    #[test]
    fn clear_pane_state_effect_unsets_all_pane_state_keys() {
        let io = LoopWorkerIo::default();

        handle_runtime_effects(
            vec![RuntimeEffect::ClearPaneState {
                pane_id: "%1".to_string(),
            }],
            None,
            &io,
            None,
        )
        .unwrap();

        let calls = io.pane_options.lock().unwrap().clone();
        let expected = crate::options::PANE_STATE_KEYS
            .iter()
            .map(|key| ("%1".to_string(), (*key).to_string(), None))
            .collect::<Vec<_>>();
        assert_eq!(calls, expected);
    }

    #[test]
    fn mark_pane_done_effect_sets_idle_and_resets_task_state() {
        let io = LoopWorkerIo::default();

        handle_runtime_effects(
            vec![RuntimeEffect::MarkPaneDone {
                pane_id: "%1".to_string(),
                completed_at: 1234,
            }],
            None,
            &io,
            None,
        )
        .unwrap();

        assert_eq!(
            io.pane_options.lock().unwrap().as_slice(),
            &[
                (
                    "%1".to_string(),
                    crate::options::KEY_STATUS.to_string(),
                    Some("idle".to_string())
                ),
                (
                    "%1".to_string(),
                    crate::options::KEY_ATTENTION.to_string(),
                    Some("1".to_string())
                ),
                (
                    "%1".to_string(),
                    crate::options::KEY_COMPLETED_AT.to_string(),
                    Some("1234".to_string())
                ),
                (
                    "%1".to_string(),
                    crate::options::KEY_WAIT_REASON.to_string(),
                    None
                ),
                (
                    "%1".to_string(),
                    crate::options::KEY_TASKS.to_string(),
                    None
                ),
                (
                    "%1".to_string(),
                    crate::options::KEY_TASK_ITEMS.to_string(),
                    None
                ),
                (
                    "%1".to_string(),
                    crate::options::KEY_TASK_ITEM_IDS.to_string(),
                    None
                ),
                (
                    "%1".to_string(),
                    crate::options::KEY_SUBAGENTS.to_string(),
                    None
                ),
            ]
        );
    }
}

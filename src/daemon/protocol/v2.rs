use std::io;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::session_badge::{BadgeState, BadgeStateCounts};
use crate::daemon::{SidebarFrame, TransitionEvent};
use crate::pane_state::{
    DaemonInstanceId, EventId, MAX_REQUEST_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES,
    PaneEventEnvelope, PaneInstance, PaneStateLoadError, ResolvedPaneState, StateVersion,
    StoredStateDescriptor, ViewEvent,
};

pub const PROTOCOL_VERSION: u16 = 2;
pub const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

pub struct V2Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    daemon_instance_id: DaemonInstanceId,
    server_identity: String,
    phase: DaemonPhase,
    hook_health: HookHealth,
    deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V2RequestFailureStage {
    BeforeFullWrite,
    AfterFullWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2RequestError {
    pub stage: V2RequestFailureStage,
    pub message: String,
}

impl std::fmt::Display for V2RequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for V2RequestError {}

impl V2Client {
    pub fn connect(socket: &Path, expected_server_identity: &str) -> Result<Self> {
        Self::connect_with_timeout(socket, expected_server_identity, CLIENT_REQUEST_TIMEOUT)
    }

    pub fn connect_with_timeout(
        socket: &Path,
        expected_server_identity: &str,
        timeout: Duration,
    ) -> Result<Self> {
        Self::connect_with_deadline(socket, expected_server_identity, Instant::now() + timeout)
    }

    pub fn connect_with_deadline(
        socket: &Path,
        expected_server_identity: &str,
        deadline: Instant,
    ) -> Result<Self> {
        let mut writer = connect_unix_with_deadline(socket, deadline)
            .with_context(|| format!("failed to connect to daemon socket {}", socket.display()))?;
        let reader_stream = writer.try_clone()?;
        let mut reader = BufReader::new(reader_stream);
        write_client_message(
            &mut writer,
            &ClientMessage::Hello {
                proto: PROTOCOL_VERSION,
            },
            deadline,
        )?;
        let response = read_server_message(&mut reader, deadline)?;
        let ServerMessage::HelloAck {
            proto,
            daemon_instance_id,
            server_identity,
            phase,
            hook_health,
        } = response
        else {
            return server_response_error("HelloAck", response);
        };
        if proto != PROTOCOL_VERSION {
            bail!("daemon returned unsupported protocol version {proto}");
        }
        if server_identity != expected_server_identity {
            bail!(
                "daemon server identity mismatch: expected {expected_server_identity}, received {server_identity}"
            );
        }
        Ok(Self {
            writer,
            reader,
            daemon_instance_id,
            server_identity,
            phase,
            hook_health,
            deadline,
        })
    }

    pub fn daemon_instance_id(&self) -> &DaemonInstanceId {
        &self.daemon_instance_id
    }

    pub fn server_identity(&self) -> &str {
        &self.server_identity
    }

    pub fn phase(&self) -> DaemonPhase {
        self.phase
    }

    pub fn hook_health(&self) -> HookHealth {
        self.hook_health
    }

    pub fn request(&mut self, message: &ClientMessage) -> Result<ServerMessage> {
        self.request_with_stage(message).map_err(anyhow::Error::new)
    }

    pub fn request_with_stage(
        &mut self,
        message: &ClientMessage,
    ) -> std::result::Result<ServerMessage, V2RequestError> {
        self.validate_request(message)
            .map_err(|error| V2RequestError {
                stage: V2RequestFailureStage::BeforeFullWrite,
                message: error.to_string(),
            })?;
        write_client_message(&mut self.writer, message, self.deadline).map_err(|error| {
            V2RequestError {
                stage: V2RequestFailureStage::BeforeFullWrite,
                message: error.to_string(),
            }
        })?;
        read_server_message(&mut self.reader, self.deadline).map_err(|error| V2RequestError {
            stage: V2RequestFailureStage::AfterFullWrite,
            message: error.to_string(),
        })
    }

    fn validate_request(&self, message: &ClientMessage) -> Result<()> {
        if message.proto() != PROTOCOL_VERSION {
            bail!("client request must use protocol version {PROTOCOL_VERSION}");
        }
        if matches!(message, ClientMessage::Hello { .. }) {
            bail!("Hello is only valid as the first connection frame");
        }
        if message
            .mutation_instance_id()
            .is_some_and(|instance_id| instance_id != &self.daemon_instance_id)
        {
            bail!("client request uses a stale daemon instance ID");
        }
        Ok(())
    }
}

fn connect_unix_with_deadline(socket: &Path, deadline: Instant) -> io::Result<UnixStream> {
    if deadline.checked_duration_since(Instant::now()).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "daemon connect deadline exceeded",
        ));
    }
    let path = socket.as_os_str().as_bytes();
    if path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon socket path contains NUL",
        ));
    }

    // SAFETY: socket has no pointer arguments and returns a new owned descriptor on success.
    let raw_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw_fd was just returned by socket and ownership is transferred exactly once.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    // SAFETY: fcntl marks a valid owned descriptor close-on-exec.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl reads flags for a valid owned descriptor.
    let original_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if original_flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl updates flags for a valid owned descriptor.
    if unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_SETFL,
            original_flags | libc::O_NONBLOCK,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: zero is a valid initial representation for sockaddr_un before fields are filled.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon socket path is too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    #[cfg(any(
        target_os = "aix",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "haiku",
        target_os = "hurd",
        target_os = "ios",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos"
    ))]
    {
        address.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    }
    // SAFETY: sun_path has been capacity-checked, both regions are valid, and they do not overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            path.as_ptr(),
            address.sun_path.as_mut_ptr().cast::<u8>(),
            path.len(),
        );
    }
    // SAFETY: address points to an initialized sockaddr_un with a valid length.
    let connect_result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if connect_result < 0 {
        let error = io::Error::last_os_error();
        let code = error.raw_os_error().unwrap_or_default();
        if code != libc::EINPROGRESS && code != libc::EAGAIN && code != libc::EWOULDBLOCK {
            return Err(error);
        }
        wait_for_unix_connect(&fd, deadline)?;
    }

    // SAFETY: fcntl restores the original blocking flags on a valid descriptor.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, original_flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ownership moves from OwnedFd into UnixStream exactly once.
    Ok(unsafe { UnixStream::from_raw_fd(fd.into_raw_fd()) })
}

fn wait_for_unix_connect(fd: &OwnedFd, deadline: Instant) -> io::Result<()> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon connect deadline exceeded",
            ));
        };
        let timeout_ms = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: poll_fd points to one initialized pollfd for the duration of the call.
        let result = unsafe { libc::poll(&raw mut poll_fd, 1, timeout_ms) };
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon connect deadline exceeded",
            ));
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        let mut socket_error: libc::c_int = 0;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: socket_error and length are valid output buffers for SO_ERROR.
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut socket_error).cast(),
                &raw mut length,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        return if socket_error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(socket_error))
        };
    }
}

fn write_client_message(
    writer: &mut UnixStream,
    message: &ClientMessage,
    deadline: Instant,
) -> Result<()> {
    let mut frame = serde_json::to_vec(message)?;
    if frame.len() > MAX_REQUEST_FRAME_BYTES {
        bail!("request frame exceeds 1 MiB");
    }
    frame.push(b'\n');
    let mut written = 0;
    while written < frame.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("daemon request write deadline exceeded");
        }
        writer.set_write_timeout(Some(remaining))?;
        match writer.write(&frame[written..]) {
            Ok(0) => bail!("daemon closed the connection while reading the request"),
            Ok(count) => written += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                bail!("daemon request write deadline exceeded")
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn read_server_message(
    reader: &mut BufReader<UnixStream>,
    deadline: Instant,
) -> Result<ServerMessage> {
    let mut frame = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("daemon response read deadline exceeded");
        }
        reader.get_ref().set_read_timeout(Some(remaining))?;
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                bail!("daemon response read deadline exceeded")
            }
            Err(error) => return Err(error.into()),
        };
        if chunk.is_empty() {
            bail!("daemon closed the connection before responding");
        }
        let consumed = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |position| position + 1);
        frame.extend_from_slice(&chunk[..consumed]);
        reader.consume(consumed);
        if frame.len() > MAX_RESPONSE_FRAME_BYTES.saturating_add(1) {
            bail!("daemon response exceeds 16 MiB");
        }
        if frame.last() == Some(&b'\n') {
            break;
        }
    }
    Ok(serde_json::from_slice(&frame)?)
}

fn server_response_error<T>(expected: &str, response: ServerMessage) -> Result<T> {
    if let ServerMessage::Error { code, message, .. } = response {
        bail!("daemon returned {code:?}: {message}");
    }
    bail!("expected {expected}, received {response:?}")
}

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
    if frame.len() > MAX_RESPONSE_FRAME_BYTES {
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
    fn unix_connect_rejects_an_expired_deadline_before_opening_socket() {
        let error = connect_unix_with_deadline(
            Path::new("/tmp/vde-tmux-never-connect.sock"),
            Instant::now() - Duration::from_millis(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
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

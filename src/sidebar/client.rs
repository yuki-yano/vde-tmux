use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::daemon::protocol::v2::{
    ClientMessage as V2ClientMessage, PROTOCOL_VERSION, ResolvedSnapshot,
    ServerMessage as V2ServerMessage, SidebarCommand as V2SidebarCommand, V2Client,
};
use crate::pane_state::{
    EventId, MAX_REQUEST_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES, PaneInstance, StateVersion,
};

const V2_SIDEBAR_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const V2_SUBSCRIBE_INITIAL_TIMEOUT: Duration = Duration::from_millis(500);

/// One update from the daemon live preview stream. Delivered per fixed target;
/// the run loop drops updates whose target no longer matches its selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSubscriptionUpdate {
    Body {
        target: PaneInstance,
        live_revision: u64,
        body: String,
    },
    Unavailable {
        target: PaneInstance,
    },
}

/// Owner handle of one fixed-target live subscription connection. Changing the
/// target requires shutting this handle down and spawning a new subscription.
pub struct LiveSubscriptionHandle {
    target: PaneInstance,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stream: std::sync::Arc<std::sync::Mutex<Option<UnixStream>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl LiveSubscriptionHandle {
    pub fn target(&self) -> &PaneInstance {
        &self.target
    }

    /// Shuts the subscription socket down to release the blocking read and
    /// waits for the worker thread to exit, so a replacement subscription can
    /// never race a stale delivery from this one.
    pub fn shutdown_and_join(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(stream) = self
            .stream
            .lock()
            .expect("live subscription stream lock poisoned")
            .take()
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for LiveSubscriptionHandle {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(stream) = self
            .stream
            .lock()
            .expect("live subscription stream lock poisoned")
            .take()
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

pub fn spawn_live_subscription(
    socket: std::path::PathBuf,
    server_identity: String,
    source_pane: PaneInstance,
    target_pane: PaneInstance,
    interval_ms: u64,
    tx: Sender<LiveSubscriptionUpdate>,
) -> LiveSubscriptionHandle {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stream_slot = std::sync::Arc::new(std::sync::Mutex::new(None::<UnixStream>));
    let worker = {
        let stop = stop.clone();
        let stream_slot = stream_slot.clone();
        let target = target_pane.clone();
        thread::spawn(move || {
            let mut backoff = Duration::from_millis(200);
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let result = run_live_subscription_once(
                    &socket,
                    &server_identity,
                    &source_pane,
                    &target,
                    interval_ms,
                    &tx,
                    &stop,
                    &stream_slot,
                );
                stream_slot
                    .lock()
                    .expect("live subscription stream lock poisoned")
                    .take();
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                if tx
                    .send(LiveSubscriptionUpdate::Unavailable {
                        target: target.clone(),
                    })
                    .is_err()
                {
                    return;
                }
                let _ = result;
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        })
    };
    LiveSubscriptionHandle {
        target: target_pane,
        stop,
        stream: stream_slot,
        worker: Some(worker),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_live_subscription_once(
    socket: &Path,
    server_identity: &str,
    source_pane: &PaneInstance,
    target_pane: &PaneInstance,
    interval_ms: u64,
    tx: &Sender<LiveSubscriptionUpdate>,
    stop: &std::sync::atomic::AtomicBool,
    stream_slot: &std::sync::Mutex<Option<UnixStream>>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(V2_SUBSCRIBE_INITIAL_TIMEOUT))?;
    *stream_slot
        .lock()
        .expect("live subscription stream lock poisoned") = Some(stream.try_clone()?);
    if stop.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    let deadline = Instant::now() + V2_SUBSCRIBE_INITIAL_TIMEOUT;
    write_v2_client_frame(
        &mut stream,
        &V2ClientMessage::Hello {
            proto: PROTOCOL_VERSION,
        },
        deadline,
    )?;
    let mut reader = BufReader::new(stream);
    let hello = read_v2_server_frame(&mut reader, Some(deadline))?
        .ok_or_else(|| anyhow::anyhow!("daemon closed the connection before HelloAck"))?;
    let V2ServerMessage::HelloAck {
        proto,
        daemon_instance_id,
        server_identity: actual_server_identity,
        ..
    } = hello
    else {
        return v2_server_error("HelloAck", hello);
    };
    if proto != PROTOCOL_VERSION {
        bail!("daemon returned unsupported protocol version {proto}");
    }
    if actual_server_identity != server_identity {
        bail!(
            "daemon server identity mismatch: expected {server_identity}, received {actual_server_identity}"
        );
    }
    write_v2_client_frame(
        reader.get_mut(),
        &V2ClientMessage::SubscribeLive {
            proto: PROTOCOL_VERSION,
            source_pane: source_pane.clone(),
            target_pane: target_pane.clone(),
            interval_ms,
        },
        deadline,
    )?;
    loop {
        let Some(message) = read_v2_server_frame(&mut reader, None)? else {
            return Ok(());
        };
        match message {
            V2ServerMessage::Heartbeat {
                daemon_instance_id: heartbeat_instance,
                ..
            } => {
                if heartbeat_instance != daemon_instance_id {
                    bail!("daemon instance changed during live subscription");
                }
            }
            V2ServerMessage::LivePreviewResult {
                live_revision,
                target_pane,
                body,
                ..
            } => {
                if tx
                    .send(LiveSubscriptionUpdate::Body {
                        target: target_pane,
                        live_revision,
                        body,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            V2ServerMessage::LivePreviewUnavailable { target_pane, .. } => {
                if tx
                    .send(LiveSubscriptionUpdate::Unavailable {
                        target: target_pane,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            V2ServerMessage::Error { code, message, .. } => {
                bail!("{code:?}: {message}")
            }
            other => bail!("unexpected daemon live subscription response: {other:?}"),
        }
    }
}

pub fn send_sidebar_jump_v2(
    socket: &Path,
    server_identity: &str,
    pane_instance: PaneInstance,
    source_pane: PaneInstance,
) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::JumpPane {
            pane_instance,
            source_pane,
        },
        V2SidebarResponse::SnapshotAck,
    )?;
    Ok(())
}

pub fn send_sidebar_mark_complete_v2(
    socket: &Path,
    server_identity: &str,
    pane_instance: PaneInstance,
    expected: StateVersion,
) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::MarkComplete {
            pane_instance,
            expected,
        },
        V2SidebarResponse::PaneEventResult,
    )?;
    Ok(())
}

pub fn send_sidebar_update_manual_order_v2(
    socket: &Path,
    server_identity: &str,
    expected_version: u64,
    manual_order: Vec<crate::sidebar::state::RepoId>,
    manual_chat_order: Vec<String>,
) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::UpdateManualOrder {
            expected_version,
            manual_order,
            manual_chat_order,
        },
        V2SidebarResponse::SnapshotAck,
    )?;
    Ok(())
}

pub fn send_sidebar_update_view_preferences_v2(
    socket: &Path,
    server_identity: &str,
    expected_version: u64,
    view_mode: crate::sidebar::state::ViewMode,
    filter: crate::sidebar::state::StatusFilter,
) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::UpdateViewPreferences {
            expected_version,
            view_mode,
            filter,
        },
        V2SidebarResponse::SnapshotAck,
    )?;
    Ok(())
}

pub fn send_sidebar_set_expansion_override_v2(
    socket: &Path,
    server_identity: &str,
    expected_version: u64,
    row_id: String,
    overridden: bool,
) -> Result<u64> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::SetExpansionOverride {
            expected_version,
            row_id,
            overridden,
        },
        V2SidebarResponse::SnapshotAck,
    )
}

pub fn request_topology_refresh_v2(socket: &Path, server_identity: &str) -> Result<()> {
    let mut client = V2Client::connect(socket, server_identity)?;
    let event_id = EventId::generate()?;
    let response = client.request(&V2ClientMessage::RefreshTopology {
        proto: PROTOCOL_VERSION,
        daemon_instance_id: client.daemon_instance_id().clone(),
        event_id: event_id.clone(),
    })?;
    expect_v2_mutation_response(response, &event_id, V2SidebarResponse::SnapshotAck)?;
    Ok(())
}

pub fn query_resolved_snapshot_v2(
    socket: &Path,
    server_identity: &str,
) -> Result<ResolvedSnapshot> {
    let mut client = V2Client::connect(socket, server_identity)?;
    let response = client.request(&V2ClientMessage::QueryResolvedSnapshot {
        proto: PROTOCOL_VERSION,
    })?;
    match response {
        V2ServerMessage::ResolvedSnapshotResult {
            snapshot_revision,
            snapshot,
        } if snapshot.snapshot_revision == snapshot_revision => Ok(snapshot),
        V2ServerMessage::ResolvedSnapshotResult {
            snapshot_revision,
            snapshot,
        } => bail!(
            "daemon snapshot revision mismatch: envelope={snapshot_revision}, snapshot={}",
            snapshot.snapshot_revision
        ),
        V2ServerMessage::Error { code, message, .. } => bail!("{code:?}: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

#[derive(Debug)]
pub enum SubscriptionUpdate {
    Connecting,
    Connected(Box<ResolvedSnapshot>),
    Degraded(String),
    Disconnected,
}

pub fn subscribe_v2(
    socket: &Path,
    server_identity: &str,
    expected_config_hash: &str,
    tx: Sender<SubscriptionUpdate>,
) -> Result<()> {
    let socket = socket.to_path_buf();
    let server_identity = server_identity.to_string();
    let expected_config_hash = expected_config_hash.to_string();
    thread::spawn(move || {
        let mut backoff = Duration::from_millis(100);
        loop {
            if tx.send(SubscriptionUpdate::Connecting).is_err() {
                return;
            }
            let mut subscription = match V2SnapshotSubscription::connect(
                &socket,
                &server_identity,
                &expected_config_hash,
            )
            .and_then(|mut subscription| {
                let first = subscription.read_initial_snapshot()?.ok_or_else(|| {
                    anyhow::anyhow!("daemon closed the subscription before the initial snapshot")
                })?;
                Ok((subscription, first))
            }) {
                Ok((subscription, first)) => {
                    if tx
                        .send(SubscriptionUpdate::Connected(Box::new(first)))
                        .is_err()
                    {
                        return;
                    }
                    if let Some(message) = subscription.initial_degraded.clone()
                        && tx.send(SubscriptionUpdate::Degraded(message)).is_err()
                    {
                        return;
                    }
                    subscription
                }
                Err(error) => {
                    if tx
                        .send(SubscriptionUpdate::Degraded(error.to_string()))
                        .is_err()
                    {
                        return;
                    }
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                    continue;
                }
            };
            backoff = Duration::from_millis(100);
            loop {
                match subscription.read_next_snapshot() {
                    Ok(Some(snapshot)) => {
                        if tx
                            .send(SubscriptionUpdate::Connected(Box::new(snapshot)))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(None) => {
                        if tx.send(SubscriptionUpdate::Disconnected).is_err() {
                            return;
                        }
                        break;
                    }
                    Err(error) => {
                        if tx
                            .send(SubscriptionUpdate::Degraded(error.to_string()))
                            .is_err()
                        {
                            return;
                        }
                        break;
                    }
                }
            }
            thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    });
    Ok(())
}

fn request_v2_sidebar(
    socket: &Path,
    server_identity: &str,
    command: V2SidebarCommand,
    expected_response: V2SidebarResponse,
) -> Result<u64> {
    let deadline = Instant::now() + V2_SIDEBAR_COMMAND_TIMEOUT;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(V2_SIDEBAR_COMMAND_TIMEOUT))?;
    write_v2_client_frame(
        &mut stream,
        &V2ClientMessage::Hello {
            proto: PROTOCOL_VERSION,
        },
        deadline,
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let hello = read_v2_server_frame(&mut reader, Some(deadline))?
        .ok_or_else(|| anyhow::anyhow!("daemon closed the connection before HelloAck"))?;
    let V2ServerMessage::HelloAck {
        proto,
        daemon_instance_id,
        server_identity: actual_server_identity,
        ..
    } = hello
    else {
        return v2_server_error("HelloAck", hello);
    };
    if proto != PROTOCOL_VERSION {
        bail!("daemon returned unsupported protocol version {proto}");
    }
    if actual_server_identity != server_identity {
        bail!(
            "daemon server identity mismatch: expected {server_identity}, received {actual_server_identity}"
        );
    }
    let event_id = EventId::generate()?;
    write_v2_client_frame(
        &mut stream,
        &V2ClientMessage::SidebarCommand {
            proto: PROTOCOL_VERSION,
            daemon_instance_id,
            event_id: event_id.clone(),
            command,
        },
        deadline,
    )?;
    let response = read_v2_server_frame(&mut reader, Some(deadline))?
        .ok_or_else(|| anyhow::anyhow!("daemon closed the connection before responding"))?;
    expect_v2_mutation_response(response, &event_id, expected_response)
}

fn expect_v2_mutation_response(
    response: V2ServerMessage,
    event_id: &EventId,
    expected_response: V2SidebarResponse,
) -> Result<u64> {
    match (expected_response, response) {
        (
            V2SidebarResponse::PaneEventResult,
            V2ServerMessage::PaneEventResult {
                event_id: response_event_id,
                snapshot_revision,
                ..
            },
        ) if response_event_id == *event_id => Ok(snapshot_revision),
        (
            V2SidebarResponse::SnapshotAck,
            V2ServerMessage::SnapshotAck {
                event_id: response_event_id,
                snapshot_revision,
                ..
            },
        ) if response_event_id == *event_id => Ok(snapshot_revision),
        (
            V2SidebarResponse::PaneEventResult,
            V2ServerMessage::PaneEventResult {
                event_id: response_event_id,
                ..
            },
        )
        | (
            V2SidebarResponse::SnapshotAck,
            V2ServerMessage::SnapshotAck {
                event_id: response_event_id,
                ..
            },
        ) => bail!(
            "daemon response event ID mismatch: expected {event_id:?}, received {response_event_id:?}"
        ),
        (
            _,
            V2ServerMessage::Error {
                event_id: Some(response_event_id),
                ..
            },
        ) if response_event_id != *event_id => bail!(
            "daemon response event ID mismatch: expected {event_id:?}, received {response_event_id:?}"
        ),
        (_, V2ServerMessage::Error { code, message, .. }) => bail!("{code:?}: {message}"),
        (_, other) => bail!("unexpected daemon response: {other:?}"),
    }
}

#[derive(Debug, Clone, Copy)]
enum V2SidebarResponse {
    SnapshotAck,
    PaneEventResult,
}

struct V2SnapshotSubscription {
    reader: BufReader<UnixStream>,
    daemon_instance_id: crate::pane_state::DaemonInstanceId,
    last_revision: Option<u64>,
    initial_deadline: Instant,
    initial_degraded: Option<String>,
}

impl V2SnapshotSubscription {
    fn connect(socket: &Path, server_identity: &str, expected_config_hash: &str) -> Result<Self> {
        verify_active_config_hash(socket, server_identity, expected_config_hash)?;
        let mut stream = UnixStream::connect(socket)?;
        stream.set_write_timeout(Some(V2_SUBSCRIBE_INITIAL_TIMEOUT))?;
        let deadline = Instant::now() + V2_SUBSCRIBE_INITIAL_TIMEOUT;
        write_v2_client_frame(
            &mut stream,
            &V2ClientMessage::Hello {
                proto: PROTOCOL_VERSION,
            },
            deadline,
        )?;
        let mut reader = BufReader::new(stream);
        let hello = read_v2_server_frame(&mut reader, Some(deadline))?
            .ok_or_else(|| anyhow::anyhow!("daemon closed the connection before HelloAck"))?;
        let V2ServerMessage::HelloAck {
            proto,
            daemon_instance_id,
            server_identity: actual_server_identity,
            phase,
            hook_health,
        } = hello
        else {
            return v2_server_error("HelloAck", hello);
        };
        if proto != PROTOCOL_VERSION {
            bail!("daemon returned unsupported protocol version {proto}");
        }
        if actual_server_identity != server_identity {
            bail!(
                "daemon server identity mismatch: expected {server_identity}, received {actual_server_identity}"
            );
        }
        write_v2_client_frame(
            reader.get_mut(),
            &V2ClientMessage::Subscribe {
                proto: PROTOCOL_VERSION,
            },
            deadline,
        )?;
        Ok(Self {
            reader,
            daemon_instance_id,
            last_revision: None,
            initial_deadline: deadline,
            initial_degraded: (phase != crate::daemon::protocol::v2::DaemonPhase::Serving
                || hook_health != crate::daemon::protocol::v2::HookHealth::Healthy)
                .then(|| format!("daemon health is {phase:?}/{hook_health:?}")),
        })
    }

    fn read_next_snapshot(&mut self) -> Result<Option<ResolvedSnapshot>> {
        self.read_next_snapshot_until(None)
    }

    fn read_initial_snapshot(&mut self) -> Result<Option<ResolvedSnapshot>> {
        self.read_next_snapshot_until(Some(self.initial_deadline))
    }

    fn read_next_snapshot_until(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<Option<ResolvedSnapshot>> {
        loop {
            let message = read_v2_server_frame(&mut self.reader, deadline)?;
            let Some(message) = message else {
                return Ok(None);
            };
            match message {
                V2ServerMessage::ResolvedSnapshotResult {
                    snapshot_revision,
                    snapshot,
                } => {
                    if snapshot.snapshot_revision != snapshot_revision {
                        bail!(
                            "daemon snapshot revision mismatch: envelope={snapshot_revision}, snapshot={}",
                            snapshot.snapshot_revision
                        );
                    }
                    if self
                        .last_revision
                        .is_some_and(|last_revision| snapshot_revision <= last_revision)
                    {
                        continue;
                    }
                    self.last_revision = Some(snapshot_revision);
                    return Ok(Some(snapshot));
                }
                V2ServerMessage::Heartbeat {
                    daemon_instance_id,
                    snapshot_revision,
                } => {
                    // A heartbeat only proves the connection is alive; it never
                    // adopts a snapshot or triggers a redraw. Identity and
                    // revision regressions are protocol errors that reconnect.
                    if daemon_instance_id != self.daemon_instance_id {
                        bail!(
                            "daemon instance changed during subscription: expected {}, received {}",
                            self.daemon_instance_id.as_str(),
                            daemon_instance_id.as_str()
                        );
                    }
                    if let Some(last_revision) = self.last_revision
                        && snapshot_revision < last_revision
                    {
                        bail!(
                            "daemon snapshot revision regressed in heartbeat: last={last_revision}, received={snapshot_revision}"
                        );
                    }
                    continue;
                }
                V2ServerMessage::Error { code, message, .. } => {
                    bail!("{code:?}: {message}")
                }
                other => bail!("unexpected daemon subscription response: {other:?}"),
            }
        }
    }
}

fn verify_active_config_hash(
    socket: &Path,
    server_identity: &str,
    expected_config_hash: &str,
) -> Result<()> {
    let mut client =
        V2Client::connect_with_timeout(socket, server_identity, V2_SUBSCRIBE_INITIAL_TIMEOUT)?;
    let response = client.request(&V2ClientMessage::QueryHealth {
        proto: PROTOCOL_VERSION,
    })?;
    let V2ServerMessage::HealthResult { health } = response else {
        return v2_server_error("HealthResult", response);
    };
    if health.config_hash.trim().is_empty() || health.config_hash != expected_config_hash {
        bail!("sidebar config does not match the daemon active config; run `vt daemon reload`");
    }
    Ok(())
}

fn write_v2_client_frame(
    stream: &mut UnixStream,
    message: &V2ClientMessage,
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
        stream.set_write_timeout(Some(remaining.max(Duration::from_millis(1))))?;
        match stream.write(&frame[written..]) {
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

fn read_v2_server_frame(
    reader: &mut BufReader<UnixStream>,
    deadline: Option<Instant>,
) -> Result<Option<V2ServerMessage>> {
    let mut frame = Vec::new();
    loop {
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("daemon response read deadline exceeded");
            }
            crate::daemon::protocol::v2::wait_for_unix_readable(reader.get_ref(), deadline)?;
        }
        let available = match reader.fill_buf() {
            Ok(available) => available,
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
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            bail!("daemon closed the connection before completing a response frame");
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if frame.len().saturating_add(take) > MAX_RESPONSE_FRAME_BYTES.saturating_add(1) {
            bail!("daemon response frame exceeds 16 MiB");
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            frame.pop();
            return serde_json::from_slice(&frame).map(Some).map_err(Into::into);
        }
    }
}

fn v2_server_error<T>(expected: &str, response: V2ServerMessage) -> Result<T> {
    match response {
        V2ServerMessage::Error { code, message, .. } => bail!("{code:?}: {message}"),
        other => bail!("expected {expected}, received {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const DAEMON_INSTANCE_ID: &str = "ffeeddccbbaa99887766554433221100";
    fn daemon_instance_id() -> crate::pane_state::DaemonInstanceId {
        crate::pane_state::DaemonInstanceId::parse(DAEMON_INSTANCE_ID).unwrap()
    }

    fn write_hello_ack(stream: &mut UnixStream, server_identity: &str) {
        serde_json::to_writer(
            &mut *stream,
            &V2ServerMessage::HelloAck {
                proto: PROTOCOL_VERSION,
                daemon_instance_id: daemon_instance_id(),
                server_identity: server_identity.to_string(),
                phase: crate::daemon::protocol::v2::DaemonPhase::Serving,
                hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
    }

    fn subscription_over(stream: UnixStream, last_revision: Option<u64>) -> V2SnapshotSubscription {
        V2SnapshotSubscription {
            reader: BufReader::new(stream),
            daemon_instance_id: daemon_instance_id(),
            last_revision,
            initial_deadline: Instant::now() + Duration::from_secs(1),
            initial_degraded: None,
        }
    }

    fn empty_snapshot(revision: u64) -> ResolvedSnapshot {
        ResolvedSnapshot {
            snapshot_revision: revision,
            panes: Vec::new(),
            sidebar_model: crate::daemon::SidebarModel::default(),
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn write_frame(stream: &mut UnixStream, message: &V2ServerMessage) {
        serde_json::to_writer(&mut *stream, message).unwrap();
        stream.write_all(b"\n").unwrap();
    }

    #[test]
    fn subscription_ignores_heartbeats_without_producing_snapshots() {
        let (mut server, client) = UnixStream::pair().unwrap();
        let mut subscription = subscription_over(client, Some(5));

        write_frame(
            &mut server,
            &V2ServerMessage::Heartbeat {
                daemon_instance_id: daemon_instance_id(),
                snapshot_revision: 5,
            },
        );
        write_frame(
            &mut server,
            &V2ServerMessage::Heartbeat {
                daemon_instance_id: daemon_instance_id(),
                snapshot_revision: 5,
            },
        );
        write_frame(
            &mut server,
            &V2ServerMessage::ResolvedSnapshotResult {
                snapshot_revision: 6,
                snapshot: empty_snapshot(6),
            },
        );

        // Both heartbeats are consumed silently; only the real revision comes
        // out of the subscription, so the sidebar never redraws for keepalives.
        let snapshot = subscription.read_next_snapshot().unwrap().unwrap();
        assert_eq!(snapshot.snapshot_revision, 6);
    }

    #[test]
    fn subscription_rejects_heartbeat_instance_mismatch_and_revision_regression() {
        let (mut server, client) = UnixStream::pair().unwrap();
        let mut subscription = subscription_over(client, Some(5));
        write_frame(
            &mut server,
            &V2ServerMessage::Heartbeat {
                daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                    "00112233445566778899aabbccddeeff",
                )
                .unwrap(),
                snapshot_revision: 6,
            },
        );
        let error = subscription.read_next_snapshot().unwrap_err();
        assert!(
            error.to_string().contains("daemon instance changed"),
            "{error}"
        );

        let (mut server, client) = UnixStream::pair().unwrap();
        let mut subscription = subscription_over(client, Some(5));
        write_frame(
            &mut server,
            &V2ServerMessage::Heartbeat {
                daemon_instance_id: daemon_instance_id(),
                snapshot_revision: 3,
            },
        );
        let error = subscription.read_next_snapshot().unwrap_err();
        assert!(error.to_string().contains("regressed"), "{error}");
    }

    #[test]
    fn live_subscription_forwards_stream_and_shutdown_releases_blocking_read() {
        let socket = unique_socket_path("live-subscription");
        let listener = UnixListener::bind(&socket).unwrap();
        let source = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        };
        let expected_target = target.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(matches!(
                serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
                V2ClientMessage::Hello { .. }
            ));
            write_hello_ack(&mut stream, "live-server");
            line.clear();
            reader.read_line(&mut line).unwrap();
            let subscribe = serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap();
            let V2ClientMessage::SubscribeLive {
                target_pane,
                interval_ms,
                ..
            } = subscribe
            else {
                panic!("expected SubscribeLive, found {subscribe:?}");
            };
            assert_eq!(target_pane, expected_target);
            assert_eq!(interval_ms, 2000);
            // A keepalive during a quiet capture period is consumed silently.
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::Heartbeat {
                    daemon_instance_id: daemon_instance_id(),
                    snapshot_revision: 1,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::LivePreviewResult {
                    live_revision: 1,
                    target_pane: target_pane.clone(),
                    captured_at_epoch_millis: 42,
                    body: "\u{1b}[32mok\u{1b}[0m\n".to_string(),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::LivePreviewUnavailable {
                    target_pane,
                    reason: crate::daemon::protocol::v2::LiveUnavailableReason::TargetMissing,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            // Keep the connection open so the client blocks in read until it
            // shuts the socket down from the run loop side.
            let mut buffer = [0u8; 1];
            use std::io::Read as _;
            let _ = stream.read(&mut buffer);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = spawn_live_subscription(
            socket.clone(),
            "live-server".to_string(),
            source,
            target.clone(),
            2000,
            tx,
        );

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            LiveSubscriptionUpdate::Body {
                target: target.clone(),
                live_revision: 1,
                body: "\u{1b}[32mok\u{1b}[0m\n".to_string(),
            }
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            LiveSubscriptionUpdate::Unavailable {
                target: target.clone(),
            }
        );

        handle.shutdown_and_join();
        server.join().unwrap();
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    fn accept_config_guard(listener: &UnixListener, server_identity: &str, config_hash: &str) {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
            V2ClientMessage::Hello { .. }
        ));
        write_hello_ack(&mut stream, server_identity);
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
            V2ClientMessage::QueryHealth { .. }
        ));
        serde_json::to_writer(
            &mut stream,
            &V2ServerMessage::HealthResult {
                health: crate::daemon::protocol::v2::DaemonHealth {
                    config_hash: config_hash.to_string(),
                    projection_revision: 1,
                    projection_updated_at_epoch_seconds: 2,
                    notification_enabled: true,
                    notification_failures: 0,
                    notification_queue_drops: 0,
                    notification_degraded: false,
                    last_notification_error_code: None,
                    current_quarantine_count: 0,
                    quarantine_observed_total: 0,
                    recent_error_code: None,
                    hook_delivery_failures: 0,
                    hook_delivery_degraded: false,
                    last_hook_error_code: None,
                    status_push_failures: 0,
                    status_push_degraded: false,
                    last_status_push_error: None,
                    last_status_push_error_at_epoch_seconds: None,
                },
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
    }

    fn accept_subscription(listener: &UnixListener, server_identity: &str) -> UnixStream {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
            V2ClientMessage::Hello { .. }
        ));
        write_hello_ack(&mut stream, server_identity);
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
            V2ClientMessage::Subscribe { .. }
        ));
        stream
    }

    fn accept_guarded_subscription(
        listener: &UnixListener,
        server_identity: &str,
        config_hash: &str,
    ) -> UnixStream {
        accept_config_guard(listener, server_identity, config_hash);
        accept_subscription(listener, server_identity)
    }

    fn empty_resolved_snapshot(revision: u64) -> ResolvedSnapshot {
        ResolvedSnapshot {
            snapshot_revision: revision,
            panes: Vec::new(),
            sidebar_model: crate::daemon::SidebarModel::default(),
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn recv_connected(rx: &std::sync::mpsc::Receiver<SubscriptionUpdate>) -> ResolvedSnapshot {
        loop {
            if let SubscriptionUpdate::Connected(snapshot) =
                rx.recv_timeout(Duration::from_secs(1)).unwrap()
            {
                return *snapshot;
            }
        }
    }

    #[test]
    fn v2_mark_complete_handshakes_and_sends_full_state_version() {
        let socket = unique_socket_path("vde-tmux-v2-mark-done");
        let listener = UnixListener::bind(&socket).unwrap();
        let daemon_instance_id =
            crate::pane_state::DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap();
        let expected = StateVersion {
            state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            agent_epoch: 3,
            revision: 9,
        };
        let expected_for_server = expected.clone();
        let daemon_for_server = daemon_instance_id.clone();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
                V2ClientMessage::Hello {
                    proto: PROTOCOL_VERSION
                }
            );
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::HelloAck {
                    proto: PROTOCOL_VERSION,
                    daemon_instance_id: daemon_for_server.clone(),
                    server_identity: "scratch".to_string(),
                    phase: crate::daemon::protocol::v2::DaemonPhase::Serving,
                    hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();

            line.clear();
            reader.read_line(&mut line).unwrap();
            let request = serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap();
            let V2ClientMessage::SidebarCommand {
                daemon_instance_id,
                event_id,
                command,
                ..
            } = request
            else {
                panic!("expected sidebar command");
            };
            assert_eq!(daemon_instance_id, daemon_for_server);
            assert_eq!(
                command,
                V2SidebarCommand::MarkComplete {
                    pane_instance: PaneInstance {
                        pane_id: "%7".to_string(),
                        pane_pid: 4242,
                    },
                    expected: expected_for_server.clone(),
                }
            );
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::PaneEventResult {
                    event_id,
                    accepted_seq: 1,
                    state_version: Some(expected_for_server),
                    snapshot_revision: 2,
                    outcome: crate::daemon::protocol::v2::PaneApplyOutcome::Committed,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        send_sidebar_mark_complete_v2(
            &socket,
            "scratch",
            PaneInstance {
                pane_id: "%7".to_string(),
                pane_pid: 4242,
            },
            expected,
        )
        .unwrap();

        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_client_rejects_server_identity_mismatch_before_mutation() {
        let socket = unique_socket_path("vde-tmux-v2-identity");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert!(matches!(
                serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
                V2ClientMessage::Hello { .. }
            ));
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::HelloAck {
                    proto: PROTOCOL_VERSION,
                    daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                        "ffeeddccbbaa99887766554433221100",
                    )
                    .unwrap(),
                    server_identity: "actual".to_string(),
                    phase: crate::daemon::protocol::v2::DaemonPhase::Serving,
                    hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            line.clear();
            let read = BufReader::new(stream).read_line(&mut line).unwrap();
            assert_eq!(read, 0, "mutation must not follow a mismatched HelloAck");
        });

        let error = send_sidebar_jump_v2(
            &socket,
            "expected",
            PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 1,
            },
            PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 9,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("server identity mismatch"));

        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_sidebar_helpers_encode_all_commands() {
        let socket = unique_socket_path("vt2-cmds");
        let listener = UnixListener::bind(&socket).unwrap();
        let expected_version = StateVersion {
            state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            agent_epoch: 4,
            revision: 8,
        };
        let expected_commands = vec![
            V2SidebarCommand::JumpPane {
                pane_instance: PaneInstance {
                    pane_id: "%3".to_string(),
                    pane_pid: 303,
                },
                source_pane: PaneInstance {
                    pane_id: "%9".to_string(),
                    pane_pid: 909,
                },
            },
            V2SidebarCommand::MarkComplete {
                pane_instance: PaneInstance {
                    pane_id: "%3".to_string(),
                    pane_pid: 303,
                },
                expected: expected_version.clone(),
            },
            V2SidebarCommand::UpdateManualOrder {
                expected_version: 7,
                manual_order: vec![crate::sidebar::state::RepoId::new("misc", "app")],
                manual_chat_order: vec!["%3".to_string()],
            },
            V2SidebarCommand::UpdateViewPreferences {
                expected_version: 8,
                view_mode: crate::sidebar::state::ViewMode::ByCategory,
                filter: crate::sidebar::state::StatusFilter::DoneOnly,
            },
            V2SidebarCommand::SetExpansionOverride {
                expected_version: 2,
                row_id: "repo::misc::app".to_string(),
                overridden: true,
            },
        ];
        let expected_for_server = expected_commands.clone();
        let handle = thread::spawn(move || {
            for expected in expected_for_server {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(
                    serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
                    V2ClientMessage::Hello {
                        proto: PROTOCOL_VERSION
                    }
                );
                write_hello_ack(&mut stream, "scratch");
                line.clear();
                reader.read_line(&mut line).unwrap();
                let V2ClientMessage::SidebarCommand {
                    daemon_instance_id,
                    event_id,
                    command,
                    ..
                } = serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap()
                else {
                    panic!("expected sidebar command");
                };
                assert_eq!(daemon_instance_id, self::daemon_instance_id());
                assert_eq!(command, expected);
                let response = if matches!(command, V2SidebarCommand::MarkComplete { .. }) {
                    V2ServerMessage::PaneEventResult {
                        event_id,
                        accepted_seq: 1,
                        state_version: None,
                        snapshot_revision: 2,
                        outcome: crate::daemon::protocol::v2::PaneApplyOutcome::Committed,
                    }
                } else {
                    V2ServerMessage::SnapshotAck {
                        event_id,
                        accepted_seq: 1,
                        snapshot_revision: 2,
                    }
                };
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });

        send_sidebar_jump_v2(
            &socket,
            "scratch",
            PaneInstance {
                pane_id: "%3".to_string(),
                pane_pid: 303,
            },
            PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 909,
            },
        )
        .unwrap();
        send_sidebar_mark_complete_v2(
            &socket,
            "scratch",
            PaneInstance {
                pane_id: "%3".to_string(),
                pane_pid: 303,
            },
            expected_version,
        )
        .unwrap();
        send_sidebar_update_manual_order_v2(
            &socket,
            "scratch",
            7,
            vec![crate::sidebar::state::RepoId::new("misc", "app")],
            vec!["%3".to_string()],
        )
        .unwrap();
        send_sidebar_update_view_preferences_v2(
            &socket,
            "scratch",
            8,
            crate::sidebar::state::ViewMode::ByCategory,
            crate::sidebar::state::StatusFilter::DoneOnly,
        )
        .unwrap();
        assert_eq!(
            send_sidebar_set_expansion_override_v2(
                &socket,
                "scratch",
                2,
                "repo::misc::app".to_string(),
                true,
            )
            .unwrap(),
            2
        );

        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_refresh_topology_uses_hello_daemon_instance_and_event_id() {
        let socket = unique_socket_path("vt2-refresh");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            write_hello_ack(&mut stream, "scratch");
            line.clear();
            reader.read_line(&mut line).unwrap();
            let V2ClientMessage::RefreshTopology {
                daemon_instance_id,
                event_id,
                ..
            } = serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap()
            else {
                panic!("expected topology refresh");
            };
            assert_eq!(daemon_instance_id, self::daemon_instance_id());
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::SnapshotAck {
                    event_id,
                    accepted_seq: 7,
                    snapshot_revision: 11,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        request_topology_refresh_v2(&socket, "scratch").unwrap();

        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_mutation_rejects_mismatched_response_event_id() {
        let socket = unique_socket_path("vt2-event");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            write_hello_ack(&mut stream, "scratch");
            line.clear();
            reader.read_line(&mut line).unwrap();
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::SnapshotAck {
                    event_id: EventId::generate().unwrap(),
                    accepted_seq: 1,
                    snapshot_revision: 1,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let error = send_sidebar_jump_v2(
            &socket,
            "scratch",
            PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 1,
            },
            PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 9,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("response event ID mismatch"));

        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_subscribe_delivers_only_strictly_increasing_revisions() {
        let socket = unique_socket_path("vt2-sub");
        let listener = UnixListener::bind(&socket).unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let mut stream = accept_guarded_subscription(&listener, "scratch", "hash");
            for revision in [1, 1, 0, 2] {
                serde_json::to_writer(
                    &mut stream,
                    &V2ServerMessage::ResolvedSnapshotResult {
                        snapshot_revision: revision,
                        snapshot: empty_resolved_snapshot(revision),
                    },
                )
                .unwrap();
                stream.write_all(b"\n").unwrap();
            }
            // Keep the peer open until the client's finite initial poll/read completes. A real
            // Subscribe peer remains connected while later snapshots are streamed.
            release_rx.recv().unwrap();
        });
        let (tx, rx) = std::sync::mpsc::channel();

        subscribe_v2(&socket, "scratch", "hash", tx).unwrap();

        assert_eq!(recv_connected(&rx).snapshot_revision, 1);
        assert_eq!(recv_connected(&rx).snapshot_revision, 2);
        release_tx.send(()).unwrap();
        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_subscribe_rejects_snapshot_revision_mismatch() {
        let socket = unique_socket_path("vt2-sub-rev");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = thread::spawn(move || {
            let mut stream = accept_guarded_subscription(&listener, "scratch", "hash");
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::ResolvedSnapshotResult {
                    snapshot_revision: 2,
                    snapshot: empty_resolved_snapshot(1),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let (tx, rx) = std::sync::mpsc::channel();

        subscribe_v2(&socket, "scratch", "hash", tx).unwrap();
        let degraded = loop {
            if let SubscriptionUpdate::Degraded(error) =
                rx.recv_timeout(Duration::from_secs(1)).unwrap()
            {
                break error;
            }
        };
        assert!(degraded.contains("snapshot revision mismatch"));

        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_subscription_reconnects_after_peer_is_killed() {
        let socket = unique_socket_path("vt2-sub-reconnect");
        let listener = UnixListener::bind(&socket).unwrap();
        let (kill_first_tx, kill_first_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            for revision in [1, 2] {
                let mut stream = accept_guarded_subscription(&listener, "scratch", "hash");
                serde_json::to_writer(
                    &mut stream,
                    &V2ServerMessage::ResolvedSnapshotResult {
                        snapshot_revision: revision,
                        snapshot: empty_resolved_snapshot(revision),
                    },
                )
                .unwrap();
                stream.write_all(b"\n").unwrap();
                if revision == 1 {
                    kill_first_rx.recv().unwrap();
                } else {
                    release_rx.recv().unwrap();
                }
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        subscribe_v2(&socket, "scratch", "hash", tx).unwrap();

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SubscriptionUpdate::Connecting
        ));
        let first = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            SubscriptionUpdate::Connected(snapshot) => snapshot,
            update => panic!("expected first connected update, got {update:?}"),
        };
        assert_eq!(first.snapshot_revision, 1);

        kill_first_tx.send(()).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SubscriptionUpdate::Disconnected
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SubscriptionUpdate::Connecting
        ));
        let second = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            SubscriptionUpdate::Connected(snapshot) => snapshot,
            update => panic!("expected reconnected update, got {update:?}"),
        };
        assert_eq!(second.snapshot_revision, 2);
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());

        release_tx.send(()).unwrap();
        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_subscription_keeps_an_idle_peer_without_a_protocol_heartbeat() {
        let socket = unique_socket_path("vt2-sub-idle");
        let listener = UnixListener::bind(&socket).unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let mut stream = accept_guarded_subscription(&listener, "scratch", "hash");
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::ResolvedSnapshotResult {
                    snapshot_revision: 1,
                    snapshot: empty_resolved_snapshot(1),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            release_rx.recv().unwrap();
        });
        let mut subscription = V2SnapshotSubscription::connect(&socket, "scratch", "hash").unwrap();
        assert!(subscription.read_initial_snapshot().unwrap().is_some());
        assert_eq!(subscription.reader.get_ref().read_timeout().unwrap(), None);
        release_tx.send(()).unwrap();
        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_subscription_rejects_config_hash_mismatch_before_subscribing() {
        let socket = unique_socket_path("vt2-sub-config-mismatch");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            write_hello_ack(&mut stream, "scratch");
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert!(matches!(
                serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
                V2ClientMessage::QueryHealth { .. }
            ));
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::HealthResult {
                    health: crate::daemon::protocol::v2::DaemonHealth {
                        config_hash: "active".to_string(),
                        projection_revision: 1,
                        projection_updated_at_epoch_seconds: 1,
                        notification_enabled: false,
                        notification_failures: 0,
                        notification_queue_drops: 0,
                        notification_degraded: false,
                        last_notification_error_code: None,
                        current_quarantine_count: 0,
                        quarantine_observed_total: 0,
                        recent_error_code: None,
                        hook_delivery_failures: 0,
                        hook_delivery_degraded: false,
                        last_hook_error_code: None,
                        status_push_failures: 0,
                        status_push_degraded: false,
                        last_status_push_error: None,
                        last_status_push_error_at_epoch_seconds: None,
                    },
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert!(line.is_empty(), "mismatched config must not subscribe");
        });

        let error = match V2SnapshotSubscription::connect(&socket, "scratch", "disk") {
            Ok(_) => panic!("mismatched config unexpectedly subscribed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("vt daemon reload"));
        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_sidebar_command_waits_beyond_subscribe_initial_budget() {
        assert!(V2_SIDEBAR_COMMAND_TIMEOUT > Duration::from_secs(2));
        let socket = unique_socket_path("vt2-command-budget");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            write_hello_ack(&mut stream, "scratch");
            line.clear();
            reader.read_line(&mut line).unwrap();
            let V2ClientMessage::SidebarCommand { event_id, .. } =
                serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap()
            else {
                panic!("expected sidebar command");
            };
            thread::sleep(Duration::from_millis(600));
            serde_json::to_writer(
                &mut stream,
                &V2ServerMessage::SnapshotAck {
                    event_id,
                    accepted_seq: 1,
                    snapshot_revision: 1,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let started = Instant::now();

        send_sidebar_jump_v2(
            &socket,
            "scratch",
            PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 1,
            },
            PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 9,
            },
        )
        .unwrap();

        assert!(started.elapsed() >= V2_SUBSCRIBE_INITIAL_TIMEOUT);
        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    fn unique_socket_path(label: &str) -> PathBuf {
        static NEXT_SOCKET_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let socket_id = NEXT_SOCKET_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("{label}-{}-{socket_id}.sock", std::process::id(),))
    }
}

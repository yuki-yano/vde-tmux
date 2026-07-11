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

pub fn send_sidebar_key_v2(socket: &Path, server_identity: &str, key: &str) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::Key {
            key: key.to_string(),
        },
        V2SidebarResponse::SnapshotAck,
    )?;
    Ok(())
}

pub fn send_sidebar_jump_v2(socket: &Path, server_identity: &str, pane_id: &str) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::JumpPane {
            pane_id: pane_id.to_string(),
        },
        V2SidebarResponse::SnapshotAck,
    )?;
    Ok(())
}

pub fn send_sidebar_mark_done_v2(
    socket: &Path,
    server_identity: &str,
    pane_instance: PaneInstance,
    expected: StateVersion,
) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::MarkDone {
            pane_instance,
            expected,
        },
        V2SidebarResponse::PaneEventResult,
    )?;
    Ok(())
}

pub fn send_sidebar_select_context_v2(
    socket: &Path,
    server_identity: &str,
    pane_id: Option<&str>,
    session_id: Option<&str>,
) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::SelectContext {
            pane_id: pane_id.map(ToOwned::to_owned),
            session_id: session_id.map(ToOwned::to_owned),
        },
        V2SidebarResponse::SnapshotAck,
    )?;
    Ok(())
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

pub fn subscribe_v2(
    socket: &Path,
    server_identity: &str,
    tx: Sender<ResolvedSnapshot>,
) -> Result<()> {
    let mut subscription = V2SnapshotSubscription::connect(socket, server_identity)?;
    let first = subscription.read_initial_snapshot()?.ok_or_else(|| {
        anyhow::anyhow!("daemon closed the subscription before the initial snapshot")
    })?;
    tx.send(first)
        .map_err(|_| anyhow::anyhow!("sidebar snapshot receiver disconnected"))?;
    thread::spawn(move || {
        loop {
            match subscription.read_next_snapshot() {
                Ok(Some(snapshot)) => {
                    if tx.send(snapshot).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    eprintln!("[vde-tmux] v2 sidebar subscribe error: {error:#}");
                    break;
                }
            }
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
    last_revision: Option<u64>,
    initial_deadline: Instant,
}

impl V2SnapshotSubscription {
    fn connect(socket: &Path, server_identity: &str) -> Result<Self> {
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
            &V2ClientMessage::Subscribe {
                proto: PROTOCOL_VERSION,
            },
            deadline,
        )?;
        Ok(Self {
            reader,
            last_revision: None,
            initial_deadline: deadline,
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
                V2ServerMessage::Error { code, message, .. } => {
                    bail!("{code:?}: {message}")
                }
                other => bail!("unexpected daemon subscription response: {other:?}"),
            }
        }
    }
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

    fn empty_resolved_snapshot(revision: u64) -> ResolvedSnapshot {
        ResolvedSnapshot {
            snapshot_revision: revision,
            panes: Vec::new(),
            sidebar: crate::daemon::SidebarFrame {
                state: crate::sidebar::state::SidebarState::default(),
                counts: crate::sidebar::tree::BadgeCounts::default(),
                rows: Vec::new(),
            },
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn v2_mark_done_handshakes_and_sends_full_state_version() {
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
                V2SidebarCommand::MarkDone {
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

        send_sidebar_mark_done_v2(
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

        let error = send_sidebar_key_v2(&socket, "expected", "down").unwrap_err();
        assert!(error.to_string().contains("server identity mismatch"));

        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_sidebar_helpers_encode_all_four_commands() {
        let socket = unique_socket_path("vt2-cmds");
        let listener = UnixListener::bind(&socket).unwrap();
        let expected_version = StateVersion {
            state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            agent_epoch: 4,
            revision: 8,
        };
        let expected_commands = vec![
            V2SidebarCommand::Key {
                key: "down".to_string(),
            },
            V2SidebarCommand::JumpPane {
                pane_id: "%3".to_string(),
            },
            V2SidebarCommand::MarkDone {
                pane_instance: PaneInstance {
                    pane_id: "%3".to_string(),
                    pane_pid: 303,
                },
                expected: expected_version.clone(),
            },
            V2SidebarCommand::SelectContext {
                pane_id: Some("%3".to_string()),
                session_id: Some("$1".to_string()),
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
                let response = if matches!(command, V2SidebarCommand::MarkDone { .. }) {
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

        send_sidebar_key_v2(&socket, "scratch", "down").unwrap();
        send_sidebar_jump_v2(&socket, "scratch", "%3").unwrap();
        send_sidebar_mark_done_v2(
            &socket,
            "scratch",
            PaneInstance {
                pane_id: "%3".to_string(),
                pane_pid: 303,
            },
            expected_version,
        )
        .unwrap();
        send_sidebar_select_context_v2(&socket, "scratch", Some("%3"), Some("$1")).unwrap();

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

        let error = send_sidebar_key_v2(&socket, "scratch", "down").unwrap_err();
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
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(matches!(
                serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
                V2ClientMessage::Hello { .. }
            ));
            write_hello_ack(&mut stream, "scratch");
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<V2ClientMessage>(line.trim()).unwrap(),
                V2ClientMessage::Subscribe {
                    proto: PROTOCOL_VERSION
                }
            );
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

        subscribe_v2(&socket, "scratch", tx).unwrap();
        release_tx.send(()).unwrap();

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .snapshot_revision,
            1
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .snapshot_revision,
            2
        );
        handle.join().unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn v2_subscribe_rejects_snapshot_revision_mismatch() {
        let socket = unique_socket_path("vt2-sub-rev");
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
                &V2ServerMessage::ResolvedSnapshotResult {
                    snapshot_revision: 2,
                    snapshot: empty_resolved_snapshot(1),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let (tx, _rx) = std::sync::mpsc::channel();

        let error = subscribe_v2(&socket, "scratch", tx).unwrap_err();
        assert!(error.to_string().contains("snapshot revision mismatch"));

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

        send_sidebar_key_v2(&socket, "scratch", "down").unwrap();

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

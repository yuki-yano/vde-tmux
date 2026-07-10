use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{Result, bail};

use crate::daemon::DaemonSnapshot;
use crate::daemon::protocol::v2::{
    ClientMessage as V2ClientMessage, PROTOCOL_VERSION, ServerMessage as V2ServerMessage,
    SidebarCommand as V2SidebarCommand, V2Client,
};
use crate::daemon::protocol::{ClientMessage, ServerMessage, SidebarClientEvent};
use crate::pane_state::{EventId, PaneInstance, StateVersion};

pub fn socket_path(env: &BTreeMap<String, String>) -> PathBuf {
    crate::daemon::daemon_socket_path(env, None)
}

pub fn send_sidebar_key(socket: &Path, key: &str) -> Result<()> {
    request_ack(
        socket,
        ClientMessage::SidebarEvent {
            proto: 1,
            event: SidebarClientEvent::Key {
                key: key.to_string(),
            },
        },
    )
}

pub fn send_sidebar_jump(socket: &Path, pane: &str) -> Result<()> {
    request_ack(
        socket,
        ClientMessage::SidebarEvent {
            proto: 1,
            event: SidebarClientEvent::JumpPane {
                pane: pane.to_string(),
            },
        },
    )
}

pub fn send_sidebar_mark_done(socket: &Path, pane: &str) -> Result<()> {
    request_ack(
        socket,
        ClientMessage::SidebarEvent {
            proto: 1,
            event: SidebarClientEvent::MarkDone {
                pane: pane.to_string(),
            },
        },
    )
}

pub fn send_sidebar_select_context(
    socket: &Path,
    pane: Option<&str>,
    session: Option<&str>,
) -> Result<()> {
    request_ack(
        socket,
        ClientMessage::SidebarEvent {
            proto: 1,
            event: SidebarClientEvent::SelectContext {
                pane: pane.map(ToOwned::to_owned),
                session: session.map(ToOwned::to_owned),
            },
        },
    )
}

pub fn send_sidebar_toggle(socket: &Path, row_id: &str) -> Result<()> {
    send_sidebar_key(socket, &format!("toggle:{row_id}"))
}

pub fn send_sidebar_key_v2(socket: &Path, server_identity: &str, key: &str) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::Key {
            key: key.to_string(),
        },
        V2SidebarResponse::SnapshotAck,
    )
}

pub fn send_sidebar_jump_v2(socket: &Path, server_identity: &str, pane_id: &str) -> Result<()> {
    request_v2_sidebar(
        socket,
        server_identity,
        V2SidebarCommand::JumpPane {
            pane_id: pane_id.to_string(),
        },
        V2SidebarResponse::SnapshotAck,
    )
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
    )
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
    )
}

pub fn request_pane_refresh(socket: &Path) -> Result<()> {
    request_ack(socket, ClientMessage::RefreshPanes { proto: 1 })
}

pub fn subscribe(socket: &Path, tx: Sender<DaemonSnapshot>) -> Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    serde_json::to_writer(&mut stream, &ClientMessage::Subscribe { proto: 1 })?;
    stream.write_all(b"\n")?;
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let raw = match line {
                Ok(raw) => raw,
                Err(error) => {
                    eprintln!("[vde-tmux] daemon subscribe read error: {error:#}");
                    break;
                }
            };
            let message = match serde_json::from_str::<ServerMessage>(raw.trim()) {
                Ok(message) => message,
                Err(error) => {
                    eprintln!("[vde-tmux] daemon subscribe decode error: {error:#}");
                    continue;
                }
            };
            match message {
                ServerMessage::Snapshot { snapshot } => match tx.send(snapshot) {
                    Ok(()) => {}
                    Err(_) => break,
                },
                ServerMessage::Error { message } => {
                    eprintln!("[vde-tmux] daemon subscribe error: {message}");
                    break;
                }
                _ => {}
            }
        }
    });
    Ok(())
}

fn request_ack(socket: &Path, message: ClientMessage) -> Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    serde_json::to_writer(&mut stream, &message)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    match serde_json::from_str::<ServerMessage>(line.trim())? {
        ServerMessage::Ack => Ok(()),
        ServerMessage::Error { message } => bail!(message),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn request_v2_sidebar(
    socket: &Path,
    server_identity: &str,
    command: V2SidebarCommand,
    expected_response: V2SidebarResponse,
) -> Result<()> {
    let mut client = V2Client::connect(socket, server_identity)?;
    let event_id = EventId::generate()?;
    let response = client.request(&V2ClientMessage::SidebarCommand {
        proto: PROTOCOL_VERSION,
        daemon_instance_id: client.daemon_instance_id().clone(),
        event_id: event_id.clone(),
        command,
    })?;
    match (expected_response, response) {
        (
            V2SidebarResponse::PaneEventResult,
            V2ServerMessage::PaneEventResult {
                event_id: response_event_id,
                ..
            },
        ) if response_event_id == event_id => Ok(()),
        (
            V2SidebarResponse::SnapshotAck,
            V2ServerMessage::SnapshotAck {
                event_id: response_event_id,
                ..
            },
        ) if response_event_id == event_id => Ok(()),
        (_, V2ServerMessage::Error { code, message, .. }) => bail!("{code:?}: {message}"),
        (_, other) => bail!("unexpected daemon response: {other:?}"),
    }
}

#[derive(Debug, Clone, Copy)]
enum V2SidebarResponse {
    SnapshotAck,
    PaneEventResult,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

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
    fn request_pane_refresh_sends_refresh_panes_message() {
        let socket = unique_socket_path("vde-tmux-refresh-panes");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let mut line = String::new();
                        BufReader::new(&mut stream).read_line(&mut line).unwrap();
                        let message: ClientMessage = serde_json::from_str(line.trim()).unwrap();
                        tx.send(message).unwrap();
                        serde_json::to_writer(&mut stream, &ServerMessage::Ack).unwrap();
                        stream.write_all(b"\n").unwrap();
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });

        request_pane_refresh(&socket).unwrap();

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ClientMessage::RefreshPanes { proto: 1 }
        );
        handle.join().unwrap();
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn subscribe_skips_invalid_json_line_and_reads_next_snapshot() {
        let socket = unique_socket_path("vt-sub-bad");
        let listener = UnixListener::bind(&socket).unwrap();
        let server_socket = socket.clone();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream).read_line(&mut line).unwrap();
            let message: ClientMessage = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(message, ClientMessage::Subscribe { proto: 1 });

            stream.write_all(b"{not-json}\n").unwrap();
            serde_json::to_writer(
                &mut stream,
                &ServerMessage::Snapshot {
                    snapshot: crate::daemon::build_snapshot(&[]),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            std::fs::remove_file(server_socket).unwrap();
        });
        let (tx, rx) = std::sync::mpsc::channel();

        subscribe(&socket, tx).unwrap();

        let snapshot = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(snapshot.agent_count, 0);
        handle.join().unwrap();
    }

    fn unique_socket_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{label}-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}

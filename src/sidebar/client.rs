use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{Result, bail};

use crate::daemon::DaemonSnapshot;
use crate::daemon::protocol::{ClientMessage, ServerMessage, SidebarClientEvent};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

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

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

pub fn subscribe(socket: &Path, tx: Sender<DaemonSnapshot>) -> Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    serde_json::to_writer(&mut stream, &ClientMessage::Subscribe { proto: 1 })?;
    stream.write_all(b"\n")?;
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            match line
                .map_err(anyhow::Error::from)
                .and_then(|raw| Ok(serde_json::from_str::<ServerMessage>(raw.trim())?))
            {
                Ok(ServerMessage::Snapshot { snapshot }) => {
                    if tx.send(snapshot).is_err() {
                        break;
                    }
                }
                Ok(ServerMessage::Error { message }) => {
                    eprintln!("[vde-tmux] daemon subscribe error: {message}");
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("[vde-tmux] daemon subscribe read error: {error:#}");
                    break;
                }
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

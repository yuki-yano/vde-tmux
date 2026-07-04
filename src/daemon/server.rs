//! daemon server と request handler。

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use anyhow::{Context, Result};

use super::protocol::{ClientMessage, QueryTarget, ServerMessage};
use crate::daemon::{build_snapshot, statusline_agent_badge_fallback};
use crate::options::snapshot::read_all_panes;
use crate::tmux::TmuxRunner;

pub fn handle_message(runner: &dyn TmuxRunner, message: ClientMessage) -> Result<ServerMessage> {
    match message {
        ClientMessage::Query {
            proto: _,
            what: QueryTarget::Statusline,
        } => {
            let agent_badge = statusline_agent_badge_fallback(runner)?;
            Ok(ServerMessage::Statusline { agent_badge })
        }
        ClientMessage::Subscribe { proto: _ } => {
            let panes = read_all_panes(runner)?;
            Ok(ServerMessage::Snapshot {
                snapshot: build_snapshot(&panes),
            })
        }
        ClientMessage::StatuslineAgentBadge => {
            let value = statusline_agent_badge_fallback(runner)?;
            Ok(ServerMessage::StatuslineAgentBadge { value })
        }
    }
}

pub fn handle_stream(runner: &dyn TmuxRunner, mut stream: UnixStream) -> Result<()> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader.read_line(&mut line)?;
    }
    let response = match serde_json::from_str::<ClientMessage>(line.trim()) {
        Ok(message) => handle_message(runner, message)?,
        Err(error) => ServerMessage::Error {
            message: error.to_string(),
        },
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

pub fn run_daemon_server(runner: &dyn TmuxRunner, socket_path: &Path) -> Result<()> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    if socket_path.exists() {
        fs::remove_file(socket_path)
            .with_context(|| format!("failed to remove {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(error) = handle_stream(runner, stream) {
                    eprintln!("[vde-tmux] daemon connection error: {error:#}");
                }
            }
            Err(error) => return Err(error.into()),
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

    fn pane_line(agent: &str, status: &str, wait_reason: &str) -> String {
        [
            "main",
            "@1",
            "%1",
            "/tmp",
            "zsh",
            "",
            agent,
            status,
            "",
            "",
            wait_reason,
            "",
            "",
            "",
            "",
            "",
        ]
        .join("\u{1f}")
    }

    #[test]
    fn handle_message_returns_statusline_badge() {
        let mock = MockTmuxRunner::new();
        let format = snapshot_format();
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            &format!("{}\n", pane_line("codex", "running", "")),
        );
        let response = handle_message(&mock, ClientMessage::StatuslineAgentBadge).unwrap();
        assert_eq!(
            response,
            ServerMessage::StatuslineAgentBadge {
                value: "running:1".to_string()
            }
        );
    }

    #[test]
    fn handle_query_returns_statusline_payload() {
        let mock = MockTmuxRunner::new();
        let format = snapshot_format();
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            &format!("{}\n", pane_line("codex", "running", "")),
        );
        let response = handle_message(
            &mock,
            ClientMessage::Query {
                proto: 1,
                what: crate::daemon::protocol::QueryTarget::Statusline,
            },
        )
        .unwrap();
        assert_eq!(
            response,
            ServerMessage::Statusline {
                agent_badge: "running:1".to_string()
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
        let response = handle_message(&mock, ClientMessage::Subscribe { proto: 1 }).unwrap();
        let ServerMessage::Snapshot { snapshot } = response else {
            panic!("expected snapshot response");
        };
        assert_eq!(snapshot.agent_count, 1);
    }
}

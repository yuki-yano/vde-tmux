//! daemon server と request handler。

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use anyhow::{Context, Result};

use super::protocol::{ClientMessage, ServerMessage};
use crate::daemon::statusline_agent_badge_fallback;
use crate::tmux::TmuxRunner;

pub fn handle_message(runner: &dyn TmuxRunner, message: ClientMessage) -> Result<ServerMessage> {
    match message {
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
    if socket_path.exists() {
        fs::remove_file(socket_path)
            .with_context(|| format!("failed to remove {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = handle_stream(runner, stream);
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
}

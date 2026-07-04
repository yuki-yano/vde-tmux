//! daemon server と request handler。

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use super::protocol::{ClientMessage, QueryTarget, ServerMessage};
use super::runtime::{ClientId, DaemonEvent, LatestSlot, RuntimeEffect, RuntimeState};
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
        ClientMessage::SidebarEvent { .. } => Ok(ServerMessage::Error {
            message: "sidebar events require runtime daemon".to_string(),
        }),
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
        ClientMessage::Query {
            proto: _,
            what: QueryTarget::Statusline,
        }
        | ClientMessage::StatuslineAgentBadge => {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(DaemonEvent::QueryStatusline { reply: reply_tx })?;
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

fn write_server_message(stream: &mut UnixStream, message: &ServerMessage) -> Result<()> {
    serde_json::to_writer(&mut *stream, message)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
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

pub fn run_runtime_daemon_server(
    config: crate::config::Config,
    socket_path: &Path,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
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

    let (tx, rx) = mpsc::channel();
    let latest_panes = Arc::new(crate::daemon::workers::LatestPanes::default());
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    let worker_io = Arc::new(crate::daemon::workers::SystemWorkerIo::new(runner));
    crate::daemon::workers::start_tmux_worker(
        worker_io.clone(),
        latest_panes.clone(),
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
    )
}

pub fn run_runtime_loop(
    mut state: RuntimeState,
    rx: mpsc::Receiver<DaemonEvent>,
    state_path: Option<std::path::PathBuf>,
    worker_io: Arc<dyn crate::daemon::workers::WorkerIo>,
) -> Result<()> {
    while state.is_running() {
        let effects = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => state.apply_event(event),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                state.apply_event(DaemonEvent::DebounceCheck(std::time::Instant::now()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        handle_runtime_effects(effects, state_path.as_deref(), worker_io.as_ref())?;
    }
    handle_runtime_effects(
        state.apply_event(DaemonEvent::DebounceCheck(
            std::time::Instant::now() + Duration::from_secs(1),
        )),
        state_path.as_deref(),
        worker_io.as_ref(),
    )?;
    Ok(())
}

fn handle_runtime_effects(
    effects: Vec<RuntimeEffect>,
    state_path: Option<&Path>,
    worker_io: &dyn crate::daemon::workers::WorkerIo,
) -> Result<()> {
    for effect in effects {
        match effect {
            RuntimeEffect::JumpPane(pane_id) => worker_io.jump_to_pane(&pane_id)?,
            RuntimeEffect::SaveState(state) => {
                if let Some(path) = state_path {
                    crate::sidebar::store::save_state(path, &state)?;
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

    #[derive(Default)]
    struct LoopWorkerIo {
        jumps: std::sync::Mutex<Vec<String>>,
    }

    impl crate::daemon::workers::WorkerIo for LoopWorkerIo {
        fn read_panes(&self) -> anyhow::Result<Vec<crate::options::snapshot::PaneSnapshot>> {
            Ok(Vec::new())
        }

        fn capture_tail(&self, _pane_id: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }

        fn jump_to_pane(&self, pane_id: &str) -> anyhow::Result<()> {
            self.jumps.lock().unwrap().push(pane_id.to_string());
            Ok(())
        }
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
            agent: "codex".to_string(),
            status: "running".to_string(),
            ..PaneSnapshot::default()
        }]))
        .unwrap();
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(DaemonEvent::QueryStatusline { reply: reply_tx })
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
        )
        .unwrap();

        assert_eq!(
            reply_rx.recv().unwrap(),
            ServerMessage::Statusline {
                agent_badge: "running:1".to_string()
            }
        );
        assert_eq!(io.jumps.lock().unwrap().as_slice(), ["%1"]);
    }
}

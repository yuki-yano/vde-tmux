//! daemon server と request handler。

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
use crate::daemon::{build_snapshot, statusline_agent_badge_fallback};
use crate::options::snapshot::read_all_panes;
use crate::tmux::TmuxRunner;

const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
static SHUTDOWN_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

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
        ClientMessage::SidebarEvent { .. } => Ok(ServerMessage::Error {
            message: "sidebar events require runtime daemon".to_string(),
        }),
        ClientMessage::Shutdown { .. } => Ok(ServerMessage::Error {
            message: "shutdown requires runtime daemon".to_string(),
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
        ClientMessage::Shutdown { proto: _ } => {
            tx.send(DaemonEvent::Shutdown)?;
            write_server_message(&mut stream, &ServerMessage::Ack)?;
        }
        ClientMessage::Query {
            proto: _,
            what: QueryTarget::Statusline,
        } => {
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
    install_shutdown_signal_handler(tx.clone())?;
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
            RuntimeEffect::SetSessionBadge { session, value } => {
                if let Err(error) = worker_io.set_session_option(
                    &session,
                    crate::options::KEY_SESSION_STATUS,
                    &value,
                ) {
                    eprintln!("[vde-tmux] session badge set failed: {error:#}");
                }
            }
            RuntimeEffect::ClearSessionBadge { session } => {
                if let Err(error) =
                    worker_io.unset_session_option(&session, crate::options::KEY_SESSION_STATUS)
                {
                    eprintln!("[vde-tmux] session badge clear failed: {error:#}");
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
            agent,
            "0",
            "0",
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
        jumps: std::sync::Mutex<Vec<String>>,
        previews: std::sync::Mutex<Vec<(String, u32)>>,
        session_options: std::sync::Mutex<Vec<(String, String, Option<String>)>>,
        fail_jump: bool,
    }

    impl crate::daemon::workers::WorkerIo for LoopWorkerIo {
        fn read_panes(&self) -> anyhow::Result<Vec<crate::options::snapshot::PaneSnapshot>> {
            Ok(Vec::new())
        }

        fn capture_tail(&self, _pane_id: &str) -> anyhow::Result<String> {
            Ok(String::new())
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
        tx.send(DaemonEvent::QueryStatusline { reply: reply_tx })
            .unwrap();
        drop(tx);

        run_runtime_loop(
            RuntimeState::new(crate::config::Config::default(), SidebarState::default()),
            rx,
            None,
            io,
        )
        .unwrap();

        assert_eq!(
            reply_rx.recv().unwrap(),
            ServerMessage::Statusline {
                agent_badge: String::new()
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
            thread::spawn(move || run_runtime_loop(state, rx, None, io))
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
                    Some("🟡 ".to_string()),
                ),
                ("main".to_string(), "@vde_session_status".to_string(), None),
            ]
        );
    }
}

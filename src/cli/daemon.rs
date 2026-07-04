use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::daemon::protocol::{ClientMessage, ServerMessage};
use crate::tmux::TmuxRunner;

pub(crate) fn statusline_agent_badge(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    crate::daemon::statusline_agent_badge(runner, env)
}

pub(crate) fn run_daemon(
    _runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let socket_path = crate::daemon::daemon_socket_path(env, socket);
    let loaded = crate::config::load::load_config(env);
    for warning in loaded.warnings {
        eprintln!("{warning}");
    }
    crate::daemon::server::run_runtime_daemon_server(loaded.config, &socket_path, env)?;
    Ok(None)
}

pub(crate) fn stop_daemon(
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let socket_path = crate::daemon::daemon_socket_path(env, socket);
    if !socket_path.exists() {
        return Ok(Some(format!(
            "daemon is not running: {}",
            socket_path.display()
        )));
    }

    match request_shutdown(&socket_path) {
        Ok(()) => {
            wait_for_socket_owner_exit(&socket_path);
            remove_socket_file(&socket_path)?;
            Ok(Some(format!("daemon stopped: {}", socket_path.display())))
        }
        Err(protocol_error) => {
            if terminate_socket_owner(&socket_path)? {
                wait_for_socket_owner_exit(&socket_path);
                remove_socket_file(&socket_path)?;
                Ok(Some(format!("daemon stopped: {}", socket_path.display())))
            } else if is_stale_socket_error(&protocol_error) {
                remove_socket_file(&socket_path)?;
                Ok(Some(format!(
                    "removed stale daemon socket: {}",
                    socket_path.display()
                )))
            } else {
                Err(protocol_error)
                    .with_context(|| format!("failed to stop daemon at {}", socket_path.display()))
            }
        }
    }
}

fn request_shutdown(socket_path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect {}", socket_path.display()))?;
    serde_json::to_writer(&mut stream, &ClientMessage::Shutdown { proto: 1 })?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line)?;
    match serde_json::from_str::<ServerMessage>(line.trim())? {
        ServerMessage::Ack => Ok(()),
        ServerMessage::Error { message } => bail!(message),
        ServerMessage::Statusline { .. } => bail!("unexpected daemon statusline response"),
        ServerMessage::Snapshot { .. } => bail!("unexpected daemon snapshot response"),
    }
}

fn terminate_socket_owner(socket_path: &Path) -> Result<bool> {
    let output = match Command::new("lsof").arg("-t").arg(socket_path).output() {
        Ok(output) => output,
        Err(_) => return Ok(false),
    };
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(pid) = stdout
        .lines()
        .find_map(|line| line.trim().parse::<libc::pid_t>().ok())
    else {
        return Ok(false);
    };
    // SAFETY: `kill` is called with a pid reported by lsof and a standard signal.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to send SIGTERM to daemon");
    }
    Ok(true)
}

fn wait_for_socket_owner_exit(socket_path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !socket_owner_exists(socket_path) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn socket_owner_exists(socket_path: &Path) -> bool {
    Command::new("lsof")
        .arg("-t")
        .arg(socket_path)
        .output()
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

fn remove_socket_file(socket_path: &Path) -> Result<()> {
    match std::fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove daemon socket {}", socket_path.display())),
    }
}

fn is_stale_socket_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| {
                matches!(
                    io_error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                )
            })
    })
}

pub(crate) fn config_schema() -> Result<Option<String>> {
    Ok(Some(serde_json::to_string_pretty(
        &crate::config::schema::config_schema(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{ClientMessage, ServerMessage};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn stop_daemon_sends_shutdown_and_removes_socket() {
        let socket_path = unique_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            assert_eq!(
                serde_json::from_str::<ClientMessage>(request.trim()).unwrap(),
                ClientMessage::Shutdown { proto: 1 }
            );
            serde_json::to_writer(&mut stream, &ServerMessage::Ack).unwrap();
            stream.write_all(b"\n").unwrap();
            drop(listener);
        });

        let env = BTreeMap::new();
        let output = stop_daemon(&env, socket_path.to_str()).unwrap().unwrap();

        handle.join().unwrap();
        assert!(output.contains("daemon stopped:"));
        assert!(!socket_path.exists());
    }

    #[test]
    fn stop_daemon_removes_stale_socket_without_owner() {
        let socket_path = unique_socket_path();
        UnixListener::bind(&socket_path).unwrap();

        let env = BTreeMap::new();
        let output = stop_daemon(&env, socket_path.to_str()).unwrap().unwrap();

        assert!(output.contains("removed stale daemon socket:"));
        assert!(!socket_path.exists());
    }

    fn unique_socket_path() -> PathBuf {
        let counter = SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        PathBuf::from(format!(
            "/tmp/vt-daemon-stop-test-{}-{counter}-{nanos}.sock",
            std::process::id()
        ))
    }
}

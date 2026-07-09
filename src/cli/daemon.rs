use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::daemon::protocol::{ClientMessage, ServerMessage};
use crate::tmux::TmuxRunner;

pub(crate) fn statusline_summary(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<String> {
    crate::daemon::statusline_summary(runner, env, config)
}

pub(crate) fn statusline_attention(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<String> {
    crate::statusline::statusline_attention(runner, env, config)
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
    let report = stop_daemon_instances(&socket_path)?;
    if report.stopped {
        return Ok(Some(format!("daemon stopped: {}", socket_path.display())));
    }
    if report.removed_stale_socket {
        return Ok(Some(format!(
            "removed stale daemon socket: {}",
            socket_path.display()
        )));
    }
    Ok(Some(format!(
        "daemon is not running: {}",
        socket_path.display()
    )))
}

pub(crate) fn restart_daemon(
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    restart_daemon_with(
        env,
        socket,
        &exe,
        &crate::daemon::lifecycle::SystemDaemonSpawner,
    )
}

fn restart_daemon_with(
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
    exe: &Path,
    spawner: &dyn crate::daemon::lifecycle::DaemonSpawner,
) -> Result<Option<String>> {
    let socket_path = crate::daemon::daemon_socket_path(env, socket);
    if let Some(parent) = socket_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    let _start_lock = crate::daemon::lifecycle::acquire_daemon_start_lock(&socket_path)?;
    stop_daemon_instances(&socket_path)?;
    crate::daemon::lifecycle::ensure_daemon_started_with_lock(&socket_path, exe, spawner)?;
    Ok(Some(format!("daemon restarted: {}", socket_path.display())))
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
        ServerMessage::Summary { .. } => bail!("unexpected daemon summary response"),
        ServerMessage::Attention { .. } => bail!("unexpected daemon attention response"),
        ServerMessage::Snapshot { .. } => bail!("unexpected daemon snapshot response"),
    }
}

#[derive(Debug, Default)]
struct StopReport {
    stopped: bool,
    removed_stale_socket: bool,
}

fn stop_daemon_instances(socket_path: &Path) -> Result<StopReport> {
    let socket_existed = socket_path.exists();
    let mut report = StopReport::default();
    let mut pids = daemon_pids_for_socket(socket_path)?;
    let mut protocol_error = None;

    if socket_existed {
        match request_shutdown(socket_path) {
            Ok(()) => report.stopped = true,
            Err(error) => protocol_error = Some(error),
        }
    }

    pids.extend(daemon_pids_for_socket(socket_path)?);
    wait_for_pids_exit(&pids, Duration::from_secs(2));
    let alive = alive_pids(&pids);
    if !alive.is_empty() {
        terminate_pids(&alive)?;
        report.stopped = true;
        wait_for_pids_exit(&alive, Duration::from_secs(2));
        let remaining = alive_pids(&alive);
        if !remaining.is_empty() {
            bail!("daemon did not stop: pids {remaining:?}");
        }
    }

    if let Some(error) = protocol_error
        && !report.stopped
        && !is_stale_socket_error(&error)
    {
        return Err(error)
            .with_context(|| format!("failed to stop daemon at {}", socket_path.display()));
    }

    if socket_path.exists() {
        remove_socket_file(socket_path)?;
        if !report.stopped && socket_existed {
            report.removed_stale_socket = true;
        }
    }
    Ok(report)
}

fn daemon_pids_for_socket(socket_path: &Path) -> Result<BTreeSet<libc::pid_t>> {
    let mut pids = socket_owner_pids(socket_path);
    pids.extend(command_pids_for_socket(socket_path)?);
    pids.remove(&(std::process::id() as libc::pid_t));
    Ok(pids)
}

fn socket_owner_pids(socket_path: &Path) -> BTreeSet<libc::pid_t> {
    let output = match Command::new("lsof").arg("-t").arg(socket_path).output() {
        Ok(output) => output,
        Err(_) => return BTreeSet::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<libc::pid_t>().ok())
        .collect()
}

fn command_pids_for_socket(socket_path: &Path) -> Result<BTreeSet<libc::pid_t>> {
    let output = match Command::new("ps")
        .args(["axww", "-o", "pid=", "-o", "command="])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Ok(BTreeSet::new()),
    };
    if !output.status.success() {
        return Ok(BTreeSet::new());
    }
    let mut pids = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim_start();
        let Some(separator) = line.find(char::is_whitespace) else {
            continue;
        };
        let (pid, command) = line.split_at(separator);
        let Ok(pid) = pid.trim().parse::<libc::pid_t>() else {
            continue;
        };
        if command_matches_daemon_socket(command.trim_start(), socket_path) {
            pids.insert(pid);
        }
    }
    Ok(pids)
}

fn command_matches_daemon_socket(command: &str, socket_path: &Path) -> bool {
    let socket = socket_path.display().to_string();
    let args = command.split_whitespace().collect::<Vec<_>>();
    let Some(daemon_index) = args.iter().position(|arg| *arg == "daemon") else {
        return false;
    };
    let daemon_args = &args[(daemon_index + 1)..];
    if daemon_args
        .iter()
        .any(|arg| matches!(*arg, "stop" | "restart"))
    {
        return false;
    }
    daemon_args
        .windows(2)
        .any(|window| matches!(window, ["--socket", value] if *value == socket.as_str()))
        || daemon_args
            .iter()
            .any(|arg| arg.strip_prefix("--socket=") == Some(socket.as_str()))
}

fn terminate_pids(pids: &BTreeSet<libc::pid_t>) -> Result<()> {
    for pid in pids {
        send_signal(*pid, libc::SIGTERM)
            .with_context(|| format!("failed to send SIGTERM to daemon pid {pid}"))?;
    }
    Ok(())
}

fn wait_for_pids_exit(pids: &BTreeSet<libc::pid_t>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if alive_pids(pids).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive_pids(pids: &BTreeSet<libc::pid_t>) -> BTreeSet<libc::pid_t> {
    pids.iter()
        .copied()
        .filter(|pid| process_exists(*pid))
        .collect()
}

fn process_exists(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    matches!(error.raw_os_error(), Some(libc::EPERM))
}

fn send_signal(pid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::ESRCH)) {
        return Ok(());
    }
    Err(error.into())
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
                ) || matches!(io_error.raw_os_error(), Some(libc::ENOTSOCK))
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
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

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

    #[test]
    fn restart_daemon_removes_stale_socket_and_starts_daemon() {
        let dir = unique_socket_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = dir.join("daemon.sock");
        std::fs::write(&socket_path, "stale").unwrap();
        let spawner = ReadyDaemonSpawner::default();

        let output = restart_daemon_with(
            &BTreeMap::new(),
            socket_path.to_str(),
            Path::new("/tmp/vt"),
            &spawner,
        )
        .unwrap()
        .unwrap();

        assert!(output.contains("daemon restarted:"));
        assert_eq!(spawner.spawn_count(), 1);
        assert!(crate::daemon::lifecycle::daemon_socket_responds(
            &socket_path
        ));
        spawner.stop();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn command_matches_only_daemon_process_for_socket() {
        let socket = PathBuf::from("/tmp/vde-tmux-501/daemon.sock");

        assert!(command_matches_daemon_socket(
            "/Users/yuki/.cargo/bin/vt daemon --socket /tmp/vde-tmux-501/daemon.sock",
            &socket
        ));
        assert!(command_matches_daemon_socket(
            "/Users/yuki/.cargo/bin/vt daemon --socket=/tmp/vde-tmux-501/daemon.sock",
            &socket
        ));
        assert!(!command_matches_daemon_socket(
            "/Users/yuki/.cargo/bin/vt daemon --socket /tmp/vde-tmux-501/other.sock",
            &socket
        ));
        assert!(!command_matches_daemon_socket(
            "/Users/yuki/.cargo/bin/vt daemon restart --socket /tmp/vde-tmux-501/daemon.sock",
            &socket
        ));
        assert!(!command_matches_daemon_socket(
            "/Users/yuki/.cargo/bin/vt daemon --socket /tmp/vde-tmux-501/daemon.sock restart",
            &socket
        ));
    }

    #[derive(Default)]
    struct ReadyDaemonSpawner {
        spawns: AtomicUsize,
        stop: Arc<AtomicBool>,
        handles: Mutex<Vec<thread::JoinHandle<()>>>,
    }

    impl ReadyDaemonSpawner {
        fn spawn_count(&self) -> usize {
            self.spawns.load(Ordering::SeqCst)
        }

        fn stop(&self) {
            self.stop.store(true, Ordering::SeqCst);
            for handle in self.handles.lock().unwrap().drain(..) {
                handle.join().unwrap();
            }
        }
    }

    impl crate::daemon::lifecycle::DaemonSpawner for ReadyDaemonSpawner {
        fn spawn_detached(&self, exe: &Path, socket: &Path) -> anyhow::Result<()> {
            assert_eq!(exe, Path::new("/tmp/vt"));
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let socket = socket.to_path_buf();
            let stop = self.stop.clone();
            let handle = thread::spawn(move || serve_ready_daemon(socket, stop));
            self.handles.lock().unwrap().push(handle);
            Ok(())
        }
    }

    fn serve_ready_daemon(socket: PathBuf, stop: Arc<AtomicBool>) {
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = String::new();
                    BufReader::new(&mut stream).read_line(&mut request).unwrap();
                    serde_json::to_writer(
                        &mut stream,
                        &ServerMessage::Summary {
                            text: String::new(),
                        },
                    )
                    .unwrap();
                    stream.write_all(b"\n").unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("test daemon accept failed: {error}"),
            }
        }
        drop(listener);
        let _ = std::fs::remove_file(socket);
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

    fn unique_socket_dir() -> PathBuf {
        let counter = SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(format!("/tmp/vt-dmn-{}-{counter}", std::process::id()))
    }
}

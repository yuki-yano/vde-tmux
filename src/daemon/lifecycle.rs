use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::daemon::topology::ServerIdentity;
use crate::tmux::TmuxRunner;

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_START_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxServerIncarnation {
    pub socket_path: PathBuf,
    pub identity: ServerIdentity,
    pub hash: String,
}

impl TmuxServerIncarnation {
    pub fn resolve(runner: &dyn TmuxRunner, env: &BTreeMap<String, String>) -> Result<Self> {
        let tmux = env
            .get("TMUX")
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("TMUX is required to identify the tmux server"))?;
        let socket_path = tmux
            .split(',')
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("TMUX has an invalid server socket path"))?;
        let output = runner.run(&[
            "display-message",
            "-p",
            "#{pid}\t#{start_time}\t#{socket_path}",
        ])?;
        let mut fields = output.trim_end().split('\t');
        let pid = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| anyhow::anyhow!("tmux returned an invalid server PID"))?;
        let start_time = fields
            .next()
            .and_then(|value| value.parse::<i64>().ok())
            .ok_or_else(|| anyhow::anyhow!("tmux returned an invalid server start time"))?;
        let reported_socket = fields
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("tmux returned an invalid server socket path"))?;
        if fields.next().is_some() {
            bail!("tmux returned an invalid server incarnation");
        }
        let socket_path = std::fs::canonicalize(socket_path)
            .with_context(|| format!("failed to canonicalize tmux socket path {socket_path}"))?;
        let reported_socket = std::fs::canonicalize(reported_socket).with_context(|| {
            format!("failed to canonicalize reported tmux socket path {reported_socket}")
        })?;
        if reported_socket != socket_path {
            bail!(
                "tmux runner targets {}, but TMUX identifies {}",
                reported_socket.display(),
                socket_path.display()
            );
        }
        let identity = ServerIdentity { pid, start_time };
        let mut hasher = Sha256::new();
        hasher.update(socket_path.as_os_str().as_bytes());
        hasher.update([0]);
        hasher.update(pid.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(start_time.to_string().as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        Ok(Self {
            socket_path,
            identity,
            hash,
        })
    }

    pub fn verify(&self, runner: &dyn TmuxRunner, env: &BTreeMap<String, String>) -> Result<()> {
        let actual = Self::resolve(runner, env)?;
        if actual != *self {
            bail!(
                "tmux server incarnation mismatch: expected {}, received {}",
                self.hash,
                actual.hash
            );
        }
        Ok(())
    }
}

/// Ensure the daemon socket directory is private and owned by the current user.
///
/// This rejects symlinks and loose permissions, but it is still a best-effort
/// TOCTOU check around normal filesystem operations.
pub fn ensure_secure_socket_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("socket dir must not be a symlink: {}", path.display());
    }
    if !metadata.is_dir() {
        bail!("socket dir is not a directory: {}", path.display());
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        bail!(
            "socket dir owner mismatch for {}: expected uid {}, got {}",
            path.display(),
            uid,
            metadata.uid()
        );
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "insecure socket dir mode for {}: {:o}",
            path.display(),
            mode & 0o777
        );
    }
    Ok(())
}

pub fn probe_v2_daemon(
    socket: &Path,
    expected_server_identity: &str,
) -> Option<crate::daemon::protocol::v2::DaemonPhase> {
    probe_v2_daemon_until(
        socket,
        expected_server_identity,
        Instant::now() + Duration::from_millis(150),
    )
}

fn probe_v2_daemon_until(
    socket: &Path,
    expected_server_identity: &str,
    deadline: Instant,
) -> Option<crate::daemon::protocol::v2::DaemonPhase> {
    if deadline <= Instant::now() {
        return None;
    }
    crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        socket,
        expected_server_identity,
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(150)),
    )
    .ok()
    .map(|client| client.phase())
}

pub fn ensure_daemon_live_v2(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    ensure_daemon_live_v2_until(
        runner,
        env,
        explicit_socket,
        Instant::now() + DAEMON_START_TIMEOUT,
    )
}

pub fn ensure_daemon_live_v2_until(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
    deadline: Instant,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    ensure_deadline_remaining(deadline, "resolving tmux server incarnation")?;
    let incarnation = TmuxServerIncarnation::resolve(runner, env)?;
    ensure_daemon_live_v2_for_incarnation_until(incarnation, env, explicit_socket, deadline)
}

pub fn ensure_daemon_live_v2_for_incarnation_until(
    incarnation: TmuxServerIncarnation,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
    deadline: Instant,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    ensure_deadline_remaining(deadline, "checking daemon liveness")?;
    let socket =
        crate::daemon::daemon_socket_path_for_incarnation(env, explicit_socket, &incarnation.hash);
    if probe_v2_daemon_until(&socket, &incarnation.hash, deadline).is_some() {
        return Ok((incarnation, socket));
    }
    if let Some(parent) = socket.parent().filter(|path| !path.as_os_str().is_empty()) {
        ensure_deadline_remaining(deadline, "creating daemon socket directory")?;
        ensure_secure_socket_dir(parent)?;
    }
    ensure_deadline_remaining(deadline, "acquiring daemon start lock")?;
    let _start_lock = acquire_daemon_start_lock_until(&socket, deadline)?;
    if probe_v2_daemon_until(&socket, &incarnation.hash, deadline).is_some() {
        return Ok((incarnation, socket));
    }
    ensure_deadline_remaining(deadline, "acquiring daemon instance lock")?;
    let stale_guard = try_acquire_daemon_instance_lock(&socket)?;
    if stale_guard.is_none() {
        loop {
            if probe_v2_daemon_until(&socket, &incarnation.hash, deadline).is_some() {
                return Ok((incarnation, socket));
            }
            if Instant::now() >= deadline {
                bail!(
                    "daemon instance lock is held but v2 socket is not responsive before deadline"
                );
            }
            sleep_with_deadline(deadline);
        }
    }
    if socket.exists() {
        ensure_deadline_remaining(deadline, "verifying stale daemon socket")?;
        verify_stale_socket_can_be_removed(&socket, deadline)?;
        ensure_deadline_remaining(deadline, "removing stale daemon socket")?;
        std::fs::remove_file(&socket)
            .with_context(|| format!("failed to remove stale socket {}", socket.display()))?;
    }
    drop(stale_guard);
    ensure_deadline_remaining(deadline, "spawning daemon")?;
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(&exe);
    command
        .arg("daemon")
        .arg("--socket")
        .arg(&socket)
        .arg("--server-identity")
        .arg(&incarnation.hash)
        .arg("--server-pid")
        .arg(incarnation.identity.pid.to_string())
        .arg("--server-start-time")
        .arg(incarnation.identity.start_time.to_string())
        .arg("--tmux-server-socket")
        .arg(&incarnation.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    ensure_deadline_remaining(deadline, "spawning daemon")?;
    command.spawn().with_context(|| {
        format!(
            "failed to spawn v2 daemon {} --socket {}",
            exe.display(),
            socket.display()
        )
    })?;
    loop {
        if probe_v2_daemon_until(&socket, &incarnation.hash, deadline).is_some() {
            return Ok((incarnation, socket));
        }
        if Instant::now() >= deadline {
            bail!(
                "v2 daemon did not become live at {} before the caller deadline ({:?} maximum)",
                socket.display(),
                DAEMON_START_TIMEOUT
            );
        }
        sleep_with_deadline(deadline);
    }
}

pub fn ensure_daemon_serving_v2(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    ensure_daemon_serving_v2_until(runner, env, explicit_socket, deadline)
}

pub fn ensure_daemon_serving_v2_until(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
    deadline: Instant,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    let (incarnation, socket) =
        ensure_daemon_live_v2_until(runner, env, explicit_socket, deadline)?;
    loop {
        if probe_v2_daemon_until(&socket, &incarnation.hash, deadline)
            == Some(crate::daemon::protocol::v2::DaemonPhase::Serving)
        {
            return Ok((incarnation, socket));
        }
        if Instant::now() >= deadline {
            bail!("v2 daemon did not enter Serving before the caller deadline");
        }
        sleep_with_deadline(deadline);
    }
}

#[derive(Debug)]
pub(crate) struct DaemonFileLock {
    file: File,
}

impl Drop for DaemonFileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn acquire_daemon_start_lock_until(socket: &Path, deadline: Instant) -> Result<DaemonFileLock> {
    let path = daemon_lock_path(socket, ".start.lock");
    loop {
        ensure_deadline_remaining(deadline, "acquiring daemon start lock")?;
        if let Some(lock) = try_lock_file(&path)? {
            return Ok(lock);
        }
        sleep_with_deadline(deadline);
    }
}

fn ensure_deadline_remaining(deadline: Instant, stage: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("daemon lifecycle deadline exceeded while {stage}");
    }
    Ok(())
}

fn sleep_with_deadline(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        thread::sleep(remaining.min(DAEMON_START_POLL_INTERVAL));
    }
}

pub(crate) fn try_acquire_daemon_instance_lock(socket: &Path) -> Result<Option<DaemonFileLock>> {
    try_lock_file(&daemon_lock_path(socket, ".lock"))
}

pub(crate) fn try_acquire_writer_lease(namespace: &Path) -> Result<Option<DaemonFileLock>> {
    try_lock_file(&daemon_lock_path(namespace, ".writer.lock"))
}

fn daemon_lock_path(socket: &Path, suffix: &str) -> PathBuf {
    let mut name = socket
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| OsString::from("daemon.sock"));
    name.push(suffix);
    socket.with_file_name(name)
}

pub(crate) fn verify_stale_socket_can_be_removed(socket: &Path, deadline: Instant) -> Result<()> {
    let metadata = std::fs::symlink_metadata(socket)
        .with_context(|| format!("failed to stat stale socket {}", socket.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to remove symlink at daemon socket {}",
            socket.display()
        );
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        bail!(
            "refusing to remove daemon socket {} owned by uid {}",
            socket.display(),
            metadata.uid()
        );
    }
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to remove non-socket daemon path {}",
            socket.display()
        );
    }
    ensure_deadline_remaining(deadline, "checking stale daemon socket owner")?;
    let mut child = Command::new("lsof")
        .args(["-n", "-t", "--"])
        .arg(socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "cannot verify owner process for stale socket {}",
                socket.display()
            )
        })?;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "timed out while verifying owner process for stale socket {}",
                    socket.display()
                );
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).with_context(|| {
                    format!(
                        "cannot verify owner process for stale socket {}",
                        socket.display()
                    )
                });
            }
        }
    }
    let output = child.wait_with_output().with_context(|| {
        format!(
            "cannot collect owner process output for stale socket {}",
            socket.display()
        )
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty()
        || !(output.status.success() || output.status.code() == Some(1) && stdout.trim().is_empty())
    {
        bail!(
            "cannot verify owner process for stale socket {}: {}",
            socket.display(),
            stderr.trim()
        );
    }
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let pid = line.trim().parse::<i32>().map_err(|_| {
            anyhow::anyhow!(
                "cannot parse owner process for stale socket {}",
                socket.display()
            )
        })?;
        // SAFETY: signal 0 performs a process-existence/permission check only.
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            bail!(
                "refusing to remove daemon socket {} owned by live process {}",
                socket.display(),
                pid
            );
        }
    }
    Ok(())
}

fn try_lock_file(path: &Path) -> Result<Option<DaemonFileLock>> {
    let file = open_lock_file(path)?;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != -1 {
            return Ok(Some(DaemonFileLock { file }));
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Ok(None),
            _ => {
                return Err(error).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open lock file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use sha2::{Digest, Sha256};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_dir(label: &str) -> PathBuf {
        let counter = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        PathBuf::from(format!("/tmp/vt-{label}-{}-{counter}", std::process::id()))
    }

    #[test]
    fn tmux_server_incarnation_uses_canonical_socket_pid_and_start_time() {
        let root = unique_dir("incarnation");
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("tmux.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mock = crate::tmux::mock::MockTmuxRunner::new();
        mock.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("321\t654\t{}\n", socket.display()),
        );
        let env = BTreeMap::from([("TMUX".to_string(), format!("{},321,0", socket.display()))]);

        let first = super::TmuxServerIncarnation::resolve(&mock, &env).unwrap();
        assert_eq!(first.socket_path, std::fs::canonicalize(&socket).unwrap());
        assert_eq!(first.identity.pid, 321);
        assert_eq!(first.identity.start_time, 654);
        assert_eq!(first.hash.len(), 64);

        let second_mock = crate::tmux::mock::MockTmuxRunner::new();
        second_mock.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("321\t655\t{}\n", socket.display()),
        );
        let second = super::TmuxServerIncarnation::resolve(&second_mock, &env).unwrap();
        assert_ne!(first.hash, second.hash);
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tmux_server_incarnation_rejects_runner_target_mismatch() {
        let root = unique_dir("incarnation-mismatch");
        std::fs::create_dir_all(&root).unwrap();
        let expected = root.join("expected.sock");
        let actual = root.join("actual.sock");
        let expected_listener = UnixListener::bind(&expected).unwrap();
        let actual_listener = UnixListener::bind(&actual).unwrap();
        let mock = crate::tmux::mock::MockTmuxRunner::new();
        mock.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("321\t654\t{}\n", actual.display()),
        );
        let env = BTreeMap::from([("TMUX".to_string(), format!("{},321,0", expected.display()))]);

        let error = super::TmuxServerIncarnation::resolve(&mock, &env).unwrap_err();
        assert!(error.to_string().contains("runner targets"));
        drop(expected_listener);
        drop(actual_listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v2_probe_treats_installing_hooks_as_live() {
        let root = unique_dir("v2-probe");
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            let hello: crate::daemon::protocol::v2::ClientMessage =
                serde_json::from_str(request.trim()).unwrap();
            assert_eq!(
                hello,
                crate::daemon::protocol::v2::ClientMessage::Hello { proto: 2 }
            );
            serde_json::to_writer(
                &mut stream,
                &crate::daemon::protocol::v2::ServerMessage::HelloAck {
                    proto: 2,
                    daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                        "00112233445566778899aabbccddeeff",
                    )
                    .unwrap(),
                    server_identity: "server-hash".to_string(),
                    phase: crate::daemon::protocol::v2::DaemonPhase::InstallingHooks,
                    hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        assert_eq!(
            super::probe_v2_daemon_until(
                &socket,
                "server-hash",
                Instant::now() + Duration::from_secs(2),
            ),
            Some(crate::daemon::protocol::v2::DaemonPhase::InstallingHooks)
        );
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expired_v2_deadline_does_not_remove_or_spawn() {
        let root = unique_dir("expired-v2");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let base = root.join("daemon.sock");
        let incarnation = super::TmuxServerIncarnation {
            socket_path: root.join("tmux.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 10,
                start_time: 20,
            },
            hash: format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes())),
        };
        let target = crate::daemon::daemon_socket_path_for_incarnation(
            &BTreeMap::new(),
            base.to_str(),
            &incarnation.hash,
        );
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::set_permissions(
            target.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(&target, "sentinel").unwrap();

        let error = super::ensure_daemon_live_v2_for_incarnation_until(
            incarnation,
            &BTreeMap::new(),
            base.to_str(),
            Instant::now() - Duration::from_millis(1),
        )
        .unwrap_err();

        assert!(error.to_string().contains("deadline exceeded"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "sentinel");
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_socket_verification_rejects_regular_file_and_live_owner() {
        let root = unique_dir("stale-verification");
        std::fs::create_dir_all(&root).unwrap();
        let regular = root.join("regular.sock");
        std::fs::write(&regular, "sentinel").unwrap();
        assert!(
            super::verify_stale_socket_can_be_removed(
                &regular,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err()
            .to_string()
            .contains("non-socket")
        );

        let live = root.join("live.sock");
        let listener = UnixListener::bind(&live).unwrap();
        assert!(
            super::verify_stale_socket_can_be_removed(
                &live,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err()
            .to_string()
            .contains("live process")
        );
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_unowned_unix_socket_can_be_removed() {
        let root = unique_dir("stale-unowned");
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("stale.sock");
        drop(UnixListener::bind(&socket).unwrap());

        super::verify_stale_socket_can_be_removed(&socket, Instant::now() + Duration::from_secs(1))
            .unwrap();

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writer_lease_rejects_second_writer_for_same_namespace() {
        let root = unique_dir("writer-lease");
        std::fs::create_dir_all(&root).unwrap();
        let namespace = root.join("server-incarnation");
        let first = super::try_acquire_writer_lease(&namespace).unwrap();
        assert!(first.is_some());
        let second = super::try_acquire_writer_lease(&namespace).unwrap();
        assert!(second.is_none());
        drop(first);
        assert!(
            super::try_acquire_writer_lease(&namespace)
                .unwrap()
                .is_some()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn distinct_server_incarnations_use_independent_socket_and_writer_lease_namespaces() {
        let root = std::env::temp_dir().join(format!(
            "vde-independent-incarnations-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let first_hash = "1".repeat(64);
        let second_hash = "2".repeat(64);
        let first_socket =
            crate::daemon::daemon_socket_path_for_incarnation(&BTreeMap::new(), None, &first_hash);
        let second_socket =
            crate::daemon::daemon_socket_path_for_incarnation(&BTreeMap::new(), None, &second_hash);
        let first_namespace = crate::daemon::writer_lease_namespace(&first_hash);
        let second_namespace = crate::daemon::writer_lease_namespace(&second_hash);
        let first_test_namespace = root.join(
            first_namespace
                .strip_prefix("/")
                .expect("runtime namespace is absolute"),
        );
        let second_test_namespace = root.join(
            second_namespace
                .strip_prefix("/")
                .expect("runtime namespace is absolute"),
        );
        std::fs::create_dir_all(first_test_namespace.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second_test_namespace.parent().unwrap()).unwrap();

        assert_ne!(first_socket, second_socket);
        assert_ne!(first_namespace, second_namespace);
        let first = super::try_acquire_writer_lease(&first_test_namespace)
            .unwrap()
            .expect("first server acquires its writer lease");
        let second = super::try_acquire_writer_lease(&second_test_namespace)
            .unwrap()
            .expect("second server acquires an independent writer lease");
        assert!(
            super::try_acquire_writer_lease(&first_test_namespace)
                .unwrap()
                .is_none()
        );
        assert!(
            super::try_acquire_writer_lease(&second_test_namespace)
                .unwrap()
                .is_none()
        );

        drop((first, second));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ensure_secure_socket_dir_creates_private_directory() {
        let dir = unique_dir("sec");

        super::ensure_secure_socket_dir(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ensure_secure_socket_dir_rejects_world_readable_directory() {
        let dir = unique_dir("insec");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = super::ensure_secure_socket_dir(&dir).unwrap_err();

        assert!(error.to_string().contains("insecure socket dir mode"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

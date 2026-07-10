use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_START_POLL_INTERVAL: Duration = Duration::from_millis(25);

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

pub trait DaemonSpawner {
    fn spawn_detached(&self, exe: &Path, socket: &Path) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct SystemDaemonSpawner;

impl DaemonSpawner for SystemDaemonSpawner {
    fn spawn_detached(&self, exe: &Path, socket: &Path) -> Result<()> {
        let mut command = Command::new(exe);
        command
            .arg("daemon")
            .arg("--socket")
            .arg(socket)
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
        command.spawn().with_context(|| {
            format!(
                "failed to spawn daemon {} --socket {}",
                exe.display(),
                socket.display()
            )
        })?;
        Ok(())
    }
}

pub fn ensure_daemon_started(
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    ensure_daemon_started_with(env, explicit_socket, &exe, &SystemDaemonSpawner)
}

pub fn ensure_daemon_started_with(
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
    exe: &Path,
    spawner: &dyn DaemonSpawner,
) -> Result<()> {
    let socket = crate::daemon::daemon_socket_path(env, explicit_socket);
    if daemon_socket_responds(&socket) {
        return Ok(());
    }
    if let Some(parent) = socket.parent().filter(|path| !path.as_os_str().is_empty()) {
        ensure_secure_socket_dir(parent)?;
    }
    let _start_lock = acquire_daemon_start_lock(&socket)?;
    ensure_daemon_started_with_lock(&socket, exe, spawner)
}

pub(crate) fn ensure_daemon_started_with_lock(
    socket: &Path,
    exe: &Path,
    spawner: &dyn DaemonSpawner,
) -> Result<()> {
    if daemon_socket_responds(socket) {
        return Ok(());
    }
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("failed to remove stale socket {}", socket.display()))?;
    }
    spawner.spawn_detached(exe, socket)?;
    if wait_for_daemon_socket(socket, DAEMON_START_TIMEOUT) {
        return Ok(());
    }
    bail!(
        "daemon did not become ready at {} within {:?}",
        socket.display(),
        DAEMON_START_TIMEOUT
    )
}

pub(crate) fn daemon_socket_responds(socket: &Path) -> bool {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
    if serde_json::to_writer(
        &mut stream,
        &crate::daemon::protocol::ClientMessage::Query {
            proto: 1,
            what: crate::daemon::protocol::QueryTarget::Summary,
        },
    )
    .is_err()
    {
        return false;
    }
    if stream.write_all(b"\n").is_err() {
        return false;
    }
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return false;
    }
    serde_json::from_str::<crate::daemon::protocol::ServerMessage>(line.trim()).is_ok()
}

pub(crate) fn wait_for_daemon_socket(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if daemon_socket_responds(socket) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(DAEMON_START_POLL_INTERVAL);
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

pub(crate) fn acquire_daemon_start_lock(socket: &Path) -> Result<DaemonFileLock> {
    lock_file_blocking(&daemon_lock_path(socket, ".start.lock"))
}

pub(crate) fn try_acquire_daemon_instance_lock(socket: &Path) -> Result<Option<DaemonFileLock>> {
    try_lock_file(&daemon_lock_path(socket, ".lock"))
}

#[allow(dead_code)] // Connected when the v2 daemon becomes the production entrypoint in Phase 6.
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

fn lock_file_blocking(path: &Path) -> Result<DaemonFileLock> {
    let file = open_lock_file(path)?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to lock {}", path.display()));
    }
    Ok(DaemonFileLock { file })
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
    use std::io::{BufRead, BufReader, ErrorKind, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Duration;

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_dir(label: &str) -> PathBuf {
        let counter = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        PathBuf::from(format!("/tmp/vt-{label}-{}-{counter}", std::process::id()))
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

    struct ReadyDaemonSpawner {
        delay: Duration,
        spawns: AtomicUsize,
        stop: Arc<AtomicBool>,
        handles: Mutex<Vec<thread::JoinHandle<()>>>,
    }

    impl ReadyDaemonSpawner {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                spawns: AtomicUsize::new(0),
                stop: Arc::new(AtomicBool::new(false)),
                handles: Mutex::new(Vec::new()),
            }
        }

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

    impl super::DaemonSpawner for ReadyDaemonSpawner {
        fn spawn_detached(&self, exe: &Path, socket: &Path) -> anyhow::Result<()> {
            assert_eq!(exe, Path::new("/tmp/vt"));
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let socket = socket.to_path_buf();
            let delay = self.delay;
            let stop = self.stop.clone();
            let handle = thread::spawn(move || {
                thread::sleep(delay);
                serve_ready_daemon(socket, stop);
            });
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
                    stream.set_nonblocking(false).unwrap();
                    let mut request = String::new();
                    BufReader::new(&mut stream).read_line(&mut request).unwrap();
                    serde_json::to_writer(
                        &mut stream,
                        &crate::daemon::protocol::ServerMessage::Summary {
                            text: String::new(),
                        },
                    )
                    .unwrap();
                    stream.write_all(b"\n").unwrap();
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("test daemon accept failed: {error}"),
            }
        }
        drop(listener);
        let _ = std::fs::remove_file(socket);
    }

    #[test]
    fn ensure_daemon_started_removes_stale_socket_before_spawn() {
        let dir = unique_dir("stale");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("daemon.sock");
        std::fs::write(&socket, "stale").unwrap();
        let spawner = ReadyDaemonSpawner::new(Duration::ZERO);

        super::ensure_daemon_started_with(
            &BTreeMap::new(),
            Some(socket.display().to_string().as_str()),
            &PathBuf::from("/tmp/vt"),
            &spawner,
        )
        .unwrap();

        assert_eq!(spawner.spawn_count(), 1);
        spawner.stop();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ensure_daemon_started_serializes_concurrent_spawns() {
        let dir = unique_dir("lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("daemon.sock");
        let socket_arg = Arc::new(socket.display().to_string());
        let spawner = Arc::new(ReadyDaemonSpawner::new(Duration::from_millis(75)));
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let socket_arg = socket_arg.clone();
            let spawner = spawner.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                super::ensure_daemon_started_with(
                    &BTreeMap::new(),
                    Some(socket_arg.as_str()),
                    &PathBuf::from("/tmp/vt"),
                    spawner.as_ref(),
                )
                .unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(spawner.spawn_count(), 1);
        spawner.stop();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

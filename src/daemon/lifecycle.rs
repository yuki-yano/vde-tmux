//! daemon lifecycle と socket directory 検証。

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

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
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("failed to remove stale socket {}", socket.display()))?;
    }
    if let Some(parent) = socket.parent().filter(|path| !path.as_os_str().is_empty()) {
        ensure_secure_socket_dir(parent)?;
    }
    spawner.spawn_detached(exe, &socket)
}

fn daemon_socket_responds(socket: &Path) -> bool {
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
            what: crate::daemon::protocol::QueryTarget::Statusline,
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[test]
    fn ensure_secure_socket_dir_creates_private_directory() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-secure-dir-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        super::ensure_secure_socket_dir(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ensure_secure_socket_dir_rejects_world_readable_directory() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-insecure-dir-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = super::ensure_secure_socket_dir(&dir).unwrap_err();

        assert!(error.to_string().contains("insecure socket dir mode"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[derive(Default)]
    struct MockSpawner {
        spawns: Mutex<Vec<Vec<String>>>,
    }

    impl super::DaemonSpawner for MockSpawner {
        fn spawn_detached(&self, exe: &Path, socket: &Path) -> anyhow::Result<()> {
            self.spawns.lock().unwrap().push(vec![
                exe.display().to_string(),
                "daemon".to_string(),
                "--socket".to_string(),
                socket.display().to_string(),
            ]);
            Ok(())
        }
    }

    #[test]
    fn ensure_daemon_started_removes_stale_socket_before_spawn() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-stale-socket-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("daemon.sock");
        std::fs::write(&socket, "stale").unwrap();
        let spawner = MockSpawner::default();

        super::ensure_daemon_started_with(
            &BTreeMap::new(),
            Some(socket.display().to_string().as_str()),
            &PathBuf::from("/tmp/vt"),
            &spawner,
        )
        .unwrap();

        assert!(!socket.exists());
        assert_eq!(spawner.spawns.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

use std::collections::BTreeMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pane_state::PaneInstance;

const MAX_CONTROL_FRAME_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlMessage {
    Input {
        key: String,
    },
    Focus {
        pane_instance: PaneInstance,
        session_id: String,
    },
}

pub struct ControlListener {
    socket: UnixDatagram,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl ControlListener {
    pub fn bind(server_identity: &str, sidebar: &PaneInstance) -> Result<Self> {
        sidebar.validate().map_err(anyhow::Error::msg)?;
        let path = control_socket_path(server_identity, sidebar)?;
        let root = path.parent().expect("control socket path has a parent");
        ensure_secure_socket_dir(root)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() }
                {
                    bail!(
                        "refusing to replace insecure control socket {}",
                        path.display()
                    );
                }
                std::fs::remove_file(&path)
                    .with_context(|| format!("failed to remove stale {}", path.display()))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let socket = UnixDatagram::bind(&path)
            .with_context(|| format!("failed to bind {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure {}", path.display()))?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
        {
            let _ = std::fs::remove_file(&path);
            bail!(
                "control socket security verification failed: {}",
                path.display()
            );
        }
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    pub fn try_recv(&self) -> Result<Option<ControlMessage>> {
        let mut frame = [0_u8; MAX_CONTROL_FRAME_BYTES];
        match self.socket.recv(&mut frame) {
            Ok(length) => {
                let message = serde_json::from_slice(&frame[..length])
                    .context("invalid sidebar control frame")?;
                Ok(Some(message))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).context("failed to read sidebar control socket"),
        }
    }
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        let matches_bound_socket =
            std::fs::symlink_metadata(&self.path)
                .ok()
                .is_some_and(|metadata| {
                    metadata.file_type().is_socket()
                        && metadata.dev() == self.device
                        && metadata.ino() == self.inode
                });
        if matches_bound_socket {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn send(server_identity: &str, sidebar: &PaneInstance, message: &ControlMessage) -> Result<()> {
    let frame = serde_json::to_vec(message)?;
    if frame.len() > MAX_CONTROL_FRAME_BYTES {
        bail!("sidebar control frame is too large");
    }
    let path = control_socket_path(server_identity, sidebar)?;
    let socket = UnixDatagram::unbound()?;
    socket
        .send_to(&frame, &path)
        .with_context(|| format!("failed to send sidebar control frame to {}", path.display()))?;
    Ok(())
}

pub fn control_socket_path(server_identity: &str, sidebar: &PaneInstance) -> Result<PathBuf> {
    if server_identity.is_empty()
        || !server_identity
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid daemon server identity");
    }
    sidebar.validate().map_err(anyhow::Error::msg)?;
    let identity = serde_json::to_vec(&(server_identity, sidebar))?;
    let digest = format!("{:x}", Sha256::digest(identity));
    Ok(runtime_root().join(format!("{}.sock", &digest[..24])))
}

pub fn resolve_current_pane_instance(
    runner: &dyn crate::tmux::TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<PaneInstance> {
    let target = env
        .get("TMUX_PANE")
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty());
    let raw = match target {
        Some(target) => runner.run(&[
            "display-message",
            "-p",
            "-t",
            target,
            "-F",
            "#{pane_id}\u{1f}#{pane_pid}",
        ])?,
        None => runner.run(&["display-message", "-p", "-F", "#{pane_id}\u{1f}#{pane_pid}"])?,
    };
    parse_pane_instance(raw.trim())
}

pub fn resolve_pane_instance(
    runner: &dyn crate::tmux::TmuxRunner,
    target: &str,
) -> Result<PaneInstance> {
    let raw = runner.run(&[
        "display-message",
        "-p",
        "-t",
        target,
        "-F",
        "#{pane_id}\u{1f}#{pane_pid}",
    ])?;
    parse_pane_instance(raw.trim())
}

pub fn parse_pane_instance(raw: &str) -> Result<PaneInstance> {
    let (pane_id, pane_pid) = raw
        .split_once('\u{1f}')
        .context("tmux pane instance is missing fields")?;
    let pane = PaneInstance {
        pane_id: pane_id.to_string(),
        pane_pid: pane_pid.parse().context("invalid tmux pane PID")?,
    };
    pane.validate().map_err(anyhow::Error::msg)?;
    Ok(pane)
}

fn runtime_root() -> PathBuf {
    Path::new(&format!("/tmp/vt-{}/v2/sidebar-control", unsafe {
        libc::geteuid()
    }))
    .to_path_buf()
}

fn ensure_secure_socket_dir(path: &Path) -> Result<()> {
    crate::runtime_dir::ensure_secure_runtime_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn socket_path_includes_full_sidebar_instance() {
        let path = control_socket_path(
            "server_1",
            &PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 42,
            },
        )
        .unwrap();
        assert!(
            path.extension()
                .is_some_and(|extension| extension == "sock")
        );
        assert!(path.as_os_str().len() < 100);
    }

    #[test]
    fn control_socket_roundtrips_focus_identity() {
        let sidebar = PaneInstance {
            pane_id: "%987654".to_string(),
            pane_pid: std::process::id(),
        };
        let listener = ControlListener::bind("test_server", &sidebar).unwrap();
        let message = ControlMessage::Focus {
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 7,
            },
            session_id: "$1".to_string(),
        };
        send("test_server", &sidebar, &message).unwrap();
        assert_eq!(listener.try_recv().unwrap(), Some(message));
    }

    #[test]
    fn secure_socket_dir_tightens_loose_mode_and_rejects_symlink() {
        let loose = unique_path("vt-sidebar-loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_secure_socket_dir(&loose).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&loose)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir(&loose).unwrap();

        let target = unique_path("vt-sidebar-target");
        let link = unique_path("vt-sidebar-link");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(ensure_secure_socket_dir(&link).is_err());
        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir(target).unwrap();
    }

    #[test]
    fn listener_drop_does_not_unlink_replaced_socket_inode() {
        let sidebar = PaneInstance {
            pane_id: "%987653".to_string(),
            pane_pid: std::process::id(),
        };
        let identity = format!("inode_test_{}", std::process::id());
        let listener = ControlListener::bind(&identity, &sidebar).unwrap();
        let path = control_socket_path(&identity, &sidebar).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixDatagram::bind(&path).unwrap();

        drop(listener);

        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_socket()
        );
        drop(replacement);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn listener_rejects_preexisting_symlink() {
        let sidebar = PaneInstance {
            pane_id: "%987652".to_string(),
            pane_pid: std::process::id(),
        };
        let identity = format!("symlink_test_{}", std::process::id());
        let path = control_socket_path(&identity, &sidebar).unwrap();
        ensure_secure_socket_dir(path.parent().unwrap()).unwrap();
        let target = unique_path("vt-sidebar-socket-target");
        std::fs::write(&target, b"not a socket").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        assert!(ControlListener::bind(&identity, &sidebar).is_err());

        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(target).unwrap();
    }
}

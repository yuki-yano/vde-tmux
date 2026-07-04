//! daemon lifecycle と socket directory 検証。

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};

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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

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
}

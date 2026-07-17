//! Secure creation and validation of the per-user runtime directory tree.
//!
//! Runtime sockets live under `/tmp/vt-<euid>`, and `/tmp` is world-writable.
//! Validating only the leaf directory leaves the shared `/tmp/vt-<euid>` root
//! open to a pre-creation/DoS attack by another local user. Every directory
//! from the runtime root down to the leaf is therefore verified: a directory
//! owned by another user or reached through a symlink is rejected, while a
//! directory we own that an older version left group/other-accessible is
//! tightened back to 0700.

use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The per-user runtime root under `/tmp`.
pub fn per_user_runtime_root() -> PathBuf {
    PathBuf::from(format!("/tmp/vt-{}", unsafe { libc::geteuid() }))
}

/// Create `leaf` (which must be `root` itself or a descendant of `root`) and
/// verify that `root` and every intermediate directory down to `leaf` is a
/// non-symlink directory owned by the current euid with mode `0700`.
///
/// This is a best-effort TOCTOU check: each level is created and validated in
/// order so an attacker-owned ancestor is rejected before descending into it.
pub fn ensure_secure_dir_chain(root: &Path, leaf: &Path) -> Result<()> {
    let rel = leaf
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", leaf.display(), root.display()))?;
    create_and_verify_dir(root)?;
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        create_and_verify_dir(&current)?;
    }
    Ok(())
}

/// Validate `path` as a private runtime directory.
///
/// When `path` lives under the shared `/tmp/vt-<euid>` root, the whole chain
/// from that root down is validated so an attacker-owned ancestor is rejected.
/// Otherwise (e.g. a state directory under `$XDG_STATE_HOME`, or a test
/// temporary directory) only `path` itself is created and validated.
pub fn ensure_secure_runtime_dir(path: &Path) -> Result<()> {
    let root = per_user_runtime_root();
    if path.starts_with(&root) {
        ensure_secure_dir_chain(&root, path)
    } else {
        ensure_secure_dir_chain(path, path)
    }
}

fn create_and_verify_dir(path: &Path) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to create {}", path.display()));
        }
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("runtime dir must not be a symlink: {}", path.display());
    }
    if !metadata.is_dir() {
        bail!("runtime path is not a directory: {}", path.display());
    }
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        // A directory owned by another user under world-writable /tmp is an
        // attacker pre-creating our runtime path. Never adopt it.
        bail!(
            "runtime dir owner mismatch for {}: expected uid {}, got {}",
            path.display(),
            euid,
            metadata.uid()
        );
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        // We own it; tighten permissions left loose by an older version or the
        // umask. The per-level ownership check above still guards each child, so
        // adopting our own directory here does not widen the trust boundary.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique_root() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("vt-runtime-dir-test-{}-{}", std::process::id(), n))
    }

    #[test]
    fn creates_full_chain_with_private_mode() {
        let root = unique_root();
        let leaf = root.join("v2").join("sidebar-control");
        ensure_secure_dir_chain(&root, &leaf).unwrap();
        for dir in [&root, &root.join("v2"), &leaf] {
            let meta = std::fs::symlink_metadata(dir).unwrap();
            assert!(meta.is_dir());
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_symlinked_root() {
        let root = unique_root();
        let target = unique_root();
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, &root).unwrap();
        let leaf = root.join("v2");
        let result = ensure_secure_dir_chain(&root, &leaf);
        assert!(result.is_err(), "symlinked root must be rejected");
        let _ = std::fs::remove_file(&root);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn tightens_loose_permissions_on_owned_directory() {
        // An owned but world-traversable dir left by an older version must be
        // tightened to 0700 rather than rejected, so upgrades keep working.
        let root = unique_root();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let leaf = root.join("v2");
        ensure_secure_dir_chain(&root, &leaf).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&root)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

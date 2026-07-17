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

/// Outcome of validating one existing directory in the chain against the
/// current euid. Kept as a pure decision so every branch is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum DirVerdict {
    Ok,
    Tighten,
    Reject(&'static str),
}

fn classify_dir(is_symlink: bool, is_dir: bool, uid: u32, mode: u32, euid: u32) -> DirVerdict {
    if is_symlink {
        return DirVerdict::Reject("runtime dir must not be a symlink");
    }
    if !is_dir {
        return DirVerdict::Reject("runtime path is not a directory");
    }
    if uid != euid {
        // A directory owned by another user under world-writable /tmp is an
        // attacker pre-creating our runtime path. Never adopt it.
        return DirVerdict::Reject("runtime dir owner mismatch");
    }
    if mode & 0o777 != 0o700 {
        // We own it; tighten permissions left loose by an older version or the
        // umask. The per-level ownership check above still guards each child, so
        // adopting our own directory here does not widen the trust boundary.
        return DirVerdict::Tighten;
    }
    DirVerdict::Ok
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
    let euid = unsafe { libc::geteuid() };
    match classify_dir(
        metadata.file_type().is_symlink(),
        metadata.is_dir(),
        metadata.uid(),
        metadata.permissions().mode(),
        euid,
    ) {
        DirVerdict::Ok => {}
        DirVerdict::Tighten => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to secure {}", path.display()))?;
        }
        DirVerdict::Reject(reason) => bail!("{reason}: {}", path.display()),
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
    fn classify_dir_covers_every_branch() {
        let euid = 1000;
        assert_eq!(
            classify_dir(true, true, euid, 0o700, euid),
            DirVerdict::Reject("runtime dir must not be a symlink")
        );
        assert_eq!(
            classify_dir(false, false, euid, 0o700, euid),
            DirVerdict::Reject("runtime path is not a directory")
        );
        assert_eq!(
            classify_dir(false, true, euid + 1, 0o700, euid),
            DirVerdict::Reject("runtime dir owner mismatch")
        );
        assert_eq!(
            classify_dir(false, true, euid, 0o755, euid),
            DirVerdict::Tighten
        );
        assert_eq!(classify_dir(false, true, euid, 0o700, euid), DirVerdict::Ok);
    }

    #[test]
    fn rejects_symlinked_intermediate() {
        let root = unique_root();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = unique_root();
        std::fs::create_dir_all(&target).unwrap();
        // root/v2 is a symlink instead of a real directory.
        symlink(&target, root.join("v2")).unwrap();
        let leaf = root.join("v2").join("sidebar-control");
        assert!(ensure_secure_dir_chain(&root, &leaf).is_err());
        let _ = std::fs::remove_file(root.join("v2"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn rejects_regular_file_in_chain() {
        let root = unique_root();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.join("v2"), b"not a dir").unwrap();
        let leaf = root.join("v2");
        assert!(ensure_secure_dir_chain(&root, &leaf).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reuses_existing_valid_chain_idempotently() {
        let root = unique_root();
        let leaf = root.join("v2").join("sidebar-control");
        ensure_secure_dir_chain(&root, &leaf).unwrap();
        // A second call over the already-created chain must still succeed.
        ensure_secure_dir_chain(&root, &leaf).unwrap();
        let _ = std::fs::remove_dir_all(&root);
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

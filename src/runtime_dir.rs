//! Secure creation and validation of the per-user runtime directory tree.
//!
//! Runtime sockets live under `/tmp/vt-<euid>`, and `/tmp` is world-writable.
//! Validating only the leaf directory leaves the shared `/tmp/vt-<euid>` root
//! open to a pre-creation/DoS attack by another local user. Every directory
//! from the runtime root down to the leaf is therefore verified: a directory
//! owned by another user or reached through a symlink is rejected, while a
//! directory we own that an older version left group/other-accessible is
//! tightened back to 0700.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::daemon::topology::ServerIdentity;

const PANE_DISPATCH_LOCK_DOMAIN: &[u8] = b"vde-tmux:guarded-pane-dispatch-lock:v1\0";
const PANE_DISPATCH_LOCK_DIR: &str = "guarded-pane-dispatch-locks-v1";

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

/// Try to acquire the process-wide dispatch lease for one exact pane instance.
///
/// The lease is nonblocking. `Ok(None)` means another dispatch currently owns
/// the same server/pane/PID tuple. Dropping the returned guard releases it.
pub fn try_acquire_pane_dispatch_lock(
    server_identity: &ServerIdentity,
    pane_id: &str,
    pane_pid: u32,
) -> Result<Option<PaneDispatchLock>> {
    let directory = per_user_runtime_root().join(PANE_DISPATCH_LOCK_DIR);
    try_acquire_pane_dispatch_lock_in(&directory, server_identity, pane_id, pane_pid)
}

#[derive(Debug)]
pub struct PaneDispatchLock {
    file: File,
}

impl Drop for PaneDispatchLock {
    fn drop(&mut self) {
        // SAFETY: `file` owns a valid descriptor until after this Drop returns.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn try_acquire_pane_dispatch_lock_in(
    directory: &Path,
    server_identity: &ServerIdentity,
    pane_id: &str,
    pane_pid: u32,
) -> Result<Option<PaneDispatchLock>> {
    ensure_secure_runtime_dir(directory)?;
    let path = pane_dispatch_lock_path(directory, server_identity, pane_id, pane_pid);
    let file = open_private_lock_file(&path)?;
    try_lock_nonblocking(file, &path)
}

fn pane_dispatch_lock_path(
    directory: &Path,
    server_identity: &ServerIdentity,
    pane_id: &str,
    pane_pid: u32,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(PANE_DISPATCH_LOCK_DOMAIN);
    hash_field(
        &mut hasher,
        b"server-pid",
        &server_identity.pid.to_be_bytes(),
    );
    hash_field(
        &mut hasher,
        b"server-start-time",
        &server_identity.start_time.to_be_bytes(),
    );
    hash_field(&mut hasher, b"pane-id", pane_id.as_bytes());
    hash_field(&mut hasher, b"pane-pid", &pane_pid.to_be_bytes());
    directory.join(format!("{:x}.lock", hasher.finalize()))
}

fn hash_field(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_be_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn open_private_lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open pane dispatch lock {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat pane dispatch lock {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "pane dispatch lock is not a regular file: {}",
            path.display()
        );
    }
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        bail!(
            "pane dispatch lock owner mismatch for {}: expected uid {}, got {}",
            path.display(),
            euid,
            metadata.uid()
        );
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        bail!(
            "pane dispatch lock mode mismatch for {}: expected 600, got {:o}",
            path.display(),
            mode
        );
    }
    Ok(file)
}

fn try_lock_nonblocking(file: File, path: &Path) -> Result<Option<PaneDispatchLock>> {
    loop {
        // SAFETY: `file` owns a valid descriptor for the duration of this call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != -1 {
            return Ok(Some(PaneDispatchLock { file }));
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            ErrorKind::Interrupted => continue,
            ErrorKind::WouldBlock => return Ok(None),
            _ => {
                return Err(error).with_context(|| {
                    format!("failed to lock pane dispatch lock {}", path.display())
                });
            }
        }
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

    fn server_identity() -> ServerIdentity {
        ServerIdentity {
            pid: 1234,
            start_time: 5678,
        }
    }

    #[test]
    fn pane_dispatch_lock_is_nonblocking_and_released_on_drop() {
        let directory = unique_root();
        let first = try_acquire_pane_dispatch_lock_in(&directory, &server_identity(), "%7", 9001)
            .unwrap()
            .expect("first acquisition");
        assert!(
            try_acquire_pane_dispatch_lock_in(&directory, &server_identity(), "%7", 9001)
                .unwrap()
                .is_none()
        );

        drop(first);
        assert!(
            try_acquire_pane_dispatch_lock_in(&directory, &server_identity(), "%7", 9001)
                .unwrap()
                .is_some()
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn pane_dispatch_lock_key_is_hashed_and_separates_every_identity_field() {
        let directory = unique_root();
        let identity = server_identity();
        let base = pane_dispatch_lock_path(&directory, &identity, "%7/../../raw", 9001);
        let other_server = pane_dispatch_lock_path(
            &directory,
            &ServerIdentity {
                pid: identity.pid,
                start_time: identity.start_time + 1,
            },
            "%7/../../raw",
            9001,
        );
        let other_pane = pane_dispatch_lock_path(&directory, &identity, "%8", 9001);
        let other_pid = pane_dispatch_lock_path(&directory, &identity, "%7/../../raw", 9002);

        let file_name = base.file_name().unwrap().to_string_lossy();
        assert_eq!(file_name.len(), 64 + ".lock".len());
        assert!(file_name.ends_with(".lock"));
        assert!(!file_name.contains("raw"));
        assert_ne!(base, other_server);
        assert_ne!(base, other_pane);
        assert_ne!(base, other_pid);
    }

    #[test]
    fn pane_dispatch_lock_file_is_private_regular_and_rejects_symlinks() {
        let directory = unique_root();
        let identity = server_identity();
        let guard = try_acquire_pane_dispatch_lock_in(&directory, &identity, "%1", 42)
            .unwrap()
            .unwrap();
        let path = pane_dispatch_lock_path(&directory, &identity, "%1", 42);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        drop(guard);

        let symlink_path = pane_dispatch_lock_path(&directory, &identity, "%2", 43);
        symlink(&path, &symlink_path).unwrap();
        assert!(
            try_acquire_pane_dispatch_lock_in(&directory, &identity, "%2", 43).is_err(),
            "O_NOFOLLOW must reject a lock-file symlink"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn pane_dispatch_lock_rejects_an_existing_nonprivate_file() {
        let directory = unique_root();
        ensure_secure_runtime_dir(&directory).unwrap();
        let identity = server_identity();
        let path = pane_dispatch_lock_path(&directory, &identity, "%3", 44);
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            try_acquire_pane_dispatch_lock_in(&directory, &identity, "%3", 44).is_err(),
            "an existing lock file must already be mode 0600"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}

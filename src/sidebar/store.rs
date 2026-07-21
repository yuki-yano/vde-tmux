use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::sidebar::state::SidebarPreferences;

pub fn encode_state(state: &SidebarPreferences) -> Result<String> {
    Ok(serde_json::to_string_pretty(state)?)
}

pub fn decode_state(raw: &str) -> Result<SidebarPreferences> {
    let state = serde_json::from_str::<SidebarPreferences>(raw)?;
    state.validate().map_err(anyhow::Error::msg)?;
    Ok(state)
}

pub fn load_state(path: &std::path::Path) -> Result<SidebarPreferences> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_state_file(path, &metadata)?;
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => decode_state(&raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SidebarPreferences::default())
        }
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save_state(path: &std::path::Path, state: &SidebarPreferences) -> Result<()> {
    ensure_secure_state_parent(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_state_file(path, &metadata)?;
    }
    save_encoded_state(path, encode_state(state)?)
}

fn save_encoded_state(path: &Path, encoded: String) -> Result<()> {
    let temp_path = temporary_state_path(path)?;
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        file.write_all(encoded.as_bytes())
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temp_path.display()))?;
        drop(file);
        std::fs::rename(&temp_path, path).with_context(|| {
            format!(
                "failed to replace {} with {}",
                path.display(),
                temp_path.display()
            )
        })?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::File::open(parent)
            .with_context(|| format!("failed to open {} for sync", parent.display()))?
            .sync_all()
            .with_context(|| format!("failed to sync {}", parent.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result?;
    Ok(())
}

fn temporary_state_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("state path has no file name: {}", path.display()))?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(format!(".{file_name}.tmp.{}.{}", std::process::id(), stamp)))
}

pub fn state_path(env: &BTreeMap<String, String>, socket_path: &Path) -> PathBuf {
    let base = if let Some(state_home) = env
        .get("XDG_STATE_HOME")
        .filter(|value| !value.trim().is_empty())
    {
        PathBuf::from(state_home).join("vde/tmux/sidebar-state")
    } else if let Some(home) = env.get("HOME").filter(|value| !value.trim().is_empty()) {
        PathBuf::from(home).join(".local/state/vde/tmux/sidebar-state")
    } else {
        PathBuf::from(format!("/tmp/vt-{}/sidebar-state", unsafe {
            libc::geteuid()
        }))
    };
    let mut hasher = Sha256::new();
    hasher.update(socket_path.as_os_str().as_encoded_bytes());
    base.join(format!("sidebar-preferences-{:x}.json", hasher.finalize()))
}

fn ensure_secure_state_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let runtime_root = crate::runtime_dir::per_user_runtime_root();
    if parent.starts_with(&runtime_root) {
        // The world-writable /tmp fallback needs the shared root validated too,
        // not just the sidebar-state leaf.
        return crate::runtime_dir::ensure_secure_dir_chain(&runtime_root, parent);
    }
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) => validate_private_state_dir(parent, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let ancestor = parent.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(ancestor)
                .with_context(|| format!("failed to create {}", ancestor.display()))?;
            match std::fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", parent.display()));
                }
            }
            validate_private_state_dir(parent, &std::fs::symlink_metadata(parent)?)
        }
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", parent.display())),
    }
}

fn validate_private_state_dir(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(anyhow!(
            "insecure sidebar state directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_private_state_file(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(anyhow!("insecure sidebar state file: {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::state::{RepoId, SidebarPreferences};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn state_json_roundtrips() {
        let state = SidebarPreferences {
            manual_order: vec![RepoId::new("misc", "app")],
            ..SidebarPreferences::default()
        };

        let json = encode_state(&state).unwrap();
        let decoded = decode_state(&json).unwrap();

        assert_eq!(decoded, state);
    }

    #[test]
    fn state_json_rejects_unknown_fields_and_schema_versions() {
        let unknown = r#"{"schema_version":1,"unknown":true}"#;
        let unsupported = r#"{"schema_version":2}"#;

        assert!(decode_state(unknown).is_err());
        assert!(decode_state(unsupported).is_err());
    }

    #[test]
    fn state_path_prefers_xdg_state_home() {
        let env = std::collections::BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            "/tmp/state".to_string(),
        )]);

        let first = state_path(&env, Path::new("/tmp/tmux-501/default"));
        let second = state_path(&env, Path::new("/tmp/tmux-501/other"));

        assert_eq!(
            first.parent(),
            Some(Path::new("/tmp/state/vde/tmux/sidebar-state"))
        );
        assert_ne!(first, second);
        assert_eq!(first, state_path(&env, Path::new("/tmp/tmux-501/default")));
    }

    #[test]
    fn state_path_uses_private_uid_scoped_tmp_fallback() {
        let path = state_path(&BTreeMap::new(), Path::new("/tmp/tmux/default"));

        assert_eq!(
            path.parent(),
            Some(Path::new(&format!("/tmp/vt-{}/sidebar-state", unsafe {
                libc::geteuid()
            })))
        );
    }

    #[test]
    fn save_and_load_state_roundtrips_file() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-state-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("sidebar-order-v1.json");
        let state = SidebarPreferences {
            manual_chat_order: vec!["%1".to_string()],
            ..SidebarPreferences::default()
        };

        save_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();

        assert_eq!(loaded, state);
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn socket_namespaces_persist_independently_and_survive_reload() {
        let root = std::env::temp_dir().join(format!(
            "vde-tmux-socket-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let first_path = state_path(&env, Path::new("/tmp/tmux/first"));
        let second_path = state_path(&env, Path::new("/tmp/tmux/second"));
        let first = SidebarPreferences {
            filter: crate::sidebar::state::StatusFilter::DoneOnly,
            ..SidebarPreferences::default()
        };

        save_state(&first_path, &first).unwrap();

        assert_eq!(load_state(&first_path).unwrap(), first);
        assert_eq!(
            load_state(&second_path).unwrap(),
            SidebarPreferences::default()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn state_store_uses_a_private_child_below_a_shared_state_directory() {
        let root = std::env::temp_dir().join(format!(
            "vde-tmux-shared-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let shared = root.join("vde/tmux");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        let path = state_path(&env, Path::new("/tmp/tmux/default"));

        save_state(&path, &SidebarPreferences::default()).unwrap();

        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn state_storage_rejects_insecure_parent_and_file_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "vde-tmux-state-security-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();

        let loose = root.join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(save_state(&loose.join("state.json"), &SidebarPreferences::default()).is_err());

        let target = root.join("target");
        let parent_link = root.join("parent-link");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, &parent_link).unwrap();
        assert!(
            save_state(
                &parent_link.join("state.json"),
                &SidebarPreferences::default()
            )
            .is_err()
        );

        let private = root.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target_file = root.join("target-file");
        let state_link = private.join("state.json");
        std::fs::write(&target_file, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&target_file, &state_link).unwrap();
        assert!(save_state(&state_link, &SidebarPreferences::default()).is_err());
        assert_eq!(std::fs::read(&target_file).unwrap(), b"unchanged");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_state_replaces_file_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-state-atomic-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("sidebar-order-v1.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before_inode = std::fs::metadata(&path).unwrap().ino();

        save_state(&path, &SidebarPreferences::default()).unwrap();

        let after_inode = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(after_inode, before_inode);
        assert_eq!(load_state(&path).unwrap(), SidebarPreferences::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preference_write_latency_metric() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-preference-latency-current-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("sidebar-preferences.json");
        let mut micros = Vec::new();
        for index in 0..50 {
            let state = SidebarPreferences {
                manual_chat_order: vec![format!("%{index}")],
                ..SidebarPreferences::default()
            };
            let started = std::time::Instant::now();
            save_state(&path, &state).unwrap();
            micros.push(started.elapsed().as_micros());
        }
        micros.sort_unstable();
        let p95 = micros[47];
        let max = *micros.last().unwrap();
        eprintln!("preference_write_current n=50 p95={p95}us max={max}us");
        assert!(
            p95 <= 250_000,
            "preference write p95 exceeded 250ms: {p95}us"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_missing_state_returns_default() {
        let path = std::env::temp_dir().join(format!(
            "vde-tmux-missing-state-test-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let loaded = load_state(&path).unwrap();

        assert_eq!(loaded, SidebarPreferences::default());
    }
}

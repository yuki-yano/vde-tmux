use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::sidebar::state::{SidebarExpansionPreferences, SidebarOrderPreferences};

#[derive(Debug)]
pub enum OrderUpdateError {
    Busy,
    Stale { current_version: u64 },
    Storage(anyhow::Error),
}

impl std::fmt::Display for OrderUpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("sidebar order is locked by another process"),
            Self::Stale { current_version } => {
                write!(
                    formatter,
                    "sidebar order version is stale: current {current_version}"
                )
            }
            Self::Storage(error) => {
                write!(formatter, "sidebar order persistence failed: {error:#}")
            }
        }
    }
}

impl std::error::Error for OrderUpdateError {}

pub fn compare_and_swap_order(
    path: &Path,
    expected_version: u64,
    manual_order: Vec<crate::sidebar::state::RepoId>,
    manual_chat_order: Vec<String>,
) -> std::result::Result<SidebarOrderPreferences, OrderUpdateError> {
    ensure_secure_state_parent(path).map_err(OrderUpdateError::Storage)?;
    let _lock = crate::daemon::lifecycle::try_acquire_writer_lease(path)
        .map_err(OrderUpdateError::Storage)?
        .ok_or(OrderUpdateError::Busy)?;
    let mut current = load_state(path).map_err(OrderUpdateError::Storage)?;
    if current.version != expected_version {
        return Err(OrderUpdateError::Stale {
            current_version: current.version,
        });
    }
    let changed = current
        .replace_manual_order(expected_version, manual_order, manual_chat_order)
        .map_err(|current_version| OrderUpdateError::Stale { current_version })?;
    if changed {
        save_state(path, &current).map_err(OrderUpdateError::Storage)?;
    }
    Ok(current)
}

pub fn compare_and_swap_view_preferences(
    path: &Path,
    expected_version: u64,
    view_mode: crate::sidebar::state::ViewMode,
    filter: crate::sidebar::state::StatusFilter,
) -> std::result::Result<SidebarOrderPreferences, OrderUpdateError> {
    ensure_secure_state_parent(path).map_err(OrderUpdateError::Storage)?;
    let _lock = crate::daemon::lifecycle::try_acquire_writer_lease(path)
        .map_err(OrderUpdateError::Storage)?
        .ok_or(OrderUpdateError::Busy)?;
    let mut current = load_state(path).map_err(OrderUpdateError::Storage)?;
    if current.version != expected_version {
        return Err(OrderUpdateError::Stale {
            current_version: current.version,
        });
    }
    let changed = current
        .replace_view_preferences(expected_version, view_mode, filter)
        .map_err(|current_version| OrderUpdateError::Stale { current_version })?;
    if changed {
        save_state(path, &current).map_err(OrderUpdateError::Storage)?;
    }
    Ok(current)
}

pub fn compare_and_swap_expansion_override(
    path: &Path,
    expected_version: u64,
    row_id: String,
    overridden: bool,
) -> std::result::Result<SidebarExpansionPreferences, OrderUpdateError> {
    ensure_secure_state_parent(path).map_err(OrderUpdateError::Storage)?;
    let _lock = crate::daemon::lifecycle::try_acquire_writer_lease(path)
        .map_err(OrderUpdateError::Storage)?
        .ok_or(OrderUpdateError::Busy)?;
    let mut current = load_expansion_state(path).map_err(OrderUpdateError::Storage)?;
    if current.version != expected_version {
        return Err(OrderUpdateError::Stale {
            current_version: current.version,
        });
    }
    let changed = current
        .set_override(expected_version, row_id, overridden)
        .map_err(|current_version| OrderUpdateError::Stale { current_version })?;
    if changed {
        save_expansion_state(path, &current).map_err(OrderUpdateError::Storage)?;
    }
    Ok(current)
}

pub fn encode_state(state: &SidebarOrderPreferences) -> Result<String> {
    Ok(serde_json::to_string_pretty(state)?)
}

pub fn decode_state(raw: &str) -> Result<SidebarOrderPreferences> {
    let state = serde_json::from_str::<SidebarOrderPreferences>(raw)?;
    state.validate().map_err(anyhow::Error::msg)?;
    Ok(state)
}

pub fn encode_expansion_state(state: &SidebarExpansionPreferences) -> Result<String> {
    Ok(serde_json::to_string_pretty(state)?)
}

pub fn decode_expansion_state(raw: &str) -> Result<SidebarExpansionPreferences> {
    let state = serde_json::from_str::<SidebarExpansionPreferences>(raw)?;
    state.validate().map_err(anyhow::Error::msg)?;
    Ok(state)
}

pub fn load_state(path: &std::path::Path) -> Result<SidebarOrderPreferences> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_state_file(path, &metadata)?;
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => decode_state(&raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SidebarOrderPreferences::default())
        }
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn load_expansion_state(path: &Path) -> Result<SidebarExpansionPreferences> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_state_file(path, &metadata)?;
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => decode_expansion_state(&raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SidebarExpansionPreferences::default())
        }
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save_state(path: &std::path::Path, state: &SidebarOrderPreferences) -> Result<()> {
    ensure_secure_state_parent(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_state_file(path, &metadata)?;
    }
    save_encoded_state(path, encode_state(state)?)
}

pub fn save_expansion_state(path: &Path, state: &SidebarExpansionPreferences) -> Result<()> {
    ensure_secure_state_parent(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_state_file(path, &metadata)?;
    }
    save_encoded_state(path, encode_expansion_state(state)?)
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

pub fn state_path(env: &BTreeMap<String, String>) -> PathBuf {
    if let Some(state_home) = env
        .get("XDG_STATE_HOME")
        .filter(|value| !value.trim().is_empty())
    {
        return PathBuf::from(state_home).join("vde/tmux/sidebar-state/sidebar-order-v1.json");
    }
    if let Some(home) = env.get("HOME").filter(|value| !value.trim().is_empty()) {
        return PathBuf::from(home)
            .join(".local/state/vde/tmux/sidebar-state/sidebar-order-v1.json");
    }
    PathBuf::from(format!(
        "/tmp/vt-{}/sidebar-state/sidebar-order-v1.json",
        unsafe { libc::geteuid() }
    ))
}

pub fn expansion_state_path(env: &BTreeMap<String, String>) -> PathBuf {
    state_path(env).with_file_name("sidebar-expansion-v1.json")
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
    use crate::sidebar::state::{RepoId, SidebarExpansionPreferences, SidebarOrderPreferences};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn state_json_roundtrips() {
        let state = SidebarOrderPreferences {
            version: 7,
            manual_order: vec![RepoId::new("misc", "app")],
            ..SidebarOrderPreferences::default()
        };

        let json = encode_state(&state).unwrap();
        let decoded = decode_state(&json).unwrap();

        assert_eq!(decoded, state);
    }

    #[test]
    fn state_path_prefers_xdg_state_home() {
        let env = std::collections::BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            "/tmp/state".to_string(),
        )]);

        assert_eq!(
            state_path(&env),
            std::path::PathBuf::from("/tmp/state/vde/tmux/sidebar-state/sidebar-order-v1.json")
        );
        assert_eq!(
            expansion_state_path(&env),
            std::path::PathBuf::from("/tmp/state/vde/tmux/sidebar-state/sidebar-expansion-v1.json")
        );
    }

    #[test]
    fn state_path_uses_private_uid_scoped_tmp_fallback() {
        let expected = std::path::PathBuf::from(format!(
            "/tmp/vt-{}/sidebar-state/sidebar-order-v1.json",
            unsafe { libc::geteuid() }
        ));

        assert_eq!(state_path(&BTreeMap::new()), expected);
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
        let state = SidebarOrderPreferences {
            version: 1,
            manual_chat_order: vec!["%1".to_string()],
            ..SidebarOrderPreferences::default()
        };

        save_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();

        assert_eq!(loaded, state);
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
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
        let path = expansion_state_path(&env);

        save_expansion_state(&path, &SidebarExpansionPreferences::default()).unwrap();

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
        assert!(
            save_state(
                &loose.join("state.json"),
                &SidebarOrderPreferences::default()
            )
            .is_err()
        );

        let target = root.join("target");
        let parent_link = root.join("parent-link");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, &parent_link).unwrap();
        assert!(
            save_state(
                &parent_link.join("state.json"),
                &SidebarOrderPreferences::default()
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
        assert!(save_state(&state_link, &SidebarOrderPreferences::default()).is_err());
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

        save_state(&path, &SidebarOrderPreferences::default()).unwrap();

        let after_inode = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(after_inode, before_inode);
        assert_eq!(
            load_state(&path).unwrap(),
            SidebarOrderPreferences::default()
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

        assert_eq!(loaded, SidebarOrderPreferences::default());
    }

    #[test]
    fn global_order_cas_rejects_second_server_with_stale_version() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-order-cas-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("sidebar-order-v1.json");

        let first =
            compare_and_swap_order(&path, 0, vec![RepoId::new("misc", "first")], Vec::new())
                .unwrap();
        let second =
            compare_and_swap_order(&path, 0, vec![RepoId::new("misc", "second")], Vec::new())
                .unwrap_err();

        assert_eq!(first.version, 1);
        assert!(matches!(
            second,
            OrderUpdateError::Stale { current_version: 1 }
        ));
        assert_eq!(load_state(&path).unwrap().manual_order, first.manual_order);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expansion_state_roundtrips_and_rejects_stale_writes() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-expansion-cas-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("sidebar-expansion-v1.json");

        let first =
            compare_and_swap_expansion_override(&path, 0, "repo::misc::app".to_string(), true)
                .unwrap();
        let stale =
            compare_and_swap_expansion_override(&path, 0, "category::work".to_string(), true)
                .unwrap_err();

        assert_eq!(first.version, 1);
        assert_eq!(load_expansion_state(&path).unwrap(), first);
        assert!(matches!(
            stale,
            OrderUpdateError::Stale { current_version: 1 }
        ));
        assert_eq!(
            decode_expansion_state(&encode_expansion_state(&first).unwrap()).unwrap(),
            first
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn view_preference_cas_preserves_manual_order_and_rejects_stale_writes() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-view-pref-cas-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("sidebar-order-v1.json");
        let ordered = compare_and_swap_order(
            &path,
            0,
            vec![RepoId::new("misc", "app")],
            vec!["%1".to_string()],
        )
        .unwrap();

        let updated = compare_and_swap_view_preferences(
            &path,
            ordered.version,
            crate::sidebar::state::ViewMode::ByCategory,
            crate::sidebar::state::StatusFilter::DoneOnly,
        )
        .unwrap();

        assert_eq!(updated.manual_order, ordered.manual_order);
        assert_eq!(updated.manual_chat_order, ordered.manual_chat_order);
        assert_eq!(
            updated.view_mode,
            crate::sidebar::state::ViewMode::ByCategory
        );
        assert_eq!(
            updated.filter,
            crate::sidebar::state::StatusFilter::DoneOnly
        );
        assert!(matches!(
            compare_and_swap_view_preferences(
                &path,
                ordered.version,
                crate::sidebar::state::ViewMode::Flat,
                crate::sidebar::state::StatusFilter::All,
            ),
            Err(OrderUpdateError::Stale { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn order_cas_reports_storage_failure_before_acknowledgement() {
        let dir = std::env::temp_dir().join(format!(
            "vde-tmux-order-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state-parent");
        std::fs::create_dir(&path).unwrap();

        let error =
            compare_and_swap_order(&path, 0, Vec::new(), vec!["%1".to_string()]).unwrap_err();

        assert!(matches!(error, OrderUpdateError::Storage(_)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

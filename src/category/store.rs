use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use super::CategoryState;

pub fn encode_state(state: &CategoryState) -> Result<String> {
    state.validate().map_err(anyhow::Error::msg)?;
    Ok(serde_json::to_string_pretty(state)?)
}

pub fn decode_state(raw: &str) -> Result<CategoryState> {
    let state = serde_json::from_str::<CategoryState>(raw)?;
    state.validate().map_err(anyhow::Error::msg)?;
    Ok(state)
}

pub fn load_state(path: &Path) -> Result<CategoryState> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_state_file(path, &metadata)?;
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => decode_state(&raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CategoryState::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save_state(path: &Path, state: &CategoryState) -> Result<()> {
    ensure_secure_state_parent(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_state_file(path, &metadata)?;
    }
    let encoded = encode_state(state)?;
    let temp_path = temporary_state_path(path)?;
    let result = (|| -> Result<()> {
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
        std::fs::rename(&temp_path, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        std::fs::File::open(path.parent().unwrap_or_else(|| Path::new(".")))?
            .sync_all()
            .with_context(|| format!("failed to sync parent of {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
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
    base.join(format!("category-state-{:x}.json", hasher.finalize()))
}

fn temporary_state_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("state path has no file name: {}", path.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{file_name}.tmp.{}.{}", std::process::id(), stamp)))
}

fn ensure_secure_state_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let runtime_root = crate::runtime_dir::per_user_runtime_root();
    if parent.starts_with(&runtime_root) {
        return crate::runtime_dir::ensure_secure_dir_chain(&runtime_root, parent);
    }
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) => validate_private_state_dir(parent, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let ancestor = parent.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(ancestor)?;
            match std::fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            validate_private_state_dir(parent, &std::fs::symlink_metadata(parent)?)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_private_state_dir(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(anyhow!(
            "insecure category state directory: {}",
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
        return Err(anyhow!("insecure category state file: {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::category::{CategoryName, RepoKey};

    #[test]
    fn state_roundtrips_and_rejects_unknown_schema() {
        let mut state = CategoryState::default();
        state
            .dynamic_categories
            .insert(CategoryName::parse("work").unwrap());
        state
            .repo_overrides
            .insert(RepoKey::path("/repo"), CategoryName::parse("work").unwrap());
        let encoded = encode_state(&state).unwrap();
        assert_eq!(decode_state(&encoded).unwrap(), state);
        assert!(decode_state(r#"{"schema_version":2,"revision":0}"#).is_err());
    }

    #[test]
    fn socket_namespaces_are_private_and_atomic() {
        let root = unique_temp_dir();
        let state_home = root.join("state");
        std::fs::create_dir_all(&state_home).unwrap();
        std::fs::set_permissions(&state_home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            state_home.to_string_lossy().into_owned(),
        )]);
        let first = state_path(&env, Path::new("/tmp/tmux/default"));
        let second = state_path(&env, Path::new("/tmp/tmux/other"));
        assert_ne!(first, second);

        let mut state = CategoryState::default();
        state
            .dynamic_categories
            .insert(CategoryName::parse("work").unwrap());
        save_state(&first, &state).unwrap();

        assert_eq!(load_state(&first).unwrap(), state);
        assert_eq!(load_state(&second).unwrap(), CategoryState::default());
        assert_eq!(
            std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "vde-tmux-category-store-{}-{stamp}",
            std::process::id()
        ))
    }
}

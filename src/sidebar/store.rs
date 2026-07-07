use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::sidebar::state::SidebarState;

pub fn encode_state(state: &SidebarState) -> Result<String> {
    Ok(serde_json::to_string_pretty(state)?)
}

pub fn decode_state(raw: &str) -> Result<SidebarState> {
    Ok(serde_json::from_str(raw)?)
}

pub fn load_state(path: &std::path::Path) -> Result<SidebarState> {
    match std::fs::read_to_string(path) {
        Ok(raw) => decode_state(&raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SidebarState::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save_state(path: &std::path::Path, state: &SidebarState) -> Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let encoded = encode_state(state)?;
    let temp_path = temporary_state_path(path)?;
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
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
        return PathBuf::from(state_home).join("vde/tmux/state.json");
    }
    if let Some(home) = env.get("HOME").filter(|value| !value.trim().is_empty()) {
        return PathBuf::from(home).join(".local/state/vde/tmux/state.json");
    }
    PathBuf::from("/tmp/vde-tmux-state.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::state::{SidebarState, ViewMode};
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn state_json_roundtrips() {
        let mut state = SidebarState {
            version: 7,
            view_mode: ViewMode::ByCategory,
            selection: Some("pane::%1".to_string()),
            ..SidebarState::default()
        };
        state.collapsed.insert("repo::misc::app".to_string());

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
            std::path::PathBuf::from("/tmp/state/vde/tmux/state.json")
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
        let path = dir.join("state.json");
        let mut state = SidebarState {
            version: 1,
            view_mode: ViewMode::Flat,
            selection: Some("pane::%1".to_string()),
            ..SidebarState::default()
        };
        state.collapsed.insert("repo::misc::app".to_string());

        save_state(&path, &state).unwrap();
        let loaded = load_state(&path).unwrap();

        assert_eq!(loaded, state);
        std::fs::remove_dir_all(dir).unwrap();
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
        let path = dir.join("state.json");
        std::fs::write(&path, "{}").unwrap();
        let before_inode = std::fs::metadata(&path).unwrap().ino();

        save_state(&path, &SidebarState::default()).unwrap();

        let after_inode = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(after_inode, before_inode);
        assert_eq!(load_state(&path).unwrap(), SidebarState::default());
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

        assert_eq!(loaded, SidebarState::default());
    }
}

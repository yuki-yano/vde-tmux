use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

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
    std::fs::write(path, encoded).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
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

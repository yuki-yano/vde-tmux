//! config のパス解決と読み込み。
//! 方針(設計書 §3.3): 読めない・パースできない場合は default + 警告で継続し、
//! daemon / statusline を絶対に止めない。

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::Config;

/// $XDG_CONFIG_HOME(未設定なら $HOME/.config)/vde/tmux/config.yml
pub fn config_file_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let base = match env.get("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env.get("HOME").filter(|v| !v.is_empty())?).join(".config"),
    };
    Some(base.join("vde").join("tmux").join("config.yml"))
}

/// 読み込み結果。warnings は呼び出し側(CLI 境界)が stderr に出す。
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

/// YAML 文字列から Config を得る。空文字列は default。
/// パース失敗時は default + 位置情報付き警告(serde_path_to_error)。
pub fn parse_config(yaml: &str) -> LoadedConfig {
    if yaml.trim().is_empty() {
        return LoadedConfig {
            config: Config::default(),
            warnings: Vec::new(),
        };
    }
    let deserializer = serde_yaml_ng::Deserializer::from_str(yaml);
    match serde_path_to_error::deserialize::<_, Config>(deserializer) {
        Ok(config) => LoadedConfig {
            config,
            warnings: Vec::new(),
        },
        Err(error) => LoadedConfig {
            config: Config::default(),
            warnings: vec![format!("invalid config (path: {}): {error}", error.path())],
        },
    }
}

/// ファイルから読み込む。ファイル不在は警告なしの default(初回利用で騒がない)。
/// 読み取りエラー・パースエラーは default + 警告。
pub fn load_config(env: &BTreeMap<String, String>) -> LoadedConfig {
    let Some(path) = config_file_path(env) else {
        return LoadedConfig {
            config: Config::default(),
            warnings: vec!["HOME is not set; using default config".to_string()],
        };
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_config(&content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LoadedConfig {
            config: Config::default(),
            warnings: Vec::new(),
        },
        Err(error) => LoadedConfig {
            config: Config::default(),
            warnings: vec![format!("failed to read {}: {error}", path.display())],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn path_prefers_xdg_config_home() {
        let path = config_file_path(&env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")])).unwrap();
        assert_eq!(path, PathBuf::from("/x/vde/tmux/config.yml"));
    }

    #[test]
    fn path_falls_back_to_home_dot_config() {
        let path = config_file_path(&env(&[("HOME", "/h")])).unwrap();
        assert_eq!(path, PathBuf::from("/h/.config/vde/tmux/config.yml"));
    }

    #[test]
    fn path_none_without_home() {
        assert!(config_file_path(&env(&[])).is_none());
    }

    #[test]
    fn parse_valid_yaml_no_warnings() {
        let loaded = parse_config("daemon:\n  poll_ms: 123\n");
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.daemon.poll_ms, 123);
    }

    #[test]
    fn parse_broken_yaml_returns_default_with_warning() {
        let loaded = parse_config("daemon:\n  poll_ms: [not-a-number\n");
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("invalid config"),
            "{}",
            loaded.warnings[0]
        );
    }

    #[test]
    fn parse_type_mismatch_warning_includes_path() {
        let loaded = parse_config("daemon:\n  poll_ms: \"abc\"\n");
        assert_eq!(loaded.config, Config::default());
        assert!(
            loaded.warnings[0].contains("daemon.poll_ms"),
            "{}",
            loaded.warnings[0]
        );
    }

    #[test]
    fn parse_empty_is_default_silent() {
        let loaded = parse_config("   \n");
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn load_missing_file_is_default_silent() {
        let loaded = load_config(&env(&[("HOME", "/nonexistent-home-for-vde-tmux-test")]));
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn parse_config_accepts_session_badge_overrides() {
        let yaml = r#"
statusline:
  session_badge:
    suffix: ""
    glyphs:
      blocked: "!"
"#;
        let loaded = parse_config(yaml);
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.statusline.session_badge.suffix, "");
        assert_eq!(loaded.config.statusline.session_badge.glyphs.blocked, "!");
        assert_eq!(loaded.config.statusline.session_badge.glyphs.working, "🟡");
        assert!(loaded.config.statusline.session_badge.enabled);
    }
}

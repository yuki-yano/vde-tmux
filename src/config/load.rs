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
    parse_config_with_env(yaml, &BTreeMap::new())
}

pub fn parse_config_with_env(yaml: &str, env: &BTreeMap<String, String>) -> LoadedConfig {
    if yaml.trim().is_empty() {
        return LoadedConfig {
            config: Config::default(),
            warnings: Vec::new(),
        };
    }
    let deserializer = serde_yaml_ng::Deserializer::from_str(yaml);
    match serde_path_to_error::deserialize::<_, Config>(deserializer) {
        Ok(mut config) => match expand_config_patterns(&mut config, env) {
            Ok(()) => LoadedConfig {
                config,
                warnings: Vec::new(),
            },
            Err(warning) => LoadedConfig {
                config: Config::default(),
                warnings: vec![warning],
            },
        },
        Err(error) => LoadedConfig {
            config: Config::default(),
            warnings: vec![format!("invalid config (path: {}): {error}", error.path())],
        },
    }
}

fn expand_config_patterns(
    config: &mut Config,
    env: &BTreeMap<String, String>,
) -> Result<(), String> {
    for (rule_index, rule) in config.categories.rules.iter_mut().enumerate() {
        for (pattern_index, pattern) in rule.path_patterns.iter_mut().enumerate() {
            *pattern = expand_pattern(
                pattern,
                env,
                &format!("categories.rules.{rule_index}.path_patterns.{pattern_index}"),
            )?;
        }
    }
    for (rule_index, rule) in config.categories.session_name_rules.iter_mut().enumerate() {
        for (pattern_index, pattern) in rule.patterns.iter_mut().enumerate() {
            *pattern = expand_pattern(
                pattern,
                env,
                &format!("categories.session_name_rules.{rule_index}.patterns.{pattern_index}"),
            )?;
        }
    }
    Ok(())
}

fn expand_pattern(
    value: &str,
    env: &BTreeMap<String, String>,
    path: &str,
) -> Result<String, String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            output.push_str(&rest[start..]);
            return Ok(output);
        };
        let name = &after_start[..end];
        let token_len = 2 + end + 1;
        if !is_env_name(name) {
            output.push_str(&rest[start..start + token_len]);
            rest = &after_start[end + 1..];
            continue;
        }
        let Some(replacement) = env.get(name) else {
            return Err(format!(
                "invalid config (path: {path}): environment variable {name} is not defined"
            ));
        };
        output.push_str(replacement);
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn is_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
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
        Ok(content) => parse_config_with_env(&content, env),
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
    fn parse_unknown_nested_field_returns_default_with_warning() {
        let loaded = parse_config("sidebar:\n  preview:\n    history_line: 2000\n");

        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("sidebar.preview.history_line"),
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
    fn config_pattern_env_expands_path_and_session_patterns() {
        let loaded = parse_config_with_env(
            r#"
categories:
  rules:
    - category: work
      path_patterns:
        - github.com/${WORK_GHQ_OWNER}/*
  session_name_rules:
    - category: work
      patterns:
        - ${WORK_PREFIX}-*
"#,
            &env(&[("WORK_GHQ_OWNER", "acme"), ("WORK_PREFIX", "corp")]),
        );
        assert!(loaded.warnings.is_empty());
        assert_eq!(
            loaded.config.categories.rules[0].path_patterns[0],
            "github.com/acme/*"
        );
        assert_eq!(
            loaded.config.categories.session_name_rules[0].patterns[0],
            "corp-*"
        );
    }

    #[test]
    fn config_pattern_env_missing_var_returns_default_with_warning() {
        let loaded = parse_config_with_env(
            "categories:\n  rules:\n    - category: work\n      path_patterns:\n        - github.com/${WORK_GHQ_OWNER}/*\n",
            &env(&[]),
        );
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("WORK_GHQ_OWNER"));
    }

    #[test]
    fn parse_config_accepts_session_badge_overrides() {
        let yaml = r#"
statusline:
  session_badge:
    suffix: ""
badge:
  glyphs:
    blocked: "!"
"#;
        let loaded = parse_config(yaml);
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.statusline.session_badge.suffix, "");
        assert_eq!(loaded.config.badge.glyphs.blocked, "!");
        assert_eq!(loaded.config.badge.glyphs.working, "●");
        assert!(loaded.config.statusline.session_badge.enabled);
    }

    #[test]
    fn config_pattern_env_expands_path_patterns() {
        let loaded = parse_config_with_env(
            r#"
categories:
  rules:
    - category: work
      path_patterns:
        - github.com/${WORK_OWNER}/*
"#,
            &env(&[("WORK_OWNER", "acme")]),
        );

        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(
            loaded.config.categories.rules[0].path_patterns[0],
            "github.com/acme/*"
        );
    }
}

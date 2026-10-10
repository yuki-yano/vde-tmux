use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Config;

pub fn config_file_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let base = match env.get("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env.get("HOME").filter(|v| !v.is_empty())?).join(".config"),
    };
    Some(base.join("vde").join("tmux").join("config.yml"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

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
        Ok(mut config) => {
            config.codex.capacity_auto_resume.prompt = config
                .codex
                .capacity_auto_resume
                .prompt
                .trim_end_matches('\n')
                .to_owned();
            match expand_config_patterns(&mut config, env).and_then(|()| validate_config(&config)) {
                Ok(()) => LoadedConfig {
                    config,
                    warnings: Vec::new(),
                },
                Err(warning) => LoadedConfig {
                    config: Config::default(),
                    warnings: vec![warning],
                },
            }
        }
        Err(error) => LoadedConfig {
            config: Config::default(),
            warnings: vec![format!("invalid config (path: {}): {error}", error.path())],
        },
    }
}

fn validate_config(config: &Config) -> Result<(), String> {
    crate::codex_capacity::validate_prompt(&config.codex.capacity_auto_resume.prompt)?;
    crate::category::configured_category_names(config)
        .map(|_| ())
        .map_err(|error| format!("categories: {error}"))?;
    let summary = &config.sidebar.task_summary;
    if summary.debounce_ms == 0 {
        return Err("sidebar.task_summary.debounce_ms must be positive".to_string());
    }
    if summary.timeout_ms == 0 {
        return Err("sidebar.task_summary.timeout_ms must be positive".to_string());
    }
    for (name, model) in [
        ("codex_model", summary.codex_model.as_deref()),
        ("claude_model", summary.claude_model.as_deref()),
    ] {
        if model.is_some_and(|value| value.trim().is_empty() || value.contains(['\r', '\n'])) {
            return Err(format!(
                "sidebar.task_summary.{name} must be a non-empty single-line model name"
            ));
        }
    }
    Ok(())
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

pub fn load_config_strict(env: &BTreeMap<String, String>) -> Result<Config, String> {
    let loaded = load_config(env);
    if loaded.warnings.is_empty() {
        Ok(loaded.config)
    } else {
        Err(loaded.warnings.join("; "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigReloadIdentity {
    pub source_hash: String,
    pub context_hash: String,
    pub context_vars: Vec<String>,
    pub executable: String,
}

pub(crate) fn load_config_for_reload(
    env: &BTreeMap<String, String>,
) -> Result<(Config, ConfigReloadIdentity), String> {
    use std::os::unix::fs::MetadataExt;
    let path = config_file_path(env).ok_or("HOME is not set")?;
    let path = std::path::absolute(path).map_err(|error| error.to_string())?;
    let (contents, present) = match std::fs::read_to_string(&path) {
        Ok(contents) => (contents, true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    let loaded = parse_config_with_env(&contents, env);
    if !loaded.warnings.is_empty() {
        return Err(loaded.warnings.join("; "));
    }
    let mut source = Sha256::new();
    source.update([u8::from(present)]);
    source.update(contents.as_bytes());
    // Only category path patterns expand environment variables. Comments and
    // literal prompt text do not affect the environment identity.
    let raw = if contents.trim().is_empty() {
        Config::default()
    } else {
        serde_yaml_ng::from_str::<Config>(&contents).map_err(|error| error.to_string())?
    };
    let mut names = std::collections::BTreeSet::new();
    for pattern in raw
        .categories
        .rules
        .iter()
        .flat_map(|rule| &rule.path_patterns)
    {
        for rest in pattern.split("${").skip(1) {
            if let Some((name, _)) = rest.split_once('}')
                && is_env_name(name)
            {
                names.insert(name.to_string());
            }
        }
    }
    let context_vars = names.into_iter().collect::<Vec<_>>();
    let context_hash = config_context_hash(env, &context_vars)?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = std::fs::canonicalize(exe).map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(&exe).map_err(|error| error.to_string())?;
    let executable = format!(
        "{}:{}:{}:{}:{}:{}",
        exe.display(),
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    );
    Ok((
        loaded.config,
        ConfigReloadIdentity {
            source_hash: format!("{:x}", source.finalize()),
            context_hash,
            context_vars,
            executable,
        },
    ))
}

pub(crate) fn config_context_hash(
    env: &BTreeMap<String, String>,
    names: &[String],
) -> Result<String, String> {
    let path = config_file_path(env).ok_or("HOME is not set")?;
    let path = std::path::absolute(path).map_err(|error| error.to_string())?;
    let variables = names
        .iter()
        .map(|name| (name, env.get(name)))
        .collect::<BTreeMap<_, _>>();
    let context = (path, env.get("HOME"), variables);
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&context).map_err(|error| error.to_string())?)
    ))
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
    fn capacity_config_is_opt_in_and_normalizes_only_trailing_lf() {
        assert!(!parse_config("").config.codex.capacity_auto_resume.enabled);
        let loaded = parse_config(
            "codex:\n  capacity_auto_resume:\n    enabled: true\n    prompt: |\n      続行してください。\n\n",
        );
        assert!(loaded.warnings.is_empty());
        assert_eq!(
            loaded.config.codex.capacity_auto_resume.prompt,
            "続行してください。"
        );
        for prompt in [
            "",
            " x",
            "x ",
            "/model",
            "!cmd",
            "continue @file",
            "continue $skill",
        ] {
            let yaml = format!(
                "codex:\n  capacity_auto_resume:\n    enabled: true\n    prompt: {}\n",
                serde_json::to_string(prompt).unwrap()
            );
            assert!(!parse_config(&yaml).warnings.is_empty(), "{prompt}");
        }
    }

    #[test]
    fn strict_capacity_load_rejects_invalid_prompt_without_using_default() {
        let root = std::env::temp_dir().join(format!(
            "vde-capacity-config-{}",
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        let path = root.join("vde/tmux/config.yml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let env = env(&[("XDG_CONFIG_HOME", root.to_str().unwrap())]);
        for prompt in [
            "",
            "/model",
            "Continue @file",
            " leading",
            "trailing ",
            "x\r",
        ] {
            std::fs::write(
                &path,
                format!(
                    "codex:\n  capacity_auto_resume:\n    enabled: true\n    prompt: {}\n",
                    serde_json::to_string(prompt).unwrap()
                ),
            )
            .unwrap();
            assert!(
                load_config_strict(&env)
                    .unwrap_err()
                    .contains("codex.capacity_auto_resume.prompt")
            );
        }
        std::fs::write(&path,"codex:\n  capacity_auto_resume:\n    enabled: true\n    prompt: |+\n      続行してください。\n      条件を保持してください。\n\n\n").unwrap();
        assert_eq!(
            load_config_strict(&env)
                .unwrap()
                .codex
                .capacity_auto_resume
                .prompt,
            "続行してください。\n条件を保持してください。"
        );
        std::fs::remove_dir_all(root).unwrap();
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
        let loaded = parse_config("sidebar:\n  preview:\n    history_lines: 2000\n");

        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("sidebar.preview"),
            "{}",
            loaded.warnings[0]
        );
    }

    #[test]
    fn task_summary_rejects_zero_timeout_and_blank_model() {
        let zero = parse_config("sidebar:\n  task_summary:\n    timeout_ms: 0\n");
        assert_eq!(zero.config, Config::default());
        assert!(zero.warnings[0].contains("timeout_ms"));

        let blank = parse_config("sidebar:\n  task_summary:\n    codex_model: '   '\n");
        assert_eq!(blank.config, Config::default());
        assert!(blank.warnings[0].contains("codex_model"));
    }

    #[test]
    fn parse_empty_is_default_silent() {
        for input in ["", "   \n"] {
            let loaded = parse_config(input);
            let config = loaded.config;
            assert!(loaded.warnings.is_empty());
            assert!(config.statusline.summary.enabled);
            assert!(!config.statusline.summary.hide_idle);
            assert_eq!(config.statusline.summary.format, "{badge} {count}");
            assert_eq!(config.daemon.poll_ms, 1000);
            assert_eq!(config.daemon.git.timeout_ms, 500);
            assert_eq!(
                config.sidebar.width,
                crate::config::SidebarWidth::Columns(40)
            );
            assert_eq!(config.sidebar.min_width, 40);
            assert!(!config.sidebar.task_summary.enabled);
            assert_eq!(config.sidebar.task_summary.debounce_ms, 750);
            assert_eq!(config.sidebar.task_summary.timeout_ms, 90_000);
            assert_eq!(config.popup.width, "50%");
            assert_eq!(config.popup.height, "50%");
            assert!(config.session_manager.kill.send_ctrl_c);
            assert_eq!(config.session_manager.kill.term_wait_ms, 300);
            assert_eq!(config.session_manager.kill.kill_wait_ms, 300);
            assert_eq!(config.statusline.category.mode, "list");
        }
    }

    #[test]
    fn load_missing_file_is_default_silent() {
        let loaded = load_config(&env(&[("HOME", "/nonexistent-home-for-vde-tmux-test")]));
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn strict_load_rejects_invalid_config_instead_of_falling_back() {
        let root = std::env::temp_dir().join(format!(
            "vde-config-strict-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        let config = root.join("vde/tmux/config.yml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "daemon:\n  poll_ms: invalid\n").unwrap();

        let error =
            load_config_strict(&env(&[("XDG_CONFIG_HOME", root.to_str().unwrap())])).unwrap_err();

        assert!(error.contains("daemon.poll_ms"));
        std::fs::remove_dir_all(root).unwrap();
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
        let yaml = r##"
statusline:
  session_badge:
    mode: counts
    chip:
      bg: "#30304a"
      cap_left: "<"
      cap_right: ">"
    suffix: ""
badge:
  glyphs:
    blocked: "!"
"##;
        let loaded = parse_config(yaml);
        assert!(loaded.warnings.is_empty());
        assert_eq!(
            loaded.config.statusline.session_badge.mode,
            crate::config::SessionBadgeMode::Counts
        );
        assert_eq!(loaded.config.statusline.session_badge.chip.bg, "#30304a");
        assert_eq!(loaded.config.statusline.session_badge.chip.cap_left, "<");
        assert_eq!(loaded.config.statusline.session_badge.chip.cap_right, ">");
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

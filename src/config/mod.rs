//! 単一 config(~/.config/vde/tmux/config.yml)のスキーマ。snake_case。
//! すべてのフィールドに default を持たせ、部分的な config でも常に動く。

pub mod load;

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ghq_root: Option<String>,
    pub categories: CategoriesConfig,
    pub statusline: StatuslineConfig,
    pub sidebar: SidebarConfig,
    pub daemon: DaemonConfig,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct CategoriesConfig {
    pub display_names: BTreeMap<String, String>,
    pub order: BTreeMap<String, i64>,
    pub default_category: Option<String>,
    pub rules: Vec<CategoryRule>,
    pub session_name_rules: Vec<SessionNameRule>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct CategoryRule {
    pub category: String,
    pub ghq_patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SessionNameRule {
    pub category: String,
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct StatuslineConfig {
    pub sessions: StatuslineSessionsConfig,
    pub category: StatuslineCategoryConfig,
    pub agent_badge: AgentBadgeConfig,
}

// show_index=false / SegmentStyle::default() は derive の Default と一致するため
// 手書き impl にしない(clippy::derivable_impls が -D warnings でエラーになる)。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct StatuslineSessionsConfig {
    pub show_index: bool,
    pub current: SegmentStyle,
    pub other: SegmentStyle,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SegmentStyle {
    pub format: String,
    pub prefix: String,
    pub suffix: String,
    pub bold: bool,
    pub colors: SegmentColors,
}

impl Default for SegmentStyle {
    fn default() -> Self {
        Self {
            format: " {session} ".to_string(),
            prefix: String::new(),
            suffix: String::new(),
            bold: false,
            colors: SegmentColors::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SegmentColors {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub outer_bg: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct StatuslineCategoryConfig {
    /// "list"(全カテゴリ列挙)または "current"(現在のみ)。
    pub mode: String,
    pub format: String,
    pub prefix: String,
    pub suffix: String,
    pub bold: bool,
    pub colors: SegmentColors,
    pub inactive_colors: SegmentColors,
}

impl Default for StatuslineCategoryConfig {
    fn default() -> Self {
        Self {
            mode: "list".to_string(),
            format: "{category} ".to_string(),
            prefix: String::new(),
            suffix: String::new(),
            bold: false,
            colors: SegmentColors::default(),
            inactive_colors: SegmentColors::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AgentBadgeConfig {
    pub enabled: bool,
}

impl Default for AgentBadgeConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub width: u16,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self { width: 40 }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub poll_ms: u64,
    pub git: GitConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            poll_ms: 1000,
            git: GitConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 500,
            poll_interval_ms: 10_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yaml_yields_full_defaults() {
        let config: Config = serde_yaml_ng::from_str("").unwrap_or_default();
        assert_eq!(config, Config::default());
        assert!(config.statusline.agent_badge.enabled);
        assert_eq!(config.daemon.poll_ms, 1000);
        assert_eq!(config.daemon.git.timeout_ms, 500);
        assert_eq!(config.sidebar.width, 40);
        assert_eq!(config.statusline.category.mode, "list");
    }

    #[test]
    fn partial_yaml_overrides_only_given_keys() {
        let yaml = r#"
ghq_root: "~/repos"
statusline:
  sessions:
    show_index: true
daemon:
  poll_ms: 250
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.ghq_root.as_deref(), Some("~/repos"));
        assert!(config.statusline.sessions.show_index);
        assert_eq!(config.daemon.poll_ms, 250);
        // 触っていないキーは default のまま
        assert_eq!(config.daemon.git.poll_interval_ms, 10_000);
        assert_eq!(config.statusline.sessions.current.format, " {session} ");
    }

    #[test]
    fn categories_section_parses() {
        let yaml = r#"
categories:
  display_names:
    private: "P"
    work: "W"
  order:
    private: 10
    work: 30
  default_category: public
  rules:
    - category: private
      ghq_patterns:
        - github.com/example/project-a
        - github.com/${WORK_GHQ_OWNER}/*
  session_name_rules:
    - category: private
      patterns:
        - dotfiles
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.categories.display_names["private"], "P");
        assert_eq!(config.categories.order["work"], 30);
        assert_eq!(
            config.categories.default_category.as_deref(),
            Some("public")
        );
        assert_eq!(config.categories.rules.len(), 1);
        assert_eq!(
            config.categories.rules[0].ghq_patterns[1],
            "github.com/${WORK_GHQ_OWNER}/*"
        );
        assert_eq!(
            config.categories.session_name_rules[0].patterns,
            vec!["dotfiles"]
        );
    }
}

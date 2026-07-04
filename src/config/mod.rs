//! 単一 config(~/.config/vde/tmux/config.yml)のスキーマ。snake_case。
//! すべてのフィールドに default を持たせ、部分的な config でも常に動く。

pub mod load;
pub mod schema;

use std::collections::BTreeMap;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, de};

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub categories: CategoriesConfig,
    pub statusline: StatuslineConfig,
    pub sidebar: SidebarConfig,
    pub daemon: DaemonConfig,
    pub badge: BadgeConfig,
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
#[serde(default, deny_unknown_fields)]
pub struct CategoryRule {
    pub category: String,
    pub path_patterns: Vec<String>,
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
    pub session_badge: SessionBadgeConfig,
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
#[serde(default, deny_unknown_fields)]
pub struct SessionBadgeConfig {
    pub enabled: bool,
    /// グリフ直後に付ける区切り文字列。絵文字は表示幅が広いので
    /// デフォルトで半角スペース 1 個を挟む(バッジ値自体に含める)。
    pub suffix: String,
}

impl Default for SessionBadgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            suffix: " ".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BadgeConfig {
    pub glyphs: BadgeGlyphs,
}

impl Default for BadgeConfig {
    fn default() -> Self {
        Self {
            glyphs: BadgeGlyphs::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BadgeGlyphs {
    pub blocked: String,
    pub working: String,
    pub done: String,
    pub idle: String,
}

impl Default for BadgeGlyphs {
    fn default() -> Self {
        Self {
            blocked: "🔴".to_string(),
            working: "🟡".to_string(),
            done: "🔵".to_string(),
            idle: "🟢".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub width: SidebarWidth,
    pub min_width: u16,
    pub colors: SidebarColorsConfig,
    pub header: SidebarHeaderConfig,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            width: SidebarWidth::default(),
            min_width: 40,
            colors: SidebarColorsConfig::default(),
            header: SidebarHeaderConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SidebarColorsConfig {
    pub error: Option<String>,
    pub running: Option<String>,
    pub permission: Option<String>,
    pub background: Option<String>,
    pub waiting: Option<String>,
    pub idle: Option<String>,
    pub attention: Option<String>,
    pub selection_bg: Option<String>,
    pub selection_active_bg: Option<String>,
    pub header_active_bg: Option<String>,
    pub header_active_fg: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarHeaderConfig {
    pub format: String,
    pub prefix: String,
    pub suffix: String,
    pub separator: String,
    pub bold: bool,
    pub colors: SegmentColors,
}

impl Default for SidebarHeaderConfig {
    fn default() -> Self {
        Self {
            format: "{label} ".to_string(),
            prefix: String::new(),
            suffix: String::new(),
            separator: String::new(),
            bold: false,
            colors: SegmentColors::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarWidth {
    Columns(u16),
    Percent(u16),
}

impl Default for SidebarWidth {
    fn default() -> Self {
        Self::Columns(40)
    }
}

impl FromStr for SidebarWidth {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if let Some(percent) = value.strip_suffix('%') {
            let percent = percent
                .parse::<u16>()
                .map_err(|_| format!("expected percentage width like \"10%\", got {value:?}"))?;
            if !(1..=100).contains(&percent) {
                return Err(format!(
                    "expected percentage width from 1% to 100%, got {value:?}"
                ));
            }
            return Ok(Self::Percent(percent));
        }
        let columns = value
            .parse::<u16>()
            .map_err(|_| format!("expected sidebar width like 64 or \"10%\", got {value:?}"))?;
        if columns == 0 {
            return Err("sidebar width must be greater than 0".to_string());
        }
        Ok(Self::Columns(columns))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawSidebarWidth {
    Columns(u16),
    Percent(String),
}

impl<'de> Deserialize<'de> for SidebarWidth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match RawSidebarWidth::deserialize(deserializer)? {
            RawSidebarWidth::Columns(columns) if columns > 0 => Ok(Self::Columns(columns)),
            RawSidebarWidth::Columns(_) => {
                Err(de::Error::custom("sidebar width must be greater than 0"))
            }
            RawSidebarWidth::Percent(value) if value.trim().ends_with('%') => {
                value.parse().map_err(de::Error::custom)
            }
            RawSidebarWidth::Percent(value) => Err(de::Error::custom(format!(
                "expected percentage width like \"10%\", got {value:?}"
            ))),
        }
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
        assert_eq!(config.sidebar.width, SidebarWidth::Columns(40));
        assert_eq!(config.sidebar.min_width, 40);
        assert_eq!(config.statusline.category.mode, "list");
    }

    #[test]
    fn sidebar_width_accepts_columns_and_percent() {
        let columns = serde_yaml_ng::from_str::<Config>("sidebar:\n  width: 64\n").unwrap();
        assert_eq!(columns.sidebar.width, SidebarWidth::Columns(64));
        assert_eq!(columns.sidebar.min_width, 40);

        let percent = serde_yaml_ng::from_str::<Config>("sidebar:\n  width: \"10%\"\n").unwrap();
        assert_eq!(percent.sidebar.width, SidebarWidth::Percent(10));
        assert_eq!(percent.sidebar.min_width, 40);
    }

    #[test]
    fn sidebar_min_width_can_be_overridden() {
        let config =
            serde_yaml_ng::from_str::<Config>("sidebar:\n  width: \"10%\"\n  min_width: 48\n")
                .unwrap();
        assert_eq!(config.sidebar.width, SidebarWidth::Percent(10));
        assert_eq!(config.sidebar.min_width, 48);
    }

    #[test]
    fn sidebar_colors_accept_old_sidebar_color_keys() {
        let config = serde_yaml_ng::from_str::<Config>(
            "sidebar:\n  colors:\n    running: green\n    selection_bg: \"237\"\n    header_active_bg: \"24\"\n",
        )
        .unwrap();

        assert_eq!(config.sidebar.colors.running.as_deref(), Some("green"));
        assert_eq!(config.sidebar.colors.selection_bg.as_deref(), Some("237"));
        assert_eq!(
            config.sidebar.colors.header_active_bg.as_deref(),
            Some("24")
        );
    }

    #[test]
    fn sidebar_header_style_can_be_configured() {
        let config = serde_yaml_ng::from_str::<Config>(
            r##"
sidebar:
  header:
    prefix: "["
    suffix: "]"
    format: " {label} "
    separator: " "
    bold: true
    colors:
      fg: white
      bg: "24"
"##,
        )
        .unwrap();

        assert_eq!(config.sidebar.header.prefix, "[");
        assert_eq!(config.sidebar.header.suffix, "]");
        assert_eq!(config.sidebar.header.format, " {label} ");
        assert_eq!(config.sidebar.header.separator, " ");
        assert!(config.sidebar.header.bold);
        assert_eq!(config.sidebar.header.colors.fg.as_deref(), Some("white"));
        assert_eq!(config.sidebar.header.colors.bg.as_deref(), Some("24"));
    }

    #[test]
    fn sidebar_width_rejects_invalid_percent() {
        let err = serde_yaml_ng::from_str::<Config>("sidebar:\n  width: \"%\"\n").unwrap_err();
        assert!(err.to_string().contains("expected percentage width"));
    }

    #[test]
    fn ghq_root_is_no_longer_accepted_as_top_level_config() {
        let err = serde_yaml_ng::from_str::<Config>("ghq_root: ~/repos\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn partial_yaml_overrides_only_given_keys() {
        let yaml = r#"
statusline:
  sessions:
    show_index: true
daemon:
  poll_ms: 250
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
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
      path_patterns:
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
            config.categories.rules[0].path_patterns[1],
            "github.com/${WORK_GHQ_OWNER}/*"
        );
        assert_eq!(
            config.categories.session_name_rules[0].patterns,
            vec!["dotfiles"]
        );
    }

    #[test]
    fn session_badge_defaults_to_emoji_glyphs_with_space_suffix() {
        let badge = BadgeConfig::default();
        let config = SessionBadgeConfig::default();
        assert!(config.enabled);
        assert_eq!(config.suffix, " ");
        assert_eq!(badge.glyphs.blocked, "🔴");
        assert_eq!(badge.glyphs.working, "🟡");
        assert_eq!(badge.glyphs.done, "🔵");
        assert_eq!(badge.glyphs.idle, "🟢");
    }

    #[test]
    fn categories_section_parses_path_patterns_only() {
        let yaml = r#"
categories:
  rules:
    - category: work
      path_patterns:
        - github.com/${WORK_OWNER}/*
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            config.categories.rules[0].path_patterns[0],
            "github.com/${WORK_OWNER}/*"
        );

        let err = serde_yaml_ng::from_str::<Config>(
            "categories:\n  rules:\n    - category: work\n      ghq_patterns:\n        - github.com/acme/*\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("ghq_patterns"));
    }

    #[test]
    fn badge_glyphs_are_top_level_config() {
        let config: Config = serde_yaml_ng::from_str(
            "badge:\n  glyphs:\n    working: W\nstatusline:\n  session_badge:\n    suffix: \"\"\n",
        )
        .unwrap();
        assert_eq!(config.badge.glyphs.working, "W");
        assert_eq!(config.statusline.session_badge.suffix, "");

        let err = serde_yaml_ng::from_str::<Config>(
            "statusline:\n  session_badge:\n    glyphs:\n      working: W\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("glyphs"));
    }
}

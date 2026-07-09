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
    pub notify: NotifyConfig,
    pub popup: PopupConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PopupConfig {
    pub width: String,
    pub height: String,
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            width: "50%".to_string(),
            height: "50%".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct SessionNameRule {
    pub category: String,
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslineConfig {
    pub sessions: StatuslineSessionsConfig,
    pub windows: StatuslineWindowsConfig,
    pub panes: StatuslinePanesConfig,
    pub category: StatuslineCategoryConfig,
    pub summary: SummaryConfig,
    pub attention: AttentionConfig,
    pub session_badge: SessionBadgeConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslineSessionsConfig {
    pub show_index: bool,
    pub current: SegmentStyle,
    pub other: SegmentStyle,
    pub badge_style: BadgeStyle,
    pub separator: String,
}

impl Default for StatuslineSessionsConfig {
    fn default() -> Self {
        Self {
            show_index: false,
            current: SegmentStyle {
                bold: true,
                ..SegmentStyle::default()
            },
            other: SegmentStyle::default(),
            badge_style: BadgeStyle::Inline,
            separator: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslineWindowsConfig {
    pub current: SegmentStyle,
    pub other: SegmentStyle,
    pub last: SegmentColors,
    pub bell: SegmentColors,
    pub activity: SegmentColors,
    pub separator: String,
}

impl Default for StatuslineWindowsConfig {
    fn default() -> Self {
        Self {
            current: SegmentStyle {
                format: " {index}:{window} ".to_string(),
                bold: true,
                ..SegmentStyle::default()
            },
            other: SegmentStyle {
                format: " {index}:{window} ".to_string(),
                ..SegmentStyle::default()
            },
            last: SegmentColors::default(),
            bell: SegmentColors::default(),
            activity: SegmentColors::default(),
            separator: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslinePanesConfig {
    pub current: SegmentStyle,
    pub other: SegmentStyle,
}

impl Default for StatuslinePanesConfig {
    fn default() -> Self {
        Self {
            current: SegmentStyle {
                format: " {pane} \u{e0b1} {detail} ".to_string(),
                prefix: "#[fg=#4a4a70,bg=#1C1C1C]\u{e0b6}".to_string(),
                suffix: "#[fg=#4a4a70,bg=#1C1C1C]\u{e0b4}#[default]".to_string(),
                colors: SegmentColors {
                    fg: Some("#e7e3f6".to_string()),
                    bg: Some("#4a4a70".to_string()),
                    outer_bg: Some("#1C1C1C".to_string()),
                },
                ..SegmentStyle::default()
            },
            other: SegmentStyle {
                format: " {pane} #[fg=#9696CE]\u{e0b1}#[fg=#BDC4E3] {detail} ".to_string(),
                prefix: "#[fg=#373A56,bg=#1C1C1C]\u{e0b6}".to_string(),
                suffix: "#[fg=#373A56,bg=#1C1C1C]\u{e0b4}#[default]".to_string(),
                colors: SegmentColors {
                    fg: Some("#BDC4E3".to_string()),
                    bg: Some("#373A56".to_string()),
                    outer_bg: Some("#1C1C1C".to_string()),
                },
                ..SegmentStyle::default()
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadgeStyle {
    #[default]
    Inline,
    Plain,
    Outer,
    Chip,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct SegmentColors {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub outer_bg: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslineCategoryConfig {
    pub mode: String,
    pub format: String,
    pub inactive_format: String,
    pub prefix: String,
    pub suffix: String,
    pub inactive_prefix: String,
    pub inactive_suffix: String,
    pub bold: bool,
    pub show_badge: bool,
    pub colors: SegmentColors,
    pub inactive_colors: SegmentColors,
}

impl Default for StatuslineCategoryConfig {
    fn default() -> Self {
        Self {
            mode: "list".to_string(),
            format: "{category} ".to_string(),
            inactive_format: "{category} ".to_string(),
            prefix: String::new(),
            suffix: String::new(),
            inactive_prefix: String::new(),
            inactive_suffix: String::new(),
            bold: false,
            show_badge: false,
            colors: SegmentColors::default(),
            inactive_colors: SegmentColors::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SummaryConfig {
    pub enabled: bool,
    pub hide_idle: bool,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hide_idle: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AttentionConfig {
    pub format: String,
    pub prefix: String,
    pub suffix: String,
    pub bold: bool,
    pub colors: SegmentColors,
}

impl Default for AttentionConfig {
    fn default() -> Self {
        Self {
            format: "{attention}".to_string(),
            prefix: String::new(),
            suffix: String::new(),
            bold: false,
            colors: SegmentColors {
                fg: Some("#ff6b6b".to_string()),
                bg: None,
                outer_bg: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionBadgeConfig {
    pub enabled: bool,
    pub mode: SessionBadgeMode,
    pub chip: SessionBadgeChipConfig,
    pub suffix: String,
    pub hide_idle: bool,
}

impl Default for SessionBadgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: SessionBadgeMode::Rollup,
            chip: SessionBadgeChipConfig::default(),
            suffix: String::new(),
            hide_idle: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionBadgeChipConfig {
    pub bg: String,
    pub cap_left: String,
    pub cap_right: String,
}

impl Default for SessionBadgeChipConfig {
    fn default() -> Self {
        Self {
            bg: "#303047".to_string(),
            cap_left: "\u{e0b6}".to_string(),
            cap_right: "\u{e0b4}".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBadgeMode {
    #[default]
    Rollup,
    Counts,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BadgeConfig {
    pub glyphs: BadgeGlyphs,
    pub colors: BadgeColors,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BadgeGlyphs {
    pub blocked: String,
    pub working: String,
    pub done: String,
    pub idle: String,
}

impl Default for BadgeGlyphs {
    fn default() -> Self {
        Self {
            blocked: "▲".to_string(),
            working: "●".to_string(),
            done: "✓".to_string(),
            idle: "○".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BadgeColors {
    pub blocked: String,
    pub working: String,
    pub done: String,
    pub idle: String,
}

impl Default for BadgeColors {
    fn default() -> Self {
        Self {
            blocked: "#ff6b6b".to_string(),
            working: "#4fd08a".to_string(),
            done: "#45cbe6".to_string(),
            idle: "#a8a8b2".to_string(),
        }
    }
}

impl BadgeColors {
    pub fn for_state(&self, state: &str) -> Option<&str> {
        match state {
            "blocked" => Some(self.blocked.as_str()),
            "working" => Some(self.working.as_str()),
            "done" => Some(self.done.as_str()),
            "idle" => Some(self.idle.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarConfig {
    pub width: SidebarWidth,
    pub min_width: u16,
    pub colors: SidebarColorsConfig,
    pub header: SidebarHeaderConfig,
    pub preview: SidebarPreviewConfig,
    pub live: SidebarLiveConfig,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            width: SidebarWidth::default(),
            min_width: 40,
            colors: SidebarColorsConfig::default(),
            header: SidebarHeaderConfig::default(),
            preview: SidebarPreviewConfig::default(),
            live: SidebarLiveConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarLiveConfig {
    pub enabled: bool,
    pub lines: u16,
    pub interval_ms: u64,
    pub cut_markers: Vec<String>,
}

impl Default for SidebarLiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lines: 3,
            interval_ms: 2000,
            cut_markers: [
                "╭",
                "? for shortcuts",
                "› ",
                "❯",
                "Ask Codex",
                "⏎ send",
                "context left",
                "new task?",
                "bypass permissions",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarPreviewConfig {
    pub history_lines: u32,
}

impl Default for SidebarPreviewConfig {
    fn default() -> Self {
        Self {
            history_lines: 2000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarColorsConfig {
    pub selection_bg: Option<String>,
    pub badge_blocked: Option<String>,
    pub badge_working: Option<String>,
    pub badge_done: Option<String>,
    pub badge_idle: Option<String>,
    pub header_active_bg: Option<String>,
    pub header_active_fg: Option<String>,
    pub header_chip_fg: Option<String>,
    pub header_filter_bg: Option<String>,
    pub header_total_bg: Option<String>,
    pub header_total_fg: Option<String>,
    pub detail: Option<String>,
    pub marker: Option<String>,
    pub toggle: Option<String>,
    pub category: Option<String>,
    pub header_mode: Option<String>,
    pub active_bg: Option<String>,
    pub active_bar: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub live: Option<String>,
    pub task_done: Option<String>,
    pub task_working: Option<String>,
    pub task_pending: Option<String>,
    pub task_label: Option<String>,
    pub subagent_label: Option<String>,
    pub subagent_id: Option<String>,
    pub worktree: Option<String>,
    pub worktree_activity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarHeaderConfig {
    pub format: String,
    pub prefix: String,
    pub suffix: String,
    pub bold: bool,
    pub colors: SegmentColors,
    pub chip_prefix: String,
    pub chip_suffix: String,
}

impl Default for SidebarHeaderConfig {
    fn default() -> Self {
        Self {
            format: " {label} ".to_string(),
            prefix: String::new(),
            suffix: "\u{e0b0}".to_string(),
            bold: true,
            colors: SegmentColors {
                fg: Some("16".to_string()),
                bg: Some("147".to_string()),
                outer_bg: Some("235".to_string()),
            },
            chip_prefix: String::new(),
            chip_suffix: String::new(),
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
        assert!(config.statusline.summary.enabled);
        assert_eq!(config.daemon.poll_ms, 1000);
        assert_eq!(config.daemon.git.timeout_ms, 500);
        assert_eq!(config.sidebar.width, SidebarWidth::Columns(40));
        assert_eq!(config.sidebar.min_width, 40);
        assert_eq!(config.popup.width, "50%");
        assert_eq!(config.popup.height, "50%");
        assert_eq!(config.statusline.category.mode, "list");
    }

    #[test]
    fn popup_size_defaults_and_overrides() {
        let config = Config::default();
        assert_eq!(config.popup.width, "50%");
        assert_eq!(config.popup.height, "50%");

        let config =
            serde_yaml_ng::from_str::<Config>("popup:\n  width: \"72%\"\n  height: \"60%\"\n")
                .unwrap();
        assert_eq!(config.popup.width, "72%");
        assert_eq!(config.popup.height, "60%");
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
    fn sidebar_preview_history_lines_defaults_to_2000() {
        let config = Config::default();
        assert_eq!(config.sidebar.preview.history_lines, 2000);

        let config =
            serde_yaml_ng::from_str::<Config>("sidebar:\n  preview:\n    history_lines: 5000\n")
                .unwrap();
        assert_eq!(config.sidebar.preview.history_lines, 5000);
    }

    #[test]
    fn sidebar_live_config_defaults_and_overrides() {
        let config = Config::default();
        assert!(config.sidebar.live.enabled);
        assert_eq!(config.sidebar.live.lines, 3);
        assert_eq!(config.sidebar.live.interval_ms, 2000);

        let config = serde_yaml_ng::from_str::<Config>(
            "sidebar:\n  live:\n    enabled: false\n    lines: 5\n    interval_ms: 750\n",
        )
        .unwrap();
        assert!(!config.sidebar.live.enabled);
        assert_eq!(config.sidebar.live.lines, 5);
        assert_eq!(config.sidebar.live.interval_ms, 750);
    }

    #[test]
    fn sidebar_colors_accept_ui_color_keys_and_reject_state_color_keys() {
        let config = serde_yaml_ng::from_str::<Config>(
            "sidebar:\n  colors:\n    selection_bg: \"237\"\n    header_active_bg: \"24\"\n    header_filter_bg: \"255\"\n",
        )
        .unwrap();

        assert_eq!(config.sidebar.colors.selection_bg.as_deref(), Some("237"));
        assert_eq!(
            config.sidebar.colors.header_active_bg.as_deref(),
            Some("24")
        );
        assert_eq!(
            config.sidebar.colors.header_filter_bg.as_deref(),
            Some("255")
        );

        for key in [
            "error",
            "running",
            "permission",
            "background",
            "waiting",
            "idle",
        ] {
            let yaml = format!("sidebar:\n  colors:\n    {key}: red\n");
            let err = serde_yaml_ng::from_str::<Config>(&yaml).unwrap_err();
            assert!(err.to_string().contains(key), "{err}");
        }
    }

    #[test]
    fn sidebar_colors_accept_task_subagent_and_worktree_detail_color_keys() {
        let config = serde_yaml_ng::from_str::<Config>(
            r##"
sidebar:
  colors:
    task_done: "220"
    task_working: "221"
    task_pending: darkgray
    task_label: "246"
    subagent_label: "73"
    subagent_id: "74"
    worktree: cyan
    worktree_activity: "#4fd08a"
"##,
        )
        .unwrap();

        assert_eq!(config.sidebar.colors.task_done.as_deref(), Some("220"));
        assert_eq!(config.sidebar.colors.task_working.as_deref(), Some("221"));
        assert_eq!(
            config.sidebar.colors.task_pending.as_deref(),
            Some("darkgray")
        );
        assert_eq!(config.sidebar.colors.task_label.as_deref(), Some("246"));
        assert_eq!(config.sidebar.colors.subagent_label.as_deref(), Some("73"));
        assert_eq!(config.sidebar.colors.subagent_id.as_deref(), Some("74"));
        assert_eq!(config.sidebar.colors.worktree.as_deref(), Some("cyan"));
        assert_eq!(
            config.sidebar.colors.worktree_activity.as_deref(),
            Some("#4fd08a")
        );
    }

    #[test]
    fn sidebar_colors_reject_dead_keys() {
        let attention =
            serde_yaml_ng::from_str::<Config>("sidebar:\n  colors:\n    attention: yellow\n")
                .unwrap_err();
        assert!(attention.to_string().contains("attention"));

        let active_bg = serde_yaml_ng::from_str::<Config>(
            "sidebar:\n  colors:\n    selection_active_bg: \"24\"\n",
        )
        .unwrap_err();
        assert!(active_bg.to_string().contains("selection_active_bg"));
    }

    #[test]
    fn sidebar_header_style_can_be_configured() {
        let config = serde_yaml_ng::from_str::<Config>(
            r##"
sidebar:
  header:
    format: " {label} "
    prefix: "["
    suffix: "]"
    bold: true
    colors:
      fg: white
      bg: "24"
      outer_bg: "235"
"##,
        )
        .unwrap();

        assert_eq!(config.sidebar.header.format, " {label} ");
        assert_eq!(config.sidebar.header.prefix, "[");
        assert_eq!(config.sidebar.header.suffix, "]");
        assert!(config.sidebar.header.bold);
        assert_eq!(config.sidebar.header.colors.fg.as_deref(), Some("white"));
        assert_eq!(config.sidebar.header.colors.bg.as_deref(), Some("24"));
        assert_eq!(
            config.sidebar.header.colors.outer_bg.as_deref(),
            Some("235")
        );
    }

    #[test]
    fn sidebar_header_rejects_removed_keys() {
        for key in ["powerline", "separator"] {
            let yaml = format!("sidebar:\n  header:\n    {key}: x\n");
            let err = serde_yaml_ng::from_str::<Config>(&yaml).unwrap_err();

            assert!(err.to_string().contains(key), "{err}");
        }
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
        assert_eq!(config.daemon.git.poll_interval_ms, 10_000);
        assert_eq!(config.statusline.sessions.current.format, " {session} ");
        assert_eq!(
            config.statusline.windows.current.format,
            " {index}:{window} "
        );
        assert!(config.statusline.windows.current.bold);
        assert_eq!(
            config.statusline.panes.current.format,
            " {pane} \u{e0b1} {detail} "
        );
    }

    #[test]
    fn statusline_windows_section_parses_styles_and_overlays() {
        let yaml = r##"
statusline:
  windows:
    separator: "#[fg=#8f8ba8]│#[default]"
    current:
      format: " {index}:{window} "
      bold: false
      colors:
        fg: "#20233a"
        bg: "#9d8cf5"
      prefix: "#[fg=#9d8cf5]"
      suffix: "#[fg=#9d8cf5,bg=default]#[default]"
    other:
      format: " {name} "
      colors:
        fg: "#9591ad"
    bell:
      fg: "#ff6b6b"
    activity:
      fg: "#ff6b6b"
"##;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();

        assert_eq!(
            config.statusline.windows.separator,
            "#[fg=#8f8ba8]│#[default]"
        );
        assert_eq!(
            config.statusline.windows.current.format,
            " {index}:{window} "
        );
        assert!(!config.statusline.windows.current.bold);
        assert_eq!(
            config.statusline.windows.current.colors.fg.as_deref(),
            Some("#20233a")
        );
        assert_eq!(
            config.statusline.windows.current.colors.bg.as_deref(),
            Some("#9d8cf5")
        );
        assert_eq!(config.statusline.windows.current.prefix, "#[fg=#9d8cf5]");
        assert_eq!(config.statusline.windows.other.format, " {name} ");
        assert_eq!(
            config.statusline.windows.other.colors.fg.as_deref(),
            Some("#9591ad")
        );
        assert_eq!(
            config.statusline.windows.bell.fg.as_deref(),
            Some("#ff6b6b")
        );
        assert_eq!(
            config.statusline.windows.activity.fg.as_deref(),
            Some("#ff6b6b")
        );
    }

    #[test]
    fn statusline_panes_section_parses_styles() {
        let yaml = r##"
statusline:
  panes:
    current:
      format: " {pane} | {detail} "
      bold: true
      colors:
        fg: "#cbc8dd"
        bg: "#3d3d5f"
      prefix: "#[fg=#3d3d5f]"
      suffix: "#[fg=#3d3d5f]#[default]"
    other:
      format: " {id} {process} "
      colors:
        fg: "#BDC4E3"
"##;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();

        assert_eq!(
            config.statusline.panes.current.format,
            " {pane} | {detail} "
        );
        assert!(config.statusline.panes.current.bold);
        assert_eq!(
            config.statusline.panes.current.colors.bg.as_deref(),
            Some("#3d3d5f")
        );
        assert_eq!(config.statusline.panes.current.prefix, "#[fg=#3d3d5f]");
        assert_eq!(config.statusline.panes.other.format, " {id} {process} ");
        assert_eq!(
            config.statusline.panes.other.colors.fg.as_deref(),
            Some("#BDC4E3")
        );
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
    fn session_badge_defaults_to_single_width_glyphs_with_empty_suffix() {
        let badge = BadgeConfig::default();
        let config = SessionBadgeConfig::default();
        assert!(config.enabled);
        assert_eq!(config.mode, SessionBadgeMode::Rollup);
        assert_eq!(config.chip.bg, "#303047");
        assert_eq!(config.chip.cap_left, "\u{e0b6}");
        assert_eq!(config.chip.cap_right, "\u{e0b4}");
        assert_eq!(config.suffix, "");
        assert!(!config.hide_idle);
        assert_eq!(badge.glyphs.blocked, "▲");
        assert_eq!(badge.glyphs.working, "●");
        assert_eq!(badge.glyphs.done, "✓");
        assert_eq!(badge.glyphs.idle, "○");
        assert_eq!(badge.colors.blocked, "#ff6b6b");
        assert_eq!(badge.colors.working, "#4fd08a");
        assert_eq!(badge.colors.done, "#45cbe6");
        assert_eq!(badge.colors.idle, "#a8a8b2");
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

    #[test]
    fn session_badge_mode_parses_counts() {
        let config =
            serde_yaml_ng::from_str::<Config>("statusline:\n  session_badge:\n    mode: counts\n")
                .unwrap();

        assert_eq!(
            config.statusline.session_badge.mode,
            SessionBadgeMode::Counts
        );
    }

    #[test]
    fn statusline_sessions_badge_style_and_session_badge_chip_parse() {
        let config = serde_yaml_ng::from_str::<Config>(
            r##"
statusline:
  sessions:
    badge_style: chip
  session_badge:
    chip:
      bg: "#30304a"
      cap_left: "<"
      cap_right: ">"
"##,
        )
        .unwrap();

        assert_eq!(config.statusline.sessions.badge_style, BadgeStyle::Chip);
        assert_eq!(config.statusline.session_badge.chip.bg, "#30304a");
        assert_eq!(config.statusline.session_badge.chip.cap_left, "<");
        assert_eq!(config.statusline.session_badge.chip.cap_right, ">");
    }

    #[test]
    fn badge_colors_can_be_overridden_in_yaml() {
        let config = serde_yaml_ng::from_str::<Config>(
            r##"
badge:
  colors:
    working: "#00ff00"
"##,
        )
        .unwrap();
        assert_eq!(config.badge.colors.working, "#00ff00");
        assert_eq!(config.badge.colors.blocked, "#ff6b6b");
    }

    #[test]
    fn notify_config_defaults_to_disabled_and_parses_top_level() {
        let config = Config::default();
        assert!(!config.notify.enabled);
        assert_eq!(config.notify.command, "");

        let config = serde_yaml_ng::from_str::<Config>(
            "notify:\n  enabled: true\n  command: \"printf blocked\"\n",
        )
        .unwrap();
        assert!(config.notify.enabled);
        assert_eq!(config.notify.command, "printf blocked");
    }
}

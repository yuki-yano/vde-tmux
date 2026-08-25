use crate::daemon::session_badge::{BadgeState, glyph_for_state};
use crate::hook::RollupLevel;
use ratatui::style::Color;

#[cfg(test)]
mod tests;

pub(super) const CODEX_AGENT_COLOR: Color = Color::Rgb(169, 142, 210);
pub(super) const CLAUDE_AGENT_COLOR: Color = Color::Rgb(127, 166, 195);
pub(super) const AGENT_ORIGIN_COLOR: Color = Color::Rgb(146, 191, 193);
pub(super) const RESPONSE_PREVIEW_COLOR: Color = Color::Rgb(166, 173, 200);
const POWERLINE_ARROW: &str = "\u{e0b0}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRenderTheme {
    pub selection_bg: Color,
    pub selection_bar: Color,
    pub header_active_bg: Option<Color>,
    pub header_active_fg: Option<Color>,
    pub header_chip_fg: Option<Color>,
    pub header_filter_bg: Option<Color>,
    pub header_total_bg: Option<Color>,
    pub header_total_fg: Option<Color>,
    pub header_active_bold: bool,
    pub header_badge_fg: Color,
    pub header_format: String,
    pub header_prefix: String,
    pub header_suffix: String,
    pub header_outer_bg: Option<Color>,
    pub header_chip_prefix: String,
    pub header_chip_suffix: String,
    pub badge_glyphs: crate::config::BadgeGlyphs,
    pub badge_blocked: Color,
    pub badge_limited: Color,
    pub badge_working: Color,
    pub badge_done: Color,
    pub badge_idle: Color,
    pub detail: Color,
    pub marker: Color,
    pub toggle: Color,
    pub category: Color,
    pub header_mode: Color,
    pub active_bg: Color,
    pub active_bar: Color,
    pub repo: Color,
    pub branch: Color,
    pub git_ahead: Color,
    pub git_behind: Color,
    pub git_insertions: Color,
    pub git_deletions: Color,
    pub task_done: Color,
    pub task_working: Color,
    pub task_pending: Color,
    pub task_label: Color,
    pub subagent_label: Color,
    pub subagent_id: Color,
    pub worktree: Color,
    pub worktree_activity: Color,
}

impl Default for SidebarRenderTheme {
    fn default() -> Self {
        Self {
            selection_bg: Color::Rgb(0x30, 0x30, 0x34),
            selection_bar: Color::Indexed(229),
            header_active_bg: None,
            header_active_fg: None,
            header_chip_fg: None,
            header_filter_bg: None,
            header_total_bg: None,
            header_total_fg: None,
            header_active_bold: false,
            header_badge_fg: Color::Indexed(16),
            header_format: " {label} ▾ ".to_string(),
            header_prefix: String::new(),
            header_suffix: POWERLINE_ARROW.to_string(),
            header_outer_bg: Some(Color::Indexed(235)),
            header_chip_prefix: String::new(),
            header_chip_suffix: String::new(),
            badge_glyphs: crate::config::BadgeGlyphs::default(),
            badge_blocked: Color::Red,
            badge_limited: Color::Rgb(0xf5, 0xa7, 0x42),
            badge_working: Color::Green,
            badge_done: Color::Cyan,
            badge_idle: Color::Indexed(248),
            detail: Color::Indexed(246),
            marker: Color::DarkGray,
            toggle: Color::Indexed(147),
            category: Color::Indexed(215),
            header_mode: Color::Indexed(147),
            active_bg: Color::Indexed(235),
            active_bar: Color::Indexed(147),
            repo: Color::LightCyan,
            branch: Color::Indexed(73),
            git_ahead: Color::Indexed(108),
            git_behind: Color::Indexed(179),
            git_insertions: Color::Indexed(78),
            git_deletions: Color::Indexed(174),
            task_done: Color::Indexed(220),
            task_working: Color::Indexed(220),
            task_pending: Color::DarkGray,
            task_label: Color::Indexed(246),
            subagent_label: Color::Indexed(73),
            subagent_id: Color::Indexed(73),
            worktree: Color::Indexed(73),
            worktree_activity: Color::Indexed(73),
        }
    }
}

impl SidebarRenderTheme {
    pub fn from_config(config: &crate::config::SidebarColorsConfig) -> Self {
        let default = Self::default();
        Self {
            selection_bg: parse_color(config.selection_bg.as_deref())
                .unwrap_or(default.selection_bg),
            selection_bar: parse_color(config.selection_bar.as_deref())
                .unwrap_or(default.selection_bar),
            header_active_bg: parse_color(config.header_active_bg.as_deref()),
            header_active_fg: parse_color(config.header_active_fg.as_deref()),
            header_chip_fg: parse_color(config.header_chip_fg.as_deref()),
            header_filter_bg: parse_color(config.header_filter_bg.as_deref()),
            header_total_bg: parse_color(config.header_total_bg.as_deref()),
            header_total_fg: parse_color(config.header_total_fg.as_deref()),
            header_active_bold: default.header_active_bold,
            header_badge_fg: default.header_badge_fg,
            header_format: default.header_format,
            header_prefix: default.header_prefix,
            header_suffix: default.header_suffix,
            header_outer_bg: default.header_outer_bg,
            header_chip_prefix: default.header_chip_prefix,
            header_chip_suffix: default.header_chip_suffix,
            badge_glyphs: default.badge_glyphs,
            badge_blocked: parse_color(config.badge_blocked.as_deref())
                .unwrap_or(default.badge_blocked),
            badge_limited: parse_color(config.badge_limited.as_deref())
                .unwrap_or(default.badge_limited),
            badge_working: parse_color(config.badge_working.as_deref())
                .unwrap_or(default.badge_working),
            badge_done: parse_color(config.badge_done.as_deref()).unwrap_or(default.badge_done),
            badge_idle: parse_color(config.badge_idle.as_deref()).unwrap_or(default.badge_idle),
            detail: parse_color(config.detail.as_deref()).unwrap_or(default.detail),
            marker: parse_color(config.marker.as_deref()).unwrap_or(default.marker),
            toggle: parse_color(config.toggle.as_deref()).unwrap_or(default.toggle),
            category: parse_color(config.category.as_deref()).unwrap_or(default.category),
            header_mode: parse_color(config.header_mode.as_deref()).unwrap_or(default.header_mode),
            active_bg: parse_color(config.active_bg.as_deref()).unwrap_or(default.active_bg),
            active_bar: parse_color(config.active_bar.as_deref()).unwrap_or(default.active_bar),
            repo: parse_color(config.repo.as_deref()).unwrap_or(default.repo),
            branch: parse_color(config.branch.as_deref()).unwrap_or(default.branch),
            git_ahead: parse_color(config.git_ahead.as_deref()).unwrap_or(default.git_ahead),
            git_behind: parse_color(config.git_behind.as_deref()).unwrap_or(default.git_behind),
            git_insertions: parse_color(config.git_insertions.as_deref())
                .unwrap_or(default.git_insertions),
            git_deletions: parse_color(config.git_deletions.as_deref())
                .unwrap_or(default.git_deletions),
            task_done: parse_color(config.task_done.as_deref()).unwrap_or(default.task_done),
            task_working: parse_color(config.task_working.as_deref())
                .unwrap_or(default.task_working),
            task_pending: parse_color(config.task_pending.as_deref())
                .unwrap_or(default.task_pending),
            task_label: parse_color(config.task_label.as_deref()).unwrap_or(default.task_label),
            subagent_label: parse_color(config.subagent_label.as_deref())
                .unwrap_or(default.subagent_label),
            subagent_id: parse_color(config.subagent_id.as_deref()).unwrap_or(default.subagent_id),
            worktree: parse_color(config.worktree.as_deref()).unwrap_or(default.worktree),
            worktree_activity: parse_color(config.worktree_activity.as_deref())
                .unwrap_or(default.worktree_activity),
        }
    }

    pub fn from_sidebar_config(config: &crate::config::SidebarConfig) -> Self {
        let mut theme = Self::from_config(&config.colors);
        theme.header_active_fg =
            parse_color(config.header.colors.fg.as_deref()).or(theme.header_active_fg);
        theme.header_active_bg =
            parse_color(config.header.colors.bg.as_deref()).or(theme.header_active_bg);
        theme.header_outer_bg =
            parse_color(config.header.colors.outer_bg.as_deref()).or(theme.header_outer_bg);
        theme.header_active_bold = config.header.bold;
        theme.header_format = config.header.format.clone();
        theme.header_prefix = config.header.prefix.clone();
        theme.header_suffix = config.header.suffix.clone();
        theme.header_chip_prefix = config.header.chip_prefix.clone();
        theme.header_chip_suffix = config.header.chip_suffix.clone();
        theme
    }

    pub fn from_app_config(config: &crate::config::Config) -> Self {
        let mut theme = Self::from_sidebar_config(&config.sidebar);
        theme.badge_glyphs = config.badge.glyphs.clone();
        let badge = &config.badge.colors;
        let overrides = &config.sidebar.colors;
        theme.badge_blocked = parse_color(overrides.badge_blocked.as_deref())
            .or_else(|| parse_color(Some(&badge.blocked)))
            .unwrap_or(theme.badge_blocked);
        theme.badge_limited = parse_color(overrides.badge_limited.as_deref())
            .or_else(|| parse_color(Some(&badge.limited)))
            .unwrap_or(theme.badge_limited);
        theme.badge_working = parse_color(overrides.badge_working.as_deref())
            .or_else(|| parse_color(Some(&badge.working)))
            .unwrap_or(theme.badge_working);
        theme.badge_done = parse_color(overrides.badge_done.as_deref())
            .or_else(|| parse_color(Some(&badge.done)))
            .unwrap_or(theme.badge_done);
        theme.badge_idle = parse_color(overrides.badge_idle.as_deref())
            .or_else(|| parse_color(Some(&badge.idle)))
            .unwrap_or(theme.badge_idle);
        theme
    }

    pub(super) fn rollup_color(&self, level: RollupLevel) -> Color {
        match level {
            RollupLevel::Error | RollupLevel::Permission | RollupLevel::Waiting => {
                self.badge_color(BadgeState::Blocked)
            }
            RollupLevel::Limited => self.badge_color(BadgeState::Limited),
            RollupLevel::Running => self.badge_color(BadgeState::Working),
            RollupLevel::Background | RollupLevel::Idle => self.badge_color(BadgeState::Idle),
        }
    }

    pub(crate) fn badge_glyph(&self, state: BadgeState) -> &str {
        glyph_for_state(state, &self.badge_glyphs)
    }

    pub(crate) fn badge_color(&self, state: BadgeState) -> Color {
        match state {
            BadgeState::Blocked => self.badge_blocked,
            BadgeState::Limited => self.badge_limited,
            BadgeState::Working => self.badge_working,
            BadgeState::Done => self.badge_done,
            BadgeState::Idle => self.badge_idle,
        }
    }
}

fn parse_color(raw: Option<&str>) -> Option<Color> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(index) = raw.parse::<u8>() {
        return Some(Color::Indexed(index));
    }
    if let Some(hex) = raw.strip_prefix('#')
        && hex.len() == 6
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(red, green, blue));
    }
    match raw.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
        "reset" | "default" => Some(Color::Reset),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "lightred" => Some(Color::LightRed),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "cyan" => Some(Color::Cyan),
        "magenta" => Some(Color::Magenta),
        "white" => Some(Color::White),
        "black" => Some(Color::Black),
        _ => None,
    }
}

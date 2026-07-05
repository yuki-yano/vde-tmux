use crate::daemon::session_badge::{BadgeState, glyph_for_state};
use crate::hook::RollupLevel;
use crate::sidebar::state::{SidebarState, StatusFilter, ViewMode};
use crate::sidebar::tree::{SidebarRow, SidebarRowKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRenderTheme {
    pub error: Color,
    pub running: Color,
    pub permission: Color,
    pub background: Color,
    pub waiting: Color,
    pub idle: Color,
    pub selection_bg: Color,
    pub header_active_bg: Option<Color>,
    pub header_active_fg: Option<Color>,
    pub header_active_bold: bool,
    pub header_format: String,
    pub header_prefix: String,
    pub header_suffix: String,
    pub header_separator: String,
    pub badge_glyphs: crate::config::BadgeGlyphs,
    pub badge_blocked: Color,
    pub badge_working: Color,
    pub badge_done: Color,
    pub badge_idle: Color,
}

impl Default for SidebarRenderTheme {
    fn default() -> Self {
        Self {
            error: Color::Red,
            running: Color::Green,
            permission: Color::LightRed,
            background: Color::DarkGray,
            waiting: Color::Yellow,
            idle: Color::Reset,
            selection_bg: Color::Indexed(237),
            header_active_bg: None,
            header_active_fg: None,
            header_active_bold: false,
            header_format: "{label}".to_string(),
            header_prefix: String::new(),
            header_suffix: String::new(),
            header_separator: String::new(),
            badge_glyphs: crate::config::BadgeGlyphs::default(),
            badge_blocked: Color::Red,
            badge_working: Color::Green,
            badge_done: Color::Cyan,
            badge_idle: Color::DarkGray,
        }
    }
}

impl SidebarRenderTheme {
    pub fn from_config(config: &crate::config::SidebarColorsConfig) -> Self {
        let default = Self::default();
        Self {
            error: parse_color(config.error.as_deref()).unwrap_or(default.error),
            running: parse_color(config.running.as_deref()).unwrap_or(default.running),
            permission: parse_color(config.permission.as_deref()).unwrap_or(default.permission),
            background: parse_color(config.background.as_deref()).unwrap_or(default.background),
            waiting: parse_color(config.waiting.as_deref()).unwrap_or(default.waiting),
            idle: parse_color(config.idle.as_deref()).unwrap_or(default.idle),
            selection_bg: parse_color(config.selection_bg.as_deref())
                .unwrap_or(default.selection_bg),
            header_active_bg: parse_color(config.header_active_bg.as_deref()),
            header_active_fg: parse_color(config.header_active_fg.as_deref()),
            header_active_bold: default.header_active_bold,
            header_format: default.header_format,
            header_prefix: default.header_prefix,
            header_suffix: default.header_suffix,
            header_separator: default.header_separator,
            badge_glyphs: default.badge_glyphs,
            badge_blocked: parse_color(config.badge_blocked.as_deref())
                .unwrap_or(default.badge_blocked),
            badge_working: parse_color(config.badge_working.as_deref())
                .unwrap_or(default.badge_working),
            badge_done: parse_color(config.badge_done.as_deref()).unwrap_or(default.badge_done),
            badge_idle: parse_color(config.badge_idle.as_deref()).unwrap_or(default.badge_idle),
        }
    }

    pub fn from_sidebar_config(config: &crate::config::SidebarConfig) -> Self {
        let mut theme = Self::from_config(&config.colors);
        theme.header_active_fg =
            parse_color(config.header.colors.fg.as_deref()).or(theme.header_active_fg);
        theme.header_active_bg =
            parse_color(config.header.colors.bg.as_deref()).or(theme.header_active_bg);
        theme.header_active_bold = config.header.bold;
        theme.header_format = config.header.format.clone();
        theme.header_prefix = config.header.prefix.clone();
        theme.header_suffix = config.header.suffix.clone();
        theme.header_separator = config.header.separator.clone();
        theme
    }

    pub fn from_app_config(config: &crate::config::Config) -> Self {
        let mut theme = Self::from_sidebar_config(&config.sidebar);
        theme.badge_glyphs = config.badge.glyphs.clone();
        theme
    }

    fn rollup_color(&self, level: RollupLevel) -> Color {
        match level {
            RollupLevel::Error => self.error,
            RollupLevel::Running => self.running,
            RollupLevel::Permission => self.permission,
            RollupLevel::Background => self.background,
            RollupLevel::Waiting => self.waiting,
            RollupLevel::Idle => self.idle,
        }
    }

    fn badge_glyph(&self, state: BadgeState) -> &str {
        glyph_for_state(state, &self.badge_glyphs)
    }

    pub(crate) fn badge_color(&self, state: BadgeState) -> Color {
        match state {
            BadgeState::Blocked => self.badge_blocked,
            BadgeState::Working => self.badge_working,
            BadgeState::Done => self.badge_done,
            BadgeState::Idle => self.badge_idle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderAction {
    CycleViewMode,
    ToggleFilter,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeaderLayout {
    pub lines: Vec<HeaderLine>,
}

impl HeaderLayout {
    pub fn row_count(&self) -> u16 {
        self.lines.len() as u16
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderLine {
    pub text: String,
    pub segments: Vec<HeaderSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderSegment {
    pub range: std::ops::Range<u16>,
    pub action: HeaderAction,
}

pub fn build_header_layout(state: &SidebarState, width: u16) -> HeaderLayout {
    build_header_layout_with_theme(state, width, &SidebarRenderTheme::default())
}

pub fn build_header_layout_with_theme(
    state: &SidebarState,
    width: u16,
    theme: &SidebarRenderTheme,
) -> HeaderLayout {
    if width <= 2 {
        return HeaderLayout::default();
    }
    let mode_badge = format_header_segment(view_mode_label(state.view_mode), theme);
    let filter_badge = format_header_segment(filter_label(state.filter), theme);
    let separator = if theme.header_separator.is_empty() {
        " · ".to_string()
    } else {
        theme.header_separator.clone()
    };
    let full_text = format!(" {mode_badge}{separator}{filter_badge}");
    let text = truncate_display(&full_text, width as usize);
    let mut segments = Vec::new();
    let mode_len = mode_badge.chars().count();
    let separator_len = separator.chars().count();
    if let Some(range) = visible_segment_range(&text, 1, mode_len) {
        segments.push(HeaderSegment {
            range,
            action: HeaderAction::CycleViewMode,
        });
    }
    if let Some(range) = visible_segment_range(
        &text,
        1 + mode_len + separator_len,
        filter_badge.chars().count(),
    ) {
        segments.push(HeaderSegment {
            range,
            action: HeaderAction::ToggleFilter,
        });
    }
    HeaderLayout {
        lines: vec![HeaderLine { text, segments }],
    }
}

fn format_header_segment(label: &str, theme: &SidebarRenderTheme) -> String {
    let body = theme.header_format.replace("{label}", label);
    format!("{}{}{}", theme.header_prefix, body, theme.header_suffix)
}

pub fn header_hit_test(layout: &HeaderLayout, row: u16, column: u16) -> Option<HeaderAction> {
    layout
        .lines
        .get(row as usize)?
        .segments
        .iter()
        .find(|segment| segment.range.contains(&column))
        .map(|segment| segment.action)
}

pub fn render_header_lines(
    layout: &HeaderLayout,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    layout
        .lines
        .iter()
        .map(|line| {
            let mut spans = Vec::new();
            let mut cursor = 0_u16;
            for segment in &line.segments {
                if cursor < segment.range.start {
                    spans.push(Span::raw(slice_chars(
                        &line.text,
                        cursor,
                        segment.range.start,
                    )));
                }
                spans.push(Span::styled(
                    slice_chars(&line.text, segment.range.start, segment.range.end),
                    header_segment_style(theme),
                ));
                cursor = segment.range.end;
            }
            let text_len = line.text.chars().count() as u16;
            if cursor < text_len {
                spans.push(Span::raw(slice_chars(&line.text, cursor, text_len)));
            }
            Line::from(spans)
        })
        .collect()
}

pub fn build_footer_line(width: usize) -> Line<'static> {
    let text = truncate_display(" j/k move  enter jump  tab filter", width);
    Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    ))
}

fn header_segment_style(theme: &SidebarRenderTheme) -> Style {
    let mut style = Style::default();
    if let Some(fg) = theme.header_active_fg {
        style = style.fg(fg);
    }
    if let Some(bg) = theme.header_active_bg {
        style = style.bg(bg);
    }
    if theme.header_active_bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

pub fn render_rows(rows: &[SidebarRow], state: &SidebarState, width: usize) -> String {
    render_lines(rows, state, width, &SidebarRenderTheme::default())
        .into_iter()
        .map(line_to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_lines(
    rows: &[SidebarRow],
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    if width <= 2 {
        return render_rail_lines(rows, state, theme);
    }
    rows.iter()
        .map(|row| render_row_line(row, state, width, theme))
        .collect()
}

fn render_row_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = state.selection.as_deref() == Some(row.id.as_str());
    let style = row_style(row, theme);
    let content_width = width.saturating_sub(2);

    let indent = "  ".repeat(row.depth);
    let head = match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Chat => {
            let marker = if row.expanded { "▾" } else { "▸" };
            format!("{indent}{marker} ")
        }
        SidebarRowKind::Detail => indent.clone(),
        SidebarRowKind::Jump => format!("{indent}-> "),
    };
    let badge = if row.kind == SidebarRowKind::Chat {
        row.badge_state.map(|state| {
            (
                format!("{} ", theme.badge_glyph(state)),
                theme.badge_color(state),
            )
        })
    } else {
        None
    };
    let git = if row.kind == SidebarRowKind::Repo {
        row.git
            .as_ref()
            .map(format_git_badge_parts)
            .filter(|git| !git.branch.is_empty())
    } else {
        None
    };
    let right = right_label(row);

    let badge_width = badge
        .as_ref()
        .map(|(text, _)| display_width(text))
        .unwrap_or(0);
    let git_width = git.as_ref().map(git_badge_width).unwrap_or(0);
    let right_width = right.as_deref().map(display_width).unwrap_or(0);
    let right_reserved = if right_width > 0 { right_width + 1 } else { 0 };
    let label_budget = content_width
        .saturating_sub(display_width(&head))
        .saturating_sub(badge_width)
        .saturating_sub(git_width)
        .saturating_sub(right_reserved);
    let label = truncate_display(&row.label, label_budget);

    let mut spans = vec![Span::styled(format!(" {head}"), style)];
    if let Some((glyph, color)) = badge {
        spans.push(Span::styled(glyph, Style::default().fg(color)));
    }
    spans.push(Span::styled(label, style));
    if let Some(git) = &git {
        spans.push(Span::styled(format!(" {}", git.branch), style));
        if let Some(ahead) = &git.ahead {
            spans.push(Span::styled(format!(" {ahead}"), style.fg(Color::Green)));
        }
        if let Some(behind) = &git.behind {
            spans.push(Span::styled(format!(" {behind}"), style.fg(Color::Red)));
        }
    }
    let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    let filler = width
        .saturating_sub(1)
        .saturating_sub(used)
        .saturating_sub(right_width);
    spans.push(Span::raw(" ".repeat(filler)));
    if let Some(right) = right {
        spans.push(Span::styled(right, right_style(row, theme)));
    }
    spans.push(Span::raw(" "));

    let mut line = Line::from(spans);
    if selected {
        line = line.style(
            Style::default()
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        );
    }
    line
}

struct GitBadgeText {
    branch: String,
    ahead: Option<String>,
    behind: Option<String>,
}

fn format_git_badge_parts(badge: &crate::git::GitBadge) -> GitBadgeText {
    let mut parts = vec![badge.branch.clone()];
    let branch = parts.remove(0);
    GitBadgeText {
        branch,
        ahead: (badge.ahead > 0).then(|| format!("+{}", badge.ahead)),
        behind: (badge.behind > 0).then(|| format!("-{}", badge.behind)),
    }
}

fn render_rail_lines(
    rows: &[SidebarRow],
    state: &SidebarState,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    rows.iter()
        .filter(|row| matches!(row.kind, SidebarRowKind::Chat | SidebarRowKind::Jump))
        .map(|row| {
            let mut style = Style::default().fg(theme.rollup_color(row.rollup));
            if state.selection.as_deref() == Some(row.id.as_str()) {
                style = style.bg(theme.selection_bg).add_modifier(Modifier::BOLD);
            }
            let glyph = row.badge_state.expect("rail rows must carry badge_state");
            Line::from(Span::styled(theme.badge_glyph(glyph).to_string(), style))
        })
        .collect()
}

pub(crate) fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

pub(crate) fn truncate_display(text: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_string();
    }
    let budget = max_width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

fn right_label(row: &SidebarRow) -> Option<String> {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            let count = row.meta.as_ref()?.attention_count?;
            (count > 0).then(|| format!("▲{count}"))
        }
        SidebarRowKind::Chat => match row.rollup {
            RollupLevel::Error => Some("err".to_string()),
            RollupLevel::Permission => Some("perm".to_string()),
            RollupLevel::Waiting => Some("wait".to_string()),
            RollupLevel::Background => Some("bg".to_string()),
            RollupLevel::Running => row
                .meta
                .as_ref()
                .and_then(|meta| meta.elapsed_secs)
                .map(elapsed_label),
            RollupLevel::Idle => None,
        },
        SidebarRowKind::Detail | SidebarRowKind::Jump => None,
    }
}

fn elapsed_label(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

fn right_style(row: &SidebarRow, theme: &SidebarRenderTheme) -> Style {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            Style::default().fg(theme.badge_color(BadgeState::Blocked))
        }
        _ => Style::default()
            .fg(theme.rollup_color(row.rollup))
            .add_modifier(Modifier::DIM),
    }
}

fn git_badge_width(git: &GitBadgeText) -> usize {
    let mut width = 1 + display_width(&git.branch);
    if let Some(ahead) = &git.ahead {
        width += 1 + display_width(ahead);
    }
    if let Some(behind) = &git.behind {
        width += 1 + display_width(behind);
    }
    width
}

fn row_style(row: &SidebarRow, theme: &SidebarRenderTheme) -> Style {
    let style = Style::default().fg(match row.kind {
        SidebarRowKind::Category => Color::Blue,
        SidebarRowKind::Repo
        | SidebarRowKind::Chat
        | SidebarRowKind::Detail
        | SidebarRowKind::Jump => theme.rollup_color(row.rollup),
    });
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => style.add_modifier(Modifier::BOLD),
        SidebarRowKind::Detail => style.add_modifier(Modifier::DIM),
        SidebarRowKind::Jump => style.fg(Color::Cyan),
        SidebarRowKind::Chat => style,
    }
}

fn line_to_string(line: Line<'_>) -> String {
    line.spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
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

fn view_mode_label(view_mode: ViewMode) -> &'static str {
    match view_mode {
        ViewMode::Flat => "flat",
        ViewMode::ByRepo => "repo",
        ViewMode::ByCategory => "category",
    }
}

fn filter_label(filter: StatusFilter) -> &'static str {
    match filter {
        StatusFilter::All => "all",
        StatusFilter::AttentionOnly => "attention",
    }
}

fn visible_segment_range(text: &str, start: usize, len: usize) -> Option<std::ops::Range<u16>> {
    let visible_len = text.chars().count();
    if start >= visible_len {
        return None;
    }
    let end = (start + len).min(visible_len);
    Some(start as u16..end as u16)
}

fn slice_chars(text: &str, start: u16, end: u16) -> String {
    text.chars()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::RollupLevel;
    use crate::sidebar::state::SidebarState;
    use crate::sidebar::tree::{SidebarRow, SidebarRowKind};

    fn row(
        id: &str,
        kind: SidebarRowKind,
        depth: usize,
        label: &str,
        rollup: RollupLevel,
    ) -> SidebarRow {
        SidebarRow {
            id: id.to_string(),
            kind,
            depth,
            label: label.to_string(),
            chat_count: 1,
            rollup,
            badge_state: None,
            expanded: true,
            pane_id: None,
            git: None,
            meta: None,
        }
    }

    #[test]
    fn render_rows_includes_selection_indentation_and_rollup() {
        let rows = vec![
            row(
                "repo::misc::app",
                SidebarRowKind::Repo,
                0,
                "app",
                RollupLevel::Running,
            ),
            row(
                "chat::%1",
                SidebarRowKind::Chat,
                1,
                "codex %1",
                RollupLevel::Running,
            ),
        ];
        let state = SidebarState {
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };

        let rendered = render_rows(&rows, &state, 32);

        assert!(rendered.contains(" ▾ app"));
        assert!(rendered.contains("   ▾ codex %1"));
        assert!(!rendered.contains("> "));
    }

    #[test]
    fn render_rows_uses_rail_for_narrow_width() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex %1",
            RollupLevel::Permission,
        );
        chat.badge_state = Some(BadgeState::Blocked);
        let rows = vec![chat];
        let rendered = render_rows(&rows, &SidebarState::default(), 2);
        assert_eq!(rendered, "▲");
    }

    #[test]
    fn render_repo_row_includes_git_badge() {
        let mut repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Running,
        );
        repo.git = Some(crate::git::GitBadge {
            branch: "main".to_string(),
            ahead: 2,
            behind: 1,
        });

        let rendered = render_rows(&[repo], &SidebarState::default(), 80);

        assert!(rendered.contains("main +2 -1"));
    }

    #[test]
    fn render_repo_row_omits_zero_git_counts() {
        let mut repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Idle,
        );
        repo.git = Some(crate::git::GitBadge {
            branch: "main".to_string(),
            ahead: 0,
            behind: 0,
        });

        let rendered = render_rows(&[repo], &SidebarState::default(), 80);

        assert!(rendered.contains("▾ app main"));
        assert!(!rendered.contains("+0"));
        assert!(!rendered.contains("-0"));
    }

    #[test]
    fn render_lines_color_rollup_category_selection_and_git_badges() {
        let mut repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Running,
        );
        repo.git = Some(crate::git::GitBadge {
            branch: "main".to_string(),
            ahead: 2,
            behind: 1,
        });
        let category = row(
            "category::misc",
            SidebarRowKind::Category,
            0,
            "misc",
            RollupLevel::Idle,
        );
        let state = SidebarState {
            selection: Some("repo::misc::app".to_string()),
            ..SidebarState::default()
        };

        let lines = render_lines(
            &[category, repo],
            &state,
            80,
            &SidebarRenderTheme::default(),
        );

        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Blue));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[1].style.bg, Some(Color::Indexed(237)));
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| { span.content.trim() == "+2" && span.style.fg == Some(Color::Green) })
        );
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| { span.content.trim() == "-1" && span.style.fg == Some(Color::Red) })
        );
    }

    #[test]
    fn header_layout_shows_current_values_only_and_hit_tests_tokens() {
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            filter: StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };

        let header = build_header_layout(&state, 80);

        assert_eq!(header.lines[0].text, " category · attention");
        assert_eq!(
            header_hit_test(&header, 0, 2),
            Some(HeaderAction::CycleViewMode)
        );
        assert_eq!(
            header_hit_test(&header, 0, 13),
            Some(HeaderAction::ToggleFilter)
        );
    }

    #[test]
    fn header_layout_defaults_to_compact_dot_separated_segments() {
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            filter: StatusFilter::All,
            ..SidebarState::default()
        };

        let header = build_header_layout(&state, 80);

        assert_eq!(header.lines[0].text, " repo · all");
        assert_eq!(header.lines[0].segments[0].range, 1..5);
        assert_eq!(header.lines[0].segments[1].range, 8..11);
        assert_eq!(
            header_hit_test(&header, 0, 2),
            Some(HeaderAction::CycleViewMode)
        );
        assert_eq!(
            header_hit_test(&header, 0, 9),
            Some(HeaderAction::ToggleFilter)
        );
        assert_eq!(header_hit_test(&header, 0, 6), None);
    }

    #[test]
    fn header_layout_can_be_configured_as_pill_buttons() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
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
        let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            filter: StatusFilter::All,
            ..SidebarState::default()
        };

        let header = build_header_layout_with_theme(&state, 80, &theme);
        let lines = render_header_lines(&header, &theme);

        assert_eq!(header.lines[0].text, " [ repo ] [ all ]");
        assert_eq!(header.lines[0].segments[0].range, 1..9);
        assert_eq!(header.lines[0].segments[1].range, 10..17);
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::White));
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Indexed(24)));
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn chat_rows_render_badge_glyph_and_omit_trailing_status_text() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex (%1)",
            RollupLevel::Running,
        );
        chat.badge_state = Some(crate::daemon::session_badge::BadgeState::Working);

        let rendered = render_rows(&[chat], &SidebarState::default(), 80);

        assert!(rendered.contains("● codex (%1)"), "{rendered}");
        assert!(!rendered.contains("[running]"), "{rendered}");
    }

    #[test]
    fn rail_uses_badge_glyphs() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Idle,
        );
        chat.badge_state = Some(crate::daemon::session_badge::BadgeState::Done);

        let rendered = render_rows(&[chat], &SidebarState::default(), 2);

        assert_eq!(rendered, "✓");
    }

    #[test]
    fn display_width_counts_cjk_as_two_cells() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("あいう"), 6);
        assert_eq!(display_width("a…"), 2);
    }

    #[test]
    fn truncate_display_appends_ellipsis_within_width() {
        assert_eq!(truncate_display("hello", 10), "hello");
        assert_eq!(truncate_display("hello world", 8), "hello w…");
        assert_eq!(truncate_display("あいうえお", 7), "あいう…");
        assert_eq!(truncate_display("abc", 0), "");
    }

    #[test]
    fn theme_maps_badge_states_to_default_colors() {
        let theme = SidebarRenderTheme::default();
        assert_eq!(theme.badge_color(BadgeState::Blocked), Color::Red);
        assert_eq!(theme.badge_color(BadgeState::Working), Color::Green);
        assert_eq!(theme.badge_color(BadgeState::Done), Color::Cyan);
        assert_eq!(theme.badge_color(BadgeState::Idle), Color::DarkGray);
    }

    #[test]
    fn badge_colors_are_configurable() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
            r##"
sidebar:
  colors:
    badge_working: yellow
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
        assert_eq!(theme.badge_color(BadgeState::Working), Color::Yellow);
        assert_eq!(theme.badge_color(BadgeState::Blocked), Color::Red);
    }

    #[test]
    fn rows_have_horizontal_padding_and_no_selection_marker() {
        let rows = vec![row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Running,
        )];
        let state = SidebarState {
            selection: Some("repo::misc::app".to_string()),
            ..SidebarState::default()
        };
        let rendered = render_rows(&rows, &state, 20);
        assert!(rendered.starts_with(" ▾ app"), "{rendered:?}");
        assert!(!rendered.contains("> "), "{rendered:?}");
        assert_eq!(display_width(&rendered), 20, "{rendered:?}");
    }

    #[test]
    fn repo_row_right_aligns_attention_count() {
        let mut repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Permission,
        );
        repo.meta = Some(crate::sidebar::tree::RowMeta {
            attention_count: Some(2),
            ..Default::default()
        });
        let rendered = render_rows(&[repo], &SidebarState::default(), 20);
        assert!(rendered.ends_with("▲2 "), "{rendered:?}");
        assert!(!rendered.contains("[permission:"), "{rendered:?}");
    }

    #[test]
    fn chat_row_right_aligns_status_short_label() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: review PR",
            RollupLevel::Permission,
        );
        chat.badge_state = Some(BadgeState::Blocked);
        let rendered = render_rows(&[chat], &SidebarState::default(), 30);
        assert!(rendered.ends_with("perm "), "{rendered:?}");
        assert!(rendered.contains("▲ codex: review PR"), "{rendered:?}");
    }

    #[test]
    fn chat_row_shows_elapsed_when_running() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: fix",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            elapsed_secs: Some(815),
            ..Default::default()
        });
        let rendered = render_rows(&[chat], &SidebarState::default(), 30);
        assert!(rendered.ends_with("13m "), "{rendered:?}");
    }

    #[test]
    fn long_cjk_label_is_truncated_with_ellipsis_keeping_right_column() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: 日本語のとても長いプロンプトを表示する",
            RollupLevel::Permission,
        );
        chat.badge_state = Some(BadgeState::Blocked);
        let rendered = render_rows(&[chat], &SidebarState::default(), 24);
        assert!(rendered.contains('…'), "{rendered:?}");
        assert!(rendered.ends_with("perm "), "{rendered:?}");
        assert_eq!(display_width(&rendered), 24, "{rendered:?}");
    }

    #[test]
    fn badge_glyph_is_rendered_in_badge_color_span() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        let lines = render_lines(
            &[chat],
            &SidebarState::default(),
            30,
            &SidebarRenderTheme::default(),
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.contains('●') && span.style.fg == Some(Color::Green)),
            "{lines:?}"
        );
    }
}

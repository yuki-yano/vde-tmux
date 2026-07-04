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
    pub selection_active_bg: Color,
    pub header_active_bg: Option<Color>,
    pub header_active_fg: Option<Color>,
    pub header_active_bold: bool,
    pub header_format: String,
    pub header_prefix: String,
    pub header_suffix: String,
    pub header_separator: String,
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
            selection_active_bg: Color::Indexed(24),
            header_active_bg: None,
            header_active_fg: None,
            header_active_bold: false,
            header_format: "{label} ".to_string(),
            header_prefix: String::new(),
            header_suffix: String::new(),
            header_separator: String::new(),
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
            selection_active_bg: parse_color(config.selection_active_bg.as_deref())
                .unwrap_or(default.selection_active_bg),
            header_active_bg: parse_color(config.header_active_bg.as_deref()),
            header_active_fg: parse_color(config.header_active_fg.as_deref()),
            header_active_bold: default.header_active_bold,
            header_format: default.header_format,
            header_prefix: default.header_prefix,
            header_suffix: default.header_suffix,
            header_separator: default.header_separator,
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

const VIEW_MODE_BADGE_WIDTH: usize = "category".len();
const FILTER_BADGE_WIDTH: usize = "attn".len();

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
    let mode = view_mode_label(state.view_mode);
    let filter = filter_label(state.filter);
    let mode_badge = format_header_segment(mode, VIEW_MODE_BADGE_WIDTH, theme);
    let filter_badge = format_header_segment(filter, FILTER_BADGE_WIDTH, theme);
    let full_text = format!("{}{}{}", mode_badge, theme.header_separator, filter_badge);
    let text = truncate_width(&full_text, width as usize);
    let mut segments = Vec::new();
    let mode_len = mode_badge.chars().count();
    let separator_len = theme.header_separator.chars().count();
    if let Some(range) = visible_segment_range(&text, 0, mode_len) {
        segments.push(HeaderSegment {
            range,
            action: HeaderAction::CycleViewMode,
        });
    }
    if let Some(range) = visible_segment_range(
        &text,
        mode_len + separator_len,
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

fn format_header_segment(label: &str, width: usize, theme: &SidebarRenderTheme) -> String {
    let label = format!("{label:<width$}");
    let body = theme.header_format.replace("{label}", &label);
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
    let mut style = row_style(row, theme);
    if selected {
        style = style.bg(theme.selection_bg).add_modifier(Modifier::BOLD);
    }
    let text = render_row_text(row, state, width);
    if row.kind == SidebarRowKind::Repo
        && let Some(git) = &row.git
    {
        return render_repo_line_with_git(row, &text, git, style, width);
    }
    Line::from(Span::styled(text, style))
}

fn render_row_text(row: &SidebarRow, state: &SidebarState, width: usize) -> String {
    let selected = if state.selection.as_deref() == Some(row.id.as_str()) {
        "> "
    } else {
        "  "
    };
    let indent = "  ".repeat(row.depth);
    let line = match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            let marker = if row.expanded { "v" } else { ">" };
            format!(
                "{selected}{indent}{marker} {} [{}:{}]",
                row.label,
                rollup_label(row.rollup),
                row.chat_count
            )
        }
        SidebarRowKind::Chat => {
            let marker = if row.expanded { "v" } else { ">" };
            format!(
                "{selected}{indent}{marker} {} [{}]",
                row.label,
                rollup_label(row.rollup)
            )
        }
        SidebarRowKind::Detail => {
            format!("{selected}{indent}{}", row.label)
        }
        SidebarRowKind::Jump => {
            format!("{selected}{indent}-> {}", row.label)
        }
    };
    truncate_width(&line, width)
}

fn render_repo_line_with_git(
    row: &SidebarRow,
    text: &str,
    badge: &crate::git::GitBadge,
    style: Style,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![Span::styled(text.to_string(), style)];
    let used = text.chars().count();
    if used >= width {
        return Line::from(spans);
    }
    let git = format_git_badge_parts(badge);
    if git.branch.is_empty() {
        return Line::from(spans);
    }
    spans.push(Span::styled(format!(" {}", git.branch), style));
    if let Some(ahead) = git.ahead {
        spans.push(Span::styled(format!(" {ahead}"), style.fg(Color::Green)));
    }
    if let Some(behind) = git.behind {
        spans.push(Span::styled(format!(" {behind}"), style.fg(Color::Red)));
    }
    let _ = row;
    Line::from(spans)
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
            Line::from(Span::styled(rollup_glyph(row.rollup).to_string(), style))
        })
        .collect()
}

fn rollup_label(level: RollupLevel) -> &'static str {
    match level {
        RollupLevel::Error => "error",
        RollupLevel::Running => "running",
        RollupLevel::Permission => "permission",
        RollupLevel::Background => "background",
        RollupLevel::Waiting => "waiting",
        RollupLevel::Idle => "idle",
    }
}

fn rollup_glyph(level: RollupLevel) -> char {
    match level {
        RollupLevel::Error => 'E',
        RollupLevel::Running => 'R',
        RollupLevel::Permission => 'P',
        RollupLevel::Background => 'B',
        RollupLevel::Waiting => 'W',
        RollupLevel::Idle => 'I',
    }
}

fn truncate_width(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_string();
    }
    line.chars().take(width).collect()
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
        StatusFilter::AttentionOnly => "attn",
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
            expanded: true,
            pane_id: None,
            git: None,
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

        assert!(rendered.contains("v app [running:1]"));
        assert!(rendered.contains(">   v codex %1 [running]"));
    }

    #[test]
    fn render_rows_uses_rail_for_narrow_width() {
        let rows = vec![row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex %1",
            RollupLevel::Permission,
        )];
        let rendered = render_rows(&rows, &SidebarState::default(), 2);
        assert_eq!(rendered, "P");
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

        assert!(rendered.contains("v app [idle:1] main"));
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
        assert_eq!(lines[1].spans[0].style.bg, Some(Color::Indexed(237)));
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

        assert_eq!(header.lines[0].text, "category attn ");
        assert_eq!(
            header_hit_test(&header, 0, 1),
            Some(HeaderAction::CycleViewMode)
        );
        assert_eq!(
            header_hit_test(&header, 0, 9),
            Some(HeaderAction::ToggleFilter)
        );
    }

    #[test]
    fn header_layout_defaults_to_statusline_category_like_segments() {
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            filter: StatusFilter::All,
            ..SidebarState::default()
        };

        let header = build_header_layout(&state, 80);
        let lines = render_header_lines(&header, &SidebarRenderTheme::default());

        assert_eq!(header.lines[0].text, "repo     all  ");
        assert_eq!(header.lines[0].segments[0].range, 0..9);
        assert_eq!(header.lines[0].segments[1].range, 9..14);
        assert_eq!(
            header_hit_test(&header, 0, 8),
            Some(HeaderAction::CycleViewMode)
        );
        assert_eq!(
            header_hit_test(&header, 0, 13),
            Some(HeaderAction::ToggleFilter)
        );
        assert_eq!(lines[0].spans[0].content.as_ref(), "repo     ");
        assert_eq!(lines[0].spans[0].style.bg, None);
        assert!(
            !lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[1].content.as_ref(), "all  ");
        assert_eq!(lines[0].spans[1].style.bg, None);
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

        assert_eq!(header.lines[0].text, "[ repo     ] [ all  ]");
        assert_eq!(header.lines[0].segments[0].range, 0..12);
        assert_eq!(header.lines[0].segments[1].range, 13..21);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::White));
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Indexed(24)));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }
}

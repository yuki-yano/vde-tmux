use crate::agent::{display_agent_label_prefix, display_agent_name};
use crate::daemon::session_badge::{BadgeState, glyph_for_state};
use crate::hook::RollupLevel;
use crate::sidebar::state::{SidebarState, StatusFilter, ViewMode};
use crate::sidebar::tree::{BadgeCounts, SidebarRow, SidebarRowKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRenderTheme {
    pub selection_bg: Color,
    pub selection_bar: Color,
    pub action_icon: Color,
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
    pub live: Color,
    pub task_done: Color,
    pub task_working: Color,
    pub task_pending: Color,
    pub task_label: Color,
    pub subagent_label: Color,
    pub subagent_id: Color,
    pub worktree: Color,
    pub worktree_activity: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpRowAction {
    Jump,
    Preview,
    MarkDone,
}

impl Default for SidebarRenderTheme {
    fn default() -> Self {
        Self {
            selection_bg: Color::Rgb(0x30, 0x30, 0x34),
            selection_bar: Color::Indexed(229),
            action_icon: Color::Indexed(73),
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
            live: Color::Magenta,
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
            action_icon: parse_color(config.action_icon.as_deref()).unwrap_or(default.action_icon),
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
            live: parse_color(config.live.as_deref()).unwrap_or(default.live),
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

    fn rollup_color(&self, level: RollupLevel) -> Color {
        match level {
            RollupLevel::Error | RollupLevel::Permission | RollupLevel::Waiting => {
                self.badge_color(BadgeState::Blocked)
            }
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
            BadgeState::Working => self.badge_working,
            BadgeState::Done => self.badge_done,
            BadgeState::Idle => self.badge_idle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderAction {
    CycleViewMode,
    SetFilter(StatusFilter),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthTier {
    Rail,
    Micro,
    Dense,
    Standard,
}

impl WidthTier {
    pub fn from_width(width: usize) -> Self {
        match width {
            0..=3 => Self::Rail,
            4..=23 => Self::Micro,
            24..=35 => Self::Dense,
            _ => Self::Standard,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLines {
    pub lines: Vec<Line<'static>>,
    pub row_indices: Vec<Option<usize>>,
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
    pub action: Option<HeaderAction>,
    pub style: Option<Style>,
}

pub fn build_header_layout(state: &SidebarState, width: u16) -> HeaderLayout {
    build_header_layout_with_theme(state, width, &SidebarRenderTheme::default())
}

pub fn build_header_layout_with_theme(
    state: &SidebarState,
    width: u16,
    theme: &SidebarRenderTheme,
) -> HeaderLayout {
    build_header_layout_with_counts(state, width, theme, BadgeCounts::default())
}

pub fn build_header_layout_with_counts(
    state: &SidebarState,
    width: u16,
    theme: &SidebarRenderTheme,
    counts: BadgeCounts,
) -> HeaderLayout {
    if width <= 2 {
        return HeaderLayout::default();
    }

    let section = build_header_section_line(width as usize, theme);
    let title = build_header_title_line(state, width as usize, theme, counts);
    let chips = build_header_chip_line(state, width as usize, theme, counts);
    HeaderLayout {
        lines: vec![section, title, chips],
    }
}

const POWERLINE_ARROW: &str = "\u{e0b0}";

fn build_header_section_line(width: usize, theme: &SidebarRenderTheme) -> HeaderLine {
    let text = truncate_display(" SIDEBAR", width);
    let range = 0..display_width(&text) as u16;
    HeaderLine {
        text,
        segments: vec![HeaderSegment {
            range,
            action: None,
            style: Some(
                Style::default()
                    .fg(theme.category)
                    .add_modifier(Modifier::BOLD),
            ),
        }],
    }
}

fn build_header_title_line(
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
    counts: BadgeCounts,
) -> HeaderLine {
    let mode_body = format_header_mode_body(state.view_mode, theme);
    let mode_prefix = theme.header_prefix.as_str();
    let mode_text = format!("{mode_prefix}{mode_body}");
    let mode_suffix = theme.header_suffix.as_str();
    let count_text = format!(" {}", counts.total);
    let task_label = if counts.total == 1 { "task" } else { "tasks" };
    let total_label_text = format!(" {task_label} ");
    let total_bg = theme.header_total_bg.unwrap_or(theme.active_bg);
    let total_flat = total_bg == Color::Reset;
    let total_suffix = if total_flat {
        ""
    } else {
        header_total_suffix(theme)
    };
    let full_text = format!("{mode_text}{mode_suffix}{count_text}{total_label_text}{total_suffix}");
    let include_total = display_width(&full_text) <= width;
    let text = if include_total {
        full_text
    } else {
        truncate_display(&mode_text, width)
    };

    let mut pieces = Vec::new();
    if !mode_prefix.is_empty() {
        pieces.push((
            mode_prefix.to_string(),
            Style::default().fg(mode_bg(theme)),
            Some(HeaderAction::CycleViewMode),
        ));
    }
    pieces.push((
        mode_body,
        mode_segment_style(theme),
        Some(HeaderAction::CycleViewMode),
    ));
    if include_total {
        if !mode_suffix.is_empty() {
            let mut suffix_style = Style::default().fg(mode_bg(theme));
            if !total_flat {
                suffix_style = suffix_style.bg(total_bg);
            }
            pieces.push((mode_suffix.to_string(), suffix_style, None));
        }
        let mut count_style = Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD);
        if !total_flat {
            count_style = count_style.bg(total_bg);
        }
        pieces.push((count_text, count_style, None));
        let mut label_style = Style::default().fg(theme.header_total_fg.unwrap_or(theme.detail));
        if !total_flat {
            label_style = label_style.bg(total_bg);
        }
        pieces.push((total_label_text, label_style, None));
        if !total_suffix.is_empty() {
            pieces.push((
                total_suffix.to_string(),
                Style::default().fg(total_bg),
                None,
            ));
        }
    }

    let mut segments = Vec::new();
    let mut start = 0;
    for (piece, style, action) in pieces {
        let len = display_width(&piece);
        if let Some(range) = visible_segment_range(&text, start, len) {
            segments.push(HeaderSegment {
                range,
                action,
                style: Some(style),
            });
        }
        start += len;
    }

    HeaderLine { text, segments }
}

fn format_header_mode_body(view_mode: ViewMode, theme: &SidebarRenderTheme) -> String {
    let mode = view_mode_label(view_mode);
    let mode_padded = view_mode_label_padded(view_mode);
    let label = format!("≣ {mode_padded}");
    theme
        .header_format
        .replace("{label}", &label)
        .replace("{mode_padded}", &mode_padded)
        .replace("{mode}", mode)
}

fn header_total_suffix(theme: &SidebarRenderTheme) -> &str {
    theme.header_suffix.as_str()
}

#[derive(Clone, Copy)]
struct HeaderChipSpec {
    filter: StatusFilter,
    count: usize,
    badge_state: Option<BadgeState>,
}

fn build_header_chip_line(
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
    counts: BadgeCounts,
) -> HeaderLine {
    let specs = [
        HeaderChipSpec {
            filter: StatusFilter::All,
            count: counts.total,
            badge_state: None,
        },
        HeaderChipSpec {
            filter: StatusFilter::AttentionOnly,
            count: counts.attention,
            badge_state: Some(BadgeState::Blocked),
        },
        HeaderChipSpec {
            filter: StatusFilter::WorkingOnly,
            count: counts.working,
            badge_state: Some(BadgeState::Working),
        },
        HeaderChipSpec {
            filter: StatusFilter::DoneOnly,
            count: counts.done,
            badge_state: Some(BadgeState::Done),
        },
        HeaderChipSpec {
            filter: StatusFilter::IdleOnly,
            count: counts.idle,
            badge_state: Some(BadgeState::Idle),
        },
    ];

    let caps_enabled = !theme.header_chip_prefix.is_empty() || !theme.header_chip_suffix.is_empty();
    let mut pieces: Vec<(String, Option<Style>, Option<HeaderAction>)> = Vec::new();
    for (index, spec) in specs.into_iter().enumerate() {
        let active = state.filter == spec.filter;
        let action = if active || counts.filter_is_available(spec.filter) {
            Some(HeaderAction::SetFilter(spec.filter))
        } else {
            None
        };
        let style = chip_style(theme, active, spec.badge_state, spec.count);
        if caps_enabled && index > 0 {
            pieces.push((" ".to_string(), None, None));
        }
        let bg = chip_bg(theme, active, spec.count);
        match bg {
            Some(bg) if caps_enabled => {
                let cap = Style::default().fg(bg);
                if !theme.header_chip_prefix.is_empty() {
                    pieces.push((theme.header_chip_prefix.clone(), Some(cap), action));
                }
                push_chip_label_pieces(&mut pieces, theme, spec, active, style, action);
                if !theme.header_chip_suffix.is_empty() {
                    pieces.push((theme.header_chip_suffix.clone(), Some(cap), action));
                }
            }
            _ => push_chip_label_pieces(&mut pieces, theme, spec, active, style, action),
        }
    }

    let full_text: String = pieces.iter().map(|(text, _, _)| text.as_str()).collect();
    let text = truncate_display(&full_text, width);
    let mut segments = Vec::new();
    let mut start = 0;
    for (piece, style, action) in pieces {
        let len = display_width(&piece);
        if let Some(style) = style
            && let Some(range) = visible_segment_range(&text, start, len)
        {
            segments.push(HeaderSegment {
                range,
                action,
                style: Some(style),
            });
        }
        start += len;
    }

    HeaderLine { text, segments }
}

fn chip_bg(theme: &SidebarRenderTheme, active: bool, count: usize) -> Option<Color> {
    if active {
        Some(filter_bg(theme))
    } else if count > 0 {
        Some(theme.active_bg)
    } else {
        None
    }
}

fn push_chip_label_pieces(
    pieces: &mut Vec<(String, Option<Style>, Option<HeaderAction>)>,
    theme: &SidebarRenderTheme,
    spec: HeaderChipSpec,
    active: bool,
    text_style: Style,
    action: Option<HeaderAction>,
) {
    let Some(state) = spec.badge_state else {
        pieces.push((format!(" ≡ {} ", spec.count), Some(text_style), action));
        return;
    };

    pieces.push((" ".to_string(), Some(text_style), action));
    pieces.push((
        theme.badge_glyph(state).to_string(),
        Some(chip_badge_style(theme, active, state, spec.count)),
        action,
    ));
    pieces.push((format!(" {} ", spec.count), Some(text_style), action));
}

fn chip_badge_style(
    theme: &SidebarRenderTheme,
    active: bool,
    state: BadgeState,
    count: usize,
) -> Style {
    let mut style = Style::default().fg(theme.badge_color(state));
    if active {
        style = style.bg(filter_bg(theme));
        if header_bold(theme) {
            style = style.add_modifier(Modifier::BOLD);
        }
    } else if count == 0 {
        style = style.add_modifier(Modifier::DIM);
    } else {
        style = style.bg(theme.active_bg);
    }
    style
}

fn chip_style(
    theme: &SidebarRenderTheme,
    active: bool,
    badge_state: Option<BadgeState>,
    count: usize,
) -> Style {
    if active {
        let mut style = Style::default()
            .fg(theme.header_chip_fg.unwrap_or_else(|| mode_fg(theme)))
            .bg(filter_bg(theme));
        if header_bold(theme) {
            style = style.add_modifier(Modifier::BOLD);
        }
        return style;
    }
    if count == 0 {
        return Style::default()
            .fg(theme.detail)
            .add_modifier(Modifier::DIM);
    }
    let fg = match badge_state {
        Some(state) => theme.badge_color(state),
        None => theme.header_chip_fg.unwrap_or_else(|| mode_fg(theme)),
    };
    Style::default().fg(fg).bg(theme.active_bg)
}

pub fn header_hit_test(layout: &HeaderLayout, row: u16, column: u16) -> Option<HeaderAction> {
    layout
        .lines
        .get(row as usize)?
        .segments
        .iter()
        .find(|segment| segment.range.contains(&column))
        .and_then(|segment| segment.action)
}

pub fn render_header_lines(
    layout: &HeaderLayout,
    _theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    layout
        .lines
        .iter()
        .map(|line| {
            let mut spans = Vec::new();
            let mut cursor = 0_u16;
            for segment in &line.segments {
                if cursor < segment.range.start {
                    spans.push(Span::raw(slice_display(
                        &line.text,
                        cursor,
                        segment.range.start,
                    )));
                }
                spans.push(Span::styled(
                    slice_display(&line.text, segment.range.start, segment.range.end),
                    segment.style.expect("header segment style"),
                ));
                cursor = segment.range.end;
            }
            let text_len = display_width(&line.text) as u16;
            if cursor < text_len {
                spans.push(Span::raw(slice_display(&line.text, cursor, text_len)));
            }
            Line::from(spans)
        })
        .collect()
}

pub fn build_footer_line(width: usize) -> Line<'static> {
    let help = if width >= 64 {
        " j/k move  enter jump  p preview  d complete  tab/S-tab filter"
    } else if width >= 36 {
        " j/k move  enter jump  d complete"
    } else if width >= 24 {
        " j/k move  d complete"
    } else {
        " j/k  d complete"
    };
    let text = truncate_display(help, width);
    Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    ))
}

fn mode_segment_style(theme: &SidebarRenderTheme) -> Style {
    let mut style = Style::default().fg(mode_fg(theme));
    let background = mode_bg(theme);
    if background != Color::Reset {
        style = style.bg(background);
    }
    if header_bold(theme) {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn header_style_configured(theme: &SidebarRenderTheme) -> bool {
    theme.header_active_fg.is_some() || theme.header_active_bg.is_some() || theme.header_active_bold
}

fn header_bold(theme: &SidebarRenderTheme) -> bool {
    !header_style_configured(theme) || theme.header_active_bold
}

fn mode_fg(theme: &SidebarRenderTheme) -> Color {
    theme.header_active_fg.unwrap_or(theme.header_badge_fg)
}

fn mode_bg(theme: &SidebarRenderTheme) -> Color {
    theme.header_active_bg.unwrap_or(theme.header_mode)
}

fn filter_bg(theme: &SidebarRenderTheme) -> Color {
    theme.header_filter_bg.unwrap_or_else(|| mode_bg(theme))
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
    render_lines_with_indices(rows, state, width, theme).lines
}

pub fn render_lines_with_indices(
    rows: &[SidebarRow],
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> RenderedLines {
    match WidthTier::from_width(width) {
        WidthTier::Rail => render_rail_lines(rows, state, width, theme),
        WidthTier::Micro => render_micro_lines(rows, state, width, theme),
        WidthTier::Dense => render_dense_lines(rows, state, width, theme),
        WidthTier::Standard => render_standard_lines(rows, state, width, theme),
    }
}

fn render_standard_lines(
    rows: &[SidebarRow],
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> RenderedLines {
    let mut lines = Vec::new();
    let mut row_indices = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if row.kind == SidebarRowKind::Chat && !row.expanded {
            lines.extend(render_closed_chat_digest_lines(row, state, width, theme));
            row_indices.push(Some(index));
            row_indices.push(Some(index));
        } else {
            lines.push(render_row_line(row, state, width, theme));
            row_indices.push(Some(index));
        }
    }
    RenderedLines { lines, row_indices }
}

fn render_closed_chat_digest_lines(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    vec![
        render_closed_chat_summary_line(row, state, width, theme),
        render_closed_chat_prompt_line(row, state, width, theme),
    ]
}

fn render_closed_chat_summary_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = row_is_selected(row, state);
    let indent = "  ".repeat(row.depth);
    let badge_state = row.badge_state.unwrap_or(BadgeState::Idle);
    let glyph = theme.badge_glyph(badge_state);
    let agent_source = chat_agent_label(row);

    let mut prefix = Vec::new();
    push_leading_marker_span(&mut prefix, row, selected, theme, &indent);
    prefix.push(Span::styled(
        " ".to_string(),
        Style::default().fg(theme.marker),
    ));
    prefix.push(Span::styled("▸ ".to_string(), toggle_marker_style(theme)));
    prefix.push(Span::styled(
        format!("{glyph} "),
        badge_style(theme.badge_color(badge_state), row),
    ));
    let prefix_width: usize = prefix.iter().map(|span| display_width(&span.content)).sum();
    let available_after_prefix = width.saturating_sub(1).saturating_sub(prefix_width);
    let min_agent_width = display_width(&agent_source)
        .min(7)
        .min(available_after_prefix);
    let right_budget = width
        .saturating_sub(1)
        .saturating_sub(prefix_width)
        .saturating_sub(min_agent_width)
        .saturating_sub(1);
    let right_parts = closed_chat_right_parts_for_width(row, right_budget);
    let right_width = closed_chat_right_parts_width(&right_parts);
    let right_reserved = if right_width > 0 { right_width + 1 } else { 0 };
    let agent_budget = width
        .saturating_sub(1)
        .saturating_sub(prefix_width)
        .saturating_sub(right_reserved);
    let agent = truncate_display(&agent_source, agent_budget);

    let mut spans = prefix;
    spans.push(Span::styled(
        agent,
        row_style(row, theme).add_modifier(Modifier::BOLD),
    ));
    let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    let filler = width
        .saturating_sub(1)
        .saturating_sub(used)
        .saturating_sub(right_width);
    spans.push(Span::raw(" ".repeat(filler)));
    if !right_parts.is_empty() {
        spans.extend(closed_chat_right_spans(&right_parts, row, theme));
    }
    spans.push(Span::raw(" ".to_string()));
    style_chat_digest_line(Line::from(spans), selected, theme)
}

fn render_closed_chat_prompt_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = row_is_selected(row, state);
    let indent = format!("{}    ", "  ".repeat(row.depth));
    let mut spans = Vec::new();
    push_leading_marker_span(&mut spans, row, false, theme, &indent);
    let prefix_width: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    let available = width.saturating_sub(1).saturating_sub(prefix_width);
    let reason = closed_chat_reason_token(row);
    let reason_width = reason.as_deref().map(display_width).unwrap_or(0);
    let (prompt_budget, reason) = match reason {
        Some(reason) if available > reason_width + 1 => {
            (available - reason_width - 1, Some(reason))
        }
        _ => (available, None),
    };
    let prompt = truncate_display(&chat_prompt_label(row), prompt_budget);
    spans.push(Span::styled(prompt, row_style(row, theme)));
    let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    let reason_width = reason.as_deref().map(display_width).unwrap_or(0);
    let filler = width
        .saturating_sub(1)
        .saturating_sub(used)
        .saturating_sub(reason_width);
    spans.push(Span::raw(" ".repeat(filler)));
    if let Some(reason) = reason {
        spans.push(Span::styled(
            reason,
            Style::default().fg(theme.rollup_color(row.rollup)),
        ));
    }
    spans.push(Span::raw(" ".to_string()));
    style_chat_digest_line(Line::from(spans), selected, theme)
}

fn style_chat_digest_line(
    mut line: Line<'static>,
    selected: bool,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    if selected {
        line = line.style(
            Style::default()
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        );
    }
    line
}

fn render_row_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = row_is_selected(row, state);
    let selected_marker =
        selected && !matches!(row.kind, SidebarRowKind::Detail | SidebarRowKind::Jump);
    if row.kind == SidebarRowKind::Zone {
        let text = truncate_display(
            &format!(" ▍{} {}", row.label, row.chat_count),
            width.saturating_sub(1),
        );
        let style = Style::default()
            .fg(theme.badge_color(BadgeState::Blocked))
            .add_modifier(Modifier::BOLD);
        return Line::from(Span::styled(text, style));
    }
    let style = row_style(row, theme);
    let content_width = width.saturating_sub(2);

    let indent = "  ".repeat(row.depth);
    let head = match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            let marker = if row.expanded { "▾" } else { "▸" };
            format!("{indent}{marker} ")
        }
        SidebarRowKind::Chat => {
            let marker = if row.expanded { "▾" } else { "▸" };
            format!("{indent} {marker} ")
        }
        SidebarRowKind::Detail if row.id.starts_with("meta::") => format!("{indent}  "),
        SidebarRowKind::Detail => format!("{indent}│ "),
        SidebarRowKind::Jump => format!("{indent}└ "),
        SidebarRowKind::Zone => unreachable!("zone rows return before generic rendering"),
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
    let label_source = match row.kind {
        SidebarRowKind::Category => row.label.clone(),
        SidebarRowKind::Jump => JUMP_ROW_LABEL.to_string(),
        SidebarRowKind::Chat => chat_display_label(row),
        _ => row.label.clone(),
    };
    let label = truncate_display(&label_source, label_budget);

    let mut spans = Vec::new();
    if row.kind == SidebarRowKind::Chat {
        let marker = if row.expanded { "▾" } else { "▸" };
        push_leading_marker_span(&mut spans, row, selected_marker, theme, &indent);
        spans.push(Span::styled(
            " ".to_string(),
            Style::default().fg(theme.marker),
        ));
        spans.push(Span::styled(
            format!("{marker} "),
            toggle_marker_style(theme),
        ));
    } else if matches!(row.kind, SidebarRowKind::Category | SidebarRowKind::Repo) {
        let marker = if row.expanded { "▾" } else { "▸" };
        push_leading_marker_span(&mut spans, row, selected_marker, theme, &indent);
        spans.push(Span::styled(
            format!("{marker} "),
            toggle_marker_style(theme),
        ));
    } else if row.kind == SidebarRowKind::Detail && row.id.starts_with("meta::") {
        push_leading_marker_span(&mut spans, row, selected_marker, theme, &indent);
        spans.push(Span::styled(
            "  ".to_string(),
            Style::default().fg(theme.marker),
        ));
    } else {
        push_leading_marker_span(&mut spans, row, selected_marker, theme, &head);
    }
    if let Some((glyph, color)) = badge {
        spans.push(Span::styled(glyph, badge_style(color, row)));
    }
    if row.kind == SidebarRowKind::Jump {
        spans.extend(jump_action_spans(&label, theme));
    } else {
        spans.extend(label_spans(label, row, style, theme));
    }
    if let Some(git) = &git {
        spans.push(Span::styled(
            format!(" {}", git.branch),
            Style::default().fg(theme.branch),
        ));
        if let Some(ahead) = &git.ahead {
            spans.push(Span::styled(
                format!(" {ahead}"),
                Style::default().fg(Color::Green),
            ));
        }
        if let Some(behind) = &git.behind {
            spans.push(Span::styled(
                format!(" {behind}"),
                Style::default().fg(Color::Red),
            ));
        }
    }
    let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    let filler = width
        .saturating_sub(1)
        .saturating_sub(used)
        .saturating_sub(right_width);
    if row.kind == SidebarRowKind::Category && filler > 2 {
        spans.push(Span::styled(
            format!(" {} ", "─".repeat(filler.saturating_sub(2))),
            Style::default().fg(theme.marker),
        ));
    } else {
        spans.push(Span::raw(" ".repeat(filler)));
    }
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

fn row_is_selected(row: &SidebarRow, state: &SidebarState) -> bool {
    let Some(selection) = state.selection.as_deref() else {
        return false;
    };
    if selection == row.id {
        return true;
    }
    if !matches!(
        row.kind,
        SidebarRowKind::Chat | SidebarRowKind::Detail | SidebarRowKind::Jump
    ) {
        return false;
    }
    let Some(selected_pane) = crate::sidebar::tree::pane_instance_from_row_id(selection) else {
        return false;
    };
    crate::sidebar::tree::pane_instance_from_row_id(&row.id).as_ref() == Some(&selected_pane)
}

fn push_leading_marker_span(
    spans: &mut Vec<Span<'static>>,
    row: &SidebarRow,
    selected: bool,
    theme: &SidebarRenderTheme,
    tail: &str,
) {
    spans.push(row_leading_marker_span(row, selected, theme));
    if !tail.is_empty() {
        spans.push(Span::styled(
            tail.to_string(),
            Style::default().fg(theme.marker),
        ));
    }
}

fn row_leading_marker_span(
    row: &SidebarRow,
    selected: bool,
    theme: &SidebarRenderTheme,
) -> Span<'static> {
    let (marker, style) = match (row.active, selected) {
        (_, true) => (
            "▎",
            Style::default()
                .fg(theme.selection_bar)
                .add_modifier(Modifier::BOLD),
        ),
        (true, false) => ("▎", Style::default().fg(theme.active_bar)),
        (false, false) => (" ", Style::default().fg(theme.marker)),
    };
    Span::styled(marker.to_string(), style)
}

fn label_spans(
    label: String,
    row: &SidebarRow,
    base: Style,
    theme: &SidebarRenderTheme,
) -> Vec<Span<'static>> {
    if row.kind == SidebarRowKind::Detail
        && let Some(spans) = detail_label_spans(&label, row, theme)
    {
        return spans;
    }
    if row.kind == SidebarRowKind::Chat
        && let Some(agent) = row
            .meta
            .as_ref()
            .and_then(|meta| meta.agent.as_deref())
            .filter(|agent| !agent.is_empty())
            .map(display_agent_name)
        && label.starts_with(&agent)
    {
        let (agent_part, rest) = label.split_at(agent.len());
        if row.expanded
            && let Some(state_context) = rest.strip_prefix(": ")
        {
            let mut spans = vec![
                Span::styled(agent_part.to_string(), base.add_modifier(Modifier::BOLD)),
                Span::styled(": ".to_string(), base),
            ];
            spans.extend(state_context_spans(state_context, row, theme));
            return spans;
        }
        return vec![
            Span::styled(agent_part.to_string(), base.add_modifier(Modifier::BOLD)),
            Span::styled(rest.to_string(), base),
        ];
    }
    vec![Span::styled(label, base)]
}

fn detail_label_spans(
    label: &str,
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
) -> Option<Vec<Span<'static>>> {
    if row.id.contains("::task::") {
        return task_detail_label_spans(label, row, theme);
    }
    if row.id.contains("::subagent::") {
        return subagent_detail_label_spans(label, theme);
    }
    if row.id.ends_with("::worktree-activity") {
        return Some(vec![Span::styled(
            label.to_string(),
            Style::default().fg(theme.worktree_activity),
        )]);
    }
    if row.id.ends_with("::worktree") {
        return Some(vec![Span::styled(
            label.to_string(),
            Style::default().fg(theme.worktree),
        )]);
    }
    None
}

fn task_detail_label_spans(
    label: &str,
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
) -> Option<Vec<Span<'static>>> {
    let (connector, rest) = split_tree_connector(label)?;
    let mut chars = rest.chars();
    let icon = chars.next()?;
    let body = chars.collect::<String>();
    Some(vec![
        Span::styled(connector, Style::default().fg(theme.marker)),
        Span::styled(
            icon.to_string(),
            Style::default().fg(task_detail_icon_color(row, theme)),
        ),
        Span::styled(body, Style::default().fg(theme.task_label)),
    ])
}

fn subagent_detail_label_spans(
    label: &str,
    theme: &SidebarRenderTheme,
) -> Option<Vec<Span<'static>>> {
    let (connector, rest) = split_tree_connector(label)?;
    let mut spans = vec![Span::styled(connector, Style::default().fg(theme.marker))];
    if let Some((agent_label, id)) = rest.rsplit_once(" #") {
        spans.push(Span::styled(
            agent_label.to_string(),
            Style::default().fg(theme.subagent_label),
        ));
        spans.push(Span::styled(
            format!(" #{id}"),
            Style::default().fg(theme.subagent_id),
        ));
    } else {
        spans.push(Span::styled(
            rest.to_string(),
            Style::default().fg(theme.subagent_label),
        ));
    }
    Some(spans)
}

fn split_tree_connector(label: &str) -> Option<(String, &str)> {
    let mut iter = label.char_indices();
    let (_, connector) = iter.next()?;
    if connector != '\u{251c}' && connector != '\u{2514}' {
        return None;
    }
    let (space_index, space) = iter.next()?;
    if space != ' ' {
        return None;
    }
    let rest_index = space_index + space.len_utf8();
    Some((label[..rest_index].to_string(), &label[rest_index..]))
}

fn task_detail_icon_color(row: &SidebarRow, theme: &SidebarRenderTheme) -> Color {
    if row.id.ends_with("::completed") {
        theme.task_done
    } else if row.id.ends_with("::in_progress") {
        theme.task_working
    } else {
        theme.task_pending
    }
}

fn state_context_spans(
    state_context: &str,
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
) -> Vec<Span<'static>> {
    if state_context.is_empty() {
        return Vec::new();
    }
    let state_len = state_context
        .find(|ch: char| ch.is_whitespace() || ch == '(')
        .unwrap_or(state_context.len());
    let (state, context) = state_context.split_at(state_len);
    let mut spans = vec![Span::styled(
        state.to_string(),
        Style::default().fg(theme.rollup_color(row.rollup)),
    )];
    if !context.is_empty() {
        spans.push(Span::styled(
            context.to_string(),
            Style::default().fg(theme.detail),
        ));
    }
    spans
}

fn toggle_marker_style(theme: &SidebarRenderTheme) -> Style {
    Style::default()
        .fg(theme.toggle)
        .add_modifier(Modifier::BOLD)
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

fn render_dense_lines(
    rows: &[SidebarRow],
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> RenderedLines {
    let mut lines = Vec::new();
    let mut row_indices = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let line = match row.kind {
            SidebarRowKind::Detail | SidebarRowKind::Jump => None,
            SidebarRowKind::Zone => Some(render_zone_dense_line(row, width, theme)),
            SidebarRowKind::Category | SidebarRowKind::Repo => {
                Some(render_group_dense_line(row, state, width, theme))
            }
            SidebarRowKind::Chat => Some(render_chat_dense_line(row, state, width, theme)),
        };
        if let Some(line) = line {
            lines.push(line);
            row_indices.push(Some(index));
        }
    }
    RenderedLines { lines, row_indices }
}

fn render_zone_dense_line(
    row: &SidebarRow,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let text = truncate_display(&format!(" ▍{} {}", row.label, row.chat_count), width);
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(theme.badge_color(BadgeState::Blocked))
            .add_modifier(Modifier::BOLD),
    ))
}

fn render_group_dense_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = state.selection.as_deref() == Some(row.id.as_str());
    let marker = if row.expanded { "▾" } else { "▸" };
    let text = truncate_display(&format!(" {marker} {}", row.label), width);
    let mut style = row_style(row, theme);
    if selected {
        style = style.bg(theme.selection_bg).add_modifier(Modifier::BOLD);
    }
    leading_marker_line(row, selected, theme, pad_to_width(text, width), style)
}

fn render_chat_dense_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = state.selection.as_deref() == Some(row.id.as_str());
    let badge_state = row.badge_state.unwrap_or(BadgeState::Idle);
    let glyph = theme.badge_glyph(badge_state);
    let agent = row
        .meta
        .as_ref()
        .and_then(|meta| meta.agent.as_deref())
        .unwrap_or_else(|| row.label.split(':').next().unwrap_or(row.label.as_str()));
    let agent = truncate_display(&display_agent_name(agent), 7);
    let origin = row
        .meta
        .as_ref()
        .and_then(|meta| meta.origin.as_deref())
        .and_then(|origin| origin.rsplit('/').next())
        .unwrap_or("");
    let origin = origin.chars().take(3).collect::<String>();
    let right = right_label(row).unwrap_or_default();
    let body = row
        .label
        .split_once(':')
        .map(|(_, body)| body.trim())
        .unwrap_or(row.label.as_str());
    let prefix_after_glyph = format!(" {agent:<7} {origin:<3} ");
    let right_width = display_width(&right);
    let right_reserved = if right_width > 0 { right_width + 1 } else { 0 };
    let label_budget = width
        .saturating_sub(1)
        .saturating_sub(display_width(glyph))
        .saturating_sub(display_width(&prefix_after_glyph))
        .saturating_sub(right_reserved)
        .saturating_sub(1);
    let label = truncate_display(body, label_budget);
    let used =
        1 + display_width(glyph) + display_width(&prefix_after_glyph) + display_width(&label);
    let filler = width
        .saturating_sub(1)
        .saturating_sub(used)
        .saturating_sub(right_width);
    let mut style = row_style(row, theme);
    if row_flash(row) {
        style = style.add_modifier(Modifier::REVERSED);
    }
    let mut right_status_style = right_style(row, theme);
    if row_flash(row) {
        right_status_style = right_status_style.add_modifier(Modifier::REVERSED);
    }
    let mut line = Line::from(vec![
        row_leading_marker_span(row, selected, theme),
        Span::styled(
            glyph.to_string(),
            badge_style(theme.badge_color(badge_state), row),
        ),
        Span::styled(prefix_after_glyph, style),
        Span::styled(label, style),
        Span::raw(" ".repeat(filler)),
        Span::styled(right, right_status_style),
        Span::raw(" ".to_string()),
    ]);
    if selected {
        line = line.style(
            Style::default()
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        );
    }
    line
}

fn leading_marker_line(
    row: &SidebarRow,
    selected: bool,
    theme: &SidebarRenderTheme,
    text: String,
    style: Style,
) -> Line<'static> {
    if !row.active && !selected {
        return Line::from(Span::styled(text, style));
    }
    let rest = text.chars().skip(1).collect::<String>();
    Line::from(vec![
        row_leading_marker_span(row, selected, theme),
        Span::styled(rest, style),
    ])
}

fn render_micro_lines(
    rows: &[SidebarRow],
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> RenderedLines {
    let mut lines = Vec::new();
    let mut row_indices = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if row.kind != SidebarRowKind::Chat {
            continue;
        }
        let badge_state = row.badge_state.unwrap_or(BadgeState::Idle);
        let glyph = theme.badge_glyph(badge_state);
        let right = right_label(row).unwrap_or_default();
        let selected = state.selection.as_deref() == Some(row.id.as_str());
        let marker = row_leading_marker_span(row, selected, theme);
        let text = if right.is_empty() {
            glyph.to_string()
        } else {
            format!("{glyph} {right}")
        };
        let body = pad_to_width(
            truncate_display(&text, width.saturating_sub(1)),
            width.saturating_sub(1),
        );
        let mut line = Line::from(vec![
            marker,
            Span::styled(body, badge_style(theme.badge_color(badge_state), row)),
        ]);
        if selected {
            line = line.style(
                Style::default()
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
            );
        }
        lines.push(line);
        row_indices.push(Some(index));
    }
    RenderedLines { lines, row_indices }
}

fn render_rail_lines(
    rows: &[SidebarRow],
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> RenderedLines {
    let chat_rows = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| matches!(row.kind, SidebarRowKind::Chat))
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut row_indices = Vec::new();
    for state in [
        BadgeState::Blocked,
        BadgeState::Working,
        BadgeState::Done,
        BadgeState::Idle,
    ] {
        let count = chat_rows
            .iter()
            .filter(|(_, row)| row.badge_state == Some(state))
            .count();
        if count > 0 {
            let text = format!("{}{}", theme.badge_glyph(state), compact_rail_count(count));
            lines.push(Line::from(Span::styled(
                truncate_display(&text, width),
                Style::default().fg(theme.badge_color(state)),
            )));
            row_indices.push(None);
        }
    }
    if !lines.is_empty() && !chat_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "──",
            Style::default().fg(Color::DarkGray),
        )));
        row_indices.push(None);
    }
    for (index, row) in chat_rows {
        let mut style = Style::default().fg(theme.rollup_color(row.rollup));
        if row_flash(row) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let selected = state.selection.as_deref() == Some(row.id.as_str());
        if selected {
            style = style.bg(theme.selection_bg).add_modifier(Modifier::BOLD);
        }
        let glyph = row.badge_state.expect("rail rows must carry badge_state");
        lines.push(Line::from(vec![
            row_leading_marker_span(row, selected, theme),
            Span::styled(theme.badge_glyph(glyph).to_string(), style),
        ]));
        row_indices.push(Some(index));
    }
    RenderedLines { lines, row_indices }
}

fn compact_rail_count(count: usize) -> String {
    if count < 10 {
        count.to_string()
    } else {
        "9+".to_string()
    }
}

fn badge_style(color: Color, row: &SidebarRow) -> Style {
    let mut style = Style::default().fg(color);
    if row_flash(row) {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn row_flash(row: &SidebarRow) -> bool {
    row.meta
        .as_ref()
        .and_then(|meta| meta.flash)
        .unwrap_or(false)
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

fn pad_to_width(mut text: String, width: usize) -> String {
    let used = display_width(&text);
    if used < width {
        text.push_str(&" ".repeat(width - used));
    }
    text
}

fn right_label(row: &SidebarRow) -> Option<String> {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            let count = row.meta.as_ref()?.attention_count?;
            (count > 0).then(|| format!("▲{count}"))
        }
        SidebarRowKind::Chat => {
            if row.expanded {
                return expanded_chat_right_label(row);
            }
            match row.rollup {
                RollupLevel::Error => Some("Err".to_string()),
                RollupLevel::Permission => Some("Perm".to_string()),
                RollupLevel::Waiting => Some("Wait".to_string()),
                RollupLevel::Background => Some("Bg".to_string()),
                RollupLevel::Running => row
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.elapsed_secs)
                    .map(elapsed_label),
                RollupLevel::Idle => row
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.completed_age_secs)
                    .map(|secs| format!("{} ago", elapsed_label(secs))),
            }
        }
        SidebarRowKind::Detail | SidebarRowKind::Jump | SidebarRowKind::Zone => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosedChatRightTone {
    State,
    TaskDone,
    TaskWorking,
    Subagent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClosedChatRightPart {
    text: String,
    tone: ClosedChatRightTone,
}

fn closed_chat_right_parts_for_width(row: &SidebarRow, budget: usize) -> Vec<ClosedChatRightPart> {
    if budget == 0 {
        return Vec::new();
    }
    let parts = closed_chat_right_parts(row);
    let mut included = Vec::new();
    for part in parts {
        let candidate_width = closed_chat_right_parts_width_with_candidate(&included, &part);
        if candidate_width <= budget {
            included.push(part);
        } else if included.is_empty() {
            return vec![ClosedChatRightPart {
                text: truncate_display(&part.text, budget),
                tone: part.tone,
            }];
        }
    }
    included
}

fn closed_chat_right_parts_width(parts: &[ClosedChatRightPart]) -> usize {
    let text_width: usize = parts.iter().map(|part| display_width(&part.text)).sum();
    text_width + parts.len().saturating_sub(1) * display_width(" · ")
}

fn closed_chat_right_parts_width_with_candidate(
    included: &[ClosedChatRightPart],
    candidate: &ClosedChatRightPart,
) -> usize {
    let separator_width = if included.is_empty() {
        0
    } else {
        display_width(" · ")
    };
    closed_chat_right_parts_width(included) + separator_width + display_width(&candidate.text)
}

fn closed_chat_right_spans(
    parts: &[ClosedChatRightPart],
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " · ".to_string(),
                Style::default().fg(theme.marker),
            ));
        }
        spans.push(Span::styled(
            part.text.clone(),
            closed_chat_right_tone_style(part.tone, row, theme),
        ));
    }
    spans
}

fn closed_chat_right_tone_style(
    tone: ClosedChatRightTone,
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
) -> Style {
    match tone {
        ClosedChatRightTone::State => {
            Style::default().fg(theme.badge_color(row.badge_state.unwrap_or(BadgeState::Idle)))
        }
        ClosedChatRightTone::TaskDone => Style::default().fg(theme.task_done),
        ClosedChatRightTone::TaskWorking => Style::default().fg(theme.task_working),
        ClosedChatRightTone::Subagent => Style::default().fg(theme.subagent_label),
    }
}

fn closed_chat_right_parts(row: &SidebarRow) -> Vec<ClosedChatRightPart> {
    let mut parts = Vec::new();
    if let Some(state_or_time) = closed_chat_state_or_time_label(row) {
        parts.push(ClosedChatRightPart {
            text: state_or_time,
            tone: ClosedChatRightTone::State,
        });
    }
    if let Some(task) = task_progress_token(row) {
        parts.push(task);
    }
    if let Some(subagents) = subagent_count_token(row) {
        parts.push(subagents);
    }
    parts
}

fn closed_chat_state_or_time_label(row: &SidebarRow) -> Option<String> {
    expanded_chat_right_label(row)
}

fn task_progress_token(row: &SidebarRow) -> Option<ClosedChatRightPart> {
    let meta = row.meta.as_ref()?;
    let done = meta.tasks_done?;
    let total = meta.tasks_total?;
    (total > 0).then(|| ClosedChatRightPart {
        text: format!("☑ {done}/{total}"),
        tone: task_progress_tone(done, total),
    })
}

fn task_progress_tone(done: i64, total: i64) -> ClosedChatRightTone {
    if done >= total {
        ClosedChatRightTone::TaskDone
    } else {
        ClosedChatRightTone::TaskWorking
    }
}

fn subagent_count_token(row: &SidebarRow) -> Option<ClosedChatRightPart> {
    let count = row.meta.as_ref()?.subagent_count?;
    (count > 0).then(|| ClosedChatRightPart {
        text: format!("↳ {count}"),
        tone: ClosedChatRightTone::Subagent,
    })
}

fn closed_chat_reason_token(row: &SidebarRow) -> Option<String> {
    if !matches!(
        row.rollup,
        RollupLevel::Permission | RollupLevel::Waiting | RollupLevel::Error
    ) {
        return None;
    }
    row.meta
        .as_ref()
        .and_then(|meta| meta.wait_reason.as_deref())
        .filter(|reason| !reason.trim().is_empty())
        .map(|reason| format!("↩ {}", short_wait_reason(reason)))
}

fn short_wait_reason(reason: &str) -> String {
    let reason = reason.trim();
    match reason {
        "permission_prompt" | "permission" => "permission".to_string(),
        "waiting_input" | "input" | "user_input" => "input".to_string(),
        "rate_limit" | "rate_limited" => "rate-limit".to_string(),
        "network_error" => "network".to_string(),
        _ => truncate_display(&reason.replace('_', "-"), 16),
    }
}

fn chat_agent_label(row: &SidebarRow) -> String {
    row.meta
        .as_ref()
        .and_then(|meta| meta.agent.as_deref())
        .filter(|agent| !agent.trim().is_empty())
        .map(display_agent_name)
        .unwrap_or_else(|| {
            display_agent_name(row.label.split(':').next().unwrap_or(row.label.as_str()))
        })
}

fn chat_display_label(row: &SidebarRow) -> String {
    let Some(raw_agent) = row
        .meta
        .as_ref()
        .and_then(|meta| meta.agent.as_deref())
        .filter(|agent| !agent.trim().is_empty())
    else {
        return display_agent_label_prefix(&row.label);
    };
    let display_agent = display_agent_name(raw_agent);
    if row.label.starts_with(&display_agent) {
        return row.label.clone();
    }
    if let Some(rest) = row.label.strip_prefix(raw_agent) {
        return format!("{display_agent}{rest}");
    }
    display_agent_label_prefix(&row.label)
}

fn chat_prompt_label(row: &SidebarRow) -> String {
    if let Some(prompt) = row
        .meta
        .as_ref()
        .and_then(|meta| meta.prompt.as_deref())
        .filter(|prompt| !prompt.trim().is_empty())
    {
        return prompt.to_string();
    }
    row.label
        .split_once(':')
        .map(|(_, prompt)| prompt.trim().to_string())
        .unwrap_or_default()
}

fn elapsed_label(secs: i64) -> String {
    crate::sidebar::tree::humanize_secs(secs)
}

fn elapsed_full_label(secs: i64) -> String {
    crate::sidebar::tree::humanize_secs_full(secs)
}

fn expanded_chat_right_label(row: &SidebarRow) -> Option<String> {
    let state = expanded_chat_state_label(row)?;
    match row.badge_state? {
        BadgeState::Blocked | BadgeState::Working => row
            .meta
            .as_ref()
            .and_then(|meta| meta.elapsed_secs)
            .map(|secs| format!("{state} {}", elapsed_full_label(secs))),
        BadgeState::Done | BadgeState::Idle => row
            .meta
            .as_ref()
            .and_then(|meta| meta.completed_age_secs)
            .map(|secs| format!("{state} {} ago", elapsed_label(secs))),
    }
}

fn expanded_chat_state_label(row: &SidebarRow) -> Option<String> {
    let badge_state = row.badge_state?;
    let mut state = match badge_state {
        BadgeState::Blocked => match row.rollup {
            RollupLevel::Error => "Error",
            RollupLevel::Permission | RollupLevel::Waiting => "Waiting",
            _ => "Blocked",
        },
        BadgeState::Working => "Running",
        BadgeState::Done => "Done",
        BadgeState::Idle => "Idle",
    }
    .to_string();

    if matches!(badge_state, BadgeState::Blocked)
        && let Some(wait_reason) = row
            .meta
            .as_ref()
            .and_then(|meta| meta.wait_reason.as_deref())
            .filter(|value| !value.trim().is_empty())
    {
        state.push_str(&format!(" ({wait_reason})"));
    }
    Some(state)
}

fn right_style(row: &SidebarRow, theme: &SidebarRenderTheme) -> Style {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            Style::default().fg(theme.badge_color(BadgeState::Blocked))
        }
        SidebarRowKind::Chat if row.expanded && right_label(row).is_some() => {
            Style::default().fg(theme.badge_color(row.badge_state.unwrap_or(BadgeState::Idle)))
        }
        SidebarRowKind::Chat
            if !row.expanded
                && right_label(row)
                    .as_deref()
                    .is_some_and(|label| label.ends_with(" ago")) =>
        {
            Style::default().fg(theme.badge_color(BadgeState::Idle))
        }
        SidebarRowKind::Chat
            if row.badge_state == Some(BadgeState::Done)
                && !row.expanded
                && right_label(row).is_some() =>
        {
            Style::default().fg(Color::White)
        }
        SidebarRowKind::Chat
            if row.rollup == RollupLevel::Idle && !row.expanded && right_label(row).is_some() =>
        {
            Style::default().fg(theme.detail)
        }
        _ => Style::default().fg(theme.rollup_color(row.rollup)),
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
    match row.kind {
        SidebarRowKind::Zone => Style::default().fg(Color::Reset),
        SidebarRowKind::Category => Style::default()
            .fg(theme.category)
            .add_modifier(Modifier::BOLD),
        SidebarRowKind::Repo => Style::default().fg(theme.repo).add_modifier(Modifier::BOLD),
        SidebarRowKind::Chat => Style::default().fg(Color::Reset),
        SidebarRowKind::Detail if row.id.ends_with("::prompt") => Style::default().fg(Color::Reset),
        SidebarRowKind::Detail => Style::default().fg(theme.detail),
        SidebarRowKind::Jump => Style::default().fg(theme.detail),
    }
}

const JUMP_ACTION_LABEL: &str = "↗ Jump";
const PREVIEW_ACTION_LABEL: &str = "⌕ Preview";
const MARK_DONE_ACTION_LABEL: &str = "✓ Complete";
const ACTION_SEPARATOR: &str = " · ";
const JUMP_ROW_LABEL: &str = "↗ Jump · ⌕ Preview · ✓ Complete";

fn jump_action_spans(label: &str, theme: &SidebarRenderTheme) -> Vec<Span<'static>> {
    if label != JUMP_ROW_LABEL {
        return action_label_spans(label, theme);
    }
    let separator = Style::default().fg(theme.marker);
    let text = Style::default().fg(theme.detail);
    vec![
        Span::styled("↗".to_string(), Style::default().fg(theme.action_icon)),
        Span::styled(" Jump".to_string(), text),
        Span::styled(ACTION_SEPARATOR.to_string(), separator),
        Span::styled("⌕".to_string(), Style::default().fg(theme.action_icon)),
        Span::styled(" Preview".to_string(), text),
        Span::styled(ACTION_SEPARATOR.to_string(), separator),
        Span::styled("✓".to_string(), Style::default().fg(theme.badge_done)),
        Span::styled(" Complete".to_string(), text),
    ]
}

fn action_label_spans(label: &str, theme: &SidebarRenderTheme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut text = String::new();
    let flush_text = |spans: &mut Vec<Span<'static>>, text: &mut String| {
        if !text.is_empty() {
            spans.push(Span::styled(
                std::mem::take(text),
                Style::default().fg(theme.detail),
            ));
        }
    };
    for ch in label.chars() {
        match ch {
            '↗' => {
                flush_text(&mut spans, &mut text);
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().fg(theme.action_icon),
                ));
            }
            '⌕' => {
                flush_text(&mut spans, &mut text);
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().fg(theme.action_icon),
                ));
            }
            '✓' => {
                flush_text(&mut spans, &mut text);
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().fg(theme.badge_done),
                ));
            }
            '·' => {
                flush_text(&mut spans, &mut text);
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().fg(theme.marker),
                ));
            }
            _ => text.push(ch),
        }
    }
    flush_text(&mut spans, &mut text);
    spans
}

pub fn jump_row_action_at(
    row: &SidebarRow,
    column: u16,
    rendered_width: u16,
) -> Option<JumpRowAction> {
    if row.kind != SidebarRowKind::Jump {
        return None;
    }
    let column = column as usize;
    // Every rendered row reserves its final cell as padding. At narrow widths the
    // action label is truncated, so the full label's hit area must not extend into
    // cells where no action was drawn.
    if column >= usize::from(rendered_width).saturating_sub(1) {
        return None;
    }
    if jump_row_action_range(row, JumpRowAction::Jump).contains(&column) {
        Some(JumpRowAction::Jump)
    } else if jump_row_action_range(row, JumpRowAction::Preview).contains(&column) {
        Some(JumpRowAction::Preview)
    } else if jump_row_action_range(row, JumpRowAction::MarkDone).contains(&column) {
        Some(JumpRowAction::MarkDone)
    } else {
        None
    }
}

pub fn jump_row_action_start(row: &SidebarRow) -> usize {
    3 + 2 * row.depth
}

pub fn jump_row_action_column(row: &SidebarRow, action: JumpRowAction) -> usize {
    jump_row_action_range(row, action).start
}

fn jump_row_action_range(row: &SidebarRow, action: JumpRowAction) -> std::ops::Range<usize> {
    let jump_start = jump_row_action_start(row);
    let jump_end = jump_start + display_width(JUMP_ACTION_LABEL);
    let preview_start = jump_end + display_width(ACTION_SEPARATOR);
    let preview_end = preview_start + display_width(PREVIEW_ACTION_LABEL);
    let mark_done_start = preview_end + display_width(ACTION_SEPARATOR);
    let mark_done_end = mark_done_start + display_width(MARK_DONE_ACTION_LABEL);

    match action {
        JumpRowAction::Jump => jump_start..jump_end,
        JumpRowAction::Preview => preview_start..preview_end,
        JumpRowAction::MarkDone => mark_done_start..mark_done_end,
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
        ViewMode::Flat => "Flat",
        ViewMode::ByRepo => "Repository",
        ViewMode::ByCategory => "Category",
    }
}

fn view_mode_label_padded(view_mode: ViewMode) -> String {
    let width = [ViewMode::Flat, ViewMode::ByRepo, ViewMode::ByCategory]
        .into_iter()
        .map(|mode| view_mode_label(mode).len())
        .max()
        .unwrap_or(0);
    format!("{:<width$}", view_mode_label(view_mode))
}

fn visible_segment_range(text: &str, start: usize, len: usize) -> Option<std::ops::Range<u16>> {
    let visible_len = display_width(text);
    if start >= visible_len {
        return None;
    }
    let end = (start + len).min(visible_len);
    Some(start as u16..end as u16)
}

fn slice_display(text: &str, start: u16, end: u16) -> String {
    let mut cell = 0_u16;
    let mut out = String::new();
    for ch in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if cell >= end {
            break;
        }
        if cell >= start && cell + width <= end {
            out.push(ch);
        }
        cell = cell.saturating_add(width);
    }
    out
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
            active: false,
            meta: None,
        }
    }

    fn assert_span_fg(spans: &[Span<'_>], content: &str, color: Color) {
        assert!(
            spans
                .iter()
                .any(|span| span.content.as_ref() == content && span.style.fg == Some(color)),
            "span {content:?} with fg {color:?} not found in {spans:?}"
        );
    }

    #[test]
    fn zone_row_renders_as_colored_heading() {
        let mut zone = row(
            "zone::triage",
            SidebarRowKind::Zone,
            0,
            "TRIAGE",
            RollupLevel::Permission,
        );
        zone.chat_count = 2;

        let lines = render_lines(
            &[zone],
            &SidebarState::default(),
            30,
            &SidebarRenderTheme::default(),
        );
        let text = line_to_string(lines[0].clone());

        assert!(text.starts_with(" ▍TRIAGE 2"), "{text:?}");
        assert!(
            lines[0].spans.iter().any(|span| {
                span.style.fg == Some(Color::Red)
                    && span.style.add_modifier.contains(Modifier::BOLD)
            }),
            "{lines:?}"
        );
    }

    #[test]
    fn branch_defaults_to_muted_cyan() {
        assert_eq!(SidebarRenderTheme::default().branch, Color::Indexed(73));
    }

    #[test]
    fn selection_and_active_colors_are_configurable() {
        let config = crate::config::SidebarColorsConfig {
            action_icon: Some("#74c7ec".to_string()),
            selection_bar: Some("#f2d98f".to_string()),
            active_bg: Some("235".to_string()),
            active_bar: Some("magenta".to_string()),
            ..Default::default()
        };
        let theme = SidebarRenderTheme::from_config(&config);

        assert_eq!(theme.action_icon, Color::Rgb(0x74, 0xc7, 0xec));
        assert_eq!(theme.selection_bar, Color::Rgb(0xf2, 0xd9, 0x8f));
        assert_eq!(theme.active_bg, Color::Indexed(235));
        assert_eq!(theme.active_bar, Color::Magenta);
        assert_eq!(SidebarRenderTheme::default().active_bg, Color::Indexed(235));
        assert_eq!(
            SidebarRenderTheme::default().action_icon,
            Color::Indexed(73)
        );
        assert_eq!(
            SidebarRenderTheme::default().active_bar,
            Color::Indexed(147)
        );
        assert_eq!(
            SidebarRenderTheme::default().selection_bar,
            Color::Indexed(229)
        );
    }

    #[test]
    fn sidebar_render_theme_reads_task_subagent_and_worktree_detail_colors() {
        let config = crate::config::SidebarColorsConfig {
            task_done: Some("220".to_string()),
            task_working: Some("221".to_string()),
            task_pending: Some("darkgray".to_string()),
            task_label: Some("246".to_string()),
            subagent_label: Some("73".to_string()),
            subagent_id: Some("74".to_string()),
            worktree: Some("cyan".to_string()),
            worktree_activity: Some("#4fd08a".to_string()),
            ..Default::default()
        };
        let theme = SidebarRenderTheme::from_config(&config);

        assert_eq!(theme.task_done, Color::Indexed(220));
        assert_eq!(theme.task_working, Color::Indexed(221));
        assert_eq!(theme.task_pending, Color::DarkGray);
        assert_eq!(theme.task_label, Color::Indexed(246));
        assert_eq!(theme.subagent_label, Color::Indexed(73));
        assert_eq!(theme.subagent_id, Color::Indexed(74));
        assert_eq!(theme.worktree, Color::Cyan);
        assert_eq!(theme.worktree_activity, Color::Rgb(79, 208, 138));

        let default = SidebarRenderTheme::default();
        assert_eq!(default.task_done, Color::Indexed(220));
        assert_eq!(default.task_working, Color::Indexed(220));
        assert_eq!(default.task_pending, Color::DarkGray);
        assert_eq!(default.task_label, Color::Indexed(246));
        assert_eq!(default.subagent_label, Color::Indexed(73));
        assert_eq!(default.subagent_id, Color::Indexed(73));
        assert_eq!(default.worktree, Color::Indexed(73));
        assert_eq!(default.worktree_activity, Color::Indexed(73));
    }

    #[test]
    fn width_tier_boundaries() {
        assert_eq!(WidthTier::from_width(2), WidthTier::Rail);
        assert_eq!(WidthTier::from_width(3), WidthTier::Rail);
        assert_eq!(WidthTier::from_width(4), WidthTier::Micro);
        assert_eq!(WidthTier::from_width(23), WidthTier::Micro);
        assert_eq!(WidthTier::from_width(24), WidthTier::Dense);
        assert_eq!(WidthTier::from_width(35), WidthTier::Dense);
        assert_eq!(WidthTier::from_width(36), WidthTier::Standard);
    }

    #[test]
    fn dense_tier_renders_one_line_per_chat_with_origin_abbrev() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "claude: fix the bug",
            RollupLevel::Running,
        );
        chat.badge_state = Some(crate::daemon::session_badge::BadgeState::Working);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("claude".to_string()),
            elapsed_secs: Some(780),
            origin: Some("misc/vde-tmux".to_string()),
            ..Default::default()
        });

        let rendered = render_rows(&[chat], &SidebarState::default(), 30);

        assert!(rendered.contains("● Claude  vde"), "{rendered:?}");
        assert!(rendered.ends_with("13m "), "{rendered:?}");
    }

    #[test]
    fn dense_tier_renders_badge_glyph_in_status_color() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "claude: fix the bug",
            RollupLevel::Running,
        );
        chat.badge_state = Some(crate::daemon::session_badge::BadgeState::Working);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("claude".to_string()),
            elapsed_secs: Some(780),
            origin: Some("misc/vde-tmux".to_string()),
            ..Default::default()
        });
        let theme = SidebarRenderTheme::default();

        let lines = render_lines(&[chat], &SidebarState::default(), 30, &theme);

        let glyph = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "●")
            .unwrap_or_else(|| panic!("badge glyph span not found: {:?}", lines[0]));
        assert_eq!(glyph.style.fg, Some(theme.badge_color(BadgeState::Working)));
    }

    #[test]
    fn micro_tier_renders_glyph_and_status_only() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Permission,
        );
        chat.badge_state = Some(crate::daemon::session_badge::BadgeState::Blocked);
        chat.expanded = false;

        let rendered = render_rows(&[chat], &SidebarState::default(), 8);

        assert_eq!(rendered, " ▲ Perm ");
    }

    #[test]
    fn rail_renders_counts_then_rows() {
        let mut blocked1 = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Permission,
        );
        blocked1.badge_state = Some(crate::daemon::session_badge::BadgeState::Blocked);
        let mut blocked2 = row(
            "chat::%2",
            SidebarRowKind::Chat,
            0,
            "claude",
            RollupLevel::Permission,
        );
        blocked2.badge_state = Some(crate::daemon::session_badge::BadgeState::Blocked);
        let mut working = row(
            "chat::%3",
            SidebarRowKind::Chat,
            0,
            "opencode",
            RollupLevel::Running,
        );
        working.badge_state = Some(crate::daemon::session_badge::BadgeState::Working);

        let rendered = render_rows(&[blocked1, blocked2, working], &SidebarState::default(), 3);

        assert_eq!(rendered, "▲2\n●1\n──\n ▲\n ▲\n ●");
    }

    #[test]
    fn rail_uses_explicit_overflow_marker_for_double_digit_counts() {
        let rows = (0..10)
            .map(|index| {
                let mut chat = row(
                    &format!("chat::%{index}"),
                    SidebarRowKind::Chat,
                    0,
                    "codex",
                    RollupLevel::Running,
                );
                chat.badge_state = Some(BadgeState::Working);
                chat
            })
            .collect::<Vec<_>>();

        let rendered = render_rows(&rows, &SidebarState::default(), 3);

        assert_eq!(rendered.lines().next(), Some("●9+"));
        assert!(rendered.lines().all(|line| display_width(line) <= 3));
    }

    #[test]
    fn rail_does_not_double_count_expanded_chat() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            1,
            "codex",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.expanded = true;
        chat.pane_id = Some("%1".to_string());
        let mut jump = row(
            "jump::%1",
            SidebarRowKind::Jump,
            2,
            "jump",
            RollupLevel::Running,
        );
        jump.badge_state = Some(BadgeState::Working);
        jump.pane_id = Some("%1".to_string());

        let text = render_rows(&[chat, jump], &SidebarState::default(), 2);

        assert_eq!(text, "●1\n──\n ●");
    }

    #[test]
    fn dense_micro_and_rail_modes_continue_to_omit_detail_rows() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            elapsed_secs: Some(60),
            ..Default::default()
        });
        let detail = row(
            "detail::%1::task::0::in_progress",
            SidebarRowKind::Detail,
            1,
            "\u{2514} ● Task - Build",
            RollupLevel::Running,
        );
        let rows = vec![chat, detail];

        for width in [2, 12, 30] {
            let rendered = render_rows(&rows, &SidebarState::default(), width);
            assert!(!rendered.contains("Task - Build"), "{width}: {rendered:?}");
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

        let rendered = render_rows(&rows, &state, 40);

        assert!(rendered.contains(" ▾ app"));
        assert!(rendered.contains("▎   ▾ Codex %1"));
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
        assert_eq!(rendered, "▲1\n──\n ▲");
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

        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.as_ref() == "▾ "
                    && span.style.fg == Some(Color::Indexed(147))
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "{:?}",
            lines[0]
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.trim() == "misc"
                    && span.style.fg == Some(Color::Indexed(215))
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "{:?}",
            lines[0]
        );
        assert_eq!(lines[1].style.bg, Some(Color::Rgb(0x30, 0x30, 0x34)));
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
    fn category_and_repo_rows_use_distinct_colors() {
        let theme = SidebarRenderTheme::default();
        let category = row(
            "category::misc",
            SidebarRowKind::Category,
            0,
            "misc",
            RollupLevel::Idle,
        );
        let repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Idle,
        );

        assert_eq!(row_style(&category, &theme).fg, Some(Color::Indexed(215)));
        assert_eq!(row_style(&repo, &theme).fg, Some(Color::LightCyan));
    }

    #[test]
    fn mode_segment_uses_header_mode_color_and_glyph() {
        let theme = SidebarRenderTheme::default();
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };

        let mode_style = mode_segment_style(&theme);
        assert_eq!(mode_style.fg, Some(Color::Indexed(16)));
        assert_eq!(mode_style.bg, Some(Color::Indexed(147)));
        assert!(mode_style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            build_header_layout(&state, 80).lines[1].text,
            format!(" ≣ Repository ▾ \u{e0b0} 0 tasks \u{e0b0}")
        );
    }

    #[test]
    fn reset_header_background_keeps_mode_and_section_foreground_only() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
            r##"
sidebar:
  header:
    prefix: ""
    suffix: ""
    bold: true
    colors:
      fg: "#b4befe"
      bg: reset
  colors:
    category: "#cba6f7"
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
        let header = build_header_layout_with_counts(
            &SidebarState::default(),
            80,
            &theme,
            rich_header_counts(),
        );

        let section = style_for_segment(&header, 0, "SIDEBAR");
        assert_eq!(section.fg, Some(Color::Rgb(0xcb, 0xa6, 0xf7)));
        assert_eq!(section.bg, None);
        assert!(section.add_modifier.contains(Modifier::BOLD));

        let mode = style_for_segment(&header, 1, "≣ Category");
        assert_eq!(mode.fg, Some(Color::Rgb(0xb4, 0xbe, 0xfe)));
        assert_eq!(mode.bg, None);
        assert!(mode.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn header_filter_positions_are_stable_across_view_modes() {
        let text_for = |view_mode: ViewMode| {
            let state = SidebarState {
                view_mode,
                ..SidebarState::default()
            };
            build_header_layout(&state, 80).lines[2].text.clone()
        };
        let flat = text_for(ViewMode::Flat);
        let repo = text_for(ViewMode::ByRepo);
        let category = text_for(ViewMode::ByCategory);

        assert_eq!(flat.find('≡'), repo.find('≡'), "{flat:?} vs {repo:?}");
        assert_eq!(
            repo.find('≡'),
            category.find('≡'),
            "{repo:?} vs {category:?}"
        );
        assert_eq!(display_width(&flat), display_width(&repo));
        assert_eq!(display_width(&repo), display_width(&category));
    }

    #[test]
    fn category_row_label_omits_diamond_in_every_tier() {
        let category = row(
            "category::dev",
            SidebarRowKind::Category,
            0,
            "dev",
            RollupLevel::Idle,
        );

        let standard = render_rows(
            std::slice::from_ref(&category),
            &SidebarState::default(),
            40,
        );
        let dense = render_rows(&[category], &SidebarState::default(), 30);

        assert!(standard.contains("▾ dev"), "{standard:?}");
        assert!(!standard.contains("◆"), "{standard:?}");
        assert!(!dense.contains("◆"), "{dense:?}");
    }

    #[test]
    fn category_row_fills_remaining_width_with_rule() {
        let mut category = row(
            "category::dev",
            SidebarRowKind::Category,
            0,
            "dev",
            RollupLevel::Idle,
        );
        category.meta = Some(crate::sidebar::tree::RowMeta {
            attention_count: Some(1),
            ..Default::default()
        });

        let rendered = render_rows(&[category], &SidebarState::default(), 40);

        assert!(rendered.contains("▾ dev ─"), "{rendered:?}");
        assert!(rendered.contains("─ ▲1 "), "{rendered:?}");
    }

    #[test]
    fn repo_and_chat_rows_keep_space_filler() {
        let repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Idle,
        );
        let chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Idle,
        );

        let rendered = render_rows(&[repo, chat], &SidebarState::default(), 40);

        assert!(!rendered.contains('─'), "{rendered:?}");
    }

    #[test]
    fn active_rows_render_left_bar_without_chat_bg() {
        let mut category = row(
            "category::dev",
            SidebarRowKind::Category,
            0,
            "dev",
            RollupLevel::Idle,
        );
        category.active = true;
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            1,
            "codex",
            RollupLevel::Running,
        );
        chat.active = true;
        chat.expanded = false;
        let theme = SidebarRenderTheme::default();

        let lines = render_lines(
            &[category.clone(), chat.clone()],
            &SidebarState::default(),
            40,
            &theme,
        );

        assert_eq!(line_to_string(lines[0].clone()).chars().next(), Some('▎'));
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.active_bar));
        assert_eq!(lines[0].style.bg, None);
        assert_eq!(line_to_string(lines[1].clone()).chars().next(), Some('▎'));
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.active_bar));
        assert_eq!(lines[1].style.bg, None);
        assert_eq!(line_to_string(lines[2].clone()).chars().next(), Some('▎'));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme.active_bar));
        assert_eq!(lines[2].style.bg, None);

        let selected = SidebarState {
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        let selected_lines = render_lines(&[chat], &selected, 40, &theme);
        assert_eq!(selected_lines[0].style.bg, Some(theme.selection_bg));
        assert_eq!(selected_lines[1].style.bg, Some(theme.selection_bg));
        assert_eq!(
            selected_lines[0].spans[0].style.fg,
            Some(theme.selection_bar)
        );
        assert_ne!(theme.selection_bar, theme.active_bar);
        assert_eq!(
            line_to_string(selected_lines[0].clone()).chars().next(),
            Some('▎')
        );
        assert_eq!(
            line_to_string(selected_lines[1].clone()).chars().next(),
            Some('▎')
        );
    }

    #[test]
    fn expanded_chat_selection_styles_chat_detail_and_jump_rows() {
        let mut chat = row(
            "chat::%1::101",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Running,
        );
        chat.expanded = true;
        chat.pane_id = Some("%1".to_string());
        let mut detail = row(
            "detail::%1::101::prompt",
            SidebarRowKind::Detail,
            1,
            "review PR",
            RollupLevel::Running,
        );
        detail.pane_id = Some("%1".to_string());
        let mut jump = row(
            "jump::%1::101",
            SidebarRowKind::Jump,
            1,
            "jump",
            RollupLevel::Running,
        );
        jump.pane_id = Some("%1".to_string());
        let other = row(
            "chat::%2::202",
            SidebarRowKind::Chat,
            0,
            "claude",
            RollupLevel::Running,
        );
        let state = SidebarState {
            selection: Some("chat::%1::101".to_string()),
            ..SidebarState::default()
        };
        let theme = SidebarRenderTheme::default();

        let lines = render_lines(&[chat, detail, jump, other], &state, 60, &theme);

        assert_eq!(lines[0].style.bg, Some(theme.selection_bg));
        assert_eq!(lines[1].style.bg, Some(theme.selection_bg));
        assert_eq!(lines[2].style.bg, Some(theme.selection_bg));
        assert_eq!(lines[3].style.bg, None);
        assert_eq!(line_to_string(lines[0].clone()).chars().next(), Some('▎'));
        assert_eq!(line_to_string(lines[1].clone()).chars().next(), Some(' '));
        assert_eq!(line_to_string(lines[2].clone()).chars().next(), Some(' '));
    }

    #[test]
    fn jump_row_renders_action_buttons() {
        let jump = row(
            "jump::%1",
            SidebarRowKind::Jump,
            2,
            "jump",
            RollupLevel::Running,
        );

        let rendered = render_rows(std::slice::from_ref(&jump), &SidebarState::default(), 80);

        assert!(
            rendered.starts_with("     └ ↗ Jump · ⌕ Preview · ✓ Complete"),
            "{rendered:?}"
        );

        let theme = SidebarRenderTheme {
            action_icon: Color::Rgb(0x74, 0xc7, 0xec),
            ..SidebarRenderTheme::default()
        };
        let lines = render_lines(&[jump], &SidebarState::default(), 80, &theme);
        let style_of = |needle: &str| {
            lines[0]
                .spans
                .iter()
                .find(|span| span.content == needle)
                .unwrap_or_else(|| panic!("span {needle:?} not found: {:?}", lines[0]))
                .style
        };
        assert_eq!(style_of("↗").fg, Some(theme.action_icon));
        assert_eq!(style_of("⌕").fg, Some(theme.action_icon));
        assert_eq!(style_of("✓").fg, Some(theme.badge_done));
        assert_eq!(style_of(" Jump").fg, Some(theme.detail));
        assert_eq!(style_of(" Preview").fg, Some(theme.detail));
        assert_eq!(style_of(" Complete").fg, Some(theme.detail));
        assert_eq!(style_of(ACTION_SEPARATOR).fg, Some(theme.marker));
    }

    #[test]
    fn truncated_jump_row_keeps_preview_icon_color() {
        let theme = SidebarRenderTheme {
            action_icon: Color::Rgb(0x74, 0xc7, 0xec),
            ..SidebarRenderTheme::default()
        };
        let spans = jump_action_spans("↗ Jump · ⌕ Preview · ✓ Com…", &theme);
        let style_of = |needle: &str| {
            spans
                .iter()
                .find(|span| span.content == needle)
                .unwrap_or_else(|| panic!("span {needle:?} not found: {spans:?}"))
                .style
        };

        assert_eq!(style_of("↗").fg, Some(theme.action_icon));
        assert_eq!(style_of("⌕").fg, Some(theme.action_icon));
        assert_eq!(style_of("✓").fg, Some(theme.badge_done));
        assert_eq!(style_of("·").fg, Some(theme.marker));
    }

    #[test]
    fn jump_row_hit_test_maps_columns_to_actions() {
        let jump = row(
            "jump::%1",
            SidebarRowKind::Jump,
            2,
            "jump",
            RollupLevel::Running,
        );

        // " " + indent(4) + "└ " => actions start at 7.
        // "↗ Jump"(7..13) · "⌕ Preview"(16..25) · "✓ Complete"(28..38)
        assert_eq!(jump_row_action_at(&jump, 6, 80), None);
        assert_eq!(jump_row_action_at(&jump, 7, 80), Some(JumpRowAction::Jump));
        assert_eq!(jump_row_action_at(&jump, 12, 80), Some(JumpRowAction::Jump));
        assert_eq!(jump_row_action_at(&jump, 13, 80), None);
        assert_eq!(jump_row_action_at(&jump, 15, 80), None);
        assert_eq!(
            jump_row_action_at(&jump, 16, 80),
            Some(JumpRowAction::Preview)
        );
        assert_eq!(
            jump_row_action_at(&jump, 24, 80),
            Some(JumpRowAction::Preview)
        );
        assert_eq!(jump_row_action_at(&jump, 25, 80), None);
        assert_eq!(jump_row_action_at(&jump, 27, 80), None);
        assert_eq!(
            jump_row_action_at(&jump, 28, 80),
            Some(JumpRowAction::MarkDone)
        );
        assert_eq!(
            jump_row_action_at(&jump, 37, 80),
            Some(JumpRowAction::MarkDone)
        );
        assert_eq!(jump_row_action_at(&jump, 38, 80), None);

        // Standard tier starts at width 36. Column 35 is the row's padding,
        // although the untruncated Complete label would extend through 37.
        assert_eq!(
            jump_row_action_at(&jump, 34, 36),
            Some(JumpRowAction::MarkDone)
        );
        assert_eq!(jump_row_action_at(&jump, 35, 36), None);
    }

    #[test]
    fn category_row_never_renders_badge_glyph() {
        let mut category = row(
            "category::dev",
            SidebarRowKind::Category,
            0,
            "dev",
            RollupLevel::Permission,
        );
        category.badge_state = Some(BadgeState::Blocked);

        let lines = render_lines(
            std::slice::from_ref(&category),
            &SidebarState::default(),
            40,
            &SidebarRenderTheme::default(),
        );
        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!text.contains('▲'), "{text:?}");
    }

    fn rich_header_counts() -> BadgeCounts {
        BadgeCounts {
            total: 7,
            attention: 2,
            blocked: 1,
            working: 1,
            done: 0,
            idle: 5,
        }
    }

    fn segment_text(line: &HeaderLine, segment: &HeaderSegment) -> String {
        slice_display(&line.text, segment.range.start, segment.range.end)
    }

    fn style_for_segment(layout: &HeaderLayout, row: usize, needle: &str) -> Style {
        let line = &layout.lines[row];
        line.segments
            .iter()
            .find(|segment| segment_text(line, segment).contains(needle))
            .unwrap_or_else(|| panic!("segment {needle:?} not found in {:?}", line.segments))
            .style
            .expect("segment style")
    }

    fn style_after_segment(layout: &HeaderLayout, row: usize, needle: &str) -> Style {
        let line = &layout.lines[row];
        let index = line
            .segments
            .iter()
            .position(|segment| segment_text(line, segment) == needle)
            .unwrap_or_else(|| panic!("segment {needle:?} not found in {:?}", line.segments));
        line.segments[index + 1]
            .style
            .expect("following segment style")
    }

    #[test]
    fn header_layout_uses_powerline_title_and_filter_chip_rows() {
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            filter: StatusFilter::All,
            ..SidebarState::default()
        };

        let header = build_header_layout_with_counts(
            &state,
            80,
            &SidebarRenderTheme::default(),
            rich_header_counts(),
        );

        assert_eq!(header.row_count(), 3);
        assert_eq!(header.lines[0].text, " SIDEBAR");
        assert_eq!(
            header.lines[1].text,
            format!(" ≣ Category   ▾ \u{e0b0} 7 tasks \u{e0b0}")
        );
        assert_eq!(header.lines[2].text, " ≡ 7  ▲ 2  ● 1  ✓ 0  ○ 5 ");
        let section = style_for_segment(&header, 0, "SIDEBAR");
        assert_eq!(section.fg, Some(SidebarRenderTheme::default().category));
        assert!(section.add_modifier.contains(Modifier::BOLD));
        let total = style_for_segment(&header, 1, " 7");
        assert_eq!(total.fg, Some(Color::Reset));
        assert!(total.add_modifier.contains(Modifier::BOLD));
        let task_label = style_for_segment(&header, 1, " tasks ");
        assert_eq!(task_label.fg, Some(SidebarRenderTheme::default().detail));
    }

    #[test]
    fn header_title_uses_singular_task_label_for_one_agent() {
        let counts = BadgeCounts {
            total: 1,
            idle: 1,
            ..BadgeCounts::default()
        };

        let header = build_header_layout_with_counts(
            &SidebarState::default(),
            80,
            &SidebarRenderTheme::default(),
            counts,
        );

        assert_eq!(
            header.lines[1].text,
            format!(" ≣ Category   ▾ \u{e0b0} 1 task \u{e0b0}")
        );
    }

    #[test]
    fn header_hit_test_ignores_total_segment_and_zero_count_chips() {
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            filter: StatusFilter::All,
            ..SidebarState::default()
        };

        let header = build_header_layout_with_counts(
            &state,
            80,
            &SidebarRenderTheme::default(),
            rich_header_counts(),
        );

        assert_eq!(header_hit_test(&header, 0, 2), None);
        assert_eq!(
            header_hit_test(&header, 1, 2),
            Some(HeaderAction::CycleViewMode)
        );
        assert_eq!(header_hit_test(&header, 1, 18), None);
        assert_eq!(
            header_hit_test(&header, 2, 1),
            Some(HeaderAction::SetFilter(StatusFilter::All))
        );
        assert_eq!(
            header_hit_test(&header, 2, 6),
            Some(HeaderAction::SetFilter(StatusFilter::AttentionOnly))
        );
        assert_eq!(
            header_hit_test(&header, 2, 11),
            Some(HeaderAction::SetFilter(StatusFilter::WorkingOnly))
        );
        assert_eq!(header_hit_test(&header, 2, 16), None);
        assert_eq!(
            header_hit_test(&header, 2, 21),
            Some(HeaderAction::SetFilter(StatusFilter::IdleOnly))
        );
    }

    #[test]
    fn attention_chip_uses_attention_count_and_is_clickable_without_blocked_count() {
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            filter: StatusFilter::All,
            ..SidebarState::default()
        };
        let counts = BadgeCounts {
            total: 2,
            attention: 2,
            blocked: 0,
            working: 2,
            ..BadgeCounts::default()
        };

        let header =
            build_header_layout_with_counts(&state, 80, &SidebarRenderTheme::default(), counts);

        assert!(
            header.lines[2].text.contains("▲ 2"),
            "{:?}",
            header.lines[2].text
        );
        assert_eq!(
            header_hit_test(&header, 2, 6),
            Some(HeaderAction::SetFilter(StatusFilter::AttentionOnly))
        );
    }

    #[test]
    fn active_chip_fg_follows_configured_header_fg() {
        let theme = SidebarRenderTheme {
            header_active_fg: Some(Color::Rgb(0x19, 0x16, 0x27)),
            ..SidebarRenderTheme::default()
        };
        let counts = BadgeCounts {
            total: 3,
            attention: 1,
            blocked: 1,
            working: 1,
            done: 0,
            idle: 1,
        };
        let state = SidebarState {
            filter: StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };

        let header = build_header_layout_with_counts(&state, 80, &theme, counts);

        let badge = style_for_segment(&header, 2, "▲");
        assert_eq!(badge.fg, Some(theme.badge_blocked));
        assert_eq!(badge.bg, Some(theme.header_mode));
        let active_text = style_after_segment(&header, 2, "▲");
        assert_eq!(active_text.fg, Some(Color::Rgb(0x19, 0x16, 0x27)));
        assert_eq!(active_text.bg, Some(theme.header_mode));
        assert!(!active_text.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn header_chip_fg_overrides_active_chip_fg_but_not_mode_fg() {
        let theme = SidebarRenderTheme {
            header_active_fg: Some(Color::Rgb(0x98, 0xb2, 0xf6)),
            header_chip_fg: Some(Color::Rgb(0x23, 0x23, 0x32)),
            ..SidebarRenderTheme::default()
        };
        let counts = BadgeCounts {
            total: 3,
            attention: 1,
            blocked: 1,
            working: 1,
            done: 0,
            idle: 1,
        };
        let state = SidebarState {
            filter: StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };

        let header = build_header_layout_with_counts(&state, 80, &theme, counts);

        let badge = style_for_segment(&header, 2, "▲");
        assert_eq!(badge.fg, Some(theme.badge_blocked));
        assert_eq!(badge.bg, Some(theme.header_mode));
        let active_text = style_after_segment(&header, 2, "▲");
        assert_eq!(active_text.fg, Some(Color::Rgb(0x23, 0x23, 0x32)));
        assert_eq!(active_text.bg, Some(theme.header_mode));
        let mode = style_for_segment(&header, 1, "≣");
        assert_eq!(mode.fg, Some(Color::Rgb(0x98, 0xb2, 0xf6)));
    }

    #[test]
    fn active_all_chip_bg_follows_configured_filter_bg() {
        let theme = SidebarRenderTheme {
            header_active_bg: Some(Color::Rgb(0x45, 0x3f, 0x9e)),
            header_filter_bg: Some(Color::Rgb(0xee, 0xee, 0xf4)),
            ..SidebarRenderTheme::default()
        };
        let state = SidebarState {
            filter: StatusFilter::All,
            ..SidebarState::default()
        };

        let header = build_header_layout_with_counts(&state, 80, &theme, rich_header_counts());

        let active = style_for_segment(&header, 2, "≡ 7");
        assert_eq!(active.bg, Some(Color::Rgb(0xee, 0xee, 0xf4)));
    }

    #[test]
    fn header_chips_use_configured_badge_glyphs() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
            r##"
badge:
  glyphs:
    working: "W"
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_app_config(&config);
        let state = SidebarState::default();

        let header = build_header_layout_with_counts(&state, 80, &theme, rich_header_counts());

        assert!(
            header.lines[2].text.contains("W 1"),
            "{:?}",
            header.lines[2].text
        );
        assert!(
            !header.lines[2].text.contains("● 1"),
            "{:?}",
            header.lines[2].text
        );
    }

    #[test]
    fn custom_header_suffix_is_rendered_after_total_segment() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
            r##"
sidebar:
  header:
    suffix: ""
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);

        let header = build_header_layout_with_counts(
            &SidebarState::default(),
            80,
            &theme,
            rich_header_counts(),
        );

        assert!(
            header.lines[1].text.ends_with("7 tasks "),
            "{:?}",
            header.lines[1].text
        );
    }

    #[test]
    fn chip_caps_render_as_pill_and_skip_zero_chips() {
        let theme = SidebarRenderTheme {
            header_chip_prefix: "\u{e0b6}".to_string(),
            header_chip_suffix: "\u{e0b4}".to_string(),
            ..SidebarRenderTheme::default()
        };
        let counts = BadgeCounts {
            total: 3,
            attention: 1,
            blocked: 1,
            working: 0,
            done: 0,
            idle: 2,
        };
        let state = SidebarState::default();

        let header = build_header_layout_with_counts(&state, 80, &theme, counts);
        let line = &header.lines[2];

        assert_eq!(
            line.text,
            "\u{e0b6} ≡ 3 \u{e0b4} \u{e0b6} ▲ 1 \u{e0b4}  ● 0   ✓ 0  \u{e0b6} ○ 2 \u{e0b4}"
        );
        let cap = style_for_segment(&header, 2, "\u{e0b6}");
        assert_eq!(cap.fg, Some(theme.header_mode));
        assert_eq!(
            header_hit_test(&header, 2, 0),
            Some(HeaderAction::SetFilter(StatusFilter::All))
        );
        let zero_badge = style_for_segment(&header, 2, "●");
        assert_eq!(zero_badge.fg, Some(theme.badge_working));
        assert_eq!(zero_badge.bg, None);
        assert!(zero_badge.add_modifier.contains(Modifier::DIM));
        let zero_count = style_after_segment(&header, 2, "●");
        assert_eq!(zero_count.fg, Some(theme.detail));
        assert_eq!(zero_count.bg, None);
        assert!(zero_count.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn header_chip_styles_distinguish_active_nonzero_and_zero_states() {
        let theme = SidebarRenderTheme::default();
        let counts = BadgeCounts {
            total: 7,
            attention: 0,
            blocked: 0,
            working: 2,
            done: 0,
            idle: 5,
        };
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            filter: StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };

        let header = build_header_layout_with_counts(&state, 80, &theme, counts);

        let mode = style_for_segment(&header, 1, "≣ Category");
        assert_eq!(mode.fg, Some(Color::Indexed(16)));
        assert_eq!(mode.bg, Some(theme.header_mode));
        assert!(mode.add_modifier.contains(Modifier::BOLD));

        let active_badge = style_for_segment(&header, 2, "▲");
        assert_eq!(active_badge.fg, Some(theme.badge_blocked));
        assert_eq!(active_badge.bg, Some(theme.header_mode));
        assert!(active_badge.add_modifier.contains(Modifier::BOLD));
        let active_count = style_after_segment(&header, 2, "▲");
        assert_eq!(active_count.fg, Some(Color::Indexed(16)));
        assert_eq!(active_count.bg, Some(theme.header_mode));
        assert!(active_count.add_modifier.contains(Modifier::BOLD));

        let working = style_for_segment(&header, 2, "●");
        assert_eq!(working.fg, Some(theme.badge_working));
        assert_eq!(working.bg, Some(theme.active_bg));

        let done_badge = style_for_segment(&header, 2, "✓");
        assert_eq!(done_badge.fg, Some(theme.badge_done));
        assert_eq!(done_badge.bg, None);
        assert!(done_badge.add_modifier.contains(Modifier::DIM));
        let done_count = style_after_segment(&header, 2, "✓");
        assert_eq!(done_count.fg, Some(theme.detail));
        assert_eq!(done_count.bg, None);
        assert!(done_count.add_modifier.contains(Modifier::DIM));
        assert!(!done_count.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn header_suffix_can_remove_powerline_arrow() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
            r##"
sidebar:
  header:
    suffix: ""
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
        let header = build_header_layout_with_counts(
            &SidebarState::default(),
            80,
            &theme,
            rich_header_counts(),
        );

        assert_eq!(theme.header_suffix, "");
        assert!(!header.lines[1].text.contains('\u{e0b0}'));
        assert_eq!(header.lines[1].text, " ≣ Category   ▾  7 tasks ");
    }

    #[test]
    fn header_width_fallback_drops_total_before_truncating_mode() {
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            ..SidebarState::default()
        };

        let compact = build_header_layout_with_counts(
            &state,
            12,
            &SidebarRenderTheme::default(),
            rich_header_counts(),
        );
        assert_eq!(compact.lines[1].text, " ≣ Category…");
        assert!(!compact.lines[1].text.contains("tasks"));

        let narrow = build_header_layout_with_counts(
            &state,
            6,
            &SidebarRenderTheme::default(),
            rich_header_counts(),
        );
        assert!(display_width(&narrow.lines[1].text) <= 6);
        assert!(
            narrow.lines[1].text.ends_with('…'),
            "{:?}",
            narrow.lines[1].text
        );
    }

    #[test]
    fn footer_documents_forward_and_reverse_filter_keys() {
        let footer = line_to_string(build_footer_line(64));

        assert!(footer.contains("tab/S-tab filter"), "{footer:?}");
    }

    #[test]
    fn header_mode_badge_style_can_be_configured() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
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
        let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
        let state = SidebarState::default();

        let header = build_header_layout_with_counts(&state, 80, &theme, rich_header_counts());
        let lines = render_header_lines(&header, &theme);
        let mode = style_for_segment(&header, 1, "≣ Category");
        let suffix = style_for_segment(&header, 1, "]");

        assert_eq!(header.lines[1].text, "[ ≣ Category   ] 7 tasks ]");
        assert_eq!(mode.fg, Some(Color::White));
        assert_eq!(mode.bg, Some(Color::Indexed(24)));
        assert!(mode.add_modifier.contains(Modifier::BOLD));
        assert_eq!(suffix.fg, Some(Color::Indexed(24)));
        assert_eq!(suffix.bg, Some(Color::Indexed(235)));
        assert_eq!(
            lines[1].spans[0].style,
            Style::default().fg(Color::Indexed(24))
        );
        assert_eq!(lines[1].spans[1].style, mode);
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

        assert!(rendered.contains("● Codex (%1)"), "{rendered}");
        assert!(!rendered.contains("[Running]"), "{rendered}");
    }

    #[test]
    fn colorize_follows_ideal_multi_tone_scheme() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "claude: fix flicker",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.active = true;
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("claude".to_string()),
            elapsed_secs: Some(780),
            ..Default::default()
        });
        let detail = row(
            "detail::%1::note",
            SidebarRowKind::Detail,
            1,
            "plain detail",
            RollupLevel::Running,
        );
        let prompt_detail = row(
            "detail::%1::prompt",
            SidebarRowKind::Detail,
            1,
            "fix flicker",
            RollupLevel::Running,
        );
        let lines = render_lines(
            &[chat, detail, prompt_detail],
            &SidebarState::default(),
            40,
            &SidebarRenderTheme::default(),
        );

        let chat_spans = &lines[0].spans;
        assert!(
            chat_spans
                .iter()
                .any(|span| span.content.as_ref() == "Claude"
                    && span.style.fg == Some(Color::Reset)
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "{chat_spans:?}"
        );
        assert!(
            chat_spans
                .iter()
                .any(|span| span.content.as_ref() == "Claude"
                    && span.style.fg == Some(Color::Reset)
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "{chat_spans:?}"
        );
        let prompt_spans = &lines[1].spans;
        assert!(
            prompt_spans
                .iter()
                .any(|span| span.content.as_ref().contains("fix flicker")
                    && span.style.fg == Some(Color::Reset)
                    && !span.style.add_modifier.contains(Modifier::BOLD)),
            "{prompt_spans:?}"
        );
        assert_eq!(chat_spans[0].content.as_ref(), "▎");
        assert_eq!(chat_spans[0].style.fg, Some(Color::Indexed(147)));
        assert!(
            chat_spans.iter().any(|span| span.content.as_ref() == "▸ "
                && span.style.fg == Some(Color::Indexed(147))
                && span.style.add_modifier.contains(Modifier::BOLD)),
            "{chat_spans:?}"
        );
        assert!(
            chat_spans
                .iter()
                .any(|span| span.content.as_ref() == "Running 13m 00s"
                    && span.style.fg == Some(Color::Green)
                    && !span.style.add_modifier.contains(Modifier::DIM)),
            "{chat_spans:?}"
        );
        let detail_spans = &lines[2].spans;
        assert!(
            detail_spans
                .iter()
                .any(|span| span.content.as_ref().contains("plain detail")
                    && span.style.fg == Some(Color::Indexed(246))
                    && !span.style.add_modifier.contains(Modifier::DIM)),
            "{detail_spans:?}"
        );
        let prompt_detail_spans = &lines[3].spans;
        assert!(
            prompt_detail_spans
                .iter()
                .any(|span| span.content.as_ref().contains("fix flicker")
                    && span.style.fg == Some(Color::Reset)
                    && !span.style.add_modifier.contains(Modifier::DIM)),
            "{prompt_detail_spans:?}"
        );
    }

    #[test]
    fn task_detail_rows_colorize_status_icons() {
        let theme = SidebarRenderTheme::default();
        let rows = vec![
            row(
                "detail::%1::task::0::completed",
                SidebarRowKind::Detail,
                1,
                "\u{251c} ✓ Task - Explore",
                RollupLevel::Running,
            ),
            row(
                "detail::%1::task::1::in_progress",
                SidebarRowKind::Detail,
                1,
                "\u{251c} ● Task - Build",
                RollupLevel::Running,
            ),
            row(
                "detail::%1::task::2::pending",
                SidebarRowKind::Detail,
                1,
                "\u{2514} ○ Task - Verify",
                RollupLevel::Running,
            ),
        ];

        let lines = render_lines(&rows, &SidebarState::default(), 60, &theme);

        assert_span_fg(&lines[0].spans, "✓", theme.task_done);
        assert_span_fg(&lines[1].spans, "●", theme.task_working);
        assert_span_fg(&lines[2].spans, "○", theme.task_pending);
        assert_span_fg(&lines[0].spans, " Task - Explore", theme.task_label);
        assert_span_fg(&lines[0].spans, "\u{251c} ", theme.marker);
    }

    #[test]
    fn subagent_detail_rows_colorize_label_and_id() {
        let theme = SidebarRenderTheme::default();
        let detail = row(
            "detail::%1::subagent::0",
            SidebarRowKind::Detail,
            1,
            "\u{2514} Agent - Explore #sub1",
            RollupLevel::Running,
        );

        let lines = render_lines(&[detail], &SidebarState::default(), 60, &theme);

        assert_span_fg(&lines[0].spans, "\u{2514} ", theme.marker);
        assert_span_fg(&lines[0].spans, "Agent - Explore", theme.subagent_label);
        assert_span_fg(&lines[0].spans, " #sub1", theme.subagent_id);
    }

    #[test]
    fn worktree_detail_row_uses_worktree_color() {
        let theme = SidebarRenderTheme::default();
        let detail = row(
            "detail::%1::worktree",
            SidebarRowKind::Detail,
            1,
            "+ feature",
            RollupLevel::Running,
        );

        let lines = render_lines(&[detail], &SidebarState::default(), 60, &theme);

        assert_span_fg(&lines[0].spans, "+ feature", theme.worktree);
    }

    #[test]
    fn worktree_activity_detail_row_uses_worktree_activity_color() {
        let theme = SidebarRenderTheme::default();
        let detail = row(
            "detail::%1::worktree-activity",
            SidebarRowKind::Detail,
            1,
            "vw exec feature",
            RollupLevel::Running,
        );

        let lines = render_lines(&[detail], &SidebarState::default(), 60, &theme);

        assert_span_fg(&lines[0].spans, "vw exec feature", theme.worktree_activity);
    }

    #[test]
    fn prompt_detail_row_keeps_reset_color() {
        let theme = SidebarRenderTheme::default();
        let detail = row(
            "detail::%1::prompt",
            SidebarRowKind::Detail,
            1,
            "fix flicker",
            RollupLevel::Running,
        );

        let lines = render_lines(&[detail], &SidebarState::default(), 60, &theme);

        assert_span_fg(&lines[0].spans, "fix flicker", Color::Reset);
    }

    #[test]
    fn narrow_width_truncates_task_detail_without_panicking() {
        let detail = row(
            "detail::%1::task::0::in_progress",
            SidebarRowKind::Detail,
            1,
            "\u{2514} ● Task - Implement an extremely long task label",
            RollupLevel::Running,
        );

        let rendered = render_rows(&[detail], &SidebarState::default(), 36);

        assert!(rendered.contains('●'), "{rendered:?}");
    }

    #[test]
    fn expanded_chat_row_right_aligns_state_and_time_with_state_color() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.expanded = true;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            elapsed_secs: Some(720),
            ..Default::default()
        });
        let theme = SidebarRenderTheme::default();

        assert_eq!(right_label(&chat).as_deref(), Some("Running 12m 00s"));
        assert_eq!(
            right_style(&chat, &theme).fg,
            Some(theme.badge_color(BadgeState::Working))
        );
        let lines = render_lines(&[chat], &SidebarState::default(), 40, &theme);
        let chat_spans = &lines[0].spans;

        assert!(
            chat_spans
                .iter()
                .any(|span| span.content.as_ref() == "Codex"
                    && span.style.add_modifier.contains(Modifier::BOLD)
                    && span.style.fg == Some(Color::Reset)),
            "{chat_spans:?}"
        );
        assert!(
            !chat_spans
                .iter()
                .any(|span| span.content.as_ref() == "Running"),
            "{chat_spans:?}"
        );
        assert_span_fg(
            chat_spans,
            "Running 12m 00s",
            theme.badge_color(BadgeState::Working),
        );
        assert!(line_to_string(lines[0].clone()).ends_with("Running 12m 00s "));
    }

    #[test]
    fn sidebar_state_labels_start_with_uppercase_letters() {
        let cases = [
            (BadgeState::Blocked, RollupLevel::Error, "Error"),
            (BadgeState::Blocked, RollupLevel::Permission, "Waiting"),
            (BadgeState::Blocked, RollupLevel::Background, "Blocked"),
            (BadgeState::Working, RollupLevel::Running, "Running"),
            (BadgeState::Done, RollupLevel::Idle, "Done"),
            (BadgeState::Idle, RollupLevel::Idle, "Idle"),
        ];

        for (badge, rollup, expected) in cases {
            let mut chat = row("chat::%1", SidebarRowKind::Chat, 0, "codex", rollup);
            chat.badge_state = Some(badge);
            assert_eq!(expanded_chat_state_label(&chat).as_deref(), Some(expected));
        }
    }

    #[test]
    fn expanded_chat_row_keeps_wait_reason_context_muted() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Permission,
        );
        chat.badge_state = Some(BadgeState::Blocked);
        chat.expanded = true;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            wait_reason: Some("permission_prompt".to_string()),
            elapsed_secs: Some(120),
            ..Default::default()
        });
        let theme = SidebarRenderTheme::default();

        assert_eq!(
            right_label(&chat).as_deref(),
            Some("Waiting (permission_prompt) 2m 00s")
        );
        let lines = render_lines(&[chat], &SidebarState::default(), 60, &theme);
        let chat_spans = &lines[0].spans;

        assert_span_fg(
            chat_spans,
            "Waiting (permission_prompt) 2m 00s",
            theme.badge_color(BadgeState::Blocked),
        );
        assert!(line_to_string(lines[0].clone()).ends_with("Waiting (permission_prompt) 2m 00s "));
    }

    #[test]
    fn expanded_idle_chat_row_right_aligns_completed_age() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Idle,
        );
        chat.badge_state = Some(BadgeState::Idle);
        chat.expanded = true;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            completed_age_secs: Some(815),
            ..Default::default()
        });
        let theme = SidebarRenderTheme::default();

        assert_eq!(right_label(&chat).as_deref(), Some("Idle 13m ago"));
        assert_eq!(
            right_style(&chat, &theme).fg,
            Some(theme.badge_color(BadgeState::Idle))
        );

        let rendered = render_rows(&[chat], &SidebarState::default(), 32);
        assert!(rendered.ends_with("Idle 13m ago "), "{rendered:?}");
    }

    #[test]
    fn expanded_done_chat_row_right_aligns_done_age_with_done_color() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Idle,
        );
        chat.badge_state = Some(BadgeState::Done);
        chat.expanded = true;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            completed_age_secs: Some(815),
            ..Default::default()
        });
        let theme = SidebarRenderTheme::default();

        assert_eq!(right_label(&chat).as_deref(), Some("Done 13m ago"));
        assert_eq!(
            right_style(&chat, &theme).fg,
            Some(theme.badge_color(BadgeState::Done))
        );

        let lines = render_lines(&[chat], &SidebarState::default(), 32, &theme);
        assert_span_fg(
            &lines[0].spans,
            "Done 13m ago",
            theme.badge_color(BadgeState::Done),
        );
    }

    #[test]
    fn repo_branch_is_rendered_in_branch_color() {
        let mut repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Running,
        );
        repo.git = Some(crate::git::GitBadge {
            branch: "main".to_string(),
            ahead: 0,
            behind: 0,
        });
        let lines = render_lines(
            &[repo],
            &SidebarState::default(),
            40,
            &SidebarRenderTheme::default(),
        );
        let spans = &lines[0].spans;
        assert!(
            spans.iter().any(|span| span.content.as_ref() == "app"
                && span.style.fg == Some(Color::LightCyan)
                && span.style.add_modifier.contains(Modifier::BOLD)),
            "{spans:?}"
        );
        assert!(
            spans.iter().any(|span| span.content.trim() == "main"
                && span.style.fg == Some(Color::Indexed(73))
                && !span.style.add_modifier.contains(Modifier::BOLD)),
            "{spans:?}"
        );
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

        assert_eq!(rendered, "✓1\n──\n ✓");
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
        assert_eq!(theme.badge_color(BadgeState::Idle), Color::Indexed(248));
    }

    #[test]
    fn sidebar_badge_colors_use_shared_badge_colors() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
            r##"
badge:
  colors:
    working: "#57d98a"
    done: "#5aa6ff"
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_app_config(&config);
        assert_eq!(
            theme.badge_color(BadgeState::Working),
            Color::Rgb(0x57, 0xd9, 0x8a)
        );
        assert_eq!(
            theme.badge_color(BadgeState::Done),
            Color::Rgb(0x5a, 0xa6, 0xff)
        );
        assert_eq!(
            theme.badge_color(BadgeState::Blocked),
            Color::Rgb(0xff, 0x6b, 0x6b)
        );
        assert_eq!(
            theme.badge_color(BadgeState::Idle),
            Color::Rgb(0xa8, 0xa8, 0xb2)
        );
    }

    #[test]
    fn sidebar_colors_badge_overrides_take_precedence_over_badge_colors() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
            r##"
badge:
  colors:
    working: "#57d98a"
    idle: "#c6c3d8"
sidebar:
  colors:
    badge_idle: "#8b88a0"
    badge_done: "#4d7fc4"
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_app_config(&config);
        assert_eq!(
            theme.badge_color(BadgeState::Idle),
            Color::Rgb(0x8b, 0x88, 0xa0)
        );
        assert_eq!(
            theme.badge_color(BadgeState::Done),
            Color::Rgb(0x4d, 0x7f, 0xc4)
        );
        assert_eq!(
            theme.badge_color(BadgeState::Working),
            Color::Rgb(0x57, 0xd9, 0x8a)
        );
        assert_eq!(
            theme.badge_color(BadgeState::Blocked),
            Color::Rgb(0xff, 0x6b, 0x6b)
        );
    }

    #[test]
    fn sidebar_colors_badge_overrides_apply_without_app_config() {
        let config = serde_yaml_ng::from_str::<crate::config::SidebarColorsConfig>(
            r##"
badge_working: "#3fae7a"
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_config(&config);
        assert_eq!(
            theme.badge_color(BadgeState::Working),
            Color::Rgb(0x3f, 0xae, 0x7a)
        );
        assert_eq!(theme.badge_color(BadgeState::Idle), Color::Indexed(248));
    }

    #[test]
    fn sidebar_rollup_colors_use_shared_badge_colors() {
        let config = serde_yaml_ng::from_str::<crate::config::Config>(
            r##"
badge:
  colors:
    blocked: "#ff1111"
    working: "#22ff22"
    idle: "#999999"
"##,
        )
        .unwrap();
        let theme = SidebarRenderTheme::from_app_config(&config);

        assert_eq!(
            theme.rollup_color(RollupLevel::Running),
            Color::Rgb(0x22, 0xff, 0x22)
        );
        assert_eq!(
            theme.rollup_color(RollupLevel::Permission),
            Color::Rgb(0xff, 0x11, 0x11)
        );
        assert_eq!(
            theme.rollup_color(RollupLevel::Waiting),
            Color::Rgb(0xff, 0x11, 0x11)
        );
        assert_eq!(
            theme.rollup_color(RollupLevel::Error),
            Color::Rgb(0xff, 0x11, 0x11)
        );
        assert_eq!(
            theme.rollup_color(RollupLevel::Background),
            Color::Rgb(0x99, 0x99, 0x99)
        );
        assert_eq!(
            theme.rollup_color(RollupLevel::Idle),
            Color::Rgb(0x99, 0x99, 0x99)
        );
    }

    #[test]
    fn selected_rows_have_a_selection_bar_marker_and_horizontal_padding() {
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
        let rendered = render_rows(&rows, &state, 40);
        assert!(rendered.starts_with("▎▾ app"), "{rendered:?}");
        assert_eq!(display_width(&rendered), 40, "{rendered:?}");
    }

    #[test]
    fn selected_chat_has_a_selection_bar_marker_in_every_width_tier() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        let state = SidebarState {
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };

        for width in [2, 8, 30, 40] {
            let rendered = render_rows(std::slice::from_ref(&chat), &state, width);
            assert!(rendered.contains('▎'), "{width}: {rendered:?}");
            let lines = render_lines(
                std::slice::from_ref(&chat),
                &state,
                width,
                &SidebarRenderTheme::default(),
            );
            assert!(
                lines.iter().flat_map(|line| &line.spans).any(|span| {
                    span.content == "▎"
                        && span.style.fg == Some(SidebarRenderTheme::default().selection_bar)
                }),
                "{width}: {lines:?}"
            );
        }
    }

    #[test]
    fn boundary_width_ascii_cjk_emoji_golden() {
        let state = SidebarState {
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        for label in ["Codex: fix sidebar", "Codex: 修正確認", "Codex: fix 🧭✨"] {
            let mut chat = row(
                "chat::%1",
                SidebarRowKind::Chat,
                0,
                label,
                RollupLevel::Running,
            );
            chat.expanded = false;
            chat.badge_state = Some(BadgeState::Working);
            chat.pane_id = Some("%1".to_string());
            chat.meta = Some(crate::sidebar::tree::RowMeta {
                agent: Some("codex".to_string()),
                prompt: label
                    .split_once(':')
                    .map(|(_, prompt)| prompt.trim().to_string()),
                elapsed_secs: Some(90),
                ..Default::default()
            });
            for width in [16, 24, 35, 36] {
                let lines = render_lines(
                    std::slice::from_ref(&chat),
                    &state,
                    width,
                    &SidebarRenderTheme::default(),
                );
                let rendered = lines.into_iter().map(line_to_string).collect::<Vec<_>>();
                assert!(
                    rendered.iter().all(|line| display_width(line) <= width),
                    "{label:?} width={width}: {rendered:?}"
                );
                assert!(rendered.iter().any(|line| line.contains('▎')));
                let expected = match (label, width) {
                    ("Codex: fix sidebar", 16) => vec!["▎● 1m30s        "],
                    ("Codex: fix sidebar", 24) => vec!["▎● Codex       f… 1m30s "],
                    ("Codex: fix sidebar", 35) => {
                        vec!["▎● Codex       fix sidebar   1m30s "]
                    }
                    ("Codex: fix sidebar", 36) => vec![
                        "▎ ▸ ● Codex          Running 1m 30s ",
                        "     fix sidebar                    ",
                    ],
                    ("Codex: 修正確認", 16) => vec!["▎● 1m30s        "],
                    ("Codex: 修正確認", 24) => vec!["▎● Codex       …  1m30s "],
                    ("Codex: 修正確認", 35) => {
                        vec!["▎● Codex       修正確認      1m30s "]
                    }
                    ("Codex: 修正確認", 36) => vec![
                        "▎ ▸ ● Codex          Running 1m 30s ",
                        "     修正確認                       ",
                    ],
                    ("Codex: fix 🧭✨", 16) => vec!["▎● 1m30s        "],
                    ("Codex: fix 🧭✨", 24) => vec!["▎● Codex       f… 1m30s "],
                    ("Codex: fix 🧭✨", 35) => {
                        vec!["▎● Codex       fix 🧭✨      1m30s "]
                    }
                    ("Codex: fix 🧭✨", 36) => vec![
                        "▎ ▸ ● Codex          Running 1m 30s ",
                        "     fix 🧭✨                       ",
                    ],
                    _ => unreachable!(),
                };
                assert_eq!(rendered, expected, "{label:?} width={width}");
            }
        }
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
        let rendered = render_rows(&[repo], &SidebarState::default(), 40);
        assert!(rendered.ends_with("▲2 "), "{rendered:?}");
        assert!(!rendered.contains("[permission:"), "{rendered:?}");
    }

    #[test]
    fn closed_chat_standard_renders_two_line_digest_with_signals() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: review sidebar state shape",
            RollupLevel::Permission,
        );
        chat.badge_state = Some(BadgeState::Blocked);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            prompt: Some("review sidebar state shape".to_string()),
            wait_reason: Some("permission_prompt".to_string()),
            elapsed_secs: Some(127),
            tasks_done: Some(2),
            tasks_total: Some(5),
            subagent_count: Some(2),
            ..Default::default()
        });
        let theme = SidebarRenderTheme::default();
        let rendered = render_lines_with_indices(&[chat], &SidebarState::default(), 64, &theme);
        let text = rendered
            .lines
            .iter()
            .cloned()
            .map(line_to_string)
            .collect::<Vec<_>>();

        assert_eq!(rendered.row_indices, vec![Some(0), Some(0)]);
        assert_eq!(text.len(), 2);
        assert!(text[0].contains("▸ ▲ Codex"), "{text:?}");
        assert!(
            text[0].ends_with("Waiting (permission_prompt) 2m 07s · ☑ 2/5 · ↳ 2 "),
            "{text:?}"
        );
        assert!(
            text[1].starts_with("     review sidebar state shape"),
            "{text:?}"
        );
        assert!(text[1].ends_with("↩ permission "), "{text:?}");
        assert_span_fg(&rendered.lines[0].spans, "☑ 2/5", theme.task_working);
        assert_span_fg(&rendered.lines[0].spans, "↳ 2", theme.subagent_label);
        assert_span_fg(
            &rendered.lines[0].spans,
            "Waiting (permission_prompt) 2m 07s",
            theme.badge_color(BadgeState::Blocked),
        );
        assert!(
            text.iter().all(|line| display_width(line) == 64),
            "{text:?}"
        );
    }

    #[test]
    fn closed_and_expanded_prompt_content_start_at_the_same_column() {
        let prompt = "align this prompt";
        for depth in [0, 2] {
            for (active, selected) in [(false, false), (true, false), (false, true)] {
                let mut closed = row(
                    "chat::%1::10",
                    SidebarRowKind::Chat,
                    depth,
                    "codex: align this prompt",
                    RollupLevel::Running,
                );
                closed.badge_state = Some(BadgeState::Working);
                closed.expanded = false;
                closed.active = active;
                closed.meta = Some(crate::sidebar::tree::RowMeta {
                    agent: Some("codex".to_string()),
                    prompt: Some(prompt.to_string()),
                    ..Default::default()
                });
                let mut expanded = row(
                    "detail::%1::10::prompt",
                    SidebarRowKind::Detail,
                    depth + 1,
                    prompt,
                    RollupLevel::Running,
                );
                expanded.active = active;
                let state = SidebarState {
                    selection: selected.then(|| "chat::%1::10".to_string()),
                    ..SidebarState::default()
                };

                let closed_line = line_to_string(
                    render_lines(&[closed], &state, 64, &SidebarRenderTheme::default())[1].clone(),
                );
                let expanded_line = line_to_string(
                    render_lines(&[expanded], &state, 64, &SidebarRenderTheme::default())[0]
                        .clone(),
                );

                let prompt_column = |line: &str| {
                    let byte_index = line.find(prompt).expect("prompt must be rendered");
                    display_width(&line[..byte_index])
                };
                assert_eq!(
                    prompt_column(&closed_line),
                    prompt_column(&expanded_line),
                    "depth={depth} active={active} selected={selected}"
                );
            }
        }
    }

    #[test]
    fn closed_chat_task_progress_with_zero_done_uses_working_color() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: implement sidebar task colors",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            prompt: Some("implement sidebar task colors".to_string()),
            elapsed_secs: Some(42),
            tasks_done: Some(0),
            tasks_total: Some(3),
            ..Default::default()
        });
        let theme = SidebarRenderTheme::default();

        let lines = render_lines(&[chat], &SidebarState::default(), 64, &theme);

        assert_span_fg(&lines[0].spans, "☑ 0/3", theme.task_working);
    }

    #[test]
    fn closed_chat_selection_styles_both_digest_lines() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: review PR",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            prompt: Some("review PR".to_string()),
            elapsed_secs: Some(522),
            ..Default::default()
        });
        let state = SidebarState {
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };

        let lines = render_lines(&[chat], &state, 40, &SidebarRenderTheme::default());

        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|line| { line.style.bg == Some(SidebarRenderTheme::default().selection_bg) })
        );
        assert_eq!(line_to_string(lines[0].clone()).chars().next(), Some('▎'));
        assert_eq!(line_to_string(lines[1].clone()).chars().next(), Some(' '));
        assert!(line_to_string(lines[0].clone()).ends_with("Running 8m 42s "));
        assert_span_fg(
            &lines[0].spans,
            "Running 8m 42s",
            SidebarRenderTheme::default().badge_color(BadgeState::Working),
        );
    }

    #[test]
    fn closed_chat_completed_state_matches_expanded_state_appearance() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: review PR",
            RollupLevel::Idle,
        );
        chat.badge_state = Some(BadgeState::Done);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            prompt: Some("review PR".to_string()),
            completed_age_secs: Some(815),
            ..Default::default()
        });
        let theme = SidebarRenderTheme::default();

        let lines = render_lines(&[chat], &SidebarState::default(), 40, &theme);

        assert_span_fg(
            &lines[0].spans,
            "Done 13m ago",
            theme.badge_color(BadgeState::Done),
        );
    }

    #[test]
    fn standard_boundary_switches_closed_chat_from_dense_to_digest() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: review PR",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            prompt: Some("review PR".to_string()),
            elapsed_secs: Some(720),
            ..Default::default()
        });

        assert_eq!(
            render_rows(&[chat.clone()], &SidebarState::default(), 35)
                .lines()
                .count(),
            1
        );
        assert_eq!(
            render_rows(&[chat], &SidebarState::default(), 36)
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn closed_chat_digest_truncates_long_right_tokens_to_width() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: review very long sidebar prompt",
            RollupLevel::Permission,
        );
        chat.badge_state = Some(BadgeState::Blocked);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            prompt: Some("review very long sidebar prompt".to_string()),
            wait_reason: Some("very_long_custom_wait_reason".to_string()),
            elapsed_secs: Some(8 * 60 + 42),
            tasks_done: Some(123),
            tasks_total: Some(999),
            subagent_count: Some(42),
            ..Default::default()
        });

        let rendered = render_rows(&[chat], &SidebarState::default(), 36);

        assert_eq!(rendered.lines().count(), 2, "{rendered:?}");
        assert!(
            rendered.lines().all(|line| display_width(line) == 36),
            "{rendered:?}"
        );
        assert!(
            rendered.lines().next().unwrap().contains('…'),
            "{rendered:?}"
        );
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
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            elapsed_secs: Some(815),
            ..Default::default()
        });
        let rendered = render_rows(&[chat], &SidebarState::default(), 30);
        assert!(rendered.ends_with("13m "), "{rendered:?}");
    }

    #[test]
    fn chat_row_shows_completed_age_when_done() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: fix",
            RollupLevel::Idle,
        );
        chat.badge_state = Some(BadgeState::Done);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            completed_age_secs: Some(815),
            ..Default::default()
        });

        assert_eq!(right_label(&chat).as_deref(), Some("13m ago"));
        assert_eq!(
            right_style(&chat, &SidebarRenderTheme::default()).fg,
            Some(SidebarRenderTheme::default().badge_color(BadgeState::Idle))
        );

        let rendered = render_rows(&[chat], &SidebarState::default(), 30);
        assert!(rendered.ends_with("13m ago "), "{rendered:?}");
        assert_eq!(
            display_width(rendered.lines().next().unwrap()),
            30,
            "{rendered:?}"
        );
    }

    #[test]
    fn chat_row_shows_completed_age_when_idle() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: fix",
            RollupLevel::Idle,
        );
        chat.badge_state = Some(BadgeState::Idle);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            completed_age_secs: Some(815),
            ..Default::default()
        });

        assert_eq!(right_label(&chat).as_deref(), Some("13m ago"));
        assert_eq!(
            right_style(&chat, &SidebarRenderTheme::default()).fg,
            Some(SidebarRenderTheme::default().badge_color(BadgeState::Idle))
        );

        let rendered = render_rows(&[chat], &SidebarState::default(), 30);
        assert!(rendered.ends_with("13m ago "), "{rendered:?}");
    }

    #[test]
    fn expanded_chat_row_uses_full_elapsed_right_label() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: fix",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            elapsed_secs: Some(780),
            ..Default::default()
        });

        assert_eq!(right_label(&chat).as_deref(), Some("13m"));

        chat.expanded = true;

        assert_eq!(right_label(&chat).as_deref(), Some("Running 13m 00s"));
        assert_eq!(
            right_style(&chat, &SidebarRenderTheme::default()).fg,
            Some(SidebarRenderTheme::default().badge_color(BadgeState::Working))
        );
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
        chat.expanded = false;
        let rendered = render_rows(&[chat], &SidebarState::default(), 24);
        assert!(rendered.contains('…'), "{rendered:?}");
        assert!(rendered.ends_with("Perm "), "{rendered:?}");
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
            40,
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

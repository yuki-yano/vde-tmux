use crate::daemon::session_badge::{BadgeState, glyph_for_state};
use crate::hook::RollupLevel;
use crate::sidebar::state::{SidebarState, StatusFilter, ViewMode};
use crate::sidebar::tree::{SidebarRow, SidebarRowKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// サイドバーの配色。色は 5 族の規約で運用する:
/// - 状態族: badge_* / rollup 色(▲赤 ●緑 ✓シアン ○灰)。状態を示す場所にだけ使う
/// - 構造族: repo(青太字)/ category(ピーチ太字)/ branch(淡シアン 73)
/// - 操作族: ラベンダー 147/103(pin ✦ / mode ≣ / active ▎ / preview ⌕)
/// - 本文族: 本文=通常色 / 補足=detail(246)/ 記号=marker(暗灰)
/// - 実況: live(マゼンタ)は LIVE/EVENTS 見出し専用の孤立色
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
    /// Detail / meta 行の前景色(DIM は使わず読める中間グレーにする)
    pub detail: Color,
    /// 展開マーカー ▾/▸ と pin 印の色
    pub marker: Color,
    /// pin 中の chat / meta 印の色
    pub pin: Color,
    /// category 見出しの色
    pub category: Color,
    /// ヘッダー mode セグメントの色
    pub header_mode: Color,
    /// active chat 行の薄背景色
    pub active_bg: Color,
    /// active 系譜の左端バー色
    pub active_bar: Color,
    /// repo 名(および category 見出し)の色
    pub repo: Color,
    /// git branch 名の色
    pub branch: Color,
    /// LIVE / EVENTS 見出しラベルの色
    pub live: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpRowAction {
    Jump,
    Preview,
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
            detail: Color::Indexed(246),
            marker: Color::DarkGray,
            pin: Color::Indexed(147),
            category: Color::Indexed(215),
            header_mode: Color::Indexed(147),
            active_bg: Color::Indexed(235),
            active_bar: Color::Indexed(147),
            repo: Color::Blue,
            branch: Color::Indexed(73),
            live: Color::Magenta,
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
            detail: parse_color(config.detail.as_deref()).unwrap_or(default.detail),
            marker: parse_color(config.marker.as_deref()).unwrap_or(default.marker),
            pin: parse_color(config.pin.as_deref()).unwrap_or(default.pin),
            category: parse_color(config.category.as_deref()).unwrap_or(default.category),
            header_mode: parse_color(config.header_mode.as_deref()).unwrap_or(default.header_mode),
            active_bg: parse_color(config.active_bg.as_deref()).unwrap_or(default.active_bg),
            active_bar: parse_color(config.active_bar.as_deref()).unwrap_or(default.active_bar),
            repo: parse_color(config.repo.as_deref()).unwrap_or(default.repo),
            branch: parse_color(config.branch.as_deref()).unwrap_or(default.branch),
            live: parse_color(config.live.as_deref()).unwrap_or(default.live),
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
    ToggleFilter,
    SetFilter(StatusFilter),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BadgeCounts {
    pub total: usize,
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
}

impl BadgeCounts {
    pub fn from_rows(rows: &[SidebarRow]) -> Self {
        let mut counts = Self::default();
        for row in rows.iter().filter(|row| row.kind == SidebarRowKind::Chat) {
            counts.total += 1;
            match row.badge_state {
                Some(BadgeState::Blocked) => counts.blocked += 1,
                Some(BadgeState::Working) => counts.working += 1,
                Some(BadgeState::Done) => counts.done += 1,
                Some(BadgeState::Idle) | None => counts.idle += 1,
            }
        }
        counts
    }
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
            0..=2 => Self::Rail,
            3..=23 => Self::Micro,
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
    pub action: HeaderAction,
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
    let mode_badge = format_header_segment(
        &format!("≣ {}", view_mode_label_padded(state.view_mode)),
        theme,
    );
    let separator = if theme.header_separator.is_empty() {
        " · ".to_string()
    } else {
        theme.header_separator.clone()
    };
    let filter_items = [
        (
            format_header_segment(&format!("≡ {}", counts.total), theme),
            HeaderAction::SetFilter(StatusFilter::All),
            StatusFilter::All,
            None,
            counts.total,
        ),
        (
            format_header_segment(&format!("▲ {}", counts.blocked), theme),
            HeaderAction::SetFilter(StatusFilter::AttentionOnly),
            StatusFilter::AttentionOnly,
            Some(BadgeState::Blocked),
            counts.blocked,
        ),
        (
            format_header_segment(&format!("● {}", counts.working), theme),
            HeaderAction::SetFilter(StatusFilter::WorkingOnly),
            StatusFilter::WorkingOnly,
            Some(BadgeState::Working),
            counts.working,
        ),
        (
            format_header_segment(&format!("✓ {}", counts.done), theme),
            HeaderAction::SetFilter(StatusFilter::DoneOnly),
            StatusFilter::DoneOnly,
            Some(BadgeState::Done),
            counts.done,
        ),
        (
            format_header_segment(&format!("○ {}", counts.idle), theme),
            HeaderAction::SetFilter(StatusFilter::IdleOnly),
            StatusFilter::IdleOnly,
            Some(BadgeState::Idle),
            counts.idle,
        ),
    ];
    let filter_badges = filter_items
        .iter()
        .map(|(label, _, _, _, _)| label.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let full_text = format!(" {mode_badge}{separator}{filter_badges}");
    let text = truncate_display(&full_text, width as usize);
    let mut segments = Vec::new();
    let mode_len = display_width(&mode_badge);
    let separator_len = display_width(&separator);
    if let Some(range) = visible_segment_range(&text, 1, mode_len) {
        segments.push(HeaderSegment {
            range,
            action: HeaderAction::CycleViewMode,
            style: Some(mode_segment_style(theme)),
        });
    }
    let mut start = 1 + mode_len + separator_len;
    for (label, action, filter, badge_state, count) in filter_items {
        let label_len = display_width(&label);
        if let Some(range) = visible_segment_range(&text, start, label_len) {
            let style = if state.filter == filter {
                active_filter_style(theme, badge_state)
            } else if count == 0 {
                // 0件は控えめに(状態色で塗らない)
                Style::default().fg(theme.marker)
            } else if let Some(badge_state) = badge_state {
                Style::default().fg(theme.badge_color(badge_state))
            } else {
                Style::default()
            };
            segments.push(HeaderSegment {
                range,
                action,
                style: Some(style),
            });
        }
        start += label_len + 1;
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
                    spans.push(Span::raw(slice_display(
                        &line.text,
                        cursor,
                        segment.range.start,
                    )));
                }
                spans.push(Span::styled(
                    slice_display(&line.text, segment.range.start, segment.range.end),
                    segment.style.unwrap_or_else(|| header_segment_style(theme)),
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
    let text = truncate_display(" j/k move  enter jump  tab filter", width);
    Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// ヘッダーの mode(view 切替)セグメントの色。
/// `sidebar.header` で明示スタイルが設定されていればそれを優先し、
/// 無指定なら header_mode 色 + BOLD。
fn mode_segment_style(theme: &SidebarRenderTheme) -> Style {
    if header_style_configured(theme) {
        header_segment_style(theme)
    } else {
        Style::default()
            .fg(theme.header_mode)
            .add_modifier(Modifier::BOLD)
    }
}

/// 現在アクティブなフィルタセグメントの強調。
/// 明示スタイルがあればそれを優先し、無指定なら
/// 「状態色 + selection_bg + BOLD」でアクティブを示す。
fn active_filter_style(theme: &SidebarRenderTheme, badge_state: Option<BadgeState>) -> Style {
    if header_style_configured(theme) {
        return header_segment_style(theme);
    }
    let mut style = Style::default()
        .bg(theme.selection_bg)
        .add_modifier(Modifier::BOLD);
    if let Some(badge_state) = badge_state {
        style = style.fg(theme.badge_color(badge_state));
    }
    style
}

fn header_style_configured(theme: &SidebarRenderTheme) -> bool {
    theme.header_active_fg.is_some() || theme.header_active_bg.is_some() || theme.header_active_bold
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
    render_lines_with_indices(rows, state, width, theme).lines
}

pub fn render_lines_with_indices(
    rows: &[SidebarRow],
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> RenderedLines {
    match WidthTier::from_width(width) {
        WidthTier::Rail => render_rail_lines(rows, state, theme),
        WidthTier::Micro => render_micro_lines(rows, state, width, theme),
        WidthTier::Dense => render_dense_lines(rows, state, width, theme),
        WidthTier::Standard => RenderedLines {
            lines: rows
                .iter()
                .map(|row| render_row_line(row, state, width, theme))
                .collect(),
            row_indices: (0..rows.len()).map(Some).collect(),
        },
    }
}

fn render_row_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = state.selection.as_deref() == Some(row.id.as_str());
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
            let pin = if row
                .meta
                .as_ref()
                .and_then(|meta| meta.pinned)
                .unwrap_or(false)
            {
                "✦"
            } else {
                " "
            };
            format!("{indent}{pin}{marker} ")
        }
        SidebarRowKind::Detail if row.id.starts_with("meta::") => format!("{indent}✦ "),
        SidebarRowKind::Detail => indent.clone(),
        SidebarRowKind::Jump => indent.clone(),
        SidebarRowKind::Zone => unreachable!("zone rows return before generic rendering"),
    };
    let is_state_detail = row.kind == SidebarRowKind::Detail && row.id.ends_with("::state");
    let badge = if row.kind == SidebarRowKind::Chat || is_state_detail {
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
        SidebarRowKind::Category => format!("◆ {}", row.label),
        SidebarRowKind::Jump => JUMP_ROW_LABEL.to_string(),
        _ => row.label.clone(),
    };
    let label = truncate_display(&label_source, label_budget);

    let mut spans = Vec::new();
    if row.kind == SidebarRowKind::Chat {
        let marker = if row.expanded { "▾" } else { "▸" };
        let pinned = row
            .meta
            .as_ref()
            .and_then(|meta| meta.pinned)
            .unwrap_or(false);
        push_leading_marker_span(&mut spans, row, theme, &indent);
        spans.push(Span::styled(
            if pinned { "✦" } else { " " }.to_string(),
            Style::default().fg(theme.pin),
        ));
        spans.push(Span::styled(
            format!("{marker} "),
            Style::default().fg(theme.marker),
        ));
    } else if row.kind == SidebarRowKind::Detail && row.id.starts_with("meta::") {
        push_leading_marker_span(&mut spans, row, theme, &indent);
        spans.push(Span::styled(
            "✦ ".to_string(),
            Style::default().fg(theme.pin),
        ));
    } else {
        push_leading_marker_span(&mut spans, row, theme, &head);
    }
    if let Some((glyph, color)) = badge {
        spans.push(Span::styled(glyph, badge_style(color, row)));
    }
    if row.kind == SidebarRowKind::Jump {
        spans.extend(jump_action_spans(&label, theme));
    } else if is_state_detail {
        spans.extend(state_detail_label_spans(label, row, theme));
    } else {
        spans.extend(label_spans(label, row, style));
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
    } else if row.active && row.kind == SidebarRowKind::Chat {
        line = line.style(Style::default().bg(theme.active_bg));
    }
    line
}

fn push_leading_marker_span(
    spans: &mut Vec<Span<'static>>,
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
    tail: &str,
) {
    if row.active {
        spans.push(Span::styled(
            "▎".to_string(),
            Style::default().fg(theme.active_bar),
        ));
        if !tail.is_empty() {
            spans.push(Span::styled(
                tail.to_string(),
                Style::default().fg(theme.marker),
            ));
        }
    } else {
        spans.push(Span::styled(
            format!(" {tail}"),
            Style::default().fg(theme.marker),
        ));
    }
}

/// Chat 行のラベルを「agent 名(太字)+ 残り(通常)」に分ける。
/// それ以外の行、および truncate で agent 名が欠けた場合は単一 span。
fn label_spans(label: String, row: &SidebarRow, base: Style) -> Vec<Span<'static>> {
    if row.kind == SidebarRowKind::Chat
        && let Some(agent) = row
            .meta
            .as_ref()
            .and_then(|meta| meta.agent.as_deref())
            .filter(|agent| !agent.is_empty())
        && label.starts_with(agent)
    {
        let (agent_part, rest) = label.split_at(agent.len());
        return vec![
            Span::styled(agent_part.to_string(), base.add_modifier(Modifier::BOLD)),
            Span::styled(rest.to_string(), base),
        ];
    }
    vec![Span::styled(label, base)]
}

fn state_detail_label_spans(
    label: String,
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
) -> Vec<Span<'static>> {
    let color = row
        .badge_state
        .map(|state| theme.badge_color(state))
        .unwrap_or(theme.detail);
    let (word, rest) = match label.split_once(' ') {
        Some((word, rest)) => (word.to_string(), format!(" {rest}")),
        None => (label, String::new()),
    };
    let mut spans = vec![Span::styled(word, Style::default().fg(color))];
    if !rest.is_empty() {
        spans.push(Span::styled(rest, Style::default().fg(theme.detail)));
    }
    spans
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
    let marker = if row.expanded { "▾" } else { "▸" };
    let text = truncate_display(&format!(" {marker} {}", row.label), width);
    let mut style = row_style(row, theme);
    if state.selection.as_deref() == Some(row.id.as_str()) {
        style = style.bg(theme.selection_bg).add_modifier(Modifier::BOLD);
    }
    active_bar_line(row, theme, pad_to_width(text, width), style)
}

fn render_chat_dense_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let badge_state = row.badge_state.unwrap_or(BadgeState::Idle);
    let glyph = theme.badge_glyph(badge_state);
    let agent = row
        .meta
        .as_ref()
        .and_then(|meta| meta.agent.as_deref())
        .unwrap_or_else(|| row.label.split(':').next().unwrap_or(row.label.as_str()));
    let agent = truncate_display(agent, 7);
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
        Span::styled(
            if row.active { "▎" } else { " " }.to_string(),
            if row.active {
                Style::default().fg(theme.active_bar)
            } else {
                style
            },
        ),
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
    if state.selection.as_deref() == Some(row.id.as_str()) {
        line = line.style(
            Style::default()
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        );
    }
    line
}

fn active_bar_line(
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
    text: String,
    style: Style,
) -> Line<'static> {
    if !row.active {
        return Line::from(Span::styled(text, style));
    }
    let rest = text.chars().skip(1).collect::<String>();
    Line::from(vec![
        Span::styled("▎".to_string(), Style::default().fg(theme.active_bar)),
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
        let text = if right.is_empty() {
            format!(" {glyph}")
        } else {
            format!(" {glyph} {right}")
        };
        let mut line = Line::from(Span::styled(
            pad_to_width(truncate_display(&text, width), width),
            badge_style(theme.badge_color(badge_state), row),
        ));
        if state.selection.as_deref() == Some(row.id.as_str()) {
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
            lines.push(Line::from(Span::styled(
                format!("{}{}", theme.badge_glyph(state), count),
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
        if state.selection.as_deref() == Some(row.id.as_str()) {
            style = style.bg(theme.selection_bg).add_modifier(Modifier::BOLD);
        }
        let glyph = row.badge_state.expect("rail rows must carry badge_state");
        lines.push(Line::from(Span::styled(
            theme.badge_glyph(glyph).to_string(),
            style,
        )));
        row_indices.push(Some(index));
    }
    RenderedLines { lines, row_indices }
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
                return None;
            }
            match row.rollup {
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
            }
        }
        SidebarRowKind::Detail | SidebarRowKind::Jump | SidebarRowKind::Zone => None,
    }
}

fn elapsed_label(secs: i64) -> String {
    crate::sidebar::tree::humanize_secs(secs)
}

fn right_style(row: &SidebarRow, theme: &SidebarRenderTheme) -> Style {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            Style::default().fg(theme.badge_color(BadgeState::Blocked))
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
    // 状態(rollup)の色はバッジグリフと右カラムだけに載せ、
    // 本文テキストは通常色に保つ(理想形の多トーン構成)。
    match row.kind {
        SidebarRowKind::Zone => Style::default().fg(Color::Reset),
        SidebarRowKind::Category => Style::default()
            .fg(theme.category)
            .add_modifier(Modifier::BOLD),
        SidebarRowKind::Repo => Style::default().fg(theme.repo).add_modifier(Modifier::BOLD),
        SidebarRowKind::Chat => Style::default().fg(Color::Reset),
        SidebarRowKind::Detail => Style::default().fg(theme.detail),
        SidebarRowKind::Jump => Style::default().fg(theme.detail),
    }
}

/// Jump 行のアクションチップの全文(幅計算と truncate 判定に使う)。
const JUMP_ROW_LABEL: &str = "[↗ jump] [⌕ preview]";
/// jump グリフの淡シアン(branch の明るい Cyan と彩度で差別化する)
const ACTION_JUMP_GLYPH: Color = Color::Indexed(73);
/// preview グリフは jump と同じ操作アクセント色に揃える。
const ACTION_PREVIEW_GLYPH: Color = ACTION_JUMP_GLYPH;

/// Jump 行の [↗ jump] [⌕ preview] チップ。ブラケットは marker、
/// ラベルは detail で detail ゾーンに馴染ませ、グリフ1文字だけ
/// 淡色アクセントにして「押せる」ことを示す。
fn jump_action_spans(label: &str, theme: &SidebarRenderTheme) -> Vec<Span<'static>> {
    if label != JUMP_ROW_LABEL {
        // 幅不足で truncate された場合も、操作グリフだけは色を保持する。
        return action_label_spans(label, theme);
    }
    let bracket = Style::default().fg(theme.marker);
    let text = Style::default().fg(theme.detail);
    vec![
        Span::styled("[".to_string(), bracket),
        Span::styled("↗".to_string(), Style::default().fg(ACTION_JUMP_GLYPH)),
        Span::styled(" jump".to_string(), text),
        Span::styled("]".to_string(), bracket),
        Span::raw(" ".to_string()),
        Span::styled("[".to_string(), bracket),
        Span::styled("⌕".to_string(), Style::default().fg(ACTION_PREVIEW_GLYPH)),
        Span::styled(" preview".to_string(), text),
        Span::styled("]".to_string(), bracket),
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
                    Style::default().fg(ACTION_JUMP_GLYPH),
                ));
            }
            '⌕' => {
                flush_text(&mut spans, &mut text);
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().fg(ACTION_PREVIEW_GLYPH),
                ));
            }
            '[' | ']' => {
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

/// Jump 行のクリック列をアクションへ変換する。
/// レイアウトは " " + indent(2*depth) + "[↗ jump]"(8桁) + " " + "[⌕ preview]"(11桁)。
/// クリック範囲はブラケット込み(見た目のチップと一致させる)。
pub fn jump_row_action_at(row: &SidebarRow, column: u16) -> Option<JumpRowAction> {
    if row.kind != SidebarRowKind::Jump {
        return None;
    }
    let jump_start = 1 + 2 * row.depth;
    let jump_end = jump_start + 8;
    let preview_start = jump_end + 1;
    let preview_end = preview_start + 11;
    let column = column as usize;
    if (jump_start..jump_end).contains(&column) {
        Some(JumpRowAction::Jump)
    } else if (preview_start..preview_end).contains(&column) {
        Some(JumpRowAction::Preview)
    } else {
        None
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

/// mode ラベルを最長ラベル("category")に合わせて右空白で固定幅にする。
/// mode 切替でヘッダー後続(フィルタ群)の表示位置がずれないようにするため。
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
    fn pinned_chat_row_shows_pin_glyph() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Running,
        );
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            pinned: Some(true),
            ..Default::default()
        });
        chat.badge_state = Some(crate::daemon::session_badge::BadgeState::Working);
        chat.expanded = false;
        let mut unpinned = row(
            "chat::%2",
            SidebarRowKind::Chat,
            0,
            "claude",
            RollupLevel::Running,
        );
        unpinned.badge_state = Some(crate::daemon::session_badge::BadgeState::Working);
        unpinned.expanded = false;

        let rendered = render_rows(&[chat, unpinned], &SidebarState::default(), 40);

        assert!(
            rendered.lines().next().unwrap().starts_with(" ✦▸ "),
            "{rendered:?}"
        );
        assert!(
            rendered.lines().nth(1).unwrap().starts_with("  ▸ "),
            "{rendered:?}"
        );
    }

    #[test]
    fn pin_color_is_configurable() {
        let config = crate::config::SidebarColorsConfig {
            pin: Some("magenta".to_string()),
            ..Default::default()
        };
        let theme = SidebarRenderTheme::from_config(&config);

        assert_eq!(theme.pin, Color::Magenta);
        assert_eq!(SidebarRenderTheme::default().pin, Color::Indexed(147));
    }

    #[test]
    fn branch_defaults_to_muted_cyan() {
        assert_eq!(SidebarRenderTheme::default().branch, Color::Indexed(73));
    }

    #[test]
    fn active_colors_are_configurable() {
        let config = crate::config::SidebarColorsConfig {
            active_bg: Some("235".to_string()),
            active_bar: Some("magenta".to_string()),
            ..Default::default()
        };
        let theme = SidebarRenderTheme::from_config(&config);

        assert_eq!(theme.active_bg, Color::Indexed(235));
        assert_eq!(theme.active_bar, Color::Magenta);
        assert_eq!(SidebarRenderTheme::default().active_bg, Color::Indexed(235));
        assert_eq!(
            SidebarRenderTheme::default().active_bar,
            Color::Indexed(147)
        );
    }

    #[test]
    fn width_tier_boundaries() {
        assert_eq!(WidthTier::from_width(2), WidthTier::Rail);
        assert_eq!(WidthTier::from_width(3), WidthTier::Micro);
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

        assert!(rendered.contains("● claude  vde"), "{rendered:?}");
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

        assert_eq!(rendered, " ▲ perm ");
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

        let rendered = render_rows(&[blocked1, blocked2, working], &SidebarState::default(), 2);

        assert_eq!(rendered, "▲2\n●1\n──\n▲\n▲\n●");
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

        assert_eq!(text, "●1\n──\n●");
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
        assert_eq!(rendered, "▲1\n──\n▲");
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

        // 多トーン検証は colorize_follows_ideal_multi_tone_scheme も参照
        // 先頭 span はマーカー(DarkGray)、ラベル span が category 色 + BOLD
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.trim() == "◆ misc"
                    && span.style.fg == Some(Color::Indexed(215))
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "{:?}",
            lines[0]
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
        assert_eq!(row_style(&repo, &theme).fg, Some(Color::Blue));
    }

    #[test]
    fn mode_segment_uses_header_mode_color_and_glyph() {
        let theme = SidebarRenderTheme::default();
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };

        assert_eq!(mode_segment_style(&theme).fg, Some(Color::Indexed(147)));
        assert_eq!(
            build_header_layout(&state, 80).lines[0].text,
            " ≣ repo     · ≡ 0 ▲ 0 ● 0 ✓ 0 ○ 0"
        );
    }

    #[test]
    fn header_filter_positions_are_stable_across_view_modes() {
        let text_for = |view_mode: ViewMode| {
            let state = SidebarState {
                view_mode,
                ..SidebarState::default()
            };
            build_header_layout(&state, 80).lines[0].text.clone()
        };
        let flat = text_for(ViewMode::Flat);
        let repo = text_for(ViewMode::ByRepo);
        let category = text_for(ViewMode::ByCategory);

        // mode を切り替えてもフィルタ群の開始位置と全体幅が動かない
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
    fn category_row_label_has_diamond_prefix_in_standard_tier() {
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

        assert!(standard.contains("▾ ◆ dev"), "{standard:?}");
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

        assert!(rendered.contains("▾ ◆ dev ─"), "{rendered:?}");
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
    fn active_rows_render_left_bar_and_chat_bg() {
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
        assert_eq!(lines[1].style.bg, Some(theme.active_bg));

        let selected = SidebarState {
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        let selected_lines = render_lines(&[chat], &selected, 40, &theme);
        assert_eq!(selected_lines[0].style.bg, Some(theme.selection_bg));
    }

    #[test]
    fn jump_row_renders_two_action_buttons() {
        let jump = row(
            "jump::%1",
            SidebarRowKind::Jump,
            2,
            "jump",
            RollupLevel::Running,
        );

        let rendered = render_rows(std::slice::from_ref(&jump), &SidebarState::default(), 40);

        assert!(
            rendered.starts_with("     [↗ jump] [⌕ preview]"),
            "{rendered:?}"
        );

        // グリフだけ淡色アクセント、ラベルは detail、ブラケットは marker
        let theme = SidebarRenderTheme::default();
        let lines = render_lines(&[jump], &SidebarState::default(), 40, &theme);
        let style_of = |needle: &str| {
            lines[0]
                .spans
                .iter()
                .find(|span| span.content == needle)
                .unwrap_or_else(|| panic!("span {needle:?} not found: {:?}", lines[0]))
                .style
        };
        assert_eq!(style_of("↗").fg, Some(Color::Indexed(73)));
        assert_eq!(style_of("⌕").fg, Some(Color::Indexed(73)));
        assert_eq!(style_of(" jump").fg, Some(theme.detail));
        assert_eq!(style_of(" preview").fg, Some(theme.detail));
        assert_eq!(style_of("[").fg, Some(theme.marker));
    }

    #[test]
    fn truncated_jump_row_keeps_preview_icon_color() {
        let theme = SidebarRenderTheme::default();
        let spans = jump_action_spans("[↗ jump] [⌕ pre…", &theme);
        let style_of = |needle: &str| {
            spans
                .iter()
                .find(|span| span.content == needle)
                .unwrap_or_else(|| panic!("span {needle:?} not found: {spans:?}"))
                .style
        };

        assert_eq!(style_of("↗").fg, Some(Color::Indexed(73)));
        assert_eq!(style_of("⌕").fg, Some(Color::Indexed(73)));
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

        // " " + indent(4) + "[↗ jump]"(5..13) + " "(13) + "[⌕ preview]"(14..25)
        assert_eq!(jump_row_action_at(&jump, 4), None);
        assert_eq!(jump_row_action_at(&jump, 5), Some(JumpRowAction::Jump));
        assert_eq!(jump_row_action_at(&jump, 12), Some(JumpRowAction::Jump));
        assert_eq!(jump_row_action_at(&jump, 13), None);
        assert_eq!(jump_row_action_at(&jump, 14), Some(JumpRowAction::Preview));
        assert_eq!(jump_row_action_at(&jump, 24), Some(JumpRowAction::Preview));
        assert_eq!(jump_row_action_at(&jump, 25), None);
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

    #[test]
    fn header_layout_shows_current_values_only_and_hit_tests_tokens() {
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            filter: StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };

        let header = build_header_layout(&state, 80);

        assert_eq!(header.lines[0].text, " ≣ category · ≡ 0 ▲ 0 ● 0 ✓ 0 ○ 0");
        assert_eq!(
            header_hit_test(&header, 0, 2),
            Some(HeaderAction::CycleViewMode)
        );
        assert_eq!(
            header_hit_test(&header, 0, 19),
            Some(HeaderAction::SetFilter(StatusFilter::AttentionOnly))
        );
    }

    #[test]
    fn header_shows_badge_counts_as_filter_segments() {
        let mut blocked = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex",
            RollupLevel::Permission,
        );
        blocked.badge_state = Some(BadgeState::Blocked);
        let mut working = row(
            "chat::%2",
            SidebarRowKind::Chat,
            0,
            "claude",
            RollupLevel::Running,
        );
        working.badge_state = Some(BadgeState::Working);
        let mut done = row(
            "chat::%3",
            SidebarRowKind::Chat,
            0,
            "opencode",
            RollupLevel::Idle,
        );
        done.badge_state = Some(BadgeState::Done);
        let idle = row(
            "chat::%4",
            SidebarRowKind::Chat,
            0,
            "cursor",
            RollupLevel::Idle,
        );
        let repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Running,
        );
        let counts = BadgeCounts::from_rows(&[blocked, working, done, idle, repo]);
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            filter: StatusFilter::IdleOnly,
            ..SidebarState::default()
        };

        let header =
            build_header_layout_with_counts(&state, 80, &SidebarRenderTheme::default(), counts);

        assert_eq!(header.lines[0].text, " ≣ repo     · ≡ 4 ▲ 1 ● 1 ✓ 1 ○ 1");
        assert_eq!(
            header_hit_test(&header, 0, 2),
            Some(HeaderAction::CycleViewMode)
        );
        assert_eq!(
            header_hit_test(&header, 0, 14),
            Some(HeaderAction::SetFilter(StatusFilter::All))
        );
        assert_eq!(
            header_hit_test(&header, 0, 19),
            Some(HeaderAction::SetFilter(StatusFilter::AttentionOnly))
        );
        assert_eq!(
            header_hit_test(&header, 0, 23),
            Some(HeaderAction::SetFilter(StatusFilter::WorkingOnly))
        );
        assert_eq!(
            header_hit_test(&header, 0, 27),
            Some(HeaderAction::SetFilter(StatusFilter::DoneOnly))
        );
        assert_eq!(
            header_hit_test(&header, 0, 31),
            Some(HeaderAction::SetFilter(StatusFilter::IdleOnly))
        );
        assert!(header.lines[0].segments[5].style.is_some());
    }

    #[test]
    fn header_layout_defaults_to_compact_dot_separated_segments() {
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            filter: StatusFilter::All,
            ..SidebarState::default()
        };

        let header = build_header_layout(&state, 80);

        assert_eq!(header.lines[0].text, " ≣ repo     · ≡ 0 ▲ 0 ● 0 ✓ 0 ○ 0");
        assert_eq!(header.lines[0].segments[0].range, 1..11);
        assert_eq!(header.lines[0].segments[1].range, 14..17);
        assert_eq!(
            header_hit_test(&header, 0, 3),
            Some(HeaderAction::CycleViewMode)
        );
        assert_eq!(
            header_hit_test(&header, 0, 15),
            Some(HeaderAction::SetFilter(StatusFilter::All))
        );
        assert_eq!(header_hit_test(&header, 0, 12), None);
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

        assert_eq!(
            header.lines[0].text,
            " [ ≣ repo     ] [ ≡ 0 ] [ ▲ 0 ] [ ● 0 ] [ ✓ 0 ] [ ○ 0 ]"
        );
        assert_eq!(header.lines[0].segments[0].range, 1..15);
        assert_eq!(header.lines[0].segments[1].range, 16..23);
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
    fn header_segments_use_display_cells_with_cjk_prefix() {
        let mut config = crate::config::Config::default();
        config.sidebar.header.prefix = "「".to_string();
        config.sidebar.header.suffix = "」".to_string();
        let theme = SidebarRenderTheme::from_app_config(&config);
        let state = SidebarState::default();

        let layout = build_header_layout_with_counts(&state, 60, &theme, BadgeCounts::default());
        let line = &layout.lines[0];
        let mode = &line.segments[0];
        let mode_text = format_header_segment(
            &format!("≣ {}", view_mode_label_padded(state.view_mode)),
            &theme,
        );

        assert_eq!(
            (mode.range.end - mode.range.start) as usize,
            display_width(&mode_text)
        );
        let lines = render_header_lines(&layout, &theme);
        assert!(
            lines[0].spans.iter().any(|span| span.content == mode_text),
            "{lines:?}"
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
    fn header_segments_are_colorized_by_default() {
        let theme = SidebarRenderTheme::default();
        let counts = BadgeCounts {
            total: 4,
            blocked: 0,
            working: 1,
            done: 0,
            idle: 3,
        };
        let state = SidebarState::default(); // filter = All がアクティブ
        let header = build_header_layout_with_counts(&state, 60, &theme, counts);
        let segments = &header.lines[0].segments;
        // mode: header_mode 色 + BOLD
        assert_eq!(
            segments[0].style,
            Some(
                Style::default()
                    .fg(Color::Indexed(147))
                    .add_modifier(Modifier::BOLD)
            )
        );
        // アクティブフィルタ(≡): selection_bg + BOLD で強調
        let active = segments[1].style.unwrap();
        assert_eq!(active.bg, Some(Color::Indexed(237)));
        assert!(active.add_modifier.contains(Modifier::BOLD));
        // 0件(▲0)は marker 色、非0(●1)は状態色
        assert_eq!(
            segments[2].style,
            Some(Style::default().fg(Color::DarkGray))
        );
        assert_eq!(segments[3].style, Some(Style::default().fg(Color::Green)));
    }

    #[test]
    fn colorize_follows_ideal_multi_tone_scheme() {
        // running な chat 行でも本文は通常色、状態色はバッジと右カラムのみ。
        // agent 名は太字、prompt は通常。Detail は DIM ではなく読める中間グレー。
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "claude: fix flicker",
            RollupLevel::Running,
        );
        chat.badge_state = Some(BadgeState::Working);
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("claude".to_string()),
            elapsed_secs: Some(780),
            ..Default::default()
        });
        let detail = row(
            "detail::%1::place",
            SidebarRowKind::Detail,
            1,
            "main · %1",
            RollupLevel::Running,
        );
        let lines = render_lines(
            &[chat, detail],
            &SidebarState::default(),
            40,
            &SidebarRenderTheme::default(),
        );

        let chat_spans = &lines[0].spans;
        // agent 名: 通常色 + BOLD
        assert!(
            chat_spans
                .iter()
                .any(|span| span.content.as_ref() == "claude"
                    && span.style.fg == Some(Color::Reset)
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "{chat_spans:?}"
        );
        // prompt 部分: 通常色・非 BOLD(running でも緑にしない)
        assert!(
            chat_spans
                .iter()
                .any(|span| span.content.as_ref().contains(": fix flicker")
                    && span.style.fg == Some(Color::Reset)
                    && !span.style.add_modifier.contains(Modifier::BOLD)),
            "{chat_spans:?}"
        );
        // マーカーは marker 色(DarkGray)
        assert_eq!(chat_spans[0].style.fg, Some(Color::DarkGray));
        // 右カラム(経過)は rollup 色で DIM なし
        assert!(
            chat_spans.iter().any(|span| span.content.as_ref() == "13m"
                && span.style.fg == Some(Color::Green)
                && !span.style.add_modifier.contains(Modifier::DIM)),
            "{chat_spans:?}"
        );
        // Detail 行: DIM なしの中間グレー
        let detail_spans = &lines[1].spans;
        assert!(
            detail_spans
                .iter()
                .any(|span| span.content.as_ref().contains("main · %1")
                    && span.style.fg == Some(Color::Indexed(246))
                    && !span.style.add_modifier.contains(Modifier::DIM)),
            "{detail_spans:?}"
        );
    }

    #[test]
    fn state_detail_row_colors_glyph_and_state_word() {
        let mut state_row = row(
            "detail::%1::state",
            SidebarRowKind::Detail,
            1,
            "running · 12m",
            RollupLevel::Running,
        );
        state_row.badge_state = Some(BadgeState::Working);
        let mut place_row = row(
            "detail::%1::place",
            SidebarRowKind::Detail,
            1,
            "vde-tmux · %1",
            RollupLevel::Running,
        );
        place_row.badge_state = Some(BadgeState::Working);
        let theme = SidebarRenderTheme::default();
        let lines = render_lines(
            &[state_row, place_row],
            &SidebarState::default(),
            40,
            &theme,
        );
        let state_spans = &lines[0].spans;

        assert!(
            state_spans
                .iter()
                .any(|span| span.content.as_ref() == "● "
                    && span.style.fg == Some(theme.badge_working)),
            "{state_spans:?}"
        );
        assert!(
            state_spans
                .iter()
                .any(|span| span.content.as_ref() == "running"
                    && span.style.fg == Some(theme.badge_working)),
            "{state_spans:?}"
        );
        assert!(
            state_spans.iter().any(
                |span| span.content.as_ref() == " · 12m" && span.style.fg == Some(theme.detail)
            ),
            "{state_spans:?}"
        );

        let place_spans = &lines[1].spans;
        assert!(
            place_spans
                .iter()
                .any(|span| span.content.as_ref() == "vde-tmux · %1"
                    && span.style.fg == Some(theme.detail)),
            "{place_spans:?}"
        );
        assert!(
            !place_spans.iter().any(|span| span.content.as_ref() == "● "),
            "{place_spans:?}"
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
        // repo 名は repo 色 + BOLD、branch は淡シアン(73)非 BOLD
        assert!(
            spans.iter().any(|span| span.content.as_ref() == "app"
                && span.style.fg == Some(Color::Blue)
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

        assert_eq!(rendered, "✓1\n──\n✓");
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
        let rendered = render_rows(&rows, &state, 40);
        assert!(rendered.starts_with(" ▾ app"), "{rendered:?}");
        assert!(!rendered.contains("> "), "{rendered:?}");
        assert_eq!(display_width(&rendered), 40, "{rendered:?}");
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
    fn chat_row_right_aligns_status_short_label() {
        let mut chat = row(
            "chat::%1",
            SidebarRowKind::Chat,
            0,
            "codex: review PR",
            RollupLevel::Permission,
        );
        chat.badge_state = Some(BadgeState::Blocked);
        chat.expanded = false;
        let rendered = render_rows(&[chat], &SidebarState::default(), 40);
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
        chat.expanded = false;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            elapsed_secs: Some(815),
            ..Default::default()
        });
        let rendered = render_rows(&[chat], &SidebarState::default(), 30);
        assert!(rendered.ends_with("13m "), "{rendered:?}");
    }

    #[test]
    fn expanded_chat_row_suppresses_right_label() {
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

        assert_eq!(right_label(&chat), None);
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

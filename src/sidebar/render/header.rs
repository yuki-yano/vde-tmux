use crate::daemon::session_badge::BadgeState;
use crate::sidebar::state::{CategoryScope, PresentationMode, SidebarState, StatusFilter};
use crate::sidebar::tree::BadgeCounts;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::{display_width, slice_display, truncate_display, visible_segment_range};
use super::theme::SidebarRenderTheme;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderAction {
    ToggleCategoryScope,
    CyclePresentationMode,
    SetFilter(StatusFilter),
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
    let title = build_header_title_line(state, width as usize, theme);
    let chips = build_header_chip_line(state, width as usize, theme, counts);
    HeaderLayout {
        lines: vec![section, title, chips],
    }
}

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
) -> HeaderLine {
    let mode_pieces = format_header_mode_pieces(state, theme);
    let mode_body = mode_pieces
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<String>();
    let mode_prefix = theme.header_prefix.as_str();
    let mode_text = format!("{mode_prefix}{mode_body}");
    let mode_suffix = theme.header_suffix.as_str();
    let full_text = format!("{mode_text}{mode_suffix}");
    let include_suffix = display_width(&full_text) <= width;
    let text = if include_suffix {
        full_text
    } else if display_width(&mode_text) <= width {
        mode_text
    } else {
        truncate_display(&mode_text, width)
    };

    let mut pieces = Vec::new();
    if !mode_prefix.is_empty() {
        pieces.push((
            mode_prefix.to_string(),
            Style::default().fg(mode_bg(theme)),
            None,
        ));
    }
    pieces.extend(
        mode_pieces
            .into_iter()
            .map(|(text, action)| (text, mode_segment_style(theme), action)),
    );
    if include_suffix && !mode_suffix.is_empty() {
        let mut suffix_style = Style::default().fg(mode_bg(theme));
        if let Some(outer_bg) = theme.header_outer_bg.filter(|color| *color != Color::Reset) {
            suffix_style = suffix_style.bg(outer_bg);
        }
        pieces.push((mode_suffix.to_string(), suffix_style, None));
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

fn format_header_mode_pieces(
    state: &SidebarState,
    theme: &SidebarRenderTheme,
) -> Vec<(String, Option<HeaderAction>)> {
    let scope = category_scope_label_padded(state.category_scope);
    let presentation = presentation_mode_label_padded(state.presentation_mode);
    let scope_segment = format!("◉ {scope}");
    let presentation_segment = format!("≣ {presentation}");
    let label = format!("{scope_segment} · {presentation_segment}");
    let formatted = theme
        .header_format
        .replace("{label}", &label)
        .replace("{scope}", category_scope_label(state.category_scope))
        .replace(
            "{presentation}",
            presentation_mode_label(state.presentation_mode),
        );
    let Some(scope_start) = formatted.find(&scope_segment) else {
        return vec![(formatted, None)];
    };
    let scope_end = scope_start + scope_segment.len();
    let Some(relative_presentation_start) = formatted[scope_end..].find(&presentation_segment)
    else {
        return vec![(formatted, None)];
    };
    let presentation_start = scope_end + relative_presentation_start;
    let presentation_end = presentation_start + presentation_segment.len();
    let mut pieces = Vec::new();
    if scope_start > 0 {
        pieces.push((formatted[..scope_start].to_string(), None));
    }
    pieces.push((
        formatted[scope_start..scope_end].to_string(),
        Some(HeaderAction::ToggleCategoryScope),
    ));
    if scope_end < presentation_start {
        pieces.push((formatted[scope_end..presentation_start].to_string(), None));
    }
    pieces.push((
        formatted[presentation_start..presentation_end].to_string(),
        Some(HeaderAction::CyclePresentationMode),
    ));
    if presentation_end < formatted.len() {
        pieces.push((formatted[presentation_end..].to_string(), None));
    }
    pieces
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
            count: counts.blocked,
            badge_state: Some(BadgeState::Blocked),
        },
        HeaderChipSpec {
            filter: StatusFilter::LimitedOnly,
            count: counts.limited,
            badge_state: Some(BadgeState::Limited),
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
        " j/k move  gg/G ends  C-d/u half  C-f/b page  enter jump"
    } else if width >= 36 {
        " j/k move  gg/G ends  enter jump"
    } else if width >= 24 {
        " j/k  gg/G  enter jump"
    } else {
        " j/k  gg/G"
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

fn category_scope_label(scope: CategoryScope) -> &'static str {
    match scope {
        CategoryScope::Current => "Current",
        CategoryScope::All => "All",
    }
}

fn category_scope_label_padded(scope: CategoryScope) -> String {
    let width = [CategoryScope::Current, CategoryScope::All]
        .into_iter()
        .map(|scope| category_scope_label(scope).len())
        .max()
        .unwrap_or(0);
    format!("{:<width$}", category_scope_label(scope))
}

fn presentation_mode_label(mode: PresentationMode) -> &'static str {
    match mode {
        PresentationMode::Tree => "Tree",
        PresentationMode::Priority => "Priority",
        PresentationMode::Flat => "Flat",
    }
}

fn presentation_mode_label_padded(mode: PresentationMode) -> String {
    let width = [
        PresentationMode::Tree,
        PresentationMode::Priority,
        PresentationMode::Flat,
    ]
    .into_iter()
    .map(|mode| presentation_mode_label(mode).len())
    .max()
    .unwrap_or(0);
    format!("{:<width$}", presentation_mode_label(mode))
}

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::config::Config;
use crate::daemon::protocol::v2::ResolvedSnapshot;
#[cfg(test)]
use crate::daemon::session_badge::BadgeState;
use crate::sidebar::render::{
    HeaderLayout, RenderedLines, SidebarRenderTheme, build_footer_line,
    build_header_layout_with_counts, display_width, render_header_lines, render_lines_with_indices,
};
use crate::sidebar::state::{SidebarState, StatusFilter};
use crate::sidebar::tree::{SidebarRow, SidebarRowKind, pane_instance_from_row_id};

use super::control_flow::ConnectionState;
use super::dialog::{CategoryDialog, CategoryDialogPhase, CategoryEditMode, MembershipChoice};
use super::projection::project_view;
use super::types::{Notice, NoticeLevel, SidebarView, rendered_row_range};

#[cfg(test)]
mod tests;

pub fn draw_snapshot<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &ResolvedSnapshot,
) -> Result<()> {
    draw_snapshot_with_theme(terminal, snapshot, &SidebarRenderTheme::default())
}

pub fn draw_snapshot_with_theme<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &ResolvedSnapshot,
    theme: &SidebarRenderTheme,
) -> Result<()> {
    let sidebar = project_view(snapshot, &Config::default(), &SidebarState::default());
    draw_snapshot_with_theme_and_scroll(terminal, snapshot, &sidebar, theme, 0)
}

fn draw_snapshot_with_theme_and_scroll<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    theme: &SidebarRenderTheme,
    scroll: usize,
) -> Result<()> {
    draw_snapshot_with_theme_and_scroll_options(
        terminal,
        snapshot,
        sidebar,
        DrawOptions {
            theme,
            scroll,
            connection: &ConnectionState::Connected,
            toast: None,
            category_dialog: None,
            rendered: None,
        },
    )
}

#[derive(Clone, Copy)]
pub(super) struct DrawOptions<'a> {
    pub(super) theme: &'a SidebarRenderTheme,
    pub(super) scroll: usize,
    pub(super) connection: &'a ConnectionState,
    pub(super) toast: Option<Notice<'a>>,
    pub(super) category_dialog: Option<&'a CategoryDialog>,
    /// Rows already rendered by the caller for scroll resolution; when present
    /// the draw path reuses them instead of rendering the same rows again.
    pub(super) rendered: Option<&'a RenderedLines>,
}

pub(super) fn draw_snapshot_with_theme_and_scroll_options<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    options: DrawOptions<'_>,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        draw_snapshot_in_area(frame, area, snapshot, sidebar, options);
    })?;
    Ok(())
}

pub fn draw_connecting<B: Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    draw_connection_placeholder(terminal, &ConnectionState::Connecting)
}

pub(super) fn draw_connection_placeholder<B: Backend>(
    terminal: &mut Terminal<B>,
    connection: &ConnectionState,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let message = match connection {
            ConnectionState::Connecting => "connecting to daemon...",
            ConnectionState::Connected => "connected",
            ConnectionState::ConfigChanged(_) => "reloading sidebar config...",
            ConnectionState::Degraded(_) => "daemon degraded; reconnecting...",
            ConnectionState::Disconnected => "daemon disconnected; reconnecting...",
        };
        draw_placeholder(frame, area, message);
    })?;
    Ok(())
}

fn draw_snapshot_in_area(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    options: DrawOptions<'_>,
) {
    let DrawOptions {
        theme,
        scroll,
        connection,
        toast,
        category_dialog,
        rendered,
    } = options;
    let header = build_header_layout_with_counts(&sidebar.state, area.width, theme, sidebar.counts);
    let areas = compute_areas(area, &header);
    if areas.header_rows > 0 {
        let header_area = Rect {
            height: areas.header_rows,
            ..area
        };
        frame.render_widget(
            Paragraph::new(render_header_lines(&header, theme)),
            header_area,
        );
    }
    if let Some(dialog) = category_dialog {
        let dialog_area = Rect {
            y: area.y + areas.header_rows,
            height: area.height.saturating_sub(areas.header_rows),
            ..area
        };
        draw_category_dialog(frame, dialog_area, dialog, theme);
        return;
    }
    let rows_area = Rect {
        y: area.y + areas.header_rows,
        height: areas.rows_height,
        ..area
    };
    let items = if sidebar.rows.is_empty() {
        connection_empty_lines(connection, theme, area.width as usize)
            .unwrap_or_else(|| {
                empty_rows_placeholder_lines(
                    sidebar.state.filter,
                    !snapshot.panes.is_empty(),
                    sidebar.counts.total > 0,
                    theme,
                )
            })
            .into_iter()
            .map(ListItem::new)
            .collect::<Vec<_>>()
    } else {
        let fallback;
        let rendered = match rendered {
            Some(rendered) => rendered,
            None => {
                fallback = render_lines_with_indices(
                    &sidebar.rows,
                    &sidebar.state,
                    area.width as usize,
                    theme,
                );
                &fallback
            }
        };
        rendered
            .lines
            .iter()
            .skip(scroll)
            .take(areas.rows_height as usize)
            .cloned()
            .map(ListItem::new)
            .collect::<Vec<_>>()
    };
    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, rows_area);
    if areas.footer_rows > 0 {
        let footer_area = Rect {
            y: area.y + areas.header_rows + areas.rows_height,
            height: areas.footer_rows,
            ..area
        };
        let footer = contextual_footer_line(area.width as usize, theme, toast, connection);
        frame.render_widget(Paragraph::new(footer), footer_area);
    }
}

fn draw_category_dialog(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    dialog: &CategoryDialog,
    theme: &SidebarRenderTheme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < 4 || area.height < 3 {
        let message = match dialog.phase {
            CategoryDialogPhase::Editing => dialog.title().trim(),
            CategoryDialogPhase::Saving { .. } => "Saving…",
        };
        frame.render_widget(
            Paragraph::new(crate::sidebar::render::truncate_display(
                message,
                area.width as usize,
            )),
            area,
        );
        return;
    }

    let popup = category_dialog_area(area, dialog);
    let accent = if matches!(&dialog.edit, CategoryEditMode::Delete { .. }) {
        theme.badge_blocked
    } else {
        theme.category
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(
            dialog.title(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(category_dialog_lines(
            dialog,
            inner.width as usize,
            inner.height as usize,
            theme,
        )),
        inner,
    );
}

fn category_dialog_area(area: Rect, dialog: &CategoryDialog) -> Rect {
    let horizontal_margin = u16::from(area.width >= 28);
    let width = area.width.saturating_sub(horizontal_margin * 2);
    let desired_height = match &dialog.edit {
        CategoryEditMode::Add { .. } => 8,
        CategoryEditMode::Rename { .. } => 9,
        CategoryEditMode::MoveRepo { choices, .. } => {
            (choices.len().min(7) as u16).saturating_add(7)
        }
        CategoryEditMode::Delete { choices, .. } => (choices.len().min(6) as u16).saturating_add(9),
    };
    let height = desired_height.min(area.height);
    Rect {
        x: area.x + horizontal_margin,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(super) fn category_dialog_lines(
    dialog: &CategoryDialog,
    width: usize,
    height: usize,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }

    let label_style = Style::default()
        .fg(theme.marker)
        .add_modifier(Modifier::DIM);
    let value_style = Style::default().fg(theme.category);
    let mut header = Vec::new();
    let choices = match &dialog.edit {
        CategoryEditMode::Add { input } => {
            header.push(Line::from(Span::styled("Name", label_style)));
            header.push(Line::from(Span::styled(
                tail_display(&format!("› {input}_"), width),
                value_style,
            )));
            None
        }
        CategoryEditMode::Rename { current, input } => {
            header.push(Line::from(vec![
                Span::styled("Current  ", label_style),
                Span::styled(current.to_string(), value_style),
            ]));
            header.push(Line::from(Span::styled("New name", label_style)));
            header.push(Line::from(Span::styled(
                tail_display(&format!("› {input}_"), width),
                value_style,
            )));
            None
        }
        CategoryEditMode::MoveRepo {
            repo_label,
            choices,
            selected,
            ..
        } => {
            header.push(Line::from(Span::styled("Repository", label_style)));
            header.push(Line::from(Span::styled(
                crate::sidebar::render::truncate_display(repo_label, width),
                Style::default().fg(theme.repo),
            )));
            header.push(Line::from(Span::styled("Move to", label_style)));
            Some((choices.as_slice(), *selected))
        }
        CategoryEditMode::Delete {
            category,
            repository_count,
            choices,
            selected,
            ..
        } => {
            header.push(Line::from(Span::styled(
                format!("Delete “{category}”?"),
                Style::default()
                    .fg(theme.badge_blocked)
                    .add_modifier(Modifier::BOLD),
            )));
            let noun = if *repository_count == 1 {
                "repository"
            } else {
                "repositories"
            };
            header.push(Line::from(Span::styled(
                format!("{repository_count} {noun} will be reassigned."),
                Style::default().fg(theme.detail),
            )));
            header.push(Line::from(Span::styled("Move to", label_style)));
            Some((choices.as_slice(), *selected))
        }
    };

    let mut footer = Vec::new();
    if let Some(error) = &dialog.error {
        footer.push(Line::from(Span::styled(
            crate::sidebar::render::truncate_display(error, width),
            Style::default().fg(theme.badge_blocked),
        )));
    }
    if choices.is_some() && matches!(dialog.phase, CategoryDialogPhase::Editing) {
        footer.push(Line::from(Span::styled(
            "j/k Select · gg/G Ends",
            label_style,
        )));
    }
    let action_color = match dialog.phase {
        CategoryDialogPhase::Saving { .. } => theme.badge_working,
        CategoryDialogPhase::Editing if matches!(&dialog.edit, CategoryEditMode::Delete { .. }) => {
            theme.badge_blocked
        }
        CategoryDialogPhase::Editing => theme.badge_done,
    };
    footer.push(Line::from(Span::styled(
        dialog.action_hint(),
        Style::default()
            .fg(action_color)
            .add_modifier(Modifier::BOLD),
    )));
    if footer.len() > height {
        footer = footer.split_off(footer.len() - height);
    }

    let body_height = height.saturating_sub(footer.len());
    let header_limit = if choices.is_some() && body_height > 0 {
        body_height.saturating_sub(1)
    } else {
        body_height
    };
    let mut lines = header
        .into_iter()
        .take(header_limit)
        .map(|line| fit_line_to_width(line, width))
        .collect::<Vec<_>>();
    if let Some((choices, selected)) = choices {
        let capacity = body_height.saturating_sub(lines.len());
        lines.extend(category_choice_lines(
            choices, selected, capacity, width, theme,
        ));
    }
    while lines.len() < body_height {
        lines.push(Line::default());
    }
    lines.extend(
        footer
            .into_iter()
            .map(|line| fit_line_to_width(line, width)),
    );
    lines
}

fn category_choice_lines(
    choices: &[MembershipChoice],
    selected: usize,
    capacity: usize,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    if choices.is_empty() || capacity == 0 {
        return Vec::new();
    }
    let visible = capacity.min(choices.len());
    let start = selected
        .saturating_sub(visible / 2)
        .min(choices.len().saturating_sub(visible));
    choices
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, choice)| {
            let text = crate::sidebar::render::truncate_display(
                &format!(
                    "{} {}",
                    if index == selected { '›' } else { ' ' },
                    choice.label()
                ),
                width,
            );
            let style = if index == selected {
                Style::default()
                    .fg(theme.selection_bar)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.detail)
            };
            Line::from(Span::styled(pad_display(text, width), style))
        })
        .collect()
}

pub(super) fn tail_display(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let target = width.saturating_sub(1);
    let mut used = 0;
    let mut suffix = Vec::new();
    for ch in text.chars().rev() {
        let char_width = display_width(&ch.to_string());
        if used + char_width > target {
            break;
        }
        suffix.push(ch);
        used += char_width;
    }
    suffix.reverse();
    format!("…{}", suffix.into_iter().collect::<String>())
}

fn pad_display(mut text: String, width: usize) -> String {
    let used = display_width(&text);
    if used < width {
        text.push_str(&" ".repeat(width - used));
    }
    text
}

fn contextual_footer_line(
    width: usize,
    theme: &SidebarRenderTheme,
    toast: Option<Notice<'_>>,
    connection: &ConnectionState,
) -> Line<'static> {
    let mut footer = build_footer_line(width);
    if let Some(notice) = toast.or_else(|| connection.notice()) {
        let color = match notice.level {
            NoticeLevel::Success => theme.badge_done,
            NoticeLevel::Progress => theme.badge_working,
            NoticeLevel::Warning => theme.category,
            NoticeLevel::Failure => theme.badge_blocked,
        };
        let mut spans = vec![Span::styled(
            format!("{} · ", notice.message),
            Style::default().fg(color),
        )];
        spans.extend(footer.spans);
        footer = Line::from(spans);
    }
    fit_line_to_width(footer, width)
}

fn connection_empty_lines(
    connection: &ConnectionState,
    theme: &SidebarRenderTheme,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    let message = match connection {
        ConnectionState::Connected => return None,
        ConnectionState::Connecting => "Connecting to daemon".to_string(),
        ConnectionState::ConfigChanged(_) => "Reloading sidebar config".to_string(),
        ConnectionState::Disconnected => "Daemon disconnected; reconnecting".to_string(),
        ConnectionState::Degraded(message) => format!("Degraded: {message}"),
    };
    Some(vec![fit_line_to_width(
        Line::from(Span::styled(
            message,
            Style::default().fg(theme.badge_blocked),
        )),
        width,
    )])
}

fn draw_placeholder(frame: &mut ratatui::Frame<'_>, area: Rect, message: &str) {
    let message = crate::sidebar::render::truncate_display(message, area.width as usize);
    let list = List::new(vec![ListItem::new(Line::from(message))])
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, area);
}

fn empty_rows_placeholder_lines(
    filter: StatusFilter,
    has_panes: bool,
    has_agents: bool,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    if filter == StatusFilter::All {
        let _ = (has_panes, has_agents);
        return vec![Line::from("No agents detected")];
    }
    vec![
        Line::from(Span::styled(
            "No matching agents",
            Style::default().fg(theme.detail),
        )),
        Line::from(Span::styled(
            format!(
                "Filter: {} · tab: next · S-tab: previous · ≡: reset",
                filter.label()
            ),
            Style::default()
                .fg(theme.marker)
                .add_modifier(Modifier::DIM),
        )),
    ]
}

fn truncate_spans_to_width(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let target = width.saturating_sub(1);
    let mut used = 0usize;
    let mut out = Vec::new();
    let mut ellipsis_style = Style::default();
    for span in spans {
        ellipsis_style = span.style;
        let mut content = String::new();
        let mut truncated = false;
        for ch in span.content.chars() {
            let ch_width = display_width(&ch.to_string());
            if used + ch_width > target {
                truncated = true;
                break;
            }
            content.push(ch);
            used += ch_width;
        }
        if !content.is_empty() {
            out.push(Span::styled(content, span.style));
        }
        if truncated || used >= target {
            break;
        }
    }
    out.push(Span::styled("…".to_string(), ellipsis_style));
    out
}

fn fit_line_to_width(line: Line<'static>, width: usize) -> Line<'static> {
    if line
        .spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum::<usize>()
        <= width
    {
        return line;
    }
    Line::from(truncate_spans_to_width(line.spans, width))
}

pub(crate) struct SidebarAreas {
    pub(crate) header_rows: u16,
    pub(crate) rows_height: u16,
    pub(crate) footer_rows: u16,
}

pub(crate) fn compute_areas(area: Rect, header: &HeaderLayout) -> SidebarAreas {
    let header_rows = header.row_count().min(area.height);
    let remaining = area.height.saturating_sub(header_rows);
    let footer_rows = if area.width > 2 && area.height >= 12 && remaining > 1 {
        1
    } else {
        0
    };
    SidebarAreas {
        header_rows,
        rows_height: remaining.saturating_sub(footer_rows),
        footer_rows,
    }
}

pub(crate) fn resolve_scroll_range(
    prev: usize,
    selection_range: Option<(usize, usize)>,
    rows_len: usize,
    viewport: usize,
) -> usize {
    if viewport == 0 || rows_len <= viewport {
        return 0;
    }
    let mut scroll = clamp_scroll_range(prev, rows_len, viewport);
    let max_scroll = rows_len.saturating_sub(viewport);
    if let Some((start, end)) = selection_range {
        let selection_height = end.saturating_sub(start).saturating_add(1);
        if selection_height > viewport || start < scroll {
            scroll = start;
        } else if end >= scroll + viewport {
            scroll = end + 1 - viewport;
        }
    }
    scroll.min(max_scroll)
}

pub(super) fn clamp_scroll_range(prev: usize, rows_len: usize, viewport: usize) -> usize {
    if viewport == 0 || rows_len <= viewport {
        0
    } else {
        prev.min(rows_len - viewport)
    }
}

pub(super) fn rendered_selection_range(
    rows: &[SidebarRow],
    row_indices: &[Option<usize>],
    selected_row_index: usize,
) -> Option<(usize, usize)> {
    let selected = rows.get(selected_row_index)?;
    let (start, mut end) = rendered_row_range(row_indices, selected_row_index)?;
    if selected.kind != SidebarRowKind::Chat {
        return Some((start, end));
    }
    let Some(selected_pane) = pane_instance_from_row_id(&selected.id) else {
        return Some((start, end));
    };
    for (row_index, row) in rows.iter().enumerate().skip(selected_row_index + 1) {
        if row.kind != SidebarRowKind::Detail
            || pane_instance_from_row_id(&row.id).as_ref() != Some(&selected_pane)
        {
            break;
        }
        if let Some((_, row_end)) = rendered_row_range(row_indices, row_index) {
            end = row_end;
        }
    }
    Some((start, end))
}

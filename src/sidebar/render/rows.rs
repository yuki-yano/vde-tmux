use crate::agent::{display_agent_label_prefix, display_agent_name};
use crate::daemon::session_badge::BadgeState;
use crate::hook::RollupLevel;
use crate::sidebar::state::{PresentationMode, SidebarState};
use crate::sidebar::tree::{
    PRIORITY_PINNED_ZONE_ID, SidebarRow, SidebarRowKind, task_progress_label,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::{display_width, line_to_string, pad_to_width, truncate_display};
use super::theme::{
    AGENT_ORIGIN_COLOR, CLAUDE_AGENT_COLOR, CODEX_AGENT_COLOR, RESPONSE_PREVIEW_COLOR,
    SidebarRenderTheme,
};

#[cfg(test)]
mod tests;

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
            let digest = render_closed_chat_digest_lines(row, state, width, theme);
            row_indices.extend(std::iter::repeat_n(Some(index), digest.len()));
            lines.extend(digest);
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
    let mut lines = vec![render_closed_chat_summary_line(row, state, width, theme)];
    if closed_chat_has_detail_line(row) {
        lines.push(render_closed_chat_detail_line(row, state, width, theme));
    }
    lines
}

fn closed_chat_has_detail_line(row: &SidebarRow) -> bool {
    !chat_task_summary_label(row).trim().is_empty() || closed_chat_reason_token(row).is_some()
}

fn render_closed_chat_summary_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = row_is_selected(row, state);
    let current_agent = row_is_current_agent(row, state);
    let indent = "  ".repeat(row.depth);
    let badge_state = row.badge_state.unwrap_or(BadgeState::Idle);
    let glyph = theme.badge_glyph(badge_state);
    let agent_source = chat_agent_label(row);

    let mut prefix = Vec::new();
    push_leading_marker_span(&mut prefix, row, current_agent, theme, &indent);
    prefix.push(pin_marker_span(row, state, theme));
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
    spans.extend(label_spans(agent, row, row_style(row, theme), theme));
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

fn render_closed_chat_detail_line(
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
    let (summary_budget, reason) = match reason {
        Some(reason) if available > reason_width + 1 => {
            (available - reason_width - 1, Some(reason))
        }
        _ => (available, None),
    };
    let summary = truncate_display(&chat_task_summary_label(row), summary_budget);
    spans.push(Span::styled(summary, row_style(row, theme)));
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
    let current_agent_marker = row.kind == SidebarRowKind::Chat && row_is_current_agent(row, state);
    if row.kind == SidebarRowKind::Zone {
        let text = truncate_display(
            &format!(" ▍{} {}", row.label, row.chat_count),
            width.saturating_sub(1),
        );
        let zone_color = if row.id == PRIORITY_PINNED_ZONE_ID {
            theme.toggle
        } else {
            theme.badge_color(row.badge_state.unwrap_or(BadgeState::Blocked))
        };
        let style = Style::default().fg(zone_color).add_modifier(Modifier::BOLD);
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
        SidebarRowKind::Chat => chat_display_label(row),
        _ => row.label.clone(),
    };
    let label = truncate_display(&label_source, label_budget);

    let mut spans = Vec::new();
    if row.kind == SidebarRowKind::Chat {
        let marker = if row.expanded { "▾" } else { "▸" };
        push_leading_marker_span(&mut spans, row, current_agent_marker, theme, &indent);
        spans.push(pin_marker_span(row, state, theme));
        spans.push(Span::styled(
            format!("{marker} "),
            toggle_marker_style(theme),
        ));
    } else if matches!(row.kind, SidebarRowKind::Category | SidebarRowKind::Repo) {
        let marker = if row.expanded { "▾" } else { "▸" };
        push_leading_marker_span(&mut spans, row, current_agent_marker, theme, &indent);
        spans.push(Span::styled(
            format!("{marker} "),
            toggle_marker_style(theme),
        ));
    } else if row.kind == SidebarRowKind::Detail && row.id.starts_with("meta::") {
        push_leading_marker_span(&mut spans, row, current_agent_marker, theme, &indent);
        spans.push(Span::styled(
            "  ".to_string(),
            Style::default().fg(theme.marker),
        ));
    } else {
        push_leading_marker_span(&mut spans, row, current_agent_marker, theme, &head);
    }
    if let Some((glyph, color)) = badge {
        spans.push(Span::styled(glyph, badge_style(color, row)));
    }
    spans.extend(label_spans(label, row, style, theme));
    if let Some(git) = &git {
        spans.push(Span::styled(
            format!(" {}", git.branch),
            Style::default().fg(theme.branch),
        ));
        if let Some(ahead) = &git.ahead {
            spans.push(Span::styled(
                format!(" {ahead}"),
                Style::default().fg(theme.git_ahead),
            ));
        }
        if let Some(behind) = &git.behind {
            spans.push(Span::styled(
                format!(" {behind}"),
                Style::default().fg(theme.git_behind),
            ));
        }
        if let Some(insertions) = &git.insertions {
            spans.push(Span::styled(
                format!(" {insertions}"),
                Style::default().fg(theme.git_insertions),
            ));
        }
        if let Some(deletions) = &git.deletions {
            spans.push(Span::styled(
                format!(" {deletions}"),
                Style::default().fg(theme.git_deletions),
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
    if !matches!(row.kind, SidebarRowKind::Chat | SidebarRowKind::Detail) {
        return false;
    }
    let Some(selected_pane) = crate::sidebar::tree::pane_instance_from_row_id(selection) else {
        return false;
    };
    crate::sidebar::tree::pane_instance_from_row_id(&row.id).as_ref() == Some(&selected_pane)
}

fn row_is_current_agent(row: &SidebarRow, state: &SidebarState) -> bool {
    if row.kind != SidebarRowKind::Chat {
        return false;
    }
    crate::sidebar::tree::pane_instance_from_row_id(&row.id)
        .is_some_and(|pane| state.current_agents.contains(&pane))
}

fn push_leading_marker_span(
    spans: &mut Vec<Span<'static>>,
    row: &SidebarRow,
    current_agent: bool,
    theme: &SidebarRenderTheme,
    tail: &str,
) {
    spans.push(row_leading_marker_span(row, current_agent, theme));
    if !tail.is_empty() {
        spans.push(Span::styled(
            tail.to_string(),
            Style::default().fg(theme.marker),
        ));
    }
}

fn row_leading_marker_span(
    row: &SidebarRow,
    current_agent: bool,
    theme: &SidebarRenderTheme,
) -> Span<'static> {
    let (marker, style) = match (row.active, current_agent) {
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
        let agent_style = base
            .fg(agent_identity_color(&agent, theme))
            .add_modifier(Modifier::BOLD);
        if row.expanded
            && let Some(state_context) = rest.strip_prefix(": ")
        {
            let mut spans = vec![
                Span::styled(agent_part.to_string(), agent_style),
                Span::styled(": ".to_string(), base.fg(AGENT_ORIGIN_COLOR)),
            ];
            spans.extend(state_context_spans(state_context, row, theme));
            return spans;
        }
        return vec![
            Span::styled(agent_part.to_string(), agent_style),
            Span::styled(rest.to_string(), base.fg(AGENT_ORIGIN_COLOR)),
        ];
    }
    vec![Span::styled(label, base)]
}

fn detail_label_spans(
    label: &str,
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
) -> Option<Vec<Span<'static>>> {
    if row.id.ends_with("::signal") {
        return Some(signal_label_spans(label, row, theme));
    }
    if row.id.ends_with("::origin") {
        return Some(vec![Span::styled(
            label.to_string(),
            Style::default().fg(AGENT_ORIGIN_COLOR),
        )]);
    }
    if row.id.ends_with("::background") {
        return Some(vec![Span::styled(
            label.to_string(),
            Style::default().fg(theme.badge_working),
        )]);
    }
    if row.id.ends_with("::response") {
        let body = label.strip_prefix("▷ ").unwrap_or(label);
        return Some(vec![
            Span::styled(
                "▷ ".to_string(),
                Style::default()
                    .fg(theme.branch)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                body.to_string(),
                Style::default().fg(RESPONSE_PREVIEW_COLOR),
            ),
        ]);
    }
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

fn signal_label_spans(
    label: &str,
    row: &SidebarRow,
    theme: &SidebarRenderTheme,
) -> Vec<Span<'static>> {
    let branch_style = Style::default().fg(theme.branch);
    let task = task_progress_token(row);
    let git = row.git.as_ref();
    let ahead = git
        .filter(|git| git.ahead > 0)
        .map(|git| format!("↑ {}", git.ahead));
    let behind = git
        .filter(|git| git.behind > 0)
        .map(|git| format!("↓ {}", git.behind));
    let insertions = git
        .filter(|git| git.insertions > 0)
        .map(|git| format!("+{}", git.insertions));
    let deletions = git
        .filter(|git| git.deletions > 0)
        .map(|git| format!("-{}", git.deletions));
    let mut spans = Vec::new();
    for (index, part) in label.split("  ").enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ".to_string(), branch_style));
        }
        let style = if ahead.as_deref() == Some(part) {
            Style::default().fg(theme.git_ahead)
        } else if behind.as_deref() == Some(part) {
            Style::default().fg(theme.git_behind)
        } else if insertions.as_deref() == Some(part) {
            Style::default().fg(theme.git_insertions)
        } else if deletions.as_deref() == Some(part) {
            Style::default().fg(theme.git_deletions)
        } else if let Some(task) = task.as_ref().filter(|task| task.text == part) {
            closed_chat_right_tone_style(task.tone, row, theme)
        } else {
            branch_style
        };
        spans.push(Span::styled(part.to_string(), style));
    }
    spans
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
    insertions: Option<String>,
    deletions: Option<String>,
}

fn format_git_badge_parts(badge: &crate::git::GitBadge) -> GitBadgeText {
    let mut parts = vec![badge.branch.clone()];
    let branch = parts.remove(0);
    GitBadgeText {
        branch,
        ahead: (badge.ahead > 0).then(|| format!("↑{}", badge.ahead)),
        behind: (badge.behind > 0).then(|| format!("↓{}", badge.behind)),
        insertions: (badge.insertions > 0).then(|| format!("+{}", badge.insertions)),
        deletions: (badge.deletions > 0).then(|| format!("-{}", badge.deletions)),
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
            SidebarRowKind::Detail => None,
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
    let color = if row.id == PRIORITY_PINNED_ZONE_ID {
        theme.toggle
    } else {
        theme.badge_color(row.badge_state.unwrap_or(BadgeState::Blocked))
    };
    Line::from(Span::styled(
        text,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
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
    leading_marker_line(row, false, theme, pad_to_width(text, width), style)
}

fn render_chat_dense_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = state.selection.as_deref() == Some(row.id.as_str());
    let current_agent = row_is_current_agent(row, state);
    let badge_state = row.badge_state.unwrap_or(BadgeState::Idle);
    let glyph = theme.badge_glyph(badge_state);
    let agent_source = row
        .meta
        .as_ref()
        .and_then(|meta| meta.agent.as_deref())
        .unwrap_or_else(|| row.label.split(':').next().unwrap_or(row.label.as_str()));
    let agent_color = agent_identity_color(agent_source, theme);
    let agent = truncate_display(&display_agent_name(agent_source), 7);
    let origin = row
        .meta
        .as_ref()
        .and_then(|meta| meta.origin.as_deref())
        .and_then(|origin| origin.rsplit('/').next())
        .unwrap_or("");
    let origin = origin.chars().take(3).collect::<String>();
    let right = right_label(row).unwrap_or_default();
    let body = chat_task_summary_label(row);
    let agent_cell = format!(" {agent:<7}");
    let origin_cell = if origin.is_empty() {
        String::new()
    } else {
        format!(" {origin:<3} ")
    };
    let prefix_after_glyph = format!("{agent_cell}{origin_cell}");
    let show_pin_cell = state.presentation_mode == PresentationMode::Priority || row_pinned(row);
    let pin_width = usize::from(show_pin_cell);
    let right_width = display_width(&right);
    let right_reserved = if right_width > 0 { right_width + 1 } else { 0 };
    let label_budget = width
        .saturating_sub(1)
        .saturating_sub(pin_width)
        .saturating_sub(display_width(glyph))
        .saturating_sub(display_width(&prefix_after_glyph))
        .saturating_sub(right_reserved)
        .saturating_sub(1);
    let label = truncate_display(&body, label_budget);
    let used = 1
        + pin_width
        + display_width(glyph)
        + display_width(&prefix_after_glyph)
        + display_width(&label);
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
    let mut spans = vec![row_leading_marker_span(row, current_agent, theme)];
    if show_pin_cell {
        spans.push(pin_marker_span(row, state, theme));
    }
    spans.extend([
        Span::styled(
            glyph.to_string(),
            badge_style(theme.badge_color(badge_state), row),
        ),
        Span::styled(
            agent_cell,
            style.fg(agent_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(origin_cell, style.fg(AGENT_ORIGIN_COLOR)),
        Span::styled(label, style),
        Span::raw(" ".repeat(filler)),
        Span::styled(right, right_status_style),
        Span::raw(" ".to_string()),
    ]);
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

fn leading_marker_line(
    row: &SidebarRow,
    current_agent: bool,
    theme: &SidebarRenderTheme,
    text: String,
    style: Style,
) -> Line<'static> {
    if !row.active && !current_agent {
        return Line::from(Span::styled(text, style));
    }
    let rest = text.chars().skip(1).collect::<String>();
    Line::from(vec![
        row_leading_marker_span(row, current_agent, theme),
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
        let marker = row_leading_marker_span(row, row_is_current_agent(row, state), theme);
        let pinned = row_pinned(row);
        let pin_width = usize::from(pinned);
        let text = if right.is_empty() {
            glyph.to_string()
        } else {
            format!("{glyph} {right}")
        };
        let body = pad_to_width(
            truncate_display(&text, width.saturating_sub(1 + pin_width)),
            width.saturating_sub(1 + pin_width),
        );
        let mut spans = vec![marker];
        if pinned {
            spans.push(Span::styled(
                "✦".to_string(),
                Style::default().fg(theme.toggle),
            ));
        }
        spans.push(Span::styled(
            body,
            badge_style(theme.badge_color(badge_state), row),
        ));
        let mut line = Line::from(spans);
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
        BadgeState::Limited,
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
        let pinned = row_pinned(row);
        let visible_glyph = if pinned {
            "✦"
        } else {
            theme.badge_glyph(glyph)
        };
        let glyph_style = if pinned {
            style.fg(theme.toggle)
        } else {
            style
        };
        lines.push(Line::from(vec![
            row_leading_marker_span(row, row_is_current_agent(row, state), theme),
            Span::styled(visible_glyph.to_string(), glyph_style),
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

fn row_pinned(row: &SidebarRow) -> bool {
    row.meta.as_ref().is_some_and(|meta| meta.pinned)
}

fn pin_marker_span(
    row: &SidebarRow,
    state: &SidebarState,
    theme: &SidebarRenderTheme,
) -> Span<'static> {
    match row.meta.as_ref() {
        Some(meta) if meta.pinned => {
            Span::styled("✦".to_string(), Style::default().fg(theme.toggle))
        }
        Some(meta) if state.presentation_mode == PresentationMode::Priority && meta.is_unread => {
            Span::styled("·".to_string(), Style::default().fg(theme.marker))
        }
        _ => Span::styled(" ".to_string(), Style::default().fg(theme.marker)),
    }
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
            closed_chat_state_or_time_label(row)
        }
        SidebarRowKind::Detail | SidebarRowKind::Zone => None,
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
    if let Some(task) = task_progress_token(row) {
        parts.push(task);
    }
    if let Some(subagents) = subagent_count_token(row) {
        parts.push(subagents);
    }
    if let Some(time) = closed_chat_state_or_time_label(row) {
        parts.push(ClosedChatRightPart {
            text: time,
            tone: ClosedChatRightTone::State,
        });
    }
    parts
}

fn closed_chat_state_or_time_label(row: &SidebarRow) -> Option<String> {
    match row.badge_state? {
        BadgeState::Blocked | BadgeState::Limited | BadgeState::Working => row
            .meta
            .as_ref()
            .and_then(|meta| meta.elapsed_secs)
            .map(elapsed_label),
        BadgeState::Done | BadgeState::Idle => row
            .meta
            .as_ref()
            .and_then(|meta| meta.completed_age_secs)
            .map(|secs| format!("{} ago", elapsed_label(secs))),
    }
}

fn task_progress_token(row: &SidebarRow) -> Option<ClosedChatRightPart> {
    let meta = row.meta.as_ref()?;
    let done = meta.tasks_done?;
    let total = meta.tasks_total?;
    task_progress_label(done, total).map(|text| ClosedChatRightPart {
        text,
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
        RollupLevel::Permission | RollupLevel::Waiting | RollupLevel::Limited | RollupLevel::Error
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
        "usage_limit" => "usage-limit".to_string(),
        "network_error" => "network".to_string(),
        _ => truncate_display(&reason.replace('_', "-"), 16),
    }
}

fn chat_agent_label(row: &SidebarRow) -> String {
    let agent = row
        .meta
        .as_ref()
        .and_then(|meta| meta.agent.as_deref())
        .filter(|agent| !agent.trim().is_empty())
        .map(display_agent_name)
        .unwrap_or_else(|| {
            display_agent_name(row.label.split(':').next().unwrap_or(row.label.as_str()))
        });
    match row
        .meta
        .as_ref()
        .and_then(|meta| meta.origin.as_deref())
        .filter(|origin| !origin.trim().is_empty())
    {
        Some(origin) => format!("{agent} · {origin}"),
        None => agent,
    }
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

fn chat_task_summary_label(row: &SidebarRow) -> String {
    row.meta
        .as_ref()
        .and_then(|meta| meta.task_summary.as_deref())
        .filter(|summary| !summary.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

fn elapsed_label(secs: i64) -> String {
    crate::sidebar::tree::humanize_secs(secs)
}

fn elapsed_full_label(secs: i64) -> String {
    crate::sidebar::tree::humanize_secs_full(secs)
}

fn expanded_chat_right_label(row: &SidebarRow) -> Option<String> {
    match row.badge_state? {
        BadgeState::Blocked | BadgeState::Limited | BadgeState::Working => row
            .meta
            .as_ref()
            .and_then(|meta| meta.elapsed_secs)
            .map(elapsed_full_label),
        BadgeState::Done | BadgeState::Idle => row
            .meta
            .as_ref()
            .and_then(|meta| meta.completed_age_secs)
            .map(|secs| format!("{} ago", elapsed_label(secs))),
    }
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
    if let Some(insertions) = &git.insertions {
        width += 1 + display_width(insertions);
    }
    if let Some(deletions) = &git.deletions {
        width += 1 + display_width(deletions);
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
        SidebarRowKind::Detail if row.id.ends_with("::summary") => {
            Style::default().fg(Color::Reset)
        }
        SidebarRowKind::Detail => Style::default().fg(theme.detail),
    }
}

fn agent_identity_color(agent: &str, theme: &SidebarRenderTheme) -> Color {
    match agent.trim().to_ascii_lowercase().as_str() {
        "codex" => CODEX_AGENT_COLOR,
        "claude" => CLAUDE_AGENT_COLOR,
        _ => theme.branch,
    }
}

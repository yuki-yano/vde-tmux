use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::daemon::protocol::v2::ResolvedSnapshot;
#[cfg(test)]
use crate::daemon::session_badge::BadgeState;
use crate::pane_state::PaneInstance;
use crate::sidebar::render::{RenderedLines, SidebarRenderTheme, render_lines_with_indices};
use crate::sidebar::state::{
    CategoryScope, PresentationMode, SidebarAction, SidebarState, StatusFilter,
};
use crate::sidebar::tree::{
    SidebarProjection, SidebarRow, SidebarRowKind, chat_row_id, pane_instance_from_row_id,
    project_sidebar, row_refs,
};

use super::effects::dispatch_click_action;
use super::types::{
    ClickAction, ClickContext, DrawnFrame, MarkCompleteUi, NavigationRequest, NoticeLevel,
    SidebarView, rendered_row_range,
};

#[cfg(test)]
mod tests;

pub(super) fn project_view(
    snapshot: &ResolvedSnapshot,
    config: &Config,
    state: &SidebarState,
) -> SidebarView {
    let SidebarProjection { rows, counts } = project_sidebar(
        config,
        &snapshot.panes,
        &snapshot.sidebar_model,
        &snapshot.events,
        state,
        crate::sidebar::tree::now_epoch_secs(),
    );
    SidebarView {
        state: state.clone(),
        rows,
        counts,
    }
}

pub(super) fn clear_stale_pane_selection(
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
) -> bool {
    let Some(selected) = state
        .selection
        .as_deref()
        .and_then(pane_instance_from_row_id)
    else {
        return false;
    };
    if !snapshot
        .panes
        .iter()
        .any(|pane| pane.pane_instance == selected)
    {
        state.selection = None;
        state.manual_scroll = false;
        state.version = state.version.saturating_add(1);
        return true;
    }
    false
}

pub(super) fn apply_remote_navigation(
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
    last_revision: &mut u64,
    last_queued: &mut Option<NavigationRequest>,
) -> bool {
    let navigation = &snapshot.sidebar_model.navigation;
    if navigation.revision <= *last_revision {
        return false;
    }
    let changed = state.selection != navigation.selection
        || state.scroll != navigation.scroll
        || state.manual_scroll != navigation.manual_scroll;
    state.selection = navigation.selection.clone();
    state.scroll = navigation.scroll;
    state.manual_scroll = navigation.manual_scroll;
    if changed {
        state.version = state.version.saturating_add(1);
    }
    *last_revision = navigation.revision;
    *last_queued = Some(NavigationRequest {
        selection: navigation.selection.clone(),
        scroll: navigation.scroll,
        manual_scroll: navigation.manual_scroll,
    });
    changed
}

pub(super) fn seed_initial_sidebar_context(
    snapshot: &ResolvedSnapshot,
    config: &Config,
    state: &mut SidebarState,
    pane_id: Option<&str>,
    pane_pid: Option<u32>,
    session_id: Option<&str>,
) {
    let pane_instance = pane_id.zip(pane_pid).and_then(|(pane_id, pane_pid)| {
        let pane = PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        };
        pane.validate().is_ok().then_some(pane)
    });
    if let Some(pane) = pane_instance.as_ref() {
        set_sidebar_context(snapshot, state, pane, session_id.unwrap_or_default());
    } else {
        state.current_session_id = session_id
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        refresh_current_category(snapshot, state);
    }
    select_context_agent(snapshot, config, state, pane_instance.as_ref(), session_id);
}

pub(super) fn set_sidebar_context(
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
    pane_instance: &PaneInstance,
    session_id: &str,
) -> bool {
    let Some(pane) = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance == *pane_instance)
    else {
        return false;
    };
    if !session_id.is_empty()
        && !pane
            .session_links
            .iter()
            .any(|link| link.session_id == session_id)
    {
        return false;
    }
    let next_session = (!session_id.trim().is_empty()).then(|| session_id.to_string());
    let context_changed = state.return_target.as_ref() != Some(pane_instance)
        || state.current_session_id != next_session;
    state.return_target = Some(pane_instance.clone());
    state.current_session_id = next_session;
    let category_changed = refresh_current_category(snapshot, state);
    if context_changed && !category_changed {
        state.version = state.version.saturating_add(1);
    }
    context_changed || category_changed
}

pub(super) fn refresh_current_category(
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
) -> bool {
    let session_category = state
        .current_session_id
        .as_ref()
        .and_then(|session_id| snapshot.sidebar_model.session_categories.get(session_id))
        .cloned();
    let pane_category = state.return_target.as_ref().and_then(|target| {
        let pane = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_instance == *target)?;
        let category = snapshot
            .sidebar_model
            .repo_identities
            .get(&pane.current_path)
            .and_then(|identity| {
                snapshot
                    .sidebar_model
                    .categories
                    .placements
                    .get(&identity.key)
            })
            .map(|placement| placement.category.to_string())
            .unwrap_or_else(|| crate::category::UNCATEGORIZED.to_string());
        Some(category)
    });
    let next = session_category.or(pane_category);
    if state.current_category == next {
        return false;
    }
    state.current_category = next;
    state.version = state.version.saturating_add(1);
    true
}

pub(super) fn refresh_current_agents(
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
) -> bool {
    let next = snapshot
        .panes
        .iter()
        .filter(|pane| {
            pane.focused
                && pane
                    .resolved
                    .as_ref()
                    .is_some_and(|resolved| resolved.canonical.agent_present)
        })
        .map(|pane| pane.pane_instance.clone())
        .collect::<BTreeSet<_>>();
    if state.current_agents == next {
        return false;
    }
    state.current_agents = next;
    state.version = state.version.saturating_add(1);
    true
}

pub(super) fn seed_persisted_sidebar_preferences(
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
) {
    state.category_scope = snapshot.sidebar_model.preferences.category_scope;
    state.presentation_mode = snapshot.sidebar_model.preferences.presentation_mode;
    state.filter = snapshot.sidebar_model.preferences.filter;
    state.collapsed = snapshot
        .sidebar_model
        .preferences
        .expansion_overrides
        .clone();
}

pub(super) fn apply_remote_sidebar_preferences(
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
    last_remote: &mut Option<(CategoryScope, PresentationMode, StatusFilter)>,
    last_queued: &mut Option<(CategoryScope, PresentationMode, StatusFilter)>,
) -> bool {
    let remote = (
        snapshot.sidebar_model.preferences.category_scope,
        snapshot.sidebar_model.preferences.presentation_mode,
        snapshot.sidebar_model.preferences.filter,
    );
    if last_remote.as_ref() == Some(&remote) {
        return false;
    }
    let local = (state.category_scope, state.presentation_mode, state.filter);
    let mut queued = last_queued.unwrap_or(local);
    // A field that differs from the last locally queued value was changed by a newer local input
    // that has not reached the preference worker yet. Preserve it while adopting independent
    // remote fields so rapid mode/filter inputs cannot undo each other.
    let next = (
        if local.0 == queued.0 {
            queued.0 = remote.0;
            remote.0
        } else {
            local.0
        },
        if local.1 == queued.1 {
            queued.1 = remote.1;
            remote.1
        } else {
            local.1
        },
        if local.2 == queued.2 {
            queued.2 = remote.2;
            remote.2
        } else {
            local.2
        },
    );
    let changed = local != next;
    state.category_scope = next.0;
    state.presentation_mode = next.1;
    state.filter = next.2;
    if changed {
        state.version = state.version.saturating_add(1);
    }
    *last_remote = Some(remote);
    *last_queued = Some(queued);
    changed
}

pub(super) fn select_context_agent(
    snapshot: &ResolvedSnapshot,
    config: &Config,
    state: &mut SidebarState,
    direct_pane: Option<&PaneInstance>,
    session_id: Option<&str>,
) -> bool {
    // Persisted filters are the default presentation for this instance, but they
    // must not suppress the canonical startup target required by the tmux origin
    // context. Keep the restored filter on `state` and use an unfiltered clone
    // only to resolve the stable row identity in the current view-mode order.
    let mut selection_state = state.clone();
    selection_state.filter = StatusFilter::All;
    let sidebar = project_view(snapshot, config, &selection_state);
    let direct_row = direct_pane.and_then(|pane| {
        let row_id = chat_row_id(pane);
        sidebar
            .rows
            .iter()
            .any(|row| row.kind == SidebarRowKind::Chat && row.id == row_id)
            .then_some(row_id)
    });
    let selection = direct_row.or_else(|| {
        let session_id = session_id.filter(|value| !value.trim().is_empty())?;
        sidebar.rows.iter().find_map(|row| {
            if row.kind != SidebarRowKind::Chat {
                return None;
            }
            let pane = pane_instance_from_row_id(&row.id)?;
            snapshot
                .panes
                .iter()
                .find(|candidate| {
                    candidate.pane_instance == pane
                        && candidate.resolved.is_some()
                        && candidate
                            .session_links
                            .iter()
                            .any(|link| link.session_id == session_id)
                })
                .map(|_| row.id.clone())
        })
    });
    if state.selection == selection {
        return false;
    }
    state.selection = selection;
    state.manual_scroll = false;
    state.version = state.version.saturating_add(1);
    true
}

pub(super) fn apply_local_sidebar_key(state: &mut SidebarState, sidebar: &SidebarView, key: &str) {
    use crate::sidebar::input::SidebarInputAction;

    let Some(action) = crate::sidebar::input::parse_key(key) else {
        return;
    };
    let refs = row_refs(&sidebar.rows);
    match action {
        SidebarInputAction::MoveNext => {
            if state.apply(SidebarAction::MoveNext, &refs) {
                state.manual_scroll = false;
            }
        }
        SidebarInputAction::MovePrevious => {
            if state.apply(SidebarAction::MovePrevious, &refs) {
                state.manual_scroll = false;
            }
        }
        SidebarInputAction::ToggleExpand => {
            state.apply(SidebarAction::ToggleExpand, &refs);
        }
        SidebarInputAction::ToggleCategoryScope => {
            state.apply(SidebarAction::ToggleCategoryScope, &refs);
        }
        SidebarInputAction::SetPresentationMode(mode) => {
            state.apply(SidebarAction::SetPresentationMode(mode), &refs);
        }
        SidebarInputAction::CyclePresentationMode => {
            state.apply(SidebarAction::CyclePresentationMode, &refs);
        }
        SidebarInputAction::SetFilter(filter) => {
            if sidebar.counts.filter_is_available(filter) {
                state.set_filter(filter);
            }
        }
        SidebarInputAction::CycleFilterForward | SidebarInputAction::CycleFilterBackward => {
            let forward = matches!(action, SidebarInputAction::CycleFilterForward);
            let mut filter = if forward {
                state.filter.next()
            } else {
                state.filter.previous()
            };
            while !sidebar.counts.filter_is_available(filter) {
                filter = if forward {
                    filter.next()
                } else {
                    filter.previous()
                };
            }
            state.set_filter(filter);
        }
        SidebarInputAction::ToggleRow(row_id) => {
            let row_id = pane_instance_from_row_id(&row_id)
                .map(|pane| chat_row_id(&pane))
                .unwrap_or(row_id);
            state.selection = Some(row_id.clone());
            state.manual_scroll = false;
            state.toggle_expanded(&row_id);
        }
        SidebarInputAction::FocusNextAttention | SidebarInputAction::FocusPreviousAttention => {
            let ids = sidebar
                .rows
                .iter()
                .filter(|row| {
                    row.kind == SidebarRowKind::Chat
                        && row
                            .badge_state
                            .is_some_and(crate::sidebar::tree::badge_needs_user_input)
                })
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return;
            }
            let forward = matches!(action, SidebarInputAction::FocusNextAttention);
            let current = state
                .selection
                .as_deref()
                .and_then(|selection| ids.iter().position(|id| *id == selection));
            let index = match (current, forward) {
                (None, true) => 0,
                (None, false) => ids.len() - 1,
                (Some(index), true) => (index + 1) % ids.len(),
                (Some(index), false) => (index + ids.len() - 1) % ids.len(),
            };
            if state.selection.as_deref() != Some(ids[index]) {
                state.selection = Some(ids[index].to_string());
                state.manual_scroll = false;
                state.version = state.version.saturating_add(1);
            }
        }
        SidebarInputAction::Activate
        | SidebarInputAction::MoveFirst
        | SidebarInputAction::MoveLast
        | SidebarInputAction::HalfPageDown
        | SidebarInputAction::HalfPageUp
        | SidebarInputAction::PageDown
        | SidebarInputAction::PageUp
        | SidebarInputAction::AgentNext
        | SidebarInputAction::AgentPrevious
        | SidebarInputAction::ReadCurrent
        | SidebarInputAction::UnreadLatest
        | SidebarInputAction::TogglePanePin
        | SidebarInputAction::ReorderUp
        | SidebarInputAction::ReorderDown => {}
    }
}

pub(super) fn adjacent_agent_target(
    selection: Option<&str>,
    rows: &[SidebarRow],
    forward: bool,
) -> Option<(String, PaneInstance)> {
    let agents = rows
        .iter()
        .filter(|row| row.kind == SidebarRowKind::Chat)
        .filter_map(|row| pane_instance_from_row_id(&row.id).map(|pane| (row.id.clone(), pane)))
        .collect::<Vec<_>>();
    if agents.is_empty() {
        return None;
    }
    let current =
        selection.and_then(|selection| agents.iter().position(|(row_id, _)| row_id == selection));
    let index = match (current, forward) {
        (None, true) => 0,
        (None, false) => agents.len() - 1,
        (Some(index), true) => index.checked_add(1).filter(|next| *next < agents.len())?,
        (Some(index), false) => index.checked_sub(1)?,
    };
    Some(agents[index].clone())
}

pub(super) struct PeekSource<'a> {
    pub(super) pane: &'a PaneInstance,
    pub(super) client_pid: u32,
}

pub(super) fn peek_from_control(
    socket: &Path,
    server_identity: &str,
    snapshot: &ResolvedSnapshot,
    source: PeekSource<'_>,
    target: Option<(String, PaneInstance)>,
    state: &mut SidebarState,
    ui: &mut MarkCompleteUi,
) {
    if let Err(error) = source.pane.validate() {
        ui.set_toast(
            format!("invalid jump source: {error}"),
            NoticeLevel::Failure,
            Duration::from_secs(5),
        );
        return;
    }
    let Some((_row_id, pane_instance)) = target else {
        return;
    };
    if !snapshot
        .panes
        .iter()
        .any(|pane| pane.pane_instance == pane_instance)
    {
        ui.set_toast(
            "jump target is stale".to_string(),
            NoticeLevel::Warning,
            Duration::from_secs(5),
        );
        return;
    }
    match crate::sidebar::client::send_sidebar_peek_v2(
        socket,
        server_identity,
        pane_instance,
        source.pane.clone(),
        source.client_pid,
    ) {
        Ok(actual) => {
            let actual_row_id = chat_row_id(&actual);
            if state.selection.as_deref() != Some(actual_row_id.as_str()) || state.manual_scroll {
                state.selection = Some(actual_row_id);
                state.manual_scroll = false;
                state.version = state.version.saturating_add(1);
            }
        }
        Err(error) => ui.set_toast(
            format!("peek failed: {error}"),
            NoticeLevel::Failure,
            Duration::from_secs(5),
        ),
    }
}

fn unread_advance_candidates(source_pane: &PaneInstance, rows: &[SidebarRow]) -> Vec<PaneInstance> {
    let Some(current) = rows.iter().position(|row| {
        row.kind == SidebarRowKind::Chat
            && pane_instance_from_row_id(&row.id).as_ref() == Some(source_pane)
    }) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    rows.iter()
        .skip(current + 1)
        .filter(|row| row.kind == SidebarRowKind::Chat)
        .filter(|row| row.meta.as_ref().is_some_and(|meta| meta.is_unread))
        .filter_map(|row| pane_instance_from_row_id(&row.id))
        .filter(|pane| seen.insert(pane.clone()))
        .take(crate::pane_state::MAX_VIEW_PANES)
        .collect()
}

pub(super) fn read_current_from_control(
    socket: &Path,
    server_identity: &str,
    source_pane: &PaneInstance,
    client_pid: u32,
    rows: &[SidebarRow],
    state: &mut SidebarState,
    ui: &mut MarkCompleteUi,
) {
    let candidates = unread_advance_candidates(source_pane, rows);
    match crate::sidebar::client::send_sidebar_read_peek_v2(
        socket,
        server_identity,
        source_pane.clone(),
        client_pid,
        candidates,
    ) {
        Ok(result) => {
            let (advance, level) = match result.advance_outcome {
                crate::daemon::protocol::v2::PeekAdvanceOutcome::Jumped { pane_instance } => {
                    let row_id = chat_row_id(&pane_instance);
                    if state.selection.as_deref() != Some(row_id.as_str()) || state.manual_scroll {
                        state.selection = Some(row_id);
                        state.manual_scroll = false;
                        state.version = state.version.saturating_add(1);
                    }
                    ("; advanced to next unread", NoticeLevel::Success)
                }
                crate::daemon::protocol::v2::PeekAdvanceOutcome::Stayed => {
                    ("; stayed on current pane", NoticeLevel::Success)
                }
                crate::daemon::protocol::v2::PeekAdvanceOutcome::Failed => {
                    ("; advance failed", NoticeLevel::Warning)
                }
            };
            let read = match result.read_outcome {
                crate::daemon::protocol::v2::PaneApplyOutcome::Committed => "marked pane read",
                crate::daemon::protocol::v2::PaneApplyOutcome::Noop => "pane was already read",
            };
            ui.set_toast(format!("{read}{advance}"), level, Duration::from_secs(3));
        }
        Err(error) => ui.set_toast(
            format!("read-current failed: {error}"),
            NoticeLevel::Failure,
            Duration::from_secs(5),
        ),
    }
}

pub(super) fn jump_latest_unread_from_control(
    socket: &Path,
    server_identity: &str,
    source_pane: &PaneInstance,
    ui: &mut MarkCompleteUi,
) {
    let result = crate::sidebar::client::send_latest_unread_jump_v2(
        socket,
        server_identity,
        source_pane.clone(),
    );
    let (message, level, duration) = match result {
        Ok(()) => (
            "jumped to latest unread pane".to_string(),
            NoticeLevel::Success,
            Duration::from_secs(3),
        ),
        Err(error) => (
            format!("unread jump failed: {error}"),
            NoticeLevel::Failure,
            Duration::from_secs(5),
        ),
    };
    ui.set_toast(message, level, duration);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ViewportMove {
    First,
    Last,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
}

pub(super) fn move_viewport_selection(
    snapshot: &ResolvedSnapshot,
    config: &Config,
    state: &mut SidebarState,
    frame: Option<&DrawnFrame>,
    movement: ViewportMove,
    theme: &SidebarRenderTheme,
) {
    let Some(frame) = frame else {
        return;
    };
    let sidebar = project_view(snapshot, config, state);
    let rendered =
        render_lines_with_indices(&sidebar.rows, &sidebar.state, frame.width as usize, theme);
    move_projected_viewport(&sidebar, &rendered, state, frame, movement);
}

pub(super) fn move_projected_viewport(
    sidebar: &SidebarView,
    rendered: &RenderedLines,
    state: &mut SidebarState,
    frame: &DrawnFrame,
    movement: ViewportMove,
) {
    if rendered.lines.is_empty() {
        return;
    }
    let viewport = usize::from(frame.rows_height).max(1);
    let max_scroll = rendered.lines.len().saturating_sub(viewport);
    let current_line = state
        .selection
        .as_deref()
        .and_then(|selection| sidebar.rows.iter().position(|row| row.id == selection))
        .and_then(|row_index| rendered_row_range(&rendered.row_indices, row_index))
        .map(|range| range.0)
        .unwrap_or(state.scroll.min(rendered.lines.len() - 1));
    let amount = match movement {
        ViewportMove::HalfPageDown | ViewportMove::HalfPageUp => (viewport / 2).max(1),
        ViewportMove::PageDown | ViewportMove::PageUp => viewport,
        ViewportMove::First | ViewportMove::Last => 0,
    };
    let (target_line, target_scroll, forward) = match movement {
        ViewportMove::First => (0, 0, true),
        ViewportMove::Last => (rendered.lines.len() - 1, max_scroll, false),
        ViewportMove::HalfPageDown | ViewportMove::PageDown => (
            current_line
                .saturating_add(amount)
                .min(rendered.lines.len() - 1),
            state.scroll.saturating_add(amount).min(max_scroll),
            true,
        ),
        ViewportMove::HalfPageUp | ViewportMove::PageUp => (
            current_line.saturating_sub(amount),
            state.scroll.saturating_sub(amount),
            false,
        ),
    };
    let selection = navigable_selection_at(sidebar, &rendered.row_indices, target_line, forward);
    if state.selection != selection || state.scroll != target_scroll {
        state.selection = selection;
        state.scroll = target_scroll;
        state.manual_scroll = false;
        state.version = state.version.saturating_add(1);
    }
}

fn navigable_selection_at(
    sidebar: &SidebarView,
    row_indices: &[Option<usize>],
    target_line: usize,
    forward: bool,
) -> Option<String> {
    let candidate = |line: usize| {
        let row_index = row_indices.get(line).copied().flatten()?;
        let row = sidebar.rows.get(row_index)?;
        match row.kind {
            SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Chat => {
                Some(row.id.clone())
            }
            SidebarRowKind::Detail => {
                let pane = pane_instance_from_row_id(&row.id)?;
                let chat = chat_row_id(&pane);
                sidebar
                    .rows
                    .iter()
                    .any(|row| row.id == chat)
                    .then_some(chat)
            }
            SidebarRowKind::Zone => None,
        }
    };
    if let Some(selection) = candidate(target_line) {
        return Some(selection);
    }
    if forward {
        (target_line + 1..row_indices.len())
            .find_map(candidate)
            .or_else(|| (0..target_line).rev().find_map(candidate))
    } else {
        (0..target_line)
            .rev()
            .find_map(candidate)
            .or_else(|| (target_line + 1..row_indices.len()).find_map(candidate))
    }
}

pub(super) fn activate_local_selection(
    context: &ClickContext<'_>,
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
    sidebar: &SidebarView,
    mark_ui: &mut MarkCompleteUi,
) {
    let selected_pane = state
        .selection
        .as_deref()
        .and_then(pane_instance_from_row_id);
    match crate::sidebar::input::activate_selected(state.selection.as_deref(), &sidebar.rows) {
        Some(crate::sidebar::input::SidebarCommand::JumpPane(_)) => {
            if let Some(pane_instance) = selected_pane.filter(|selected| {
                snapshot
                    .panes
                    .iter()
                    .any(|pane| pane.pane_instance == *selected)
            }) {
                dispatch_click_action(context, mark_ui, ClickAction::JumpPane(pane_instance));
            } else {
                state.selection = None;
                state.manual_scroll = false;
                mark_ui.set_toast(
                    "selected pane is stale".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(5),
                );
            }
        }
        Some(crate::sidebar::input::SidebarCommand::ToggleExpand(row_id)) => {
            state.selection = Some(row_id.clone());
            state.manual_scroll = false;
            state.toggle_expanded(&row_id);
        }
        None => {}
    }
}

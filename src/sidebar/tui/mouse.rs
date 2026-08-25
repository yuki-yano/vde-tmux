use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;

use crate::daemon::protocol::v2::ResolvedSnapshot;
#[cfg(test)]
use crate::daemon::session_badge::BadgeState;
use crate::pane_state::{PaneInstance, StateVersion, StoredStateDescriptor};
use crate::sidebar::render::{HeaderAction, header_hit_test};
use crate::sidebar::state::SidebarState;
use crate::sidebar::tree::{SidebarRow, SidebarRowKind, chat_row_id, pane_instance_from_row_id};

use super::effects::{
    MarkCompleteRequest, PanePinRequest, dispatch_click_action, queue_mark_complete,
};
use super::projection::apply_local_sidebar_key;
use super::types::{
    ClickAction, ClickContext, DrawnFrame, MarkCompleteUi, NoticeLevel, SidebarView,
};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RowClickIntent {
    Toggle(String),
    Jump(PaneInstance),
}

pub(super) struct ClickedRenderedRow<'a> {
    row: &'a SidebarRow,
    pub(super) is_first_rendered_line: bool,
}

fn row_click_intent(clicked: &ClickedRenderedRow<'_>) -> Option<RowClickIntent> {
    match clicked.row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            Some(RowClickIntent::Toggle(clicked.row.id.clone()))
        }
        SidebarRowKind::Chat if clicked.is_first_rendered_line => {
            Some(RowClickIntent::Toggle(clicked.row.id.clone()))
        }
        SidebarRowKind::Chat | SidebarRowKind::Detail => {
            pane_instance_from_row_id(&clicked.row.id).map(RowClickIntent::Jump)
        }
        SidebarRowKind::Zone => None,
    }
}

pub(super) fn row_for_click_with_indices<'a>(
    sidebar: &'a SidebarView,
    row: u16,
    header_rows: u16,
    scroll: usize,
    row_indices: &[Option<usize>],
) -> Option<ClickedRenderedRow<'a>> {
    if row < header_rows {
        return None;
    }
    let display_index = usize::from(row - header_rows) + scroll;
    let row_index = row_indices.get(display_index).and_then(|index| *index)?;
    let first_rendered_index = row_indices
        .iter()
        .position(|index| *index == Some(row_index))?;
    Some(ClickedRenderedRow {
        row: sidebar.rows.get(row_index)?,
        is_first_rendered_line: display_index == first_rendered_index,
    })
}

pub(super) fn scroll_mouse_viewport(state: &mut SidebarState, frame: &DrawnFrame, down: bool) {
    let viewport = usize::from(frame.rows_height);
    let max_scroll = frame.row_indices.len().saturating_sub(viewport);
    let current = frame.scroll.min(max_scroll);
    let target = if down {
        current.saturating_add(3).min(max_scroll)
    } else {
        current.saturating_sub(3)
    };
    if target == current {
        return;
    }
    state.scroll = target;
    state.manual_scroll = true;
    state.version = state.version.saturating_add(1);
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ClickPosition {
    pub(super) row: u16,
    pub(super) column: u16,
}

pub(super) fn handle_left_click(
    context: &ClickContext<'_>,
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
    sidebar: &SidebarView,
    mark_ui: &mut MarkCompleteUi,
    frame: &DrawnFrame,
    position: ClickPosition,
) -> Result<()> {
    let ClickPosition { row, column } = position;
    let header = &frame.header;
    if row < header.row_count() {
        match header_hit_test(header, row, column) {
            Some(HeaderAction::ToggleCategoryScope) => {
                apply_local_sidebar_key(state, sidebar, "c");
            }
            Some(HeaderAction::CyclePresentationMode) => {
                apply_local_sidebar_key(state, sidebar, "v");
            }
            Some(HeaderAction::SetFilter(filter)) => {
                apply_local_sidebar_key(state, sidebar, filter.key());
            }
            None => {}
        }
        return Ok(());
    }
    if row >= frame.header_rows + frame.rows_height {
        return Ok(());
    }
    let Some(clicked) = row_for_click_with_indices(
        sidebar,
        row,
        header.row_count(),
        frame.scroll,
        &frame.row_indices,
    ) else {
        return Ok(());
    };
    match row_click_intent(&clicked) {
        Some(RowClickIntent::Toggle(row_id)) => {
            apply_local_sidebar_key(state, sidebar, &format!("toggle:{row_id}"));
        }
        Some(RowClickIntent::Jump(pane_instance))
            if snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_instance == pane_instance) =>
        {
            let chat_id = chat_row_id(&pane_instance);
            if state.selection.as_deref() != Some(chat_id.as_str()) {
                state.selection = Some(chat_id);
                state.manual_scroll = false;
                state.version = state.version.saturating_add(1);
            }
            dispatch_click_action(context, mark_ui, ClickAction::JumpPane(pane_instance));
        }
        Some(RowClickIntent::Jump(_)) | None => {}
    }
    Ok(())
}

pub(super) fn mark_done_target(
    snapshot: &ResolvedSnapshot,
    pane_instance: &PaneInstance,
) -> Option<(PaneInstance, StateVersion)> {
    snapshot.panes.iter().find_map(|pane| {
        if &pane.pane_instance != pane_instance {
            return None;
        }
        let StoredStateDescriptor::Canonical { version } = pane.stored.as_ref()?;
        Some((pane.pane_instance.clone(), version.clone()))
    })
}

fn mark_complete_target_for_selection(
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
) -> Option<(PaneInstance, StateVersion)> {
    let pane = pane_for_selection(sidebar)?;
    mark_done_target(snapshot, &pane)
}

pub(super) fn queue_mark_complete_for_selection(
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    tx: &mpsc::Sender<MarkCompleteRequest>,
    ui: &mut MarkCompleteUi,
) {
    if let Some((pane_instance, expected)) = mark_complete_target_for_selection(snapshot, sidebar) {
        queue_mark_complete(tx, ui, pane_instance, expected);
    }
}

fn pane_for_selection(sidebar: &SidebarView) -> Option<PaneInstance> {
    let selection = sidebar.state.selection.as_deref()?;
    let row = sidebar.rows.iter().find(|row| row.id == selection)?;
    match row.kind {
        SidebarRowKind::Chat | SidebarRowKind::Detail => pane_instance_from_row_id(&row.id),
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Zone => None,
    }
}

pub(super) fn queue_pane_pin_for_selection(
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    tx: &mpsc::Sender<PanePinRequest>,
    ui: &mut MarkCompleteUi,
) {
    let Some(selection) = sidebar.state.selection.as_deref() else {
        ui.set_toast(
            "select a pane to pin".to_string(),
            NoticeLevel::Warning,
            Duration::from_secs(4),
        );
        return;
    };
    let Some(row) = sidebar
        .rows
        .iter()
        .find(|row| row.id == selection && row.kind == SidebarRowKind::Chat)
    else {
        ui.set_toast(
            "select a pane to pin".to_string(),
            NoticeLevel::Warning,
            Duration::from_secs(4),
        );
        return;
    };
    let Some(pane_instance) = pane_instance_from_row_id(&row.id) else {
        ui.set_toast(
            "selected pane is stale".to_string(),
            NoticeLevel::Warning,
            Duration::from_secs(4),
        );
        return;
    };
    if !snapshot
        .panes
        .iter()
        .any(|pane| pane.pane_instance == pane_instance && pane.resolved.is_some())
    {
        ui.set_toast(
            "selected pane is stale".to_string(),
            NoticeLevel::Warning,
            Duration::from_secs(4),
        );
        return;
    }
    if !ui.pin_pending.insert(pane_instance.clone()) {
        return;
    }
    let request = PanePinRequest {
        pane_instance: pane_instance.clone(),
        pinned: !snapshot
            .sidebar_model
            .preferences
            .pinned_panes
            .contains(&pane_instance),
    };
    if tx.send(request).is_err() {
        ui.pin_pending.remove(&pane_instance);
        ui.set_toast(
            "pin worker unavailable".to_string(),
            NoticeLevel::Failure,
            Duration::from_secs(5),
        );
    }
}

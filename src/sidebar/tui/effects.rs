use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;

use crate::pane_state::{PaneInstance, StateVersion};
use crate::sidebar::client::{send_sidebar_jump_v2, send_sidebar_mark_complete_v2};
use crate::sidebar::state::{SidebarPreferenceIntent, StatusFilter};
use crate::sidebar::tree::SidebarRowKind;

use super::types::{
    ClickAction, ClickContext, MarkCompleteUi, NavigationRequest, NoticeLevel, SidebarView,
};

#[cfg(test)]
mod tests;

pub(super) struct MarkCompleteRequest {
    pub(super) pane_instance: PaneInstance,
    pub(super) expected: StateVersion,
}

pub(super) struct MarkCompleteResult {
    pub(super) pane_instance: PaneInstance,
    pub(super) result: Result<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PanePinRequest {
    pub(super) pane_instance: PaneInstance,
    pub(super) pinned: bool,
}

pub(super) struct PanePinResult {
    pub(super) pane_instance: PaneInstance,
    pub(super) pinned: bool,
    pub(super) result: Result<()>,
}

pub(super) struct PreferenceIntentRequest {
    pub(super) intent: SidebarPreferenceIntent,
}

pub(super) struct PreferenceIntentResult {
    pub(super) intent: SidebarPreferenceIntent,
    pub(super) result: Result<()>,
}

pub(super) struct CategoryIntentRequest {
    pub(super) intent: crate::category::CategoryIntent,
    pub(super) dialog_request_id: Option<u64>,
}

pub(super) struct CategoryIntentResult {
    pub(super) dialog_request_id: Option<u64>,
    pub(super) result: Result<()>,
}

pub(super) struct NavigationResult {
    pub(super) request: NavigationRequest,
    pub(super) result: Result<()>,
}

pub(super) fn spawn_mark_complete_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<MarkCompleteRequest>,
    tx: mpsc::Sender<MarkCompleteResult>,
) {
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let pane_instance = request.pane_instance.clone();
            let result = send_sidebar_mark_complete_v2(
                &socket,
                &server_identity,
                request.pane_instance,
                request.expected,
            );
            if tx
                .send(MarkCompleteResult {
                    pane_instance,
                    result,
                })
                .is_err()
            {
                return;
            }
        }
    });
}

pub(super) fn spawn_pane_pin_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<PanePinRequest>,
    tx: mpsc::Sender<PanePinResult>,
) {
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let pane_instance = request.pane_instance.clone();
            let pinned = request.pinned;
            let result = crate::sidebar::client::send_sidebar_preference_intent_v2(
                &socket,
                &server_identity,
                SidebarPreferenceIntent::SetPanePinned {
                    pane_instance: request.pane_instance,
                    pinned,
                },
            );
            if tx
                .send(PanePinResult {
                    pane_instance,
                    pinned,
                    result,
                })
                .is_err()
            {
                return;
            }
        }
    });
}

pub(super) fn spawn_preference_intent_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<PreferenceIntentRequest>,
    tx: mpsc::Sender<PreferenceIntentResult>,
) {
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let intent = request.intent;
            let result = crate::sidebar::client::send_sidebar_preference_intent_v2(
                &socket,
                &server_identity,
                intent.clone(),
            );
            let failed = result.is_err();
            if tx.send(PreferenceIntentResult { intent, result }).is_err() {
                return;
            }
            if failed {
                while rx.try_recv().is_ok() {}
            }
        }
    });
}

pub(super) fn spawn_category_intent_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<CategoryIntentRequest>,
    tx: mpsc::Sender<CategoryIntentResult>,
) {
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let dialog_request_id = request.dialog_request_id;
            let result = crate::sidebar::client::send_category_intent_v2(
                &socket,
                &server_identity,
                request.intent,
            )
            .map(|_| ());
            let failed = result.is_err();
            if tx
                .send(CategoryIntentResult {
                    dialog_request_id,
                    result,
                })
                .is_err()
            {
                return;
            }
            if failed {
                while let Ok(dropped) = rx.try_recv() {
                    if tx
                        .send(CategoryIntentResult {
                            dialog_request_id: dropped.dialog_request_id,
                            result: Err(anyhow::anyhow!(
                                "category request cancelled after an earlier save failure"
                            )),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
}

pub(super) fn spawn_navigation_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<NavigationRequest>,
    tx: mpsc::Sender<NavigationResult>,
) {
    std::thread::spawn(move || {
        while let Ok(mut request) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                request = newer;
            }
            let result = crate::sidebar::client::send_sidebar_navigation_v2(
                &socket,
                &server_identity,
                request.selection.clone(),
                request.scroll,
                request.manual_scroll,
            );
            let failed = result.is_err();
            if tx.send(NavigationResult { request, result }).is_err() {
                return;
            }
            if failed {
                while rx.try_recv().is_ok() {}
            }
        }
    });
}

pub(super) fn queue_reorder(
    sidebar: &SidebarView,
    up: bool,
    preference_tx: &mpsc::Sender<PreferenceIntentRequest>,
    category_tx: &mpsc::Sender<CategoryIntentRequest>,
    ui: &mut MarkCompleteUi,
) {
    if sidebar.state.filter != StatusFilter::All {
        ui.set_toast(
            "reorder requires the All filter".to_string(),
            NoticeLevel::Warning,
            Duration::from_secs(4),
        );
        return;
    }
    let Some(selection) = sidebar.state.selection.as_deref() else {
        return;
    };
    let Some(selected) = sidebar.rows.iter().find(|row| row.id == selection) else {
        return;
    };
    let direction = if up {
        crate::sidebar::state::MoveDirection::Up
    } else {
        crate::sidebar::state::MoveDirection::Down
    };
    let preference_intent = match selected.kind {
        SidebarRowKind::Chat => {
            let chats = sidebar
                .rows
                .iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .filter_map(|row| row.pane_id.as_ref())
                .collect::<Vec<_>>();
            let Some(pane_id) = selected.pane_id.as_ref() else {
                return;
            };
            let Some(index) = chats.iter().position(|candidate| *candidate == pane_id) else {
                return;
            };
            let neighbor = if up {
                index.checked_sub(1).and_then(|index| chats.get(index))
            } else {
                chats.get(index + 1)
            };
            let Some(neighbor) = neighbor else { return };
            Some(SidebarPreferenceIntent::MoveChat {
                pane_id: pane_id.clone(),
                neighbor_pane_id: (*neighbor).clone(),
                direction,
            })
        }
        _ => None,
    };
    if let Some(intent) = preference_intent {
        if preference_tx
            .send(PreferenceIntentRequest { intent })
            .is_err()
        {
            ui.set_toast(
                "preference worker unavailable".to_string(),
                NoticeLevel::Failure,
                Duration::from_secs(5),
            );
        } else {
            ui.set_toast(
                "saving order...".to_string(),
                NoticeLevel::Progress,
                Duration::from_secs(3),
            );
        }
        return;
    }

    let category_intent = match selected.kind {
        SidebarRowKind::Category => {
            let Some(category) = category_name_from_row_id(&selected.id) else {
                return;
            };
            let categories = sidebar
                .rows
                .iter()
                .filter(|row| row.kind == SidebarRowKind::Category)
                .filter_map(|row| category_name_from_row_id(&row.id))
                .collect::<Vec<_>>();
            let Some(index) = categories
                .iter()
                .position(|candidate| candidate == &category)
            else {
                return;
            };
            let neighbor = if up {
                index.checked_sub(1).and_then(|index| categories.get(index))
            } else {
                categories.get(index + 1)
            };
            let Some(neighbor) = neighbor else { return };
            crate::category::CategoryIntent::MoveCategory {
                category,
                neighbor: neighbor.clone(),
                direction,
            }
        }
        SidebarRowKind::Repo => {
            let repos = sidebar
                .rows
                .iter()
                .filter(|row| row.kind == SidebarRowKind::Repo)
                .filter_map(|row| category_repo_from_row_id(&row.id))
                .filter(|(category, _)| {
                    category_repo_from_row_id(&selected.id)
                        .is_some_and(|(selected_category, _)| selected_category == *category)
                })
                .collect::<Vec<_>>();
            let Some((category, repo)) = category_repo_from_row_id(&selected.id) else {
                return;
            };
            let Some(index) = repos.iter().position(|(_, candidate)| *candidate == repo) else {
                return;
            };
            let neighbor = if up {
                index.checked_sub(1).and_then(|index| repos.get(index))
            } else {
                repos.get(index + 1)
            };
            let Some(neighbor) = neighbor else { return };
            crate::category::CategoryIntent::MoveRepo {
                repo,
                neighbor: neighbor.1.clone(),
                category,
                direction,
            }
        }
        _ => return,
    };
    if category_tx
        .send(CategoryIntentRequest {
            intent: category_intent,
            dialog_request_id: None,
        })
        .is_err()
    {
        ui.set_toast(
            "category worker unavailable".to_string(),
            NoticeLevel::Failure,
            Duration::from_secs(5),
        );
    } else {
        ui.set_toast(
            "saving order...".to_string(),
            NoticeLevel::Progress,
            Duration::from_secs(3),
        );
    }
}

pub(super) fn dispatch_click_action(
    context: &ClickContext<'_>,
    mark_ui: &mut MarkCompleteUi,
    action: ClickAction,
) {
    match action {
        ClickAction::JumpPane(pane_instance) => {
            let result = send_sidebar_jump_v2(
                context.socket,
                context.server_identity,
                pane_instance,
                context.source_pane.clone(),
            );
            let (message, level, duration) = match result {
                Ok(()) => (
                    "jumped to pane".to_string(),
                    NoticeLevel::Success,
                    Duration::from_secs(3),
                ),
                Err(error) => (
                    format!("jump failed: {error}"),
                    NoticeLevel::Failure,
                    Duration::from_secs(5),
                ),
            };
            mark_ui.set_toast(message, level, duration);
        }
    }
}

pub(super) fn category_name_from_row_id(id: &str) -> Option<crate::category::CategoryName> {
    let name = id.strip_prefix("category::")?;
    if name == crate::category::UNCATEGORIZED {
        Some(crate::category::CategoryName::uncategorized())
    } else {
        crate::category::CategoryName::parse(name).ok()
    }
}

pub(super) fn category_repo_from_row_id(
    id: &str,
) -> Option<(crate::category::CategoryName, crate::category::RepoKey)> {
    let rest = id.strip_prefix("repo::")?;
    let split = rest.find("::git:").or_else(|| rest.find("::path:"))?;
    let category = &rest[..split];
    let repo = &rest[split + 2..];
    let category = if category == crate::category::UNCATEGORIZED {
        crate::category::CategoryName::uncategorized()
    } else {
        crate::category::CategoryName::parse(category).ok()?
    };
    Some((category, crate::category::RepoKey::parse(repo).ok()?))
}

pub(super) fn queue_mark_complete(
    tx: &mpsc::Sender<MarkCompleteRequest>,
    ui: &mut MarkCompleteUi,
    pane_instance: PaneInstance,
    expected: StateVersion,
) {
    if !ui.pending.insert(pane_instance.clone()) {
        return;
    }
    if tx
        .send(MarkCompleteRequest {
            pane_instance: pane_instance.clone(),
            expected,
        })
        .is_err()
    {
        ui.pending.remove(&pane_instance);
        ui.set_toast(
            "mark complete worker unavailable".to_string(),
            NoticeLevel::Failure,
            Duration::from_secs(5),
        );
    }
}

pub(super) fn drain_mark_complete_results(
    rx: &mpsc::Receiver<MarkCompleteResult>,
    ui: &mut MarkCompleteUi,
) -> bool {
    let mut changed = false;
    while let Ok(result) = rx.try_recv() {
        changed = true;
        ui.pending.remove(&result.pane_instance);
        let (message, level, duration) = match result.result {
            Ok(()) => (
                "marked complete".to_string(),
                NoticeLevel::Success,
                Duration::from_secs(3),
            ),
            Err(error) if error.to_string().contains("Stale") => (
                "state changed; retry mark complete".to_string(),
                NoticeLevel::Warning,
                Duration::from_secs(5),
            ),
            Err(error) => (
                format!("mark complete failed: {error}"),
                NoticeLevel::Failure,
                Duration::from_secs(5),
            ),
        };
        ui.set_toast(message, level, duration);
    }
    changed
}

pub(super) fn drain_pane_pin_results(
    rx: &mpsc::Receiver<PanePinResult>,
    ui: &mut MarkCompleteUi,
) -> bool {
    let mut changed = false;
    while let Ok(result) = rx.try_recv() {
        changed = true;
        ui.pin_pending.remove(&result.pane_instance);
        let (message, level, duration) = match result.result {
            Ok(()) if result.pinned => (
                "pinned pane".to_string(),
                NoticeLevel::Success,
                Duration::from_secs(3),
            ),
            Ok(()) => (
                "unpinned pane".to_string(),
                NoticeLevel::Success,
                Duration::from_secs(3),
            ),
            Err(error) => (
                format!("pin failed: {error}"),
                NoticeLevel::Failure,
                Duration::from_secs(5),
            ),
        };
        ui.set_toast(message, level, duration);
    }
    changed
}

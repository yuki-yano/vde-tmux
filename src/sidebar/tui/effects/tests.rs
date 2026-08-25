use super::super::test_support::*;
use super::*;
use crate::config::Config;
use crate::pane_state::{StateId, StoredStateDescriptor};
use crate::sidebar::state::{CategoryScope, PresentationMode, SidebarState};
use crate::sidebar::tree::chat_row_id;
use crate::sidebar::tui::mouse::{mark_done_target, queue_pane_pin_for_selection};
use crate::sidebar::tui::projection::project_view;

#[test]
fn category_and_repo_reorder_emit_category_state_intents() {
    let (preference_tx, _preference_rx) = mpsc::channel();
    let (category_tx, category_rx) = mpsc::channel();
    let mut ui = MarkCompleteUi::default();
    let category_view = SidebarView {
        state: SidebarState {
            selection: Some("category::work".to_string()),
            ..SidebarState::default()
        },
        rows: vec![
            structural_row("category::work", SidebarRowKind::Category),
            structural_row("category::public", SidebarRowKind::Category),
        ],
        ..SidebarView::default()
    };

    queue_reorder(&category_view, false, &preference_tx, &category_tx, &mut ui);
    assert_eq!(
        category_rx.recv().unwrap().intent,
        crate::category::CategoryIntent::MoveCategory {
            category: crate::category::CategoryName::parse("work").unwrap(),
            neighbor: crate::category::CategoryName::parse("public").unwrap(),
            direction: crate::sidebar::state::MoveDirection::Down,
        }
    );

    let repo_view = SidebarView {
        state: SidebarState {
            selection: Some("repo::work::path:/repo/a".to_string()),
            ..SidebarState::default()
        },
        rows: vec![
            structural_row("repo::work::path:/repo/a", SidebarRowKind::Repo),
            structural_row("repo::work::path:/repo/b", SidebarRowKind::Repo),
            structural_row("repo::public::path:/repo/c", SidebarRowKind::Repo),
        ],
        ..SidebarView::default()
    };
    queue_reorder(&repo_view, false, &preference_tx, &category_tx, &mut ui);
    assert_eq!(
        category_rx.recv().unwrap().intent,
        crate::category::CategoryIntent::MoveRepo {
            repo: crate::category::RepoKey::path("/repo/a"),
            neighbor: crate::category::RepoKey::path("/repo/b"),
            category: crate::category::CategoryName::parse("work").unwrap(),
            direction: crate::sidebar::state::MoveDirection::Down,
        }
    );
}

#[test]
fn preference_sender_drops_queued_intents_after_connection_failure() {
    let socket = std::env::temp_dir().join(format!(
        "vde-missing-preference-socket-{}-{}",
        std::process::id(),
        crate::pane_state::EventId::generate().unwrap().as_str()
    ));
    let (request_tx, request_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    request_tx
        .send(PreferenceIntentRequest {
            intent: SidebarPreferenceIntent::SetDefaultFilter {
                filter: StatusFilter::DoneOnly,
            },
        })
        .unwrap();
    request_tx
        .send(PreferenceIntentRequest {
            intent: SidebarPreferenceIntent::SetDefaultPresentationMode {
                presentation_mode: PresentationMode::Flat,
            },
        })
        .unwrap();
    drop(request_tx);

    spawn_preference_intent_worker(socket, "missing".to_string(), request_rx, result_tx);

    assert!(
        result_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .result
            .is_err()
    );
    assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn mark_complete_never_retargets_reused_pane_id() {
    let mut snapshot = snapshot(11);
    snapshot.panes[0].stored = Some(StoredStateDescriptor::Canonical {
        version: StateVersion {
            state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
            agent_epoch: 1,
            revision: 1,
        },
    });
    let stale = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 10,
    };
    let current = PaneInstance {
        pane_id: "%1".to_string(),
        pane_pid: 11,
    };

    assert!(mark_done_target(&snapshot, &stale).is_none());
    assert!(mark_done_target(&snapshot, &current).is_some());
}

#[test]
fn pane_pin_accepts_read_chat_in_any_view_and_toggles_persisted_state() {
    let target = PaneInstance {
        pane_id: "%7".to_string(),
        pane_pid: 70,
    };
    let presentation = resolved_pane("%7", 70, "$1");
    let mut snapshot = snapshot(70);
    snapshot.panes = vec![presentation];
    let state = SidebarState {
        category_scope: CategoryScope::All,
        presentation_mode: PresentationMode::Flat,
        selection: Some(chat_row_id(&target)),
        ..SidebarState::default()
    };
    let sidebar = project_view(&snapshot, &Config::default(), &state);
    assert!(
        sidebar
            .rows
            .iter()
            .any(|row| row.id == chat_row_id(&target) && row.kind == SidebarRowKind::Chat)
    );
    let (tx, rx) = mpsc::channel();
    let mut ui = MarkCompleteUi::default();

    queue_pane_pin_for_selection(&snapshot, &sidebar, &tx, &mut ui);

    assert_eq!(
        rx.try_recv().unwrap(),
        PanePinRequest {
            pane_instance: target.clone(),
            pinned: true,
        }
    );
    assert!(ui.pin_pending.contains(&target));

    snapshot
        .sidebar_model
        .preferences
        .pinned_panes
        .insert(target.clone());
    let sidebar = project_view(&snapshot, &Config::default(), &state);
    let mut unpin_ui = MarkCompleteUi::default();
    queue_pane_pin_for_selection(&snapshot, &sidebar, &tx, &mut unpin_ui);
    assert_eq!(
        rx.try_recv().unwrap(),
        PanePinRequest {
            pane_instance: target,
            pinned: false,
        }
    );
}

#[test]
fn pane_pin_results_report_success_and_failure() {
    let target = PaneInstance {
        pane_id: "%7".to_string(),
        pane_pid: 70,
    };
    let (tx, rx) = mpsc::channel();
    let mut ui = MarkCompleteUi::default();
    ui.pin_pending.insert(target.clone());
    tx.send(PanePinResult {
        pane_instance: target.clone(),
        pinned: true,
        result: Ok(()),
    })
    .unwrap();
    assert!(drain_pane_pin_results(&rx, &mut ui));
    assert_eq!(ui.notice().unwrap().message, "pinned pane");
    assert!(!ui.pin_pending.contains(&target));

    ui.pin_pending.insert(target.clone());
    tx.send(PanePinResult {
        pane_instance: target.clone(),
        pinned: false,
        result: Err(anyhow::anyhow!("persistence unavailable")),
    })
    .unwrap();
    assert!(drain_pane_pin_results(&rx, &mut ui));
    assert_eq!(
        ui.notice().unwrap().message,
        "pin failed: persistence unavailable"
    );
    assert!(!ui.pin_pending.contains(&target));
}

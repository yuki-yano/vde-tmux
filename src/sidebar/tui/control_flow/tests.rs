use super::super::test_support::*;
use super::*;
use crate::daemon::protocol::v2::{DaemonDiagnostic, ErrorCode, SessionLinkPresentation};
use crate::sidebar::state::StatusFilter;
use std::collections::BTreeSet;

#[test]
fn render_gate_draws_once_per_second_when_visible_and_idle() {
    let mut gate = RenderGate::new();

    assert!(gate.take_draw_decision(100, true));
    for _ in 0..20 {
        assert!(!gate.take_draw_decision(100, true));
    }
    // The second boundary refreshes elapsed labels exactly once.
    assert!(gate.take_draw_decision(101, true));
    assert!(!gate.take_draw_decision(101, true));
    gate.mark_dirty();
    assert!(gate.take_draw_decision(101, true));
    assert!(!gate.take_draw_decision(101, true));
}

#[test]
fn render_gate_placeholder_only_redraws_on_marked_changes() {
    let mut gate = RenderGate::new();

    // The connection placeholder is drawn once and has no elapsed labels,
    // so second boundaries must not redraw it.
    assert!(gate.take_draw_decision(100, false));
    assert!(!gate.take_draw_decision(101, false));
    assert!(!gate.take_draw_decision(102, false));
    gate.mark_dirty();
    assert!(gate.take_draw_decision(102, false));
}

#[test]
fn render_gate_toast_transitions_mark_dirty_once() {
    let mut gate = RenderGate::new();
    let _ = gate.take_draw_decision(100, true);

    gate.note_toast(None);
    assert!(!gate.take_draw_decision(100, true));
    gate.note_toast(Some(Notice {
        message: "saved",
        level: NoticeLevel::Success,
    }));
    assert!(gate.take_draw_decision(100, true));
    gate.note_toast(Some(Notice {
        message: "saved",
        level: NoticeLevel::Success,
    }));
    assert!(!gate.take_draw_decision(100, true));
    gate.note_toast(None);
    assert!(gate.take_draw_decision(100, true));
}

#[test]
fn only_visible_sidebar_uses_the_elapsed_clock() {
    let mut gates = (0..20).map(|_| RenderGate::new()).collect::<Vec<_>>();
    for gate in &mut gates {
        assert!(gate.take_draw_decision(1000, true));
    }

    let mut draws = 0;
    for tick in 0..20 {
        let now = if tick < 10 { 1000 } else { 1001 };
        for (index, gate) in gates.iter_mut().enumerate() {
            if gate.take_draw_decision(now, index == 0) {
                draws += 1;
            }
        }
    }

    assert_eq!(draws, 1);

    // Explicit state changes still redraw hidden sidebars immediately.
    gates[1].mark_dirty();
    assert!(gates[1].take_draw_decision(1001, false));
}

#[test]
fn sidebar_elapsed_clock_is_visible_only_in_an_attached_current_window() {
    let sidebar = PaneInstance {
        pane_id: "%9".to_string(),
        pane_pid: 90,
    };
    let mut sidebar_pane = pane(90);
    sidebar_pane.pane_instance = sidebar.clone();
    sidebar_pane.session_links = vec![
        SessionLinkPresentation {
            session_id: "$1".to_string(),
            session_name: "main".to_string(),
            window_index: 0,
            window_active: false,
            window_last: true,
        },
        SessionLinkPresentation {
            session_id: "$2".to_string(),
            session_name: "linked".to_string(),
            window_index: 3,
            window_active: true,
            window_last: false,
        },
    ];
    let mut snapshot = ResolvedSnapshot {
        panes: vec![sidebar_pane],
        ..snapshot(10)
    };

    snapshot.sidebar_model.active_sessions = BTreeSet::from(["$1".to_string()]);
    assert!(!sidebar_elapsed_clock_visible(&snapshot, &sidebar));

    snapshot.sidebar_model.active_sessions = BTreeSet::from(["$2".to_string()]);
    assert!(sidebar_elapsed_clock_visible(&snapshot, &sidebar));

    snapshot.sidebar_model.active_sessions.clear();
    assert!(!sidebar_elapsed_clock_visible(&snapshot, &sidebar));
}

#[test]
fn same_revision_snapshot_update_does_not_mark_dirty() {
    let (tx, rx) = mpsc::channel();
    let mut current = None;
    let mut connection = ConnectionState::Connecting;

    tx.send(SubscriptionUpdate::Connected(Box::new(snapshot(10))))
        .unwrap();
    assert!(drain_snapshot_updates(&rx, &mut current, &mut connection));

    tx.send(SubscriptionUpdate::Connected(Box::new(snapshot(10))))
        .unwrap();
    assert!(!drain_snapshot_updates(&rx, &mut current, &mut connection));

    let mut newer = snapshot(10);
    newer.snapshot_revision = 11;
    tx.send(SubscriptionUpdate::Connected(Box::new(newer)))
        .unwrap();
    assert!(drain_snapshot_updates(&rx, &mut current, &mut connection));
}

#[test]
fn config_change_update_requests_tui_reinitialization() {
    let (tx, rx) = mpsc::channel();
    let mut current = Some(snapshot(10));
    let mut connection = ConnectionState::Connected;

    tx.send(SubscriptionUpdate::ConfigChanged {
        active_config_hash: "new-config".to_string(),
    })
    .unwrap();
    drop(tx);

    assert!(drain_snapshot_updates(&rx, &mut current, &mut connection));
    assert_eq!(
        connection,
        ConnectionState::ConfigChanged("new-config".to_string())
    );
    assert_eq!(current.unwrap().snapshot_revision, 9);
}

#[test]
fn focus_message_rejects_reused_pane_id_with_different_pid() {
    let snapshot = snapshot(10);
    let mut state = SidebarState::default();

    assert!(!apply_focus_message(
        &snapshot,
        &Config::default(),
        &mut state,
        PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 11,
        },
        "$1",
    ));
    assert!(state.return_target.is_none());
    assert!(apply_focus_message(
        &snapshot,
        &Config::default(),
        &mut state,
        PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        },
        "$1",
    ));
}

#[test]
fn reconnect_updates_preserve_last_snapshot_and_local_state() {
    let (tx, rx) = mpsc::channel();
    let mut current = Some(snapshot(10));
    let mut connection = ConnectionState::Connected;
    let mut state = SidebarState {
        filter: StatusFilter::DoneOnly,
        selection: Some("chat::%1".to_string()),
        ..SidebarState::default()
    };
    tx.send(SubscriptionUpdate::Disconnected).unwrap();
    tx.send(SubscriptionUpdate::Connecting).unwrap();

    drain_snapshot_updates(&rx, &mut current, &mut connection);

    assert_eq!(current.as_ref().unwrap().snapshot_revision, 9);
    assert_eq!(state.filter, StatusFilter::DoneOnly);
    assert_eq!(state.selection.as_deref(), Some("chat::%1"));
    assert_eq!(connection, ConnectionState::Connecting);
    state.scroll = 3;
}

#[test]
fn historical_diagnostic_snapshot_is_adopted_without_degrading_connection() {
    let (tx, rx) = mpsc::channel();
    let mut current = Some(snapshot(10));
    let mut connection = ConnectionState::Connected;
    let mut next = snapshot(11);
    next.snapshot_revision = 10;
    next.diagnostics.push(DaemonDiagnostic {
        code: ErrorCode::PersistFailed,
        message: "disk failed".to_string(),
        pane_instance: None,
        event_id: None,
    });
    tx.send(SubscriptionUpdate::Connected(Box::new(next)))
        .unwrap();

    drain_snapshot_updates(&rx, &mut current, &mut connection);

    assert_eq!(current.unwrap().snapshot_revision, 10);
    assert_eq!(connection, ConnectionState::Connected);
}

#[test]
fn current_hook_collision_degrades_connection_until_a_healthy_snapshot_arrives() {
    let (tx, rx) = mpsc::channel();
    let mut current = None;
    let mut connection = ConnectionState::Connecting;
    let mut degraded = snapshot(10);
    degraded.diagnostics.push(DaemonDiagnostic {
        code: ErrorCode::HookCollision,
        message: "hook ownership collision".to_string(),
        pane_instance: None,
        event_id: None,
    });
    tx.send(SubscriptionUpdate::Connected(Box::new(degraded)))
        .unwrap();
    drain_snapshot_updates(&rx, &mut current, &mut connection);
    assert_eq!(
        connection,
        ConnectionState::Degraded("hook ownership collision".to_string())
    );

    tx.send(SubscriptionUpdate::Connected(Box::new(snapshot(11))))
        .unwrap();
    drain_snapshot_updates(&rx, &mut current, &mut connection);
    assert_eq!(connection, ConnectionState::Connected);
}

use super::super::test_support::*;
use super::*;
use crate::hook::RollupLevel;
use crate::sidebar::tree::BadgeCounts;

#[test]
fn local_view_changes_do_not_change_daemon_snapshot_revision() {
    let snapshot = snapshot(10);
    let mut state = SidebarState::default();
    let view = project_view(&snapshot, &Config::default(), &state);

    apply_local_sidebar_key(&mut state, &view, "v");
    apply_local_sidebar_key(&mut state, &view, "tab");

    assert_eq!(snapshot.snapshot_revision, 9);
    assert_ne!(state, SidebarState::default());
}

#[test]
fn filter_cycles_in_both_directions_and_skips_empty_filters() {
    let mut state = SidebarState::default();
    let view = SidebarView {
        counts: BadgeCounts {
            total: 6,
            blocked: 0,
            limited: 0,
            working: 2,
            done: 0,
            idle: 4,
        },
        ..SidebarView::default()
    };

    apply_local_sidebar_key(&mut state, &view, "tab");
    assert_eq!(state.filter, StatusFilter::WorkingOnly);
    apply_local_sidebar_key(&mut state, &view, "tab");
    assert_eq!(state.filter, StatusFilter::IdleOnly);
    apply_local_sidebar_key(&mut state, &view, "tab");
    assert_eq!(state.filter, StatusFilter::All);

    apply_local_sidebar_key(&mut state, &view, "backtab");
    assert_eq!(state.filter, StatusFilter::IdleOnly);
    apply_local_sidebar_key(&mut state, &view, "backtab");
    assert_eq!(state.filter, StatusFilter::WorkingOnly);
    apply_local_sidebar_key(&mut state, &view, "backtab");
    assert_eq!(state.filter, StatusFilter::All);
}

#[test]
fn attention_navigation_targets_only_blocked_agents() {
    let row = |pane_id: &str, pane_pid: u32, badge_state: BadgeState| SidebarRow {
        id: chat_row_id(&PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        }),
        kind: SidebarRowKind::Chat,
        depth: 0,
        label: pane_id.to_string(),
        chat_count: 1,
        rollup: RollupLevel::Idle,
        badge_state: Some(badge_state),
        expanded: false,
        pane_id: Some(pane_id.to_string()),
        git: None,
        active: false,
        meta: None,
    };
    let blocked = row("%1", 101, BadgeState::Blocked);
    let done = row("%2", 202, BadgeState::Done);
    let working = row("%3", 303, BadgeState::Working);
    let sidebar = SidebarView {
        rows: vec![blocked.clone(), working, done.clone()],
        ..SidebarView::default()
    };
    let mut state = SidebarState {
        selection: Some(done.id),
        ..SidebarState::default()
    };

    apply_local_sidebar_key(&mut state, &sidebar, "n");
    assert_eq!(state.selection, Some(blocked.id.clone()));
    apply_local_sidebar_key(&mut state, &sidebar, "n");
    assert_eq!(state.selection, Some(blocked.id.clone()));
    apply_local_sidebar_key(&mut state, &sidebar, "N");
    assert_eq!(state.selection, Some(blocked.id));
}

#[test]
fn non_agent_origin_selects_first_focusable_agent_in_the_same_session() {
    let mut non_agent = pane(90);
    non_agent.pane_instance.pane_id = "%9".to_string();
    let agent = resolved_pane("%2", 20, "$1");
    let snapshot = ResolvedSnapshot {
        panes: vec![non_agent, agent.clone()],
        ..snapshot(10)
    };
    let mut state = SidebarState::default();

    seed_initial_sidebar_context(
        &snapshot,
        &Config::default(),
        &mut state,
        Some("%9"),
        Some(90),
        Some("$1"),
    );

    assert_eq!(state.selection, Some(chat_row_id(&agent.pane_instance)));
    assert_eq!(
        state.return_target,
        Some(PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 90,
        })
    );
}

#[test]
fn same_session_fallback_uses_the_first_agent_in_current_render_order() {
    let mut non_agent = pane(90);
    non_agent.pane_instance.pane_id = "%9".to_string();
    let snapshot = ResolvedSnapshot {
        panes: vec![
            non_agent,
            resolved_pane("%3", 30, "$1"),
            resolved_pane("%2", 20, "$1"),
        ],
        ..snapshot(10)
    };
    let mut state = SidebarState {
        current_category: Some(crate::category::UNCATEGORIZED.to_string()),
        ..SidebarState::default()
    };
    let expected = project_view(&snapshot, &Config::default(), &state)
        .rows
        .into_iter()
        .find(|row| row.kind == SidebarRowKind::Chat)
        .map(|row| row.id)
        .expect("two focusable agent rows must render");

    seed_initial_sidebar_context(
        &snapshot,
        &Config::default(),
        &mut state,
        Some("%9"),
        Some(90),
        Some("$1"),
    );

    assert_eq!(state.selection, Some(expected));
}

#[test]
fn current_category_prefers_linked_session_and_re_resolves_pane_placement() {
    let mut snapshot = snapshot(90);
    let identity = crate::category::RepoIdentity {
        key: crate::category::RepoKey::path("/tmp"),
        rule_path: "/tmp".to_string(),
        display_name: "tmp".to_string(),
    };
    let model_for = |category: &str| {
        let mut config = Config::default();
        config.categories.rules.push(crate::config::CategoryRule {
            category: category.to_string(),
            path_patterns: vec!["/tmp".to_string()],
        });
        crate::category::EffectiveCategoryModel::build(
            &config,
            &crate::category::CategoryState::default(),
            [identity.clone()],
        )
        .unwrap()
    };
    snapshot
        .sidebar_model
        .repo_identities
        .insert("/tmp".to_string(), identity.clone());
    snapshot.sidebar_model.categories = model_for("pane-category");
    snapshot
        .sidebar_model
        .session_categories
        .insert("$1".to_string(), "session-category".to_string());
    let target = snapshot.panes[0].pane_instance.clone();
    let mut state = SidebarState::default();

    assert!(set_sidebar_context(&snapshot, &mut state, &target, "$1"));
    assert_eq!(state.current_category.as_deref(), Some("session-category"));

    snapshot.sidebar_model.session_categories.clear();
    assert!(refresh_current_category(&snapshot, &mut state));
    assert_eq!(state.current_category.as_deref(), Some("pane-category"));

    snapshot.sidebar_model.categories = model_for("moved-category");
    assert!(refresh_current_category(&snapshot, &mut state));
    assert_eq!(state.current_category.as_deref(), Some("moved-category"));

    let before = state.clone();
    assert!(!set_sidebar_context(
        &snapshot,
        &mut state,
        &target,
        "$not-linked"
    ));
    assert_eq!(state, before);
}

#[test]
fn current_agent_tracks_logical_focus_and_clears_on_non_agent_panes() {
    let first = resolved_pane("%1", 10, "$1");
    let mut second = resolved_pane("%2", 20, "$2");
    second.active = false;
    second.focused = false;
    second.session_links[0].window_active = false;
    let mut non_agent = pane(90);
    non_agent.pane_instance.pane_id = "%9".to_string();
    non_agent.active = false;
    non_agent.focused = false;
    let mut snapshot = ResolvedSnapshot {
        panes: vec![first.clone(), second.clone(), non_agent.clone()],
        ..snapshot(10)
    };
    snapshot.sidebar_model.active_sessions = BTreeSet::from(["$1".to_string()]);
    let mut state = SidebarState::default();

    assert!(refresh_current_agents(&snapshot, &mut state));
    assert_eq!(
        state.current_agents,
        BTreeSet::from([first.pane_instance.clone()])
    );
    assert!(!refresh_current_agents(&snapshot, &mut state));

    snapshot.panes[1].focused = true;
    snapshot.panes[1].session_links[0].window_active = true;
    snapshot.sidebar_model.active_sessions = BTreeSet::from(["$1".to_string(), "$2".to_string()]);
    assert!(refresh_current_agents(&snapshot, &mut state));
    assert_eq!(
        state.current_agents,
        BTreeSet::from([first.pane_instance.clone(), second.pane_instance.clone()])
    );

    snapshot.panes[0].focused = false;
    snapshot.panes[1].focused = false;
    snapshot.panes[2].focused = true;
    snapshot.sidebar_model.active_sessions = BTreeSet::from(["$1".to_string()]);
    assert!(refresh_current_agents(&snapshot, &mut state));
    assert!(state.current_agents.is_empty());

    snapshot.panes[1].focused = true;
    snapshot.panes[1].session_links[0].window_active = true;
    snapshot.panes[2].focused = false;
    snapshot.sidebar_model.active_sessions = BTreeSet::from(["$2".to_string()]);
    assert!(refresh_current_agents(&snapshot, &mut state));
    assert_eq!(
        state.current_agents,
        BTreeSet::from([second.pane_instance.clone()])
    );

    snapshot.panes[1]
        .resolved
        .as_mut()
        .expect("second pane is an agent")
        .canonical
        .agent_present = false;
    assert!(refresh_current_agents(&snapshot, &mut state));
    assert!(state.current_agents.is_empty());
}

#[test]
fn direct_agent_match_wins_over_same_session_fallback() {
    let first = resolved_pane("%2", 20, "$1");
    let direct = resolved_pane("%3", 30, "$1");
    let snapshot = ResolvedSnapshot {
        panes: vec![first, direct.clone()],
        ..snapshot(10)
    };
    let mut state = SidebarState::default();

    seed_initial_sidebar_context(
        &snapshot,
        &Config::default(),
        &mut state,
        Some("%3"),
        Some(30),
        Some("$1"),
    );

    assert_eq!(state.selection, Some(chat_row_id(&direct.pane_instance)));
}

#[test]
fn agent_navigation_targets_only_chat_rows_and_stops_at_edges() {
    let chat = |pane_id: &str, pane_pid: u32| SidebarRow {
        id: chat_row_id(&PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        }),
        kind: SidebarRowKind::Chat,
        depth: 0,
        label: pane_id.to_string(),
        chat_count: 1,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: false,
        pane_id: Some(pane_id.to_string()),
        git: None,
        active: false,
        meta: None,
    };
    let first = chat("%1", 101);
    let second = chat("%2", 202);
    let rows = vec![
        SidebarRow {
            id: "zone::running".to_string(),
            kind: SidebarRowKind::Zone,
            depth: 0,
            label: "RUNNING".to_string(),
            chat_count: 2,
            rollup: RollupLevel::Running,
            badge_state: Some(BadgeState::Working),
            expanded: true,
            pane_id: None,
            git: None,
            active: false,
            meta: None,
        },
        first.clone(),
        SidebarRow {
            id: "detail::%1::101::prompt".to_string(),
            kind: SidebarRowKind::Detail,
            depth: 1,
            label: "details".to_string(),
            chat_count: 0,
            rollup: RollupLevel::Running,
            badge_state: Some(BadgeState::Working),
            expanded: true,
            pane_id: Some("%1".to_string()),
            git: None,
            active: false,
            meta: None,
        },
        second.clone(),
    ];

    assert_eq!(
        adjacent_agent_target(None, &rows, true).map(|target| target.0),
        Some(first.id.clone())
    );
    assert_eq!(
        adjacent_agent_target(None, &rows, false).map(|target| target.0),
        Some(second.id.clone())
    );
    assert_eq!(
        adjacent_agent_target(Some(&first.id), &rows, true).map(|target| target.0),
        Some(second.id.clone())
    );
    assert!(adjacent_agent_target(Some(&first.id), &rows, false).is_none());
    assert!(adjacent_agent_target(Some(&second.id), &rows, true).is_none());
}

#[test]
fn read_current_candidates_are_subsequent_visible_unread_chats_without_wrapping() {
    let chat = |pane_id: &str, pane_pid: u32, unread: bool| SidebarRow {
        id: chat_row_id(&PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        }),
        kind: SidebarRowKind::Chat,
        depth: 0,
        label: pane_id.to_string(),
        chat_count: 1,
        rollup: RollupLevel::Idle,
        badge_state: Some(BadgeState::Idle),
        expanded: true,
        pane_id: Some(pane_id.to_string()),
        git: None,
        active: false,
        meta: Some(crate::sidebar::tree::RowMeta {
            is_unread: unread,
            ..crate::sidebar::tree::RowMeta::default()
        }),
    };
    let rows = vec![
        chat("%0", 100, true),
        chat("%1", 101, true),
        chat("%2", 102, false),
        SidebarRow {
            id: "zone::idle".to_string(),
            kind: SidebarRowKind::Zone,
            depth: 0,
            label: "IDLE".to_string(),
            chat_count: 0,
            rollup: RollupLevel::Idle,
            badge_state: None,
            expanded: true,
            pane_id: None,
            git: None,
            active: false,
            meta: None,
        },
        chat("%3", 103, true),
    ];

    assert_eq!(
        unread_advance_candidates(
            &PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 101,
            },
            &rows,
        ),
        vec![PaneInstance {
            pane_id: "%3".to_string(),
            pane_pid: 103,
        }]
    );
    assert!(
        unread_advance_candidates(
            &PaneInstance {
                pane_id: "%3".to_string(),
                pane_pid: 103,
            },
            &rows,
        )
        .is_empty()
    );
}

#[test]
fn persisted_preferences_seed_axes_filter_and_global_expansion() {
    let mut snapshot = snapshot(10);
    snapshot.sidebar_model.preferences.category_scope = CategoryScope::All;
    snapshot.sidebar_model.preferences.presentation_mode = PresentationMode::Tree;
    snapshot.sidebar_model.preferences.filter = StatusFilter::DoneOnly;
    snapshot.sidebar_model.preferences.expansion_overrides =
        std::collections::BTreeSet::from(["category::work".to_string()]);
    let mut state = SidebarState {
        selection: Some("chat::%7::70".to_string()),
        collapsed: std::collections::BTreeSet::from(["repo::misc::app".to_string()]),
        scroll: 4,
        return_target: Some(PaneInstance {
            pane_id: "%7".to_string(),
            pane_pid: 70,
        }),
        ..SidebarState::default()
    };
    let instance_local = (
        state.selection.clone(),
        state.scroll,
        state.return_target.clone(),
    );

    seed_persisted_sidebar_preferences(&snapshot, &mut state);

    assert_eq!(state.category_scope, CategoryScope::All);
    assert_eq!(state.presentation_mode, PresentationMode::Tree);
    assert_eq!(state.filter, StatusFilter::DoneOnly);
    assert_eq!(
        state.collapsed,
        std::collections::BTreeSet::from(["category::work".to_string()])
    );
    assert_eq!(
        (state.selection, state.scroll, state.return_target),
        instance_local
    );
}

#[test]
fn remote_axes_and_filter_updates_converge_without_moving_the_shared_cursor() {
    let mut snapshot = snapshot(11);
    snapshot.sidebar_model.preferences.category_scope = CategoryScope::Current;
    snapshot.sidebar_model.preferences.presentation_mode = PresentationMode::Flat;
    snapshot.sidebar_model.preferences.filter = StatusFilter::DoneOnly;
    let selection = Some("chat::%7::70".to_string());
    let mut first = SidebarState {
        category_scope: CategoryScope::All,
        selection: selection.clone(),
        scroll: 4,
        manual_scroll: true,
        ..SidebarState::default()
    };
    let mut second = first.clone();
    let original_preferences = Some((
        CategoryScope::All,
        PresentationMode::Tree,
        StatusFilter::All,
    ));
    let mut first_remote = original_preferences;
    let mut second_remote = original_preferences;
    let mut first_queued = original_preferences;
    let mut second_queued = original_preferences;

    assert!(apply_remote_sidebar_preferences(
        &snapshot,
        &mut first,
        &mut first_remote,
        &mut first_queued,
    ));
    assert!(apply_remote_sidebar_preferences(
        &snapshot,
        &mut second,
        &mut second_remote,
        &mut second_queued,
    ));

    assert_eq!(first.category_scope, CategoryScope::Current);
    assert_eq!(first.presentation_mode, PresentationMode::Flat);
    assert_eq!(first.filter, StatusFilter::DoneOnly);
    assert_eq!(first, second);
    assert_eq!(first.selection, selection);
    assert_eq!(first.scroll, 4);
    assert!(first.manual_scroll);
    assert_eq!(
        first_queued,
        Some((
            CategoryScope::Current,
            PresentationMode::Flat,
            StatusFilter::DoneOnly
        ))
    );
    assert_eq!(second_queued, first_queued);
    assert!(!apply_remote_sidebar_preferences(
        &snapshot,
        &mut second,
        &mut second_remote,
        &mut second_queued,
    ));
}

#[test]
fn remote_filter_ack_does_not_undo_a_newer_unqueued_presentation_change() {
    let mut snapshot = snapshot(12);
    snapshot.sidebar_model.preferences.presentation_mode = PresentationMode::Flat;
    snapshot.sidebar_model.preferences.filter = StatusFilter::All;
    let mut state = SidebarState {
        presentation_mode: PresentationMode::Priority,
        filter: StatusFilter::All,
        ..SidebarState::default()
    };
    let mut last_remote = Some((
        CategoryScope::Current,
        PresentationMode::Flat,
        StatusFilter::DoneOnly,
    ));
    let mut last_queued = Some((
        CategoryScope::Current,
        PresentationMode::Flat,
        StatusFilter::All,
    ));

    assert!(!apply_remote_sidebar_preferences(
        &snapshot,
        &mut state,
        &mut last_remote,
        &mut last_queued,
    ));

    assert_eq!(state.presentation_mode, PresentationMode::Priority);
    assert_eq!(state.filter, StatusFilter::All);
    assert_eq!(
        last_remote,
        Some((
            CategoryScope::Current,
            PresentationMode::Flat,
            StatusFilter::All
        ))
    );
    assert_eq!(last_queued, last_remote);
}

#[test]
fn active_session_marks_only_agents_linked_to_that_session() {
    let first = resolved_pane("%1", 10, "$1");
    let second = resolved_pane("%2", 20, "$2");
    let mut snapshot = ResolvedSnapshot {
        panes: vec![first.clone(), second.clone()],
        ..snapshot(10)
    };
    snapshot.sidebar_model.active_sessions = std::collections::BTreeSet::from(["$2".to_string()]);
    snapshot.sidebar_model.active_categories =
        std::collections::BTreeSet::from([crate::category::UNCATEGORIZED.to_string()]);
    let state = SidebarState {
        current_category: Some(crate::category::UNCATEGORIZED.to_string()),
        presentation_mode: PresentationMode::Flat,
        ..SidebarState::default()
    };

    let rows = project_view(&snapshot, &Config::default(), &state).rows;
    let first_row = rows
        .iter()
        .find(|row| row.id == chat_row_id(&first.pane_instance))
        .unwrap();
    let second_row = rows
        .iter()
        .find(|row| row.id == chat_row_id(&second.pane_instance))
        .unwrap();

    assert!(!first_row.active);
    assert!(second_row.active);
}

#[test]
fn persisted_filter_does_not_suppress_same_session_initial_selection() {
    let mut non_agent = pane(90);
    non_agent.pane_instance.pane_id = "%9".to_string();
    let agent = resolved_pane("%2", 20, "$1");
    let mut snapshot = ResolvedSnapshot {
        panes: vec![non_agent, agent.clone()],
        ..snapshot(10)
    };
    snapshot.sidebar_model.preferences.filter = StatusFilter::DoneOnly;
    let mut state = SidebarState {
        current_category: Some(crate::category::UNCATEGORIZED.to_string()),
        ..SidebarState::default()
    };

    seed_persisted_sidebar_preferences(&snapshot, &mut state);
    seed_initial_sidebar_context(
        &snapshot,
        &Config::default(),
        &mut state,
        Some("%9"),
        Some(90),
        Some("$1"),
    );

    assert_eq!(state.filter, StatusFilter::DoneOnly);
    assert_eq!(state.selection, Some(chat_row_id(&agent.pane_instance)));
}

#[test]
fn remote_navigation_updates_every_sidebar_state_once_per_revision() {
    let mut snapshot = snapshot(10);
    snapshot.sidebar_model.navigation = crate::sidebar::state::SidebarNavigation {
        revision: 1,
        selection: Some("chat::%1::10".to_string()),
        scroll: 7,
        manual_scroll: true,
    };
    let mut first = SidebarState::default();
    let mut second = SidebarState::default();
    let mut first_revision = 0;
    let mut second_revision = 0;
    let mut first_queued = None;
    let mut second_queued = None;

    assert!(apply_remote_navigation(
        &snapshot,
        &mut first,
        &mut first_revision,
        &mut first_queued,
    ));
    assert!(apply_remote_navigation(
        &snapshot,
        &mut second,
        &mut second_revision,
        &mut second_queued,
    ));
    assert_eq!(first.selection, second.selection);
    assert_eq!(first.scroll, 7);
    assert_eq!(second.scroll, 7);
    assert!(first.manual_scroll);
    assert!(second.manual_scroll);
    assert!(!apply_remote_navigation(
        &snapshot,
        &mut second,
        &mut second_revision,
        &mut second_queued,
    ));
}

#[test]
fn stale_selection_is_cleared_on_pane_id_reuse() {
    let snapshot = snapshot(11);
    let mut state = SidebarState {
        selection: Some(chat_row_id(&PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        })),
        ..SidebarState::default()
    };

    clear_stale_pane_selection(&snapshot, &mut state);

    assert!(state.selection.is_none());
}

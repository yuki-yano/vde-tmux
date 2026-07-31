use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Once;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::config::Config;
use crate::daemon::protocol::v2::ResolvedSnapshot;
use crate::daemon::session_badge::BadgeState;
use crate::pane_state::{PaneInstance, StateVersion, StoredStateDescriptor};
use crate::sidebar::client::{
    SubscriptionUpdate, send_sidebar_jump_v2, send_sidebar_mark_complete_v2, subscribe_v2,
};
use crate::sidebar::render::{
    HeaderAction, HeaderLayout, RenderedLines, SidebarRenderTheme, build_footer_line,
    build_header_layout_with_counts, display_width, header_hit_test, render_header_lines,
    render_lines_with_indices,
};
use crate::sidebar::state::{SidebarAction, SidebarPreferenceIntent, SidebarState, StatusFilter};
use crate::sidebar::tree::{
    BadgeCounts, SidebarProjection, SidebarRow, SidebarRowKind, chat_row_id,
    pane_instance_from_row_id, project_sidebar, row_refs,
};
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

static PANIC_RESTORE_HOOK: Once = Once::new();

pub fn run_live_tui(
    env: &BTreeMap<String, String>,
    config: &Config,
    socket: &Path,
    server_identity: &str,
) -> Result<Option<String>> {
    install_panic_restore_hook();
    let close_window =
        resolve_current_window_id(&SystemTmuxRunner::from_env(Duration::from_secs(1)), env)?;

    enable_raw_mode()?;
    let mut restore_guard = TerminalRestoreGuard { active: true };
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let runner = SystemTmuxRunner::from_env(Duration::from_secs(1));
    let sidebar_instance = crate::sidebar::control::resolve_current_pane_instance(&runner, env)?;
    let control =
        crate::sidebar::control::ControlListener::bind(server_identity, &sidebar_instance)?;
    let mut active_config = config.clone();
    let result = loop {
        let (tx, rx) = mpsc::channel();
        let config_hash = crate::daemon::lifecycle::config_hash(&active_config);
        subscribe_v2(socket, server_identity, &config_hash, tx)?;
        let theme = SidebarRenderTheme::from_app_config(&active_config);
        let runtime_config = RunLoopConfig {
            app: &active_config,
            theme: &theme,
        };
        match run_loop(
            &mut terminal,
            RunLoopIo {
                socket,
                server_identity,
                snapshots: &rx,
                env,
                sidebar_instance: &sidebar_instance,
                control: &control,
            },
            runtime_config,
        ) {
            Ok(TuiExit::ConfigChanged { active_config_hash }) => {
                let reloaded = crate::config::load::load_config_strict(env).map_err(|error| {
                    anyhow::anyhow!("failed to reload sidebar config after daemon reload: {error}")
                })?;
                let reloaded_hash = crate::daemon::lifecycle::config_hash(&reloaded);
                if reloaded_hash != active_config_hash {
                    std::thread::sleep(Duration::from_millis(200));
                }
                active_config = reloaded;
            }
            result => break result,
        }
    };
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    restore_guard.active = false;
    match result? {
        TuiExit::Quit => {
            spawn_detached_sidebar_close(&std::env::current_exe()?, &close_window)?;
        }
        TuiExit::Disconnected => {
            eprintln!(
                "[vde-tmux] daemon への接続が終了しました。daemon を再起動して attach し直してください。"
            );
        }
        TuiExit::ConfigChanged { .. } => unreachable!("config reload is handled in the TUI loop"),
    }
    Ok(None)
}

struct TerminalRestoreGuard {
    active: bool,
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.active {
            let mut stderr = io::stderr();
            let _ = restore_terminal_after_panic(&mut stderr);
        }
    }
}

#[cfg(test)]
mod local_state_tests {
    use super::*;
    use crate::daemon::protocol::v2::{
        DaemonDiagnostic, ErrorCode, PanePresentation, SessionLinkPresentation,
    };
    use crate::hook::RollupLevel;
    use crate::pane_state::StateId;
    use crate::sidebar::state::ViewMode;

    fn structural_row(id: &str, kind: SidebarRowKind) -> SidebarRow {
        SidebarRow {
            id: id.to_string(),
            kind,
            depth: 0,
            label: id.to_string(),
            chat_count: 0,
            rollup: RollupLevel::Idle,
            badge_state: None,
            expanded: true,
            pane_id: None,
            git: None,
            active: false,
            meta: None,
        }
    }

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
    fn category_text_editor_emits_create_intent() {
        let (tx, rx) = mpsc::channel();
        let mut mode = Some(CategoryEditMode::Add {
            input: String::new(),
        });
        let mut ui = MarkCompleteUi::default();
        for key in ['n', 'e', 'w'] {
            handle_category_edit_key(
                crossterm::event::KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                &mut mode,
                &tx,
                &mut ui,
            );
        }
        handle_category_edit_key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut mode,
            &tx,
            &mut ui,
        );

        assert!(mode.is_none());
        assert_eq!(
            rx.recv().unwrap().intent,
            crate::category::CategoryIntent::CreateCategory {
                name: crate::category::CategoryName::parse("new").unwrap(),
            }
        );
    }

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

    fn pane(pane_pid: u32) -> PanePresentation {
        PanePresentation {
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid,
            },
            session_links: vec![SessionLinkPresentation {
                session_id: "$1".to_string(),
                session_name: "main".to_string(),
                window_index: 0,
                window_active: true,
                window_last: false,
            }],
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            current_path: "/tmp".to_string(),
            current_command: "zsh".to_string(),
            pane_width: 80,
            active: true,
            stored: None,
            resolved: None,
        }
    }

    fn snapshot(pane_pid: u32) -> ResolvedSnapshot {
        ResolvedSnapshot {
            snapshot_revision: 9,
            panes: vec![pane(pane_pid)],
            sidebar_model: crate::daemon::SidebarModel::default(),
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn resolved_pane(pane_id: &str, pane_pid: u32, session_id: &str) -> PanePresentation {
        use crate::pane_state::{
            AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneState, StateId, TaskState,
        };
        let pane_instance = PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        };
        let canonical = PaneState {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            state_id: StateId::parse(format!("{pane_pid:032x}")).unwrap(),
            revision: 1,
            pane_instance: pane_instance.clone(),
            agent: AgentKind::parse("codex").unwrap(),
            agent_session_id: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Running,
            run_seq: 1,
            completed_seq: 0,
            acknowledged_seq: 0,
            started_at: Some(1),
            completed_at: None,
            prompt: None,
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
        };
        PanePresentation {
            pane_instance: pane_instance.clone(),
            session_links: vec![SessionLinkPresentation {
                session_id: session_id.to_string(),
                session_name: "main".to_string(),
                window_index: 0,
                window_active: true,
                window_last: false,
            }],
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            current_path: "/tmp/app".to_string(),
            current_command: "codex".to_string(),
            pane_width: 80,
            active: true,
            stored: Some(StoredStateDescriptor::Canonical {
                version: canonical.version(),
            }),
            resolved: Some(crate::pane_state::ResolvedPaneState {
                canonical,
                window_id: "@1".to_string(),
                pane_id: pane_id.to_string(),
                current_path: "/tmp/app".to_string(),
                badge: BadgeState::Working,
            }),
        }
    }

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
                attention: 0,
                blocked: 0,
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
        let mut state = SidebarState::default();
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
    fn persisted_preferences_seed_view_filter_and_global_expansion() {
        let mut snapshot = snapshot(10);
        snapshot.sidebar_model.preferences.view_mode = ViewMode::ByCategory;
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

        assert_eq!(state.view_mode, ViewMode::ByCategory);
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
                intent: SidebarPreferenceIntent::SetDefaultViewMode {
                    view_mode: ViewMode::Flat,
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
    fn active_session_marks_only_agents_linked_to_that_session() {
        let first = resolved_pane("%1", 10, "$1");
        let second = resolved_pane("%2", 20, "$2");
        let mut snapshot = ResolvedSnapshot {
            panes: vec![first.clone(), second.clone()],
            ..snapshot(10)
        };
        snapshot.sidebar_model.active_sessions =
            std::collections::BTreeSet::from(["$2".to_string()]);
        let state = SidebarState {
            view_mode: ViewMode::Flat,
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
        let mut state = SidebarState::default();

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
    fn mouse_coordinates_map_through_header_scroll_and_rendered_rows() {
        let row = |id: &str| SidebarRow {
            id: id.to_string(),
            kind: SidebarRowKind::Chat,
            depth: 0,
            label: id.to_string(),
            chat_count: 1,
            rollup: RollupLevel::Running,
            badge_state: Some(BadgeState::Working),
            expanded: false,
            pane_id: Some(id.to_string()),
            git: None,
            active: false,
            meta: None,
        };
        let sidebar = SidebarView {
            rows: vec![row("first"), row("second")],
            ..SidebarView::default()
        };
        let row_indices = vec![None, Some(0), Some(1)];

        assert!(row_for_click_with_indices(&sidebar, 1, 2, 0, &row_indices).is_none());
        assert_eq!(
            row_for_click_with_indices(&sidebar, 2, 2, 1, &row_indices)
                .map(|clicked| clicked.row.id.as_str()),
            Some("first")
        );
        assert_eq!(
            row_for_click_with_indices(&sidebar, 3, 2, 1, &row_indices)
                .map(|clicked| clicked.row.id.as_str()),
            Some("second")
        );
    }

    #[test]
    fn agent_click_jumps_from_any_rendered_line_without_prior_selection() {
        let chat = SidebarRow {
            id: "chat::%1::101".to_string(),
            kind: SidebarRowKind::Chat,
            depth: 0,
            label: "codex".to_string(),
            chat_count: 0,
            rollup: RollupLevel::Running,
            badge_state: Some(BadgeState::Working),
            expanded: false,
            pane_id: Some("%1".to_string()),
            git: None,
            active: false,
            meta: None,
        };
        let detail = SidebarRow {
            id: "detail::%1::101::prompt".to_string(),
            kind: SidebarRowKind::Detail,
            depth: 1,
            label: "fix bug".to_string(),
            chat_count: 0,
            rollup: RollupLevel::Running,
            badge_state: Some(BadgeState::Working),
            expanded: true,
            pane_id: Some("%1".to_string()),
            git: None,
            active: false,
            meta: None,
        };
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };

        assert_eq!(
            row_click_intent(&ClickedRenderedRow { row: &chat }),
            Some(RowClickIntent::Jump(pane.clone()))
        );
        assert_eq!(
            row_click_intent(&ClickedRenderedRow { row: &detail }),
            Some(RowClickIntent::Jump(pane))
        );
    }

    #[test]
    fn remote_navigation_updates_every_sidebar_state_once_per_revision() {
        let mut snapshot = snapshot(10);
        snapshot.sidebar_model.navigation = crate::sidebar::state::SidebarNavigation {
            revision: 1,
            selection: Some("chat::%1::10".to_string()),
            scroll: 7,
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
        assert!(!apply_remote_navigation(
            &snapshot,
            &mut second,
            &mut second_revision,
            &mut second_queued,
        ));
    }

    #[test]
    fn vim_viewport_moves_select_ends_and_scroll_by_page_size() {
        let rows = (0..8)
            .map(|index| SidebarRow {
                id: format!("chat::%{}::{}", index + 1, index + 101),
                kind: SidebarRowKind::Chat,
                depth: 0,
                label: format!("agent-{index}"),
                chat_count: 0,
                rollup: RollupLevel::Running,
                badge_state: Some(BadgeState::Working),
                expanded: false,
                pane_id: Some(format!("%{}", index + 1)),
                git: None,
                active: false,
                meta: None,
            })
            .collect::<Vec<_>>();
        let mut state = SidebarState {
            selection: Some(rows[0].id.clone()),
            ..SidebarState::default()
        };
        let sidebar = SidebarView {
            state: state.clone(),
            rows,
            counts: BadgeCounts::default(),
        };
        let theme = SidebarRenderTheme::default();
        let rendered = render_lines_with_indices(&sidebar.rows, &sidebar.state, 60, &theme);
        let frame = DrawnFrame {
            header: build_header_layout_with_counts(&sidebar.state, 60, &theme, sidebar.counts),
            header_rows: 2,
            rows_height: 4,
            width: 60,
            scroll: 0,
            row_indices: rendered.row_indices.clone(),
        };

        move_projected_viewport(
            &sidebar,
            &rendered,
            &mut state,
            &frame,
            ViewportMove::WheelDown,
        );
        assert_eq!(state.scroll, 3);
        assert_ne!(
            state.selection.as_deref(),
            Some(sidebar.rows[0].id.as_str())
        );

        move_projected_viewport(
            &sidebar,
            &rendered,
            &mut state,
            &frame,
            ViewportMove::WheelUp,
        );
        assert_eq!(state.scroll, 0);
        assert_eq!(
            state.selection.as_deref(),
            Some(sidebar.rows[0].id.as_str())
        );

        move_projected_viewport(
            &sidebar,
            &rendered,
            &mut state,
            &frame,
            ViewportMove::PageDown,
        );
        assert_eq!(state.scroll, 4);
        assert_ne!(
            state.selection.as_deref(),
            Some(sidebar.rows[0].id.as_str())
        );

        move_projected_viewport(&sidebar, &rendered, &mut state, &frame, ViewportMove::First);
        assert_eq!(
            state.selection.as_deref(),
            Some(sidebar.rows[0].id.as_str())
        );
        assert_eq!(state.scroll, 0);

        move_projected_viewport(&sidebar, &rendered, &mut state, &frame, ViewportMove::Last);
        assert_eq!(
            state.selection.as_deref(),
            Some(sidebar.rows[7].id.as_str())
        );
        assert_eq!(
            state.scroll,
            rendered
                .lines
                .len()
                .saturating_sub(frame.rows_height as usize)
        );
    }

    #[test]
    fn panic_restore_emits_mouse_disable_and_alternate_screen_exit() {
        let mut output = Vec::new();

        restore_terminal_after_panic(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{1b}[?1000l"), "{output:?}");
        assert!(output.contains("\u{1b}[?1049l"), "{output:?}");
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
    fn degraded_empty_message_takes_priority_over_healthy_empty() {
        let lines = connection_empty_lines(
            &ConnectionState::Degraded("hook collision".to_string()),
            &SidebarRenderTheme::default(),
            80,
        )
        .unwrap();
        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Degraded: hook collision"));
    }

    #[test]
    fn toast_and_connection_lines_respect_width_and_semantic_colors() {
        let theme = SidebarRenderTheme::default();
        let success = contextual_footer_line(
            12,
            &theme,
            Some(Notice {
                message: "operation completed successfully with a long message",
                level: NoticeLevel::Success,
            }),
            &ConnectionState::Connected,
        );
        assert!(
            success
                .spans
                .iter()
                .map(|span| display_width(span.content.as_ref()))
                .sum::<usize>()
                <= 12
        );
        assert_eq!(
            success.spans.first().and_then(|span| span.style.fg),
            Some(theme.badge_done)
        );

        for connection in [
            ConnectionState::Disconnected,
            ConnectionState::Degraded("a very long degraded diagnostic".to_string()),
        ] {
            let footer = contextual_footer_line(10, &theme, None, &connection);
            assert!(
                footer
                    .spans
                    .iter()
                    .map(|span| display_width(span.content.as_ref()))
                    .sum::<usize>()
                    <= 10
            );
            assert_eq!(
                footer.spans.first().and_then(|span| span.style.fg),
                Some(theme.badge_blocked)
            );
            let empty = connection_empty_lines(&connection, &theme, 10).unwrap();
            assert!(empty.iter().all(|line| {
                line.spans
                    .iter()
                    .map(|span| display_width(span.content.as_ref()))
                    .sum::<usize>()
                    <= 10
            }));
        }
    }
}

fn install_panic_restore_hook() {
    PANIC_RESTORE_HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let mut stderr = io::stderr();
            let _ = restore_terminal_after_panic(&mut stderr);
            previous(info);
        }));
    });
}

fn restore_terminal_after_panic<W: Write>(writer: &mut W) -> Result<()> {
    let _ = disable_raw_mode();
    execute!(writer, DisableMouseCapture, LeaveAlternateScreen)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiExit {
    Quit,
    Disconnected,
    ConfigChanged { active_config_hash: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum ConnectionState {
    #[default]
    Connecting,
    Connected,
    ConfigChanged(String),
    Degraded(String),
    Disconnected,
}

impl ConnectionState {
    fn label(&self) -> Option<&str> {
        match self {
            Self::Connecting => Some("connecting"),
            Self::Connected => None,
            Self::ConfigChanged(_) => Some("reloading config"),
            Self::Degraded(_) => Some("degraded"),
            Self::Disconnected => Some("disconnected · reconnecting"),
        }
    }

    fn notice(&self) -> Option<Notice<'_>> {
        self.label().map(|message| Notice {
            message,
            level: match self {
                Self::Connecting | Self::ConfigChanged(_) => NoticeLevel::Progress,
                Self::Degraded(_) | Self::Disconnected => NoticeLevel::Failure,
                Self::Connected => NoticeLevel::Success,
            },
        })
    }
}

/// Decides whether a run-loop iteration projects the sidebar view and draws a frame.
/// The gate stays clean until a visible change is reported; elapsed-time labels in a
/// sidebar visible to an attached client are refreshed once per second boundary.
#[derive(Debug)]
struct RenderGate {
    dirty: bool,
    last_elapsed_second: Option<i64>,
    last_toast: Option<(String, NoticeLevel)>,
}

impl RenderGate {
    fn new() -> Self {
        Self {
            dirty: true,
            last_elapsed_second: None,
            last_toast: None,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn mark_dirty_if(&mut self, changed: bool) {
        if changed {
            self.dirty = true;
        }
    }

    fn note_toast(&mut self, notice: Option<Notice<'_>>) {
        let current = notice.map(|notice| (notice.message.to_string(), notice.level));
        if self.last_toast != current {
            self.last_toast = current;
            self.dirty = true;
        }
    }

    fn take_draw_decision(&mut self, now_epoch_secs: i64, elapsed_clock_visible: bool) -> bool {
        if elapsed_clock_visible && self.last_elapsed_second != Some(now_epoch_secs) {
            self.last_elapsed_second = Some(now_epoch_secs);
            self.dirty = true;
        }
        std::mem::take(&mut self.dirty)
    }
}

fn sidebar_elapsed_clock_visible(
    snapshot: &ResolvedSnapshot,
    sidebar_instance: &PaneInstance,
) -> bool {
    snapshot
        .panes
        .iter()
        .find(|pane| &pane.pane_instance == sidebar_instance)
        .is_some_and(|pane| {
            pane.session_links.iter().any(|link| {
                link.window_active
                    && snapshot
                        .sidebar_model
                        .active_sessions
                        .contains(&link.session_id)
            })
        })
}

/// Geometry and row mapping of the most recently drawn frame. Click hit-testing
/// must use exactly what was drawn, so the run loop records it on every draw.
#[derive(Debug, Clone)]
struct DrawnFrame {
    header: crate::sidebar::render::HeaderLayout,
    header_rows: u16,
    rows_height: u16,
    width: u16,
    scroll: usize,
    row_indices: Vec<Option<usize>>,
}

#[derive(Debug, Clone, Copy)]
pub struct RunLoopConfig<'a> {
    pub app: &'a Config,
    pub theme: &'a SidebarRenderTheme,
}

struct RunLoopIo<'a> {
    socket: &'a Path,
    server_identity: &'a str,
    snapshots: &'a mpsc::Receiver<SubscriptionUpdate>,
    env: &'a BTreeMap<String, String>,
    sidebar_instance: &'a PaneInstance,
    control: &'a crate::sidebar::control::ControlListener,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SidebarView {
    state: SidebarState,
    rows: Vec<SidebarRow>,
    counts: BadgeCounts,
}

struct MarkCompleteRequest {
    pane_instance: PaneInstance,
    expected: StateVersion,
}

struct MarkCompleteResult {
    pane_instance: PaneInstance,
    result: Result<()>,
}

struct PreferenceIntentRequest {
    intent: SidebarPreferenceIntent,
}

struct PreferenceIntentResult {
    intent: SidebarPreferenceIntent,
    result: Result<()>,
}

struct CategoryIntentRequest {
    intent: crate::category::CategoryIntent,
}

struct CategoryIntentResult {
    result: Result<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NavigationRequest {
    selection: Option<String>,
    scroll: usize,
}

struct NavigationResult {
    request: NavigationRequest,
    result: Result<()>,
}

#[derive(Debug, Clone)]
enum CategoryEditMode {
    Add {
        input: String,
    },
    Rename {
        current: crate::category::CategoryName,
        input: String,
    },
    MoveRepo {
        repo: crate::category::RepoKey,
        choices: Vec<MembershipChoice>,
        selected: usize,
        pending_g: bool,
    },
    Delete {
        category: crate::category::CategoryName,
        choices: Vec<MembershipChoice>,
        selected: usize,
        pending_g: bool,
    },
}

#[derive(Debug, Clone)]
enum MembershipChoice {
    Automatic,
    Category(crate::category::CategoryName),
}

impl MembershipChoice {
    fn label(&self) -> &str {
        match self {
            Self::Automatic => crate::category::AUTOMATIC_LABEL,
            Self::Category(category) => category.as_str(),
        }
    }

    fn target(&self) -> crate::category::MembershipTarget {
        match self {
            Self::Automatic => crate::category::MembershipTarget::Automatic,
            Self::Category(category) => {
                crate::category::MembershipTarget::Category(category.clone())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeLevel {
    Success,
    Progress,
    Warning,
    Failure,
}

#[derive(Debug, Clone)]
struct ToastNotice {
    message: String,
    level: NoticeLevel,
}

#[derive(Debug, Clone, Copy)]
struct Notice<'a> {
    message: &'a str,
    level: NoticeLevel,
}

#[derive(Debug, Default)]
struct MarkCompleteUi {
    pending: std::collections::BTreeSet<PaneInstance>,
    toast: Option<(ToastNotice, Instant)>,
}

impl MarkCompleteUi {
    fn notice(&self) -> Option<Notice<'_>> {
        self.toast
            .as_ref()
            .filter(|(_, expires)| *expires > Instant::now())
            .map(|(toast, _)| Notice {
                message: toast.message.as_str(),
                level: toast.level,
            })
            .or_else(|| {
                (!self.pending.is_empty()).then_some(Notice {
                    message: "marking complete...",
                    level: NoticeLevel::Progress,
                })
            })
    }

    fn set_toast(&mut self, message: String, level: NoticeLevel, duration: Duration) {
        self.toast = Some((ToastNotice { message, level }, Instant::now() + duration));
    }
}

fn begin_category_edit(
    key: char,
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    mode: &mut Option<CategoryEditMode>,
    ui: &mut MarkCompleteUi,
) -> bool {
    let selected = sidebar
        .state
        .selection
        .as_deref()
        .and_then(|selection| sidebar.rows.iter().find(|row| row.id == selection));
    *mode = match key {
        'a' => Some(CategoryEditMode::Add {
            input: String::new(),
        }),
        'r' => {
            let Some(category) = selected.and_then(|row| category_name_from_row_id(&row.id)) else {
                return false;
            };
            let editable = snapshot
                .sidebar_model
                .categories
                .category(&category)
                .is_some_and(|category| {
                    category.source == crate::category::CategorySource::Dynamic
                });
            if !editable {
                ui.set_toast(
                    "only dynamic categories can be renamed".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(4),
                );
                return true;
            }
            Some(CategoryEditMode::Rename {
                current: category,
                input: String::new(),
            })
        }
        'm' => {
            let Some((_, repo)) = selected.and_then(|row| category_repo_from_row_id(&row.id))
            else {
                return false;
            };
            Some(CategoryEditMode::MoveRepo {
                repo,
                choices: membership_choices(snapshot, None),
                selected: 0,
                pending_g: false,
            })
        }
        'D' => {
            let Some(category) = selected.and_then(|row| category_name_from_row_id(&row.id)) else {
                return false;
            };
            let editable = snapshot
                .sidebar_model
                .categories
                .category(&category)
                .is_some_and(|candidate| {
                    candidate.source == crate::category::CategorySource::Dynamic
                });
            if !editable {
                ui.set_toast(
                    "only dynamic categories can be deleted".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(4),
                );
                return true;
            }
            Some(CategoryEditMode::Delete {
                choices: membership_choices(snapshot, Some(&category)),
                category,
                selected: 0,
                pending_g: false,
            })
        }
        _ => return false,
    };
    refresh_category_edit_notice(mode.as_ref(), ui);
    true
}

fn membership_choices(
    snapshot: &ResolvedSnapshot,
    exclude: Option<&crate::category::CategoryName>,
) -> Vec<MembershipChoice> {
    std::iter::once(MembershipChoice::Automatic)
        .chain(
            snapshot
                .sidebar_model
                .categories
                .categories
                .iter()
                .filter(|category| exclude != Some(&category.name))
                .map(|category| MembershipChoice::Category(category.name.clone())),
        )
        .collect()
}

fn handle_category_edit_key(
    key: crossterm::event::KeyEvent,
    mode: &mut Option<CategoryEditMode>,
    tx: &mpsc::Sender<CategoryIntentRequest>,
    ui: &mut MarkCompleteUi,
) -> bool {
    let Some(current) = mode.as_mut() else {
        return false;
    };
    if key.code == KeyCode::Esc {
        *mode = None;
        ui.set_toast(
            "category edit cancelled".to_string(),
            NoticeLevel::Warning,
            Duration::from_secs(2),
        );
        return true;
    }
    let mut intent = None;
    match current {
        CategoryEditMode::Add { input } => match key.code {
            KeyCode::Enter => match crate::category::CategoryName::parse(input.as_str()) {
                Ok(name) => intent = Some(crate::category::CategoryIntent::CreateCategory { name }),
                Err(error) => {
                    ui.set_toast(error, NoticeLevel::Failure, Duration::from_secs(5));
                    return true;
                }
            },
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.push(ch),
            _ => {}
        },
        CategoryEditMode::Rename { current, input } => match key.code {
            KeyCode::Enter => match crate::category::CategoryName::parse(input.as_str()) {
                Ok(replacement) => {
                    intent = Some(crate::category::CategoryIntent::RenameCategory {
                        current: current.clone(),
                        replacement,
                    })
                }
                Err(error) => {
                    ui.set_toast(error, NoticeLevel::Failure, Duration::from_secs(5));
                    return true;
                }
            },
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.push(ch),
            _ => {}
        },
        CategoryEditMode::MoveRepo {
            repo,
            choices,
            selected,
            pending_g,
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                *pending_g = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(choices.len().saturating_sub(1));
                *pending_g = false;
            }
            KeyCode::Char('g') => {
                if *pending_g {
                    *selected = 0;
                    *pending_g = false;
                } else {
                    *pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                *selected = choices.len().saturating_sub(1);
                *pending_g = false;
            }
            KeyCode::Enter => {
                intent = choices.get(*selected).map(|choice| match choice {
                    MembershipChoice::Automatic => {
                        crate::category::CategoryIntent::SetRepoAutomatic { repo: repo.clone() }
                    }
                    MembershipChoice::Category(category) => {
                        crate::category::CategoryIntent::AssignRepo {
                            repo: repo.clone(),
                            category: category.clone(),
                        }
                    }
                });
            }
            _ => *pending_g = false,
        },
        CategoryEditMode::Delete {
            category,
            choices,
            selected,
            pending_g,
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                *pending_g = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(choices.len().saturating_sub(1));
                *pending_g = false;
            }
            KeyCode::Char('g') => {
                if *pending_g {
                    *selected = 0;
                    *pending_g = false;
                } else {
                    *pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                *selected = choices.len().saturating_sub(1);
                *pending_g = false;
            }
            KeyCode::Enter => {
                intent = choices.get(*selected).map(|choice| {
                    crate::category::CategoryIntent::DeleteCategory {
                        category: category.clone(),
                        replacement: choice.target(),
                    }
                });
            }
            _ => *pending_g = false,
        },
    }
    if let Some(intent) = intent {
        if tx.send(CategoryIntentRequest { intent }).is_err() {
            ui.set_toast(
                "category worker unavailable".to_string(),
                NoticeLevel::Failure,
                Duration::from_secs(5),
            );
        } else {
            ui.set_toast(
                "saving category...".to_string(),
                NoticeLevel::Progress,
                Duration::from_secs(3),
            );
        }
        *mode = None;
    } else {
        refresh_category_edit_notice(mode.as_ref(), ui);
    }
    true
}

fn refresh_category_edit_notice(mode: Option<&CategoryEditMode>, ui: &mut MarkCompleteUi) {
    let Some(mode) = mode else {
        return;
    };
    let message = match mode {
        CategoryEditMode::Add { input } => format!("add category: {input}_"),
        CategoryEditMode::Rename { current, input } => {
            format!("rename {current} to: {input}_")
        }
        CategoryEditMode::MoveRepo {
            choices, selected, ..
        } => format!(
            "move repo to: {}  [j/k, Enter]",
            choices
                .get(*selected)
                .map(MembershipChoice::label)
                .unwrap_or("-")
        ),
        CategoryEditMode::Delete {
            category,
            choices,
            selected,
            ..
        } => format!(
            "delete {category}; move repos to: {}  [j/k, Enter]",
            choices
                .get(*selected)
                .map(MembershipChoice::label)
                .unwrap_or("-")
        ),
    };
    ui.set_toast(
        message,
        NoticeLevel::Progress,
        Duration::from_secs(24 * 60 * 60),
    );
}

fn spawn_mark_complete_worker(
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

fn spawn_preference_intent_worker(
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

fn spawn_category_intent_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<CategoryIntentRequest>,
    tx: mpsc::Sender<CategoryIntentResult>,
) {
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let result = crate::sidebar::client::send_category_intent_v2(
                &socket,
                &server_identity,
                request.intent,
            );
            let failed = result.is_err();
            if tx.send(CategoryIntentResult { result }).is_err() {
                return;
            }
            if failed {
                while rx.try_recv().is_ok() {}
            }
        }
    });
}

fn spawn_navigation_worker(
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

fn queue_reorder(
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

fn category_name_from_row_id(id: &str) -> Option<crate::category::CategoryName> {
    let name = id.strip_prefix("category::")?;
    if name == crate::category::UNCATEGORIZED {
        Some(crate::category::CategoryName::uncategorized())
    } else {
        crate::category::CategoryName::parse(name).ok()
    }
}

fn category_repo_from_row_id(
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

fn queue_mark_complete(
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

fn drain_mark_complete_results(
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

fn project_view(snapshot: &ResolvedSnapshot, config: &Config, state: &SidebarState) -> SidebarView {
    let SidebarProjection { rows, counts } = project_sidebar(
        config,
        &snapshot.panes,
        &snapshot.sidebar_model,
        state,
        crate::sidebar::tree::now_epoch_secs(),
    );
    SidebarView {
        state: state.clone(),
        rows,
        counts,
    }
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    io: RunLoopIo<'_>,
    config: RunLoopConfig<'_>,
) -> Result<TuiExit> {
    let RunLoopIo {
        socket,
        server_identity,
        snapshots: rx,
        env,
        sidebar_instance,
        control,
    } = io;
    let theme = config.theme;
    let mut current: Option<ResolvedSnapshot> = None;
    let mut connection = ConnectionState::Connecting;
    let mut last_known_rows: Option<(Vec<SidebarRow>, BadgeCounts)> = None;
    let mut sidebar_state = SidebarState::default();
    let mut initial_context_seeded = false;
    let mut last_queued_preferences = None;
    let mut last_expansion_view: Option<BTreeSet<String>> = None;
    let mut last_remote_expansion: Option<BTreeSet<String>> = None;
    let mut last_remote_navigation_revision = 0;
    let mut last_queued_navigation: Option<(Option<String>, usize)> = None;
    let (mark_request_tx, mark_request_rx) = mpsc::channel();
    let (mark_result_tx, mark_result_rx) = mpsc::channel();
    spawn_mark_complete_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        mark_request_rx,
        mark_result_tx,
    );
    let (preference_intent_tx, preference_intent_rx) = mpsc::channel();
    let (preference_result_tx, preference_result_rx) = mpsc::channel();
    spawn_preference_intent_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        preference_intent_rx,
        preference_result_tx,
    );
    let (category_intent_tx, category_intent_rx) = mpsc::channel();
    let (category_result_tx, category_result_rx) = mpsc::channel();
    spawn_category_intent_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        category_intent_rx,
        category_result_tx,
    );
    let (navigation_tx, navigation_rx) = mpsc::channel();
    let (navigation_result_tx, navigation_result_rx) = mpsc::channel();
    spawn_navigation_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        navigation_rx,
        navigation_result_tx,
    );
    let mut mark_ui = MarkCompleteUi::default();
    let mut render_gate = RenderGate::new();
    let mut last_frame: Option<DrawnFrame> = None;
    let mut pending_g = false;
    let mut category_edit_mode: Option<CategoryEditMode> = None;
    loop {
        render_gate.mark_dirty_if(drain_snapshot_updates(rx, &mut current, &mut connection));
        if let ConnectionState::ConfigChanged(active_config_hash) = &connection {
            return Ok(TuiExit::ConfigChanged {
                active_config_hash: active_config_hash.clone(),
            });
        }
        if !initial_context_seeded && let Some(snapshot) = current.as_ref() {
            seed_persisted_sidebar_preferences(snapshot, &mut sidebar_state);
            last_queued_preferences = Some((sidebar_state.view_mode, sidebar_state.filter));
            last_expansion_view = Some(sidebar_state.collapsed.clone());
            last_remote_expansion = Some(
                snapshot
                    .sidebar_model
                    .preferences
                    .expansion_overrides
                    .clone(),
            );
            let pane = env.get(crate::sidebar::layout::ENV_SELECTION_PANE).cloned();
            let pane_pid = env
                .get(crate::sidebar::layout::ENV_SELECTION_PANE_PID)
                .and_then(|value| value.parse::<u32>().ok());
            let session_id = env
                .get(crate::sidebar::layout::ENV_SELECTION_SESSION)
                .map(String::as_str);
            seed_initial_sidebar_context(
                snapshot,
                config.app,
                &mut sidebar_state,
                pane.as_deref(),
                pane_pid,
                session_id,
            );
            let navigation = &snapshot.sidebar_model.navigation;
            last_remote_navigation_revision = navigation.revision;
            if navigation.revision > 0 {
                sidebar_state.selection = navigation.selection.clone();
                sidebar_state.scroll = navigation.scroll;
                last_queued_navigation = Some((navigation.selection.clone(), navigation.scroll));
            }
            initial_context_seeded = true;
        }
        if let Some(snapshot) = current.as_ref()
            && apply_remote_navigation(
                snapshot,
                &mut sidebar_state,
                &mut last_remote_navigation_revision,
                &mut last_queued_navigation,
            )
        {
            render_gate.mark_dirty();
        }
        if let Some(snapshot) = current.as_ref() {
            render_gate.mark_dirty_if(clear_stale_pane_selection(snapshot, &mut sidebar_state));
        }
        render_gate.mark_dirty_if(drain_mark_complete_results(&mark_result_rx, &mut mark_ui));
        while let Ok(result) = preference_result_rx.try_recv() {
            render_gate.mark_dirty();
            if let Err(error) = result.result {
                if matches!(result.intent, SidebarPreferenceIntent::SetExpanded { .. }) {
                    last_remote_expansion = None;
                }
                mark_ui.set_toast(
                    format!("preference save failed: {error}"),
                    NoticeLevel::Failure,
                    Duration::from_secs(5),
                );
            }
        }
        while let Ok(result) = category_result_rx.try_recv() {
            render_gate.mark_dirty();
            if let Err(error) = result.result {
                mark_ui.set_toast(
                    format!("category save failed: {error}"),
                    NoticeLevel::Failure,
                    Duration::from_secs(5),
                );
            }
        }
        while let Ok(result) = navigation_result_rx.try_recv() {
            if let Err(error) = result.result {
                if last_queued_navigation
                    == Some((result.request.selection.clone(), result.request.scroll))
                {
                    last_queued_navigation = None;
                }
                mark_ui.set_toast(
                    format!("navigation sync failed: {error}"),
                    NoticeLevel::Failure,
                    Duration::from_secs(5),
                );
                render_gate.mark_dirty();
            }
        }
        if !matches!(connection, ConnectionState::Connected) {
            if let Some(snapshot) = current.as_ref() {
                let remote = &snapshot.sidebar_model.preferences.expansion_overrides;
                sidebar_state.collapsed = remote.clone();
                last_remote_expansion = Some(remote.clone());
                last_expansion_view = Some(remote.clone());
            }
            last_queued_preferences = Some((sidebar_state.view_mode, sidebar_state.filter));
        } else if let Some(previous) = last_expansion_view.as_ref() {
            for row_id in previous
                .symmetric_difference(&sidebar_state.collapsed)
                .cloned()
                .collect::<Vec<_>>()
            {
                let default_open = !row_id.starts_with("chat::");
                let expanded = default_open ^ sidebar_state.collapsed.contains(&row_id);
                if preference_intent_tx
                    .send(PreferenceIntentRequest {
                        intent: SidebarPreferenceIntent::SetExpanded { row_id, expanded },
                    })
                    .is_err()
                {
                    mark_ui.set_toast(
                        "preference worker unavailable".to_string(),
                        NoticeLevel::Failure,
                        Duration::from_secs(5),
                    );
                }
            }
            last_expansion_view = Some(sidebar_state.collapsed.clone());
        }
        if let Some(snapshot) = current.as_ref() {
            let remote = &snapshot.sidebar_model.preferences.expansion_overrides;
            if last_remote_expansion.as_ref() != Some(remote) {
                sidebar_state.collapsed = remote.clone();
                last_expansion_view = Some(sidebar_state.collapsed.clone());
                last_remote_expansion = Some(remote.clone());
                render_gate.mark_dirty();
            }
        }
        let preferences = (sidebar_state.view_mode, sidebar_state.filter);
        if matches!(connection, ConnectionState::Connected)
            && last_queued_preferences.is_some_and(|previous| previous != preferences)
        {
            let previous = last_queued_preferences.expect("preference seed checked");
            let intents = [
                (previous.0 != sidebar_state.view_mode).then_some(
                    SidebarPreferenceIntent::SetDefaultViewMode {
                        view_mode: sidebar_state.view_mode,
                    },
                ),
                (previous.1 != sidebar_state.filter).then_some(
                    SidebarPreferenceIntent::SetDefaultFilter {
                        filter: sidebar_state.filter,
                    },
                ),
            ];
            for intent in intents.into_iter().flatten() {
                if preference_intent_tx
                    .send(PreferenceIntentRequest { intent })
                    .is_err()
                {
                    mark_ui.set_toast(
                        "preference worker unavailable".to_string(),
                        NoticeLevel::Failure,
                        Duration::from_secs(5),
                    );
                    break;
                }
            }
            last_queued_preferences = Some(preferences);
        }
        let navigation = (sidebar_state.selection.clone(), sidebar_state.scroll);
        if matches!(connection, ConnectionState::Connected)
            && last_queued_navigation.as_ref() != Some(&navigation)
        {
            if navigation_tx
                .send(NavigationRequest {
                    selection: navigation.0.clone(),
                    scroll: navigation.1,
                })
                .is_err()
            {
                mark_ui.set_toast(
                    "navigation worker unavailable".to_string(),
                    NoticeLevel::Failure,
                    Duration::from_secs(5),
                );
            } else {
                last_queued_navigation = Some(navigation);
            }
        }
        render_gate.mark_dirty_if(drain_control_messages(
            control,
            &mut sidebar_state,
            &preference_intent_tx,
            &category_intent_tx,
            &mut mark_ui,
            ControlMessageContext {
                snapshot: current.as_ref(),
                config: config.app,
                preferences_connected: matches!(connection, ConnectionState::Connected),
                frame: last_frame.as_ref(),
                theme,
            },
        )?);
        let context = ClickContext {
            socket,
            server_identity,
            source_pane: sidebar_instance,
        };
        render_gate.note_toast(mark_ui.notice());
        let elapsed_clock_visible = current
            .as_ref()
            .is_some_and(|snapshot| sidebar_elapsed_clock_visible(snapshot, sidebar_instance));
        let draw_this_loop = render_gate.take_draw_decision(
            crate::sidebar::tree::now_epoch_secs(),
            elapsed_clock_visible,
        );
        if draw_this_loop {
            if let Some(snapshot) = &current {
                let mut sidebar = project_view(snapshot, config.app, &sidebar_state);
                if sidebar.rows.is_empty() && matches!(connection, ConnectionState::Degraded(_)) {
                    if let Some((rows, counts)) = &last_known_rows {
                        sidebar.rows = rows.clone();
                        sidebar.counts = *counts;
                    }
                } else if !sidebar.rows.is_empty() {
                    last_known_rows = Some((sidebar.rows.clone(), sidebar.counts));
                } else if matches!(connection, ConnectionState::Connected) {
                    last_known_rows = None;
                }
                let size = terminal.size()?;
                let area = Rect::new(0, 0, size.width, size.height);
                let header = build_header_layout_with_counts(
                    &sidebar.state,
                    area.width,
                    theme,
                    sidebar.counts,
                );
                let areas = compute_areas(area, &header);
                let rendered = render_lines_with_indices(
                    &sidebar.rows,
                    &sidebar.state,
                    area.width as usize,
                    theme,
                );
                let selected_row_index =
                    sidebar.state.selection.as_deref().and_then(|selection| {
                        sidebar.rows.iter().position(|row| row.id == selection)
                    });
                let selection_range = selected_row_index
                    .and_then(|row_index| rendered_row_range(&rendered.row_indices, row_index));
                let frame_scroll = resolve_scroll_range(
                    sidebar_state.scroll,
                    selection_range,
                    rendered.lines.len(),
                    areas.rows_height as usize,
                );
                last_frame = Some(DrawnFrame {
                    header,
                    header_rows: areas.header_rows,
                    rows_height: areas.rows_height,
                    width: area.width,
                    scroll: frame_scroll,
                    row_indices: rendered.row_indices.clone(),
                });
                draw_snapshot_with_theme_and_scroll_options(
                    terminal,
                    snapshot,
                    &sidebar,
                    DrawOptions {
                        theme,
                        scroll: frame_scroll,
                        connection: &connection,
                        toast: mark_ui.notice(),
                        rendered: Some(&rendered),
                    },
                )?;
            } else {
                draw_connection_placeholder(terminal, &connection)?;
                last_frame = None;
            }
        }
        if event::poll(Duration::from_millis(50))? {
            let state_before = sidebar_state.clone();
            match event::read()? {
                Event::Key(key) if category_edit_mode.is_some() => {
                    pending_g = false;
                    handle_category_edit_key(
                        key,
                        &mut category_edit_mode,
                        &category_intent_tx,
                        &mut mark_ui,
                    );
                    render_gate.mark_dirty();
                }
                Event::Key(key) => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(TuiExit::Quit),
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            move_viewport_selection(
                                snapshot,
                                config.app,
                                &mut sidebar_state,
                                last_frame.as_ref(),
                                ViewportMove::HalfPageDown,
                                theme,
                            );
                        }
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            move_viewport_selection(
                                snapshot,
                                config.app,
                                &mut sidebar_state,
                                last_frame.as_ref(),
                                ViewportMove::HalfPageUp,
                                theme,
                            );
                        }
                    }
                    KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            move_viewport_selection(
                                snapshot,
                                config.app,
                                &mut sidebar_state,
                                last_frame.as_ref(),
                                ViewportMove::PageDown,
                                theme,
                            );
                        }
                    }
                    KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            move_viewport_selection(
                                snapshot,
                                config.app,
                                &mut sidebar_state,
                                last_frame.as_ref(),
                                ViewportMove::PageUp,
                                theme,
                            );
                        }
                    }
                    KeyCode::Char('d') => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            let sidebar = project_view(snapshot, config.app, &sidebar_state);
                            queue_mark_complete_for_selection(
                                snapshot,
                                &sidebar,
                                &mark_request_tx,
                                &mut mark_ui,
                            );
                        }
                    }
                    KeyCode::Char(' ') => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            let sidebar = project_view(snapshot, config.app, &sidebar_state);
                            apply_local_sidebar_key(&mut sidebar_state, &sidebar, "space");
                        }
                    }
                    KeyCode::Char(ch) => {
                        if let Some(snapshot) = &current {
                            let sidebar = project_view(snapshot, config.app, &sidebar_state);
                            if matches!(ch, 'a' | 'm' | 'r' | 'D')
                                && begin_category_edit(
                                    ch,
                                    snapshot,
                                    &sidebar,
                                    &mut category_edit_mode,
                                    &mut mark_ui,
                                )
                            {
                                pending_g = false;
                                render_gate.mark_dirty();
                            } else if ch == 'g' {
                                if pending_g {
                                    move_viewport_selection(
                                        snapshot,
                                        config.app,
                                        &mut sidebar_state,
                                        last_frame.as_ref(),
                                        ViewportMove::First,
                                        theme,
                                    );
                                    pending_g = false;
                                } else {
                                    pending_g = true;
                                }
                            } else if ch == 'G' {
                                move_viewport_selection(
                                    snapshot,
                                    config.app,
                                    &mut sidebar_state,
                                    last_frame.as_ref(),
                                    ViewportMove::Last,
                                    theme,
                                );
                                pending_g = false;
                            } else if matches!(connection, ConnectionState::Connected)
                                && matches!(ch, 'J' | 'K')
                            {
                                pending_g = false;
                                queue_reorder(
                                    &sidebar,
                                    ch == 'K',
                                    &preference_intent_tx,
                                    &category_intent_tx,
                                    &mut mark_ui,
                                );
                            } else {
                                pending_g = false;
                                apply_local_sidebar_key(
                                    &mut sidebar_state,
                                    &sidebar,
                                    &ch.to_string(),
                                );
                            }
                        }
                    }
                    KeyCode::Down | KeyCode::Up | KeyCode::Tab | KeyCode::BackTab => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            let sidebar = project_view(snapshot, config.app, &sidebar_state);
                            let key = match key.code {
                                KeyCode::Down => "down",
                                KeyCode::Up => "up",
                                KeyCode::Tab => "tab",
                                KeyCode::BackTab => "backtab",
                                _ => unreachable!(),
                            };
                            apply_local_sidebar_key(&mut sidebar_state, &sidebar, key);
                        }
                    }
                    KeyCode::Enter => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            let sidebar = project_view(snapshot, config.app, &sidebar_state);
                            activate_local_selection(
                                &context,
                                snapshot,
                                &mut sidebar_state,
                                &sidebar,
                                &mut mark_ui,
                            );
                        }
                    }
                    _ => pending_g = false,
                },
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    pending_g = false;
                    if let (Some(snapshot), Some(frame)) = (&current, last_frame.as_ref()) {
                        let sidebar = project_view(snapshot, config.app, &sidebar_state);
                        handle_left_click(
                            &context,
                            snapshot,
                            &mut sidebar_state,
                            &sidebar,
                            &mut mark_ui,
                            frame,
                            ClickPosition {
                                row: mouse.row,
                                column: mouse.column,
                            },
                        )?;
                    }
                }
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                    ) =>
                {
                    pending_g = false;
                    if let (Some(snapshot), Some(frame)) = (&current, last_frame.as_ref()) {
                        let movement = if mouse.kind == MouseEventKind::ScrollDown {
                            ViewportMove::WheelDown
                        } else {
                            ViewportMove::WheelUp
                        };
                        move_viewport_selection(
                            snapshot,
                            config.app,
                            &mut sidebar_state,
                            Some(frame),
                            movement,
                            theme,
                        );
                    }
                }
                Event::Resize(_, _) => {
                    pending_g = false;
                    render_gate.mark_dirty();
                }
                _ => {}
            }
            render_gate.mark_dirty_if(sidebar_state != state_before);
        }
    }
}

fn clear_stale_pane_selection(snapshot: &ResolvedSnapshot, state: &mut SidebarState) -> bool {
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
        state.version = state.version.saturating_add(1);
        return true;
    }
    false
}

fn apply_remote_navigation(
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
    last_revision: &mut u64,
    last_queued: &mut Option<(Option<String>, usize)>,
) -> bool {
    let navigation = &snapshot.sidebar_model.navigation;
    if navigation.revision <= *last_revision {
        return false;
    }
    let changed = state.selection != navigation.selection || state.scroll != navigation.scroll;
    state.selection = navigation.selection.clone();
    state.scroll = navigation.scroll;
    if changed {
        state.version = state.version.saturating_add(1);
    }
    *last_revision = navigation.revision;
    *last_queued = Some((navigation.selection.clone(), navigation.scroll));
    changed
}

fn seed_initial_sidebar_context(
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
    if let Some(pane) = pane_instance.as_ref()
        && snapshot
            .panes
            .iter()
            .any(|candidate| candidate.pane_instance == *pane)
    {
        state.return_target = Some(pane.clone());
    }
    select_context_agent(snapshot, config, state, pane_instance.as_ref(), session_id);
}

fn seed_persisted_sidebar_preferences(snapshot: &ResolvedSnapshot, state: &mut SidebarState) {
    state.view_mode = snapshot.sidebar_model.preferences.view_mode;
    state.filter = snapshot.sidebar_model.preferences.filter;
    state.collapsed = snapshot
        .sidebar_model
        .preferences
        .expansion_overrides
        .clone();
}

fn select_context_agent(
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
    state.version = state.version.saturating_add(1);
    true
}

fn apply_local_sidebar_key(state: &mut SidebarState, sidebar: &SidebarView, key: &str) {
    use crate::sidebar::input::SidebarInputAction;

    let Some(action) = crate::sidebar::input::parse_key(key) else {
        return;
    };
    let refs = row_refs(&sidebar.rows);
    match action {
        SidebarInputAction::MoveNext => {
            state.apply(SidebarAction::MoveNext, &refs);
        }
        SidebarInputAction::MovePrevious => {
            state.apply(SidebarAction::MovePrevious, &refs);
        }
        SidebarInputAction::ToggleExpand => {
            state.apply(SidebarAction::ToggleExpand, &refs);
        }
        SidebarInputAction::SetViewMode(mode) => {
            state.apply(SidebarAction::SetViewMode(mode), &refs);
        }
        SidebarInputAction::CycleViewMode => {
            state.apply(SidebarAction::CycleViewMode, &refs);
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
            state.toggle_expanded(&row_id);
        }
        SidebarInputAction::FocusNextAttention | SidebarInputAction::FocusPreviousAttention => {
            let ids = sidebar
                .rows
                .iter()
                .filter(|row| {
                    row.kind == SidebarRowKind::Chat && row.badge_state == Some(BadgeState::Blocked)
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
        | SidebarInputAction::ReorderUp
        | SidebarInputAction::ReorderDown => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewportMove {
    First,
    Last,
    WheelDown,
    WheelUp,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
}

fn move_viewport_selection(
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

fn move_projected_viewport(
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
        ViewportMove::WheelDown | ViewportMove::WheelUp => 3,
        ViewportMove::HalfPageDown | ViewportMove::HalfPageUp => (viewport / 2).max(1),
        ViewportMove::PageDown | ViewportMove::PageUp => viewport,
        ViewportMove::First | ViewportMove::Last => 0,
    };
    let (target_line, target_scroll, forward) = match movement {
        ViewportMove::First => (0, 0, true),
        ViewportMove::Last => (rendered.lines.len() - 1, max_scroll, false),
        ViewportMove::WheelDown | ViewportMove::HalfPageDown | ViewportMove::PageDown => (
            current_line
                .saturating_add(amount)
                .min(rendered.lines.len() - 1),
            state.scroll.saturating_add(amount).min(max_scroll),
            true,
        ),
        ViewportMove::WheelUp | ViewportMove::HalfPageUp | ViewportMove::PageUp => (
            current_line.saturating_sub(amount),
            state.scroll.saturating_sub(amount),
            false,
        ),
    };
    let selection = navigable_selection_at(sidebar, &rendered.row_indices, target_line, forward);
    if state.selection != selection || state.scroll != target_scroll {
        state.selection = selection;
        state.scroll = target_scroll;
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

fn activate_local_selection(
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
                mark_ui.set_toast(
                    "selected pane is stale".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(5),
                );
            }
        }
        Some(crate::sidebar::input::SidebarCommand::ToggleExpand(row_id)) => {
            state.selection = Some(row_id.clone());
            state.toggle_expanded(&row_id);
        }
        None => {}
    }
}

struct ControlMessageContext<'a> {
    snapshot: Option<&'a ResolvedSnapshot>,
    config: &'a Config,
    preferences_connected: bool,
    frame: Option<&'a DrawnFrame>,
    theme: &'a SidebarRenderTheme,
}

fn drain_control_messages(
    control: &crate::sidebar::control::ControlListener,
    state: &mut SidebarState,
    preference_tx: &mpsc::Sender<PreferenceIntentRequest>,
    category_tx: &mpsc::Sender<CategoryIntentRequest>,
    ui: &mut MarkCompleteUi,
    context: ControlMessageContext<'_>,
) -> Result<bool> {
    let ControlMessageContext {
        snapshot,
        config,
        preferences_connected,
        frame,
        theme,
    } = context;
    let mut before: Option<SidebarState> = None;
    while let Some(message) = control.try_recv()? {
        if before.is_none() {
            before = Some(state.clone());
        }
        match message {
            crate::sidebar::control::ControlMessage::Input { key } => {
                if let Some(snapshot) = snapshot {
                    let sidebar = project_view(snapshot, config, state);
                    match crate::sidebar::input::parse_key(&key) {
                        Some(crate::sidebar::input::SidebarInputAction::MoveFirst) => {
                            move_viewport_selection(
                                snapshot,
                                config,
                                state,
                                frame,
                                ViewportMove::First,
                                theme,
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::MoveLast) => {
                            move_viewport_selection(
                                snapshot,
                                config,
                                state,
                                frame,
                                ViewportMove::Last,
                                theme,
                            );
                        }
                        Some(
                            action @ (crate::sidebar::input::SidebarInputAction::HalfPageDown
                            | crate::sidebar::input::SidebarInputAction::HalfPageUp
                            | crate::sidebar::input::SidebarInputAction::PageDown
                            | crate::sidebar::input::SidebarInputAction::PageUp),
                        ) => {
                            let movement = match action {
                                crate::sidebar::input::SidebarInputAction::HalfPageDown => {
                                    ViewportMove::HalfPageDown
                                }
                                crate::sidebar::input::SidebarInputAction::HalfPageUp => {
                                    ViewportMove::HalfPageUp
                                }
                                crate::sidebar::input::SidebarInputAction::PageDown => {
                                    ViewportMove::PageDown
                                }
                                crate::sidebar::input::SidebarInputAction::PageUp => {
                                    ViewportMove::PageUp
                                }
                                _ => unreachable!(),
                            };
                            move_viewport_selection(
                                snapshot, config, state, frame, movement, theme,
                            );
                        }
                        Some(crate::sidebar::input::SidebarInputAction::ReorderUp)
                            if preferences_connected =>
                        {
                            queue_reorder(&sidebar, true, preference_tx, category_tx, ui);
                        }
                        Some(crate::sidebar::input::SidebarInputAction::ReorderDown)
                            if preferences_connected =>
                        {
                            queue_reorder(&sidebar, false, preference_tx, category_tx, ui);
                        }
                        _ => apply_local_sidebar_key(state, &sidebar, &key),
                    }
                }
            }
            crate::sidebar::control::ControlMessage::Focus {
                pane_instance,
                session_id,
            } => {
                let Some(snapshot) = snapshot else {
                    continue;
                };
                apply_focus_message(snapshot, config, state, pane_instance, &session_id);
            }
        }
    }
    Ok(before.is_some_and(|before| before != *state))
}

fn apply_focus_message(
    snapshot: &ResolvedSnapshot,
    config: &Config,
    state: &mut SidebarState,
    pane_instance: PaneInstance,
    session_id: &str,
) -> bool {
    let Some(pane) = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance == pane_instance)
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
    state.return_target = Some(pane_instance.clone());
    let changed = select_context_agent(
        snapshot,
        config,
        state,
        Some(&pane_instance),
        Some(session_id),
    );
    if changed {
        true
    } else {
        state.version = state.version.saturating_add(1);
        true
    }
}

fn drain_snapshot_updates(
    rx: &mpsc::Receiver<SubscriptionUpdate>,
    current: &mut Option<ResolvedSnapshot>,
    connection: &mut ConnectionState,
) -> bool {
    let mut changed = false;
    let set_connection = |connection: &mut ConnectionState, next: ConnectionState| {
        if *connection != next {
            *connection = next;
            true
        } else {
            false
        }
    };
    loop {
        match rx.try_recv() {
            Ok(SubscriptionUpdate::Connecting) => {
                changed |= set_connection(connection, ConnectionState::Connecting);
            }
            Ok(SubscriptionUpdate::Connected(snapshot)) => {
                let next = match snapshot_degraded_message(&snapshot) {
                    Some(message) => ConnectionState::Degraded(message),
                    None => ConnectionState::Connected,
                };
                let revision_changed = current.as_ref().is_none_or(|previous| {
                    previous.snapshot_revision != snapshot.snapshot_revision
                });
                *current = Some(*snapshot);
                changed |= revision_changed;
                changed |= set_connection(connection, next);
            }
            Ok(SubscriptionUpdate::ConfigChanged { active_config_hash }) => {
                changed |= set_connection(
                    connection,
                    ConnectionState::ConfigChanged(active_config_hash),
                );
                return changed;
            }
            Ok(SubscriptionUpdate::Degraded(error)) => {
                changed |= set_connection(connection, ConnectionState::Degraded(error));
            }
            Ok(SubscriptionUpdate::Disconnected) => {
                changed |= set_connection(connection, ConnectionState::Disconnected);
            }
            Err(mpsc::TryRecvError::Empty) => return changed,
            Err(mpsc::TryRecvError::Disconnected) => {
                changed |= set_connection(connection, ConnectionState::Disconnected);
                return changed;
            }
        }
    }
}

fn snapshot_degraded_message(snapshot: &ResolvedSnapshot) -> Option<String> {
    crate::sidebar::current_degraded_message(snapshot)
}

fn resolve_current_window_id(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let mut args = vec!["display-message", "-p"];
    if let Some(pane) = env
        .get("TMUX_PANE")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        args.extend(["-t", pane]);
    }
    args.extend(["-F", "#{window_id}"]);
    let window = runner.run(&args)?.trim().to_string();
    if window.is_empty() {
        anyhow::bail!("failed to resolve current sidebar window");
    }
    Ok(window)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClickAction {
    JumpPane(PaneInstance),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RowClickIntent {
    Toggle(String),
    Jump(PaneInstance),
}

struct ClickedRenderedRow<'a> {
    row: &'a SidebarRow,
}

fn row_click_intent(clicked: &ClickedRenderedRow<'_>) -> Option<RowClickIntent> {
    match clicked.row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            Some(RowClickIntent::Toggle(clicked.row.id.clone()))
        }
        SidebarRowKind::Chat | SidebarRowKind::Detail => {
            pane_instance_from_row_id(&clicked.row.id).map(RowClickIntent::Jump)
        }
        SidebarRowKind::Zone => None,
    }
}

fn row_for_click_with_indices<'a>(
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
    Some(ClickedRenderedRow {
        row: sidebar.rows.get(row_index)?,
    })
}

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
            rendered: None,
        },
    )
}

#[derive(Clone, Copy)]
struct DrawOptions<'a> {
    theme: &'a SidebarRenderTheme,
    scroll: usize,
    connection: &'a ConnectionState,
    toast: Option<Notice<'a>>,
    /// Rows already rendered by the caller for scroll resolution; when present
    /// the draw path reuses them instead of rendering the same rows again.
    rendered: Option<&'a RenderedLines>,
}

fn draw_snapshot_with_theme_and_scroll_options<B: Backend>(
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

fn draw_connection_placeholder<B: Backend>(
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
    let max_scroll = rows_len - viewport;
    let mut scroll = prev.min(max_scroll);
    if let Some((start, end)) = selection_range {
        if start < scroll {
            scroll = start;
        } else if end >= scroll + viewport {
            scroll = end + 1 - viewport;
        }
    }
    scroll.min(max_scroll)
}

fn rendered_row_range(row_indices: &[Option<usize>], row_index: usize) -> Option<(usize, usize)> {
    let start = row_indices
        .iter()
        .position(|mapped| *mapped == Some(row_index))?;
    let end = row_indices
        .iter()
        .rposition(|mapped| *mapped == Some(row_index))?;
    Some((start, end))
}

struct ClickContext<'a> {
    socket: &'a Path,
    server_identity: &'a str,
    source_pane: &'a PaneInstance,
}

#[derive(Debug, Clone, Copy)]
struct ClickPosition {
    row: u16,
    column: u16,
}

fn handle_left_click(
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
            Some(HeaderAction::CycleViewMode) => apply_local_sidebar_key(state, sidebar, "v"),
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
                state.version = state.version.saturating_add(1);
            }
            dispatch_click_action(context, mark_ui, ClickAction::JumpPane(pane_instance));
        }
        Some(RowClickIntent::Jump(_)) | None => {}
    }
    Ok(())
}

fn dispatch_click_action(
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

fn mark_done_target(
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

fn queue_mark_complete_for_selection(
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarCloseCommand {
    program: PathBuf,
    args: Vec<String>,
}

fn sidebar_close_command(exe: &Path, window: &str) -> SidebarCloseCommand {
    SidebarCloseCommand {
        program: exe.to_path_buf(),
        args: vec![
            "sidebar".to_string(),
            "close".to_string(),
            "--window".to_string(),
            window.to_string(),
        ],
    }
}

fn spawn_detached_sidebar_close(exe: &Path, window: &str) -> Result<()> {
    let command_spec = sidebar_close_command(exe, window);
    let mut command = Command::new(&command_spec.program);
    command
        .args(&command_spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .with_context(|| format!("failed to spawn sidebar close for window {window}"))?;
    Ok(())
}

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
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, MouseButton, MouseEventKind,
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

use crate::config::{Config, SidebarLiveConfig};
use crate::daemon::protocol::v2::ResolvedSnapshot;
use crate::sidebar::client::{
    SubscriptionUpdate, send_sidebar_jump_v2, send_sidebar_mark_complete_v2, subscribe_v2,
};
use crate::sidebar::preview::{guarded_capture_pane_args, open_preview_floating_pane};
use crate::sidebar::render::{
    HeaderAction, HeaderLayout, JumpRowAction, SidebarRenderTheme, build_footer_line,
    build_header_layout_with_counts, display_width, header_hit_test, jump_row_action_at,
    render_header_lines, render_lines_with_indices,
};
use crate::sidebar::state::{SidebarAction, SidebarState, StatusFilter};
use crate::sidebar::tree::{
    BadgeCounts, SidebarProjection, SidebarRow, SidebarRowKind, project_sidebar, row_refs,
};
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

const LIVE_CARD_MIN_WIDTH: u16 = 24;

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
    let (tx, rx) = mpsc::channel();
    let config_hash = crate::daemon::lifecycle::config_hash(config);
    subscribe_v2(socket, server_identity, &config_hash, tx)?;
    let theme = SidebarRenderTheme::from_app_config(config);
    let (live_request_tx, live_request_rx) = mpsc::channel();
    let (live_result_tx, live_result_rx) = mpsc::channel();
    spawn_live_capture_worker(live_request_rx, live_result_tx);
    let runtime_config = RunLoopConfig {
        app: config,
        theme: &theme,
        preview_history_lines: config.sidebar.preview.history_lines,
        live: &config.sidebar.live,
        live_capture_tx: &live_request_tx,
        live_capture_rx: &live_result_rx,
    };
    let result = run_loop(
        &mut terminal,
        RunLoopIo {
            socket,
            server_identity,
            snapshots: &rx,
            runner: &runner,
            env,
            sidebar_instance: &sidebar_instance,
            control: &control,
        },
        runtime_config,
    );
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
    }
    let _ = config;
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

    fn pane(pane_pid: u32) -> crate::daemon::protocol::v2::PanePresentation {
        crate::daemon::protocol::v2::PanePresentation {
            pane_instance: crate::pane_state::PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid,
            },
            session_links: vec![crate::daemon::protocol::v2::SessionLinkPresentation {
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
            diagnostic: None,
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

    fn resolved_pane(
        pane_id: &str,
        pane_pid: u32,
        session_id: &str,
    ) -> crate::daemon::protocol::v2::PanePresentation {
        use crate::pane_state::{
            AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneState, StateId, TaskState,
        };
        let pane_instance = crate::pane_state::PaneInstance {
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
        crate::daemon::protocol::v2::PanePresentation {
            pane_instance: pane_instance.clone(),
            session_links: vec![crate::daemon::protocol::v2::SessionLinkPresentation {
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
            stored: Some(crate::pane_state::StoredStateDescriptor::Canonical {
                version: canonical.version(),
            }),
            resolved: Some(crate::pane_state::ResolvedPaneState {
                canonical,
                window_id: "@1".to_string(),
                pane_id: pane_id.to_string(),
                current_path: "/tmp/app".to_string(),
                badge: crate::daemon::session_badge::BadgeState::Working,
            }),
            diagnostic: None,
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
            crate::pane_state::PaneInstance {
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
            crate::pane_state::PaneInstance {
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

        assert_eq!(
            state.selection,
            Some(crate::sidebar::tree::chat_row_id(&agent.pane_instance))
        );
        assert_eq!(
            state.return_target,
            Some(crate::pane_state::PaneInstance {
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

        assert_eq!(
            state.selection,
            Some(crate::sidebar::tree::chat_row_id(&direct.pane_instance))
        );
    }

    #[test]
    fn persisted_preferences_seed_view_filter_and_global_expansion() {
        let mut snapshot = snapshot(10);
        snapshot.sidebar_model.order.view_mode = crate::sidebar::state::ViewMode::ByCategory;
        snapshot.sidebar_model.order.filter = StatusFilter::DoneOnly;
        snapshot.sidebar_model.expansion.overrides =
            std::collections::BTreeSet::from(["category::work".to_string()]);
        let mut state = SidebarState {
            selection: Some("chat::%7::70".to_string()),
            collapsed: std::collections::BTreeSet::from(["repo::misc::app".to_string()]),
            scroll: 4,
            return_target: Some(crate::pane_state::PaneInstance {
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

        assert_eq!(state.view_mode, crate::sidebar::state::ViewMode::ByCategory);
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
    fn unobserved_expansion_ack_keeps_optimistic_open_state() {
        let row_id = "chat::%1::10".to_string();
        let mut pending = BTreeMap::from([(
            row_id.clone(),
            PendingExpansion {
                overridden: true,
                acknowledged_revision: Some(12),
            },
        )]);
        let mut intermediate = snapshot(10);
        intermediate.snapshot_revision = 11;
        intermediate.sidebar_model.expansion.version = 1;
        let mut state = SidebarState {
            collapsed: BTreeSet::from([row_id.clone()]),
            ..SidebarState::default()
        };

        assert!(!discard_acknowledged_expansions(
            &mut pending,
            intermediate.snapshot_revision
        ));
        apply_expansion_snapshot(&mut state, &intermediate, &pending);

        assert!(state.collapsed.contains(&row_id));
        assert_eq!(pending.len(), 1);

        let mut acknowledged = intermediate;
        acknowledged.snapshot_revision = 12;
        acknowledged.sidebar_model.expansion.version = 2;
        acknowledged
            .sidebar_model
            .expansion
            .overrides
            .insert(row_id.clone());
        assert!(discard_acknowledged_expansions(
            &mut pending,
            acknowledged.snapshot_revision
        ));
        apply_expansion_snapshot(&mut state, &acknowledged, &pending);

        assert!(state.collapsed.contains(&row_id));
        assert!(pending.is_empty());
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
            view_mode: crate::sidebar::state::ViewMode::Flat,
            ..SidebarState::default()
        };

        let rows = project_view(&snapshot, &Config::default(), &state).rows;
        let first_row = rows
            .iter()
            .find(|row| row.id == crate::sidebar::tree::chat_row_id(&first.pane_instance))
            .unwrap();
        let second_row = rows
            .iter()
            .find(|row| row.id == crate::sidebar::tree::chat_row_id(&second.pane_instance))
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
        snapshot.sidebar_model.order.filter = StatusFilter::DoneOnly;
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
        assert_eq!(
            state.selection,
            Some(crate::sidebar::tree::chat_row_id(&agent.pane_instance))
        );
    }

    #[test]
    fn mouse_coordinates_map_through_header_scroll_and_rendered_rows() {
        let row = |id: &str| SidebarRow {
            id: id.to_string(),
            kind: SidebarRowKind::Chat,
            depth: 0,
            label: id.to_string(),
            chat_count: 1,
            rollup: crate::hook::RollupLevel::Running,
            badge_state: Some(crate::daemon::session_badge::BadgeState::Working),
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
            row_for_click_with_indices(&sidebar, 2, 2, 1, &row_indices).map(|row| row.id.as_str()),
            Some("first")
        );
        assert_eq!(
            row_for_click_with_indices(&sidebar, 3, 2, 1, &row_indices).map(|row| row.id.as_str()),
            Some("second")
        );
    }

    #[test]
    fn ansi_stripping_removes_csi_and_osc_sequences() {
        assert_eq!(
            strip_ansi("plain\u{1b}[31mred\u{1b}[0m\u{1b}]0;title\u{7}tail"),
            "plainredtail"
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
        next.diagnostics
            .push(crate::daemon::protocol::v2::DaemonDiagnostic {
                code: crate::daemon::protocol::v2::ErrorCode::PersistFailed,
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
        degraded
            .diagnostics
            .push(crate::daemon::protocol::v2::DaemonDiagnostic {
                code: crate::daemon::protocol::v2::ErrorCode::HookCollision,
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
    fn current_pane_quarantine_degrades_connection() {
        let (tx, rx) = mpsc::channel();
        let mut current = None;
        let mut connection = ConnectionState::Connecting;
        let mut snapshot = snapshot(10);
        let pane_instance = snapshot.panes[0].pane_instance.clone();
        snapshot.panes[0].diagnostic = Some(crate::pane_state::PaneStateLoadError {
            pane_instance,
            quarantine_id: "quarantine-1".to_string(),
            message: "invalid pane state".to_string(),
        });
        tx.send(SubscriptionUpdate::Connected(Box::new(snapshot)))
            .unwrap();

        drain_snapshot_updates(&rx, &mut current, &mut connection);

        assert!(
            matches!(connection, ConnectionState::Degraded(message) if message.contains("quarantined"))
        );
    }

    #[test]
    fn stale_selection_is_cleared_on_pane_id_reuse() {
        let snapshot = snapshot(11);
        let mut state = SidebarState {
            selection: Some(crate::sidebar::tree::chat_row_id(
                &crate::pane_state::PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 10,
                },
            )),
            ..SidebarState::default()
        };

        clear_stale_pane_selection(&snapshot, &mut state);

        assert!(state.selection.is_none());
    }

    #[test]
    fn mark_complete_never_retargets_reused_pane_id() {
        let mut snapshot = snapshot(11);
        snapshot.panes[0].stored = Some(crate::pane_state::StoredStateDescriptor::Canonical {
            version: crate::pane_state::StateVersion {
                state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                    .unwrap(),
                agent_epoch: 1,
                revision: 1,
            },
        });
        let stale = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        };
        let current = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 11,
        };

        assert!(mark_done_target(&snapshot, &stale).is_none());
        assert!(mark_done_target(&snapshot, &current).is_some());
    }

    #[test]
    fn keyboard_and_mouse_mark_complete_queue_the_same_versioned_pane_without_retargeting() {
        let pane_instance = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let version = crate::pane_state::StateVersion {
            state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            agent_epoch: 3,
            revision: 9,
        };
        let mut original = snapshot(101);
        original.panes[0].stored = Some(crate::pane_state::StoredStateDescriptor::Canonical {
            version: version.clone(),
        });
        let jump = SidebarRow {
            id: "jump::%1::101".to_string(),
            kind: SidebarRowKind::Jump,
            depth: 2,
            label: "jump".to_string(),
            chat_count: 0,
            rollup: crate::hook::RollupLevel::Running,
            badge_state: None,
            expanded: true,
            pane_id: Some("%1".to_string()),
            git: None,
            active: true,
            meta: None,
        };
        let state = SidebarState {
            selection: Some(jump.id.clone()),
            ..SidebarState::default()
        };
        let sidebar = SidebarView {
            state: state.clone(),
            rows: vec![jump.clone()],
            counts: BadgeCounts::default(),
        };

        let (keyboard_tx, keyboard_rx) = mpsc::channel();
        queue_mark_complete_for_selection(
            &original,
            &sidebar,
            &keyboard_tx,
            &mut MarkCompleteUi::default(),
        );
        let keyboard = keyboard_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let (mouse_tx, mouse_rx) = mpsc::channel();
        let env = BTreeMap::new();
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let theme = SidebarRenderTheme::default();
        let source_pane = crate::pane_state::PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 909,
        };
        let context = ClickContext {
            socket: Path::new("/unused"),
            server_identity: "test",
            runner: &runner,
            env: &env,
            theme: &theme,
            preview_history_lines: 2000,
            live_lines: 0,
            mark_complete_tx: &mouse_tx,
            source_pane: &source_pane,
        };
        let width = crossterm::terminal::size().unwrap_or((80, 24)).0;
        let header = build_header_layout_with_counts(&state, width, &theme, sidebar.counts);
        let mut mouse_state = state.clone();
        handle_left_click(
            &context,
            &original,
            &mut mouse_state,
            &sidebar,
            &mut MarkCompleteUi::default(),
            ClickPosition {
                row: header.row_count(),
                column: (crate::sidebar::render::jump_row_action_start(&jump) + 8 + 11) as u16,
                scroll: 0,
            },
        )
        .unwrap();
        let mouse = mouse_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert_eq!(keyboard.pane_instance, pane_instance);
        assert_eq!(keyboard.expected, version);
        assert_eq!(mouse.pane_instance, keyboard.pane_instance);
        assert_eq!(mouse.expected, keyboard.expected);

        let mut reused = original;
        reused.panes[0].pane_instance.pane_pid = 202;
        let (stale_keyboard_tx, stale_keyboard_rx) = mpsc::channel();
        queue_mark_complete_for_selection(
            &reused,
            &sidebar,
            &stale_keyboard_tx,
            &mut MarkCompleteUi::default(),
        );
        assert!(stale_keyboard_rx.try_recv().is_err());

        let (stale_mouse_tx, stale_mouse_rx) = mpsc::channel();
        let stale_context = ClickContext {
            mark_complete_tx: &stale_mouse_tx,
            ..context
        };
        handle_left_click(
            &stale_context,
            &reused,
            &mut mouse_state,
            &sidebar,
            &mut MarkCompleteUi::default(),
            ClickPosition {
                row: header.row_count(),
                column: (crate::sidebar::render::jump_row_action_start(&jump) + 8 + 11) as u16,
                scroll: 0,
            },
        )
        .unwrap();
        assert!(stale_mouse_rx.try_recv().is_err());
    }

    #[test]
    fn live_capture_guards_pid_and_capture_in_one_tmux_command() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let pane = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        };
        let args = guarded_capture_pane_args(&pane, &["-p", "-e"]);
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        runner.stub(&refs, "one\ntwo\n");

        assert_eq!(capture_live_pane(&runner, &pane), "one\ntwo\n");

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "if-shell");
        assert_eq!(calls[0][3], "%1");
        assert_eq!(calls[0][4], "#{==:#{pane_pid},10}");
        assert!(calls[0][5].contains("capture-pane"));
        assert!(calls[0][5].contains("%1"));
    }

    #[test]
    fn live_capture_does_not_read_replacement_after_pane_id_reuse() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let stale = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        };
        let args = guarded_capture_pane_args(&stale, &["-p", "-e"]);
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        // tmux takes this false branch when %1 now belongs to a different PID.
        runner.stub(&refs, "");

        assert!(capture_live_pane(&runner, &stale).is_empty());
        assert_eq!(runner.calls().len(), 1);
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("capture-pane"))
        );
    }

    #[test]
    fn live_capture_result_requires_full_pane_instance() {
        let current = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 11,
        };
        let stale = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        };
        let mut live = LiveState {
            pane_instance: Some(current.clone()),
            requested_lines: 2,
            capture_in_flight: true,
            ..LiveState::default()
        };

        apply_live_capture_result(&mut live, &stale, "replacement\noutput\n");
        assert!(live.lines.is_empty());
        assert!(live.capture_in_flight);

        apply_live_capture_result(&mut live, &current, "one\ntwo\nthree\n");
        assert_eq!(live.lines, vec!["two".to_string(), "three".to_string()]);
        assert!(!live.capture_in_flight);
    }

    #[test]
    fn degraded_empty_message_takes_priority_over_healthy_empty() {
        let lines = connection_empty_lines(
            &ConnectionState::Degraded("quarantined".to_string()),
            &SidebarRenderTheme::default(),
            80,
        )
        .unwrap();
        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Degraded: quarantined"));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiExit {
    Quit,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum ConnectionState {
    #[default]
    Connecting,
    Connected,
    Degraded(String),
    Disconnected,
}

impl ConnectionState {
    fn label(&self) -> Option<&str> {
        match self {
            Self::Connecting => Some("connecting"),
            Self::Connected => None,
            Self::Degraded(_) => Some("degraded"),
            Self::Disconnected => Some("disconnected · reconnecting"),
        }
    }

    fn notice(&self) -> Option<Notice<'_>> {
        self.label().map(|message| Notice {
            message,
            level: match self {
                Self::Connecting => NoticeLevel::Progress,
                Self::Degraded(_) | Self::Disconnected => NoticeLevel::Failure,
                Self::Connected => NoticeLevel::Success,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LiveMode {
    #[default]
    Tail,
    Events,
}

#[derive(Debug, Clone, Default)]
struct LiveState {
    mode: LiveMode,
    pane_instance: Option<crate::pane_state::PaneInstance>,
    lines: Vec<String>,
    last_capture: Option<Instant>,
    requested_lines: u16,
    capture_in_flight: bool,
    cut_markers: Vec<String>,
}

impl LiveState {
    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            LiveMode::Tail => LiveMode::Events,
            LiveMode::Events => LiveMode::Tail,
        };
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RunLoopConfig<'a> {
    pub app: &'a Config,
    pub theme: &'a SidebarRenderTheme,
    pub preview_history_lines: u32,
    pub live: &'a SidebarLiveConfig,
    pub live_capture_tx: &'a mpsc::Sender<crate::pane_state::PaneInstance>,
    pub live_capture_rx: &'a mpsc::Receiver<(crate::pane_state::PaneInstance, String)>,
}

struct RunLoopIo<'a> {
    socket: &'a Path,
    server_identity: &'a str,
    snapshots: &'a mpsc::Receiver<SubscriptionUpdate>,
    runner: &'a dyn TmuxRunner,
    env: &'a BTreeMap<String, String>,
    sidebar_instance: &'a crate::pane_state::PaneInstance,
    control: &'a crate::sidebar::control::ControlListener,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SidebarView {
    state: SidebarState,
    rows: Vec<SidebarRow>,
    counts: BadgeCounts,
}

struct MarkCompleteRequest {
    pane_instance: crate::pane_state::PaneInstance,
    expected: crate::pane_state::StateVersion,
}

struct MarkCompleteResult {
    pane_instance: crate::pane_state::PaneInstance,
    result: Result<()>,
}

struct ReorderRequest {
    expected_version: u64,
    manual_order: Vec<crate::sidebar::state::RepoId>,
    manual_chat_order: Vec<String>,
}

struct ReorderResult(Result<()>);

struct PreferenceRequest {
    expected_version: u64,
    view_mode: crate::sidebar::state::ViewMode,
    filter: StatusFilter,
}

struct PreferenceResult(Result<()>);

struct ExpansionRequest {
    expected_version: u64,
    row_id: String,
    overridden: bool,
}

struct ExpansionResult {
    row_id: String,
    overridden: bool,
    result: Result<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingExpansion {
    overridden: bool,
    acknowledged_revision: Option<u64>,
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
    pending: std::collections::BTreeSet<crate::pane_state::PaneInstance>,
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

fn spawn_reorder_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<ReorderRequest>,
    tx: mpsc::Sender<ReorderResult>,
) {
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let result = crate::sidebar::client::send_sidebar_update_manual_order_v2(
                &socket,
                &server_identity,
                request.expected_version,
                request.manual_order,
                request.manual_chat_order,
            );
            if tx.send(ReorderResult(result)).is_err() {
                return;
            }
        }
    });
}

fn spawn_preference_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<PreferenceRequest>,
    tx: mpsc::Sender<PreferenceResult>,
) {
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let mut expected_version = request.expected_version;
            let mut stale_retries = 0_u8;
            let result = loop {
                match crate::sidebar::client::send_sidebar_update_view_preferences_v2(
                    &socket,
                    &server_identity,
                    expected_version,
                    request.view_mode,
                    request.filter,
                ) {
                    Ok(()) => break Ok(()),
                    Err(error) if error.to_string().contains("StaleStateIdentity") => {
                        if stale_retries >= 3 {
                            break Err(error);
                        }
                        stale_retries += 1;
                        match crate::sidebar::client::query_resolved_snapshot_v2(
                            &socket,
                            &server_identity,
                        ) {
                            Ok(snapshot)
                                if snapshot.sidebar_model.order.version != expected_version =>
                            {
                                expected_version = snapshot.sidebar_model.order.version;
                            }
                            Ok(_) => break Err(error),
                            Err(query_error) => break Err(query_error),
                        }
                    }
                    Err(error) => break Err(error),
                }
            };
            if tx.send(PreferenceResult(result)).is_err() {
                return;
            }
        }
    });
}

fn spawn_expansion_worker(
    socket: PathBuf,
    server_identity: String,
    rx: mpsc::Receiver<ExpansionRequest>,
    tx: mpsc::Sender<ExpansionResult>,
) {
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv() {
            let mut expected_version = request.expected_version;
            let mut stale_retries = 0_u8;
            let result = loop {
                match crate::sidebar::client::send_sidebar_set_expansion_override_v2(
                    &socket,
                    &server_identity,
                    expected_version,
                    request.row_id.clone(),
                    request.overridden,
                ) {
                    Ok(snapshot_revision) => break Ok(snapshot_revision),
                    Err(error) if error.to_string().contains("StaleStateIdentity") => {
                        if stale_retries >= 3 {
                            break Err(error);
                        }
                        stale_retries += 1;
                        match crate::sidebar::client::query_resolved_snapshot_v2(
                            &socket,
                            &server_identity,
                        ) {
                            Ok(snapshot)
                                if snapshot.sidebar_model.expansion.version != expected_version =>
                            {
                                expected_version = snapshot.sidebar_model.expansion.version;
                            }
                            Ok(_) => break Err(error),
                            Err(query_error) => break Err(query_error),
                        }
                    }
                    Err(error) => break Err(error),
                }
            };
            if tx
                .send(ExpansionResult {
                    row_id: request.row_id,
                    overridden: request.overridden,
                    result,
                })
                .is_err()
            {
                return;
            }
        }
    });
}

fn queue_reorder(
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    up: bool,
    tx: &mpsc::Sender<ReorderRequest>,
    ui: &mut MarkCompleteUi,
) {
    let Some(selection) = sidebar.state.selection.as_deref() else {
        return;
    };
    let Some(selected) = sidebar.rows.iter().find(|row| row.id == selection) else {
        return;
    };
    let mut manual_order = snapshot.sidebar_model.order.manual_order.clone();
    let mut manual_chat_order = snapshot.sidebar_model.order.manual_chat_order.clone();
    let changed = match selected.kind {
        SidebarRowKind::Chat => {
            for pane_id in sidebar
                .rows
                .iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .filter_map(|row| row.pane_id.as_ref())
            {
                if !manual_chat_order.contains(pane_id) {
                    manual_chat_order.push(pane_id.clone());
                }
            }
            selected
                .pane_id
                .as_ref()
                .is_some_and(|pane_id| move_item(&mut manual_chat_order, pane_id, up))
        }
        SidebarRowKind::Repo => {
            for repo in sidebar
                .rows
                .iter()
                .filter(|row| row.kind == SidebarRowKind::Repo)
                .filter_map(|row| crate::sidebar::state::RepoId::from_row_id(&row.id))
            {
                if !manual_order.contains(&repo) {
                    manual_order.push(repo);
                }
            }
            crate::sidebar::state::RepoId::from_row_id(&selected.id)
                .is_some_and(|repo| move_item(&mut manual_order, &repo, up))
        }
        _ => false,
    };
    if !changed {
        return;
    }
    if tx
        .send(ReorderRequest {
            expected_version: snapshot.sidebar_model.order.version,
            manual_order,
            manual_chat_order,
        })
        .is_err()
    {
        ui.set_toast(
            "reorder worker unavailable".to_string(),
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

fn move_item<T: PartialEq>(items: &mut [T], selected: &T, up: bool) -> bool {
    let Some(index) = items.iter().position(|item| item == selected) else {
        return false;
    };
    let target = if up {
        index.checked_sub(1)
    } else {
        (index + 1 < items.len()).then_some(index + 1)
    };
    let Some(target) = target else {
        return false;
    };
    items.swap(index, target);
    true
}

fn queue_mark_complete(
    tx: &mpsc::Sender<MarkCompleteRequest>,
    ui: &mut MarkCompleteUi,
    pane_instance: crate::pane_state::PaneInstance,
    expected: crate::pane_state::StateVersion,
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

fn drain_mark_complete_results(rx: &mpsc::Receiver<MarkCompleteResult>, ui: &mut MarkCompleteUi) {
    while let Ok(result) = rx.try_recv() {
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
        runner,
        env,
        sidebar_instance,
        control,
    } = io;
    let theme = config.theme;
    let preview_history_lines = config.preview_history_lines;
    let live_config = config.live;
    let mut current: Option<ResolvedSnapshot> = None;
    let mut connection = ConnectionState::Connecting;
    let mut last_known_rows: Option<(Vec<SidebarRow>, BadgeCounts)> = None;
    let mut sidebar_state = SidebarState::default();
    let mut initial_context_seeded = false;
    let mut last_queued_preferences = None;
    let mut last_expansion_view: Option<BTreeSet<String>> = None;
    let mut last_expansion_version = None;
    let mut pending_expansions = BTreeMap::<String, PendingExpansion>::new();
    let (mark_request_tx, mark_request_rx) = mpsc::channel();
    let (mark_result_tx, mark_result_rx) = mpsc::channel();
    spawn_mark_complete_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        mark_request_rx,
        mark_result_tx,
    );
    let (reorder_request_tx, reorder_request_rx) = mpsc::channel();
    let (reorder_result_tx, reorder_result_rx) = mpsc::channel();
    spawn_reorder_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        reorder_request_rx,
        reorder_result_tx,
    );
    let (preference_request_tx, preference_request_rx) = mpsc::channel();
    let (preference_result_tx, preference_result_rx) = mpsc::channel();
    spawn_preference_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        preference_request_rx,
        preference_result_tx,
    );
    let (expansion_request_tx, expansion_request_rx) = mpsc::channel();
    let (expansion_result_tx, expansion_result_rx) = mpsc::channel();
    spawn_expansion_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        expansion_request_rx,
        expansion_result_tx,
    );
    let mut mark_ui = MarkCompleteUi::default();
    let mut live = LiveState {
        requested_lines: live_rows_requested(live_config),
        cut_markers: live_config.cut_markers.clone(),
        ..LiveState::default()
    };
    loop {
        drain_snapshot_updates(rx, &mut current, &mut connection);
        if !initial_context_seeded && let Some(snapshot) = current.as_ref() {
            seed_persisted_sidebar_preferences(snapshot, &mut sidebar_state);
            last_queued_preferences = Some((sidebar_state.view_mode, sidebar_state.filter));
            last_expansion_view = Some(sidebar_state.collapsed.clone());
            last_expansion_version = Some(snapshot.sidebar_model.expansion.version);
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
            initial_context_seeded = true;
        }
        if let Some(snapshot) = current.as_ref() {
            clear_stale_pane_selection(snapshot, &mut sidebar_state);
        }
        drain_mark_complete_results(&mark_result_rx, &mut mark_ui);
        while let Ok(ReorderResult(result)) = reorder_result_rx.try_recv() {
            let (message, level, duration) = match result {
                Ok(()) => (
                    "order saved".to_string(),
                    NoticeLevel::Success,
                    Duration::from_secs(3),
                ),
                Err(error) if error.to_string().contains("Stale") => (
                    "order changed elsewhere; retry".to_string(),
                    NoticeLevel::Warning,
                    Duration::from_secs(5),
                ),
                Err(error) => (
                    format!("reorder failed: {error}"),
                    NoticeLevel::Failure,
                    Duration::from_secs(5),
                ),
            };
            mark_ui.set_toast(message, level, duration);
        }
        while let Ok(PreferenceResult(result)) = preference_result_rx.try_recv() {
            if let Err(error) = result {
                mark_ui.set_toast(
                    format!("preference save failed: {error}"),
                    NoticeLevel::Failure,
                    Duration::from_secs(5),
                );
            }
        }
        if let (Some(previous), Some(snapshot)) = (last_expansion_view.as_ref(), current.as_ref()) {
            for row_id in previous
                .symmetric_difference(&sidebar_state.collapsed)
                .cloned()
                .collect::<Vec<_>>()
            {
                let overridden = sidebar_state.collapsed.contains(&row_id);
                pending_expansions.insert(
                    row_id.clone(),
                    PendingExpansion {
                        overridden,
                        acknowledged_revision: None,
                    },
                );
                if expansion_request_tx
                    .send(ExpansionRequest {
                        expected_version: snapshot.sidebar_model.expansion.version,
                        row_id: row_id.clone(),
                        overridden,
                    })
                    .is_err()
                {
                    if pending_expansions
                        .get(&row_id)
                        .is_some_and(|pending| pending.overridden == overridden)
                    {
                        pending_expansions.remove(&row_id);
                    }
                    mark_ui.set_toast(
                        "expansion worker unavailable".to_string(),
                        NoticeLevel::Failure,
                        Duration::from_secs(5),
                    );
                }
            }
            last_expansion_view = Some(sidebar_state.collapsed.clone());
        }
        while let Ok(result) = expansion_result_rx.try_recv() {
            let is_current = pending_expansions
                .get(&result.row_id)
                .is_some_and(|pending| pending.overridden == result.overridden);
            if !is_current {
                continue;
            }
            match result.result {
                Ok(acknowledged_revision) => {
                    if current
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.snapshot_revision >= acknowledged_revision)
                    {
                        pending_expansions.remove(&result.row_id);
                        last_expansion_version = None;
                    } else if let Some(pending) = pending_expansions.get_mut(&result.row_id) {
                        pending.acknowledged_revision = Some(acknowledged_revision);
                    }
                }
                Err(error) => {
                    pending_expansions.remove(&result.row_id);
                    last_expansion_version = None;
                    mark_ui.set_toast(
                        format!("expansion save failed: {error}"),
                        NoticeLevel::Failure,
                        Duration::from_secs(5),
                    );
                }
            }
        }
        if let Some(snapshot) = current.as_ref() {
            if discard_acknowledged_expansions(&mut pending_expansions, snapshot.snapshot_revision)
            {
                last_expansion_version = None;
            }
            if last_expansion_version != Some(snapshot.sidebar_model.expansion.version) {
                apply_expansion_snapshot(&mut sidebar_state, snapshot, &pending_expansions);
                last_expansion_view = Some(sidebar_state.collapsed.clone());
                last_expansion_version = Some(snapshot.sidebar_model.expansion.version);
            }
        }
        let preferences = (sidebar_state.view_mode, sidebar_state.filter);
        if last_queued_preferences.is_some_and(|previous| previous != preferences)
            && let Some(snapshot) = current.as_ref()
        {
            if preference_request_tx
                .send(PreferenceRequest {
                    expected_version: snapshot.sidebar_model.order.version,
                    view_mode: sidebar_state.view_mode,
                    filter: sidebar_state.filter,
                })
                .is_err()
            {
                mark_ui.set_toast(
                    "preference worker unavailable".to_string(),
                    NoticeLevel::Failure,
                    Duration::from_secs(5),
                );
            }
            last_queued_preferences = Some(preferences);
        }
        drain_control_messages(control, current.as_ref(), config.app, &mut sidebar_state)?;
        let context = ClickContext {
            socket,
            server_identity,
            runner,
            env,
            theme,
            preview_history_lines,
            live_lines: live.requested_lines,
            mark_complete_tx: &mark_request_tx,
            source_pane: sidebar_instance,
        };
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
            update_live_state(
                snapshot,
                &sidebar,
                live_config,
                &mut live,
                config.live_capture_tx,
                config.live_capture_rx,
            );
            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            let header =
                build_header_layout_with_counts(&sidebar.state, area.width, theme, sidebar.counts);
            let areas = compute_areas(area, &header, live.requested_lines);
            let rendered = render_lines_with_indices(
                &sidebar.rows,
                &sidebar.state,
                area.width as usize,
                theme,
            );
            let selected_row_index = sidebar
                .state
                .selection
                .as_deref()
                .and_then(|selection| sidebar.rows.iter().position(|row| row.id == selection));
            let selection_range = selected_row_index
                .and_then(|row_index| rendered_row_range(&rendered.row_indices, row_index));
            sidebar_state.scroll = resolve_scroll_range(
                sidebar_state.scroll,
                selection_range,
                rendered.lines.len(),
                areas.rows_height as usize,
            );
            draw_snapshot_with_theme_and_scroll_live(
                terminal,
                snapshot,
                &sidebar,
                DrawOptions {
                    theme,
                    scroll: sidebar_state.scroll,
                    live: Some(&live),
                    connection: &connection,
                    toast: mark_ui.notice(),
                },
            )?;
        } else {
            draw_connection_placeholder(terminal, &connection)?;
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(TuiExit::Quit),
                    KeyCode::Char('p') => {
                        if let Some(snapshot) = &current
                            && let Some(pane) = preview_pane_for_selection(&project_view(
                                snapshot,
                                config.app,
                                &sidebar_state,
                            ))
                        {
                            spawn_preview(runner, env, &pane, preview_history_lines);
                        }
                    }
                    KeyCode::Char('e') => live.toggle_mode(),
                    KeyCode::Char('d') => {
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
                        if let Some(snapshot) = &current {
                            let sidebar = project_view(snapshot, config.app, &sidebar_state);
                            apply_local_sidebar_key(&mut sidebar_state, &sidebar, "space");
                        }
                    }
                    KeyCode::Char(ch) => {
                        if let Some(snapshot) = &current {
                            let sidebar = project_view(snapshot, config.app, &sidebar_state);
                            if matches!(ch, 'J' | 'K') {
                                queue_reorder(
                                    snapshot,
                                    &sidebar,
                                    ch == 'K',
                                    &reorder_request_tx,
                                    &mut mark_ui,
                                );
                            } else {
                                apply_local_sidebar_key(
                                    &mut sidebar_state,
                                    &sidebar,
                                    &ch.to_string(),
                                );
                            }
                        }
                    }
                    KeyCode::Down | KeyCode::Up | KeyCode::Tab | KeyCode::BackTab => {
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
                        if let Some(snapshot) = &current
                            && let sidebar = project_view(snapshot, config.app, &sidebar_state)
                            && selection_is_detail_row(&sidebar)
                            && let Some(pane) = preview_pane_for_selection(&sidebar)
                        {
                            spawn_preview(runner, env, &pane, preview_history_lines);
                        } else if let Some(snapshot) = &current {
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
                    _ => {}
                },
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(snapshot) = &current {
                        let sidebar = project_view(snapshot, config.app, &sidebar_state);
                        let scroll = sidebar_state.scroll;
                        handle_left_click(
                            &context,
                            snapshot,
                            &mut sidebar_state,
                            &sidebar,
                            &mut mark_ui,
                            ClickPosition {
                                row: mouse.row,
                                column: mouse.column,
                                scroll,
                            },
                        )?;
                    }
                }
                _ => {}
            }
        }
    }
}

fn clear_stale_pane_selection(snapshot: &ResolvedSnapshot, state: &mut SidebarState) {
    let Some(selected) = state
        .selection
        .as_deref()
        .and_then(crate::sidebar::tree::pane_instance_from_row_id)
    else {
        return;
    };
    if !snapshot
        .panes
        .iter()
        .any(|pane| pane.pane_instance == selected)
    {
        state.selection = None;
        state.version = state.version.saturating_add(1);
    }
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
        let pane = crate::pane_state::PaneInstance {
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
    state.view_mode = snapshot.sidebar_model.order.view_mode;
    state.filter = snapshot.sidebar_model.order.filter;
    state.collapsed = snapshot.sidebar_model.expansion.overrides.clone();
}

fn discard_acknowledged_expansions(
    pending: &mut BTreeMap<String, PendingExpansion>,
    snapshot_revision: u64,
) -> bool {
    let acknowledged = pending
        .iter()
        .filter(|(_, pending)| {
            pending
                .acknowledged_revision
                .is_some_and(|revision| snapshot_revision >= revision)
        })
        .map(|(row_id, _)| row_id.clone())
        .collect::<Vec<_>>();
    let changed = !acknowledged.is_empty();
    for row_id in acknowledged {
        pending.remove(&row_id);
    }
    changed
}

fn apply_expansion_snapshot(
    state: &mut SidebarState,
    snapshot: &ResolvedSnapshot,
    pending: &BTreeMap<String, PendingExpansion>,
) {
    state.collapsed = snapshot.sidebar_model.expansion.overrides.clone();
    for (row_id, pending) in pending {
        if pending.overridden {
            state.collapsed.insert(row_id.clone());
        } else {
            state.collapsed.remove(row_id);
        }
    }
}

fn select_context_agent(
    snapshot: &ResolvedSnapshot,
    config: &Config,
    state: &mut SidebarState,
    direct_pane: Option<&crate::pane_state::PaneInstance>,
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
        let row_id = crate::sidebar::tree::chat_row_id(pane);
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
            let pane = crate::sidebar::tree::pane_instance_from_row_id(&row.id)?;
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
            let row_id = crate::sidebar::tree::pane_instance_from_row_id(&row_id)
                .map(|pane| crate::sidebar::tree::chat_row_id(&pane))
                .unwrap_or(row_id);
            state.selection = Some(row_id.clone());
            state.toggle_expanded(&row_id);
        }
        SidebarInputAction::FocusNextAttention | SidebarInputAction::FocusPreviousAttention => {
            let ids = sidebar
                .rows
                .iter()
                .filter(|row| {
                    row.kind == SidebarRowKind::Chat
                        && row.badge_state
                            == Some(crate::daemon::session_badge::BadgeState::Blocked)
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
        | SidebarInputAction::ReorderUp
        | SidebarInputAction::ReorderDown => {}
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
        .and_then(crate::sidebar::tree::pane_instance_from_row_id);
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
        Some(crate::sidebar::input::SidebarCommand::PreviewPane(_)) => {
            if let Some(pane) = selected_pane {
                spawn_preview(
                    context.runner,
                    context.env,
                    &pane,
                    context.preview_history_lines,
                );
            }
        }
        None => {}
    }
}

fn drain_control_messages(
    control: &crate::sidebar::control::ControlListener,
    snapshot: Option<&ResolvedSnapshot>,
    config: &Config,
    state: &mut SidebarState,
) -> Result<()> {
    while let Some(message) = control.try_recv()? {
        match message {
            crate::sidebar::control::ControlMessage::Input { key } => {
                if let Some(snapshot) = snapshot {
                    let sidebar = project_view(snapshot, config, state);
                    apply_local_sidebar_key(state, &sidebar, &key);
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
    Ok(())
}

fn apply_focus_message(
    snapshot: &ResolvedSnapshot,
    config: &Config,
    state: &mut SidebarState,
    pane_instance: crate::pane_state::PaneInstance,
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
) {
    loop {
        match rx.try_recv() {
            Ok(SubscriptionUpdate::Connecting) => *connection = ConnectionState::Connecting,
            Ok(SubscriptionUpdate::Connected(snapshot)) => {
                if let Some(message) = snapshot_degraded_message(&snapshot) {
                    *current = Some(*snapshot);
                    *connection = ConnectionState::Degraded(message);
                } else {
                    *current = Some(*snapshot);
                    *connection = ConnectionState::Connected;
                }
            }
            Ok(SubscriptionUpdate::Degraded(error)) => {
                *connection = ConnectionState::Degraded(error);
            }
            Ok(SubscriptionUpdate::Disconnected) => {
                *connection = ConnectionState::Disconnected;
            }
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                *connection = ConnectionState::Disconnected;
                return;
            }
        }
    }
}

fn snapshot_degraded_message(snapshot: &ResolvedSnapshot) -> Option<String> {
    crate::sidebar::current_degraded_message(snapshot)
}

fn live_rows_requested(config: &SidebarLiveConfig) -> u16 {
    if config.enabled { config.lines } else { 0 }
}

fn update_live_state(
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
    config: &SidebarLiveConfig,
    live: &mut LiveState,
    request_tx: &mpsc::Sender<crate::pane_state::PaneInstance>,
    result_rx: &mpsc::Receiver<(crate::pane_state::PaneInstance, String)>,
) {
    while let Ok((pane_instance, output)) = result_rx.try_recv() {
        apply_live_capture_result(live, &pane_instance, &output);
    }
    live.requested_lines = live_rows_requested(config);
    if live.requested_lines == 0 {
        live.pane_instance = None;
        live.lines.clear();
        live.last_capture = None;
        live.capture_in_flight = false;
        return;
    }
    let selected = preview_pane_for_selection(sidebar).filter(|pane| {
        snapshot
            .panes
            .iter()
            .any(|current| current.pane_instance == *pane)
    });
    if live.pane_instance != selected {
        live.pane_instance = selected;
        live.last_capture = None;
        live.lines.clear();
        live.capture_in_flight = false;
    }
    let Some(pane_instance) = live.pane_instance.clone() else {
        return;
    };
    let now = Instant::now();
    let interval = Duration::from_millis(config.interval_ms);
    let due = live
        .last_capture
        .map(|last| now.duration_since(last) >= interval)
        .unwrap_or(true);
    if !due {
        return;
    }
    if live.capture_in_flight {
        return;
    }
    if request_tx.send(pane_instance).is_ok() {
        live.capture_in_flight = true;
        live.last_capture = Some(now);
    }
}

fn apply_live_capture_result(
    live: &mut LiveState,
    pane_instance: &crate::pane_state::PaneInstance,
    output: &str,
) {
    if live.pane_instance.as_ref() != Some(pane_instance) {
        return;
    }
    live.lines = extract_tail(output, live.requested_lines as usize, &live.cut_markers);
    live.capture_in_flight = false;
}

fn spawn_live_capture_worker(
    request_rx: mpsc::Receiver<crate::pane_state::PaneInstance>,
    result_tx: mpsc::Sender<(crate::pane_state::PaneInstance, String)>,
) {
    std::thread::spawn(move || {
        let runner = SystemTmuxRunner::from_env(Duration::from_millis(500));
        while let Ok(pane_instance) = request_rx.recv() {
            let output = capture_live_pane(&runner, &pane_instance);
            if result_tx.send((pane_instance, output)).is_err() {
                break;
            }
        }
    });
}

fn capture_live_pane(runner: &dyn TmuxRunner, pane: &crate::pane_state::PaneInstance) -> String {
    let args = guarded_capture_pane_args(pane, &["-p", "-e"]);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    runner.run(&refs).unwrap_or_default()
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
struct ClickedRow {
    id: String,
    kind: SidebarRowKind,
    pane_id: Option<String>,
}

impl ClickedRow {
    fn from_row(row: &SidebarRow) -> Self {
        Self {
            id: row.id.clone(),
            kind: row.kind,
            pane_id: row.pane_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClickAction {
    ToggleRow(String),
    PreviewPane(crate::pane_state::PaneInstance),
    JumpPane(crate::pane_state::PaneInstance),
    MarkComplete {
        pane_instance: crate::pane_state::PaneInstance,
        expected: crate::pane_state::StateVersion,
    },
}

fn single_click_action(row: &ClickedRow) -> Option<ClickAction> {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Chat => {
            Some(ClickAction::ToggleRow(row.id.clone()))
        }
        SidebarRowKind::Detail => Some(ClickAction::ToggleRow(row.id.clone())),
        SidebarRowKind::Jump | SidebarRowKind::Zone => None,
    }
}

fn row_for_click_with_indices<'a>(
    sidebar: &'a SidebarView,
    row: u16,
    header_rows: u16,
    scroll: usize,
    row_indices: &[Option<usize>],
) -> Option<&'a SidebarRow> {
    if row < header_rows {
        return None;
    }
    let display_index = usize::from(row - header_rows) + scroll;
    let row_index = row_indices.get(display_index).and_then(|index| *index)?;
    sidebar.rows.get(row_index)
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
    draw_snapshot_with_theme_and_scroll_live(
        terminal,
        snapshot,
        sidebar,
        DrawOptions {
            theme,
            scroll,
            live: None,
            connection: &ConnectionState::Connected,
            toast: None,
        },
    )
}

#[derive(Clone, Copy)]
struct DrawOptions<'a> {
    theme: &'a SidebarRenderTheme,
    scroll: usize,
    live: Option<&'a LiveState>,
    connection: &'a ConnectionState,
    toast: Option<Notice<'a>>,
}

fn draw_snapshot_with_theme_and_scroll_live<B: Backend>(
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
        live,
        connection,
        toast,
    } = options;
    let header = build_header_layout_with_counts(&sidebar.state, area.width, theme, sidebar.counts);
    let live_lines = live.map(|live| live.requested_lines).unwrap_or(0);
    let areas = compute_areas(area, &header, live_lines);
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
        let rendered =
            render_lines_with_indices(&sidebar.rows, &sidebar.state, area.width as usize, theme);
        rendered
            .lines
            .into_iter()
            .skip(scroll)
            .take(areas.rows_height as usize)
            .map(ListItem::new)
            .collect::<Vec<_>>()
    };
    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, rows_area);
    if areas.live_rows > 0
        && let Some(live) = live
    {
        let live_area = Rect {
            y: area.y + areas.header_rows + areas.rows_height,
            height: areas.live_rows,
            ..area
        };
        frame.render_widget(
            Paragraph::new(render_live_lines(
                snapshot,
                live,
                areas.live_rows,
                area.width,
                crate::sidebar::tree::now_epoch_secs(),
                theme,
            )),
            live_area,
        );
    }
    if areas.footer_rows > 0 {
        let footer_area = Rect {
            y: area.y + areas.header_rows + areas.rows_height + areas.live_rows,
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
                "Filter: {} · tab: next · S-tab: previous · ≡ all: reset",
                filter.label()
            ),
            Style::default()
                .fg(theme.marker)
                .add_modifier(Modifier::DIM),
        )),
    ]
}

fn render_live_lines(
    snapshot: &ResolvedSnapshot,
    live: &LiveState,
    live_rows: u16,
    width: u16,
    now: i64,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    use ansi_to_tui::IntoText;

    let card = width >= LIVE_CARD_MIN_WIDTH;
    let body_limit = live_rows.saturating_sub(if card { 2 } else { 1 }) as usize;
    let (label, title_rest) = match live.mode {
        LiveMode::Tail => (
            "LIVE",
            live.pane_instance
                .as_ref()
                .map(|pane| format!(" · {}", pane.pane_id))
                .unwrap_or_default(),
        ),
        LiveMode::Events => ("EVENTS", String::new()),
    };
    let body = match live.mode {
        LiveMode::Tail => live.lines.clone(),
        LiveMode::Events => event_tail(snapshot, body_limit, now, theme),
    };
    let ansi = matches!(live.mode, LiveMode::Tail);

    if card {
        let width = width as usize;
        let title = format!("{label}{title_rest}");
        let title_width = display_width(&title).min(width.saturating_sub(3));
        let top_rule = width.saturating_sub(3).saturating_sub(title_width);
        let mut lines = vec![Line::from(vec![
            Span::styled("╭╴".to_string(), Style::default().fg(theme.marker)),
            Span::styled(
                label.to_string(),
                Style::default().fg(theme.live).add_modifier(Modifier::BOLD),
            ),
            Span::styled(title_rest, Style::default().fg(theme.detail)),
            Span::styled(
                format!("{}╮", "─".repeat(top_rule)),
                Style::default().fg(theme.marker),
            ),
        ])];
        let mut body_lines = body
            .into_iter()
            .take(body_limit)
            .map(|line| live_card_body_line(&line, ansi, width, theme))
            .collect::<Vec<_>>();
        while body_lines.len() < body_limit {
            body_lines.push(live_card_body_line("", false, width, theme));
        }
        lines.extend(body_lines);
        lines.push(Line::from(Span::styled(
            format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
            Style::default().fg(theme.marker),
        )));
        return lines;
    }

    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        Span::styled(
            label,
            Style::default().fg(theme.live).add_modifier(Modifier::BOLD),
        ),
        Span::styled(title_rest, Style::default().fg(theme.detail)),
    ])];
    lines.extend(body.into_iter().take(body_limit).map(|line| {
        let plain = || {
            Line::from(Span::styled(
                format!(" {}", strip_ansi(&line)),
                Style::default().fg(theme.detail),
            ))
        };
        if !ansi {
            return plain();
        }
        match format!(" {line}").into_text() {
            Ok(text) => text.lines.into_iter().next().unwrap_or_else(plain),
            Err(_) => plain(),
        }
    }));
    lines
}

fn live_card_body_line(
    raw: &str,
    ansi: bool,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    use ansi_to_tui::IntoText;

    let content_width = width.saturating_sub(2);
    let plain = || {
        let mut text = crate::sidebar::render::truncate_display(
            &format!(" {}", strip_ansi(raw)),
            content_width,
        );
        let used = display_width(&text);
        if used < content_width {
            text.push_str(&" ".repeat(content_width - used));
        }
        vec![Span::styled(text, Style::default().fg(theme.detail))]
    };
    let mut content = if ansi {
        match format!(" {raw}").into_text() {
            Ok(text) => text.lines.into_iter().next().map(|line| line.spans),
            Err(_) => None,
        }
        .unwrap_or_else(plain)
    } else {
        plain()
    };
    let content_used: usize = content
        .iter()
        .map(|span| display_width(&span.content))
        .sum();
    if content_used > content_width {
        content = truncate_spans_to_width(content, content_width);
    }
    let content_used: usize = content
        .iter()
        .map(|span| display_width(&span.content))
        .sum();
    if content_used < content_width {
        content.push(Span::raw(" ".repeat(content_width - content_used)));
    }
    let mut spans = vec![Span::styled(
        "│".to_string(),
        Style::default().fg(theme.marker),
    )];
    spans.extend(content);
    spans.push(Span::styled(
        "│".to_string(),
        Style::default().fg(theme.marker),
    ));
    Line::from(spans)
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

pub(crate) fn extract_tail(raw: &str, limit: usize, cut_markers: &[String]) -> Vec<String> {
    let all = raw.lines().map(str::trim_end).collect::<Vec<_>>();
    let cut = cut_index(&all, cut_markers).unwrap_or(all.len());
    let mut lines = all[..cut]
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    lines.drain(0..start);
    lines
}

const CUT_SCAN_TAIL: usize = 15;

fn cut_index(lines: &[&str], markers: &[String]) -> Option<usize> {
    let scan_start = lines.len().saturating_sub(CUT_SCAN_TAIL);
    markers
        .iter()
        .filter(|marker| !marker.is_empty())
        .filter_map(|marker| {
            lines[scan_start..]
                .iter()
                .rposition(|line| strip_ansi(line).contains(marker.as_str()))
                .map(|pos| scan_start + pos)
        })
        .min()
}

pub(crate) fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

fn event_tail(
    snapshot: &ResolvedSnapshot,
    limit: usize,
    now: i64,
    theme: &SidebarRenderTheme,
) -> Vec<String> {
    let mut events = snapshot
        .events
        .iter()
        .rev()
        .take(limit)
        .map(|event| {
            let elapsed = (now - event.at_epoch).max(0);
            let ago = if elapsed >= 60 {
                format!("{}m前", elapsed / 60)
            } else {
                format!("{elapsed}s前")
            };
            let from = event
                .from
                .map(|state| theme.badge_glyph(state).to_string())
                .unwrap_or_else(|| "·".to_string());
            format!(
                "{ago} {} {} → {}",
                crate::agent::display_agent_name(&event.agent),
                from,
                theme.badge_glyph(event.to)
            )
        })
        .collect::<Vec<_>>();
    events.reverse();
    events
}

pub(crate) struct SidebarAreas {
    pub(crate) header_rows: u16,
    pub(crate) rows_height: u16,
    pub(crate) live_rows: u16,
    pub(crate) footer_rows: u16,
}

pub(crate) fn compute_areas(area: Rect, header: &HeaderLayout, live_lines: u16) -> SidebarAreas {
    let header_rows = header.row_count().min(area.height);
    let remaining = area.height.saturating_sub(header_rows);
    let footer_rows = if area.width > 2 && area.height >= 12 && remaining > 1 {
        1
    } else {
        0
    };
    let live_rows = if live_lines > 0 && area.width > 2 && area.height >= 14 {
        let live_chrome = if area.width >= LIVE_CARD_MIN_WIDTH {
            2
        } else {
            1
        };
        (live_lines + live_chrome).min(remaining.saturating_sub(footer_rows))
    } else {
        0
    };
    SidebarAreas {
        header_rows,
        rows_height: remaining
            .saturating_sub(live_rows)
            .saturating_sub(footer_rows),
        live_rows,
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
    runner: &'a dyn TmuxRunner,
    env: &'a BTreeMap<String, String>,
    theme: &'a SidebarRenderTheme,
    preview_history_lines: u32,
    live_lines: u16,
    mark_complete_tx: &'a mpsc::Sender<MarkCompleteRequest>,
    source_pane: &'a crate::pane_state::PaneInstance,
}

#[derive(Debug, Clone, Copy)]
struct ClickPosition {
    row: u16,
    column: u16,
    scroll: usize,
}

fn handle_left_click(
    context: &ClickContext<'_>,
    snapshot: &ResolvedSnapshot,
    state: &mut SidebarState,
    sidebar: &SidebarView,
    mark_ui: &mut MarkCompleteUi,
    position: ClickPosition,
) -> Result<()> {
    let ClickPosition {
        row,
        column,
        scroll,
    } = position;
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let header =
        build_header_layout_with_counts(&sidebar.state, width, context.theme, sidebar.counts);
    if row < header.row_count() {
        match header_hit_test(&header, row, column) {
            Some(HeaderAction::CycleViewMode) => apply_local_sidebar_key(state, sidebar, "v"),
            Some(HeaderAction::SetFilter(filter)) => {
                apply_local_sidebar_key(state, sidebar, filter.key());
            }
            None => {}
        }
        return Ok(());
    }
    let areas = compute_areas(Rect::new(0, 0, width, height), &header, context.live_lines);
    if row >= areas.header_rows + areas.rows_height {
        return Ok(());
    }
    let rendered =
        render_lines_with_indices(&sidebar.rows, &sidebar.state, width as usize, context.theme);
    let Some(clicked) = row_for_click_with_indices(
        sidebar,
        row,
        header.row_count(),
        scroll,
        &rendered.row_indices,
    ) else {
        return Ok(());
    };
    if clicked.kind == SidebarRowKind::Jump {
        let clicked_pane = crate::sidebar::tree::pane_instance_from_row_id(&clicked.id);
        match jump_row_action_at(clicked, column) {
            Some(JumpRowAction::Jump) => {
                if let Some(pane_instance) = clicked_pane.clone().filter(|selected| {
                    snapshot
                        .panes
                        .iter()
                        .any(|pane| pane.pane_instance == *selected)
                }) {
                    dispatch_click_action(context, mark_ui, ClickAction::JumpPane(pane_instance));
                }
            }
            Some(JumpRowAction::Preview) => {
                if let Some(pane) = clicked_pane.clone() {
                    dispatch_click_action(context, mark_ui, ClickAction::PreviewPane(pane));
                }
            }
            Some(JumpRowAction::MarkDone) => {
                if let Some(pane) = clicked_pane
                    && let Some((pane_instance, expected)) = mark_done_target(snapshot, &pane)
                {
                    dispatch_click_action(
                        context,
                        mark_ui,
                        ClickAction::MarkComplete {
                            pane_instance,
                            expected,
                        },
                    );
                }
            }
            None => {}
        }
        return Ok(());
    }
    if let Some(action) = single_click_action(&ClickedRow::from_row(clicked)) {
        if let ClickAction::ToggleRow(row_id) = action {
            apply_local_sidebar_key(state, sidebar, &format!("toggle:{row_id}"));
        } else {
            dispatch_click_action(context, mark_ui, action);
        }
    }
    Ok(())
}

fn dispatch_click_action(
    context: &ClickContext<'_>,
    mark_ui: &mut MarkCompleteUi,
    action: ClickAction,
) {
    match action {
        ClickAction::ToggleRow(row_id) => {
            debug_assert!(!row_id.is_empty());
        }
        ClickAction::PreviewPane(pane) => {
            spawn_preview(
                context.runner,
                context.env,
                &pane,
                context.preview_history_lines,
            );
        }
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
        ClickAction::MarkComplete {
            pane_instance,
            expected,
        } => {
            queue_mark_complete(context.mark_complete_tx, mark_ui, pane_instance, expected);
        }
    }
}

fn mark_done_target(
    snapshot: &ResolvedSnapshot,
    pane_instance: &crate::pane_state::PaneInstance,
) -> Option<(
    crate::pane_state::PaneInstance,
    crate::pane_state::StateVersion,
)> {
    snapshot.panes.iter().find_map(|pane| {
        if &pane.pane_instance != pane_instance {
            return None;
        }
        let crate::pane_state::StoredStateDescriptor::Canonical { version } =
            pane.stored.as_ref()?
        else {
            return None;
        };
        Some((pane.pane_instance.clone(), version.clone()))
    })
}

fn mark_complete_target_for_selection(
    snapshot: &ResolvedSnapshot,
    sidebar: &SidebarView,
) -> Option<(
    crate::pane_state::PaneInstance,
    crate::pane_state::StateVersion,
)> {
    let pane = preview_pane_for_selection(sidebar)?;
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

fn preview_pane_for_selection(sidebar: &SidebarView) -> Option<crate::pane_state::PaneInstance> {
    let selection = sidebar.state.selection.as_deref()?;
    let row = sidebar.rows.iter().find(|row| row.id == selection)?;
    match row.kind {
        SidebarRowKind::Chat | SidebarRowKind::Jump | SidebarRowKind::Detail => {
            crate::sidebar::tree::pane_instance_from_row_id(&row.id)
        }
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Zone => None,
    }
}

fn selection_is_detail_row(sidebar: &SidebarView) -> bool {
    let Some(selection) = sidebar.state.selection.as_deref() else {
        return false;
    };
    sidebar
        .rows
        .iter()
        .any(|row| row.id == selection && row.kind == SidebarRowKind::Detail)
}

fn spawn_preview(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    pane: &crate::pane_state::PaneInstance,
    history_lines: u32,
) {
    if let Err(error) = open_preview_floating_pane(runner, env, pane, history_lines) {
        eprintln!("[vde-tmux] sidebar preview failed: {error:#}");
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

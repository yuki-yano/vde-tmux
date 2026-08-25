use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
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

use crate::config::Config;
use crate::daemon::protocol::v2::ResolvedSnapshot;
#[cfg(test)]
use crate::daemon::session_badge::BadgeState;
use crate::pane_state::PaneInstance;
use crate::sidebar::client::{SubscriptionUpdate, subscribe_v2};
use crate::sidebar::render::{
    SidebarRenderTheme, build_header_layout_with_counts, render_lines_with_indices,
};
use crate::sidebar::state::{SidebarPreferenceIntent, SidebarState};
use crate::sidebar::tree::{BadgeCounts, SidebarRow};
use crate::tmux::SystemTmuxRunner;

mod close;
mod control_flow;
mod dialog;
mod draw;
mod effects;
mod mouse;
mod projection;
mod terminal;
mod types;

#[allow(unused_imports)]
pub(crate) use draw::SidebarAreas;
pub(crate) use draw::{compute_areas, resolve_scroll_range};
pub use draw::{draw_connecting, draw_snapshot, draw_snapshot_with_theme};

use close::spawn_detached_sidebar_close;
use control_flow::{
    ConnectionState, ControlMessageContext, RenderGate, drain_control_messages,
    drain_snapshot_updates, resolve_current_window_id, sidebar_elapsed_clock_visible,
};
use dialog::{
    CategoryDialog, apply_category_intent_result, begin_category_edit, handle_category_edit_key,
};
use draw::{
    DrawOptions, clamp_scroll_range, draw_connection_placeholder,
    draw_snapshot_with_theme_and_scroll_options, rendered_selection_range,
};
use effects::{
    PreferenceIntentRequest, drain_mark_complete_results, drain_pane_pin_results, queue_reorder,
    spawn_category_intent_worker, spawn_mark_complete_worker, spawn_navigation_worker,
    spawn_pane_pin_worker, spawn_preference_intent_worker,
};
use mouse::{
    ClickPosition, handle_left_click, queue_mark_complete_for_selection,
    queue_pane_pin_for_selection, scroll_mouse_viewport,
};
use projection::{
    ViewportMove, activate_local_selection, apply_local_sidebar_key, apply_remote_navigation,
    apply_remote_sidebar_preferences, clear_stale_pane_selection, move_viewport_selection,
    project_view, refresh_current_agents, refresh_current_category, seed_initial_sidebar_context,
    seed_persisted_sidebar_preferences,
};
use terminal::{TerminalRestoreGuard, install_panic_restore_hook};
use types::{ClickContext, DrawnFrame, MarkCompleteUi, NavigationRequest, NoticeLevel};

#[cfg(test)]
mod test_support;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiExit {
    Quit,
    Disconnected,
    ConfigChanged { active_config_hash: String },
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
    let mut last_remote_preferences = None;
    let mut last_expansion_view: Option<BTreeSet<String>> = None;
    let mut last_remote_expansion: Option<BTreeSet<String>> = None;
    let mut last_remote_navigation_revision = 0;
    let mut last_queued_navigation: Option<NavigationRequest> = None;
    let (mark_request_tx, mark_request_rx) = mpsc::channel();
    let (mark_result_tx, mark_result_rx) = mpsc::channel();
    spawn_mark_complete_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        mark_request_rx,
        mark_result_tx,
    );
    let (pin_request_tx, pin_request_rx) = mpsc::channel();
    let (pin_result_tx, pin_result_rx) = mpsc::channel();
    spawn_pane_pin_worker(
        socket.to_path_buf(),
        server_identity.to_string(),
        pin_request_rx,
        pin_result_tx,
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
    let mut category_dialog: Option<CategoryDialog> = None;
    let mut next_category_dialog_request_id = 1_u64;
    loop {
        render_gate.mark_dirty_if(drain_snapshot_updates(rx, &mut current, &mut connection));
        if let ConnectionState::ConfigChanged(active_config_hash) = &connection {
            return Ok(TuiExit::ConfigChanged {
                active_config_hash: active_config_hash.clone(),
            });
        }
        if !initial_context_seeded && let Some(snapshot) = current.as_ref() {
            seed_persisted_sidebar_preferences(snapshot, &mut sidebar_state);
            let preferences = (
                sidebar_state.category_scope,
                sidebar_state.presentation_mode,
                sidebar_state.filter,
            );
            last_queued_preferences = Some(preferences);
            last_remote_preferences = Some(preferences);
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
                sidebar_state.manual_scroll = navigation.manual_scroll;
                last_queued_navigation = Some(NavigationRequest {
                    selection: navigation.selection.clone(),
                    scroll: navigation.scroll,
                    manual_scroll: navigation.manual_scroll,
                });
            }
            initial_context_seeded = true;
        }
        if let Some(snapshot) = current.as_ref()
            && apply_remote_sidebar_preferences(
                snapshot,
                &mut sidebar_state,
                &mut last_remote_preferences,
                &mut last_queued_preferences,
            )
        {
            render_gate.mark_dirty();
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
            render_gate.mark_dirty_if(refresh_current_category(snapshot, &mut sidebar_state));
            render_gate.mark_dirty_if(refresh_current_agents(snapshot, &mut sidebar_state));
            render_gate.mark_dirty_if(clear_stale_pane_selection(snapshot, &mut sidebar_state));
        }
        render_gate.mark_dirty_if(drain_mark_complete_results(&mark_result_rx, &mut mark_ui));
        render_gate.mark_dirty_if(drain_pane_pin_results(&pin_result_rx, &mut mark_ui));
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
            apply_category_intent_result(result, &mut category_dialog, &mut mark_ui);
        }
        while let Ok(result) = navigation_result_rx.try_recv() {
            if let Err(error) = result.result {
                if last_queued_navigation.as_ref() == Some(&result.request) {
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
            last_queued_preferences = Some((
                sidebar_state.category_scope,
                sidebar_state.presentation_mode,
                sidebar_state.filter,
            ));
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
        let preferences = (
            sidebar_state.category_scope,
            sidebar_state.presentation_mode,
            sidebar_state.filter,
        );
        if matches!(connection, ConnectionState::Connected)
            && last_queued_preferences.is_some_and(|previous| previous != preferences)
        {
            let previous = last_queued_preferences.expect("preference seed checked");
            let intents = [
                (previous.0 != sidebar_state.category_scope).then_some(
                    SidebarPreferenceIntent::SetDefaultCategoryScope {
                        category_scope: sidebar_state.category_scope,
                    },
                ),
                (previous.1 != sidebar_state.presentation_mode).then_some(
                    SidebarPreferenceIntent::SetDefaultPresentationMode {
                        presentation_mode: sidebar_state.presentation_mode,
                    },
                ),
                (previous.2 != sidebar_state.filter).then_some(
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
        let navigation = NavigationRequest {
            selection: sidebar_state.selection.clone(),
            scroll: sidebar_state.scroll,
            manual_scroll: sidebar_state.manual_scroll,
        };
        if matches!(connection, ConnectionState::Connected)
            && last_queued_navigation.as_ref() != Some(&navigation)
        {
            if navigation_tx.send(navigation.clone()).is_err() {
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
            &pin_request_tx,
            &mut mark_ui,
            ControlMessageContext {
                snapshot: current.as_ref(),
                config: config.app,
                daemon_connected: matches!(connection, ConnectionState::Connected),
                socket,
                server_identity,
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
                let selection_range = selected_row_index.and_then(|row_index| {
                    rendered_selection_range(&sidebar.rows, &rendered.row_indices, row_index)
                });
                let frame_scroll = if sidebar_state.manual_scroll {
                    clamp_scroll_range(
                        sidebar_state.scroll,
                        rendered.lines.len(),
                        areas.rows_height as usize,
                    )
                } else {
                    resolve_scroll_range(
                        sidebar_state.scroll,
                        selection_range,
                        rendered.lines.len(),
                        areas.rows_height as usize,
                    )
                };
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
                        category_dialog: category_dialog.as_ref(),
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
                Event::Key(key) if category_dialog.is_some() => {
                    pending_g = false;
                    handle_category_edit_key(
                        key,
                        &mut category_dialog,
                        &category_intent_tx,
                        &mut next_category_dialog_request_id,
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
                    KeyCode::Char('p') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        pending_g = false;
                        if let Some(snapshot) = &current {
                            let sidebar = project_view(snapshot, config.app, &sidebar_state);
                            queue_pane_pin_for_selection(
                                snapshot,
                                &sidebar,
                                &pin_request_tx,
                                &mut mark_ui,
                            );
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
                                    &mut category_dialog,
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
                Event::Mouse(_) if category_dialog.is_some() => {
                    pending_g = false;
                }
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
                    if let Some(frame) = last_frame.as_ref() {
                        scroll_mouse_viewport(
                            &mut sidebar_state,
                            frame,
                            mouse.kind == MouseEventKind::ScrollDown,
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

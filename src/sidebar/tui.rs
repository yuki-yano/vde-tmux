use std::collections::BTreeMap;
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
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::config::Config;
use crate::daemon::DaemonSnapshot;
use crate::sidebar::client::{
    send_sidebar_jump, send_sidebar_key, send_sidebar_toggle, socket_path, subscribe,
};
use crate::sidebar::render::{
    HeaderAction, SidebarRenderTheme, build_header_layout_with_theme, header_hit_test,
    render_header_lines, render_lines,
};
use crate::sidebar::tree::{SidebarRow, SidebarRowKind};
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

const DOUBLE_CLICK_MAX: Duration = Duration::from_millis(500);

static PANIC_RESTORE_HOOK: Once = Once::new();

pub fn run_live_tui(env: &BTreeMap<String, String>, config: &Config) -> Result<Option<String>> {
    install_panic_restore_hook();
    let close_window =
        resolve_current_window_id(&SystemTmuxRunner::from_env(Duration::from_secs(1)), env)?;
    let socket = socket_path(env);
    let (tx, rx) = mpsc::channel();
    subscribe(&socket, tx)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let runner = SystemTmuxRunner::from_env(Duration::from_secs(1));
    let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
    let result = run_loop(&mut terminal, &socket, &rx, &runner, env, &theme);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    if result? == TuiExit::Quit {
        spawn_detached_sidebar_close(&std::env::current_exe()?, &close_window)?;
    }
    let _ = config;
    Ok(None)
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
}

pub fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    socket: &Path,
    rx: &mpsc::Receiver<DaemonSnapshot>,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    theme: &SidebarRenderTheme,
) -> Result<TuiExit> {
    let mut current: Option<DaemonSnapshot> = None;
    let mut clicks = ClickTracker::default();
    loop {
        while let Ok(snapshot) = rx.try_recv() {
            current = Some(snapshot);
        }
        if let Some(snapshot) = &current {
            draw_snapshot_with_theme(terminal, snapshot, theme)?;
        } else {
            draw_connecting(terminal)?;
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(TuiExit::Quit),
                    KeyCode::Char('p') => {
                        if let Some(snapshot) = &current
                            && let Some(pane_id) = preview_pane_for_selection(snapshot)
                        {
                            spawn_preview(runner, env, &pane_id);
                        }
                    }
                    KeyCode::Char(' ') => send_sidebar_key(socket, "space")?,
                    KeyCode::Char(ch) => send_sidebar_key(socket, &ch.to_string())?,
                    KeyCode::Down => send_sidebar_key(socket, "down")?,
                    KeyCode::Up => send_sidebar_key(socket, "up")?,
                    KeyCode::Right => send_sidebar_key(socket, "right")?,
                    KeyCode::Left => send_sidebar_key(socket, "left")?,
                    KeyCode::Tab => send_sidebar_key(socket, "tab")?,
                    KeyCode::Enter => send_sidebar_key(socket, "enter")?,
                    _ => {}
                },
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(snapshot) = &current {
                        let context = ClickContext {
                            socket,
                            runner,
                            env,
                            theme,
                        };
                        handle_left_click(
                            &context,
                            snapshot,
                            mouse.row,
                            mouse.column,
                            &mut clicks,
                        )?;
                    }
                }
                _ => {}
            }
        }
    }
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

#[derive(Default)]
struct ClickTracker {
    last: Option<(u16, Instant)>,
}

impl ClickTracker {
    fn register_left_click(&mut self, row: u16, now: Instant) -> bool {
        let double_clicked = self
            .last
            .map(|(last_row, last_at)| {
                last_row == row && now.duration_since(last_at) <= DOUBLE_CLICK_MAX
            })
            .unwrap_or(false);
        self.last = Some((row, now));
        double_clicked
    }
}

#[cfg(test)]
fn pane_for_click(snapshot: &DaemonSnapshot, row: u16) -> Option<String> {
    row_for_click(snapshot, row, 0)?.pane_id.clone()
}

fn row_for_click(snapshot: &DaemonSnapshot, row: u16, header_rows: u16) -> Option<&SidebarRow> {
    if row < header_rows {
        return None;
    }
    snapshot
        .sidebar
        .as_ref()?
        .rows
        .get(usize::from(row - header_rows))
}

pub fn draw_snapshot<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &DaemonSnapshot,
) -> Result<()> {
    draw_snapshot_with_theme(terminal, snapshot, &SidebarRenderTheme::default())
}

pub fn draw_snapshot_with_theme<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &DaemonSnapshot,
    theme: &SidebarRenderTheme,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        draw_snapshot_in_area(frame, area, snapshot, theme);
    })?;
    Ok(())
}

pub fn draw_connecting<B: Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        draw_placeholder(frame, area, "connecting to daemon...");
    })?;
    Ok(())
}

fn draw_snapshot_in_area(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    snapshot: &DaemonSnapshot,
    theme: &SidebarRenderTheme,
) {
    let Some(sidebar) = &snapshot.sidebar else {
        draw_placeholder(frame, area, "no sidebar data");
        return;
    };
    if sidebar.rows.is_empty() {
        draw_placeholder(frame, area, "no agents");
        return;
    };
    let header = build_header_layout_with_theme(&sidebar.state, area.width, theme);
    let header_rows = header.row_count().min(area.height);
    if header_rows > 0 {
        let header_area = Rect {
            height: header_rows,
            ..area
        };
        frame.render_widget(
            Paragraph::new(render_header_lines(&header, theme)),
            header_area,
        );
    }
    let rows_area = Rect {
        y: area.y + header_rows,
        height: area.height.saturating_sub(header_rows),
        ..area
    };
    let items = render_lines(&sidebar.rows, &sidebar.state, area.width as usize, theme)
        .into_iter()
        .map(ListItem::new)
        .collect::<Vec<_>>();
    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, rows_area);
}

fn draw_placeholder(frame: &mut ratatui::Frame<'_>, area: Rect, message: &str) {
    let list = List::new(vec![ListItem::new(Line::from(message.to_string()))])
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, area);
}

struct ClickContext<'a> {
    socket: &'a Path,
    runner: &'a dyn TmuxRunner,
    env: &'a BTreeMap<String, String>,
    theme: &'a SidebarRenderTheme,
}

fn handle_left_click(
    context: &ClickContext<'_>,
    snapshot: &DaemonSnapshot,
    row: u16,
    column: u16,
    clicks: &mut ClickTracker,
) -> Result<()> {
    let Some(sidebar) = &snapshot.sidebar else {
        return Ok(());
    };
    let width = crossterm::terminal::size()
        .map(|(width, _)| width)
        .unwrap_or(80);
    let header = build_header_layout_with_theme(&sidebar.state, width, context.theme);
    if row < header.row_count() {
        match header_hit_test(&header, row, column) {
            Some(HeaderAction::CycleViewMode) => {
                send_sidebar_request(send_sidebar_key(context.socket, "v"))
            }
            Some(HeaderAction::ToggleFilter) => {
                send_sidebar_request(send_sidebar_key(context.socket, "tab"));
            }
            None => {}
        }
        return Ok(());
    }
    let Some(clicked) = row_for_click(snapshot, row, header.row_count()) else {
        return Ok(());
    };
    if clicks.register_left_click(row, Instant::now())
        && let Some(pane_id) = &clicked.pane_id
    {
        send_sidebar_request(send_sidebar_jump(context.socket, pane_id));
        return Ok(());
    }
    match clicked.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Chat => {
            send_sidebar_request(send_sidebar_toggle(context.socket, &clicked.id));
        }
        SidebarRowKind::Jump => {
            if let Some(pane_id) = &clicked.pane_id {
                send_sidebar_request(send_sidebar_jump(context.socket, pane_id));
            }
        }
        SidebarRowKind::Detail => {
            if let Some(pane_id) = &clicked.pane_id {
                spawn_preview(context.runner, context.env, pane_id);
            }
        }
    }
    Ok(())
}

fn send_sidebar_request(result: Result<()>) {
    if let Err(error) = result {
        eprintln!("[vde-tmux] sidebar click event failed: {error:#}");
    }
}

fn preview_pane_for_selection(snapshot: &DaemonSnapshot) -> Option<String> {
    let sidebar = snapshot.sidebar.as_ref()?;
    let selection = sidebar.state.selection.as_deref()?;
    let row = sidebar.rows.iter().find(|row| row.id == selection)?;
    match row.kind {
        SidebarRowKind::Chat | SidebarRowKind::Jump | SidebarRowKind::Detail => row.pane_id.clone(),
        SidebarRowKind::Category | SidebarRowKind::Repo => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewCommand {
    args: Vec<String>,
}

fn build_preview_command(pane_id: &str, window_id: &str) -> PreviewCommand {
    let target = shell_quote(pane_id);
    PreviewCommand {
        args: vec![
            "new-pane".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
            "-x".to_string(),
            "80%".to_string(),
            "-y".to_string(),
            "80%".to_string(),
            "-X".to_string(),
            "10%".to_string(),
            "-Y".to_string(),
            "10%".to_string(),
            "-t".to_string(),
            window_id.to_string(),
            format!(
                "{{ tmux capture-pane -a -p -e -t {target} 2>/dev/null || tmux capture-pane -p -e -t {target}; }} | less -R"
            ),
        ],
    }
}

fn spawn_preview(runner: &dyn TmuxRunner, _env: &BTreeMap<String, String>, pane_id: &str) {
    if let Err(error) = open_preview_floating_pane(runner, _env, pane_id) {
        eprintln!("[vde-tmux] sidebar preview failed: {error:#}");
    }
}

fn open_preview_floating_pane(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    pane_id: &str,
) -> Result<()> {
    let window_id = resolve_current_window_id(runner, env)?;
    kill_existing_preview_panes(runner, &window_id)?;
    runner.run(&["set-option", "-s", "focus-events", "on"])?;
    let command = build_preview_command(pane_id, &window_id);
    let args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
    let pane = runner.run(&args)?.trim().to_string();
    if pane.is_empty() {
        anyhow::bail!("new-pane did not return pane_id");
    }
    if let Err(error) = configure_preview_floating_pane(runner, &pane) {
        let _ = runner.run(&["kill-pane", "-t", &pane]);
        return Err(error);
    }
    Ok(())
}

fn configure_preview_floating_pane(runner: &dyn TmuxRunner, pane: &str) -> Result<()> {
    runner.run(&["set-option", "-p", "-t", pane, "@vde_preview", "1"])?;
    runner.run(&["set-option", "-p", "-t", pane, "pane-border-status", "off"])?;
    let hook = format!("kill-pane -t '{}'", pane);
    runner.run(&["set-hook", "-p", "-t", pane, "pane-focus-out", &hook])?;
    Ok(())
}

fn kill_existing_preview_panes(runner: &dyn TmuxRunner, window_id: &str) -> Result<()> {
    let output = runner.run(&[
        "list-panes",
        "-t",
        window_id,
        "-F",
        "#{pane_id} #{@vde_preview}",
    ])?;
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let Some(pane_id) = fields.next() else {
            continue;
        };
        if fields.next() == Some("1") {
            runner.run(&["kill-pane", "-t", pane_id])?;
        }
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{DaemonSnapshot, SidebarFrame};
    use crate::hook::RollupLevel;
    use crate::sidebar::state::SidebarState;
    use crate::sidebar::tree::{SidebarRow, SidebarRowKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn row() -> SidebarRow {
        SidebarRow {
            id: "chat::%1".to_string(),
            kind: SidebarRowKind::Chat,
            depth: 0,
            label: "codex (%1)".to_string(),
            chat_count: 1,
            rollup: RollupLevel::Running,
            expanded: true,
            pane_id: Some("%1".to_string()),
            git: None,
        }
    }

    #[test]
    fn renders_snapshot_rows_on_push() {
        let snapshot = DaemonSnapshot {
            agent_count: 1,
            rollup: RollupLevel::Running,
            panes: Vec::new(),
            sidebar: Some(SidebarFrame {
                state: SidebarState::default(),
                rows: vec![row()],
            }),
        };
        let backend = TestBackend::new(40, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        draw_snapshot(&mut terminal, &snapshot).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("codex (%1)"));
    }

    #[test]
    fn renders_connecting_placeholder_before_first_snapshot() {
        let backend = TestBackend::new(40, 2);
        let mut terminal = Terminal::new(backend).unwrap();

        draw_connecting(&mut terminal).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("connecting to daemon..."));
    }

    #[test]
    fn renders_no_agents_placeholder_for_empty_sidebar_rows() {
        let snapshot = DaemonSnapshot {
            agent_count: 0,
            rollup: RollupLevel::Idle,
            panes: Vec::new(),
            sidebar: Some(SidebarFrame {
                state: SidebarState::default(),
                rows: Vec::new(),
            }),
        };
        let backend = TestBackend::new(40, 2);
        let mut terminal = Terminal::new(backend).unwrap();

        draw_snapshot(&mut terminal, &snapshot).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("no agents"));
    }

    #[test]
    fn sidebar_close_command_targets_window() {
        let command = sidebar_close_command(std::path::Path::new("/tmp/vt"), "@1");

        assert_eq!(command.program, std::path::PathBuf::from("/tmp/vt"));
        assert_eq!(command.args, vec!["sidebar", "close", "--window", "@1"]);
    }

    #[test]
    fn restore_terminal_after_panic_leaves_alternate_screen() {
        let mut output = Vec::new();

        restore_terminal_after_panic(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{1b}[?1049l"));
    }

    #[test]
    fn pane_for_click_returns_agent_row_pane_id() {
        let snapshot = DaemonSnapshot {
            agent_count: 1,
            rollup: RollupLevel::Running,
            panes: Vec::new(),
            sidebar: Some(SidebarFrame {
                state: SidebarState::default(),
                rows: vec![
                    SidebarRow {
                        id: "repo::misc::app".to_string(),
                        kind: SidebarRowKind::Repo,
                        depth: 0,
                        label: "app".to_string(),
                        chat_count: 1,
                        rollup: RollupLevel::Running,
                        expanded: true,
                        pane_id: None,
                        git: None,
                    },
                    row(),
                ],
            }),
        };

        assert_eq!(pane_for_click(&snapshot, 1).as_deref(), Some("%1"));
    }

    #[test]
    fn pane_for_click_ignores_non_agent_rows() {
        let snapshot = DaemonSnapshot {
            agent_count: 1,
            rollup: RollupLevel::Running,
            panes: Vec::new(),
            sidebar: Some(SidebarFrame {
                state: SidebarState::default(),
                rows: vec![SidebarRow {
                    id: "repo::misc::app".to_string(),
                    kind: SidebarRowKind::Repo,
                    depth: 0,
                    label: "app".to_string(),
                    chat_count: 1,
                    rollup: RollupLevel::Running,
                    expanded: true,
                    pane_id: None,
                    git: None,
                }],
            }),
        };

        assert_eq!(pane_for_click(&snapshot, 0), None);
    }

    #[test]
    fn row_for_click_offsets_header_rows() {
        let snapshot = DaemonSnapshot {
            agent_count: 1,
            rollup: RollupLevel::Running,
            panes: Vec::new(),
            sidebar: Some(SidebarFrame {
                state: SidebarState::default(),
                rows: vec![
                    SidebarRow {
                        id: "repo::misc::app".to_string(),
                        kind: SidebarRowKind::Repo,
                        depth: 0,
                        label: "app".to_string(),
                        chat_count: 1,
                        rollup: RollupLevel::Running,
                        expanded: true,
                        pane_id: None,
                        git: None,
                    },
                    row(),
                ],
            }),
        };

        assert_eq!(
            row_for_click(&snapshot, 1, 1).map(|row| row.id.as_str()),
            Some("repo::misc::app")
        );
        assert_eq!(
            row_for_click(&snapshot, 2, 1).map(|row| row.id.as_str()),
            Some("chat::%1")
        );
        assert_eq!(row_for_click(&snapshot, 0, 1), None);
    }

    #[test]
    fn preview_command_uses_floating_pane() {
        let command = build_preview_command("%26", "@1");

        assert_eq!(
            command.args,
            vec![
                "new-pane",
                "-P",
                "-F",
                "#{pane_id}",
                "-x",
                "80%",
                "-y",
                "80%",
                "-X",
                "10%",
                "-Y",
                "10%",
                "-t",
                "@1",
                "{ tmux capture-pane -a -p -e -t '%26' 2>/dev/null || tmux capture-pane -p -e -t '%26'; } | less -R"
            ]
        );
    }

    #[test]
    fn spawn_preview_configures_floating_pane() {
        let mock = crate::tmux::mock::MockTmuxRunner::new();
        mock.stub(
            &["display-message", "-p", "-t", "%10", "-F", "#{window_id}"],
            "@1\n",
        );
        mock.stub(
            &["list-panes", "-t", "@1", "-F", "#{pane_id} #{@vde_preview}"],
            "%77 1\n%10 \n",
        );
        mock.stub(&["kill-pane", "-t", "%77"], "");
        mock.stub(&["set-option", "-s", "focus-events", "on"], "");
        mock.stub(
            &[
                "new-pane",
                "-P",
                "-F",
                "#{pane_id}",
                "-x",
                "80%",
                "-y",
                "80%",
                "-X",
                "10%",
                "-Y",
                "10%",
                "-t",
                "@1",
                "{ tmux capture-pane -a -p -e -t '%26' 2>/dev/null || tmux capture-pane -p -e -t '%26'; } | less -R",
            ],
            "%99\n",
        );
        mock.stub(&["set-option", "-p", "-t", "%99", "@vde_preview", "1"], "");
        mock.stub(
            &["set-option", "-p", "-t", "%99", "pane-border-status", "off"],
            "",
        );
        mock.stub(
            &[
                "set-hook",
                "-p",
                "-t",
                "%99",
                "pane-focus-out",
                "kill-pane -t '%99'",
            ],
            "",
        );

        spawn_preview(
            &mock,
            &BTreeMap::from([("TMUX_PANE".to_string(), "%10".to_string())]),
            "%26",
        );

        assert_eq!(
            mock.calls(),
            vec![
                vec!["display-message", "-p", "-t", "%10", "-F", "#{window_id}"],
                vec!["list-panes", "-t", "@1", "-F", "#{pane_id} #{@vde_preview}"],
                vec!["kill-pane", "-t", "%77"],
                vec!["set-option", "-s", "focus-events", "on"],
                vec![
                    "new-pane",
                    "-P",
                    "-F",
                    "#{pane_id}",
                    "-x",
                    "80%",
                    "-y",
                    "80%",
                    "-X",
                    "10%",
                    "-Y",
                    "10%",
                    "-t",
                    "@1",
                    "{ tmux capture-pane -a -p -e -t '%26' 2>/dev/null || tmux capture-pane -p -e -t '%26'; } | less -R"
                ],
                vec!["set-option", "-p", "-t", "%99", "@vde_preview", "1"],
                vec!["set-option", "-p", "-t", "%99", "pane-border-status", "off"],
                vec![
                    "set-hook",
                    "-p",
                    "-t",
                    "%99",
                    "pane-focus-out",
                    "kill-pane -t '%99'"
                ],
            ]
            .into_iter()
            .map(|items| items.into_iter().map(str::to_string).collect::<Vec<_>>())
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn click_tracker_detects_double_click_on_same_row() {
        let mut tracker = ClickTracker::default();
        let now = std::time::Instant::now();

        assert!(!tracker.register_left_click(2, now));
        assert!(tracker.register_left_click(2, now + Duration::from_millis(250)));
    }

    #[test]
    fn click_tracker_ignores_slow_or_different_row_clicks() {
        let mut tracker = ClickTracker::default();
        let now = std::time::Instant::now();

        assert!(!tracker.register_left_click(2, now));
        assert!(!tracker.register_left_click(3, now + Duration::from_millis(100)));
        assert!(!tracker.register_left_click(3, now + Duration::from_millis(700)));
    }
}

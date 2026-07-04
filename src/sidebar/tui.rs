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
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::config::Config;
use crate::daemon::DaemonSnapshot;
use crate::sidebar::client::{send_sidebar_jump, send_sidebar_key, socket_path, subscribe};
use crate::sidebar::render::render_rows;
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
    let result = run_loop(&mut terminal, &socket, &rx);
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
) -> Result<TuiExit> {
    let mut current: Option<DaemonSnapshot> = None;
    let mut clicks = ClickTracker::default();
    loop {
        while let Ok(snapshot) = rx.try_recv() {
            current = Some(snapshot);
        }
        if let Some(snapshot) = &current {
            draw_snapshot(terminal, snapshot)?;
        } else {
            draw_connecting(terminal)?;
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(TuiExit::Quit),
                    KeyCode::Char(' ') => send_sidebar_key(socket, "space")?,
                    KeyCode::Char(ch) => send_sidebar_key(socket, &ch.to_string())?,
                    KeyCode::Down => send_sidebar_key(socket, "down")?,
                    KeyCode::Up => send_sidebar_key(socket, "up")?,
                    KeyCode::Enter => send_sidebar_key(socket, "enter")?,
                    _ => {}
                },
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if clicks.register_left_click(mouse.row, Instant::now())
                        && let Some(snapshot) = &current
                        && let Some(pane_id) = pane_for_click(snapshot, mouse.row)
                    {
                        send_sidebar_jump(socket, &pane_id)?;
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

fn pane_for_click(snapshot: &DaemonSnapshot, row: u16) -> Option<String> {
    snapshot
        .sidebar
        .as_ref()?
        .rows
        .get(usize::from(row))?
        .pane_id
        .clone()
}

pub fn draw_snapshot<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &DaemonSnapshot,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        draw_snapshot_in_area(frame, area, snapshot);
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

fn draw_snapshot_in_area(frame: &mut ratatui::Frame<'_>, area: Rect, snapshot: &DaemonSnapshot) {
    let Some(sidebar) = &snapshot.sidebar else {
        draw_placeholder(frame, area, "no sidebar data");
        return;
    };
    if sidebar.rows.is_empty() {
        draw_placeholder(frame, area, "no agents");
        return;
    };
    let rendered = render_rows(&sidebar.rows, &sidebar.state, area.width as usize);
    let items = rendered
        .lines()
        .map(|line| ListItem::new(Line::from(line.to_string())))
        .collect::<Vec<_>>();
    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, area);
}

fn draw_placeholder(frame: &mut ratatui::Frame<'_>, area: Rect, message: &str) {
    let list = List::new(vec![ListItem::new(Line::from(message.to_string()))])
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, area);
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
            id: "pane::%1".to_string(),
            kind: SidebarRowKind::Chat,
            depth: 0,
            label: "codex %1".to_string(),
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
        assert!(rendered.contains("codex %1"));
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

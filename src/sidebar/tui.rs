use std::collections::BTreeMap;
use std::io::{self, Write};
use std::panic;
use std::path::Path;
use std::sync::Once;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
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
use crate::sidebar::client::{send_sidebar_key, socket_path, subscribe};
use crate::sidebar::render::render_rows;

static PANIC_RESTORE_HOOK: Once = Once::new();

pub fn run_live_tui(env: &BTreeMap<String, String>, config: &Config) -> Result<Option<String>> {
    install_panic_restore_hook();
    let socket = socket_path(env);
    let (tx, rx) = mpsc::channel();
    subscribe(&socket, tx)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(&mut terminal, &socket, &rx);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result?;
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
    execute!(writer, LeaveAlternateScreen)?;
    Ok(())
}

pub fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    socket: &Path,
    rx: &mpsc::Receiver<DaemonSnapshot>,
) -> Result<()> {
    let mut current: Option<DaemonSnapshot> = None;
    loop {
        while let Ok(snapshot) = rx.try_recv() {
            current = Some(snapshot);
        }
        if let Some(snapshot) = &current {
            draw_snapshot(terminal, snapshot)?;
        }
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => break,
                KeyCode::Char(' ') => send_sidebar_key(socket, "space")?,
                KeyCode::Char(ch) => send_sidebar_key(socket, &ch.to_string())?,
                KeyCode::Down => send_sidebar_key(socket, "down")?,
                KeyCode::Up => send_sidebar_key(socket, "up")?,
                KeyCode::Enter => send_sidebar_key(socket, "enter")?,
                _ => {}
            }
        }
    }
    Ok(())
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

fn draw_snapshot_in_area(frame: &mut ratatui::Frame<'_>, area: Rect, snapshot: &DaemonSnapshot) {
    let Some(sidebar) = &snapshot.sidebar else {
        let list = List::new(vec![ListItem::new(Line::from("no sidebar data"))])
            .block(Block::default().borders(Borders::NONE));
        frame.render_widget(list, area);
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
    fn restore_terminal_after_panic_leaves_alternate_screen() {
        let mut output = Vec::new();

        restore_terminal_after_panic(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{1b}[?1049l"));
    }
}

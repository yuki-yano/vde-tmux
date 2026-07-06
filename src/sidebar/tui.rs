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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::config::{Config, SidebarLiveConfig};
use crate::daemon::DaemonSnapshot;
use crate::sidebar::client::{
    send_sidebar_jump, send_sidebar_key, send_sidebar_toggle, socket_path, subscribe,
};
use crate::sidebar::preview::open_preview_floating_pane;
use crate::sidebar::render::{
    BadgeCounts, HeaderAction, HeaderLayout, JumpRowAction, SidebarRenderTheme, build_footer_line,
    build_header_layout_with_counts, display_width, header_hit_test, jump_row_action_at,
    render_header_lines, render_lines_with_indices,
};
use crate::sidebar::state::StatusFilter;
use crate::sidebar::tree::{SidebarRow, SidebarRowKind};
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

const LIVE_CARD_MIN_WIDTH: u16 = 24;

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
    let theme = SidebarRenderTheme::from_app_config(config);
    let (live_request_tx, live_request_rx) = mpsc::channel();
    let (live_result_tx, live_result_rx) = mpsc::channel();
    spawn_live_capture_worker(live_request_rx, live_result_tx);
    let runtime_config = RunLoopConfig {
        theme: &theme,
        preview_history_lines: config.sidebar.preview.history_lines,
        live: &config.sidebar.live,
        live_capture_tx: &live_request_tx,
        live_capture_rx: &live_result_rx,
    };
    let result = run_loop(&mut terminal, &socket, &rx, &runner, env, runtime_config);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LiveMode {
    #[default]
    Tail,
    Events,
}

#[derive(Debug, Clone, Default)]
struct LiveState {
    mode: LiveMode,
    pane_id: Option<String>,
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
    pub theme: &'a SidebarRenderTheme,
    pub preview_history_lines: u32,
    pub live: &'a SidebarLiveConfig,
    pub live_capture_tx: &'a mpsc::Sender<String>,
    pub live_capture_rx: &'a mpsc::Receiver<(String, String)>,
}

pub fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    socket: &Path,
    rx: &mpsc::Receiver<DaemonSnapshot>,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: RunLoopConfig<'_>,
) -> Result<TuiExit> {
    let theme = config.theme;
    let preview_history_lines = config.preview_history_lines;
    let live_config = config.live;
    let mut current: Option<DaemonSnapshot> = None;
    let mut scroll: usize = 0;
    let mut live = LiveState {
        requested_lines: live_rows_requested(live_config),
        cut_markers: live_config.cut_markers.clone(),
        ..LiveState::default()
    };
    loop {
        if let Err(exit) = drain_snapshot_updates(rx, &mut current) {
            return Ok(exit);
        }
        let context = ClickContext {
            socket,
            runner,
            env,
            theme,
            preview_history_lines,
            live_lines: live.requested_lines,
        };
        if let Some(snapshot) = &current {
            update_live_state(
                snapshot,
                live_config,
                &mut live,
                config.live_capture_tx,
                config.live_capture_rx,
            );
            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            if let Some(sidebar) = &snapshot.sidebar {
                let header = build_header_layout_with_counts(
                    &sidebar.state,
                    area.width,
                    theme,
                    BadgeCounts::from_rows(&sidebar.rows),
                );
                let areas = compute_areas(area, &header, live.requested_lines);
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
                let selection_index = selected_row_index.and_then(|row_index| {
                    rendered
                        .row_indices
                        .iter()
                        .position(|mapped| *mapped == Some(row_index))
                });
                scroll = resolve_scroll(
                    scroll,
                    selection_index,
                    rendered.lines.len(),
                    areas.rows_height as usize,
                );
            } else {
                scroll = 0;
            }
            draw_snapshot_with_theme_and_scroll_live(
                terminal,
                snapshot,
                theme,
                scroll,
                Some(&live),
            )?;
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
                            spawn_preview(runner, env, &pane_id, preview_history_lines);
                        }
                    }
                    KeyCode::Char('e') => live.toggle_mode(),
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
                        handle_left_click(&context, snapshot, mouse.row, mouse.column, scroll)?;
                    }
                }
                _ => {}
            }
        }
    }
}

fn drain_snapshot_updates(
    rx: &mpsc::Receiver<DaemonSnapshot>,
    current: &mut Option<DaemonSnapshot>,
) -> std::result::Result<(), TuiExit> {
    loop {
        match rx.try_recv() {
            Ok(snapshot) => *current = Some(snapshot),
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => return Err(TuiExit::Disconnected),
        }
    }
}

fn live_rows_requested(config: &SidebarLiveConfig) -> u16 {
    if config.enabled { config.lines } else { 0 }
}

fn update_live_state(
    snapshot: &DaemonSnapshot,
    config: &SidebarLiveConfig,
    live: &mut LiveState,
    request_tx: &mpsc::Sender<String>,
    result_rx: &mpsc::Receiver<(String, String)>,
) {
    while let Ok((pane_id, output)) = result_rx.try_recv() {
        apply_live_capture_result(live, &pane_id, &output);
    }
    live.requested_lines = live_rows_requested(config);
    if live.requested_lines == 0 {
        live.pane_id = None;
        live.lines.clear();
        live.last_capture = None;
        live.capture_in_flight = false;
        return;
    }
    let selected = preview_pane_for_selection(snapshot);
    if live.pane_id != selected {
        live.pane_id = selected;
        live.last_capture = None;
        live.lines.clear();
        live.capture_in_flight = false;
    }
    let Some(pane_id) = live.pane_id.clone() else {
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
    if request_tx.send(pane_id).is_ok() {
        live.capture_in_flight = true;
        live.last_capture = Some(now);
    }
}

fn apply_live_capture_result(live: &mut LiveState, pane_id: &str, output: &str) {
    if live.pane_id.as_deref() != Some(pane_id) {
        return;
    }
    live.lines = extract_tail(output, live.requested_lines as usize, &live.cut_markers);
    live.capture_in_flight = false;
}

fn spawn_live_capture_worker(
    request_rx: mpsc::Receiver<String>,
    result_tx: mpsc::Sender<(String, String)>,
) {
    std::thread::spawn(move || {
        let runner = SystemTmuxRunner::from_env(Duration::from_millis(500));
        while let Ok(pane_id) = request_rx.recv() {
            let output = runner
                .run(&["capture-pane", "-p", "-e", "-t", &pane_id])
                .unwrap_or_default();
            if result_tx.send((pane_id, output)).is_err() {
                break;
            }
        }
    });
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
    #[cfg(test)]
    fn new(id: &str, kind: SidebarRowKind, pane_id: Option<&str>) -> Self {
        Self {
            id: id.to_string(),
            kind,
            pane_id: pane_id.map(ToOwned::to_owned),
        }
    }

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
    PreviewPane(String),
    JumpPane(String),
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

#[cfg(test)]
fn pane_for_click(snapshot: &DaemonSnapshot, row: u16) -> Option<String> {
    row_for_click(snapshot, row, 0, 0)?.pane_id.clone()
}

#[cfg(test)]
fn row_for_click(
    snapshot: &DaemonSnapshot,
    row: u16,
    header_rows: u16,
    scroll: usize,
) -> Option<&SidebarRow> {
    let rows_len = snapshot.sidebar.as_ref()?.rows.len();
    let row_indices = (0..rows_len).map(Some).collect::<Vec<_>>();
    row_for_click_with_indices(snapshot, row, header_rows, scroll, &row_indices)
}

fn row_for_click_with_indices<'a>(
    snapshot: &'a DaemonSnapshot,
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
    snapshot.sidebar.as_ref()?.rows.get(row_index)
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
    draw_snapshot_with_theme_and_scroll(terminal, snapshot, theme, 0)
}

fn draw_snapshot_with_theme_and_scroll<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &DaemonSnapshot,
    theme: &SidebarRenderTheme,
    scroll: usize,
) -> Result<()> {
    draw_snapshot_with_theme_and_scroll_live(terminal, snapshot, theme, scroll, None)
}

fn draw_snapshot_with_theme_and_scroll_live<B: Backend>(
    terminal: &mut Terminal<B>,
    snapshot: &DaemonSnapshot,
    theme: &SidebarRenderTheme,
    scroll: usize,
    live: Option<&LiveState>,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        draw_snapshot_in_area(frame, area, snapshot, theme, scroll, live);
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
    scroll: usize,
    live: Option<&LiveState>,
) {
    let Some(sidebar) = &snapshot.sidebar else {
        draw_placeholder(frame, area, "no sidebar data");
        return;
    };
    if sidebar.rows.is_empty() {
        draw_placeholder(frame, area, "no agents");
        return;
    };
    let header = build_header_layout_with_counts(
        &sidebar.state,
        area.width,
        theme,
        BadgeCounts::from_rows(&sidebar.rows),
    );
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
    let rendered =
        render_lines_with_indices(&sidebar.rows, &sidebar.state, area.width as usize, theme);
    let items = rendered
        .lines
        .into_iter()
        .skip(scroll)
        .take(areas.rows_height as usize)
        .map(ListItem::new)
        .collect::<Vec<_>>();
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
        frame.render_widget(
            Paragraph::new(build_footer_line(area.width as usize)),
            footer_area,
        );
    }
}

fn draw_placeholder(frame: &mut ratatui::Frame<'_>, area: Rect, message: &str) {
    let list = List::new(vec![ListItem::new(Line::from(message.to_string()))])
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, area);
}

fn render_live_lines(
    snapshot: &DaemonSnapshot,
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
            live.pane_id
                .as_deref()
                .map(|pane| format!(" · {pane}"))
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
        // capture-pane -e の ANSI を元の色のまま描画する。パース失敗時はプレーン
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
        content = plain();
    } else if content_used < content_width {
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

pub(crate) fn extract_tail(raw: &str, limit: usize, cut_markers: &[String]) -> Vec<String> {
    let all = raw.lines().map(str::trim_end).collect::<Vec<_>>();
    let cut = cut_index(&all, cut_markers).unwrap_or(all.len());
    let mut lines = all[..cut]
        .iter()
        .filter(|line| !strip_ansi(line).trim().is_empty())
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    lines.drain(0..start);
    lines
}

/// Codex / Claude Code の入力欄・フッター以下を落とすための切断位置。
/// 各マーカーの「最後の出現行」を求め、その最小 index で切る
/// (Claude Code は入力ボックス上辺 `╭`、Codex は入力プロンプト等が
///  常に最下部 UI 帯にあるため、最後の出現 = 入力欄になる)。
fn cut_index(lines: &[&str], markers: &[String]) -> Option<usize> {
    markers
        .iter()
        .filter(|marker| !marker.is_empty())
        .filter_map(|marker| {
            lines
                .iter()
                .rposition(|line| strip_ansi(line).contains(marker.as_str()))
        })
        .min()
}

/// マーカー判定・空行判定用の簡易 ANSI エスケープ除去(CSI / OSC / 単独 ESC)。
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
                // CSI: パラメータ/中間バイトを読み飛ばし、終端バイト(@-~)で終わる
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC: BEL または ESC \ で終わる
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
    snapshot: &DaemonSnapshot,
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
                event.agent,
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

pub(crate) fn resolve_scroll(
    prev: usize,
    selection_index: Option<usize>,
    rows_len: usize,
    viewport: usize,
) -> usize {
    if viewport == 0 || rows_len <= viewport {
        return 0;
    }
    let max_scroll = rows_len - viewport;
    let mut scroll = prev.min(max_scroll);
    if let Some(index) = selection_index {
        if index < scroll {
            scroll = index;
        } else if index >= scroll + viewport {
            scroll = index + 1 - viewport;
        }
    }
    scroll.min(max_scroll)
}

struct ClickContext<'a> {
    socket: &'a Path,
    runner: &'a dyn TmuxRunner,
    env: &'a BTreeMap<String, String>,
    theme: &'a SidebarRenderTheme,
    preview_history_lines: u32,
    live_lines: u16,
}

fn handle_left_click(
    context: &ClickContext<'_>,
    snapshot: &DaemonSnapshot,
    row: u16,
    column: u16,
    scroll: usize,
) -> Result<()> {
    let Some(sidebar) = &snapshot.sidebar else {
        return Ok(());
    };
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let header = build_header_layout_with_counts(
        &sidebar.state,
        width,
        context.theme,
        BadgeCounts::from_rows(&sidebar.rows),
    );
    if row < header.row_count() {
        match header_hit_test(&header, row, column) {
            Some(HeaderAction::CycleViewMode) => {
                send_sidebar_request(send_sidebar_key(context.socket, "v"))
            }
            Some(HeaderAction::ToggleFilter) => {
                send_sidebar_request(send_sidebar_key(context.socket, "tab"));
            }
            Some(HeaderAction::SetFilter(filter)) => {
                send_sidebar_request(send_sidebar_key(context.socket, filter_key(filter)));
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
        snapshot,
        row,
        header.row_count(),
        scroll,
        &rendered.row_indices,
    ) else {
        return Ok(());
    };
    if clicked.kind == SidebarRowKind::Jump {
        match jump_row_action_at(clicked, column) {
            Some(JumpRowAction::Jump) => {
                if let Some(pane_id) = clicked.pane_id.clone() {
                    dispatch_click_action(context, ClickAction::JumpPane(pane_id));
                }
            }
            Some(JumpRowAction::Preview) => {
                if let Some(pane_id) = clicked.pane_id.clone() {
                    dispatch_click_action(context, ClickAction::PreviewPane(pane_id));
                }
            }
            None => {}
        }
        return Ok(());
    }
    if let Some(action) = single_click_action(&ClickedRow::from_row(clicked)) {
        dispatch_click_action(context, action);
    }
    Ok(())
}

fn dispatch_click_action(context: &ClickContext<'_>, action: ClickAction) {
    match action {
        ClickAction::ToggleRow(row_id) => {
            send_sidebar_request(send_sidebar_toggle(context.socket, &row_id));
        }
        ClickAction::PreviewPane(pane_id) => {
            spawn_preview(
                context.runner,
                context.env,
                &pane_id,
                context.preview_history_lines,
            );
        }
        ClickAction::JumpPane(pane_id) => {
            send_sidebar_request(send_sidebar_jump(context.socket, &pane_id));
        }
    }
}

fn filter_key(filter: StatusFilter) -> &'static str {
    match filter {
        StatusFilter::All => "all",
        StatusFilter::AttentionOnly => "attn",
        StatusFilter::WorkingOnly => "working",
        StatusFilter::DoneOnly => "done",
        StatusFilter::IdleOnly => "idle",
    }
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
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Zone => None,
    }
}

fn spawn_preview(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    pane_id: &str,
    history_lines: u32,
) {
    if let Err(error) = open_preview_floating_pane(runner, env, pane_id, history_lines) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{DaemonSnapshot, SidebarFrame};
    use crate::hook::RollupLevel;
    use crate::sidebar::render::{HeaderLayout, HeaderLine};
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
            badge_state: None,
            expanded: true,
            pane_id: Some("%1".to_string()),
            git: None,
            meta: None,
        }
    }

    fn snapshot() -> DaemonSnapshot {
        DaemonSnapshot {
            agent_count: 1,
            rollup: RollupLevel::Running,
            panes: Vec::new(),
            sidebar: None,
            events: Vec::new(),
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn scroll_follows_selection() {
        assert_eq!(resolve_scroll(0, Some(5), 30, 10), 0);
        assert_eq!(resolve_scroll(0, Some(15), 30, 10), 6);
        assert_eq!(resolve_scroll(6, Some(2), 30, 10), 2);
        assert_eq!(resolve_scroll(25, Some(29), 30, 10), 20);
        assert_eq!(resolve_scroll(9, None, 5, 10), 0);
    }

    #[test]
    fn click_maps_through_scroll_offset() {
        let rows = (0..30)
            .map(|index| SidebarRow {
                id: format!("chat::%{index}"),
                pane_id: Some(format!("%{index}")),
                ..row()
            })
            .collect();
        let snapshot = DaemonSnapshot {
            agent_count: 30,
            rollup: RollupLevel::Running,
            panes: Vec::new(),
            sidebar: Some(SidebarFrame {
                state: SidebarState::default(),
                rows,
            }),
            events: Vec::new(),
        };

        assert_eq!(
            row_for_click(&snapshot, 2, 1, 6).map(|row| row.id.as_str()),
            Some("chat::%7")
        );
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
            events: Vec::new(),
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
    fn footer_is_rendered_when_height_is_sufficient() {
        let snapshot = DaemonSnapshot {
            agent_count: 1,
            rollup: RollupLevel::Running,
            panes: Vec::new(),
            sidebar: Some(SidebarFrame {
                state: SidebarState::default(),
                rows: vec![row()],
            }),
            events: Vec::new(),
        };
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        draw_snapshot(&mut terminal, &snapshot).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("j/k move"), "{rendered}");
    }

    #[test]
    fn footer_is_hidden_when_height_is_small() {
        let snapshot = DaemonSnapshot {
            agent_count: 1,
            rollup: RollupLevel::Running,
            panes: Vec::new(),
            sidebar: Some(SidebarFrame {
                state: SidebarState::default(),
                rows: vec![row()],
            }),
            events: Vec::new(),
        };
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        draw_snapshot(&mut terminal, &snapshot).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("j/k move"), "{rendered}");
    }

    #[test]
    fn clicks_below_visible_rows_are_ignored() {
        let header = HeaderLayout {
            lines: vec![HeaderLine {
                text: " repo · all".to_string(),
                segments: Vec::new(),
            }],
        };
        let areas = compute_areas(Rect::new(0, 0, 40, 24), &header, 0);
        assert_eq!(areas.header_rows, 1);
        assert_eq!(areas.footer_rows, 1);
        assert_eq!(areas.rows_height, 22);

        let small = compute_areas(Rect::new(0, 0, 40, 8), &header, 0);
        assert_eq!(small.footer_rows, 0);
        assert_eq!(small.rows_height, 7);
    }

    #[test]
    fn compute_areas_reserves_live_rows_when_enabled() {
        let header = HeaderLayout {
            lines: vec![HeaderLine {
                text: " repo · all".to_string(),
                segments: Vec::new(),
            }],
        };

        let areas = compute_areas(Rect::new(0, 0, 40, 24), &header, 3);

        assert_eq!(areas.header_rows, 1);
        assert_eq!(areas.rows_height, 17);
        assert_eq!(areas.live_rows, 5);
        assert_eq!(areas.footer_rows, 1);

        let narrow = compute_areas(Rect::new(0, 0, 20, 24), &header, 3);
        assert_eq!(narrow.header_rows, 1);
        assert_eq!(narrow.rows_height, 18);
        assert_eq!(narrow.live_rows, 4);
        assert_eq!(narrow.footer_rows, 1);

        let small = compute_areas(Rect::new(0, 0, 40, 13), &header, 3);
        assert_eq!(small.live_rows, 0);
    }

    #[test]
    fn live_card_has_rounded_border_when_wide() {
        let live = LiveState {
            pane_id: Some("%1".to_string()),
            lines: vec!["one".to_string(), "two".to_string(), "three".to_string()],
            requested_lines: 3,
            ..Default::default()
        };

        let lines = render_live_lines(&snapshot(), &live, 5, 40, 0, &SidebarRenderTheme::default());

        assert!(line_text(&lines[0]).starts_with("╭╴LIVE · %1"), "{lines:?}");
        assert!(line_text(&lines[0]).ends_with('╮'), "{lines:?}");
        assert!(line_text(&lines[1]).starts_with("│ "), "{lines:?}");
        assert!(line_text(&lines[1]).ends_with('│'), "{lines:?}");
        assert!(line_text(&lines[4]).starts_with('╰'), "{lines:?}");
        assert!(line_text(&lines[4]).ends_with('╯'), "{lines:?}");
    }

    #[test]
    fn live_card_falls_back_to_plain_when_narrow() {
        let live = LiveState {
            pane_id: Some("%1".to_string()),
            lines: vec!["one".to_string(), "two".to_string(), "three".to_string()],
            requested_lines: 3,
            ..Default::default()
        };

        let lines = render_live_lines(&snapshot(), &live, 4, 20, 0, &SidebarRenderTheme::default());

        assert_eq!(line_text(&lines[0]), " LIVE · %1");
        assert_eq!(line_text(&lines[1]), " one");
    }

    #[test]
    fn live_tail_keeps_last_nonempty_lines() {
        assert_eq!(extract_tail("a\nb\n\nc\n\n\n", 3, &[]), vec!["a", "b", "c"]);
        assert_eq!(extract_tail("a\nb\nc\nd\n", 2, &[]), vec!["c", "d"]);
    }

    #[test]
    fn live_tail_cuts_below_agent_input_area() {
        let markers = crate::config::SidebarLiveConfig::default().cut_markers;
        // Claude Code 風: 入力ボックスとフッターが末尾に居る
        let raw =
            "output 1\noutput 2\n\n╭──────────╮\n│ >        │\n╰──────────╯\n? for shortcuts\n";
        assert_eq!(extract_tail(raw, 5, &markers), vec!["output 1", "output 2"]);
        // Codex 風: 入力プロンプトとフッター
        let raw = "thinking...\ndone!\n› type here\n⏎ send  95% context left\n";
        assert_eq!(extract_tail(raw, 5, &markers), vec!["thinking...", "done!"]);
        // マーカーが無い出力はそのまま
        let raw = "plain 1\nplain 2\n";
        assert_eq!(extract_tail(raw, 5, &markers), vec!["plain 1", "plain 2"]);
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m text"), "red text");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }

    #[test]
    fn event_tail_formats_ago_agent_and_glyphs() {
        let mut snapshot = crate::daemon::build_snapshot_with_sidebar(&[], None);
        snapshot.events.push(crate::daemon::TransitionEvent {
            pane_id: "%1".to_string(),
            agent: "codex".to_string(),
            from: Some(crate::daemon::session_badge::BadgeState::Working),
            to: crate::daemon::session_badge::BadgeState::Blocked,
            at_epoch: 880,
        });
        let theme = SidebarRenderTheme::from_app_config(&crate::config::Config::default());

        let lines = event_tail(&snapshot, 3, 1000, &theme);

        assert_eq!(lines, vec!["2m前 codex ● → ▲".to_string()]);
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
            events: Vec::new(),
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
                        badge_state: None,
                        expanded: true,
                        pane_id: None,
                        git: None,
                        meta: None,
                    },
                    row(),
                ],
            }),
            events: Vec::new(),
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
                    badge_state: None,
                    expanded: true,
                    pane_id: None,
                    git: None,
                    meta: None,
                }],
            }),
            events: Vec::new(),
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
                        badge_state: None,
                        expanded: true,
                        pane_id: None,
                        git: None,
                        meta: None,
                    },
                    row(),
                ],
            }),
            events: Vec::new(),
        };

        assert_eq!(
            row_for_click(&snapshot, 1, 1, 0).map(|row| row.id.as_str()),
            Some("repo::misc::app")
        );
        assert_eq!(
            row_for_click(&snapshot, 2, 1, 0).map(|row| row.id.as_str()),
            Some("chat::%1")
        );
        assert_eq!(row_for_click(&snapshot, 0, 1, 0), None);
    }

    #[test]
    fn repo_click_toggles_immediately() {
        assert_eq!(
            single_click_action(&ClickedRow::new(
                "repo::misc::app",
                SidebarRowKind::Repo,
                None
            )),
            Some(ClickAction::ToggleRow("repo::misc::app".to_string()))
        );
    }

    #[test]
    fn detail_click_toggles_row_immediately() {
        assert_eq!(
            single_click_action(&ClickedRow::new(
                "detail::%1::status",
                SidebarRowKind::Detail,
                Some("%1")
            )),
            Some(ClickAction::ToggleRow("detail::%1::status".to_string()))
        );
    }

    #[test]
    fn drain_snapshot_updates_reports_disconnect() {
        let (tx, rx) = mpsc::channel();
        drop(tx);
        let mut current = None;

        assert_eq!(
            drain_snapshot_updates(&rx, &mut current),
            Err(TuiExit::Disconnected)
        );
    }

    #[test]
    fn live_capture_result_updates_only_current_pane() {
        let mut live = LiveState {
            pane_id: Some("%1".to_string()),
            requested_lines: 2,
            capture_in_flight: true,
            ..LiveState::default()
        };

        apply_live_capture_result(&mut live, "%2", "stale\noutput\n");

        assert!(live.lines.is_empty());
        assert!(live.capture_in_flight);

        apply_live_capture_result(&mut live, "%1", "one\ntwo\nthree\n");

        assert_eq!(live.lines, vec!["two".to_string(), "three".to_string()]);
        assert!(!live.capture_in_flight);
    }
}

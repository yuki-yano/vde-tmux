use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::PopupConfig;
use crate::session::{
    current_client_name, current_session_name, exact_session_target, list_sessions,
    switch_client_for_client,
};
use crate::tmux::{TmuxRunner, tmux_args};
use crate::window::{list_windows, list_windows_for_target};
use anyhow::{Context, Result};
use unicode_width::UnicodeWidthStr;

const FIELD_SEP: char = '\t';
const ACTIVE_ACTIVITY_THRESHOLD_SECONDS: i64 = 60 * 60;
const PREVIEW_BOX_FALLBACK_WIDTH: usize = 76;
const PREVIEW_BOX_MIN_WIDTH: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagerEntry {
    action: String,
    name: String,
    session: String,
    target: String,
    display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectorRow {
    action: String,
    name: String,
    session: String,
    target: String,
    columns: Vec<String>,
}

pub trait SessionManagerIo {
    fn choose(&mut self, rows: &[String]) -> Result<Option<String>>;
}

trait SessionAttachIo {
    fn attach_session(&mut self, target: &str) -> Result<()>;
}

pub struct SystemSessionManagerIo;
struct SystemSessionAttachIo {
    socket_name: Option<String>,
}

impl SystemSessionAttachIo {
    fn from_env(env: &BTreeMap<String, String>) -> Self {
        Self {
            socket_name: env
                .get("VDE_TMUX_SOCKET_NAME")
                .filter(|value| !value.trim().is_empty())
                .cloned(),
        }
    }
}

impl SessionManagerIo for SystemSessionManagerIo {
    fn choose(&mut self, rows: &[String]) -> Result<Option<String>> {
        if rows.is_empty() {
            return Ok(None);
        }
        let executable = std::env::current_exe().context("failed to resolve current executable")?;
        let preview_command = format!(
            "FORCE_COLOR=1 {} session-manager --popup --render-preview {{1}} --preview-name {{2}}",
            shell_quote(executable.to_string_lossy().as_ref())
        );
        let mut child = Command::new("fzf")
            .args([
                "--ansi",
                "--prompt=tmux> ",
                "--header=Current tmux | Enter switch | C-q kill | C-t new | C-r rename | C-d/C-u scroll",
                "--border=none",
                "--delimiter=\t",
                "--with-nth=5",
                "--cycle",
                "--reverse",
                "--height=100%",
                "--no-info",
                "--no-sort",
                "--exact",
                "--expect=enter,ctrl-q,ctrl-t,ctrl-r",
                "--multi",
                "--preview-window=right:65%:border-left",
                "--bind=ctrl-d:preview-page-down",
                "--bind=ctrl-u:preview-page-up",
            ])
            .arg(format!("--preview={preview_command}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn fzf")?;
        if let Some(mut stdin) = child.stdin.take() {
            for row in rows {
                writeln!(stdin, "{row}")?;
            }
        }
        let output = child.wait_with_output().context("failed to wait fzf")?;
        if !output.status.success() {
            return Ok(None);
        }
        let selected = String::from_utf8(output.stdout)
            .context("fzf output was not utf-8")?
            .trim()
            .to_string();
        Ok((!selected.is_empty()).then_some(selected))
    }
}

impl SessionAttachIo for SystemSessionAttachIo {
    fn attach_session(&mut self, target: &str) -> Result<()> {
        let owned_args = tmux_args(
            self.socket_name.as_deref(),
            &["attach-session", "-t", target],
        );
        let status = Command::new("tmux")
            .args(&owned_args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to attach tmux session")?;
        if !status.success() {
            anyhow::bail!("tmux attach-session failed with exit {status:?}");
        }
        Ok(())
    }
}

pub fn open_popup(runner: &dyn TmuxRunner, popup: &PopupConfig, exe: &str) -> Result<()> {
    let pane_path = runner
        .run(&["display-message", "-p", "#{pane_current_path}"])?
        .trim()
        .to_string();
    runner.run(&[
        "display-popup",
        "-E",
        "-w",
        &popup.width,
        "-h",
        &popup.height,
        "-d",
        &pane_path,
        exe,
        "session-manager",
        "--popup",
    ])?;
    Ok(())
}

pub fn run_interactive(runner: &dyn TmuxRunner) -> Result<()> {
    let mut io = SystemSessionManagerIo;
    run_interactive_with_io(runner, &mut io)
}

pub fn run_interactive_outside_tmux(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    let mut io = SystemSessionManagerIo;
    let mut attach = SystemSessionAttachIo::from_env(env);
    run_interactive_outside_tmux_with_io(runner, &mut io, &mut attach)
}

pub fn run_interactive_with_io(
    runner: &dyn TmuxRunner,
    io: &mut dyn SessionManagerIo,
) -> Result<()> {
    ensure_tmux_server(runner)?;
    let entries = build_entries(runner)?;
    let rows = entries.iter().map(render_entry).collect::<Vec<_>>();
    let Some(selected) = io.choose(&rows)? else {
        return Ok(());
    };
    run_selection(runner, &selected)
}

fn run_interactive_outside_tmux_with_io(
    runner: &dyn TmuxRunner,
    io: &mut dyn SessionManagerIo,
    attach: &mut dyn SessionAttachIo,
) -> Result<()> {
    ensure_tmux_server(runner)?;
    let entries = build_entries(runner)?;
    let rows = entries.iter().map(render_entry).collect::<Vec<_>>();
    let Some(selected) = io.choose(&rows)? else {
        return Ok(());
    };
    run_selection_outside_tmux(runner, &selected, attach)
}

fn ensure_tmux_server(runner: &dyn TmuxRunner) -> Result<()> {
    if runner.run(&["has-session"]).is_ok() {
        return Ok(());
    }
    runner
        .run(&["new-session", "-d"])
        .context("failed to start tmux server")?;
    Ok(())
}

fn build_entries(runner: &dyn TmuxRunner) -> Result<Vec<ManagerEntry>> {
    let sessions = list_sessions(runner)?;
    let windows = list_windows(runner)?;
    let current_session = current_session_name(runner).unwrap_or_default();
    let mut rows = Vec::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    for session in sessions {
        let session_windows = windows
            .iter()
            .filter(|window| window.session == session.name)
            .collect::<Vec<_>>();
        let total_panes = session_windows
            .iter()
            .map(|window| window.panes)
            .sum::<i64>();
        let delta = (now - session.created_at).max(0);
        let activity = if delta <= ACTIVE_ACTIVITY_THRESHOLD_SECONDS {
            yellow("*")
        } else {
            gray("·")
        };
        let state_symbol = if session.name == current_session {
            green("●")
        } else if session.attached {
            yellow("○")
        } else {
            gray("·")
        };
        let attached_state = if session.attached {
            yellow("attached")
        } else {
            gray("detached")
        };
        let category = if session.category.is_empty() {
            "-"
        } else {
            &session.category
        };
        let category_label = if session.name == current_session {
            green(format!("[{category}]"))
        } else {
            cyan(format!("[{category}]"))
        };
        rows.push(SelectorRow {
            action: "session".to_string(),
            name: session.name.clone(),
            session: session.name.clone(),
            target: String::new(),
            columns: vec![
                format!("{state_symbol} {activity} {}", bold(&session.name)),
                category_label.clone(),
                format!(
                    "{} {}",
                    gray("win"),
                    cyan(session_windows.len().to_string())
                ),
                format!("{} {}", gray("pane"), cyan(total_panes.to_string())),
                attached_state,
            ],
        });
        for (index, window) in session_windows.iter().enumerate() {
            let branch = if index + 1 == session_windows.len() {
                "└─"
            } else {
                "├─"
            };
            let marker = if window.active {
                green("▸")
            } else {
                gray("·")
            };
            let command = if window.command.is_empty() {
                gray("-")
            } else {
                magenta(truncate_visible(&window.command, 28))
            };
            rows.push(SelectorRow {
                action: "window".to_string(),
                name: window.id.clone(),
                session: session.name.clone(),
                target: window.id.clone(),
                columns: vec![
                    format!(
                        "  {branch} {marker} {}:{} {}",
                        bold(&session.name),
                        cyan(&window.index),
                        truncate_visible(&window.name, 24),
                    ),
                    category_label.clone(),
                    format!("{} {}", gray("pane"), cyan(window.panes.to_string())),
                    format!("{} {command}", gray("cmd")),
                    String::new(),
                ],
            });
        }
    }
    Ok(render_selector_rows(&rows))
}

fn render_selector_rows(rows: &[SelectorRow]) -> Vec<ManagerEntry> {
    let widths = compute_column_widths(rows);
    rows.iter()
        .map(|row| {
            let limit = last_used_column_index(&row.columns);
            let mut segments = Vec::new();
            for index in 0..=limit {
                let column = row.columns.get(index).cloned().unwrap_or_default();
                if index < limit {
                    segments.push(pad_visible(&column, *widths.get(index).unwrap_or(&0)));
                } else {
                    segments.push(column);
                }
            }
            ManagerEntry {
                action: row.action.clone(),
                name: row.name.clone(),
                session: row.session.clone(),
                target: row.target.clone(),
                display: segments.join(&format!(" {} ", gray("|"))),
            }
        })
        .collect()
}

fn compute_column_widths(rows: &[SelectorRow]) -> Vec<usize> {
    let max_columns = rows.iter().map(|row| row.columns.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; max_columns];
    for row in rows {
        let limit = last_used_column_index(&row.columns);
        for (index, width) in widths.iter_mut().enumerate().take(limit) {
            let column = row.columns.get(index).map(String::as_str).unwrap_or("");
            *width = (*width).max(visible_width(column));
        }
    }
    widths
}

fn last_used_column_index(columns: &[String]) -> usize {
    for index in (0..columns.len()).rev() {
        if columns
            .get(index)
            .map(|value| !strip_ansi(value).trim().is_empty())
            .unwrap_or(false)
        {
            return index;
        }
    }
    0
}

fn render_entry(entry: &ManagerEntry) -> String {
    [
        entry.action.as_str(),
        entry.name.as_str(),
        entry.session.as_str(),
        entry.target.as_str(),
        entry.display.as_str(),
    ]
    .join(&FIELD_SEP.to_string())
}

fn parse_selected_entry(row: &str) -> Option<ManagerEntry> {
    let fields = row.split(FIELD_SEP).collect::<Vec<_>>();
    if fields.len() < 5 {
        return None;
    }
    let action = fields[0];
    if !matches!(action, "session" | "window") || fields[1].is_empty() {
        return None;
    }
    Some(ManagerEntry {
        action: action.to_string(),
        name: fields[1].to_string(),
        session: fields[2].to_string(),
        target: fields[3].to_string(),
        display: fields[4..].join(&FIELD_SEP.to_string()),
    })
}

fn run_selection(runner: &dyn TmuxRunner, selected: &str) -> Result<()> {
    let (key, entries) = parse_selection_output(selected);
    match key.as_str() {
        "ctrl-q" => return kill_selection(runner, &entries),
        "ctrl-t" => return create_and_switch_session(runner),
        "ctrl-r" => return rename_selection(runner, entries.first()),
        _ => {}
    }
    let Some(entry) = entries.first() else {
        return Ok(());
    };
    let session = if entry.session.is_empty() {
        &entry.name
    } else {
        &entry.session
    };
    let client = current_client_name(runner)?;
    switch_client_for_client(runner, &client, session)?;
    if entry.action == "window" && !entry.target.is_empty() {
        runner.run(&["select-window", "-t", &entry.target])?;
    }
    Ok(())
}

fn run_selection_outside_tmux(
    runner: &dyn TmuxRunner,
    selected: &str,
    attach: &mut dyn SessionAttachIo,
) -> Result<()> {
    let (key, entries) = parse_selection_output(selected);
    match key.as_str() {
        "ctrl-q" => return kill_selection(runner, &entries),
        "ctrl-t" => return create_and_attach_session(runner, attach),
        "ctrl-r" => return rename_selection(runner, entries.first()),
        _ => {}
    }
    let Some(entry) = entries.first() else {
        return Ok(());
    };
    let session = if entry.session.is_empty() {
        &entry.name
    } else {
        &entry.session
    };
    let target = if entry.action == "window" && !entry.target.is_empty() {
        entry.target.clone()
    } else {
        exact_session_target(session)
    };
    attach.attach_session(&target)
}

fn parse_selection_output(output: &str) -> (String, Vec<ManagerEntry>) {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next() else {
        return ("enter".to_string(), Vec::new());
    };
    let (key, selection_lines): (String, Vec<&str>) =
        if matches!(first, "enter" | "ctrl-q" | "ctrl-t" | "ctrl-r") {
            (first.to_string(), lines.collect())
        } else {
            (
                "enter".to_string(),
                std::iter::once(first).chain(lines).collect(),
            )
        };
    (
        key,
        selection_lines
            .into_iter()
            .filter_map(parse_selected_entry)
            .collect(),
    )
}

fn kill_selection(runner: &dyn TmuxRunner, entries: &[ManagerEntry]) -> Result<()> {
    let session_targets = entries
        .iter()
        .filter_map(|entry| {
            if entry.action == "session" {
                Some(entry.name.as_str())
            } else if entry.action == "window" {
                Some(entry.session.as_str())
            } else {
                None
            }
        })
        .filter(|session| !session.is_empty())
        .collect::<Vec<_>>();
    if let Ok(current) = current_session_name(runner)
        && session_targets.iter().any(|session| *session == current)
        && let Some(fallback) = list_sessions(runner)?
            .iter()
            .find(|session| !session_targets.contains(&session.name.as_str()))
    {
        let client = current_client_name(runner)?;
        switch_client_for_client(runner, &client, &fallback.name)?;
    }
    for entry in entries {
        match entry.action.as_str() {
            "session" => {
                runner.run(&["kill-session", "-t", &exact_session_target(&entry.name)])?;
            }
            "window" => {
                let target = if entry.target.is_empty() {
                    &entry.name
                } else {
                    &entry.target
                };
                runner.run(&["kill-window", "-t", target])?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn create_and_switch_session(runner: &dyn TmuxRunner) -> Result<()> {
    let session = runner
        .run(&["new-session", "-d", "-P", "-F", "#{session_name}"])?
        .trim()
        .to_string();
    if session.is_empty() {
        return Ok(());
    }
    let client = current_client_name(runner)?;
    switch_client_for_client(runner, &client, &session)
}

fn create_and_attach_session(
    runner: &dyn TmuxRunner,
    attach: &mut dyn SessionAttachIo,
) -> Result<()> {
    let session = runner
        .run(&["new-session", "-d", "-P", "-F", "#{session_name}"])?
        .trim()
        .to_string();
    if session.is_empty() {
        return Ok(());
    }
    attach.attach_session(&exact_session_target(&session))
}

fn rename_selection(runner: &dyn TmuxRunner, entry: Option<&ManagerEntry>) -> Result<()> {
    let Some(entry) = entry else {
        return Ok(());
    };
    let session = if entry.session.is_empty() {
        &entry.name
    } else {
        &entry.session
    };
    if session.is_empty() {
        return Ok(());
    }
    let command = format!(
        "rename-session -t {} '%%'",
        shell_quote(&exact_session_target(session))
    );
    runner.run(&["command-prompt", "-I", session, &command])?;
    Ok(())
}

pub fn render_preview(
    runner: &dyn TmuxRunner,
    action: &str,
    name: &str,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    match action {
        "session" => render_session_preview(runner, name, env),
        "window" => render_window_preview(runner, name, env),
        _ => Ok("Preview not available".to_string()),
    }
}

fn render_session_preview(
    runner: &dyn TmuxRunner,
    session_name: &str,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let target = exact_session_target(session_name);
    let sessions = list_sessions(runner)?;
    let windows = list_windows_for_target(runner, &target)?;
    let session = sessions.iter().find(|session| session.name == session_name);
    let active_window = windows
        .iter()
        .find(|window| window.active)
        .or_else(|| windows.first());
    let active_path = pane_current_path(runner, &target).unwrap_or_default();
    let capture_lines = compute_session_capture_lines(env, windows.len(), 30);
    let pane_tail = capture_pane_tail(runner, &target, capture_lines).unwrap_or_default();

    let mut lines = Vec::new();
    lines.extend(render_header_box(&format!("Session {session_name}"), env));
    lines.push(String::new());
    lines.extend(render_info_block(
        &[
            (
                "Status".to_string(),
                session
                    .map(|session| {
                        if session.attached {
                            "attached".to_string()
                        } else {
                            "detached".to_string()
                        }
                    })
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            ("Windows".to_string(), windows.len().to_string()),
            (
                "Created".to_string(),
                session
                    .map(|session| format_ago_from_epoch(session.created_at))
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            (
                "Category".to_string(),
                session
                    .map(|session| {
                        if session.category.is_empty() {
                            "-".to_string()
                        } else {
                            session.category.clone()
                        }
                    })
                    .unwrap_or_else(|| "-".to_string()),
            ),
            (
                "Path".to_string(),
                if active_path.is_empty() {
                    "(unknown)".to_string()
                } else {
                    truncate_visible(shorten_home(&active_path, env), 52)
                },
            ),
        ],
        "Session Info",
    ));
    lines.push(String::new());
    lines.push("┌─ Windows".to_string());
    if windows.is_empty() {
        lines.push("└─ (no windows)".to_string());
    } else {
        for (index, window) in windows.iter().enumerate() {
            let branch = if index + 1 == windows.len() {
                "└─"
            } else {
                "├─"
            };
            let marker = if window.active { "▸" } else { "·" };
            let command_label = if window.command.is_empty() {
                String::new()
            } else {
                format!(" cmd:{}", truncate_visible(&window.command, 28))
            };
            lines.push(format!(
                "{branch} {marker} {} {} [{}P]{}",
                window.index,
                truncate_visible(&window.name, 26),
                window.panes,
                command_label
            ));
        }
    }
    lines.push(String::new());
    let preview_title = active_window
        .map(|window| format!("Active Pane {session_name}:{}.0", window.index))
        .unwrap_or_else(|| "Active Pane".to_string());
    lines.extend(render_pane_preview_block(
        &format!("{preview_title} (last {capture_lines} lines)"),
        &pane_tail,
        "(preview not available)",
        env,
    ));
    Ok(lines.join("\n"))
}

fn render_window_preview(
    runner: &dyn TmuxRunner,
    target: &str,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let panes = list_panes(runner, target)?;
    let active_path = pane_current_path(runner, target).unwrap_or_default();
    let capture_lines = compute_per_pane_capture_lines(env, panes.len(), 10);
    let pane_tail = capture_pane_tail(runner, target, capture_lines).unwrap_or_default();

    let mut lines = Vec::new();
    lines.extend(render_header_box(&format!("Window {target}"), env));
    lines.push(String::new());
    lines.extend(render_info_block(
        &[
            ("Panes".to_string(), panes.len().to_string()),
            (
                "Path".to_string(),
                if active_path.is_empty() {
                    "(unknown)".to_string()
                } else {
                    truncate_visible(shorten_home(&active_path, env), 52)
                },
            ),
        ],
        "Window Info",
    ));
    lines.push(String::new());
    lines.push("┌─ Pane List".to_string());
    if panes.is_empty() {
        lines.push("└─ (no panes)".to_string());
    } else {
        for (index, pane) in panes.iter().enumerate() {
            let branch = if index + 1 == panes.len() {
                "└─"
            } else {
                "├─"
            };
            let marker = if pane.active { "▸" } else { "·" };
            lines.push(format!(
                "{branch} {marker} {}: {} {} ({}x{})",
                pane.index,
                get_pane_icon(&pane.command),
                truncate_visible(
                    if pane.command.is_empty() {
                        "(unknown)"
                    } else {
                        &pane.command
                    },
                    20
                ),
                pane.width,
                pane.height
            ));
        }
    }
    lines.push(String::new());
    lines.extend(render_pane_preview_block(
        &format!("Active Pane (last {capture_lines} lines)"),
        &pane_tail,
        "(preview not available)",
        env,
    ));
    Ok(lines.join("\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneInfo {
    index: String,
    command: String,
    width: i64,
    height: i64,
    active: bool,
}

fn list_panes(runner: &dyn TmuxRunner, target: &str) -> Result<Vec<PaneInfo>> {
    let format = [
        "#{pane_index}",
        "#{pane_current_command}",
        "#{pane_width}",
        "#{pane_height}",
        "#{pane_active}",
    ]
    .join(&FIELD_SEP.to_string());
    let output = runner.run(&["list-panes", "-t", target, "-F", &format])?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEP).collect::<Vec<_>>();
            if fields.is_empty() || fields[0].is_empty() {
                return None;
            }
            Some(PaneInfo {
                index: fields[0].to_string(),
                command: fields.get(1).copied().unwrap_or("").to_string(),
                width: fields
                    .get(2)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
                height: fields
                    .get(3)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
                active: fields.get(4).copied().unwrap_or("") == "1",
            })
        })
        .collect())
}

fn pane_current_path(runner: &dyn TmuxRunner, target: &str) -> Result<String> {
    Ok(runner
        .run(&[
            "display-message",
            "-p",
            "-t",
            target,
            "#{pane_current_path}",
        ])?
        .trim()
        .to_string())
}

fn capture_pane_tail(runner: &dyn TmuxRunner, target: &str, lines: i64) -> Result<Vec<String>> {
    let start = format!("-{}", lines.max(1));
    Ok(runner
        .run(&["capture-pane", "-epJ", "-t", target, "-S", &start])?
        .lines()
        .map(ToOwned::to_owned)
        .collect())
}

fn compute_session_capture_lines(
    env: &BTreeMap<String, String>,
    window_count: usize,
    fallback: i64,
) -> i64 {
    let preview_lines = read_preview_lines(env).unwrap_or(0).max(0);
    let margin = 11 + 1 + window_count as i64 + 1;
    if preview_lines > margin + 3 {
        preview_lines - margin
    } else {
        fallback
    }
}

fn compute_per_pane_capture_lines(
    env: &BTreeMap<String, String>,
    pane_count: usize,
    fallback: i64,
) -> i64 {
    let preview_lines = read_preview_lines(env).unwrap_or(0).max(0);
    let static_lines = 4 + pane_count.max(1) as i64 + 6;
    if preview_lines > static_lines + 3 {
        return (preview_lines - static_lines).max(fallback);
    }
    fallback
}

fn read_preview_lines(env: &BTreeMap<String, String>) -> Option<i64> {
    env.get("FZF_PREVIEW_LINES")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
}

fn read_preview_columns(env: &BTreeMap<String, String>) -> Option<usize> {
    env.get("FZF_PREVIEW_COLUMNS")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn resolve_box_width(env: &BTreeMap<String, String>) -> usize {
    read_preview_columns(env)
        .map(|columns| columns.saturating_sub(2).max(PREVIEW_BOX_MIN_WIDTH))
        .unwrap_or(PREVIEW_BOX_FALLBACK_WIDTH)
}

fn render_header_box(title: &str, env: &BTreeMap<String, String>) -> Vec<String> {
    let width = resolve_box_width(env);
    let normalized = pad_visible(
        &format!(" {} ", truncate_visible(title, width.saturating_sub(2))),
        width,
    );
    vec![
        format!("╔{}╗", "═".repeat(width)),
        format!("║{}║", normalized),
        format!("╚{}╝", "═".repeat(width)),
    ]
}

fn render_info_block(rows: &[(String, String)], title: &str) -> Vec<String> {
    let mut lines = vec![format!("┌─ {title}")];
    if rows.is_empty() {
        lines.push("└─ (none)".to_string());
        return lines;
    }
    for (index, row) in rows.iter().enumerate() {
        let branch = if index + 1 == rows.len() {
            "└─"
        } else {
            "├─"
        };
        lines.push(format!("{branch} {:<14} {}", row.0, row.1));
    }
    lines
}

fn render_pane_preview_block(
    title: &str,
    pane_lines: &[String],
    empty_label: &str,
    env: &BTreeMap<String, String>,
) -> Vec<String> {
    let inner_width = resolve_box_width(env);
    let normalized_title = pad_visible(
        &format!(
            " {} ",
            truncate_visible(title, inner_width.saturating_sub(2))
        ),
        inner_width,
    );
    let body = if pane_lines.is_empty() {
        vec![empty_label.to_string()]
    } else {
        pane_lines.to_vec()
    };
    let mut lines = Vec::new();
    lines.push(format!("┌{}┐", "─".repeat(inner_width)));
    lines.push(format!("│{}│", normalized_title));
    lines.push(format!("├{}┤", "─".repeat(inner_width)));
    for line in body {
        let content = pad_visible(&truncate_visible(line.trim_end(), inner_width), inner_width);
        lines.push(format!("│{}│", content));
    }
    lines.push(format!("└{}┘", "─".repeat(inner_width)));
    lines
}

fn format_ago_from_epoch(epoch_seconds: i64) -> String {
    if epoch_seconds <= 0 {
        return "unknown".to_string();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(epoch_seconds);
    let delta = (now - epoch_seconds).max(0);
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

fn shorten_home(path: &str, env: &BTreeMap<String, String>) -> String {
    let Some(home) = env.get("HOME") else {
        return path.to_string();
    };
    if path == home {
        "~".to_string()
    } else if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
        format!("~/{rest}")
    } else {
        path.to_string()
    }
}

fn get_pane_icon(command: &str) -> &'static str {
    match command {
        "nvim" | "vim" | "vi" => "",
        "zsh" | "bash" | "fish" | "sh" => "",
        "node" | "bun" | "deno" => "",
        "git" => "",
        "cargo" | "rustc" => "",
        _ => "•",
    }
}

fn ansi(code: &str, text: impl AsRef<str>) -> String {
    format!("\u{1b}[{code}m{}\u{1b}[0m", text.as_ref())
}

fn bold(text: impl AsRef<str>) -> String {
    ansi("1", text)
}

fn green(text: impl AsRef<str>) -> String {
    ansi("32", text)
}

fn yellow(text: impl AsRef<str>) -> String {
    ansi("33", text)
}

fn magenta(text: impl AsRef<str>) -> String {
    ansi("35", text)
}

fn cyan(text: impl AsRef<str>) -> String {
    ansi("36", text)
}

fn gray(text: impl AsRef<str>) -> String {
    ansi("90", text)
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        output.push(ch);
    }
    output
}

fn visible_width(text: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi(text).as_str())
}

fn truncate_visible(text: impl AsRef<str>, max_width: usize) -> String {
    let text = text.as_ref();
    let plain = strip_ansi(text);
    if UnicodeWidthStr::width(plain.as_str()) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let target = max_width - 1;
    let mut output = String::new();
    let mut width = 0usize;
    for ch in plain.chars() {
        let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
        if width + ch_width > target {
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output.push('…');
    output
}

fn pad_visible(text: &str, width: usize) -> String {
    let current = visible_width(text);
    if current >= width {
        return truncate_visible(text, width);
    }
    format!("{text}{}", " ".repeat(width - current))
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

pub fn kill_window(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    runner.run(&["kill-window", "-t", target])?;
    Ok(())
}

pub fn kill_pane(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    runner.run(&["kill-pane", "-t", target])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    struct MockSessionManagerIo {
        selection: Option<String>,
        seen_rows: Vec<String>,
    }

    #[derive(Default)]
    struct MockSessionAttachIo {
        targets: Vec<String>,
    }

    impl SessionManagerIo for MockSessionManagerIo {
        fn choose(&mut self, rows: &[String]) -> Result<Option<String>> {
            self.seen_rows = rows.to_vec();
            Ok(self.selection.clone())
        }
    }

    impl SessionAttachIo for MockSessionAttachIo {
        fn attach_session(&mut self, target: &str) -> Result<()> {
            self.targets.push(target.to_string());
            Ok(())
        }
    }

    fn window_row(
        session: &str,
        index: &str,
        id: &str,
        name: &str,
        panes: &str,
        active: &str,
        command: &str,
    ) -> String {
        [
            session, index, id, name, panes, active, "0", "0", "0", "0", command,
        ]
        .join("\u{1f}")
    }

    #[test]
    fn open_popup_uses_display_popup_for_inner_session_manager() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["display-message", "-p", "#{pane_current_path}"],
            "/tmp/project\n",
        );
        mock.stub(
            &[
                "display-popup",
                "-E",
                "-w",
                "50%",
                "-h",
                "50%",
                "-d",
                "/tmp/project",
                "/tmp/my vt",
                "session-manager",
                "--popup",
            ],
            "",
        );

        open_popup(&mock, &PopupConfig::default(), "/tmp/my vt").unwrap();

        assert_eq!(
            mock.calls(),
            vec![
                vec![
                    "display-message".to_string(),
                    "-p".to_string(),
                    "#{pane_current_path}".to_string(),
                ],
                vec![
                    "display-popup".to_string(),
                    "-E".to_string(),
                    "-w".to_string(),
                    "50%".to_string(),
                    "-h".to_string(),
                    "50%".to_string(),
                    "-d".to_string(),
                    "/tmp/project".to_string(),
                    "/tmp/my vt".to_string(),
                    "session-manager".to_string(),
                    "--popup".to_string(),
                ]
            ]
        );
    }

    #[test]
    fn interactive_session_manager_switches_selected_session() {
        let mock = MockTmuxRunner::new();
        let session_format = crate::session::session_list_format();
        let window_format = crate::window::window_list_format();
        mock.stub(&["has-session"], "");
        mock.stub(
            &["list-sessions", "-F", &session_format],
            "main\u{1f}1\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}$1\nni.zsh\u{1f}0\u{1f}90\u{1f}public\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(
            &["list-windows", "-a", "-F", &window_format],
            &format!(
                "{}\n{}\n",
                window_row("main", "1", "@1", "zsh", "1", "1", "zsh"),
                window_row("ni.zsh", "1", "@2", "zsh", "1", "1", "zsh")
            ),
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["switch-client", "-c", "abc", "-t", "=ni.zsh:"], "");
        let selected = render_entry(&ManagerEntry {
            action: "session".to_string(),
            name: "ni.zsh".to_string(),
            session: "ni.zsh".to_string(),
            target: String::new(),
            display: "· ni.zsh  [public]".to_string(),
        });
        let mut io = MockSessionManagerIo {
            selection: Some(selected),
            seen_rows: Vec::new(),
        };

        run_interactive_with_io(&mock, &mut io).unwrap();

        assert!(io.seen_rows.iter().any(|row| row.contains("ni.zsh")));
    }

    #[test]
    fn interactive_session_manager_starts_tmux_server_when_missing() {
        let mock = MockTmuxRunner::new();
        let session_format = crate::session::session_list_format();
        let window_format = crate::window::window_list_format();
        mock.stub(&["new-session", "-d"], "");
        mock.stub(
            &["list-sessions", "-F", &session_format],
            "main\u{1f}0\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}$1\n",
        );
        mock.stub(
            &["list-windows", "-a", "-F", &window_format],
            &format!(
                "{}\n",
                window_row("main", "1", "@1", "zsh", "1", "1", "zsh")
            ),
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        let mut io = MockSessionManagerIo {
            selection: None,
            seen_rows: Vec::new(),
        };

        run_interactive_with_io(&mock, &mut io).unwrap();

        assert!(io.seen_rows.iter().any(|row| row.contains("main")));
        assert_eq!(
            &mock.calls()[..2],
            &[
                vec!["has-session".to_string()],
                vec!["new-session".to_string(), "-d".to_string()],
            ]
        );
    }

    #[test]
    fn outside_tmux_session_manager_attaches_selected_session() {
        let mock = MockTmuxRunner::new();
        let session_format = crate::session::session_list_format();
        let window_format = crate::window::window_list_format();
        mock.stub(&["has-session"], "");
        mock.stub(
            &["list-sessions", "-F", &session_format],
            "main\u{1f}0\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}$1\nni.zsh\u{1f}0\u{1f}90\u{1f}public\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(
            &["list-windows", "-a", "-F", &window_format],
            &format!(
                "{}\n{}\n",
                window_row("main", "1", "@1", "zsh", "1", "1", "zsh"),
                window_row("ni.zsh", "1", "@2", "zsh", "1", "1", "zsh")
            ),
        );
        let selected = render_entry(&ManagerEntry {
            action: "session".to_string(),
            name: "ni.zsh".to_string(),
            session: "ni.zsh".to_string(),
            target: String::new(),
            display: "· ni.zsh  [public]".to_string(),
        });
        let mut io = MockSessionManagerIo {
            selection: Some(selected),
            seen_rows: Vec::new(),
        };
        let mut attach = MockSessionAttachIo::default();

        run_interactive_outside_tmux_with_io(&mock, &mut io, &mut attach).unwrap();

        assert!(io.seen_rows.iter().any(|row| row.contains("ni.zsh")));
        assert_eq!(attach.targets, vec!["=ni.zsh:"]);
        assert!(
            !mock
                .calls()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("switch-client"))
        );
    }

    #[test]
    fn outside_tmux_session_manager_attaches_selected_window() {
        let mock = MockTmuxRunner::new();
        let selected = render_entry(&ManagerEntry {
            action: "window".to_string(),
            name: "@9".to_string(),
            session: "ni.zsh".to_string(),
            target: "@9".to_string(),
            display: "  └─ ni.zsh:2 editor".to_string(),
        });
        let mut attach = MockSessionAttachIo::default();

        run_selection_outside_tmux(&mock, &selected, &mut attach).unwrap();

        assert!(mock.calls().is_empty());
        assert_eq!(attach.targets, vec!["@9"]);
    }

    #[test]
    fn interactive_session_manager_switches_selected_window() {
        let mock = MockTmuxRunner::new();
        let session_format = crate::session::session_list_format();
        let window_format = crate::window::window_list_format();
        mock.stub(&["has-session"], "");
        mock.stub(
            &["list-sessions", "-F", &session_format],
            "ni.zsh\u{1f}1\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(
            &["list-windows", "-a", "-F", &window_format],
            &format!(
                "{}\n",
                window_row("ni.zsh", "2", "@9", "editor", "2", "1", "nvim")
            ),
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "ni.zsh\n");
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["switch-client", "-c", "abc", "-t", "=ni.zsh:"], "");
        mock.stub(&["select-window", "-t", "@9"], "");
        let selected = render_entry(&ManagerEntry {
            action: "window".to_string(),
            name: "@9".to_string(),
            session: "ni.zsh".to_string(),
            target: "@9".to_string(),
            display: "  └─ ni.zsh:2 editor".to_string(),
        });
        let mut io = MockSessionManagerIo {
            selection: Some(selected),
            seen_rows: Vec::new(),
        };

        run_interactive_with_io(&mock, &mut io).unwrap();

        assert_eq!(
            mock.calls().last().unwrap(),
            &vec![
                "select-window".to_string(),
                "-t".to_string(),
                "@9".to_string()
            ]
        );
    }

    #[test]
    fn ctrl_q_kills_selected_session() {
        let mock = MockTmuxRunner::new();
        let selected = render_entry(&ManagerEntry {
            action: "session".to_string(),
            name: "ni.zsh".to_string(),
            session: "ni.zsh".to_string(),
            target: String::new(),
            display: "ni.zsh".to_string(),
        });
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(&["kill-session", "-t", "=ni.zsh:"], "");

        run_selection(&mock, &format!("ctrl-q\n{selected}")).unwrap();

        assert_eq!(
            mock.calls(),
            vec![
                vec![
                    "display-message".to_string(),
                    "-p".to_string(),
                    "#{session_name}".to_string()
                ],
                vec![
                    "kill-session".to_string(),
                    "-t".to_string(),
                    "=ni.zsh:".to_string()
                ]
            ]
        );
    }

    #[test]
    fn ctrl_q_kills_selected_window() {
        let mock = MockTmuxRunner::new();
        let selected = render_entry(&ManagerEntry {
            action: "window".to_string(),
            name: "@9".to_string(),
            session: "ni.zsh".to_string(),
            target: "@9".to_string(),
            display: "ni.zsh:2".to_string(),
        });
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(&["kill-window", "-t", "@9"], "");

        run_selection(&mock, &format!("ctrl-q\n{selected}")).unwrap();

        assert_eq!(
            mock.calls(),
            vec![
                vec![
                    "display-message".to_string(),
                    "-p".to_string(),
                    "#{session_name}".to_string()
                ],
                vec![
                    "kill-window".to_string(),
                    "-t".to_string(),
                    "@9".to_string()
                ]
            ]
        );
    }

    #[test]
    fn ctrl_t_creates_and_switches_to_new_session() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["new-session", "-d", "-P", "-F", "#{session_name}"],
            "created\n",
        );
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["switch-client", "-c", "abc", "-t", "=created:"], "");

        run_selection(&mock, "ctrl-t").unwrap();

        assert_eq!(
            mock.calls().last().unwrap(),
            &vec![
                "switch-client".to_string(),
                "-c".to_string(),
                "abc".to_string(),
                "-t".to_string(),
                "=created:".to_string(),
            ]
        );
    }

    #[test]
    fn ctrl_r_opens_rename_prompt_for_selected_session() {
        let mock = MockTmuxRunner::new();
        let selected = render_entry(&ManagerEntry {
            action: "session".to_string(),
            name: "ni.zsh".to_string(),
            session: "ni.zsh".to_string(),
            target: String::new(),
            display: "ni.zsh".to_string(),
        });
        mock.stub(
            &[
                "command-prompt",
                "-I",
                "ni.zsh",
                "rename-session -t '=ni.zsh:' '%%'",
            ],
            "",
        );

        run_selection(&mock, &format!("ctrl-r\n{selected}")).unwrap();

        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn ctrl_q_switches_to_fallback_before_killing_current_session() {
        let mock = MockTmuxRunner::new();
        let session_format = crate::session::session_list_format();
        let selected = render_entry(&ManagerEntry {
            action: "session".to_string(),
            name: "main".to_string(),
            session: "main".to_string(),
            target: String::new(),
            display: "main".to_string(),
        });
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &session_format],
            "main\u{1f}1\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}$1\nother\u{1f}0\u{1f}90\u{1f}public\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["switch-client", "-c", "abc", "-t", "=other:"], "");
        mock.stub(&["kill-session", "-t", "=main:"], "");

        run_selection(&mock, &format!("ctrl-q\n{selected}")).unwrap();

        assert_eq!(
            mock.calls().last().unwrap(),
            &vec![
                "kill-session".to_string(),
                "-t".to_string(),
                "=main:".to_string()
            ]
        );
    }

    #[test]
    fn render_preview_for_session_includes_windows_and_capture() {
        let mock = MockTmuxRunner::new();
        let session_format = crate::session::session_list_format();
        let window_format = crate::window::window_list_format();
        mock.stub(
            &["list-sessions", "-F", &session_format],
            "ni.zsh\u{1f}1\u{1f}100\u{1f}public\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(
            &["list-windows", "-t", "=ni.zsh:", "-F", &window_format],
            &format!(
                "{}\n",
                window_row("ni.zsh", "2", "@9", "editor", "2", "1", "nvim")
            ),
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "=ni.zsh:",
                "#{pane_current_path}",
            ],
            "/Users/yuki/project\n",
        );
        mock.stub(
            &["capture-pane", "-epJ", "-t", "=ni.zsh:", "-S", "-30"],
            "hello\nworld\n",
        );

        let preview = render_preview(&mock, "session", "ni.zsh", &BTreeMap::new()).unwrap();

        assert!(preview.contains("Session ni.zsh"));
        assert!(preview.contains("editor"));
        assert!(preview.contains("hello"));
    }

    #[test]
    fn kill_window_issues_tmux_kill_window() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["kill-window", "-t", "@2"], "");
        kill_window(&mock, "@2").unwrap();
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn kill_pane_issues_tmux_kill_pane() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["kill-pane", "-t", "%3"], "");
        kill_pane(&mock, "%3").unwrap();
        assert_eq!(mock.calls().len(), 1);
    }
}

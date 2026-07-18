use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::SidebarWidth;
use crate::options::KEY_SIDEBAR_MARKER;
use crate::tmux::TmuxRunner;

pub const SIDEBAR_PANE_FORMAT: &str = "#{pane_id}\t#{@vde_sidebar}\t#{pane_width}";
pub(crate) const ENV_SELECTION_PANE: &str = "VDE_TMUX_SELECTION_PANE";
pub(crate) const ENV_SELECTION_PANE_PID: &str = "VDE_TMUX_SELECTION_PANE_PID";
pub(crate) const ENV_SELECTION_SESSION: &str = "VDE_TMUX_SELECTION_SESSION";
const RAIL_WIDTH: u16 = 3;
const AFTER_NEW_WINDOW_HOOK: &str = "after-new-window[90]";
const PANE_EXIT_HOOK: &str = "pane-exited[90]";
pub(crate) const SOURCE_CLIENT_MISMATCH_SENTINEL: &str = "__vde_source_client_mismatch__";
const TARGET_PANE_MISMATCH_SENTINEL: &str = "__vde_target_pane_mismatch__";

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarPane {
    pane_id: String,
    width: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarOpenMode {
    Focused,
    Detached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SidebarAttachContext {
    pub pane: Option<String>,
    pub pane_pid: Option<u32>,
    pub session: Option<String>,
}

impl SidebarAttachContext {
    pub(crate) fn new(
        pane: Option<String>,
        pane_pid: Option<u32>,
        session: Option<String>,
    ) -> Option<Self> {
        let pane = normalize_context_value(pane);
        let session = normalize_context_value(session);
        let pane_pid = pane.as_ref().and(pane_pid);
        (pane.is_some() || session.is_some()).then_some(Self {
            pane,
            pane_pid,
            session,
        })
    }
}

fn normalize_context_value(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn attach(runner: &dyn TmuxRunner, env: &BTreeMap<String, String>) -> Result<()> {
    let pane = env
        .get("TMUX_PANE")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .context("TMUX_PANE is not set; `sidebar attach` must run inside a tmux pane")?;
    runner.run(&["set-option", "-p", "-t", pane, KEY_SIDEBAR_MARKER, "1"])?;
    Ok(())
}

pub fn open(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    open_with_attach_context(runner, target, self_exe, width, min_width, None)
}

pub(crate) fn open_with_attach_context(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
    attach_context: Option<&SidebarAttachContext>,
) -> Result<()> {
    if let Some(sidebar) = find_sidebar_pane(runner, target)? {
        runner.run(&["select-pane", "-t", &sidebar.pane_id])?;
        return Ok(());
    }
    open_unchecked(
        runner,
        target,
        self_exe,
        width,
        min_width,
        attach_context,
        SidebarOpenMode::Focused,
    )
}

pub fn open_if_auto_all_enabled(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    if !auto_all_enabled(runner)? {
        return Ok(());
    }
    if find_sidebar_pane(runner, target)?.is_some() {
        return Ok(());
    }
    open_unchecked(
        runner,
        target,
        self_exe,
        width,
        min_width,
        None,
        SidebarOpenMode::Detached,
    )
}

pub fn close(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
    };
    close_sidebar_pane(runner, target, &sidebar)
}

fn close_sidebar_pane(runner: &dyn TmuxRunner, target: &str, sidebar: &SidebarPane) -> Result<()> {
    let layout = capture_window_layout(runner, target)?;
    let content_layout = layout_without_sidebar(&layout, &sidebar.pane_id).with_context(|| {
        format!("failed to preserve pane ratios while closing sidebar in {target}")
    })?;

    runner.run(&["kill-pane", "-t", &sidebar.pane_id])?;

    if let Some(layout) = content_layout {
        runner.run(&["select-layout", "-t", target, &layout])?;
    }
    Ok(())
}

pub fn toggle(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    toggle_with_attach_context(runner, target, self_exe, width, min_width, None)
}

pub(crate) fn toggle_with_attach_context(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
    attach_context: Option<&SidebarAttachContext>,
) -> Result<()> {
    if let Some(sidebar) = find_sidebar_pane(runner, target)? {
        close_sidebar_pane(runner, target, &sidebar)
    } else {
        open_unchecked(
            runner,
            target,
            self_exe,
            width,
            min_width,
            attach_context,
            SidebarOpenMode::Focused,
        )
    }
}

pub fn toggle_all(
    runner: &dyn TmuxRunner,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    toggle_all_with_attach_context(runner, self_exe, width, min_width, None)
}

pub(crate) fn toggle_all_with_attach_context(
    runner: &dyn TmuxRunner,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
    attach_context: Option<&SidebarAttachContext>,
) -> Result<()> {
    let windows = list_window_ids(runner)?;
    let sidebars = windows
        .iter()
        .map(|window| Ok((window.clone(), find_sidebar_pane(runner, window)?)))
        .collect::<Result<Vec<_>>>()?;
    if !sidebars.is_empty() && sidebars.iter().all(|(_, sidebar)| sidebar.is_some()) {
        for (window, sidebar) in sidebars {
            if let Some(sidebar) = sidebar {
                close_sidebar_pane(runner, &window, &sidebar)?;
            }
        }
        uninstall_auto_hooks(runner)?;
    } else {
        for (window, sidebar) in sidebars {
            if sidebar.is_none() {
                open_unchecked(
                    runner,
                    &window,
                    self_exe,
                    width,
                    min_width,
                    attach_context,
                    SidebarOpenMode::Detached,
                )?;
            }
        }
        install_auto_hooks(runner, self_exe, width)?;
    }
    Ok(())
}

pub fn focus_toggle(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    focus_toggle_with_attach_context(runner, target, self_exe, width, min_width, None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingSidebarFocusToggle {
    Missing,
    Focused,
    Closed,
}

pub(crate) fn focus_toggle_existing(
    runner: &dyn TmuxRunner,
    target: &str,
) -> Result<ExistingSidebarFocusToggle> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(ExistingSidebarFocusToggle::Missing);
    };
    let active = runner
        .run(&["display-message", "-p", "-t", target, "#{pane_id}"])?
        .trim()
        .to_string();
    if active == sidebar.pane_id {
        close_sidebar_pane(runner, target, &sidebar)?;
        Ok(ExistingSidebarFocusToggle::Closed)
    } else {
        runner.run(&["select-pane", "-t", &sidebar.pane_id])?;
        Ok(ExistingSidebarFocusToggle::Focused)
    }
}

pub(crate) fn focus_toggle_with_attach_context(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
    attach_context: Option<&SidebarAttachContext>,
) -> Result<()> {
    if focus_toggle_existing(runner, target)? == ExistingSidebarFocusToggle::Missing {
        open_unchecked(
            runner,
            target,
            self_exe,
            width,
            min_width,
            attach_context,
            SidebarOpenMode::Focused,
        )?;
    }
    Ok(())
}

pub fn rail(
    runner: &dyn TmuxRunner,
    target: &str,
    normal_width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
    };
    let next_width = if sidebar.width <= RAIL_WIDTH {
        let layout = capture_window_layout(runner, target)?;
        resolve_width(&layout, normal_width, min_width)?
    } else {
        RAIL_WIDTH
    };
    runner.run(&[
        "resize-pane",
        "-t",
        &sidebar.pane_id,
        "-x",
        &next_width.to_string(),
    ])?;
    Ok(())
}

pub fn jump_to_pane(runner: &dyn TmuxRunner, pane_id: &str) -> Result<()> {
    jump_to_pane_with_client(runner, pane_id, None, None)
}

pub fn jump_to_pane_for_client(
    runner: &dyn TmuxRunner,
    pane: &crate::pane_state::PaneInstance,
    client_pid: u32,
    source_pane: &crate::pane_state::PaneInstance,
) -> Result<()> {
    jump_to_pane_with_client(
        runner,
        &pane.pane_id,
        Some(pane.pane_pid),
        Some((client_pid, source_pane)),
    )
}

pub fn jump_to_pane_for_named_client(
    runner: &dyn TmuxRunner,
    pane: &crate::pane_state::PaneInstance,
    client_name: &str,
) -> Result<()> {
    if client_name.trim().is_empty() {
        bail!("explicit tmux client name must not be empty");
    }
    let target = resolve_jump_target(runner, &pane.pane_id, Some(pane.pane_pid))?;
    let pane_guard = format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid);
    let switch = crate::pane_state::store::tmux_command_string(&[
        "switch-client".to_string(),
        "-c".to_string(),
        client_name.to_string(),
        "-t".to_string(),
        target.clone(),
    ]);
    let mismatch = format!("display-message -p '{TARGET_PANE_MISMATCH_SENTINEL}'");
    let output = runner.run(&[
        "if-shell",
        "-F",
        "-t",
        &target,
        &pane_guard,
        &switch,
        &mismatch,
    ])?;
    if output
        .lines()
        .any(|line| line.trim() == TARGET_PANE_MISMATCH_SENTINEL)
    {
        bail!(TARGET_PANE_MISMATCH_SENTINEL);
    }
    Ok(())
}

fn jump_to_pane_with_client(
    runner: &dyn TmuxRunner,
    pane_id: &str,
    pane_pid: Option<u32>,
    client: Option<(u32, &crate::pane_state::PaneInstance)>,
) -> Result<()> {
    let target = resolve_jump_target(runner, pane_id, pane_pid)?;
    if let Some((client_pid, source_pane)) = client {
        const FIELD_SEP: char = '\u{1f}';
        let clients = runner.run(&["list-clients", "-F", "#{client_pid}\u{1f}#{client_name}"])?;
        let mut names = clients.lines().filter_map(|line| {
            let (pid, name) = line.split_once(FIELD_SEP)?;
            (pid.parse::<u32>().ok() == Some(client_pid) && !name.is_empty()).then_some(name)
        });
        let client = names
            .next()
            .with_context(|| format!("tmux client not found: {client_pid}"))?;
        if names.next().is_some() {
            anyhow::bail!("multiple tmux clients matched PID {client_pid}");
        }
        let source_guard = format!(
            "#{{&&:#{{==:#{{pane_id}},{}}},#{{==:#{{pane_pid}},{}}}}}",
            source_pane.pane_id, source_pane.pane_pid
        );
        let switch = crate::pane_state::store::tmux_command_string(&[
            "switch-client".to_string(),
            "-c".to_string(),
            client.to_string(),
            "-t".to_string(),
            target.clone(),
        ]);
        let mismatch = format!("display-message -p '{SOURCE_CLIENT_MISMATCH_SENTINEL}'");
        let output = runner.run(&[
            "if-shell",
            "-F",
            "-t",
            client,
            &source_guard,
            &switch,
            &mismatch,
        ])?;
        if output
            .lines()
            .any(|line| line.trim() == SOURCE_CLIENT_MISMATCH_SENTINEL)
        {
            anyhow::bail!(SOURCE_CLIENT_MISMATCH_SENTINEL);
        }
    } else {
        runner.run(&["switch-client", "-t", &target])?;
    }
    Ok(())
}

fn resolve_jump_target(
    runner: &dyn TmuxRunner,
    pane_id: &str,
    pane_pid: Option<u32>,
) -> Result<String> {
    const FIELD_SEP: char = '\u{1f}';
    let format =
        ["#{session_id}", "#{window_id}", "#{pane_id}", "#{pane_pid}"].join(&FIELD_SEP.to_string());
    let output = runner.run(&["list-panes", "-a", "-F", &format])?;
    let (session_id, window_id) = output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(FIELD_SEP);
            let session_id = fields.next()?;
            let window_id = fields.next()?;
            let candidate_pane_id = fields.next()?;
            let candidate_pane_pid = fields.next()?.parse::<u32>().ok()?;
            (fields.next().is_none()
                && candidate_pane_id == pane_id
                && pane_pid.is_none_or(|expected| candidate_pane_pid == expected))
            .then_some((session_id, window_id))
        })
        .next()
        .with_context(|| format!("pane not found: {pane_id}"))?;
    Ok(format!("{session_id}:{window_id}.{pane_id}"))
}

pub fn focus(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
    };
    runner.run(&["select-pane", "-t", &sidebar.pane_id])?;
    Ok(())
}

pub fn layout_applied(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    let Some(panes) = capture_existing_pane_ids(runner, target)? else {
        return Ok(());
    };
    if let Some(sidebar) = find_sidebar_pane(runner, target)? {
        return reconcile_existing_sidebar(runner, &panes, &sidebar);
    }
    open_unchecked(
        runner,
        target,
        self_exe,
        width,
        min_width,
        None,
        SidebarOpenMode::Detached,
    )
}

pub fn layout_changed(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let Some(panes) = capture_existing_pane_ids(runner, target)? else {
        return Ok(());
    };
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
    };
    reconcile_existing_sidebar(runner, &panes, &sidebar)
}

fn reconcile_existing_sidebar(
    runner: &dyn TmuxRunner,
    panes: &BTreeSet<String>,
    sidebar: &SidebarPane,
) -> Result<()> {
    if panes.len() == 1 && panes.contains(&sidebar.pane_id) {
        return close_lonely_sidebar_pane(runner, sidebar);
    }
    Ok(())
}

fn close_lonely_sidebar_pane(runner: &dyn TmuxRunner, sidebar: &SidebarPane) -> Result<()> {
    runner.run(&["kill-pane", "-t", &sidebar.pane_id])?;
    Ok(())
}

fn open_unchecked(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
    attach_context: Option<&SidebarAttachContext>,
    mode: SidebarOpenMode,
) -> Result<()> {
    let layout = capture_window_layout(runner, target)?;
    let width = resolve_width(&layout, width, min_width)?;
    let socket_name = std::env::var("VDE_TMUX_SOCKET_NAME")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let command = attach_shell_command(self_exe, socket_name.as_deref(), attach_context);
    let width = width.to_string();
    let mut args = vec!["split-window"];
    if mode == SidebarOpenMode::Detached {
        args.push("-d");
    }
    args.extend(["-t", target, "-hbf", "-l", &width, &command]);
    runner.run(&args)?;
    Ok(())
}

fn resolve_width(layout: &str, width: SidebarWidth, min_width: u16) -> Result<u16> {
    match width {
        SidebarWidth::Columns(columns) => Ok(columns),
        SidebarWidth::Percent(percent) => {
            let (window_width, _) = parse_layout_root_size(layout)
                .with_context(|| format!("failed to parse window layout size from {layout:?}"))?;
            let resolved = window_width.saturating_mul(percent as u32) / 100;
            u16::try_from(resolved.max(min_width as u32))
                .context("resolved sidebar width exceeds tmux pane width limit")
        }
    }
}

fn parse_layout_root_size(layout: &str) -> Option<(u32, u32)> {
    let (_, rest) = layout.split_once(',')?;
    parse_size(rest)
}

fn parse_size(value: &str) -> Option<(u32, u32)> {
    let (width, rest) = value.split_once('x')?;
    let height_end = rest
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(rest.len());
    if height_end == 0 {
        return None;
    }
    Some((width.parse().ok()?, rest[..height_end].parse().ok()?))
}

fn find_sidebar_pane(runner: &dyn TmuxRunner, target: &str) -> Result<Option<SidebarPane>> {
    let output = runner.run(&["list-panes", "-t", target, "-F", SIDEBAR_PANE_FORMAT])?;
    Ok(output.lines().find_map(parse_sidebar_pane_line))
}

fn parse_sidebar_pane_line(line: &str) -> Option<SidebarPane> {
    let mut fields = line.split('\t');
    let pane_id = fields.next()?.trim();
    let marker = fields.next()?.trim();
    let width = fields
        .next()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or_default();
    if pane_id.is_empty() || marker != "1" {
        return None;
    }
    Some(SidebarPane {
        pane_id: pane_id.to_string(),
        width,
    })
}

fn capture_window_layout(runner: &dyn TmuxRunner, target: &str) -> Result<String> {
    Ok(runner
        .run(&[
            "display-message",
            "-p",
            "-t",
            target,
            "-F",
            "#{window_layout}",
        ])?
        .trim()
        .to_string())
}

fn capture_pane_ids(runner: &dyn TmuxRunner, target: &str) -> Result<BTreeSet<String>> {
    Ok(runner
        .run(&["list-panes", "-t", target, "-F", "#{pane_id}"])?
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn layout_without_sidebar(layout: &str, sidebar_pane_id: &str) -> Result<Option<String>> {
    let root = parse_tmux_layout(layout)?;
    if !layout_contains_pane(&root, sidebar_pane_id) {
        bail!("sidebar pane {sidebar_pane_id} not found in window layout");
    }
    let root_sx = root.sx;
    let root_sy = root.sy;
    let Some(mut content) = remove_layout_pane(root, sidebar_pane_id) else {
        return Ok(None);
    };
    resize_layout_cell(&mut content, root_sx, root_sy, 0, 0)?;
    Ok(Some(format_tmux_layout(&content)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TmuxLayoutCell {
    sx: u32,
    sy: u32,
    xoff: i32,
    yoff: i32,
    pane_id: Option<String>,
    kind: TmuxLayoutKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TmuxLayoutKind {
    Leaf,
    LeftRight(Vec<TmuxLayoutCell>),
    TopBottom(Vec<TmuxLayoutCell>),
}

fn parse_tmux_layout(layout: &str) -> Result<TmuxLayoutCell> {
    let layout = layout.trim();
    if layout.contains('<') || layout.contains('>') {
        bail!("floating pane layouts are not supported");
    }
    let (checksum, body) = layout
        .split_once(',')
        .ok_or_else(|| anyhow!("invalid tmux layout"))?;
    if checksum.len() != 4 {
        bail!("invalid tmux layout checksum");
    }
    let expected = u16::from_str_radix(checksum, 16).context("invalid tmux layout checksum")?;
    let actual = tmux_layout_checksum(body);
    if expected != actual {
        bail!("invalid tmux layout checksum");
    }

    let mut parser = TmuxLayoutParser::new(body);
    let root = parser.parse_cell()?;
    if !parser.is_complete() {
        bail!("invalid trailing tmux layout input");
    }
    Ok(root)
}

fn format_tmux_layout(root: &TmuxLayoutCell) -> String {
    let body = format_layout_cell(root);
    format!("{:04x},{}", tmux_layout_checksum(&body), body)
}

fn tmux_layout_checksum(layout: &str) -> u16 {
    layout.bytes().fold(0u16, |checksum, byte| {
        checksum.rotate_right(1).wrapping_add(byte as u16)
    })
}

fn format_layout_cell(cell: &TmuxLayoutCell) -> String {
    let mut body = format!("{}x{},{},{}", cell.sx, cell.sy, cell.xoff, cell.yoff);
    if let Some(pane_id) = cell.pane_id.as_deref() {
        body.push(',');
        body.push_str(pane_id.trim_start_matches('%'));
    }
    match &cell.kind {
        TmuxLayoutKind::Leaf => body,
        TmuxLayoutKind::LeftRight(children) => {
            body.push('{');
            body.push_str(
                &children
                    .iter()
                    .map(format_layout_cell)
                    .collect::<Vec<_>>()
                    .join(","),
            );
            body.push('}');
            body
        }
        TmuxLayoutKind::TopBottom(children) => {
            body.push('[');
            body.push_str(
                &children
                    .iter()
                    .map(format_layout_cell)
                    .collect::<Vec<_>>()
                    .join(","),
            );
            body.push(']');
            body
        }
    }
}

fn remove_layout_pane(cell: TmuxLayoutCell, pane_id: &str) -> Option<TmuxLayoutCell> {
    let TmuxLayoutCell {
        sx,
        sy,
        xoff,
        yoff,
        pane_id: cell_pane_id,
        kind,
    } = cell;
    match kind {
        TmuxLayoutKind::Leaf => {
            (cell_pane_id.as_deref() != Some(pane_id)).then_some(TmuxLayoutCell {
                sx,
                sy,
                xoff,
                yoff,
                pane_id: cell_pane_id,
                kind: TmuxLayoutKind::Leaf,
            })
        }
        TmuxLayoutKind::LeftRight(children) => {
            let cell = TmuxLayoutCell {
                sx,
                sy,
                xoff,
                yoff,
                pane_id: cell_pane_id,
                kind: TmuxLayoutKind::LeftRight(Vec::new()),
            };
            collapse_removed_layout_cell(
                cell,
                TmuxLayoutKind::LeftRight(remove_children(children, pane_id)),
            )
        }
        TmuxLayoutKind::TopBottom(children) => {
            let cell = TmuxLayoutCell {
                sx,
                sy,
                xoff,
                yoff,
                pane_id: cell_pane_id,
                kind: TmuxLayoutKind::TopBottom(Vec::new()),
            };
            collapse_removed_layout_cell(
                cell,
                TmuxLayoutKind::TopBottom(remove_children(children, pane_id)),
            )
        }
    }
}

fn remove_children(children: Vec<TmuxLayoutCell>, pane_id: &str) -> Vec<TmuxLayoutCell> {
    children
        .into_iter()
        .filter_map(|child| remove_layout_pane(child, pane_id))
        .collect()
}

fn collapse_removed_layout_cell(
    mut cell: TmuxLayoutCell,
    kind: TmuxLayoutKind,
) -> Option<TmuxLayoutCell> {
    let children = match kind {
        TmuxLayoutKind::Leaf => unreachable!("removed layout node cannot become leaf here"),
        TmuxLayoutKind::LeftRight(children) | TmuxLayoutKind::TopBottom(children) => children,
    };
    match children.len() {
        0 => None,
        1 => children.into_iter().next(),
        _ => {
            cell.kind = match cell.kind {
                TmuxLayoutKind::LeftRight(_) => TmuxLayoutKind::LeftRight(children),
                TmuxLayoutKind::TopBottom(_) => TmuxLayoutKind::TopBottom(children),
                TmuxLayoutKind::Leaf => unreachable!("removed layout node cannot be leaf"),
            };
            Some(cell)
        }
    }
}

fn layout_contains_pane(cell: &TmuxLayoutCell, pane_id: &str) -> bool {
    match &cell.kind {
        TmuxLayoutKind::Leaf => cell.pane_id.as_deref() == Some(pane_id),
        TmuxLayoutKind::LeftRight(children) | TmuxLayoutKind::TopBottom(children) => children
            .iter()
            .any(|child| layout_contains_pane(child, pane_id)),
    }
}

fn resize_layout_cell(
    cell: &mut TmuxLayoutCell,
    sx: u32,
    sy: u32,
    xoff: i32,
    yoff: i32,
) -> Result<()> {
    cell.sx = sx;
    cell.sy = sy;
    cell.xoff = xoff;
    cell.yoff = yoff;

    match &mut cell.kind {
        TmuxLayoutKind::Leaf => Ok(()),
        TmuxLayoutKind::LeftRight(children) => {
            let child_count = children.len();
            let total = size_without_borders(sx, child_count)?;
            let sizes = children.iter().map(|child| child.sx).collect::<Vec<_>>();
            let widths = allocate_proportional(&sizes, total)?;
            let mut child_xoff = xoff;
            for (child, width) in children.iter_mut().zip(widths) {
                resize_layout_cell(child, width, sy, child_xoff, yoff)?;
                child_xoff += width as i32 + 1;
            }
            Ok(())
        }
        TmuxLayoutKind::TopBottom(children) => {
            let child_count = children.len();
            let total = size_without_borders(sy, child_count)?;
            let sizes = children.iter().map(|child| child.sy).collect::<Vec<_>>();
            let heights = allocate_proportional(&sizes, total)?;
            let mut child_yoff = yoff;
            for (child, height) in children.iter_mut().zip(heights) {
                resize_layout_cell(child, sx, height, xoff, child_yoff)?;
                child_yoff += height as i32 + 1;
            }
            Ok(())
        }
    }
}

fn size_without_borders(size: u32, child_count: usize) -> Result<u32> {
    if child_count == 0 {
        bail!("tmux layout node has no children");
    }
    let borders = (child_count - 1) as u32;
    if size <= borders {
        bail!("tmux layout is too small for its child count");
    }
    Ok(size - borders)
}

fn allocate_proportional(sizes: &[u32], total: u32) -> Result<Vec<u32>> {
    if sizes.is_empty() {
        return Ok(Vec::new());
    }
    let minimum = sizes.len() as u32;
    if total < minimum {
        bail!("tmux layout is too small for pane count");
    }

    let sum = sizes.iter().copied().sum::<u32>();
    if sum == 0 {
        return allocate_evenly(sizes.len(), total);
    }

    let remaining = total - minimum;
    let mut allocated = Vec::with_capacity(sizes.len());
    let mut remainders = Vec::with_capacity(sizes.len());
    let mut used = 0u32;
    for (index, size) in sizes.iter().copied().enumerate() {
        let weighted = remaining as u64 * size as u64;
        let extra = (weighted / sum as u64) as u32;
        allocated.push(1 + extra);
        remainders.push((weighted % sum as u64, index));
        used += 1 + extra;
    }

    let mut leftover = total - used;
    remainders.sort_by(|left, right| right.cmp(left));
    for (_, index) in remainders {
        if leftover == 0 {
            break;
        }
        allocated[index] += 1;
        leftover -= 1;
    }
    Ok(allocated)
}

fn allocate_evenly(count: usize, total: u32) -> Result<Vec<u32>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let count_u32 = count as u32;
    if total < count_u32 {
        bail!("tmux layout is too small for pane count");
    }
    let base = total / count_u32;
    let mut extra = total % count_u32;
    Ok((0..count)
        .map(|_| {
            let size = base + u32::from(extra > 0);
            extra = extra.saturating_sub(1);
            size
        })
        .collect())
}

struct TmuxLayoutParser<'a> {
    input: &'a str,
    cursor: usize,
}

impl<'a> TmuxLayoutParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, cursor: 0 }
    }

    fn parse_cell(&mut self) -> Result<TmuxLayoutCell> {
        let sx = self.parse_u32()?;
        self.expect_byte(b'x')?;
        let sy = self.parse_u32()?;
        self.expect_byte(b',')?;
        let xoff = self.parse_i32()?;
        self.expect_byte(b',')?;
        let yoff = self.parse_i32()?;
        let pane_id = self.parse_optional_pane_id();

        let kind = match self.peek_byte() {
            Some(b'{') => {
                self.cursor += 1;
                TmuxLayoutKind::LeftRight(self.parse_children(b'}')?)
            }
            Some(b'[') => {
                self.cursor += 1;
                TmuxLayoutKind::TopBottom(self.parse_children(b']')?)
            }
            _ => TmuxLayoutKind::Leaf,
        };

        Ok(TmuxLayoutCell {
            sx,
            sy,
            xoff,
            yoff,
            pane_id,
            kind,
        })
    }

    fn parse_children(&mut self, closing: u8) -> Result<Vec<TmuxLayoutCell>> {
        let mut children = Vec::new();
        loop {
            children.push(self.parse_cell()?);
            match self.peek_byte() {
                Some(b',') => self.cursor += 1,
                Some(byte) if byte == closing => {
                    self.cursor += 1;
                    break;
                }
                _ => bail!("invalid tmux layout children"),
            }
        }
        Ok(children)
    }

    fn parse_optional_pane_id(&mut self) -> Option<String> {
        if self.peek_byte() != Some(b',') {
            return None;
        }
        let saved = self.cursor;
        self.cursor += 1;
        let start = self.cursor;
        self.consume_digits();
        if start == self.cursor || self.peek_byte() == Some(b'x') {
            self.cursor = saved;
            return None;
        }
        Some(format!("%{}", &self.input[start..self.cursor]))
    }

    fn parse_u32(&mut self) -> Result<u32> {
        let start = self.cursor;
        self.consume_digits();
        if start == self.cursor {
            bail!("expected tmux layout number");
        }
        self.input[start..self.cursor]
            .parse()
            .context("invalid tmux layout number")
    }

    fn parse_i32(&mut self) -> Result<i32> {
        let start = self.cursor;
        if matches!(self.peek_byte(), Some(b'-' | b'+')) {
            self.cursor += 1;
        }
        let digits_start = self.cursor;
        self.consume_digits();
        if digits_start == self.cursor {
            bail!("expected tmux layout offset");
        }
        self.input[start..self.cursor]
            .parse()
            .context("invalid tmux layout offset")
    }

    fn consume_digits(&mut self) {
        while matches!(self.peek_byte(), Some(byte) if byte.is_ascii_digit()) {
            self.cursor += 1;
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<()> {
        match self.peek_byte() {
            Some(byte) if byte == expected => {
                self.cursor += 1;
                Ok(())
            }
            _ => bail!("invalid tmux layout separator"),
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.cursor).copied()
    }

    fn is_complete(&self) -> bool {
        self.cursor == self.input.len()
    }
}

fn capture_existing_pane_ids(
    runner: &dyn TmuxRunner,
    target: &str,
) -> Result<Option<BTreeSet<String>>> {
    match capture_pane_ids(runner, target) {
        Ok(panes) if panes.is_empty() => Ok(None),
        Ok(panes) => Ok(Some(panes)),
        Err(_) => Ok(None),
    }
}

fn list_window_ids(runner: &dyn TmuxRunner) -> Result<Vec<String>> {
    Ok(runner
        .run(&["list-windows", "-a", "-F", "#{window_id}"])?
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn auto_all_enabled(runner: &dyn TmuxRunner) -> Result<bool> {
    let output = runner.run(&["show-hooks", "-g", AFTER_NEW_WINDOW_HOOK])?;
    Ok(output.lines().any(|line| {
        line.trim_start()
            .strip_prefix(AFTER_NEW_WINDOW_HOOK)
            .map(|command| !command.trim().is_empty())
            .unwrap_or(false)
    }))
}

fn install_auto_hooks(runner: &dyn TmuxRunner, self_exe: &Path, width: SidebarWidth) -> Result<()> {
    install_after_new_window_hook(runner, self_exe, width)?;
    install_pane_exit_hook(runner, self_exe)
}

fn uninstall_auto_hooks(runner: &dyn TmuxRunner) -> Result<()> {
    uninstall_after_new_window_hook(runner)?;
    uninstall_pane_exit_hook(runner)
}

fn install_after_new_window_hook(
    runner: &dyn TmuxRunner,
    self_exe: &Path,
    width: SidebarWidth,
) -> Result<()> {
    let command = new_window_hook_command(self_exe, width);
    runner.run(&["set-hook", "-g", AFTER_NEW_WINDOW_HOOK, &command])?;
    Ok(())
}

fn uninstall_after_new_window_hook(runner: &dyn TmuxRunner) -> Result<()> {
    runner.run(&["set-hook", "-gu", AFTER_NEW_WINDOW_HOOK])?;
    Ok(())
}

fn install_pane_exit_hook(runner: &dyn TmuxRunner, self_exe: &Path) -> Result<()> {
    let command = pane_exit_hook_command(self_exe);
    runner.run(&["set-hook", "-g", PANE_EXIT_HOOK, &command])?;
    Ok(())
}

fn uninstall_pane_exit_hook(runner: &dyn TmuxRunner) -> Result<()> {
    runner.run(&["set-hook", "-gu", PANE_EXIT_HOOK])?;
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

fn attach_shell_command(
    self_exe: &Path,
    socket_name: Option<&str>,
    attach_context: Option<&SidebarAttachContext>,
) -> String {
    let command = format!(
        "{} sidebar attach",
        shell_quote(&self_exe.display().to_string())
    );
    let mut env = Vec::new();
    if let Some(socket_name) = socket_name.filter(|value| !value.trim().is_empty()) {
        env.push(format!("VDE_TMUX_SOCKET_NAME={}", shell_quote(socket_name)));
    }
    if let Some(context) = attach_context {
        if let Some(pane) = context.pane.as_deref() {
            env.push(format!("{ENV_SELECTION_PANE}={}", shell_quote(pane)));
        }
        if let Some(pane_pid) = context.pane_pid {
            env.push(format!("{ENV_SELECTION_PANE_PID}={pane_pid}"));
        }
        if let Some(session) = context.session.as_deref() {
            env.push(format!("{ENV_SELECTION_SESSION}={}", shell_quote(session)));
        }
    }
    if env.is_empty() {
        command
    } else {
        format!("{} {command}", env.join(" "))
    }
}

fn new_window_hook_command(self_exe: &Path, width: SidebarWidth) -> String {
    let width = sidebar_width_arg(width);
    let command = format!(
        "{} sidebar layout-applied --window {} --width {}",
        shell_quote(&self_exe.display().to_string()),
        shell_quote("#{window_id}"),
        shell_quote(&width),
    );
    let command = match std::env::var("VDE_TMUX_SOCKET_NAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        Some(socket_name) => format!(
            "VDE_TMUX_SOCKET_NAME={} {command}",
            shell_quote(&socket_name)
        ),
        None => command,
    };
    format!("run-shell {}", shell_quote(&command))
}

fn pane_exit_hook_command(self_exe: &Path) -> String {
    let command = format!(
        "{} sidebar layout-changed --window {}",
        shell_quote(&self_exe.display().to_string()),
        shell_quote("#{window_id}"),
    );
    let command = match std::env::var("VDE_TMUX_SOCKET_NAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        Some(socket_name) => format!(
            "VDE_TMUX_SOCKET_NAME={} {command}",
            shell_quote(&socket_name)
        ),
        None => command,
    };
    format!("run-shell {}", shell_quote(&command))
}

fn sidebar_width_arg(width: SidebarWidth) -> String {
    match width {
        SidebarWidth::Columns(columns) => columns.to_string(),
        SidebarWidth::Percent(percent) => format!("{percent}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::KEY_SIDEBAR_MARKER;
    use crate::tmux::mock::MockTmuxRunner;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn exe() -> PathBuf {
        PathBuf::from("/tmp/vt")
    }

    #[test]
    fn attach_shell_command_propagates_tmux_socket_name() {
        assert_eq!(
            attach_shell_command(&exe(), Some("scratch"), None),
            "VDE_TMUX_SOCKET_NAME='scratch' '/tmp/vt' sidebar attach"
        );
        assert_eq!(
            attach_shell_command(&exe(), None, None),
            "'/tmp/vt' sidebar attach"
        );
    }

    #[test]
    fn attach_shell_command_propagates_selection_context() {
        let context = SidebarAttachContext {
            pane: Some("%1".to_string()),
            pane_pid: Some(42),
            session: Some("main".to_string()),
        };

        assert_eq!(
            attach_shell_command(&exe(), Some("scratch"), Some(&context)),
            "VDE_TMUX_SOCKET_NAME='scratch' VDE_TMUX_SELECTION_PANE='%1' VDE_TMUX_SELECTION_PANE_PID=42 VDE_TMUX_SELECTION_SESSION='main' '/tmp/vt' sidebar attach"
        );
    }

    #[test]
    fn new_window_hook_command_runs_layout_applied_for_created_window() {
        assert_eq!(
            new_window_hook_command(&exe(), SidebarWidth::Percent(10)),
            "run-shell ''\\''/tmp/vt'\\'' sidebar layout-applied --window '\\''#{window_id}'\\'' --width '\\''10%'\\'''"
        );
    }

    #[test]
    fn pane_exit_hook_command_runs_layout_changed_for_current_window() {
        assert_eq!(
            pane_exit_hook_command(&exe()),
            "run-shell ''\\''/tmp/vt'\\'' sidebar layout-changed --window '\\''#{window_id}'\\'''"
        );
    }

    #[test]
    fn attach_marks_current_pane_as_sidebar() {
        let mock = MockTmuxRunner::new();
        let env = BTreeMap::from([("TMUX_PANE".to_string(), "%9".to_string())]);
        mock.stub(
            &["set-option", "-p", "-t", "%9", KEY_SIDEBAR_MARKER, "1"],
            "",
        );

        attach(&mock, &env).unwrap();

        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn open_splits_sidebar_pane_without_saving_baseline() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-before\n",
        );
        mock.stub(
            &[
                "split-window",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        open(&mock, "@1", &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 3);
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("set-option"))
        );
    }

    #[test]
    fn open_splits_sidebar_as_the_focused_pane() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-before\n",
        );
        mock.stub(
            &[
                "split-window",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        open(&mock, "@1", &exe(), SidebarWidth::Columns(40), 40).unwrap();

        assert!(mock.calls().contains(&vec![
            "split-window".to_string(),
            "-t".to_string(),
            "@1".to_string(),
            "-hbf".to_string(),
            "-l".to_string(),
            "40".to_string(),
            "'/tmp/vt' sidebar attach".to_string(),
        ]));
    }

    #[test]
    fn open_focuses_an_existing_sidebar() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%9\t1\t40\n",
        );
        mock.stub(&["select-pane", "-t", "%9"], "");

        open(&mock, "@1", &exe(), SidebarWidth::Columns(40), 40).unwrap();

        assert_eq!(
            mock.calls(),
            vec![
                vec![
                    "list-panes".to_string(),
                    "-t".to_string(),
                    "@1".to_string(),
                    "-F".to_string(),
                    SIDEBAR_PANE_FORMAT.to_string(),
                ],
                vec![
                    "select-pane".to_string(),
                    "-t".to_string(),
                    "%9".to_string(),
                ],
            ]
        );
    }

    #[test]
    fn open_resolves_percent_width_from_layout_width() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t640\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "5969,80x24,0,0,1\n",
        );
        mock.stub(
            &[
                "split-window",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        open(
            &mock,
            "@1",
            &exe(),
            crate::config::SidebarWidth::Percent(10),
            40,
        )
        .unwrap();

        let calls = mock.calls();
        assert!(calls.contains(&vec![
            "split-window".to_string(),
            "-t".to_string(),
            "@1".to_string(),
            "-hbf".to_string(),
            "-l".to_string(),
            "40".to_string(),
            "'/tmp/vt' sidebar attach".to_string(),
        ]));
        assert!(!calls.contains(&vec![
            "split-window".to_string(),
            "-t".to_string(),
            "@1".to_string(),
            "-hbf".to_string(),
            "-l".to_string(),
            "64".to_string(),
            "'/tmp/vt' sidebar attach".to_string(),
        ]));
    }

    #[test]
    fn focus_toggle_focuses_sidebar_when_not_active() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t1\t40\n",
        );
        mock.stub(&["display-message", "-p", "-t", "@1", "#{pane_id}"], "%1\n");
        mock.stub(&["select-pane", "-t", "%2"], "");

        focus_toggle(&mock, "@1", &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls[2],
            vec![
                "select-pane".to_string(),
                "-t".to_string(),
                "%2".to_string()
            ]
        );
    }

    #[test]
    fn focus_toggle_closes_sidebar_when_active() {
        let mock = MockTmuxRunner::new();
        let layout = "e565,120x40,0,0{20x40,0,0,2,99x40,21,0,1}";
        let content_layout = "aafe,120x40,0,0,1";
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t1\t40\n",
        );
        mock.stub(&["display-message", "-p", "-t", "@1", "#{pane_id}"], "%2\n");
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            &format!("{layout}\n"),
        );
        mock.stub(&["kill-pane", "-t", "%2"], "");
        mock.stub(&["select-layout", "-t", "@1", content_layout], "");

        focus_toggle(&mock, "@1", &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert!(calls.contains(&vec![
            "kill-pane".to_string(),
            "-t".to_string(),
            "%2".to_string()
        ]));
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("select-pane"))
        );
    }

    #[test]
    fn focus_toggle_opens_missing_sidebar_as_the_focused_pane() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-before\n",
        );
        mock.stub(
            &[
                "split-window",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        focus_toggle(&mock, "@1", &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().any(|call| {
            call.first().map(String::as_str) == Some("split-window")
                && !call.iter().any(|argument| argument == "-d")
        }));
    }

    #[test]
    fn open_ignores_stale_baseline_options() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-current\n",
        );
        mock.stub(
            &[
                "split-window",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        open(&mock, "@1", &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("show-options"))
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("select-layout"))
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("set-option"))
        );
    }

    #[test]
    fn layout_without_sidebar_preserves_remaining_pane_ratios() {
        let layout =
            "ccd0,120x40,0,0{20x40,0,0,9,65x40,21,0[65x29,21,0,1,65x10,21,30,2],33x40,87,0,3}";
        let expected = "c0cd,120x40,0,0{79x40,0,0[79x29,0,0,1,79x10,0,30,2],40x40,80,0,3}";

        assert_eq!(
            layout_without_sidebar(layout, "%9").unwrap(),
            Some(expected.to_string())
        );
    }

    #[test]
    fn layout_without_sidebar_returns_none_when_only_sidebar_remains() {
        assert_eq!(
            layout_without_sidebar("ab06,120x40,0,0,9", "%9").unwrap(),
            None
        );
    }

    #[test]
    fn layout_without_sidebar_rejects_floating_panes() {
        let err = layout_without_sidebar("0000,120x40,0,0<10x10,0,0,9>", "%9").unwrap_err();
        assert!(err.to_string().contains("floating pane layouts"));
    }

    #[test]
    fn open_does_not_clear_stale_baseline_options() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-current\n",
        );
        mock.stub(
            &[
                "split-window",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        open(&mock, "@1", &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("select-layout"))
        );
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("set-option"))
        );
    }

    #[test]
    fn percent_width_is_clamped_to_min_width() {
        assert_eq!(
            resolve_width(
                "abcd,320x80,0,0,1",
                crate::config::SidebarWidth::Percent(10),
                40
            )
            .unwrap(),
            40
        );
    }

    #[test]
    fn fixed_width_is_not_clamped_to_min_width() {
        assert_eq!(
            resolve_width(
                "abcd,320x80,0,0,1",
                crate::config::SidebarWidth::Columns(20),
                40
            )
            .unwrap(),
            20
        );
    }

    #[test]
    fn close_preserves_remaining_pane_ratios_from_current_layout() {
        let mock = MockTmuxRunner::new();
        let layout =
            "ccd0,120x40,0,0{20x40,0,0,9,65x40,21,0[65x29,21,0,1,65x10,21,30,2],33x40,87,0,3}";
        let content_layout = "c0cd,120x40,0,0{79x40,0,0[79x29,0,0,1,79x10,0,30,2],40x40,80,0,3}";
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n%1\t\t80\n%2\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            &format!("{layout}\n"),
        );
        mock.stub(&["kill-pane", "-t", "%9"], "");
        mock.stub(&["select-layout", "-t", "@1", content_layout], "");

        close(&mock, "@1").unwrap();

        assert_eq!(mock.calls().len(), 4);
    }

    #[test]
    fn close_expands_only_remaining_pane_to_window_size() {
        let mock = MockTmuxRunner::new();
        let layout = "e581,120x40,0,0{20x40,0,0,9,99x40,21,0,1}";
        let content_layout = "aafe,120x40,0,0,1";
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            &format!("{layout}\n"),
        );
        mock.stub(&["kill-pane", "-t", "%9"], "");
        mock.stub(&["select-layout", "-t", "@1", content_layout], "");

        close(&mock, "@1").unwrap();

        assert!(mock.calls().contains(&vec![
            "select-layout".to_string(),
            "-t".to_string(),
            "@1".to_string(),
            content_layout.to_string(),
        ]));
    }

    #[test]
    fn close_is_noop_when_sidebar_pane_already_gone() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t\t80\n",
        );

        close(&mock, "@1").unwrap();

        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn close_errors_when_current_layout_does_not_contain_sidebar_pane() {
        let mock = MockTmuxRunner::new();
        let layout_without_marker = "aafe,120x40,0,0,1";
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            &format!("{layout_without_marker}\n"),
        );

        let err = close(&mock, "@1").unwrap_err();

        assert!(err.to_string().contains("failed to preserve pane ratios"));
        assert!(
            !mock
                .calls()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("select-layout"))
        );
    }

    #[test]
    fn rail_toggles_sidebar_width() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n",
        );
        mock.stub(&["resize-pane", "-t", "%9", "-x", "3"], "");

        rail(&mock, "@1", SidebarWidth::Columns(40), 40).unwrap();

        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn rail_resolves_percent_width_when_restoring_normal_width() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t3\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "abcd,640x132,0,0,9\n",
        );
        mock.stub(&["resize-pane", "-t", "%9", "-x", "64"], "");

        rail(&mock, "@1", SidebarWidth::Percent(10), 40).unwrap();

        assert_eq!(mock.calls().len(), 3);
    }

    #[test]
    fn toggle_all_opens_all_windows_when_none_are_open() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-windows", "-a", "-F", "#{window_id}"], "@1\n@2\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-one\n",
        );
        mock.stub(
            &[
                "split-window",
                "-d",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        mock.stub(
            &["list-panes", "-t", "@2", "-F", SIDEBAR_PANE_FORMAT],
            "%2\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@2",
                "-F",
                "#{window_layout}",
            ],
            "layout-two\n",
        );
        mock.stub(
            &[
                "split-window",
                "-d",
                "-t",
                "@2",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );
        mock.stub(
            &[
                "set-hook",
                "-g",
                AFTER_NEW_WINDOW_HOOK,
                &new_window_hook_command(&exe(), SidebarWidth::Columns(40)),
            ],
            "",
        );
        mock.stub(
            &[
                "set-hook",
                "-g",
                "pane-exited[90]",
                &pane_exit_hook_command(&exe()),
            ],
            "",
        );

        toggle_all(&mock, &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 9);
        assert!(calls.iter().any(|call| {
            call.first().map(String::as_str) == Some("set-hook")
                && call.get(2).map(String::as_str) == Some("pane-exited[90]")
        }));
    }

    #[test]
    fn toggle_all_opens_missing_windows_without_closing_existing_sidebars() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-windows", "-a", "-F", "#{window_id}"], "@1\n@2\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-one\n",
        );
        mock.stub(
            &[
                "split-window",
                "-d",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "40",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );
        mock.stub(
            &["list-panes", "-t", "@2", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n%2\t\t80\n",
        );
        mock.stub(
            &[
                "set-hook",
                "-g",
                AFTER_NEW_WINDOW_HOOK,
                &new_window_hook_command(&exe(), SidebarWidth::Columns(40)),
            ],
            "",
        );
        mock.stub(
            &[
                "set-hook",
                "-g",
                "pane-exited[90]",
                &pane_exit_hook_command(&exe()),
            ],
            "",
        );

        toggle_all(&mock, &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("kill-pane"))
        );
        assert!(calls.iter().any(|call| {
            call.first().map(String::as_str) == Some("set-hook")
                && call.get(2).map(String::as_str) == Some(AFTER_NEW_WINDOW_HOOK)
        }));
        assert!(calls.iter().any(|call| {
            call.first().map(String::as_str) == Some("set-hook")
                && call.get(2).map(String::as_str) == Some("pane-exited[90]")
        }));
    }

    #[test]
    fn toggle_all_closes_all_sidebars_and_disables_new_window_hook_when_all_windows_are_open() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-windows", "-a", "-F", "#{window_id}"], "@1\n@2\n");
        for (window, sidebar, pane) in [("@1", "%9", "%1"), ("@2", "%8", "%2")] {
            let (layout, content_layout) = match pane {
                "%1" => (
                    "e581,120x40,0,0{20x40,0,0,9,99x40,21,0,1}",
                    "aafe,120x40,0,0,1",
                ),
                "%2" => (
                    "657e,120x40,0,0{20x40,0,0,8,99x40,21,0,2}",
                    "aaff,120x40,0,0,2",
                ),
                _ => unreachable!(),
            };
            mock.stub(
                &["list-panes", "-t", window, "-F", SIDEBAR_PANE_FORMAT],
                &format!("{sidebar}\t1\t40\n{pane}\t\t80\n"),
            );
            mock.stub(
                &[
                    "display-message",
                    "-p",
                    "-t",
                    window,
                    "-F",
                    "#{window_layout}",
                ],
                &format!("{layout}\n"),
            );
            mock.stub(&["kill-pane", "-t", sidebar], "");
            mock.stub(&["select-layout", "-t", window, content_layout], "");
        }
        mock.stub(&["set-hook", "-gu", AFTER_NEW_WINDOW_HOOK], "");
        mock.stub(&["set-hook", "-gu", "pane-exited[90]"], "");

        toggle_all(&mock, &exe(), SidebarWidth::Columns(40), 40).unwrap();

        let calls = mock.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.first().map(String::as_str) == Some("kill-pane"))
                .count(),
            2
        );
        assert!(calls.contains(&vec![
            "set-hook".to_string(),
            "-gu".to_string(),
            "after-new-window[90]".to_string(),
        ]));
        assert!(calls.contains(&vec![
            "set-hook".to_string(),
            "-gu".to_string(),
            "pane-exited[90]".to_string(),
        ]));
    }

    #[test]
    fn layout_applied_opens_when_sidebar_is_absent() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-before\n",
        );
        mock.stub(
            &[
                "split-window",
                "-d",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "32",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        layout_applied(&mock, "@1", &exe(), SidebarWidth::Columns(32), 40).unwrap();

        assert_eq!(mock.calls().len(), 4);
    }

    #[test]
    fn layout_applied_closes_window_when_only_sidebar_remains() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%9\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n",
        );
        mock.stub(&["kill-pane", "-t", "%9"], "");

        layout_applied(&mock, "@1", &exe(), SidebarWidth::Columns(32), 40).unwrap();

        let calls = mock.calls();
        assert!(calls.contains(&vec![
            "kill-pane".to_string(),
            "-t".to_string(),
            "%9".to_string()
        ]));
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("display-message")),
            "{calls:?}"
        );
    }

    #[test]
    fn layout_changed_closes_window_when_only_sidebar_remains() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%9\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n",
        );
        mock.stub(&["kill-pane", "-t", "%9"], "");

        layout_changed(&mock, "@1").unwrap();

        let calls = mock.calls();
        assert!(calls.contains(&vec![
            "kill-pane".to_string(),
            "-t".to_string(),
            "%9".to_string()
        ]));
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("split-window")),
            "{calls:?}"
        );
    }

    #[test]
    fn layout_changed_does_not_open_when_sidebar_is_absent() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );

        layout_changed(&mock, "@1").unwrap();

        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn layout_applied_keeps_existing_sidebar_when_non_sidebar_pane_remains() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n%9\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%9\t1\t40\n",
        );

        layout_applied(&mock, "@1", &exe(), SidebarWidth::Columns(32), 40).unwrap();

        let calls = mock.calls();
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("kill-pane")),
            "{calls:?}"
        );
        assert_eq!(calls.len(), 2);
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("set-option"))
        );
    }

    #[test]
    fn toggle_closes_when_sidebar_exists() {
        let mock = MockTmuxRunner::new();
        let layout = "e581,120x40,0,0{20x40,0,0,9,99x40,21,0,1}";
        let content_layout = "aafe,120x40,0,0,1";
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            &format!("{layout}\n"),
        );
        mock.stub(&["kill-pane", "-t", "%9"], "");
        mock.stub(&["select-layout", "-t", "@1", content_layout], "");

        toggle(&mock, "@1", &exe(), SidebarWidth::Columns(32), 40).unwrap();

        assert_eq!(mock.calls().len(), 4);
    }

    #[test]
    fn toggle_opens_sidebar_as_the_focused_pane() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@1",
                "-F",
                "#{window_layout}",
            ],
            "layout-before\n",
        );
        mock.stub(
            &[
                "split-window",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "32",
                "'/tmp/vt' sidebar attach",
            ],
            "",
        );

        toggle(&mock, "@1", &exe(), SidebarWidth::Columns(32), 40).unwrap();

        assert!(mock.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("split-window")
                && !call.iter().any(|argument| argument == "-d")
        }));
    }

    #[test]
    fn jump_to_pane_switches_session_window_and_pane() {
        let mock = MockTmuxRunner::new();
        let format = ["#{session_id}", "#{window_id}", "#{pane_id}", "#{pane_pid}"].join("\u{1f}");
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            "$1\u{1f}@1\u{1f}%1\u{1f}101\n",
        );
        mock.stub(&["switch-client", "-t", "$1:@1.%1"], "");

        jump_to_pane(&mock, "%1").unwrap();

        assert_eq!(mock.calls().len(), 2);
        assert_eq!(
            mock.calls()[1],
            vec![
                "switch-client".to_string(),
                "-t".to_string(),
                "$1:@1.%1".to_string(),
            ]
        );
    }

    #[test]
    fn jump_to_pane_for_client_never_switches_an_implicit_client() {
        let mock = MockTmuxRunner::new();
        let format = ["#{session_id}", "#{window_id}", "#{pane_id}", "#{pane_pid}"].join("\u{1f}");
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            "$1\u{1f}@1\u{1f}%1\u{1f}101\n",
        );
        mock.stub(
            &["list-clients", "-F", "#{client_pid}\u{1f}#{client_name}"],
            "10\u{1f}/dev/ttys001\n20\u{1f}/dev/ttys002\n",
        );
        let source = crate::pane_state::PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 909,
        };
        let target = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let source_guard = "#{&&:#{==:#{pane_id},%9},#{==:#{pane_pid},909}}";
        let switch = crate::pane_state::store::tmux_command_string(&[
            "switch-client".to_string(),
            "-c".to_string(),
            "/dev/ttys002".to_string(),
            "-t".to_string(),
            "$1:@1.%1".to_string(),
        ]);
        let mismatch = format!("display-message -p '{SOURCE_CLIENT_MISMATCH_SENTINEL}'");
        mock.stub(
            &[
                "if-shell",
                "-F",
                "-t",
                "/dev/ttys002",
                source_guard,
                &switch,
                &mismatch,
            ],
            "",
        );
        jump_to_pane_for_client(&mock, &target, 20, &source).unwrap();

        assert!(
            !mock
                .calls()
                .iter()
                .any(|call| { call.first().map(String::as_str) == Some("switch-client") })
        );
        assert_eq!(mock.calls().len(), 3);
    }

    #[test]
    fn jump_to_pane_for_named_client_guards_the_exact_target_instance() {
        let mock = MockTmuxRunner::new();
        let format = ["#{session_id}", "#{window_id}", "#{pane_id}", "#{pane_pid}"].join("\u{1f}");
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            "$1\u{1f}@1\u{1f}%1\u{1f}101\n",
        );
        let target = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let exact_target = "$1:@1.%1";
        let pane_guard = "#{==:#{pane_pid},101}";
        let switch = crate::pane_state::store::tmux_command_string(&[
            "switch-client".to_string(),
            "-c".to_string(),
            "client-1".to_string(),
            "-t".to_string(),
            exact_target.to_string(),
        ]);
        let mismatch = format!("display-message -p '{TARGET_PANE_MISMATCH_SENTINEL}'");
        mock.stub(
            &[
                "if-shell",
                "-F",
                "-t",
                exact_target,
                pane_guard,
                &switch,
                &mismatch,
            ],
            "",
        );

        jump_to_pane_for_named_client(&mock, &target, "client-1").unwrap();

        assert_eq!(mock.calls().len(), 2);
        assert!(mock.calls()[1][5].contains("switch-client"));
        assert!(mock.calls()[1][5].contains("client-1"));
        assert!(mock.calls()[1][5].contains(exact_target));
    }

    #[test]
    fn jump_to_pane_for_named_client_rejects_a_reused_pane_id_before_mutation() {
        let mock = MockTmuxRunner::new();
        let format = ["#{session_id}", "#{window_id}", "#{pane_id}", "#{pane_pid}"].join("\u{1f}");
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            "$1\u{1f}@1\u{1f}%1\u{1f}202\n",
        );
        let target = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };

        let error = jump_to_pane_for_named_client(&mock, &target, "client-1").unwrap_err();

        assert!(error.to_string().contains("pane not found: %1"));
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn jump_to_pane_for_client_rejects_reused_target_pane_id_before_mutation() {
        let mock = MockTmuxRunner::new();
        let format = ["#{session_id}", "#{window_id}", "#{pane_id}", "#{pane_pid}"].join("\u{1f}");
        mock.stub(
            &["list-panes", "-a", "-F", &format],
            "$1\u{1f}@1\u{1f}%1\u{1f}202\n",
        );
        let target = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let source = crate::pane_state::PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 909,
        };

        let error = jump_to_pane_for_client(&mock, &target, 20, &source).unwrap_err();

        assert!(error.to_string().contains("pane not found: %1"));
        assert_eq!(mock.calls().len(), 1);
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::SidebarWidth;
use crate::options::{KEY_LAYOUT_BASELINE, KEY_LAYOUT_PANES, KEY_SIDEBAR_MARKER};
use crate::tmux::TmuxRunner;

pub const SIDEBAR_PANE_FORMAT: &str = "#{pane_id}\t#{@vde_sidebar}\t#{pane_width}";
const RAIL_WIDTH: u16 = 2;
const AFTER_NEW_WINDOW_HOOK: &str = "after-new-window[90]";

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarPane {
    pane_id: String,
    width: u16,
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
    if find_sidebar_pane(runner, target)?.is_some() {
        return Ok(());
    }
    open_unchecked(runner, target, self_exe, width, min_width)
}

pub fn close(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return restore_or_clear_stale_baseline(runner, target);
    };
    close_sidebar_pane(runner, target, &sidebar)
}

fn close_sidebar_pane(runner: &dyn TmuxRunner, target: &str, sidebar: &SidebarPane) -> Result<()> {
    let options = runner.run(&["show-options", "-w", "-t", target])?;
    let (saved_layout, saved_panes) = parse_saved_baseline(&options);

    runner.run(&["kill-pane", "-t", &sidebar.pane_id])?;

    let current_panes = capture_pane_ids(runner, target)?;
    if let (Some(layout), Some(saved_panes)) = (saved_layout.as_deref(), saved_panes.as_ref())
        && saved_panes == &current_panes
        && !layout.contains(&sidebar.pane_id)
    {
        runner.run(&["select-layout", "-t", target, layout])?;
    }
    clear_baseline(runner, target)?;
    Ok(())
}

pub fn toggle(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    if let Some(sidebar) = find_sidebar_pane(runner, target)? {
        close_sidebar_pane(runner, target, &sidebar)
    } else {
        open_unchecked(runner, target, self_exe, width, min_width)
    }
}

pub fn toggle_all(
    runner: &dyn TmuxRunner,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
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
        uninstall_after_new_window_hook(runner)?;
    } else {
        for (window, sidebar) in sidebars {
            if sidebar.is_none() {
                open_unchecked(runner, &window, self_exe, width, min_width)?;
            }
        }
        install_after_new_window_hook(runner, self_exe, width)?;
    }
    Ok(())
}

/// 3状態トグル: 未表示なら開く、表示中でフォーカスが外れていれば
/// サイドバーへフォーカスを移す、フォーカス中なら閉じる。
pub fn focus_toggle(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return open_unchecked(runner, target, self_exe, width, min_width);
    };
    // window を target-pane に与えると、その window のアクティブ pane に解決される
    let active = runner
        .run(&["display-message", "-p", "-t", target, "#{pane_id}"])?
        .trim()
        .to_string();
    if active == sidebar.pane_id {
        close_sidebar_pane(runner, target, &sidebar)
    } else {
        runner.run(&["select-pane", "-t", &sidebar.pane_id])?;
        Ok(())
    }
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
        resolve_width(runner, target, normal_width, min_width)?
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
    let panes = crate::options::snapshot::read_all_panes(runner)?;
    let pane = panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .with_context(|| format!("pane not found: {pane_id}"))?;
    let target = crate::session::exact_session_target(&pane.session);
    runner.run(&["switch-client", "-t", &target])?;
    runner.run(&["select-window", "-t", &pane.window_id])?;
    runner.run(&["select-pane", "-t", &pane.pane_id])?;
    Ok(())
}

pub fn focus(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
    };
    runner.run(&["select-pane", "-t", &sidebar.pane_id])?;
    Ok(())
}

pub fn rebaseline(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
    };
    let layout = capture_window_layout(runner, target)?;
    let mut panes = capture_pane_ids(runner, target)?;
    panes.remove(&sidebar.pane_id);
    save_baseline(runner, target, &layout, &panes)?;
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
        if panes.len() == 1 && panes.contains(&sidebar.pane_id) {
            return close_lonely_sidebar_pane(runner, target, &sidebar);
        }
        return rebaseline(runner, target);
    }
    open_unchecked(runner, target, self_exe, width, min_width)
}

fn close_lonely_sidebar_pane(
    runner: &dyn TmuxRunner,
    target: &str,
    sidebar: &SidebarPane,
) -> Result<()> {
    clear_baseline(runner, target)?;
    runner.run(&["kill-pane", "-t", &sidebar.pane_id])?;
    Ok(())
}

fn open_unchecked(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    prepare_baseline_for_open(runner, target)?;
    let layout = capture_window_layout(runner, target)?;
    let panes = capture_pane_ids(runner, target)?;
    save_baseline(runner, target, &layout, &panes)?;
    let width = resolve_width(runner, target, width, min_width)?;
    let socket_name = std::env::var("VDE_TMUX_SOCKET_NAME")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let command = attach_shell_command(self_exe, socket_name.as_deref());
    runner.run(&[
        "split-window",
        "-t",
        target,
        "-hbf",
        "-l",
        &width.to_string(),
        &command,
    ])?;
    Ok(())
}

fn prepare_baseline_for_open(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let options = runner.run(&["show-options", "-w", "-t", target])?;
    let (saved_layout, saved_panes) = parse_saved_baseline(&options);
    if saved_layout.is_none() && saved_panes.is_none() {
        return Ok(());
    }

    restore_or_clear_saved_baseline(
        runner,
        target,
        saved_layout.as_deref(),
        saved_panes.as_ref(),
    )
}

fn restore_or_clear_stale_baseline(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let options = runner.run(&["show-options", "-w", "-t", target])?;
    let (saved_layout, saved_panes) = parse_saved_baseline(&options);
    if saved_layout.is_none() && saved_panes.is_none() {
        return Ok(());
    }
    restore_or_clear_saved_baseline(
        runner,
        target,
        saved_layout.as_deref(),
        saved_panes.as_ref(),
    )
}

fn restore_or_clear_saved_baseline(
    runner: &dyn TmuxRunner,
    target: &str,
    saved_layout: Option<&str>,
    saved_panes: Option<&BTreeSet<String>>,
) -> Result<()> {
    let current_panes = capture_pane_ids(runner, target)?;
    match (saved_layout, saved_panes) {
        (Some(layout), Some(saved_panes)) if saved_panes == &current_panes => {
            runner.run(&["select-layout", "-t", target, layout])?;
            clear_baseline(runner, target)
        }
        _ => clear_baseline(runner, target),
    }
}

fn resolve_width(
    runner: &dyn TmuxRunner,
    target: &str,
    width: SidebarWidth,
    min_width: u16,
) -> Result<u16> {
    match width {
        SidebarWidth::Columns(columns) => Ok(columns),
        SidebarWidth::Percent(percent) => {
            let output = runner.run(&[
                "display-message",
                "-p",
                "-t",
                target,
                "-F",
                "#{window_width}",
            ])?;
            let window_width = output
                .trim()
                .parse::<u32>()
                .with_context(|| format!("failed to parse window width for {target}"))?;
            let resolved = window_width.saturating_mul(percent as u32) / 100;
            Ok((resolved as u16).max(min_width))
        }
    }
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

fn save_baseline(
    runner: &dyn TmuxRunner,
    target: &str,
    layout: &str,
    panes: &BTreeSet<String>,
) -> Result<()> {
    if layout.is_empty() {
        bail!("window layout is empty for {target}");
    }
    let panes = panes.iter().cloned().collect::<Vec<_>>().join(",");
    runner.run(&[
        "set-option",
        "-w",
        "-t",
        target,
        KEY_LAYOUT_BASELINE,
        layout,
    ])?;
    runner.run(&["set-option", "-w", "-t", target, KEY_LAYOUT_PANES, &panes])?;
    Ok(())
}

fn clear_baseline(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    runner.run(&["set-option", "-w", "-u", "-t", target, KEY_LAYOUT_BASELINE])?;
    runner.run(&["set-option", "-w", "-u", "-t", target, KEY_LAYOUT_PANES])?;
    Ok(())
}

fn parse_saved_baseline(output: &str) -> (Option<String>, Option<BTreeSet<String>>) {
    let mut layout = None;
    let mut panes = None;
    for line in output.lines() {
        let Some((key, value)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key {
            KEY_LAYOUT_BASELINE => layout = Some(value.to_string()),
            KEY_LAYOUT_PANES => {
                panes = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(ToOwned::to_owned)
                        .collect(),
                );
            }
            _ => {}
        }
    }
    (layout, panes)
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

fn attach_shell_command(self_exe: &Path, socket_name: Option<&str>) -> String {
    let command = format!(
        "{} sidebar attach",
        shell_quote(&self_exe.display().to_string())
    );
    match socket_name.filter(|value| !value.trim().is_empty()) {
        Some(socket_name) => format!(
            "VDE_TMUX_SOCKET_NAME={} {command}",
            shell_quote(socket_name)
        ),
        None => command,
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

fn sidebar_width_arg(width: SidebarWidth) -> String {
    match width {
        SidebarWidth::Columns(columns) => columns.to_string(),
        SidebarWidth::Percent(percent) => format!("{percent}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{KEY_LAYOUT_BASELINE, KEY_LAYOUT_PANES, KEY_SIDEBAR_MARKER};
    use crate::tmux::mock::MockTmuxRunner;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn exe() -> PathBuf {
        PathBuf::from("/tmp/vt")
    }

    #[test]
    fn attach_shell_command_propagates_tmux_socket_name() {
        assert_eq!(
            attach_shell_command(&exe(), Some("scratch")),
            "VDE_TMUX_SOCKET_NAME='scratch' '/tmp/vt' sidebar attach"
        );
        assert_eq!(
            attach_shell_command(&exe(), None),
            "'/tmp/vt' sidebar attach"
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
    fn open_saves_baseline_and_splits_sidebar_pane() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(&["show-options", "-w", "-t", "@1"], "");
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
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n%2\n");
        mock.stub(
            &[
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-before",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1,%2"],
            "",
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

        assert_eq!(mock.calls().len(), 7);
    }

    #[test]
    fn open_resolves_percent_width_from_window_width() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t640\n",
        );
        mock.stub(&["show-options", "-w", "-t", "@1"], "");
        mock.stub(
            &["display-message", "-p", "-t", "@1", "-F", "#{window_width}"],
            "640\n",
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
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
        mock.stub(
            &[
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-before",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1"],
            "",
        );
        mock.stub(
            &[
                "split-window",
                "-t",
                "@1",
                "-hbf",
                "-l",
                "64",
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

        assert_eq!(mock.calls().len(), 8);
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
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t1\t40\n",
        );
        mock.stub(&["display-message", "-p", "-t", "@1", "#{pane_id}"], "%2\n");
        mock.stub(&["show-options", "-w", "-t", "@1"], "");
        mock.stub(&["kill-pane", "-t", "%2"], "");
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_BASELINE],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_PANES],
            "",
        );

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
    fn focus_toggle_opens_sidebar_when_missing() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(&["show-options", "-w", "-t", "@1"], "");
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
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n%2\n");
        mock.stub(
            &[
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-before",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1,%2"],
            "",
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

        assert_eq!(mock.calls().len(), 7);
    }

    #[test]
    fn open_restores_matching_stale_baseline_before_saving_new_baseline() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t\t80\n",
        );
        mock.stub(
            &["show-options", "-w", "-t", "@1"],
            "@vde_layout_baseline \"layout-before\"\n@vde_layout_panes \"%1,%2\"\n",
        );
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n%2\n");
        mock.stub(&["select-layout", "-t", "@1", "layout-before"], "");
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_BASELINE],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_PANES],
            "",
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
            "layout-restored\n",
        );
        mock.stub(
            &[
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-restored",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1,%2"],
            "",
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
        assert!(calls.contains(&vec![
            "select-layout".to_string(),
            "-t".to_string(),
            "@1".to_string(),
            "layout-before".to_string(),
        ]));
        let select_index = calls
            .iter()
            .position(|call| call.first().map(String::as_str) == Some("select-layout"))
            .unwrap();
        let save_index = calls
            .iter()
            .position(|call| {
                call == &vec![
                    "set-option".to_string(),
                    "-w".to_string(),
                    "-t".to_string(),
                    "@1".to_string(),
                    KEY_LAYOUT_BASELINE.to_string(),
                    "layout-restored".to_string(),
                ]
            })
            .unwrap();
        assert!(select_index < save_index);
    }

    #[test]
    fn open_clears_mismatched_stale_baseline_before_saving_current_layout() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t\t80\n",
        );
        mock.stub(
            &["show-options", "-w", "-t", "@1"],
            "@vde_layout_baseline \"layout-before\"\n@vde_layout_panes \"%1\"\n",
        );
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n%2\n");
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_BASELINE],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_PANES],
            "",
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
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-current",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1,%2"],
            "",
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
        assert!(calls.contains(&vec![
            "set-option".to_string(),
            "-w".to_string(),
            "-u".to_string(),
            "-t".to_string(),
            "@1".to_string(),
            KEY_LAYOUT_BASELINE.to_string(),
        ]));
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("select-layout"))
        );
    }

    #[test]
    fn percent_width_is_clamped_to_min_width() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["display-message", "-p", "-t", "@1", "-F", "#{window_width}"],
            "320\n",
        );

        assert_eq!(
            resolve_width(&mock, "@1", crate::config::SidebarWidth::Percent(10), 40).unwrap(),
            40
        );
    }

    #[test]
    fn fixed_width_is_not_clamped_to_min_width() {
        let mock = MockTmuxRunner::new();

        assert_eq!(
            resolve_width(&mock, "@1", crate::config::SidebarWidth::Columns(20), 40).unwrap(),
            20
        );
    }

    #[test]
    fn close_restores_saved_layout_when_panes_match() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n%1\t\t80\n%2\t\t80\n",
        );
        mock.stub(
            &["show-options", "-w", "-t", "@1"],
            "@vde_layout_baseline \"layout-before\"\n@vde_layout_panes \"%1,%2\"\n",
        );
        mock.stub(&["kill-pane", "-t", "%9"], "");
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n%2\n");
        mock.stub(&["select-layout", "-t", "@1", "layout-before"], "");
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_BASELINE],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_PANES],
            "",
        );

        close(&mock, "@1").unwrap();

        assert_eq!(mock.calls().len(), 7);
    }

    #[test]
    fn close_restores_stale_baseline_when_sidebar_pane_already_gone() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%2\t\t80\n",
        );
        mock.stub(
            &["show-options", "-w", "-t", "@1"],
            "@vde_layout_baseline \"layout-before\"\n@vde_layout_panes \"%1,%2\"\n",
        );
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n%2\n");
        mock.stub(&["select-layout", "-t", "@1", "layout-before"], "");
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_BASELINE],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_PANES],
            "",
        );

        close(&mock, "@1").unwrap();

        assert_eq!(mock.calls().len(), 6);
    }

    #[test]
    fn rail_toggles_sidebar_width() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n",
        );
        mock.stub(&["resize-pane", "-t", "%9", "-x", "2"], "");

        rail(&mock, "@1", SidebarWidth::Columns(40), 40).unwrap();

        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn rail_resolves_percent_width_when_restoring_normal_width() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t2\n",
        );
        mock.stub(
            &["display-message", "-p", "-t", "@1", "-F", "#{window_width}"],
            "640\n",
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
        mock.stub(&["show-options", "-w", "-t", "@1"], "");
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
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
        mock.stub(
            &[
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-one",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1"],
            "",
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

        mock.stub(
            &["list-panes", "-t", "@2", "-F", SIDEBAR_PANE_FORMAT],
            "%2\t\t80\n",
        );
        mock.stub(&["show-options", "-w", "-t", "@2"], "");
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
        mock.stub(&["list-panes", "-t", "@2", "-F", "#{pane_id}"], "%2\n");
        mock.stub(
            &[
                "set-option",
                "-w",
                "-t",
                "@2",
                KEY_LAYOUT_BASELINE,
                "layout-two",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@2", KEY_LAYOUT_PANES, "%2"],
            "",
        );
        mock.stub(
            &[
                "split-window",
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

        toggle_all(&mock, &exe(), SidebarWidth::Columns(40), 40).unwrap();

        assert_eq!(mock.calls().len(), 16);
    }

    #[test]
    fn toggle_all_opens_missing_windows_without_closing_existing_sidebars() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-windows", "-a", "-F", "#{window_id}"], "@1\n@2\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(&["show-options", "-w", "-t", "@1"], "");
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
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
        mock.stub(
            &[
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-one",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1"],
            "",
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
    }

    #[test]
    fn toggle_all_closes_all_sidebars_and_disables_new_window_hook_when_all_windows_are_open() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-windows", "-a", "-F", "#{window_id}"], "@1\n@2\n");
        for (window, sidebar, pane) in [("@1", "%9", "%1"), ("@2", "%8", "%2")] {
            mock.stub(
                &["list-panes", "-t", window, "-F", SIDEBAR_PANE_FORMAT],
                &format!("{sidebar}\t1\t40\n{pane}\t\t80\n"),
            );
            mock.stub(&["show-options", "-w", "-t", window], "");
            mock.stub(&["kill-pane", "-t", sidebar], "");
            mock.stub(
                &["list-panes", "-t", window, "-F", "#{pane_id}"],
                &format!("{pane}\n"),
            );
            mock.stub(
                &["set-option", "-w", "-u", "-t", window, KEY_LAYOUT_BASELINE],
                "",
            );
            mock.stub(
                &["set-option", "-w", "-u", "-t", window, KEY_LAYOUT_PANES],
                "",
            );
        }
        mock.stub(&["set-hook", "-gu", AFTER_NEW_WINDOW_HOOK], "");

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
    }

    #[test]
    fn layout_applied_opens_when_sidebar_is_absent() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n",
        );
        mock.stub(&["show-options", "-w", "-t", "@1"], "");
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
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-before",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1"],
            "",
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

        layout_applied(&mock, "@1", &exe(), SidebarWidth::Columns(32), 40).unwrap();

        assert_eq!(mock.calls().len(), 8);
    }

    #[test]
    fn layout_applied_closes_window_when_only_sidebar_remains() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%9\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_BASELINE],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_PANES],
            "",
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
    fn layout_applied_rebaselines_when_non_sidebar_pane_remains() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n%9\n");
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%1\t\t80\n%9\t1\t40\n",
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
            "layout-with-sidebar\n",
        );
        mock.stub(
            &[
                "set-option",
                "-w",
                "-t",
                "@1",
                KEY_LAYOUT_BASELINE,
                "layout-with-sidebar",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1"],
            "",
        );

        layout_applied(&mock, "@1", &exe(), SidebarWidth::Columns(32), 40).unwrap();

        let calls = mock.calls();
        assert!(
            !calls
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("kill-pane")),
            "{calls:?}"
        );
        assert!(calls.contains(&vec![
            "set-option".to_string(),
            "-w".to_string(),
            "-t".to_string(),
            "@1".to_string(),
            KEY_LAYOUT_PANES.to_string(),
            "%1".to_string(),
        ]));
    }

    #[test]
    fn toggle_closes_when_sidebar_exists() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n%1\t\t80\n",
        );
        mock.stub(&["show-options", "-w", "-t", "@1"], "");
        mock.stub(&["kill-pane", "-t", "%9"], "");
        mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_BASELINE],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@1", KEY_LAYOUT_PANES],
            "",
        );

        toggle(&mock, "@1", &exe(), SidebarWidth::Columns(32), 40).unwrap();

        assert_eq!(mock.calls().len(), 6);
    }

    #[test]
    fn jump_to_pane_switches_session_window_and_pane() {
        let mock = MockTmuxRunner::new();
        let line = [
            "main",
            "@1",
            "%1",
            "/tmp",
            "zsh",
            "/dev/ttys001",
            "123",
            "0",
            "0",
            "",
            "codex",
            "running",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]
        .join("\u{1f}");
        mock.stub(
            &[
                "list-panes",
                "-a",
                "-F",
                crate::options::snapshot::snapshot_format().as_str(),
            ],
            &format!("{line}\n"),
        );
        mock.stub(&["switch-client", "-t", "=main:"], "");
        mock.stub(&["select-window", "-t", "@1"], "");
        mock.stub(&["select-pane", "-t", "%1"], "");

        jump_to_pane(&mock, "%1").unwrap();

        assert_eq!(mock.calls().len(), 4);
    }
}

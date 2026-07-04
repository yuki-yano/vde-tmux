use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::options::{KEY_LAYOUT_BASELINE, KEY_LAYOUT_PANES, KEY_SIDEBAR_MARKER};
use crate::tmux::TmuxRunner;

pub const SIDEBAR_PANE_FORMAT: &str = "#{pane_id}\t#{@vde_sidebar}\t#{pane_width}";
const RAIL_WIDTH: u16 = 2;

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

pub fn open(runner: &dyn TmuxRunner, target: &str, self_exe: &Path, width: u16) -> Result<()> {
    if find_sidebar_pane(runner, target)?.is_some() {
        return Ok(());
    }
    open_unchecked(runner, target, self_exe, width)
}

pub fn close(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
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

pub fn toggle(runner: &dyn TmuxRunner, target: &str, self_exe: &Path, width: u16) -> Result<()> {
    if let Some(sidebar) = find_sidebar_pane(runner, target)? {
        close_sidebar_pane(runner, target, &sidebar)
    } else {
        open_unchecked(runner, target, self_exe, width)
    }
}

pub fn toggle_all(runner: &dyn TmuxRunner, self_exe: &Path, width: u16) -> Result<()> {
    for window in list_window_ids(runner)? {
        toggle(runner, &window, self_exe, width)?;
    }
    Ok(())
}

pub fn rail(runner: &dyn TmuxRunner, target: &str, normal_width: u16) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
    };
    let next_width = if sidebar.width <= RAIL_WIDTH {
        normal_width
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
    runner.run(&["switch-client", "-t", &pane.session])?;
    runner.run(&["select-window", "-t", &pane.window_id])?;
    runner.run(&["select-pane", "-t", &pane.pane_id])?;
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
    width: u16,
) -> Result<()> {
    if !window_exists(runner, target)? {
        return Ok(());
    }
    if find_sidebar_pane(runner, target)?.is_some() {
        return rebaseline(runner, target);
    }
    open_unchecked(runner, target, self_exe, width)
}

fn open_unchecked(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: u16,
) -> Result<()> {
    let layout = capture_window_layout(runner, target)?;
    let panes = capture_pane_ids(runner, target)?;
    save_baseline(runner, target, &layout, &panes)?;
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

fn window_exists(runner: &dyn TmuxRunner, target: &str) -> Result<bool> {
    match runner.run(&["list-panes", "-t", target, "-F", "#{pane_id}"]) {
        Ok(output) => Ok(!output.trim().is_empty()),
        Err(_) => Ok(false),
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

        open(&mock, "@1", &exe(), 40).unwrap();

        assert_eq!(mock.calls().len(), 6);
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
    fn rail_toggles_sidebar_width() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
            "%9\t1\t40\n",
        );
        mock.stub(&["resize-pane", "-t", "%9", "-x", "2"], "");

        rail(&mock, "@1", 40).unwrap();

        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn toggle_all_applies_to_each_window() {
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
        mock.stub(&["show-options", "-w", "-t", "@2"], "");
        mock.stub(&["kill-pane", "-t", "%9"], "");
        mock.stub(&["list-panes", "-t", "@2", "-F", "#{pane_id}"], "%2\n");
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@2", KEY_LAYOUT_BASELINE],
            "",
        );
        mock.stub(
            &["set-option", "-w", "-u", "-t", "@2", KEY_LAYOUT_PANES],
            "",
        );

        toggle_all(&mock, &exe(), 40).unwrap();

        assert_eq!(mock.calls().len(), 13);
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

        layout_applied(&mock, "@1", &exe(), 32).unwrap();

        assert_eq!(mock.calls().len(), 7);
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

        toggle(&mock, "@1", &exe(), 32).unwrap();

        assert_eq!(mock.calls().len(), 6);
    }

    #[test]
    fn jump_to_pane_switches_session_window_and_pane() {
        let mock = MockTmuxRunner::new();
        let line = [
            "main", "@1", "%1", "/tmp", "zsh", "0", "0", "", "codex", "running", "", "", "", "",
            "", "", "", "",
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
        mock.stub(&["switch-client", "-t", "main"], "");
        mock.stub(&["select-window", "-t", "@1"], "");
        mock.stub(&["select-pane", "-t", "%1"], "");

        jump_to_pane(&mock, "%1").unwrap();

        assert_eq!(mock.calls().len(), 4);
    }
}

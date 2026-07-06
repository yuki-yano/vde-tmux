use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::tmux::TmuxRunner;

pub const LESS_ESCAPE_QUIT_LESSKEY_SRC: &str = "#command\n\\e quit\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewCommand {
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewGeometry {
    pub width: u16,
    pub x: u16,
    pub height: String,
    pub y: String,
}

impl PreviewGeometry {
    pub fn new(window_width: u16, _window_height: u16, target_width: u16) -> Self {
        let width = target_width.max(1).min(window_width.max(1));
        Self {
            width,
            x: window_width.saturating_sub(width) / 2,
            height: "80%".to_string(),
            y: "10%".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewTarget {
    window_id: String,
    geometry: PreviewGeometry,
}

pub fn open_preview_floating_pane(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    pane_id: &str,
    history_lines: u32,
) -> Result<()> {
    let target = resolve_preview_target(runner, env, pane_id)?;
    kill_existing_preview_panes(runner, &target.window_id)?;
    runner.run(&["set-option", "-s", "focus-events", "on"])?;
    let less_keyfile = write_less_escape_keyfile(env);
    let command = build_preview_command(
        pane_id,
        &target.window_id,
        target.geometry,
        history_lines,
        less_keyfile.as_deref(),
    );
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

fn resolve_preview_target(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    pane_id: &str,
) -> Result<PreviewTarget> {
    let source_pane = env
        .get("TMUX_PANE")
        .filter(|value| !value.trim().is_empty())
        .map(String::as_str)
        .unwrap_or(pane_id);
    let source_format = "#{window_id}\u{1f}#{window_width}\u{1f}#{window_height}";
    let source_output = runner.run(&[
        "display-message",
        "-p",
        "-t",
        source_pane,
        "-F",
        source_format,
    ])?;
    let source_fields = source_output.trim().split('\u{1f}').collect::<Vec<_>>();
    if source_fields.len() != 3 {
        anyhow::bail!("failed to resolve preview source geometry for {source_pane}");
    }
    let pane_width_output = runner.run(&[
        "display-message",
        "-p",
        "-t",
        pane_id,
        "-F",
        "#{pane_width}",
    ])?;
    let pane_width = pane_width_output
        .trim()
        .parse::<u16>()
        .context("invalid pane_width")?;
    let window_width = source_fields[1]
        .parse::<u16>()
        .context("invalid window_width")?;
    let window_height = source_fields[2]
        .parse::<u16>()
        .context("invalid window_height")?;
    Ok(PreviewTarget {
        window_id: source_fields[0].to_string(),
        geometry: PreviewGeometry::new(window_width, window_height, pane_width),
    })
}

pub fn build_preview_command(
    pane_id: &str,
    window_id: &str,
    geometry: PreviewGeometry,
    history_lines: u32,
    less_keyfile: Option<&Path>,
) -> PreviewCommand {
    PreviewCommand {
        args: vec![
            "new-pane".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
            "-x".to_string(),
            geometry.width.to_string(),
            "-y".to_string(),
            geometry.height,
            "-X".to_string(),
            geometry.x.to_string(),
            "-Y".to_string(),
            geometry.y,
            "-t".to_string(),
            window_id.to_string(),
            build_preview_inner_command(pane_id, history_lines, less_keyfile),
        ],
    }
}

fn build_preview_inner_command(
    pane_id: &str,
    history_lines: u32,
    less_keyfile: Option<&Path>,
) -> String {
    let target = shell_quote(pane_id);
    let capture = format!(
        "{{ tmux capture-pane -a -p -e -S -{history_lines} -t {target} 2>/dev/null || tmux capture-pane -p -e -S -{history_lines} -t {target}; }}"
    );
    match less_keyfile {
        Some(path) => format!(
            "{capture} | LESSCHARSET=utf-8 LESSKEYIN={} less -R +G",
            shell_quote(&path.display().to_string())
        ),
        None => format!("{capture} | LESSCHARSET=utf-8 less -R +G"),
    }
}

fn less_escape_keyfile_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let base = env
        .get("XDG_CACHE_HOME")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .filter(|value| !value.trim().is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(base.join("vde").join("tmux").join("preview.lesskey"))
}

fn write_less_escape_keyfile(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let path = less_escape_keyfile_path(env)?;
    let parent = path.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    std::fs::write(&path, LESS_ESCAPE_QUIT_LESSKEY_SRC).ok()?;
    Some(path)
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

pub fn shell_quote(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    #[test]
    fn preview_geometry_uses_target_pane_width_centered_in_window() {
        let geometry = PreviewGeometry::new(100, 40, 64);

        assert_eq!(geometry.width, 64);
        assert_eq!(geometry.x, 18);
        assert_eq!(geometry.height, "80%");
        assert_eq!(geometry.y, "10%");
    }

    #[test]
    fn preview_command_captures_scrollback_and_starts_less_at_bottom() {
        let command = build_preview_command(
            "%26",
            "@1",
            PreviewGeometry::new(100, 40, 64),
            2000,
            Some(std::path::Path::new("/tmp/preview.lesskey")),
        );
        let inner = command.args.last().unwrap();

        assert!(inner.contains("capture-pane -a -p -e -S -2000 -t '%26'"));
        assert!(inner.contains("LESSCHARSET=utf-8"));
        assert!(inner.contains("LESSKEYIN='/tmp/preview.lesskey' less -R +G"));
    }

    #[test]
    fn preview_floating_pane_opens_in_current_tmux_pane_window() {
        let runner = MockTmuxRunner::new();
        let mut env = BTreeMap::new();
        env.insert("TMUX_PANE".to_string(), "%99".to_string());
        let source_format = "#{window_id}\u{1f}#{window_width}\u{1f}#{window_height}";
        runner.stub(
            &["display-message", "-p", "-t", "%99", "-F", source_format],
            "@1\u{1f}120\u{1f}40\n",
        );
        runner.stub(
            &["display-message", "-p", "-t", "%42", "-F", "#{pane_width}"],
            "72\n",
        );
        runner.stub(
            &["list-panes", "-t", "@1", "-F", "#{pane_id} #{@vde_preview}"],
            "",
        );
        runner.stub(&["set-option", "-s", "focus-events", "on"], "");
        let command =
            build_preview_command("%42", "@1", PreviewGeometry::new(120, 40, 72), 2000, None);
        let command_args = command.args.iter().map(String::as_str).collect::<Vec<_>>();
        runner.stub(&command_args, "%77\n");
        runner.stub(&["set-option", "-p", "-t", "%77", "@vde_preview", "1"], "");
        runner.stub(
            &["set-option", "-p", "-t", "%77", "pane-border-status", "off"],
            "",
        );
        runner.stub(
            &[
                "set-hook",
                "-p",
                "-t",
                "%77",
                "pane-focus-out",
                "kill-pane -t '%77'",
            ],
            "",
        );

        open_preview_floating_pane(&runner, &env, "%42", 2000).unwrap();

        let calls = runner.calls();
        let new_pane = calls
            .iter()
            .find(|call| call.first().map(String::as_str) == Some("new-pane"))
            .expect("new-pane call");
        assert!(new_pane.windows(2).any(|window| window == ["-t", "@1"]));
        assert!(new_pane.windows(2).any(|window| window == ["-x", "72"]));
        assert!(new_pane.windows(2).any(|window| window == ["-X", "24"]));
        let inner = new_pane.last().unwrap();
        assert!(inner.contains("capture-pane -a -p -e -S -2000 -t '%42'"));
        assert!(inner.contains("LESSCHARSET=utf-8 less -R +G"));
    }
}

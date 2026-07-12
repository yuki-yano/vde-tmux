use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::pane_state::PaneInstance;
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
    pane: &PaneInstance,
    history_lines: u32,
) -> Result<()> {
    let target = resolve_preview_target(runner, env, pane)?;
    kill_existing_preview_panes(runner, &target.window_id)?;
    runner.run(&["set-option", "-s", "focus-events", "on"])?;
    let less_keyfile = write_less_escape_keyfile(env);
    let command = build_preview_command(
        pane,
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
    pane: &PaneInstance,
) -> Result<PreviewTarget> {
    let source_pane = env
        .get("TMUX_PANE")
        .filter(|value| !value.trim().is_empty())
        .map(String::as_str)
        .unwrap_or(&pane.pane_id);
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
    let pane_format = "#{pane_pid}\u{1f}#{pane_width}";
    let pane_output = runner.run(&[
        "display-message",
        "-p",
        "-t",
        &pane.pane_id,
        "-F",
        pane_format,
    ])?;
    let (pane_pid, pane_width) = pane_output
        .trim()
        .split_once('\u{1f}')
        .context("invalid preview pane geometry")?;
    let pane_pid = pane_pid.parse::<u32>().context("invalid pane_pid")?;
    if pane_pid != pane.pane_pid {
        anyhow::bail!("pane instance changed: {}", pane.pane_id);
    }
    let pane_width = pane_width.parse::<u16>().context("invalid pane_width")?;
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
    pane: &PaneInstance,
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
            build_preview_inner_command(pane, history_lines, less_keyfile),
        ],
    }
}

fn build_preview_inner_command(
    pane: &PaneInstance,
    history_lines: u32,
    less_keyfile: Option<&Path>,
) -> String {
    let history_start = format!("-{history_lines}");
    let primary = guarded_capture_pane_args(pane, &["-a", "-p", "-e", "-S", &history_start]);
    let fallback = guarded_capture_pane_args(pane, &["-p", "-e", "-S", &history_start]);
    let capture = format!(
        "{{ {} 2>/dev/null || {}; }}",
        tmux_shell_command(&primary),
        tmux_shell_command(&fallback)
    );
    match less_keyfile {
        Some(path) => format!(
            "{capture} | LESSCHARSET=utf-8 LESSKEYIN={} less -R +G",
            shell_quote(&path.display().to_string())
        ),
        None => format!("{capture} | LESSCHARSET=utf-8 less -R +G"),
    }
}

pub(crate) fn guarded_capture_pane_args(
    pane: &PaneInstance,
    capture_options: &[&str],
) -> Vec<String> {
    let mut capture = vec!["capture-pane".to_string()];
    capture.extend(capture_options.iter().map(|option| (*option).to_string()));
    capture.push("-t".to_string());
    capture.push(pane.pane_id.clone());
    vec![
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid),
        crate::pane_state::store::tmux_command_string(&capture),
        "run-shell true".to_string(),
    ]
}

fn tmux_shell_command(args: &[String]) -> String {
    std::iter::once("tmux".to_string())
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
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

    fn pane(pane_id: &str, pane_pid: u32) -> PaneInstance {
        PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        }
    }

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
        let pane = pane("%26", 2600);
        let command = build_preview_command(
            &pane,
            "@1",
            PreviewGeometry::new(100, 40, 64),
            2000,
            Some(std::path::Path::new("/tmp/preview.lesskey")),
        );
        let inner = command.args.last().unwrap();

        assert!(inner.contains("#{==:#{pane_pid},2600}"));
        assert!(inner.contains("capture-pane"));
        assert!(inner.contains("-2000"));
        assert!(inner.contains("%26"));
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
            &[
                "display-message",
                "-p",
                "-t",
                "%42",
                "-F",
                "#{pane_pid}\u{1f}#{pane_width}",
            ],
            "4200\u{1f}72\n",
        );
        runner.stub(
            &["list-panes", "-t", "@1", "-F", "#{pane_id} #{@vde_preview}"],
            "",
        );
        runner.stub(&["set-option", "-s", "focus-events", "on"], "");
        let pane = pane("%42", 4200);
        let command =
            build_preview_command(&pane, "@1", PreviewGeometry::new(120, 40, 72), 2000, None);
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

        open_preview_floating_pane(&runner, &env, &pane, 2000).unwrap();

        let calls = runner.calls();
        let new_pane = calls
            .iter()
            .find(|call| call.first().map(String::as_str) == Some("new-pane"))
            .expect("new-pane call");
        assert!(new_pane.windows(2).any(|window| window == ["-t", "@1"]));
        assert!(new_pane.windows(2).any(|window| window == ["-x", "72"]));
        assert!(new_pane.windows(2).any(|window| window == ["-X", "24"]));
        let inner = new_pane.last().unwrap();
        assert!(inner.contains("#{==:#{pane_pid},4200}"));
        assert!(inner.contains("capture-pane"));
        assert!(inner.contains("-2000"));
        assert!(inner.contains("%42"));
        assert!(inner.contains("LESSCHARSET=utf-8 less -R +G"));
    }

    #[test]
    fn preview_rejects_reused_pane_id_before_opening_floating_pane() {
        let runner = MockTmuxRunner::new();
        let mut env = BTreeMap::new();
        env.insert("TMUX_PANE".to_string(), "%99".to_string());
        let source_format = "#{window_id}\u{1f}#{window_width}\u{1f}#{window_height}";
        runner.stub(
            &["display-message", "-p", "-t", "%99", "-F", source_format],
            "@1\u{1f}120\u{1f}40\n",
        );
        runner.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "%42",
                "-F",
                "#{pane_pid}\u{1f}#{pane_width}",
            ],
            "4201\u{1f}72\n",
        );

        let error =
            open_preview_floating_pane(&runner, &env, &pane("%42", 4200), 2000).unwrap_err();

        assert!(error.to_string().contains("pane instance changed"));
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.first().map(String::as_str) == Some("new-pane"))
        );
    }
}

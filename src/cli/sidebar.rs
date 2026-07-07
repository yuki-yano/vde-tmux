use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::config::SidebarWidth;
use crate::tmux::TmuxRunner;

#[derive(Debug, Subcommand)]
pub(crate) enum SidebarCommand {
    Attach {
        #[arg(long, hide = true)]
        once: bool,
    },
    Input {
        key: String,
    },
    Open {
        #[arg(long)]
        window: Option<String>,
        #[arg(long, value_parser = parse_sidebar_width)]
        width: Option<SidebarWidth>,
        #[arg(long = "delay-ms")]
        delay_ms: Option<u64>,
    },
    Toggle {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        window: Option<String>,
        #[arg(long, value_parser = parse_sidebar_width)]
        width: Option<SidebarWidth>,
    },
    #[command(name = "focus-toggle")]
    FocusToggle {
        #[arg(long)]
        window: Option<String>,
        #[arg(long, value_parser = parse_sidebar_width)]
        width: Option<SidebarWidth>,
    },
    Close {
        #[arg(long)]
        window: Option<String>,
    },
    Rail {
        #[arg(long)]
        window: Option<String>,
        #[arg(long, value_parser = parse_sidebar_width)]
        width: Option<SidebarWidth>,
    },
    Rebaseline {
        #[arg(long)]
        window: Option<String>,
    },
    #[command(name = "layout-applied")]
    LayoutApplied {
        #[arg(long)]
        window: Option<String>,
        #[arg(long, value_parser = parse_sidebar_width)]
        width: Option<SidebarWidth>,
    },
    #[command(name = "layout-changed")]
    LayoutChanged {
        #[arg(long)]
        window: Option<String>,
    },
    Jump {
        pane: String,
    },
    Focus {
        #[arg(long)]
        window: Option<String>,
    },
}

pub(crate) fn run_sidebar_command(
    command: SidebarCommand,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<Option<String>> {
    run_sidebar_command_with_ensure(command, runner, env, config, ensure_sidebar_daemon_started)
}

pub(crate) fn run_sidebar_command_with_ensure<F>(
    command: SidebarCommand,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
    ensure_daemon: F,
) -> Result<Option<String>>
where
    F: Fn(&BTreeMap<String, String>) -> Result<()>,
{
    match command {
        SidebarCommand::Attach { once } => {
            ensure_daemon(env)?;
            crate::sidebar::layout::attach(runner, env)?;
            if once {
                return crate::sidebar::once::render_once(runner, env, config).map(Some);
            }
            crate::sidebar::tui::run_live_tui(env, config)
        }
        SidebarCommand::Input { key } => {
            crate::sidebar::client::send_sidebar_key(
                &crate::sidebar::client::socket_path(env),
                &key,
            )?;
            Ok(None)
        }
        SidebarCommand::Open {
            window,
            width,
            delay_ms,
        } => {
            ensure_daemon(env)?;
            if let Some(delay_ms) = delay_ms.filter(|value| *value > 0) {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::open(
                runner,
                &target,
                &std::env::current_exe()?,
                width.unwrap_or(config.sidebar.width),
                config.sidebar.min_width,
            )?;
            Ok(None)
        }
        SidebarCommand::Toggle { all, window, width } => {
            ensure_daemon(env)?;
            if all && window.is_some() {
                bail!("--all and --window cannot be used together");
            }
            if all {
                crate::sidebar::layout::toggle_all(
                    runner,
                    &std::env::current_exe()?,
                    width.unwrap_or(config.sidebar.width),
                    config.sidebar.min_width,
                )?;
            } else {
                let target = resolve_window_target(runner, window)?;
                crate::sidebar::layout::toggle(
                    runner,
                    &target,
                    &std::env::current_exe()?,
                    width.unwrap_or(config.sidebar.width),
                    config.sidebar.min_width,
                )?;
            }
            Ok(None)
        }
        SidebarCommand::FocusToggle { window, width } => {
            ensure_daemon(env)?;
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::focus_toggle(
                runner,
                &target,
                &std::env::current_exe()?,
                width.unwrap_or(config.sidebar.width),
                config.sidebar.min_width,
            )?;
            Ok(None)
        }
        SidebarCommand::Close { window } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::close(runner, &target)?;
            Ok(None)
        }
        SidebarCommand::Rail { window, width } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::rail(
                runner,
                &target,
                width.unwrap_or(config.sidebar.width),
                config.sidebar.min_width,
            )?;
            Ok(None)
        }
        SidebarCommand::Rebaseline { window } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::rebaseline(runner, &target)?;
            Ok(None)
        }
        SidebarCommand::LayoutApplied { window, width } => {
            ensure_daemon(env)?;
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::layout_applied(
                runner,
                &target,
                &std::env::current_exe()?,
                width.unwrap_or(config.sidebar.width),
                config.sidebar.min_width,
            )?;
            Ok(None)
        }
        SidebarCommand::LayoutChanged { window } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::layout_changed(runner, &target)?;
            Ok(None)
        }
        SidebarCommand::Jump { pane } => {
            crate::sidebar::client::send_sidebar_jump(
                &crate::sidebar::client::socket_path(env),
                &pane,
            )?;
            Ok(None)
        }
        SidebarCommand::Focus { window } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::focus(runner, &target)?;
            Ok(None)
        }
    }
}

fn ensure_sidebar_daemon_started(env: &BTreeMap<String, String>) -> Result<()> {
    crate::daemon::lifecycle::ensure_daemon_started(env, None)
}

fn parse_sidebar_width(value: &str) -> std::result::Result<SidebarWidth, String> {
    value.parse()
}

fn resolve_window_target(runner: &dyn TmuxRunner, window: Option<String>) -> Result<String> {
    if let Some(window) = window.filter(|value| !value.trim().is_empty()) {
        return Ok(window);
    }
    let output = runner
        .run(&["display-message", "-p", "#{window_id}"])?
        .trim()
        .to_string();
    if output.is_empty() {
        bail!("failed to resolve current window");
    }
    Ok(output)
}

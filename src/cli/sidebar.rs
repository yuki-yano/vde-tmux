use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use crate::config::SidebarWidth;
use crate::tmux::TmuxRunner;

const SELECTION_CONTEXT_FORMAT: &str = "#{pane_id}\u{1f}#{session_name}";

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
    F: Fn(&dyn TmuxRunner, &BTreeMap<String, String>) -> Result<(String, std::path::PathBuf)>,
{
    match command {
        SidebarCommand::Attach { once } => {
            let (server_identity, socket) = ensure_daemon(runner, env)
                .context("failed to ensure the pane-state daemon for sidebar attach")?;
            crate::sidebar::layout::attach(runner, env)
                .context("failed to mark the sidebar pane")?;
            try_seed_sidebar_selection_from_env(env, &socket, &server_identity);
            if once {
                return crate::sidebar::once::render_once(&socket, &server_identity, config)
                    .context("failed to query the one-shot sidebar snapshot")
                    .map(Some);
            }
            crate::sidebar::tui::run_live_tui(env, config, &socket, &server_identity)
        }
        SidebarCommand::Input { key } => {
            let (server_identity, socket) = ensure_daemon(runner, env)?;
            crate::sidebar::client::send_sidebar_key_v2(&socket, &server_identity, &key)?;
            Ok(None)
        }
        SidebarCommand::Open {
            window,
            width,
            delay_ms,
        } => {
            let (server_identity, socket) = ensure_daemon(runner, env)?;
            if let Some(delay_ms) = delay_ms.filter(|value| *value > 0) {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            let target = resolve_window_target(runner, window)?;
            let selection_context = resolve_selection_context(runner, env).ok();
            try_seed_sidebar_selection(selection_context.as_ref(), &socket, &server_identity);
            let attach_context = selection_context.as_ref().and_then(to_attach_context);
            crate::sidebar::layout::open_with_attach_context(
                runner,
                &target,
                &std::env::current_exe()?,
                width.unwrap_or(config.sidebar.width),
                config.sidebar.min_width,
                attach_context.as_ref(),
            )?;
            Ok(None)
        }
        SidebarCommand::Toggle { all, window, width } => {
            let (server_identity, socket) = ensure_daemon(runner, env)?;
            if all && window.is_some() {
                bail!("--all and --window cannot be used together");
            }
            let selection_context = resolve_selection_context(runner, env).ok();
            try_seed_sidebar_selection(selection_context.as_ref(), &socket, &server_identity);
            let attach_context = selection_context.as_ref().and_then(to_attach_context);
            if all {
                crate::sidebar::layout::toggle_all_with_attach_context(
                    runner,
                    &std::env::current_exe()?,
                    width.unwrap_or(config.sidebar.width),
                    config.sidebar.min_width,
                    attach_context.as_ref(),
                )?;
            } else {
                let target = resolve_window_target(runner, window)?;
                crate::sidebar::layout::toggle_with_attach_context(
                    runner,
                    &target,
                    &std::env::current_exe()?,
                    width.unwrap_or(config.sidebar.width),
                    config.sidebar.min_width,
                    attach_context.as_ref(),
                )?;
            }
            Ok(None)
        }
        SidebarCommand::FocusToggle { window, width } => {
            let (server_identity, socket) = ensure_daemon(runner, env)?;
            let target = resolve_window_target(runner, window)?;
            let selection_context = resolve_selection_context(runner, env).ok();
            try_seed_sidebar_selection(selection_context.as_ref(), &socket, &server_identity);
            let attach_context = selection_context.as_ref().and_then(to_attach_context);
            crate::sidebar::layout::focus_toggle_with_attach_context(
                runner,
                &target,
                &std::env::current_exe()?,
                width.unwrap_or(config.sidebar.width),
                config.sidebar.min_width,
                attach_context.as_ref(),
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
        SidebarCommand::LayoutApplied { window, width } => {
            let _ = ensure_daemon(runner, env)?;
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
            let (server_identity, socket) = ensure_daemon(runner, env)?;
            crate::sidebar::client::send_sidebar_jump_v2(&socket, &server_identity, &pane)?;
            Ok(None)
        }
        SidebarCommand::Focus { window } => {
            let (server_identity, socket) = ensure_daemon(runner, env)?;
            let target = resolve_window_target(runner, window)?;
            let selection_context = resolve_selection_context(runner, env).ok();
            try_seed_sidebar_selection(selection_context.as_ref(), &socket, &server_identity);
            crate::sidebar::layout::focus(runner, &target)?;
            Ok(None)
        }
    }
}

fn ensure_sidebar_daemon_started(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<(String, std::path::PathBuf)> {
    let (incarnation, socket) =
        crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, None)?;
    Ok((incarnation.hash, socket))
}

fn parse_sidebar_width(value: &str) -> std::result::Result<SidebarWidth, String> {
    value.parse()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarSelectionContext {
    pane: Option<String>,
    session: Option<String>,
}

fn try_seed_sidebar_selection(
    context: Option<&SidebarSelectionContext>,
    socket: &std::path::Path,
    server_identity: &str,
) {
    let Some(context) = context else {
        return;
    };
    if context.pane.is_none() && context.session.is_none() {
        return;
    }
    let _ = crate::sidebar::client::send_sidebar_select_context_v2(
        socket,
        server_identity,
        context.pane.as_deref(),
        context.session.as_deref(),
    );
}

fn try_seed_sidebar_selection_from_env(
    env: &BTreeMap<String, String>,
    socket: &std::path::Path,
    server_identity: &str,
) {
    let context = SidebarSelectionContext {
        pane: normalize_context_field(
            env.get(crate::sidebar::layout::ENV_SELECTION_PANE)
                .map(String::as_str),
        ),
        session: normalize_context_field(
            env.get(crate::sidebar::layout::ENV_SELECTION_SESSION)
                .map(String::as_str),
        ),
    };
    try_seed_sidebar_selection(Some(&context), socket, server_identity);
}

fn to_attach_context(
    context: &SidebarSelectionContext,
) -> Option<crate::sidebar::layout::SidebarAttachContext> {
    crate::sidebar::layout::SidebarAttachContext::new(context.pane.clone(), context.session.clone())
}

fn resolve_selection_context(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<SidebarSelectionContext> {
    let mut args = vec!["display-message", "-p"];
    if let Some(pane) = env
        .get("TMUX_PANE")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        args.extend(["-t", pane]);
    }
    args.extend(["-F", SELECTION_CONTEXT_FORMAT]);
    let output = runner.run(&args)?;
    Ok(parse_selection_context(output.trim()))
}

fn parse_selection_context(raw: &str) -> SidebarSelectionContext {
    let mut fields = raw.split('\u{1f}');
    SidebarSelectionContext {
        pane: normalize_context_field(fields.next()),
        session: normalize_context_field(fields.next()),
    }
}

fn normalize_context_field(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
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

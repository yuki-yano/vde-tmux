use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;

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
        #[arg(long)]
        width: Option<u16>,
        #[arg(long = "delay-ms")]
        delay_ms: Option<u64>,
    },
    Toggle {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        width: Option<u16>,
    },
    Close {
        #[arg(long)]
        window: Option<String>,
    },
    Rail {
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        width: Option<u16>,
    },
    Rebaseline {
        #[arg(long)]
        window: Option<String>,
    },
    #[command(name = "layout-applied")]
    LayoutApplied {
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        width: Option<u16>,
    },
    Jump {
        pane: String,
    },
}

pub(crate) fn run_sidebar_command(
    command: SidebarCommand,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<Option<String>> {
    match command {
        SidebarCommand::Attach { once } => {
            crate::sidebar::layout::attach(runner, env)?;
            let rendered = render_sidebar_once(runner, env, config)?;
            if once {
                return Ok(Some(rendered));
            }
            if !rendered.is_empty() {
                println!("{rendered}");
            }
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        SidebarCommand::Input { key } => {
            handle_sidebar_input_key(runner, env, config, &key)?;
            Ok(None)
        }
        SidebarCommand::Open {
            window,
            width,
            delay_ms,
        } => {
            if let Some(delay_ms) = delay_ms.filter(|value| *value > 0) {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::open(
                runner,
                &target,
                &std::env::current_exe()?,
                width.unwrap_or(config.sidebar.width),
            )?;
            Ok(None)
        }
        SidebarCommand::Toggle { all, window, width } => {
            if all && window.is_some() {
                bail!("--all and --window cannot be used together");
            }
            if all {
                crate::sidebar::layout::toggle_all(
                    runner,
                    &std::env::current_exe()?,
                    width.unwrap_or(config.sidebar.width),
                )?;
            } else {
                let target = resolve_window_target(runner, window)?;
                crate::sidebar::layout::toggle(
                    runner,
                    &target,
                    &std::env::current_exe()?,
                    width.unwrap_or(config.sidebar.width),
                )?;
            }
            Ok(None)
        }
        SidebarCommand::Close { window } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::close(runner, &target)?;
            Ok(None)
        }
        SidebarCommand::Rail { window, width } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::rail(runner, &target, width.unwrap_or(config.sidebar.width))?;
            Ok(None)
        }
        SidebarCommand::Rebaseline { window } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::rebaseline(runner, &target)?;
            Ok(None)
        }
        SidebarCommand::LayoutApplied { window, width } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::layout_applied(
                runner,
                &target,
                &std::env::current_exe()?,
                width.unwrap_or(config.sidebar.width),
            )?;
            Ok(None)
        }
        SidebarCommand::Jump { pane } => {
            crate::sidebar::layout::jump_to_pane(runner, &pane)?;
            Ok(None)
        }
    }
}

fn render_sidebar_once(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<String> {
    let panes = crate::options::snapshot::read_all_panes(runner)?;
    let state_path = crate::sidebar::store::state_path(env);
    let state = crate::sidebar::store::load_state(&state_path)?;
    let git = crate::git::collect_git_badges(&crate::git::SystemGitRunner::default(), &panes);
    let rows = crate::sidebar::tree::build_rows_with_git(config, &panes, &state, &git);
    Ok(crate::sidebar::render::render_rows(
        &rows,
        &state,
        config.sidebar.width as usize,
    ))
}

fn handle_sidebar_input_key(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
    key: &str,
) -> Result<()> {
    let Some(action) = crate::sidebar::input::parse_key(key) else {
        return Ok(());
    };
    let state_path = crate::sidebar::store::state_path(env);
    let mut state = crate::sidebar::store::load_state(&state_path)?;
    let panes = crate::options::snapshot::read_all_panes(runner)?;
    let rows = crate::sidebar::tree::build_rows(config, &panes, &state);
    let row_refs = crate::sidebar::tree::row_refs(&rows);
    let changed = match action {
        crate::sidebar::input::SidebarInputAction::MoveNext => {
            state.apply(crate::sidebar::state::SidebarAction::MoveNext, &row_refs)
        }
        crate::sidebar::input::SidebarInputAction::MovePrevious => state.apply(
            crate::sidebar::state::SidebarAction::MovePrevious,
            &row_refs,
        ),
        crate::sidebar::input::SidebarInputAction::ToggleExpand => state.apply(
            crate::sidebar::state::SidebarAction::ToggleExpand,
            &row_refs,
        ),
        crate::sidebar::input::SidebarInputAction::SetViewMode(view_mode) => state.apply(
            crate::sidebar::state::SidebarAction::SetViewMode(view_mode),
            &row_refs,
        ),
        crate::sidebar::input::SidebarInputAction::Activate => {
            match crate::sidebar::input::activate_selected(state.selection.as_deref(), &rows) {
                Some(crate::sidebar::input::SidebarCommand::JumpPane(pane_id)) => {
                    crate::sidebar::layout::jump_to_pane(runner, &pane_id)?;
                    false
                }
                Some(crate::sidebar::input::SidebarCommand::ToggleExpand(row_id)) => {
                    state.selection = Some(row_id);
                    state.apply(
                        crate::sidebar::state::SidebarAction::ToggleExpand,
                        &row_refs,
                    )
                }
                None => false,
            }
        }
    };
    if changed {
        crate::sidebar::store::save_state(&state_path, &state)?;
    }
    Ok(())
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

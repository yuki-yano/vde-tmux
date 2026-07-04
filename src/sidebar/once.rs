use std::collections::BTreeMap;

use anyhow::Result;

use crate::config::{Config, SidebarWidth};
use crate::tmux::TmuxRunner;

pub fn render_once(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &Config,
) -> Result<String> {
    let panes = crate::options::snapshot::read_all_panes(runner)?;
    let state_path = crate::sidebar::store::state_path(env);
    let state = crate::sidebar::store::load_state(&state_path)?;
    let git = crate::git::collect_git_badges(&crate::git::SystemGitRunner::default(), &panes);
    let rows = crate::sidebar::tree::build_rows_with_git(config, &panes, &state, &git);
    let width = match config.sidebar.width {
        SidebarWidth::Columns(width) => width,
        SidebarWidth::Percent(_) => config.sidebar.min_width,
    };
    Ok(crate::sidebar::render::render_rows(
        &rows,
        &state,
        width as usize,
    ))
}

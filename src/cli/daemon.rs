use std::collections::BTreeMap;

use anyhow::Result;

use crate::tmux::TmuxRunner;

pub(crate) fn statusline_agent_badge(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    crate::daemon::statusline_agent_badge(runner, env)
}

pub(crate) fn run_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let socket_path = crate::daemon::daemon_socket_path(env, socket);
    crate::daemon::server::run_daemon_server(runner, &socket_path)?;
    Ok(None)
}

pub(crate) fn config_schema() -> Result<Option<String>> {
    Ok(Some(serde_json::to_string_pretty(
        &crate::config::schema::config_schema(),
    )?))
}

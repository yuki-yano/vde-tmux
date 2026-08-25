use std::collections::BTreeMap;

use anyhow::Result;
use clap::Subcommand;

use crate::tmux::TmuxRunner;

#[derive(Debug, Subcommand)]
pub(super) enum ApiCommand {
    /// Emit the public JSON schemas without connecting to tmux or the daemon.
    Schema,
    /// Emit one compact, canonical pane/agent snapshot.
    Snapshot,
}

pub(super) fn dispatch(
    command: ApiCommand,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    match command {
        ApiCommand::Schema => crate::api::schema_json(observed_at),
        ApiCommand::Snapshot => crate::api::snapshot(runner, env, observed_at),
    }
}

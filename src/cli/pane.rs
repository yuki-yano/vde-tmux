use std::collections::BTreeMap;

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use crate::tmux::TmuxRunner;

#[derive(Debug, Subcommand)]
pub(super) enum PaneCommand {
    /// List panes from the daemon's cached canonical snapshot.
    List,
    /// Get one pane by %pane_id or pane_ref.
    Get { target: String },
    /// Get the pane identified by TMUX_PANE, never by client focus.
    Current,
    /// Capture bounded terminal output after pinning the pane process identity.
    Read {
        /// %pane_id or pane_ref; defaults to TMUX_PANE.
        target: Option<String>,
        #[command(flatten)]
        read: ApiReadArgs,
    },
    /// Split an exact pane without changing client focus by default.
    Split {
        /// Exact pane_ref returned by pane list/get/current.
        target: String,
        #[arg(long, value_enum)]
        direction: ApiPaneSplitDirectionArg,
        #[arg(long = "size-percent")]
        size_percent: Option<u8>,
        /// Absolute cwd for the new pane; defaults to the target pane cwd.
        #[arg(long)]
        cwd: Option<String>,
        /// Select the created pane. Omitted by default so agent automation does not steal focus.
        #[arg(long)]
        focus: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum ApiReadSourceArg {
    Visible,
    Latest,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum ApiPaneSplitDirectionArg {
    Right,
    Down,
}

impl From<ApiPaneSplitDirectionArg> for crate::api::PaneSplitDirection {
    fn from(value: ApiPaneSplitDirectionArg) -> Self {
        match value {
            ApiPaneSplitDirectionArg::Right => Self::Right,
            ApiPaneSplitDirectionArg::Down => Self::Down,
        }
    }
}

impl From<ApiReadSourceArg> for crate::api::ReadSource {
    fn from(value: ApiReadSourceArg) -> Self {
        match value {
            ApiReadSourceArg::Visible => Self::Visible,
            ApiReadSourceArg::Latest => Self::Latest,
        }
    }
}

#[derive(Debug, Clone, Copy, Args)]
pub(super) struct ApiReadArgs {
    #[arg(long, default_value_t = crate::api::DEFAULT_READ_LINES)]
    pub(super) lines: usize,
    #[arg(long, value_enum, default_value_t = ApiReadSourceArg::Latest)]
    pub(super) source: ApiReadSourceArg,
    #[arg(long)]
    pub(super) ansi: bool,
}

pub(super) fn dispatch(
    command: PaneCommand,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    match command {
        PaneCommand::List => crate::api::pane_list(runner, env, observed_at),
        PaneCommand::Get { target } => crate::api::pane_get(runner, env, observed_at, &target),
        PaneCommand::Current => crate::api::pane_current(runner, env, observed_at),
        PaneCommand::Read { target, read } => {
            let target = target
                .or_else(|| env.get("TMUX_PANE").cloned())
                .ok_or_else(|| {
                    crate::api::ApiError::new(
                        crate::api::ApiErrorCode::NoCurrentPane,
                        "target is required when TMUX_PANE is not set",
                    )
                })?;
            crate::api::pane_read(
                runner,
                env,
                observed_at,
                &target,
                crate::api::ReadOptions {
                    source: read.source.into(),
                    lines: read.lines,
                    ansi: read.ansi,
                },
            )
        }
        PaneCommand::Split {
            target,
            direction,
            size_percent,
            cwd,
            focus,
        } => crate::api::pane_split(
            runner,
            env,
            observed_at,
            &target,
            crate::api::PaneSplitOptions {
                direction: direction.into(),
                size_percent,
                cwd: cwd.as_deref(),
                focus,
            },
        ),
    }
}

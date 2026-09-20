pub(crate) mod agent_dispatch;
pub mod lifecycle;
pub mod protocol;
pub mod runtime;
pub(crate) mod server;
pub mod session_badge;
pub mod status_push;
pub(crate) mod task_summary;
pub(crate) mod tmux_control;
pub mod topology;
pub mod view_hooks;
pub mod workers;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::daemon::session_badge::{BadgeState, glyph_for_state};
use crate::sidebar::state::{SidebarNavigation, SidebarPreferences};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionEvent {
    pub pane_instance: crate::pane_state::PaneInstance,
    pub agent: String,
    pub state_version: Option<crate::pane_state::StateVersion>,
    pub run_seq: u64,
    pub completed_seq: u64,
    pub prompt_digest: Option<String>,
    pub prompt_submitted: bool,
    pub from: Option<BadgeState>,
    pub to: BadgeState,
    pub at_epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SidebarModel {
    pub preferences: SidebarPreferences,
    pub navigation: SidebarNavigation,
    pub active_sessions: BTreeSet<String>,
    pub active_categories: BTreeSet<String>,
    pub session_categories: BTreeMap<String, String>,
    #[serde(default)]
    pub category_state: crate::category::CategoryState,
    #[serde(default)]
    pub categories: crate::category::EffectiveCategoryModel,
    #[serde(default)]
    pub repo_identities: BTreeMap<String, crate::category::RepoIdentity>,
    #[serde(default)]
    pub git: BTreeMap<String, crate::git::GitBadge>,
    #[serde(default)]
    pub worktrees: BTreeMap<String, crate::git::WorktreeInfo>,
    #[serde(default)]
    pub needs_action: BTreeSet<crate::pane_state::PaneInstance>,
    pub triage_panes: BTreeSet<crate::pane_state::PaneInstance>,
    #[serde(default)]
    pub flashing: BTreeSet<crate::pane_state::PaneInstance>,
    pub task_summary_loading: BTreeSet<crate::pane_state::PaneInstance>,
}

pub fn render_summary(
    counts: &[(BadgeState, usize)],
    badge: &crate::config::BadgeConfig,
    format: &str,
) -> String {
    counts
        .iter()
        .map(|(state, count)| {
            let glyph = glyph_for_state(*state, &badge.glyphs);
            let color = match state {
                BadgeState::Blocked => &badge.colors.blocked,
                BadgeState::Limited => &badge.colors.limited,
                BadgeState::Working => &badge.colors.working,
                BadgeState::Done => &badge.colors.done,
                BadgeState::Idle => &badge.colors.idle,
            };
            let dim = if *count == 0 { ",dim" } else { "" };
            let count = count.to_string();
            let token = format.replace("{badge}", glyph).replace("{count}", &count);
            format!("#[fg={color}{dim}]{token}#[default]")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn daemon_socket_path_for_incarnation(
    _env: &BTreeMap<String, String>,
    _explicit: Option<&str>,
    incarnation_hash: &str,
) -> PathBuf {
    v2_runtime_root().join(format!("{incarnation_hash}.sock"))
}

pub fn writer_lease_namespace(incarnation_hash: &str) -> PathBuf {
    v2_runtime_root()
        .join("writer-leases")
        .join(incarnation_hash)
}

fn v2_runtime_root() -> PathBuf {
    PathBuf::from(format!("/tmp/vt-{}/v2", unsafe { libc::geteuid() }))
}

pub mod lifecycle;
pub mod protocol;
pub mod runtime;
pub mod server;
pub mod session_badge;
pub mod status_push;
pub mod topology;
pub mod view_hooks;
pub mod workers;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::daemon::session_badge::{BadgeState, glyph_for_state};
use crate::sidebar::state::{SidebarExpansionPreferences, SidebarOrderPreferences};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionEvent {
    pub pane_id: String,
    pub agent: String,
    pub from: Option<BadgeState>,
    pub to: BadgeState,
    pub at_epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SidebarModel {
    pub order: SidebarOrderPreferences,
    pub expansion: SidebarExpansionPreferences,
    pub active_sessions: BTreeSet<String>,
    #[serde(default)]
    pub git: BTreeMap<String, crate::git::GitBadge>,
    #[serde(default)]
    pub worktrees: BTreeMap<String, crate::git::WorktreeInfo>,
    #[serde(default)]
    pub needs_action: BTreeSet<crate::pane_state::PaneInstance>,
    #[serde(default)]
    pub flashing: BTreeSet<crate::pane_state::PaneInstance>,
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
                BadgeState::Working => &badge.colors.working,
                BadgeState::Done => &badge.colors.done,
                BadgeState::Idle => &badge.colors.idle,
            };
            let count = count.to_string();
            let token = format.replace("{badge}", glyph).replace("{count}", &count);
            format!("#[fg={color}]{token}#[default]")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_summary_counts_states_with_markup_and_includes_zero() {
        let badge = crate::config::BadgeConfig::default();
        let counts = [
            (BadgeState::Blocked, 2),
            (BadgeState::Working, 1),
            (BadgeState::Done, 0),
            (BadgeState::Idle, 3),
        ];
        assert_eq!(
            render_summary(&counts, &badge, "{badge} {count}"),
            "#[fg=#ff6b6b]▲ 2#[default] #[fg=#4fd08a]● 1#[default] #[fg=#45cbe6]✓ 0#[default] #[fg=#a8a8b2]○ 3#[default]"
        );
    }

    #[test]
    fn render_summary_applies_custom_format_to_each_colored_token() {
        let badge = crate::config::BadgeConfig::default();
        let counts = [(BadgeState::Working, 12), (BadgeState::Done, 3)];

        assert_eq!(
            render_summary(&counts, &badge, "{badge}: {count}"),
            "#[fg=#4fd08a]●: 12#[default] #[fg=#45cbe6]✓: 3#[default]"
        );
        assert_eq!(
            render_summary(&counts, &badge, "{badge}{count}"),
            "#[fg=#4fd08a]●12#[default] #[fg=#45cbe6]✓3#[default]"
        );
        assert_eq!(
            render_summary(&counts, &badge, "{count}{badge}"),
            "#[fg=#4fd08a]12●#[default] #[fg=#45cbe6]3✓#[default]"
        );
    }
}

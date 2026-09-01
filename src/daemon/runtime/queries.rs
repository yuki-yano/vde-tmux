use std::collections::{BTreeMap, BTreeSet};

use crate::pane_state::PaneInstance;

use super::CanonicalCoordinatorState;

impl CanonicalCoordinatorState {
    /// Distinct non-empty pane paths that carry a resolved agent, used to drive
    /// git polling without building the full resolved snapshot. Mirrors the
    /// `resolved.is_some()` filter in `resolved_snapshot_with_git_at`.
    pub fn git_polling_paths(&self) -> BTreeSet<String> {
        let mut paths = self
            .topology
            .panes
            .iter()
            .filter(|topology| {
                matches!(
                    self.leased.runtime.record(&topology.pane_instance),
                    Some(state) if state.agent_present || state.unread.is_unread()
                )
            })
            .map(|topology| topology.current_path.clone())
            .filter(|path| !path.trim().is_empty())
            .collect::<BTreeSet<_>>();
        paths.extend(
            self.status_metadata
                .sessions
                .values()
                .map(|session| session.project_path.trim())
                .filter(|path| !path.is_empty())
                .map(str::to_string),
        );
        paths
    }

    /// Whether the canonical topology currently contains `pane_instance`,
    /// without building the full resolved snapshot.
    pub fn contains_pane(&self, pane_instance: &PaneInstance) -> bool {
        self.topology
            .panes
            .iter()
            .any(|pane| &pane.pane_instance == pane_instance)
    }

    pub fn latest_unread_candidates(&self) -> Vec<PaneInstance> {
        let mut candidates = self
            .topology
            .panes
            .iter()
            .filter_map(|topology| {
                let state = self.leased.runtime.record(&topology.pane_instance)?;
                let occurrence = state.unread.latest_unread()?;
                state
                    .unread
                    .is_jump_eligible(&state.lifecycle)
                    .then_some((occurrence.order, topology.pane_instance.clone()))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        candidates.into_iter().map(|(_, pane)| pane).collect()
    }

    pub fn window_panes(&self) -> BTreeMap<String, Vec<PaneInstance>> {
        let mut windows = BTreeMap::<String, Vec<PaneInstance>>::new();
        for pane in &self.topology.panes {
            windows
                .entry(pane.window_id.clone())
                .or_default()
                .push(pane.pane_instance.clone());
        }
        for panes in windows.values_mut() {
            panes.sort();
            panes.dedup();
        }
        windows
    }

    pub fn focus_equivalent_panes(&self, target: &PaneInstance) -> BTreeSet<PaneInstance> {
        let mut panes = BTreeSet::from([target.clone()]);
        if self
            .leased
            .runtime
            .record(target)
            .is_some_and(|state| state.agent_present)
        {
            panes.extend(self.topology.focus_proxy_sources(target));
        }
        panes
    }
}

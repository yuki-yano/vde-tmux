use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::config::Config;
use crate::daemon::protocol::v2::{
    DaemonDiagnostic, PanePresentation, ResolvedSnapshot, ServerMessage, SessionLinkPresentation,
    StatusContext, StatusSnapshot,
};
use crate::daemon::topology::TopologySnapshot;
use crate::daemon::{SidebarModel, TransitionEvent};
use crate::git::{GitBadge, WorktreeInfo};
use crate::pane_state::{
    ClientWitness, PaneInstance, StoreError, StoredStateDescriptor, WaitReason,
};
use crate::sidebar::tree::now_epoch_secs;

use super::{CanonicalCoordinatorState, CanonicalPaneStateRuntime, StatusProjectionMetadata};

const EVENT_CAP: usize = crate::pane_state::store::MAX_DIAGNOSTICS;

fn effective_focused_panes(
    runtime: &CanonicalPaneStateRuntime,
    topology: &TopologySnapshot,
    witnesses: &[ClientWitness],
) -> BTreeSet<PaneInstance> {
    let direct = witnesses
        .iter()
        .filter(|witness| witness.is_eligible())
        .map(|witness| witness.active_pane.clone())
        .collect::<BTreeSet<_>>();
    let mut focused = direct.clone();
    for source in direct {
        let Some(target) = topology.focus_proxy_target(&source) else {
            continue;
        };
        if runtime
            .record(target)
            .is_some_and(|state| state.agent_present)
        {
            focused.insert(target.clone());
        }
    }
    focused
}

impl CanonicalCoordinatorState {
    pub fn resolved_snapshot(&self) -> ResolvedSnapshot {
        self.resolved_snapshot_from(
            &self.leased.runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        )
    }

    pub(crate) fn checked_resolved_snapshot(&self) -> Result<ResolvedSnapshot, StoreError> {
        let snapshot = self.resolved_snapshot();
        preflight_resolved_snapshot_against_runtime(&snapshot, &self.leased.runtime)?;
        Ok(snapshot)
    }

    pub(super) fn resolved_snapshot_from(
        &self,
        runtime: &CanonicalPaneStateRuntime,
        topology_snapshot: &TopologySnapshot,
        hook_diagnostic: Option<&DaemonDiagnostic>,
        global_diagnostics: &VecDeque<DaemonDiagnostic>,
    ) -> ResolvedSnapshot {
        self.resolved_snapshot_with_git_from(
            runtime,
            topology_snapshot,
            hook_diagnostic,
            global_diagnostics,
            &self.git_badges,
            &self.worktrees,
        )
    }

    #[allow(clippy::too_many_arguments)] // Keeps projection inputs explicit and independently testable.
    fn resolved_snapshot_with_git_from(
        &self,
        runtime: &CanonicalPaneStateRuntime,
        topology_snapshot: &TopologySnapshot,
        hook_diagnostic: Option<&DaemonDiagnostic>,
        global_diagnostics: &VecDeque<DaemonDiagnostic>,
        git_badges: &BTreeMap<String, GitBadge>,
        worktrees: &BTreeMap<String, WorktreeInfo>,
    ) -> ResolvedSnapshot {
        self.resolved_snapshot_with_git_at(
            runtime,
            topology_snapshot,
            hook_diagnostic,
            global_diagnostics,
            git_badges,
            worktrees,
            now_epoch_secs(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolved_snapshot_with_git_at(
        &self,
        runtime: &CanonicalPaneStateRuntime,
        topology_snapshot: &TopologySnapshot,
        hook_diagnostic: Option<&DaemonDiagnostic>,
        global_diagnostics: &VecDeque<DaemonDiagnostic>,
        git_badges: &BTreeMap<String, GitBadge>,
        worktrees: &BTreeMap<String, WorktreeInfo>,
        now: i64,
    ) -> ResolvedSnapshot {
        use crate::daemon::protocol::v2::{
            AttentionEntry, DaemonDiagnostic, ErrorCode, PanePresentation, ResolvedSnapshot,
        };
        use crate::pane_state::{LifecycleState, ResolvedPaneState};

        let mut panes = Vec::with_capacity(topology_snapshot.panes.len());
        let mut attention = Vec::new();
        let witnesses = self.views.clients().values().cloned().collect::<Vec<_>>();
        let focused_instances = effective_focused_panes(runtime, topology_snapshot, &witnesses);
        let triage = runtime
            .triage_entries()
            .map(|(pane, badge)| (pane.clone(), badge))
            .collect::<BTreeMap<_, _>>();
        for topology in &topology_snapshot.panes {
            let stored = runtime.descriptor(&topology.pane_instance);
            let record = runtime.record(&topology.pane_instance);
            let resolved = match record {
                Some(state) if state.agent_present || state.unread.is_unread() => {
                    Some(ResolvedPaneState {
                        canonical: state.clone(),
                        window_id: topology.window_id.clone(),
                        pane_id: topology.pane_instance.pane_id.clone(),
                        current_path: topology.current_path.clone(),
                        badge: crate::pane_state::resolve_badge(state),
                    })
                }
                _ => None,
            };
            let retained_state = if resolved.is_none() {
                record.map(crate::daemon::protocol::v2::RetainedAgentState::from)
            } else {
                None
            };
            if let Some(badge) = triage.get(&topology.pane_instance)
                && !focused_instances.contains(&topology.pane_instance)
            {
                let active = record;
                let reason = match active.map(|state| &state.lifecycle) {
                    Some(LifecycleState::Waiting {
                        reason: WaitReason::PermissionPrompt,
                    }) => Some("permission_prompt".to_string()),
                    Some(LifecycleState::Waiting { reason }) if reason.is_usage_limit() => {
                        Some(crate::pane_state::USAGE_LIMIT_WAIT_REASON.to_string())
                    }
                    Some(LifecycleState::Waiting {
                        reason: WaitReason::Other(_),
                    }) => Some("Other(wait)".to_string()),
                    Some(LifecycleState::Error { .. }) => Some("error".to_string()),
                    _ => Some("Other(calm)".to_string()),
                };
                attention.push(AttentionEntry {
                    pane_instance: topology.pane_instance.clone(),
                    session_name: topology
                        .session_links
                        .first()
                        .map(|link| link.session_name.clone())
                        .unwrap_or_default(),
                    badge: *badge,
                    reason,
                    elapsed_seconds: now
                        .saturating_sub(active.and_then(|state| state.started_at).unwrap_or(now))
                        .max(0),
                });
            }
            panes.push(PanePresentation {
                pane_instance: topology.pane_instance.clone(),
                agent_process: runtime.tracker(&topology.pane_instance).agent_process,
                session_links: topology.session_links.clone(),
                window_id: topology.window_id.clone(),
                window_name: topology.window_name.clone(),
                current_path: topology.current_path.clone(),
                current_command: topology.current_command.clone(),
                pane_width: topology.pane_width,
                active: topology.active,
                focused: focused_instances.contains(&topology.pane_instance),
                stored,
                resolved,
                retained_state,
            });
        }
        panes.sort_by(|left, right| left.pane_instance.cmp(&right.pane_instance));
        attention.sort_by_key(|entry| std::cmp::Reverse(entry.elapsed_seconds));
        let mut diagnostics = runtime
            .diagnostics()
            .iter()
            .map(|diagnostic| DaemonDiagnostic {
                code: ErrorCode::PersistFailed,
                message: diagnostic.message.clone(),
                pane_instance: Some(diagnostic.pane_instance.clone()),
                event_id: None,
            })
            .collect::<Vec<_>>();
        if let Some(hook_diagnostic) = hook_diagnostic {
            diagnostics.push(hook_diagnostic.clone());
        }
        diagnostics.extend(global_diagnostics.iter().cloned());
        if diagnostics.len() > crate::pane_state::store::MAX_DIAGNOSTICS {
            diagnostics.drain(..diagnostics.len() - crate::pane_state::store::MAX_DIAGNOSTICS);
        }
        let snapshot_revision = runtime.snapshot_revision();
        let events = runtime
            .transitions()
            .iter()
            .rev()
            .filter_map(|transition| {
                let to = transition.to?;
                Some(TransitionEvent {
                    pane_instance: transition.pane_instance.clone(),
                    agent: transition
                        .agent
                        .as_ref()
                        .map(|agent| agent.as_str().to_string())
                        .unwrap_or_default(),
                    state_version: transition.state_version.clone(),
                    run_seq: transition.run_seq,
                    completed_seq: transition.completed_seq,
                    prompt_digest: transition.prompt_digest.clone(),
                    prompt_submitted: transition.prompt_submitted,
                    from: transition.from,
                    to,
                    at_epoch: transition.at_epoch,
                })
            })
            .take(EVENT_CAP)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let active_sessions = self
            .views
            .clients()
            .values()
            .filter(|witness| witness.is_eligible())
            .map(|witness| witness.session_id.clone())
            .collect::<BTreeSet<_>>();
        let categories = self.effective_category_model();
        let session_categories = self
            .status_metadata
            .sessions
            .iter()
            .map(|(session_id, session)| {
                let category = self
                    .repo_identities
                    .get(&session.project_path)
                    .and_then(|identity| categories.placements.get(&identity.key))
                    .map(|placement| placement.category.to_string())
                    .unwrap_or_else(|| crate::category::UNCATEGORIZED.to_string());
                (session_id.clone(), category)
            })
            .collect::<BTreeMap<_, _>>();
        let active_categories = active_sessions
            .iter()
            .filter_map(|session_id| session_categories.get(session_id).cloned())
            .collect();
        ResolvedSnapshot {
            snapshot_revision,
            panes,
            sidebar_model: SidebarModel {
                preferences: self.sidebar_preferences.clone(),
                navigation: self.sidebar_navigation.clone(),
                category_state: self.category_state.clone(),
                categories,
                repo_identities: self.repo_identities.clone(),
                active_sessions,
                active_categories,
                session_categories,
                git: git_badges.clone(),
                worktrees: worktrees.clone(),
                needs_action: runtime.triage_panes().cloned().collect(),
                flashing: runtime.flashing_panes().cloned().collect(),
            },
            attention,
            events,
            diagnostics,
        }
    }

    pub fn pane_presentation(&self, pane_id: &str) -> Option<PanePresentation> {
        self.resolved_snapshot()
            .panes
            .into_iter()
            .find(|pane| pane.pane_instance.pane_id == pane_id)
    }

    pub fn status_snapshot(&self, context: StatusContext) -> StatusSnapshot {
        build_status_snapshot(
            &self.resolved_snapshot(),
            context,
            &self.status_metadata,
            &self.projection_config,
        )
    }

    pub fn display_projection(
        &self,
    ) -> (StatusSnapshot, Vec<StatusSnapshot>, Vec<PanePresentation>) {
        let resolved = self.resolved_snapshot();
        let global = build_status_snapshot(
            &resolved,
            StatusContext::Global,
            &self.status_metadata,
            &self.projection_config,
        );
        let sessions = self
            .status_metadata
            .sessions
            .keys()
            .map(|session_id| {
                build_status_snapshot(
                    &resolved,
                    StatusContext::Session {
                        session_id: session_id.clone(),
                    },
                    &self.status_metadata,
                    &self.projection_config,
                )
            })
            .collect();
        (global, sessions, resolved.panes)
    }
}

#[derive(Debug, Clone)]
struct WindowStatusAggregate {
    window_name: String,
    links: BTreeMap<String, SessionLinkPresentation>,
    current_command: Option<String>,
}

pub(crate) fn build_status_snapshot(
    resolved: &ResolvedSnapshot,
    context: StatusContext,
    metadata: &StatusProjectionMetadata,
    config: &Config,
) -> StatusSnapshot {
    use crate::daemon::protocol::v2::{
        CategoryStatusPresentation, SessionStatusPresentation, StatusContext, StatusSnapshot,
        WindowStatusPresentation,
    };

    let mut pane_badges = BTreeMap::new();
    let mut session_names = BTreeMap::<String, String>::new();
    let mut session_panes = BTreeMap::<String, BTreeSet<PaneInstance>>::new();
    let mut window_panes = BTreeMap::<String, BTreeSet<PaneInstance>>::new();
    let mut windows = BTreeMap::<String, WindowStatusAggregate>::new();

    for pane in &resolved.panes {
        if let Some(resolved) = &pane.resolved {
            pane_badges
                .entry(pane.pane_instance.clone())
                .or_insert(resolved.badge);
        }
        window_panes
            .entry(pane.window_id.clone())
            .or_default()
            .insert(pane.pane_instance.clone());
        let window =
            windows
                .entry(pane.window_id.clone())
                .or_insert_with(|| WindowStatusAggregate {
                    window_name: pane.window_name.clone(),
                    links: BTreeMap::new(),
                    current_command: None,
                });
        if pane.active || window.current_command.is_none() {
            window.current_command = Some(pane.current_command.clone());
        }
        for link in &pane.session_links {
            session_names
                .entry(link.session_id.clone())
                .or_insert_with(|| link.session_name.clone());
            session_panes
                .entry(link.session_id.clone())
                .or_default()
                .insert(pane.pane_instance.clone());
            window
                .links
                .entry(link.session_id.clone())
                .or_insert_with(|| link.clone());
        }
    }

    for (session_id, session) in &metadata.sessions {
        session_names
            .entry(session_id.clone())
            .or_insert_with(|| session.session_name.clone());
        session_panes.entry(session_id.clone()).or_default();
    }

    let effective_categories = session_names
        .iter()
        .map(|(session_id, topology_name)| {
            let projection = metadata.sessions.get(session_id);
            let project_path = projection
                .map(|session| session.project_path.as_str())
                .unwrap_or_default();
            let category = resolved
                .sidebar_model
                .repo_identities
                .get(project_path)
                .and_then(|identity| {
                    resolved
                        .sidebar_model
                        .categories
                        .placements
                        .get(&identity.key)
                })
                .map(|placement| placement.category.to_string())
                .unwrap_or_else(|| crate::category::UNCATEGORIZED.to_string());
            let _ = topology_name;
            (session_id.clone(), category)
        })
        .collect::<BTreeMap<_, _>>();

    let summary =
        crate::daemon::session_badge::BadgeStateCounts::from_states(pane_badges.values().copied());
    let counts_for = |panes: Option<&BTreeSet<PaneInstance>>| {
        crate::daemon::session_badge::BadgeStateCounts::from_states(
            panes
                .into_iter()
                .flat_map(|panes| panes.iter())
                .filter_map(|pane| pane_badges.get(pane).copied()),
        )
    };

    let active_session_id = match &context {
        StatusContext::Global => None,
        StatusContext::Session { session_id } => Some(session_id.as_str()),
    };
    let active_category = active_session_id.map(|session_id| {
        effective_categories
            .get(session_id)
            .map(String::as_str)
            .unwrap_or_default()
    });

    let mut all_sessions = session_names
        .into_iter()
        .map(|(session_id, session_name)| {
            let session_metadata = metadata.sessions.get(&session_id);
            SessionStatusPresentation {
                counts: counts_for(session_panes.get(&session_id)),
                active: active_session_id == Some(session_id.as_str()),
                category: Some(
                    effective_categories
                        .get(&session_id)
                        .cloned()
                        .unwrap_or_default(),
                ),
                attached: session_metadata.and_then(|session| session.attached),
                created_at: session_metadata.and_then(|session| session.created_at),
                session_id,
                session_name,
            }
        })
        .collect::<Vec<_>>();
    all_sessions.sort_by(|left, right| {
        left.session_name
            .cmp(&right.session_name)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    let (sessions, session_zone_width) = match active_category {
        Some(active_category) => {
            let mut sessions_by_category =
                BTreeMap::<String, Vec<SessionStatusPresentation>>::new();
            for session in all_sessions {
                sessions_by_category
                    .entry(session.category.clone().unwrap_or_default())
                    .or_default()
                    .push(session);
            }
            let session_zone_width = config.statusline.sessions.fixed_width.then(|| {
                sessions_by_category
                    .values()
                    .map(|sessions| crate::statusline::sessions_display_width(config, sessions))
                    .max()
                    .unwrap_or(0)
            });
            let sessions = sessions_by_category
                .remove(active_category)
                .unwrap_or_default();
            (sessions, session_zone_width)
        }
        None => (all_sessions, None),
    };

    let mut window_presentations = windows
        .into_iter()
        .filter_map(|(window_id, window)| {
            let selected_links = match active_session_id {
                Some(session_id) => window
                    .links
                    .get(session_id)
                    .cloned()
                    .into_iter()
                    .collect::<Vec<_>>(),
                None => window.links.values().cloned().collect::<Vec<_>>(),
            };
            if active_session_id.is_some() && selected_links.is_empty() {
                return None;
            }
            let session_ids = selected_links
                .iter()
                .map(|link| link.session_id.clone())
                .collect::<Vec<_>>();
            let window_index =
                selected_links
                    .first()
                    .map(|link| link.window_index)
                    .filter(|index| {
                        selected_links
                            .iter()
                            .all(|link| link.window_index == *index)
                    });
            let window_metadata = metadata.windows.get(&window_id);
            Some(WindowStatusPresentation {
                counts: counts_for(window_panes.get(&window_id)),
                pane_count: window_panes.get(&window_id).map_or(0, BTreeSet::len),
                active: selected_links.iter().any(|link| link.window_active),
                last: selected_links.iter().any(|link| link.window_last),
                bell: window_metadata.and_then(|window| window.bell),
                activity: window_metadata.and_then(|window| window.activity),
                silence: window_metadata.and_then(|window| window.silence),
                current_command: window.current_command,
                window_id,
                window_name: window.window_name,
                session_ids,
                window_index,
            })
        })
        .collect::<Vec<_>>();
    window_presentations.sort_by(|left, right| {
        left.window_index
            .unwrap_or(i64::MAX)
            .cmp(&right.window_index.unwrap_or(i64::MAX))
            .then_with(|| left.window_id.cmp(&right.window_id))
    });

    let category_names = effective_categories
        .values()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut categories = category_names
        .into_iter()
        .map(|category| {
            let mut category_panes = BTreeSet::new();
            let mut session_ids = effective_categories
                .keys()
                .filter_map(|session_id| {
                    (effective_categories
                        .get(session_id)
                        .map(String::as_str)
                        .unwrap_or_default()
                        == category)
                        .then_some(session_id.clone())
                })
                .collect::<Vec<_>>();
            session_ids.sort();
            for session_id in &session_ids {
                if let Some(panes) = session_panes.get(session_id) {
                    category_panes.extend(panes.iter().cloned());
                }
            }
            CategoryStatusPresentation {
                counts: counts_for(Some(&category_panes)),
                active: active_category == Some(category.as_str()),
                category,
                session_ids,
            }
        })
        .collect::<Vec<_>>();
    categories.sort_by(|left, right| {
        left.category
            .is_empty()
            .cmp(&right.category.is_empty())
            .then_with(|| {
                config
                    .categories
                    .order
                    .get(&left.category)
                    .copied()
                    .unwrap_or(i64::MAX)
                    .cmp(
                        &config
                            .categories
                            .order
                            .get(&right.category)
                            .copied()
                            .unwrap_or(i64::MAX),
                    )
            })
            .then_with(|| left.category.cmp(&right.category))
    });

    StatusSnapshot {
        snapshot_revision: resolved.snapshot_revision,
        context,
        summary,
        session_zone_width,
        sessions,
        windows: window_presentations,
        categories,
        attention: resolved.attention.clone(),
    }
}

pub(super) fn preflight_resolved_snapshot(snapshot: &ResolvedSnapshot) -> Result<(), StoreError> {
    for pane in &snapshot.panes {
        if pane.pane_width == 0 {
            return Err(StoreError::FailStop(format!(
                "projection invariant violated for {}: pane width must be positive",
                pane.pane_instance.pane_id
            )));
        }
        if let (Some(StoredStateDescriptor::Canonical { version }), Some(resolved)) =
            (&pane.stored, &pane.resolved)
            && version != &resolved.canonical.version()
        {
            return Err(StoreError::Reduce(
                crate::pane_state::reducer::ReduceError::StateInvariantViolation(format!(
                    "projection invariant violated for {}: stored and resolved canonical versions differ",
                    pane.pane_instance.pane_id
                )),
            ));
        }
    }
    let message = ServerMessage::ResolvedSnapshotResult {
        snapshot_revision: snapshot.snapshot_revision,
        snapshot: snapshot.clone(),
    };
    let _bytes =
        serde_json::to_vec(&message).map_err(|error| StoreError::Random(error.to_string()))?;
    Ok(())
}

pub(super) fn preflight_resolved_snapshot_against_runtime(
    snapshot: &ResolvedSnapshot,
    runtime: &CanonicalPaneStateRuntime,
) -> Result<(), StoreError> {
    preflight_resolved_snapshot(snapshot)?;
    for pane in &snapshot.panes {
        match (&pane.stored, &pane.resolved) {
            (Some(StoredStateDescriptor::Canonical { version }), Some(resolved))
                if version == &resolved.canonical.version() => {}
            (Some(StoredStateDescriptor::Canonical { version }), None) => {
                let confirmed_ended = matches!(
                    runtime.record(&pane.pane_instance),
                    Some(state)
                        if &state.version() == version
                            && !state.agent_present
                            && !state.unread.is_unread()
                );
                if !confirmed_ended {
                    return Err(StoreError::FailStop(format!(
                        "projection invariant violated for {}: canonical state is unresolved without a confirmed agent end",
                        pane.pane_instance.pane_id
                    )));
                }
            }
            (Some(StoredStateDescriptor::Canonical { .. }), Some(_)) => {
                return Err(StoreError::FailStop(format!(
                    "projection invariant violated for {}: stored and resolved canonical versions differ",
                    pane.pane_instance.pane_id
                )));
            }
            (None, None) => {}
            (_, Some(_)) => {
                return Err(StoreError::FailStop(format!(
                    "projection invariant violated for {}: resolved state has no canonical storage",
                    pane.pane_instance.pane_id
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn preflight_status_snapshot(snapshot: &StatusSnapshot) -> Result<(), StoreError> {
    let message = ServerMessage::StatusSnapshotResult {
        snapshot_revision: snapshot.snapshot_revision,
        snapshot: snapshot.clone(),
    };
    let _bytes =
        serde_json::to_vec(&message).map_err(|error| StoreError::Random(error.to_string()))?;
    Ok(())
}

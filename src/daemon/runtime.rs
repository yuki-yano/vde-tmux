use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::config::Config;
use crate::daemon::{SidebarModel, TransitionEvent};
use crate::git::{GitBadge, WorktreeInfo};
pub use crate::pane_state::CanonicalStateRuntime as CanonicalPaneStateRuntime;
use crate::sidebar::state::SidebarOrderPreferences;
use crate::sidebar::tree::now_epoch_secs;

const EVENT_CAP: usize = crate::pane_state::store::MAX_DIAGNOSTICS;

pub(crate) struct LeasedCanonicalPaneStateRuntime {
    pub runtime: CanonicalPaneStateRuntime,
    _writer_lease: crate::daemon::lifecycle::DaemonFileLock,
}

impl LeasedCanonicalPaneStateRuntime {
    pub fn acquire(namespace: &std::path::Path) -> Result<Self, crate::pane_state::StoreError> {
        let lease = crate::daemon::lifecycle::try_acquire_writer_lease(namespace)
            .map_err(|error| crate::pane_state::StoreError::PersistFailed(error.to_string()))?
            .ok_or(crate::pane_state::StoreError::WriterLeaseHeld)?;
        Ok(Self {
            runtime: CanonicalPaneStateRuntime::default(),
            _writer_lease: lease,
        })
    }

    pub fn hydrate(&mut self, entries: Vec<crate::pane_state::RawPaneRecord>) {
        self.runtime = CanonicalPaneStateRuntime::hydrate(entries);
    }

    #[cfg(test)]
    pub fn bootstrap(
        namespace: &std::path::Path,
        load_after_lease: impl FnOnce() -> Result<
            Vec<crate::pane_state::RawPaneRecord>,
            crate::pane_state::StoreError,
        >,
    ) -> Result<Self, crate::pane_state::StoreError> {
        let mut leased = Self::acquire(namespace)?;
        let entries = load_after_lease()?;
        leased.hydrate(entries);
        Ok(leased)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StatusProjectionMetadata {
    pub categories: BTreeSet<String>,
    pub sessions: BTreeMap<String, SessionProjectionMetadata>,
    pub windows: BTreeMap<String, WindowProjectionMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SessionProjectionMetadata {
    pub session_name: String,
    pub stored_category: Option<String>,
    pub project_path: String,
    pub category_override: String,
    pub attached: Option<bool>,
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct WindowProjectionMetadata {
    pub bell: Option<bool>,
    pub activity: Option<bool>,
    pub silence: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CanonicalSidebarEffect {
    JumpPane {
        pane_instance: crate::pane_state::PaneInstance,
        client_pid: u32,
        source_pane: crate::pane_state::PaneInstance,
    },
}

pub(crate) struct CanonicalCoordinatorState {
    pub leased: LeasedCanonicalPaneStateRuntime,
    pub topology: crate::daemon::topology::TopologySnapshot,
    pub views: crate::daemon::view_hooks::ViewRegistry,
    pub sidebar_order: SidebarOrderPreferences,
    pub sidebar_expansion: crate::sidebar::state::SidebarExpansionPreferences,
    pub hook_health: crate::daemon::protocol::v2::HookHealth,
    pub hook_diagnostic: Option<crate::daemon::protocol::v2::DaemonDiagnostic>,
    pub global_diagnostics: VecDeque<crate::daemon::protocol::v2::DaemonDiagnostic>,
    pub status_metadata: StatusProjectionMetadata,
    pub git_badges: BTreeMap<String, GitBadge>,
    pub worktrees: BTreeMap<String, WorktreeInfo>,
    pub projection_config: Config,
}

impl CanonicalCoordinatorState {
    pub fn new(
        leased: LeasedCanonicalPaneStateRuntime,
        topology: crate::daemon::topology::TopologySnapshot,
        views: crate::daemon::view_hooks::ViewRegistry,
        sidebar_order: SidebarOrderPreferences,
    ) -> Self {
        Self {
            leased,
            topology,
            views,
            sidebar_order,
            sidebar_expansion: crate::sidebar::state::SidebarExpansionPreferences::default(),
            hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
            hook_diagnostic: None,
            global_diagnostics: VecDeque::new(),
            status_metadata: StatusProjectionMetadata::default(),
            git_badges: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            projection_config: Config::default(),
        }
    }

    pub fn set_hook_health(
        &mut self,
        health: crate::daemon::protocol::v2::HookHealth,
        diagnostic: Option<String>,
    ) -> Result<bool, crate::pane_state::StoreError> {
        use crate::daemon::protocol::v2::{DaemonDiagnostic, ErrorCode, HookHealth};

        let hook_diagnostic = (health == HookHealth::Degraded).then(|| DaemonDiagnostic {
            code: ErrorCode::HookCollision,
            message: diagnostic
                .unwrap_or_else(|| "pane-state hook ownership is degraded".to_string()),
            pane_instance: None,
            event_id: None,
        });
        if self.hook_health == health && self.hook_diagnostic == hook_diagnostic {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let snapshot = self.resolved_snapshot_from(
            &runtime,
            &self.topology,
            hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        );
        preflight_resolved_snapshot(&snapshot)?;
        self.leased.runtime = runtime;
        self.hook_health = health;
        self.hook_diagnostic = hook_diagnostic;
        Ok(true)
    }

    pub fn add_global_diagnostic(
        &mut self,
        code: crate::daemon::protocol::v2::ErrorCode,
        message: String,
    ) -> Result<u64, crate::pane_state::StoreError> {
        let mut diagnostics = self.global_diagnostics.clone();
        diagnostics.push_back(crate::daemon::protocol::v2::DaemonDiagnostic {
            code,
            message,
            pane_instance: None,
            event_id: None,
        });
        while diagnostics.len() > crate::pane_state::store::MAX_DIAGNOSTICS {
            diagnostics.pop_front();
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let snapshot = self.resolved_snapshot_from(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &diagnostics,
        );
        preflight_resolved_snapshot(&snapshot)?;
        self.leased.runtime = runtime;
        self.global_diagnostics = diagnostics;
        Ok(self.leased.runtime.snapshot_revision())
    }

    pub fn record_frame_too_large_diagnostic(
        &mut self,
        rejected_revision: u64,
    ) -> Result<bool, crate::pane_state::StoreError> {
        use crate::daemon::protocol::v2::{DaemonDiagnostic, ErrorCode};

        let message = format!(
            "resolved snapshot revision {rejected_revision} exceeds the response frame limit"
        );
        if self
            .global_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ErrorCode::FrameTooLarge)
        {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let mut diagnostics = self.global_diagnostics.clone();
        diagnostics.push_back(DaemonDiagnostic {
            code: ErrorCode::FrameTooLarge,
            message,
            pane_instance: None,
            event_id: None,
        });
        while diagnostics.len() > crate::pane_state::store::MAX_DIAGNOSTICS {
            diagnostics.pop_front();
        }
        self.leased.runtime = runtime;
        self.global_diagnostics = diagnostics;
        Ok(true)
    }

    pub fn records_snapshot(
        &self,
    ) -> BTreeMap<crate::pane_state::PaneInstance, crate::pane_state::StoredPaneRecord> {
        self.leased.runtime.records_snapshot()
    }

    /// Distinct non-empty pane paths that carry a resolved agent, used to drive
    /// git polling without building the full resolved snapshot. Mirrors the
    /// `resolved.is_some()` filter in `resolved_snapshot_with_git_at`.
    pub fn git_polling_paths(&self) -> BTreeSet<String> {
        use crate::pane_state::StoredPaneRecord;
        self.topology
            .panes
            .iter()
            .filter(|topology| {
                matches!(
                    self.leased.runtime.record(&topology.pane_instance),
                    Some(StoredPaneRecord::Active(state))
                        if state.agent_present || state.completed_seq > state.acknowledged_seq
                )
            })
            .map(|topology| topology.current_path.clone())
            .filter(|path| !path.trim().is_empty())
            .collect()
    }

    /// Whether the canonical topology currently contains `pane_instance`,
    /// without building the full resolved snapshot.
    pub fn contains_pane(&self, pane_instance: &crate::pane_state::PaneInstance) -> bool {
        self.topology
            .panes
            .iter()
            .any(|pane| &pane.pane_instance == pane_instance)
    }

    pub fn window_panes(&self) -> BTreeMap<String, Vec<crate::pane_state::PaneInstance>> {
        let mut windows = BTreeMap::<String, Vec<crate::pane_state::PaneInstance>>::new();
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

    pub fn replace_topology(
        &mut self,
        topology: crate::daemon::topology::TopologySnapshot,
    ) -> Result<bool, crate::pane_state::StoreError> {
        if self.topology == topology {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let snapshot = self.resolved_snapshot_from(
            &runtime,
            &topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        );
        preflight_resolved_snapshot(&snapshot)?;
        self.leased.runtime = runtime;
        self.topology = topology;
        Ok(true)
    }

    pub fn replace_git_projection(
        &mut self,
        git_badges: BTreeMap<String, GitBadge>,
        worktrees: BTreeMap<String, WorktreeInfo>,
    ) -> Result<bool, crate::pane_state::StoreError> {
        if self.git_badges == git_badges && self.worktrees == worktrees {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let snapshot = self.resolved_snapshot_with_git_at(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
            &git_badges,
            &worktrees,
            now_epoch_secs(),
        );
        preflight_resolved_snapshot(&snapshot)?;
        self.leased.runtime = runtime;
        self.git_badges = git_badges;
        self.worktrees = worktrees;
        Ok(true)
    }

    pub fn resolved_snapshot(&self) -> crate::daemon::protocol::v2::ResolvedSnapshot {
        self.resolved_snapshot_from(
            &self.leased.runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        )
    }

    pub(crate) fn checked_resolved_snapshot(
        &self,
    ) -> Result<crate::daemon::protocol::v2::ResolvedSnapshot, crate::pane_state::StoreError> {
        let snapshot = self.resolved_snapshot();
        preflight_resolved_snapshot_against_runtime(&snapshot, &self.leased.runtime)?;
        Ok(snapshot)
    }

    fn resolved_snapshot_from(
        &self,
        runtime: &CanonicalPaneStateRuntime,
        topology_snapshot: &crate::daemon::topology::TopologySnapshot,
        hook_diagnostic: Option<&crate::daemon::protocol::v2::DaemonDiagnostic>,
        global_diagnostics: &VecDeque<crate::daemon::protocol::v2::DaemonDiagnostic>,
    ) -> crate::daemon::protocol::v2::ResolvedSnapshot {
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
        topology_snapshot: &crate::daemon::topology::TopologySnapshot,
        hook_diagnostic: Option<&crate::daemon::protocol::v2::DaemonDiagnostic>,
        global_diagnostics: &VecDeque<crate::daemon::protocol::v2::DaemonDiagnostic>,
        git_badges: &BTreeMap<String, GitBadge>,
        worktrees: &BTreeMap<String, WorktreeInfo>,
    ) -> crate::daemon::protocol::v2::ResolvedSnapshot {
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
    fn resolved_snapshot_with_git_at(
        &self,
        runtime: &CanonicalPaneStateRuntime,
        topology_snapshot: &crate::daemon::topology::TopologySnapshot,
        hook_diagnostic: Option<&crate::daemon::protocol::v2::DaemonDiagnostic>,
        global_diagnostics: &VecDeque<crate::daemon::protocol::v2::DaemonDiagnostic>,
        git_badges: &BTreeMap<String, GitBadge>,
        worktrees: &BTreeMap<String, WorktreeInfo>,
        now: i64,
    ) -> crate::daemon::protocol::v2::ResolvedSnapshot {
        use crate::daemon::protocol::v2::{
            AttentionEntry, DaemonDiagnostic, ErrorCode, PanePresentation, ResolvedSnapshot,
        };
        use crate::pane_state::{LifecycleState, ResolvedPaneState, StoredPaneRecord};

        let mut panes = Vec::with_capacity(topology_snapshot.panes.len());
        let mut attention = Vec::new();
        let visible_instances = self
            .views
            .clients()
            .values()
            .filter(|witness| witness.is_eligible())
            .map(|witness| witness.active_pane.clone())
            .collect::<BTreeSet<_>>();
        let triage = runtime
            .triage_entries()
            .map(|(pane, badge)| (pane.clone(), badge))
            .collect::<BTreeMap<_, _>>();
        for topology in &topology_snapshot.panes {
            let stored = runtime.descriptor(&topology.pane_instance);
            let record = runtime.record(&topology.pane_instance);
            let resolved = match record {
                Some(StoredPaneRecord::Active(state))
                    if state.agent_present || state.completed_seq > state.acknowledged_seq =>
                {
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
            if let Some(badge) = triage.get(&topology.pane_instance)
                && !visible_instances.contains(&topology.pane_instance)
            {
                let active = match record {
                    Some(StoredPaneRecord::Active(state)) => Some(state),
                    _ => None,
                };
                let reason = match active.map(|state| &state.lifecycle) {
                    Some(LifecycleState::Waiting {
                        reason: crate::pane_state::WaitReason::PermissionPrompt,
                    }) => Some("permission_prompt".to_string()),
                    Some(LifecycleState::Waiting {
                        reason: crate::pane_state::WaitReason::Other(_),
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
                session_links: topology.session_links.clone(),
                window_id: topology.window_id.clone(),
                window_name: topology.window_name.clone(),
                current_path: topology.current_path.clone(),
                current_command: topology.current_command.clone(),
                pane_width: topology.pane_width,
                active: topology.active,
                stored,
                resolved,
                diagnostic: runtime
                    .quarantined(&topology.pane_instance)
                    .map(|record| record.error.clone()),
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
                    pane_id: transition.pane_instance.pane_id.clone(),
                    agent: transition
                        .agent
                        .as_ref()
                        .map(|agent| agent.as_str().to_string())
                        .unwrap_or_default(),
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
        ResolvedSnapshot {
            snapshot_revision,
            panes,
            sidebar_model: SidebarModel {
                order: self.sidebar_order.clone(),
                expansion: self.sidebar_expansion.clone(),
                active_sessions: self
                    .views
                    .clients()
                    .values()
                    .filter(|witness| witness.is_eligible())
                    .map(|witness| witness.session_id.clone())
                    .collect(),
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

    pub fn pane_presentation(
        &self,
        pane_id: &str,
    ) -> Option<crate::daemon::protocol::v2::PanePresentation> {
        self.resolved_snapshot()
            .panes
            .into_iter()
            .find(|pane| pane.pane_instance.pane_id == pane_id)
    }

    pub fn status_snapshot(
        &self,
        context: crate::daemon::protocol::v2::StatusContext,
    ) -> crate::daemon::protocol::v2::StatusSnapshot {
        build_status_snapshot(
            &self.resolved_snapshot(),
            context,
            &self.status_metadata,
            &self.projection_config,
        )
    }

    pub fn display_projection(
        &self,
    ) -> (
        crate::daemon::protocol::v2::StatusSnapshot,
        Vec<crate::daemon::protocol::v2::StatusSnapshot>,
        Vec<crate::daemon::protocol::v2::PanePresentation>,
    ) {
        let resolved = self.resolved_snapshot();
        let global = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Global,
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
                    crate::daemon::protocol::v2::StatusContext::Session {
                        session_id: session_id.clone(),
                    },
                    &self.status_metadata,
                    &self.projection_config,
                )
            })
            .collect();
        (global, sessions, resolved.panes)
    }

    pub fn replace_sidebar_order_preferences(
        &mut self,
        order: SidebarOrderPreferences,
    ) -> Result<bool, crate::pane_state::StoreError> {
        order
            .validate()
            .map_err(crate::pane_state::StoreError::Random)?;
        if self.sidebar_order == order {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let previous = std::mem::replace(&mut self.sidebar_order, order);
        let snapshot = self.resolved_snapshot_from(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        );
        self.sidebar_order = previous;
        preflight_resolved_snapshot(&snapshot)?;
        self.leased.runtime = runtime;
        self.sidebar_order = snapshot.sidebar_model.order;
        Ok(true)
    }

    pub fn replace_sidebar_expansion_preferences(
        &mut self,
        expansion: crate::sidebar::state::SidebarExpansionPreferences,
    ) -> Result<bool, crate::pane_state::StoreError> {
        expansion
            .validate()
            .map_err(crate::pane_state::StoreError::Random)?;
        if self.sidebar_expansion == expansion {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let previous = std::mem::replace(&mut self.sidebar_expansion, expansion);
        let snapshot = self.resolved_snapshot_from(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        );
        if let Err(error) = preflight_resolved_snapshot(&snapshot) {
            self.sidebar_expansion = previous;
            return Err(error);
        }
        self.leased.runtime = runtime;
        Ok(true)
    }

    pub fn replace_status_metadata(
        &mut self,
        metadata: StatusProjectionMetadata,
    ) -> Result<bool, crate::pane_state::StoreError> {
        if self.status_metadata == metadata {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let resolved = self.resolved_snapshot_from(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        );
        preflight_resolved_snapshot(&resolved)?;
        let status = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Global,
            &metadata,
            &self.projection_config,
        );
        preflight_status_snapshot(&status)?;
        self.leased.runtime = runtime;
        self.status_metadata = metadata;
        Ok(true)
    }
}

#[derive(Debug, Clone)]
struct WindowStatusAggregate {
    window_name: String,
    links: BTreeMap<String, crate::daemon::protocol::v2::SessionLinkPresentation>,
    current_command: Option<String>,
}

pub(crate) fn build_status_snapshot(
    resolved: &crate::daemon::protocol::v2::ResolvedSnapshot,
    context: crate::daemon::protocol::v2::StatusContext,
    metadata: &StatusProjectionMetadata,
    config: &Config,
) -> crate::daemon::protocol::v2::StatusSnapshot {
    use crate::daemon::protocol::v2::{
        CategoryStatusPresentation, SessionStatusPresentation, StatusContext, StatusSnapshot,
        WindowStatusPresentation,
    };

    let mut pane_badges = BTreeMap::new();
    let mut session_names = BTreeMap::<String, String>::new();
    let mut session_panes = BTreeMap::<String, BTreeSet<crate::pane_state::PaneInstance>>::new();
    let mut window_panes = BTreeMap::<String, BTreeSet<crate::pane_state::PaneInstance>>::new();
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
            let session = crate::session::SessionInfo {
                name: projection
                    .map(|session| session.session_name.as_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or(topology_name)
                    .to_string(),
                category: projection
                    .and_then(|session| session.stored_category.clone())
                    .unwrap_or_default(),
                project_path: projection
                    .map(|session| session.project_path.clone())
                    .unwrap_or_default(),
                category_override: projection
                    .map(|session| session.category_override.clone())
                    .unwrap_or_default(),
                id: session_id.clone(),
                ..crate::session::SessionInfo::default()
            };
            (
                session_id.clone(),
                crate::category::resolve_category_for_session(config, &session),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let summary =
        crate::daemon::session_badge::BadgeStateCounts::from_states(pane_badges.values().copied());
    let counts_for = |panes: Option<&BTreeSet<crate::pane_state::PaneInstance>>| {
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

fn preflight_resolved_snapshot(
    snapshot: &crate::daemon::protocol::v2::ResolvedSnapshot,
) -> Result<(), crate::pane_state::StoreError> {
    for pane in &snapshot.panes {
        if pane.pane_width == 0 {
            return Err(crate::pane_state::StoreError::FailStop(format!(
                "projection invariant violated for {}: pane width must be positive",
                pane.pane_instance.pane_id
            )));
        }
        if let (
            Some(crate::pane_state::StoredStateDescriptor::Canonical { version }),
            Some(resolved),
        ) = (&pane.stored, &pane.resolved)
            && version != &resolved.canonical.version()
        {
            return Err(crate::pane_state::StoreError::Reduce(
                crate::pane_state::reducer::ReduceError::StateInvariantViolation(format!(
                    "projection invariant violated for {}: stored and resolved canonical versions differ",
                    pane.pane_instance.pane_id
                )),
            ));
        }
    }
    let message = crate::daemon::protocol::v2::ServerMessage::ResolvedSnapshotResult {
        snapshot_revision: snapshot.snapshot_revision,
        snapshot: snapshot.clone(),
    };
    let _bytes = serde_json::to_vec(&message)
        .map_err(|error| crate::pane_state::StoreError::Random(error.to_string()))?;
    Ok(())
}

fn preflight_resolved_snapshot_against_runtime(
    snapshot: &crate::daemon::protocol::v2::ResolvedSnapshot,
    runtime: &CanonicalPaneStateRuntime,
) -> Result<(), crate::pane_state::StoreError> {
    preflight_resolved_snapshot(snapshot)?;
    for pane in &snapshot.panes {
        match (&pane.stored, &pane.resolved) {
            (
                Some(crate::pane_state::StoredStateDescriptor::Canonical { version }),
                Some(resolved),
            ) if version == &resolved.canonical.version() => {}
            (Some(crate::pane_state::StoredStateDescriptor::Canonical { version }), None) => {
                let confirmed_ended = matches!(
                    runtime.record(&pane.pane_instance),
                    Some(crate::pane_state::StoredPaneRecord::Active(state))
                        if &state.version() == version
                            && !state.agent_present
                            && state.completed_seq == state.acknowledged_seq
                );
                if !confirmed_ended {
                    return Err(crate::pane_state::StoreError::FailStop(format!(
                        "projection invariant violated for {}: canonical state is unresolved without a confirmed agent end",
                        pane.pane_instance.pane_id
                    )));
                }
            }
            (Some(crate::pane_state::StoredStateDescriptor::Canonical { .. }), Some(_)) => {
                return Err(crate::pane_state::StoreError::FailStop(format!(
                    "projection invariant violated for {}: stored and resolved canonical versions differ",
                    pane.pane_instance.pane_id
                )));
            }
            (None | Some(crate::pane_state::StoredStateDescriptor::Reset { .. }), None)
            | (Some(crate::pane_state::StoredStateDescriptor::Quarantined { .. }), None) => {}
            (_, Some(_)) => {
                return Err(crate::pane_state::StoreError::FailStop(format!(
                    "projection invariant violated for {}: resolved state has no canonical storage",
                    pane.pane_instance.pane_id
                )));
            }
        }
    }
    Ok(())
}

fn preflight_status_snapshot(
    snapshot: &crate::daemon::protocol::v2::StatusSnapshot,
) -> Result<(), crate::pane_state::StoreError> {
    let message = crate::daemon::protocol::v2::ServerMessage::StatusSnapshotResult {
        snapshot_revision: snapshot.snapshot_revision,
        snapshot: snapshot.clone(),
    };
    let _bytes = serde_json::to_vec(&message)
        .map_err(|error| crate::pane_state::StoreError::Random(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DoneClearOn;
    use crate::daemon::session_badge::BadgeState;
    use crate::sidebar::state::SidebarState;
    use std::time::Duration;

    #[test]
    fn canonical_bootstrap_acquires_writer_lease_before_loading_state() {
        let root = std::env::temp_dir().join(format!(
            "vde-runtime-bootstrap-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let namespace = root.join("server");
        let first =
            LeasedCanonicalPaneStateRuntime::bootstrap(&namespace, || Ok(Vec::new())).unwrap();
        assert_eq!(first.runtime.snapshot_revision(), 0);
        let mut loader_called = false;
        let second = LeasedCanonicalPaneStateRuntime::bootstrap(&namespace, || {
            loader_called = true;
            Ok(Vec::new())
        });
        assert!(matches!(
            second,
            Err(crate::pane_state::StoreError::WriterLeaseHeld)
        ));
        assert!(!loader_called);
        drop(first);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hook_health_projection_changes_revision_and_active_diagnostic_once() {
        let root = std::env::temp_dir().join(format!(
            "vde-runtime-hook-health-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let leased = LeasedCanonicalPaneStateRuntime::acquire(&root.join("server")).unwrap();
        let mut state = CanonicalCoordinatorState::new(
            leased,
            crate::daemon::topology::TopologySnapshot {
                server_identity: crate::daemon::topology::ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                panes: Vec::new(),
            },
            crate::daemon::view_hooks::ViewRegistry::default(),
            SidebarOrderPreferences::default(),
        );

        assert!(
            state
                .set_hook_health(
                    crate::daemon::protocol::v2::HookHealth::Degraded,
                    Some("foreign hook".to_string()),
                )
                .unwrap()
        );
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);
        assert_eq!(state.resolved_snapshot().diagnostics.len(), 1);
        assert!(
            !state
                .set_hook_health(
                    crate::daemon::protocol::v2::HookHealth::Degraded,
                    Some("foreign hook".to_string()),
                )
                .unwrap()
        );
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);
        assert!(
            state
                .set_hook_health(crate::daemon::protocol::v2::HookHealth::Healthy, None)
                .unwrap()
        );
        assert_eq!(state.leased.runtime.snapshot_revision(), 2);
        assert!(state.resolved_snapshot().diagnostics.is_empty());

        state
            .leased
            .runtime
            .set_snapshot_revision_for_test(u64::MAX);
        let error = state
            .set_hook_health(
                crate::daemon::protocol::v2::HookHealth::Degraded,
                Some("must not publish".to_string()),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            crate::pane_state::StoreError::CounterOverflow(_)
        ));
        assert_eq!(
            state.hook_health,
            crate::daemon::protocol::v2::HookHealth::Healthy
        );
        assert!(state.hook_diagnostic.is_none());
        assert_eq!(state.leased.runtime.snapshot_revision(), u64::MAX);

        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn status_link(
        session_id: &str,
        session_name: &str,
        window_index: i64,
        window_active: bool,
    ) -> crate::daemon::protocol::v2::SessionLinkPresentation {
        crate::daemon::protocol::v2::SessionLinkPresentation {
            session_id: session_id.to_string(),
            session_name: session_name.to_string(),
            window_index,
            window_active,
            window_last: false,
        }
    }

    fn status_pane(
        pane_id: &str,
        pane_pid: u32,
        window_id: &str,
        window_name: &str,
        links: Vec<crate::daemon::protocol::v2::SessionLinkPresentation>,
        badge: Option<BadgeState>,
        active: bool,
    ) -> crate::daemon::protocol::v2::PanePresentation {
        let pane_instance = crate::pane_state::PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        };
        let resolved = badge.map(|badge| crate::pane_state::ResolvedPaneState {
            canonical: crate::pane_state::PaneState {
                schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
                state_id: crate::pane_state::StateId::parse(format!("{pane_pid:032x}")).unwrap(),
                revision: 1,
                pane_instance: pane_instance.clone(),
                agent: crate::pane_state::AgentKind::parse("codex").unwrap(),
                agent_session_id: None,
                agent_epoch: 1,
                agent_present: true,
                scan_verified: true,
                synthetic_completion_armed: false,
                lifecycle: crate::pane_state::LifecycleState::Idle,
                run_seq: 1,
                completed_seq: 1,
                acknowledged_seq: 1,
                started_at: Some(1),
                completed_at: Some(2),
                prompt: None,
                tasks: crate::pane_state::TaskState::default(),
                subagents: Vec::new(),
                worktree_activity: None,
            },
            window_id: window_id.to_string(),
            pane_id: pane_id.to_string(),
            current_path: "/tmp".to_string(),
            badge,
        });
        crate::daemon::protocol::v2::PanePresentation {
            pane_instance,
            session_links: links,
            window_id: window_id.to_string(),
            window_name: window_name.to_string(),
            current_path: "/tmp".to_string(),
            current_command: if resolved.is_some() { "codex" } else { "zsh" }.to_string(),
            pane_width: 80,
            active,
            stored: resolved.as_ref().map(|resolved| {
                crate::pane_state::StoredStateDescriptor::Canonical {
                    version: resolved.canonical.version(),
                }
            }),
            resolved,
            diagnostic: None,
        }
    }

    fn canonical_sidebar_fixture() -> (CanonicalCoordinatorState, std::path::PathBuf) {
        use crate::pane_state::{
            AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneInstance, PaneState,
            PromptState, RawPaneRecord, StateId, StoredPaneRecord, TaskState, WaitReason,
        };

        let root = std::env::temp_dir().join(format!(
            "vde-runtime-sidebar-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut leased = LeasedCanonicalPaneStateRuntime::acquire(&root.join("server")).unwrap();
        let active = [("%1", 101_u32), ("%2", 102_u32)]
            .into_iter()
            .map(|(pane_id, pane_pid)| {
                let pane_instance = PaneInstance {
                    pane_id: pane_id.to_string(),
                    pane_pid,
                };
                let state = PaneState {
                    schema_version: PANE_STATE_SCHEMA_VERSION,
                    state_id: StateId::parse(format!("{pane_pid:032x}")).unwrap(),
                    revision: 1,
                    pane_instance: pane_instance.clone(),
                    agent: AgentKind::parse("codex").unwrap(),
                    agent_session_id: None,
                    agent_epoch: 1,
                    agent_present: true,
                    scan_verified: true,
                    synthetic_completion_armed: false,
                    lifecycle: LifecycleState::Waiting {
                        reason: WaitReason::PermissionPrompt,
                    },
                    run_seq: 1,
                    completed_seq: 0,
                    acknowledged_seq: 0,
                    started_at: Some(1),
                    completed_at: None,
                    prompt: Some(PromptState {
                        text: format!("prompt for {pane_id}"),
                        source: "test".to_string(),
                    }),
                    tasks: TaskState::default(),
                    subagents: Vec::new(),
                    worktree_activity: None,
                };
                RawPaneRecord {
                    pane_instance,
                    raw: Some(
                        crate::pane_state::serialize_record(&StoredPaneRecord::Active(state))
                            .unwrap(),
                    ),
                }
            })
            .collect::<Vec<_>>();
        leased.hydrate(active);
        let topology_pane = |pane_id: &str,
                             pane_pid: u32,
                             window_id: &str,
                             session_id: &str,
                             session_name: &str,
                             path: &str| {
            crate::daemon::topology::TopologyPane {
                pane_instance: PaneInstance {
                    pane_id: pane_id.to_string(),
                    pane_pid,
                },
                session_links: vec![status_link(session_id, session_name, 0, true)],
                window_id: window_id.to_string(),
                window_name: window_id.to_string(),
                current_path: path.to_string(),
                current_command: if pane_id == "%shell" {
                    "zsh".to_string()
                } else {
                    "codex".to_string()
                },
                pane_width: 80,
                active: true,
            }
        };
        let topology = crate::daemon::topology::TopologySnapshot {
            server_identity: crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            },
            panes: vec![
                topology_pane("%1", 101, "@1", "$1", "main", "/tmp/alpha"),
                topology_pane("%2", 102, "@2", "$2", "other", "/tmp/beta"),
                topology_pane("%shell", 103, "@3", "$1", "main", "/tmp/shell"),
            ],
        };
        (
            CanonicalCoordinatorState::new(
                leased,
                topology,
                crate::daemon::view_hooks::ViewRegistry::default(),
                SidebarOrderPreferences::default(),
            ),
            root,
        )
    }

    fn remove_canonical_sidebar_fixture(
        state: CanonicalCoordinatorState,
        root: std::path::PathBuf,
    ) {
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_polling_paths_match_resolved_snapshot_resolved_filter() {
        let (state, root) = canonical_sidebar_fixture();
        let expected: std::collections::BTreeSet<String> = state
            .resolved_snapshot()
            .panes
            .into_iter()
            .filter(|pane| pane.resolved.is_some())
            .map(|pane| pane.current_path)
            .filter(|path| !path.trim().is_empty())
            .collect();
        assert_eq!(state.git_polling_paths(), expected);
        // The fixture has resolved agent panes (%1, %2) and a plain shell pane.
        assert!(expected.contains("/tmp/alpha"));
        assert!(expected.contains("/tmp/beta"));
        assert!(!expected.contains("/tmp/shell"));
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn contains_pane_matches_resolved_snapshot_membership() {
        let (state, root) = canonical_sidebar_fixture();
        assert!(state.contains_pane(&crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        }));
        assert!(!state.contains_pane(&crate::pane_state::PaneInstance {
            pane_id: "%missing".to_string(),
            pane_pid: 999,
        }));
        remove_canonical_sidebar_fixture(state, root);
    }

    struct ImmediatePaneStateIo;

    impl crate::pane_state::PaneStateStoreIo for ImmediatePaneStateIo {
        fn write_candidate(
            &mut self,
            _pane: &crate::pane_state::PaneInstance,
            candidate: &str,
        ) -> crate::pane_state::WriteAttempt {
            crate::pane_state::WriteAttempt::ReadBack(Some(candidate.to_string()))
        }

        fn read_independent(
            &mut self,
            _pane: &crate::pane_state::PaneInstance,
        ) -> crate::pane_state::IndependentRead {
            crate::pane_state::IndependentRead::Unavailable("unused".to_string())
        }
    }

    struct ZeroRecoveryClock;

    impl crate::pane_state::RecoveryClock for ZeroRecoveryClock {
        fn elapsed(&self) -> Duration {
            Duration::ZERO
        }
    }

    fn apply_waiting_state(
        state: &mut CanonicalCoordinatorState,
        pane_id: &str,
        pane_pid: u32,
        started_at: i64,
        reason: crate::pane_state::WaitReason,
    ) {
        use crate::pane_state::{
            AgentKind, AgentSessionId, DaemonInstanceId, EventId, PaneEvent, PaneEventEnvelope,
            PaneInstance, VisibilitySnapshot,
        };

        let pane_instance = PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        };
        let mut io = ImmediatePaneStateIo;
        let mut clock = ZeroRecoveryClock;
        for event in [
            PaneEvent::BeginRun {
                started_at,
                prompt: None,
            },
            PaneEvent::WaitRequested {
                observed_at: started_at.saturating_add(1),
                reason,
            },
        ] {
            state
                .leased
                .runtime
                .apply_event(
                    &mut io,
                    &mut clock,
                    &PaneEventEnvelope {
                        daemon_instance_id: DaemonInstanceId::generate().unwrap(),
                        event_id: EventId::generate().unwrap(),
                        pane_instance: pane_instance.clone(),
                        agent: Some(AgentKind::parse("codex").unwrap()),
                        agent_session_id: Some(AgentSessionId::parse("test-session").unwrap()),
                        event,
                    },
                    &VisibilitySnapshot::default(),
                    DoneClearOn::Pane,
                )
                .unwrap();
        }
    }

    fn apply_history_event(
        state: &mut CanonicalCoordinatorState,
        agent: &str,
        session: &str,
        event: crate::pane_state::PaneEvent,
    ) {
        use crate::pane_state::{
            AgentKind, AgentSessionId, DaemonInstanceId, EventId, PaneEventEnvelope, PaneInstance,
            VisibilitySnapshot,
        };

        state
            .leased
            .runtime
            .apply_event(
                &mut ImmediatePaneStateIo,
                &mut ZeroRecoveryClock,
                &PaneEventEnvelope {
                    daemon_instance_id: DaemonInstanceId::generate().unwrap(),
                    event_id: EventId::generate().unwrap(),
                    pane_instance: PaneInstance {
                        pane_id: "%1".to_string(),
                        pane_pid: 101,
                    },
                    agent: Some(AgentKind::parse(agent).unwrap()),
                    agent_session_id: Some(AgentSessionId::parse(session).unwrap()),
                    event,
                },
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
    }

    #[test]
    fn resolved_history_keeps_discarded_completion_under_the_old_agent_and_time() {
        use crate::pane_state::{AgentSessionSource, PaneEvent, StoredPaneRecord};

        let (mut state, root) = canonical_sidebar_fixture();
        state.leased.runtime = CanonicalPaneStateRuntime::default();
        apply_history_event(
            &mut state,
            "codex",
            "session-a",
            PaneEvent::BeginRun {
                started_at: 10,
                prompt: None,
            },
        );
        apply_history_event(
            &mut state,
            "codex",
            "session-a",
            PaneEvent::CompleteRun { completed_at: 20 },
        );
        apply_history_event(
            &mut state,
            "claude",
            "session-b",
            PaneEvent::AgentSessionStarted {
                observed_at: 30,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
        );

        let pane = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let Some(StoredPaneRecord::Active(current)) = state.leased.runtime.record(&pane) else {
            panic!("expected active pane state");
        };
        assert_eq!(current.agent.as_str(), "claude");
        let snapshot = state.resolved_snapshot();
        let discarded = snapshot
            .events
            .iter()
            .find(|event| event.from == Some(BadgeState::Done) && event.to == BadgeState::Idle)
            .unwrap();
        assert_eq!(discarded.agent, "codex");
        assert_eq!(discarded.at_epoch, 20);

        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn resolved_history_keeps_same_agent_previous_session_completion_time() {
        use crate::pane_state::{AgentSessionSource, PaneEvent, StoredPaneRecord};

        let (mut state, root) = canonical_sidebar_fixture();
        state.leased.runtime = CanonicalPaneStateRuntime::default();
        apply_history_event(
            &mut state,
            "codex",
            "session-a",
            PaneEvent::BeginRun {
                started_at: 10,
                prompt: None,
            },
        );
        apply_history_event(
            &mut state,
            "codex",
            "session-a",
            PaneEvent::CompleteRun { completed_at: 20 },
        );
        apply_history_event(
            &mut state,
            "codex",
            "session-b",
            PaneEvent::AgentSessionStarted {
                observed_at: 30,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
        );

        let pane = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let Some(StoredPaneRecord::Active(current)) = state.leased.runtime.record(&pane) else {
            panic!("expected active pane state");
        };
        assert_eq!(
            current
                .agent_session_id
                .as_ref()
                .map(crate::pane_state::AgentSessionId::as_str),
            Some("session-b")
        );
        let snapshot = state.resolved_snapshot();
        let discarded = snapshot
            .events
            .iter()
            .find(|event| event.from == Some(BadgeState::Done) && event.to == BadgeState::Idle)
            .unwrap();
        assert_eq!(discarded.agent, "codex");
        assert_eq!(discarded.at_epoch, 20);

        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn resolved_history_retains_the_latest_256_transitions() {
        use crate::pane_state::{AgentSessionSource, PaneEvent};

        let (mut state, root) = canonical_sidebar_fixture();
        state.leased.runtime = CanonicalPaneStateRuntime::default();
        for observed_at in 1..=257 {
            apply_history_event(
                &mut state,
                "codex",
                "same-session",
                PaneEvent::AgentSessionStarted {
                    observed_at,
                    source: AgentSessionSource::Startup,
                    resumed_prompt: None,
                },
            );
        }

        assert_eq!(state.leased.runtime.transitions().len(), 256);
        let snapshot = state.resolved_snapshot();
        assert_eq!(snapshot.events.len(), 256);
        assert_eq!(snapshot.events.first().unwrap().at_epoch, 2);
        assert_eq!(snapshot.events.last().unwrap().at_epoch, 257);

        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn canonical_git_projection_is_atomic_and_changes_revision_only_for_new_values() {
        let (mut state, root) = canonical_sidebar_fixture();
        let badges = BTreeMap::from([(
            "/tmp/alpha".to_string(),
            GitBadge {
                branch: "main".to_string(),
                ahead: 1,
                behind: 0,
            },
        )]);
        assert!(
            state
                .replace_git_projection(badges.clone(), BTreeMap::new())
                .unwrap()
        );
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);
        assert_eq!(state.resolved_snapshot().sidebar_model.git, badges);
        assert!(
            !state
                .replace_git_projection(badges.clone(), BTreeMap::new())
                .unwrap()
        );
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);

        let mut cache_only = badges.clone();
        cache_only.insert(
            "/tmp/no-longer-visible".to_string(),
            GitBadge {
                branch: "stale".to_string(),
                ahead: 0,
                behind: 0,
            },
        );
        assert!(
            state
                .replace_git_projection(cache_only.clone(), BTreeMap::new())
                .unwrap()
        );
        assert_eq!(state.git_badges, cache_only);
        assert_eq!(state.leased.runtime.snapshot_revision(), 2);

        let oversized = BTreeMap::from([(
            "/tmp/alpha".to_string(),
            GitBadge {
                branch: "x".repeat(crate::pane_state::MAX_RESPONSE_FRAME_BYTES),
                ahead: 0,
                behind: 0,
            },
        )]);
        assert!(
            state
                .replace_git_projection(oversized.clone(), BTreeMap::new())
                .unwrap()
        );
        assert_eq!(state.git_badges, oversized);
        assert_eq!(state.leased.runtime.snapshot_revision(), 3);
        let message = crate::daemon::protocol::v2::ServerMessage::ResolvedSnapshotResult {
            snapshot_revision: 3,
            snapshot: state.resolved_snapshot(),
        };
        assert!(
            serde_json::to_vec(&message).unwrap().len()
                > crate::pane_state::MAX_RESPONSE_FRAME_BYTES
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn canonical_attention_is_sorted_and_uses_full_pane_identity_for_visibility() {
        use crate::pane_state::{ClientWitness, PaneInstance, WaitReason};

        let (mut state, root) = canonical_sidebar_fixture();
        state.leased.runtime = CanonicalPaneStateRuntime::default();
        let second = state
            .topology
            .panes
            .iter_mut()
            .find(|pane| pane.pane_instance.pane_id == "%2")
            .unwrap();
        second.window_id = "@1".to_string();
        second.session_links = vec![status_link("$1", "main", 0, true)];
        let now = now_epoch_secs();
        apply_waiting_state(
            &mut state,
            "%1",
            101,
            now.saturating_sub(10),
            WaitReason::PermissionPrompt,
        );
        apply_waiting_state(
            &mut state,
            "%2",
            102,
            now.saturating_sub(100),
            WaitReason::Other("queue".to_string()),
        );

        let snapshot = state.resolved_snapshot();
        assert_eq!(snapshot.attention.len(), 2);
        assert_eq!(snapshot.attention[0].pane_instance.pane_id, "%2");
        assert_eq!(snapshot.attention[1].pane_instance.pane_id, "%1");
        assert_eq!(snapshot.attention[0].reason.as_deref(), Some("Other(wait)"));
        assert_eq!(
            snapshot.attention[1].reason.as_deref(),
            Some("permission_prompt")
        );
        assert!(snapshot.attention[0].elapsed_seconds >= 100);
        assert!(snapshot.attention[1].elapsed_seconds >= 10);
        assert!(snapshot.attention.iter().all(|entry| {
            entry.badge == BadgeState::Blocked
                && entry.elapsed_seconds >= 0
                && entry.reason.is_some()
        }));

        let stale = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 999,
        };
        state
            .views
            .reconcile(
                &[ClientWitness {
                    client_pid: 10,
                    session_id: "$1".to_string(),
                    window_id: "@1".to_string(),
                    active_pane: stale.clone(),
                    control_mode: false,
                    active_pane_flag: false,
                }],
                &BTreeMap::from([("@1".to_string(), vec![stale])]),
            )
            .unwrap();
        let stale_snapshot = state.resolved_snapshot();
        assert!(
            stale_snapshot
                .attention
                .iter()
                .any(|entry| entry.pane_instance.pane_id == "%1")
        );

        let current = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let non_focus_split = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        state
            .views
            .reconcile(
                &[ClientWitness {
                    client_pid: 10,
                    session_id: "$1".to_string(),
                    window_id: "@1".to_string(),
                    active_pane: current.clone(),
                    control_mode: false,
                    active_pane_flag: false,
                }],
                &BTreeMap::from([("@1".to_string(), vec![current, non_focus_split])]),
            )
            .unwrap();
        assert!(
            state
                .resolved_snapshot()
                .attention
                .iter()
                .all(|entry| entry.pane_instance.pane_id != "%1")
        );
        assert!(
            state
                .resolved_snapshot()
                .attention
                .iter()
                .any(|entry| entry.pane_instance.pane_id == "%2"),
            "a blocked non-focus split in the same window remains attention-worthy"
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn resolved_snapshot_tracks_the_session_of_each_eligible_client() {
        use crate::pane_state::ClientWitness;

        let (mut state, root) = canonical_sidebar_fixture();
        let pane = state.topology.panes[0].pane_instance.clone();
        let window_id = state.topology.panes[0].window_id.clone();
        let window_panes = BTreeMap::from([(window_id.clone(), vec![pane.clone()])]);
        let witness = |session_id: &str| ClientWitness {
            client_pid: 10,
            session_id: session_id.to_string(),
            window_id: window_id.clone(),
            active_pane: pane.clone(),
            control_mode: false,
            active_pane_flag: false,
        };

        state
            .views
            .reconcile(&[witness("$1")], &window_panes)
            .unwrap();
        assert_eq!(
            state.resolved_snapshot().sidebar_model.active_sessions,
            BTreeSet::from(["$1".to_string()])
        );

        state
            .views
            .reconcile(&[witness("$2")], &window_panes)
            .unwrap();
        assert_eq!(
            state.resolved_snapshot().sidebar_model.active_sessions,
            BTreeSet::from(["$2".to_string()])
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    fn status_resolved_snapshot() -> crate::daemon::protocol::v2::ResolvedSnapshot {
        let first = status_pane(
            "%1",
            101,
            "@1",
            "linked",
            vec![
                status_link("$1", "main", 0, true),
                status_link("$2", "mirror", 4, false),
            ],
            Some(BadgeState::Blocked),
            true,
        );
        let second = status_pane(
            "%2",
            102,
            "@2",
            "editor",
            vec![status_link("$1", "main", 1, false)],
            Some(BadgeState::Working),
            true,
        );
        let non_agent = status_pane(
            "%3",
            103,
            "@2",
            "editor",
            vec![status_link("$1", "main", 1, false)],
            None,
            false,
        );
        crate::daemon::protocol::v2::ResolvedSnapshot {
            snapshot_revision: 42,
            panes: vec![first.clone(), second, non_agent],
            sidebar_model: SidebarModel::default(),
            attention: vec![crate::daemon::protocol::v2::AttentionEntry {
                pane_instance: first.pane_instance,
                session_name: "main".to_string(),
                badge: BadgeState::Blocked,
                reason: Some("permission".to_string()),
                elapsed_seconds: 30,
            }],
            events: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn resolved_snapshot_preflight_requires_full_canonical_version_match() {
        let valid = status_resolved_snapshot();
        preflight_resolved_snapshot(&valid).unwrap();

        let mut zero_width = valid.clone();
        zero_width.panes[0].pane_width = 0;
        let error = preflight_resolved_snapshot(&zero_width).unwrap_err();
        assert!(error.to_string().contains("pane width must be positive"));

        for changed_field in ["state_id", "agent_epoch", "revision"] {
            let mut mismatched = valid.clone();
            let Some(crate::pane_state::StoredStateDescriptor::Canonical { version }) =
                mismatched.panes[0].stored.as_mut()
            else {
                panic!("fixture pane must have canonical stored state");
            };
            match changed_field {
                "state_id" => {
                    version.state_id =
                        crate::pane_state::StateId::parse("abcdefabcdefabcdefabcdefabcdefab")
                            .unwrap();
                }
                "agent_epoch" => version.agent_epoch += 1,
                "revision" => version.revision += 1,
                _ => unreachable!(),
            }
            let error = preflight_resolved_snapshot(&mismatched).unwrap_err();
            assert!(
                error.to_string().contains("canonical versions differ"),
                "changed field {changed_field} was accepted"
            );
        }
    }

    #[test]
    fn checked_snapshot_rejects_unresolved_present_canonical_state() {
        let (state, root) = canonical_sidebar_fixture();
        state.checked_resolved_snapshot().unwrap();
        let mut invalid = state.resolved_snapshot();
        invalid.panes[0].resolved = None;

        let error = preflight_resolved_snapshot_against_runtime(&invalid, &state.leased.runtime)
            .unwrap_err();

        assert!(error.requires_daemon_exit());
        assert!(error.to_string().contains("without a confirmed agent end"));
        remove_canonical_sidebar_fixture(state, root);
    }

    fn status_metadata() -> StatusProjectionMetadata {
        StatusProjectionMetadata {
            categories: BTreeSet::from(["empty".to_string(), "work".to_string()]),
            sessions: BTreeMap::from([
                (
                    "$1".to_string(),
                    SessionProjectionMetadata {
                        session_name: "main".to_string(),
                        stored_category: Some("work".to_string()),
                        attached: Some(true),
                        created_at: Some(10),
                        ..SessionProjectionMetadata::default()
                    },
                ),
                (
                    "$2".to_string(),
                    SessionProjectionMetadata {
                        session_name: "mirror".to_string(),
                        stored_category: Some("work".to_string()),
                        attached: Some(false),
                        created_at: Some(20),
                        ..SessionProjectionMetadata::default()
                    },
                ),
            ]),
            windows: BTreeMap::from([(
                "@1".to_string(),
                WindowProjectionMetadata {
                    bell: Some(true),
                    activity: Some(false),
                    silence: Some(true),
                },
            )]),
        }
    }

    #[test]
    fn display_projection_builds_every_surface_from_one_resolved_revision() {
        let (mut state, root) = canonical_sidebar_fixture();
        state.status_metadata = StatusProjectionMetadata {
            sessions: BTreeMap::from([
                ("$2".to_string(), SessionProjectionMetadata::default()),
                ("$1".to_string(), SessionProjectionMetadata::default()),
            ]),
            ..StatusProjectionMetadata::default()
        };

        let (global, sessions, panes) = state.display_projection();

        assert_eq!(global.snapshot_revision, 0);
        assert_eq!(panes.len(), 3);
        assert_eq!(
            sessions
                .iter()
                .map(|snapshot| match &snapshot.context {
                    crate::daemon::protocol::v2::StatusContext::Session { session_id } => {
                        session_id.as_str()
                    }
                    crate::daemon::protocol::v2::StatusContext::Global => "global",
                })
                .collect::<Vec<_>>(),
            vec!["$1", "$2"]
        );
        assert!(
            sessions
                .iter()
                .all(|snapshot| snapshot.snapshot_revision == global.snapshot_revision)
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    fn assert_continuous_display_state(state: &CanonicalCoordinatorState, expected: BadgeState) {
        let resolved = state.resolved_snapshot();
        let pane = resolved
            .panes
            .iter()
            .find(|pane| pane.pane_instance.pane_id == "%1")
            .expect("scenario pane is present");
        assert_eq!(
            pane.resolved.as_ref().map(|pane| pane.badge),
            Some(expected)
        );
        let projection = crate::sidebar::tree::project_sidebar(
            &state.projection_config,
            &resolved.panes,
            &resolved.sidebar_model,
            &SidebarState::default(),
            now_epoch_secs(),
        );
        assert_eq!(projection.counts.total, 1);
        assert_eq!(projection.counts.blocked, 0);
        assert_eq!(
            projection.counts.working,
            usize::from(expected == BadgeState::Working)
        );
        assert_eq!(
            projection.counts.done,
            usize::from(expected == BadgeState::Done)
        );
        assert_eq!(
            projection.counts.idle,
            usize::from(expected == BadgeState::Idle)
        );
        assert!(
            projection
                .rows
                .iter()
                .any(|row| row.badge_state == Some(expected))
        );
        assert!(resolved.attention.is_empty());

        let status = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Global,
            &state.status_metadata,
            &state.projection_config,
        );
        let expected_counts =
            crate::daemon::session_badge::BadgeStateCounts::from_states([expected]);
        assert_eq!(status.snapshot_revision, resolved.snapshot_revision);
        assert_eq!(status.summary, expected_counts);
        assert_eq!(
            status
                .sessions
                .iter()
                .find(|session| session.session_id == "$1")
                .expect("scenario session is projected")
                .counts,
            expected_counts
        );
        assert_eq!(
            status
                .windows
                .iter()
                .find(|window| window.window_id == "@1")
                .expect("scenario window is projected")
                .counts,
            expected_counts
        );
        assert_eq!(
            status
                .categories
                .iter()
                .find(|category| category.category == "work")
                .expect("scenario category is projected")
                .counts,
            expected_counts
        );
    }

    #[test]
    fn done_focus_idle_focus_out_next_completion_is_consistent_across_all_surfaces() {
        use crate::pane_state::{PaneEvent, StoredPaneRecord};

        let (mut state, root) = canonical_sidebar_fixture();
        state.leased.runtime = CanonicalPaneStateRuntime::default();
        state.status_metadata = StatusProjectionMetadata {
            categories: BTreeSet::from(["work".to_string()]),
            sessions: BTreeMap::from([(
                "$1".to_string(),
                SessionProjectionMetadata {
                    session_name: "main".to_string(),
                    stored_category: Some("work".to_string()),
                    attached: Some(true),
                    created_at: Some(1),
                    ..SessionProjectionMetadata::default()
                },
            )]),
            windows: BTreeMap::new(),
        };

        apply_history_event(
            &mut state,
            "codex",
            "scenario-session",
            PaneEvent::BeginRun {
                started_at: 10,
                prompt: None,
            },
        );
        assert_continuous_display_state(&state, BadgeState::Working);
        apply_history_event(
            &mut state,
            "codex",
            "scenario-session",
            PaneEvent::CompleteRun { completed_at: 20 },
        );
        assert_continuous_display_state(&state, BadgeState::Done);

        let pane = crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let (state_id, agent_epoch, through_seq) = match state.leased.runtime.record(&pane).unwrap()
        {
            StoredPaneRecord::Active(current) => (
                current.state_id.clone(),
                current.agent_epoch,
                current.completed_seq,
            ),
            other => panic!("expected active scenario state, got {other:?}"),
        };
        apply_history_event(
            &mut state,
            "codex",
            "scenario-session",
            PaneEvent::AcknowledgeView {
                expected_state_id: state_id,
                expected_agent_epoch: agent_epoch,
                through_seq,
            },
        );
        assert_continuous_display_state(&state, BadgeState::Idle);

        let focus_out_revision = state.resolved_snapshot().snapshot_revision;
        assert_continuous_display_state(&state, BadgeState::Idle);
        assert_eq!(
            state.resolved_snapshot().snapshot_revision,
            focus_out_revision
        );

        apply_history_event(
            &mut state,
            "codex",
            "scenario-session",
            PaneEvent::BeginRun {
                started_at: 30,
                prompt: None,
            },
        );
        assert_continuous_display_state(&state, BadgeState::Working);
        apply_history_event(
            &mut state,
            "codex",
            "scenario-session",
            PaneEvent::CompleteRun { completed_at: 40 },
        );
        assert_continuous_display_state(&state, BadgeState::Done);

        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn status_snapshot_deduplicates_linked_panes_for_every_scope() {
        let snapshot = build_status_snapshot(
            &status_resolved_snapshot(),
            crate::daemon::protocol::v2::StatusContext::Global,
            &status_metadata(),
            &Config::default(),
        );

        assert_eq!(snapshot.snapshot_revision, 42);
        assert_eq!(snapshot.summary.blocked, 1);
        assert_eq!(snapshot.summary.working, 1);
        assert_eq!(snapshot.summary.total(), 2);
        assert_eq!(snapshot.windows.len(), 2);
        let linked = snapshot
            .windows
            .iter()
            .find(|window| window.window_id == "@1")
            .unwrap();
        assert_eq!(linked.counts.blocked, 1);
        assert_eq!(linked.counts.total(), 1);
        assert_eq!(linked.session_ids, vec!["$1", "$2"]);
        assert_eq!(linked.window_index, None);
        assert_eq!(linked.bell, Some(true));
        assert_eq!(linked.activity, Some(false));
        assert_eq!(linked.silence, Some(true));
        let editor = snapshot
            .windows
            .iter()
            .find(|window| window.window_id == "@2")
            .unwrap();
        assert_eq!(editor.counts.working, 1);
        assert_eq!(editor.counts.total(), 1);

        let main = snapshot
            .sessions
            .iter()
            .find(|session| session.session_id == "$1")
            .unwrap();
        assert_eq!(main.counts.blocked, 1);
        assert_eq!(main.counts.working, 1);
        assert_eq!(main.counts.total(), 2);
        let mirror = snapshot
            .sessions
            .iter()
            .find(|session| session.session_id == "$2")
            .unwrap();
        assert_eq!(mirror.counts.blocked, 1);
        assert_eq!(mirror.counts.total(), 1);

        let work = snapshot
            .categories
            .iter()
            .find(|category| category.category == "work")
            .unwrap();
        assert_eq!(work.session_ids, vec!["$1", "$2"]);
        assert_eq!(work.counts.blocked, 1);
        assert_eq!(work.counts.working, 1);
        assert_eq!(work.counts.total(), 2);
        assert!(
            snapshot
                .categories
                .iter()
                .all(|category| category.category != "empty")
        );
        assert_eq!(snapshot.attention, status_resolved_snapshot().attention);
    }

    #[test]
    fn status_snapshot_orders_sessions_by_case_sensitive_unicode_name() {
        let mut resolved = status_resolved_snapshot();
        resolved.panes.clear();
        resolved.attention.clear();
        let names = ["a2", "日本", "a10", "a", "A"];
        let metadata = StatusProjectionMetadata {
            sessions: names
                .into_iter()
                .enumerate()
                .map(|(index, name)| {
                    (
                        format!("${}", index + 1),
                        SessionProjectionMetadata {
                            session_name: name.to_string(),
                            stored_category: Some("work".to_string()),
                            ..SessionProjectionMetadata::default()
                        },
                    )
                })
                .collect(),
            ..StatusProjectionMetadata::default()
        };

        let snapshot = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Global,
            &metadata,
            &Config::default(),
        );

        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|session| session.session_name.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "a", "a10", "a2", "日本"]
        );
    }

    #[test]
    fn effective_category_filters_current_sessions_and_omits_config_only_empty_category() {
        let mut resolved = status_resolved_snapshot();
        resolved.panes.clear();
        resolved.attention.clear();
        let mut config = Config::default();
        config
            .categories
            .order
            .insert("empty-config-only".to_string(), 0);
        config.categories.default_category = Some("misc".to_string());
        let metadata = StatusProjectionMetadata {
            categories: BTreeSet::from(["empty-config-only".to_string()]),
            sessions: BTreeMap::from([
                (
                    "$1".to_string(),
                    SessionProjectionMetadata {
                        session_name: "private".to_string(),
                        stored_category: Some("stale".to_string()),
                        project_path: "/repo".to_string(),
                        category_override: "private".to_string(),
                        ..SessionProjectionMetadata::default()
                    },
                ),
                (
                    "$2".to_string(),
                    SessionProjectionMetadata {
                        session_name: "uncategorized".to_string(),
                        ..SessionProjectionMetadata::default()
                    },
                ),
            ]),
            ..StatusProjectionMetadata::default()
        };

        let snapshot = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            &metadata,
            &config,
        );

        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].session_id, "$1");
        assert_eq!(snapshot.sessions[0].category.as_deref(), Some("private"));
        assert!(
            snapshot
                .categories
                .iter()
                .all(|category| category.category != "empty-config-only")
        );
        assert!(
            snapshot
                .categories
                .iter()
                .any(|category| { category.category == "private" && category.active })
        );
        assert!(
            snapshot
                .categories
                .iter()
                .any(|category| { category.category == "misc" && !category.active })
        );
    }

    #[test]
    fn fixed_session_zone_width_stabilizes_rendering_across_active_categories() {
        let mut resolved = status_resolved_snapshot();
        resolved.panes.clear();
        resolved.attention.clear();
        let metadata = StatusProjectionMetadata {
            sessions: BTreeMap::from([
                (
                    "$1".to_string(),
                    SessionProjectionMetadata {
                        session_name: "a".to_string(),
                        stored_category: Some("short".to_string()),
                        ..SessionProjectionMetadata::default()
                    },
                ),
                (
                    "$2".to_string(),
                    SessionProjectionMetadata {
                        session_name: "much-longer-session".to_string(),
                        stored_category: Some("long".to_string()),
                        ..SessionProjectionMetadata::default()
                    },
                ),
                (
                    "$3".to_string(),
                    SessionProjectionMetadata {
                        session_name: "peer".to_string(),
                        stored_category: Some("long".to_string()),
                        ..SessionProjectionMetadata::default()
                    },
                ),
            ]),
            ..StatusProjectionMetadata::default()
        };
        let mut config = Config::default();
        config.statusline.sessions.fixed_width = true;
        config.statusline.sessions.separator = " | ".to_string();

        let short = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            &metadata,
            &config,
        );
        let long = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$2".to_string(),
            },
            &metadata,
            &config,
        );

        assert_eq!(
            short
                .sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["$1"]
        );
        assert_eq!(
            long.sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["$2", "$3"]
        );
        assert_eq!(short.session_zone_width, long.session_zone_width);
        let short_rendered =
            crate::statusline::render_structured_status_snapshot(&config, &short).unwrap();
        let long_rendered =
            crate::statusline::render_structured_status_snapshot(&config, &long).unwrap();
        let expected_width = short.session_zone_width.unwrap();
        assert_eq!(
            crate::statusline::structured_status_display_width(&short_rendered.sessions),
            expected_width
        );
        assert_eq!(
            crate::statusline::structured_status_display_width(&long_rendered.sessions),
            expected_width
        );
    }

    #[test]
    fn session_status_context_filters_windows_and_marks_active_membership() {
        let snapshot = build_status_snapshot(
            &status_resolved_snapshot(),
            crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            &status_metadata(),
            &Config::default(),
        );

        assert_eq!(snapshot.snapshot_revision, 42);
        assert_eq!(snapshot.sessions.len(), 2);
        assert!(
            snapshot
                .sessions
                .iter()
                .find(|session| session.session_id == "$1")
                .unwrap()
                .active
        );
        assert_eq!(snapshot.windows.len(), 2);
        let linked = snapshot
            .windows
            .iter()
            .find(|window| window.window_id == "@1")
            .unwrap();
        assert_eq!(linked.session_ids, vec!["$1"]);
        assert_eq!(linked.window_index, Some(0));
        assert!(linked.active);
        assert!(
            snapshot
                .categories
                .iter()
                .find(|category| category.category == "work")
                .unwrap()
                .active
        );
    }

    #[test]
    fn missing_status_metadata_is_explicit_and_non_agent_panes_do_not_count() {
        let snapshot = build_status_snapshot(
            &status_resolved_snapshot(),
            crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            &StatusProjectionMetadata::default(),
            &Config::default(),
        );

        assert_eq!(snapshot.summary.total(), 2);
        assert!(snapshot.sessions.iter().all(|session| {
            session.category.as_deref() == Some("")
                && session.attached.is_none()
                && session.created_at.is_none()
        }));
        assert!(snapshot.windows.iter().all(|window| {
            window.bell.is_none() && window.activity.is_none() && window.silence.is_none()
        }));
        assert_eq!(snapshot.categories.len(), 1);
        assert_eq!(snapshot.categories[0].category, "");
    }
}

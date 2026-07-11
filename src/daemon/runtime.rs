use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::config::Config;
use crate::daemon::session_badge::BadgeState;
use crate::daemon::{SidebarFrame, TransitionEvent};
use crate::git::{GitBadge, WorktreeInfo};
pub use crate::pane_state::CanonicalStateRuntime as CanonicalPaneStateRuntime;
use crate::sidebar::input::{SidebarCommand, SidebarInputAction, activate_selected};
use crate::sidebar::state::{RepoId, SidebarAction, SidebarState};
use crate::sidebar::tree::{
    RowBuildContext, SidebarRow, SidebarRowKind, chat_row_id, now_epoch_secs, row_refs,
};

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
    pub category: Option<String>,
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
    JumpPane(String),
    PreviewPane { pane_id: String, history_lines: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CanonicalSidebarMutationResult {
    pub snapshot_revision: u64,
    pub state_changed: bool,
    pub effect: Option<CanonicalSidebarEffect>,
}

pub(crate) struct CanonicalCoordinatorState {
    pub leased: LeasedCanonicalPaneStateRuntime,
    pub topology: crate::daemon::topology::TopologySnapshot,
    pub views: crate::daemon::view_hooks::ViewRegistry,
    pub ui_state: SidebarState,
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
        ui_state: SidebarState,
    ) -> Self {
        Self {
            leased,
            topology,
            views,
            ui_state,
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
            &self.ui_state,
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
            &self.ui_state,
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
            &self.ui_state,
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
        let now = now_epoch_secs();
        let current = self.resolved_snapshot_with_git_at(
            &self.leased.runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
            &self.ui_state,
            &self.git_badges,
            &self.worktrees,
            now,
        );
        let candidate = self.resolved_snapshot_with_git_at(
            &self.leased.runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
            &self.ui_state,
            &git_badges,
            &worktrees,
            now,
        );
        if current.sidebar == candidate.sidebar {
            self.git_badges = git_badges;
            self.worktrees = worktrees;
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let snapshot = self.resolved_snapshot_with_git_at(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
            &self.ui_state,
            &git_badges,
            &worktrees,
            now,
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
            &self.ui_state,
        )
    }

    fn resolved_snapshot_from(
        &self,
        runtime: &CanonicalPaneStateRuntime,
        topology_snapshot: &crate::daemon::topology::TopologySnapshot,
        hook_diagnostic: Option<&crate::daemon::protocol::v2::DaemonDiagnostic>,
        global_diagnostics: &VecDeque<crate::daemon::protocol::v2::DaemonDiagnostic>,
        ui_state: &SidebarState,
    ) -> crate::daemon::protocol::v2::ResolvedSnapshot {
        self.resolved_snapshot_with_git_from(
            runtime,
            topology_snapshot,
            hook_diagnostic,
            global_diagnostics,
            ui_state,
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
        ui_state: &SidebarState,
        git_badges: &BTreeMap<String, GitBadge>,
        worktrees: &BTreeMap<String, WorktreeInfo>,
    ) -> crate::daemon::protocol::v2::ResolvedSnapshot {
        self.resolved_snapshot_with_git_at(
            runtime,
            topology_snapshot,
            hook_diagnostic,
            global_diagnostics,
            ui_state,
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
        ui_state: &SidebarState,
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
        let current_instances = topology_snapshot
            .panes
            .iter()
            .map(|pane| pane.pane_instance.clone())
            .collect::<BTreeSet<_>>();
        let visible_panes = visible_instances
            .intersection(&current_instances)
            .map(|pane| pane.pane_id.clone())
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
        let row_context = RowBuildContext {
            git: git_badges.clone(),
            worktrees: worktrees.clone(),
            triage: runtime
                .triage_panes()
                .map(|pane| pane.pane_id.clone())
                .collect(),
            flash: runtime
                .flashing_panes()
                .map(|pane| pane.pane_id.clone())
                .collect(),
            now,
        };
        let (rows, counts) = crate::sidebar::tree::build_rows_from_presentations(
            &self.projection_config,
            &panes,
            ui_state,
            &row_context,
            &visible_panes,
        );
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
            sidebar: SidebarFrame {
                state: ui_state.clone(),
                counts,
                rows,
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
        build_status_snapshot(&self.resolved_snapshot(), context, &self.status_metadata)
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
                )
            })
            .collect();
        (global, sessions, resolved.panes)
    }

    pub fn apply_sidebar_key(
        &mut self,
        key: &str,
    ) -> Result<CanonicalSidebarMutationResult, crate::pane_state::StoreError> {
        let Some(action) = crate::sidebar::input::parse_key(key) else {
            return Ok(self.sidebar_mutation_result(false, None));
        };
        let snapshot = self.resolved_snapshot();
        let rows = &snapshot.sidebar.rows;
        let row_refs = row_refs(rows);
        let mut ui_state = self.ui_state.clone();
        let effect = match action {
            SidebarInputAction::MoveNext => {
                ui_state.apply(SidebarAction::MoveNext, &row_refs);
                None
            }
            SidebarInputAction::MovePrevious => {
                ui_state.apply(SidebarAction::MovePrevious, &row_refs);
                None
            }
            SidebarInputAction::ToggleExpand => {
                ui_state.apply(SidebarAction::ToggleExpand, &row_refs);
                None
            }
            SidebarInputAction::SetViewMode(view_mode) => {
                ui_state.apply(SidebarAction::SetViewMode(view_mode), &row_refs);
                None
            }
            SidebarInputAction::CycleViewMode => {
                ui_state.apply(SidebarAction::CycleViewMode, &row_refs);
                None
            }
            SidebarInputAction::SetFilter(filter) => {
                if snapshot.sidebar.counts.filter_is_available(filter) {
                    ui_state.set_filter(filter);
                }
                None
            }
            SidebarInputAction::ToggleFilter => {
                let filter = next_available_filter(ui_state.filter, snapshot.sidebar.counts);
                ui_state.set_filter(filter);
                None
            }
            SidebarInputAction::ToggleRow(row_id) => {
                if let Some(rest) = row_id
                    .strip_prefix("detail::")
                    .or_else(|| row_id.strip_prefix("meta::"))
                {
                    let pane_id = rest
                        .split_once("::")
                        .map(|(pane_id, _)| pane_id)
                        .unwrap_or(rest);
                    let chat_id = chat_row_id(pane_id);
                    ui_state.selection = Some(chat_id.clone());
                    ui_state.toggle_expanded(&chat_id);
                } else {
                    ui_state.selection = Some(row_id.clone());
                    ui_state.toggle_expanded(&row_id);
                }
                None
            }
            SidebarInputAction::FocusNextAttention => {
                Self::focus_sidebar_attention(&mut ui_state, rows, true);
                None
            }
            SidebarInputAction::FocusPreviousAttention => {
                Self::focus_sidebar_attention(&mut ui_state, rows, false);
                None
            }
            SidebarInputAction::ReorderUp => {
                Self::reorder_sidebar(&mut ui_state, rows, true);
                None
            }
            SidebarInputAction::ReorderDown => {
                Self::reorder_sidebar(&mut ui_state, rows, false);
                None
            }
            SidebarInputAction::Activate => {
                match activate_selected(ui_state.selection.as_deref(), rows) {
                    Some(SidebarCommand::JumpPane(pane_id)) => {
                        Some(CanonicalSidebarEffect::JumpPane(pane_id))
                    }
                    Some(SidebarCommand::ToggleExpand(row_id)) => {
                        ui_state.selection = Some(row_id);
                        ui_state.apply(SidebarAction::ToggleExpand, &row_refs);
                        None
                    }
                    Some(SidebarCommand::PreviewPane(pane_id)) => {
                        Some(CanonicalSidebarEffect::PreviewPane {
                            pane_id,
                            history_lines: self.projection_config.sidebar.preview.history_lines,
                        })
                    }
                    None => None,
                }
            }
        };
        self.commit_sidebar_ui(ui_state, effect)
    }

    pub fn select_sidebar_context(
        &mut self,
        pane_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<CanonicalSidebarMutationResult, crate::pane_state::StoreError> {
        if pane_id.is_none() && session_id.is_none() {
            return Ok(self.sidebar_mutation_result(false, None));
        }
        let snapshot = self.resolved_snapshot();
        let mut expanded_ui = snapshot.sidebar.state.clone();
        expanded_ui.collapsed.clear();
        let expanded = self.resolved_snapshot_from(
            &self.leased.runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
            &expanded_ui,
        );
        let selection = pane_id
            .and_then(|pane_id| Self::selection_for_sidebar_pane(&snapshot, &expanded, pane_id))
            .or_else(|| {
                let session_id = session_id?;
                snapshot
                    .panes
                    .iter()
                    .filter(|pane| {
                        pane.resolved.is_some()
                            && pane
                                .session_links
                                .iter()
                                .any(|link| link.session_id == session_id)
                    })
                    .find_map(|pane| {
                        Self::selection_for_sidebar_pane(
                            &snapshot,
                            &expanded,
                            &pane.pane_instance.pane_id,
                        )
                    })
            });
        let Some(selection) = selection else {
            return Ok(self.sidebar_mutation_result(false, None));
        };
        if self.ui_state.selection.as_deref() == Some(selection.as_str()) {
            return Ok(self.sidebar_mutation_result(false, None));
        }
        let mut ui_state = self.ui_state.clone();
        ui_state.selection = Some(selection);
        ui_state.version = ui_state.version.checked_add(1).ok_or(
            crate::pane_state::StoreError::CounterOverflow("sidebar state version"),
        )?;
        self.commit_sidebar_ui(ui_state, None)
    }

    fn focus_sidebar_attention(ui_state: &mut SidebarState, rows: &[SidebarRow], forward: bool) {
        let blocked = rows
            .iter()
            .filter(|row| {
                row.kind == SidebarRowKind::Chat && row.badge_state == Some(BadgeState::Blocked)
            })
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        if blocked.is_empty() {
            return;
        }
        let current = ui_state
            .selection
            .as_deref()
            .and_then(|id| blocked.iter().position(|blocked_id| *blocked_id == id));
        let next = match (current, forward) {
            (None, true) => 0,
            (None, false) => blocked.len() - 1,
            (Some(index), true) => (index + 1) % blocked.len(),
            (Some(index), false) => (index + blocked.len() - 1) % blocked.len(),
        };
        let next_id = blocked[next].to_string();
        if ui_state.selection.as_deref() != Some(next_id.as_str()) {
            ui_state.selection = Some(next_id);
            ui_state.version += 1;
        }
    }

    fn reorder_sidebar(ui_state: &mut SidebarState, rows: &[SidebarRow], up: bool) {
        let selection = ui_state.selection.as_deref();
        let selected = selection.and_then(|selection| rows.iter().find(|row| row.id == selection));
        if let Some(pane_id) = selected
            .filter(|row| row.kind == SidebarRowKind::Chat)
            .and_then(|row| row.pane_id.clone())
        {
            for pane_id in rows
                .iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .filter_map(|row| row.pane_id.as_deref())
            {
                if !ui_state
                    .manual_chat_order
                    .iter()
                    .any(|existing| existing == pane_id)
                {
                    ui_state.manual_chat_order.push(pane_id.to_string());
                    ui_state.version += 1;
                }
            }
            if up {
                ui_state.manual_chat_move_up(&pane_id);
            } else {
                ui_state.manual_chat_move_down(&pane_id);
            }
            return;
        }
        let Some(repo) = selected
            .filter(|row| row.kind == SidebarRowKind::Repo)
            .and_then(|row| RepoId::from_row_id(&row.id))
        else {
            return;
        };
        for repo in rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Repo)
            .filter_map(|row| RepoId::from_row_id(&row.id))
        {
            if !ui_state.manual_order.contains(&repo) {
                ui_state.manual_order.push(repo);
                ui_state.version += 1;
            }
        }
        if up {
            ui_state.apply(SidebarAction::ReorderUp(repo), &[]);
        } else {
            ui_state.apply(SidebarAction::ReorderDown(repo), &[]);
        }
    }

    fn selection_for_sidebar_pane(
        snapshot: &crate::daemon::protocol::v2::ResolvedSnapshot,
        expanded: &crate::daemon::protocol::v2::ResolvedSnapshot,
        pane_id: &str,
    ) -> Option<String> {
        snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_instance.pane_id == pane_id && pane.resolved.is_some())?;
        if let Some(row) =
            snapshot.sidebar.rows.iter().find(|row| {
                row.kind == SidebarRowKind::Chat && row.pane_id.as_deref() == Some(pane_id)
            })
        {
            return Some(row.id.clone());
        }
        let chat_index = expanded.sidebar.rows.iter().position(|row| {
            row.kind == SidebarRowKind::Chat && row.pane_id.as_deref() == Some(pane_id)
        })?;
        let mut depth = expanded.sidebar.rows[chat_index].depth;
        for row in expanded.sidebar.rows[..chat_index].iter().rev() {
            if row.depth >= depth {
                continue;
            }
            depth = row.depth;
            if snapshot
                .sidebar
                .rows
                .iter()
                .any(|visible| visible.id == row.id)
            {
                return Some(row.id.clone());
            }
        }
        None
    }

    fn commit_sidebar_ui(
        &mut self,
        ui_state: SidebarState,
        effect: Option<CanonicalSidebarEffect>,
    ) -> Result<CanonicalSidebarMutationResult, crate::pane_state::StoreError> {
        if ui_state == self.ui_state {
            return Ok(self.sidebar_mutation_result(false, effect));
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let snapshot = self.resolved_snapshot_from(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
            &ui_state,
        );
        preflight_resolved_snapshot(&snapshot)?;
        self.leased.runtime = runtime;
        self.ui_state = ui_state;
        Ok(self.sidebar_mutation_result(true, effect))
    }

    fn sidebar_mutation_result(
        &self,
        state_changed: bool,
        effect: Option<CanonicalSidebarEffect>,
    ) -> CanonicalSidebarMutationResult {
        CanonicalSidebarMutationResult {
            snapshot_revision: self.leased.runtime.snapshot_revision(),
            state_changed,
            effect,
        }
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
            &self.ui_state,
        );
        preflight_resolved_snapshot(&resolved)?;
        let status = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Global,
            &metadata,
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

    for session_id in metadata.sessions.keys() {
        session_names.entry(session_id.clone()).or_default();
        session_panes.entry(session_id.clone()).or_default();
    }

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
    let active_category = active_session_id
        .and_then(|session_id| metadata.sessions.get(session_id))
        .and_then(|session| session.category.as_deref());

    let mut sessions = session_names
        .into_iter()
        .filter(|(session_id, _)| match active_category {
            Some(category) => {
                metadata
                    .sessions
                    .get(session_id)
                    .and_then(|session| session.category.as_deref())
                    == Some(category)
            }
            None => true,
        })
        .map(|(session_id, session_name)| {
            let session_metadata = metadata.sessions.get(&session_id);
            SessionStatusPresentation {
                counts: counts_for(session_panes.get(&session_id)),
                active: active_session_id == Some(session_id.as_str()),
                category: session_metadata.and_then(|session| session.category.clone()),
                attached: session_metadata.and_then(|session| session.attached),
                created_at: session_metadata.and_then(|session| session.created_at),
                session_id,
                session_name,
            }
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        left.session_name
            .cmp(&right.session_name)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });

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

    let mut category_names = metadata.categories.clone();
    category_names.extend(
        metadata
            .sessions
            .values()
            .filter_map(|session| session.category.clone()),
    );
    let mut categories = category_names
        .into_iter()
        .map(|category| {
            let mut category_panes = BTreeSet::new();
            let mut session_ids = metadata
                .sessions
                .iter()
                .filter_map(|(session_id, session)| {
                    (session.category.as_deref() == Some(category.as_str()))
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
    categories.sort_by(|left, right| left.category.cmp(&right.category));

    StatusSnapshot {
        snapshot_revision: resolved.snapshot_revision,
        context,
        summary,
        sessions,
        windows: window_presentations,
        categories,
        attention: resolved.attention.clone(),
    }
}

fn preflight_resolved_snapshot(
    snapshot: &crate::daemon::protocol::v2::ResolvedSnapshot,
) -> Result<(), crate::pane_state::StoreError> {
    let message = crate::daemon::protocol::v2::ServerMessage::ResolvedSnapshotResult {
        snapshot_revision: snapshot.snapshot_revision,
        snapshot: snapshot.clone(),
    };
    let _bytes = serde_json::to_vec(&message)
        .map_err(|error| crate::pane_state::StoreError::Random(error.to_string()))?;
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

fn next_available_filter(
    current: crate::sidebar::state::StatusFilter,
    counts: crate::sidebar::tree::BadgeCounts,
) -> crate::sidebar::state::StatusFilter {
    let mut next = current.next();
    while !counts.filter_is_available(next) {
        next = next.next();
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DoneClearOn;
    use crate::sidebar::state::SidebarState;
    use crate::sidebar::tree::BadgeCounts;
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
            SidebarState::default(),
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

    fn canonical_sidebar_fixture(
        ui_state: SidebarState,
    ) -> (CanonicalCoordinatorState, std::path::PathBuf) {
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
                ui_state,
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

        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
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

        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
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

        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
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
        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
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
        assert!(state.resolved_snapshot().sidebar.rows.iter().any(|row| {
            row.git
                .as_ref()
                .is_some_and(|git| git.branch == "main" && git.ahead == 1)
        }));
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
            !state
                .replace_git_projection(cache_only.clone(), BTreeMap::new())
                .unwrap()
        );
        assert_eq!(state.git_badges, cache_only);
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);

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
        assert_eq!(state.leased.runtime.snapshot_revision(), 2);
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn canonical_attention_is_sorted_and_uses_full_pane_identity_for_visibility() {
        use crate::pane_state::{ClientWitness, PaneInstance, WaitReason};

        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
        state.leased.runtime = CanonicalPaneStateRuntime::default();
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
        assert!(
            stale_snapshot
                .sidebar
                .rows
                .iter()
                .all(|row| { row.pane_id.as_deref() != Some("%1") || !row.active })
        );

        let current = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
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
                &BTreeMap::from([("@1".to_string(), vec![current])]),
            )
            .unwrap();
        assert!(
            state
                .resolved_snapshot()
                .attention
                .iter()
                .all(|entry| entry.pane_instance.pane_id != "%1")
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn canonical_sidebar_key_changes_state_and_revision_once_while_noop_is_stable() {
        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
        let result = state.apply_sidebar_key("j").unwrap();
        assert!(result.state_changed);
        assert_eq!(result.snapshot_revision, 1);
        assert_eq!(result.effect, None);
        assert_eq!(state.resolved_snapshot().snapshot_revision, 1);
        assert!(state.ui_state.selection.is_some());

        let before = state.ui_state.clone();
        let result = state.apply_sidebar_key("unknown").unwrap();
        assert!(!result.state_changed);
        assert_eq!(result.snapshot_revision, 1);
        assert_eq!(state.ui_state, before);
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn canonical_sidebar_activate_returns_typed_jump_and_preview_without_revision_change() {
        let jump_ui = SidebarState {
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        let (mut jump, jump_root) = canonical_sidebar_fixture(jump_ui);
        let result = jump.apply_sidebar_key("enter").unwrap();
        assert_eq!(
            result.effect,
            Some(CanonicalSidebarEffect::JumpPane("%1".to_string()))
        );
        assert!(!result.state_changed);
        assert_eq!(result.snapshot_revision, 0);
        remove_canonical_sidebar_fixture(jump, jump_root);

        let preview_ui = SidebarState {
            selection: Some("detail::%1::prompt".to_string()),
            collapsed: BTreeSet::from(["chat::%1".to_string()]),
            ..SidebarState::default()
        };
        let (mut preview, preview_root) = canonical_sidebar_fixture(preview_ui);
        let result = preview.apply_sidebar_key("enter").unwrap();
        assert_eq!(
            result.effect,
            Some(CanonicalSidebarEffect::PreviewPane {
                pane_id: "%1".to_string(),
                history_lines: preview.projection_config.sidebar.preview.history_lines,
            })
        );
        assert!(!result.state_changed);
        assert_eq!(result.snapshot_revision, 0);
        remove_canonical_sidebar_fixture(preview, preview_root);
    }

    #[test]
    fn canonical_sidebar_attention_and_reorder_use_resolved_sidebar_rows() {
        let (mut attention, attention_root) = canonical_sidebar_fixture(SidebarState::default());
        attention.apply_sidebar_key("n").unwrap();
        assert_eq!(attention.ui_state.selection.as_deref(), Some("chat::%1"));
        attention.apply_sidebar_key("n").unwrap();
        assert_eq!(attention.ui_state.selection.as_deref(), Some("chat::%2"));
        assert_eq!(attention.leased.runtime.snapshot_revision(), 2);
        remove_canonical_sidebar_fixture(attention, attention_root);

        let reorder_ui = SidebarState {
            selection: Some("repo::misc::beta".to_string()),
            ..SidebarState::default()
        };
        let (mut reorder, reorder_root) = canonical_sidebar_fixture(reorder_ui);
        let result = reorder.apply_sidebar_key("K").unwrap();
        assert!(result.state_changed);
        assert_eq!(result.snapshot_revision, 1);
        assert_eq!(
            reorder.ui_state.manual_order,
            vec![RepoId::new("misc", "beta"), RepoId::new("misc", "alpha")]
        );
        remove_canonical_sidebar_fixture(reorder, reorder_root);
    }

    #[test]
    fn canonical_select_context_uses_pane_and_structured_session_ids() {
        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
        let result = state
            .select_sidebar_context(Some("%2"), Some("$1"))
            .unwrap();
        assert!(result.state_changed);
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%2"));

        let result = state
            .select_sidebar_context(Some("%shell"), Some("$1"))
            .unwrap();
        assert!(result.state_changed);
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));

        let before_revision = state.leased.runtime.snapshot_revision();
        let result = state
            .select_sidebar_context(Some("%shell"), Some("main"))
            .unwrap();
        assert!(!result.state_changed);
        assert_eq!(result.snapshot_revision, before_revision);
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn canonical_select_context_uses_visible_group_for_collapsed_chat() {
        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
        state.apply_sidebar_key("toggle:repo::misc::alpha").unwrap();
        state.apply_sidebar_key("j").unwrap();
        assert!(
            !state
                .resolved_snapshot()
                .sidebar
                .rows
                .iter()
                .any(|row| row.id == "chat::%1")
        );
        let result = state.select_sidebar_context(Some("%1"), None).unwrap();
        assert!(result.state_changed);
        assert_eq!(
            state.ui_state.selection.as_deref(),
            Some("repo::misc::alpha")
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn oversized_sidebar_projection_commits_for_typed_frame_too_large_publish() {
        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
        let key = format!(
            "toggle:{}",
            "x".repeat(crate::pane_state::MAX_RESPONSE_FRAME_BYTES)
        );
        let result = state.apply_sidebar_key(&key).unwrap();
        assert!(result.state_changed);
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);
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
            sidebar: SidebarFrame {
                state: SidebarState::default(),
                counts: BadgeCounts::default(),
                rows: Vec::new(),
            },
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

    fn status_metadata() -> StatusProjectionMetadata {
        StatusProjectionMetadata {
            categories: BTreeSet::from(["empty".to_string(), "work".to_string()]),
            sessions: BTreeMap::from([
                (
                    "$1".to_string(),
                    SessionProjectionMetadata {
                        category: Some("work".to_string()),
                        attached: Some(true),
                        created_at: Some(10),
                    },
                ),
                (
                    "$2".to_string(),
                    SessionProjectionMetadata {
                        category: Some("work".to_string()),
                        attached: Some(false),
                        created_at: Some(20),
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
        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
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
        assert_eq!(resolved.sidebar.counts.total, 1);
        assert_eq!(resolved.sidebar.counts.blocked, 0);
        assert_eq!(
            resolved.sidebar.counts.working,
            usize::from(expected == BadgeState::Working)
        );
        assert_eq!(
            resolved.sidebar.counts.done,
            usize::from(expected == BadgeState::Done)
        );
        assert_eq!(
            resolved.sidebar.counts.idle,
            usize::from(expected == BadgeState::Idle)
        );
        assert!(
            resolved
                .sidebar
                .rows
                .iter()
                .any(|row| row.badge_state == Some(expected))
        );
        assert!(resolved.attention.is_empty());

        let status = build_status_snapshot(
            &resolved,
            crate::daemon::protocol::v2::StatusContext::Global,
            &state.status_metadata,
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

        let (mut state, root) = canonical_sidebar_fixture(SidebarState::default());
        state.leased.runtime = CanonicalPaneStateRuntime::default();
        state.status_metadata = StatusProjectionMetadata {
            categories: BTreeSet::from(["work".to_string()]),
            sessions: BTreeMap::from([(
                "$1".to_string(),
                SessionProjectionMetadata {
                    category: Some("work".to_string()),
                    attached: Some(true),
                    created_at: Some(1),
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
        let empty = snapshot
            .categories
            .iter()
            .find(|category| category.category == "empty")
            .unwrap();
        assert_eq!(empty.counts.total(), 0);
        assert_eq!(snapshot.attention, status_resolved_snapshot().attention);
    }

    #[test]
    fn session_status_context_filters_windows_and_marks_active_membership() {
        let snapshot = build_status_snapshot(
            &status_resolved_snapshot(),
            crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            &status_metadata(),
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
        );

        assert_eq!(snapshot.summary.total(), 2);
        assert!(snapshot.sessions.iter().all(|session| {
            session.category.is_none() && session.attached.is_none() && session.created_at.is_none()
        }));
        assert!(snapshot.windows.iter().all(|window| {
            window.bell.is_none() && window.activity.is_none() && window.silence.is_none()
        }));
        assert!(snapshot.categories.is_empty());
    }
}

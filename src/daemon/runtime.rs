use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::config::Config;
use crate::daemon::protocol::v2::{DaemonDiagnostic, ErrorCode, HookHealth, StatusContext};
use crate::daemon::topology::TopologySnapshot;
use crate::git::{GitBadge, WorktreeInfo};
pub use crate::pane_state::CanonicalStateRuntime as CanonicalPaneStateRuntime;
use crate::pane_state::{PaneInstance, PaneState, StoreError};
use crate::sidebar::state::{SidebarIntentDedupe, SidebarNavigation, SidebarPreferences};
use crate::sidebar::tree::now_epoch_secs;

mod peek;
mod projection;
mod queries;

pub(crate) use peek::PeekLease;
pub(crate) use projection::build_status_snapshot;
use projection::{preflight_resolved_snapshot, preflight_status_snapshot};

pub(crate) struct LeasedCanonicalPaneStateRuntime {
    pub runtime: CanonicalPaneStateRuntime,
    _writer_lease: crate::daemon::lifecycle::DaemonFileLock,
}

impl LeasedCanonicalPaneStateRuntime {
    pub fn acquire(namespace: &std::path::Path) -> Result<Self, StoreError> {
        let lease = crate::daemon::lifecycle::try_acquire_writer_lease(namespace)
            .map_err(|error| StoreError::PersistFailed(error.to_string()))?
            .ok_or(StoreError::WriterLeaseHeld)?;
        Ok(Self {
            runtime: CanonicalPaneStateRuntime::default(),
            _writer_lease: lease,
        })
    }

    pub fn hydrate(
        &mut self,
        entries: BTreeMap<PaneInstance, PaneState>,
    ) -> Result<(), StoreError> {
        self.runtime = CanonicalPaneStateRuntime::hydrate(entries)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn bootstrap(
        namespace: &std::path::Path,
        load_after_lease: impl FnOnce() -> Result<BTreeMap<PaneInstance, PaneState>, StoreError>,
    ) -> Result<Self, StoreError> {
        let mut leased = Self::acquire(namespace)?;
        let entries = load_after_lease()?;
        leased.hydrate(entries)?;
        Ok(leased)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StatusProjectionMetadata {
    pub sessions: BTreeMap<String, SessionProjectionMetadata>,
    pub windows: BTreeMap<String, WindowProjectionMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SessionProjectionMetadata {
    pub session_name: String,
    pub project_path: String,
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
        pane_instance: PaneInstance,
        client_pid: u32,
        source_pane: PaneInstance,
    },
    JumpLatestUnread {
        candidates: Vec<PaneInstance>,
        client_pid: u32,
        source_pane: PaneInstance,
    },
    PeekPane {
        pane_instance: PaneInstance,
        client_pid: u32,
        source_pane: PaneInstance,
    },
    ReadPeekAdvance {
        candidates: Vec<PaneInstance>,
        client_pid: u32,
        source_pane: PaneInstance,
        read_outcome: crate::daemon::protocol::v2::PaneApplyOutcome,
    },
}

pub(crate) struct CanonicalCoordinatorState {
    pub leased: LeasedCanonicalPaneStateRuntime,
    pub topology: TopologySnapshot,
    topology_observation_floor: u64,
    pub views: crate::daemon::view_hooks::CurrentClientViews,
    pub sidebar_preferences: SidebarPreferences,
    pub sidebar_navigation: SidebarNavigation,
    pub sidebar_intent_dedupe: SidebarIntentDedupe,
    pub peek_leases: BTreeMap<u32, PeekLease>,
    pub hook_health: HookHealth,
    pub hook_diagnostic: Option<DaemonDiagnostic>,
    pub global_diagnostics: VecDeque<DaemonDiagnostic>,
    pub status_metadata: StatusProjectionMetadata,
    pub git_badges: BTreeMap<String, GitBadge>,
    pub worktrees: BTreeMap<String, WorktreeInfo>,
    pub repo_identities: BTreeMap<String, crate::category::RepoIdentity>,
    pub category_state: crate::category::CategoryState,
    pub projection_config: Config,
}

impl CanonicalCoordinatorState {
    pub fn new(
        leased: LeasedCanonicalPaneStateRuntime,
        topology: TopologySnapshot,
        views: crate::daemon::view_hooks::CurrentClientViews,
        sidebar_preferences: SidebarPreferences,
    ) -> Self {
        Self {
            leased,
            topology,
            topology_observation_floor: 0,
            views,
            sidebar_preferences,
            sidebar_navigation: SidebarNavigation::default(),
            sidebar_intent_dedupe: SidebarIntentDedupe::default(),
            peek_leases: BTreeMap::new(),
            hook_health: HookHealth::Healthy,
            hook_diagnostic: None,
            global_diagnostics: VecDeque::new(),
            status_metadata: StatusProjectionMetadata::default(),
            git_badges: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            repo_identities: BTreeMap::new(),
            category_state: crate::category::CategoryState::default(),
            projection_config: Config::default(),
        }
    }

    pub fn set_hook_health(
        &mut self,
        health: HookHealth,
        diagnostic: Option<String>,
    ) -> Result<bool, StoreError> {
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
        code: ErrorCode,
        message: String,
    ) -> Result<u64, StoreError> {
        let mut diagnostics = self.global_diagnostics.clone();
        diagnostics.push_back(DaemonDiagnostic {
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
    ) -> Result<bool, StoreError> {
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

    pub fn records_snapshot(&self) -> BTreeMap<PaneInstance, PaneState> {
        self.leased.runtime.records_snapshot()
    }

    pub fn replace_topology(&mut self, topology: TopologySnapshot) -> Result<bool, StoreError> {
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
        let present = self
            .topology
            .panes
            .iter()
            .map(|pane| pane.pane_instance.clone())
            .collect::<BTreeSet<_>>();
        self.peek_leases.retain(|_, lease| match lease {
            PeekLease::Active { target, .. } => present.contains(target),
            PeekLease::Pending {
                previous_target,
                candidates,
                ..
            } => {
                candidates.retain(|candidate| present.contains(candidate));
                if previous_target
                    .as_ref()
                    .is_some_and(|target| !present.contains(target))
                {
                    *previous_target = None;
                }
                !candidates.is_empty() || previous_target.is_some()
            }
        });
        Ok(true)
    }

    pub fn apply_topology_observation(
        &mut self,
        topology: TopologySnapshot,
        observation_seq: u64,
    ) -> Result<bool, StoreError> {
        debug_assert!(observation_seq > 0);
        if observation_seq <= self.topology_observation_floor {
            return Ok(false);
        }
        self.replace_topology(topology)?;
        self.topology_observation_floor = observation_seq;
        Ok(true)
    }

    pub fn replace_topology_and_fence_observations(
        &mut self,
        topology: TopologySnapshot,
        observation_floor: u64,
    ) -> Result<bool, StoreError> {
        let changed = self.replace_topology(topology)?;
        self.topology_observation_floor = self.topology_observation_floor.max(observation_floor);
        Ok(changed)
    }

    #[cfg(test)]
    pub fn replace_git_projection(
        &mut self,
        git_badges: BTreeMap<String, GitBadge>,
        worktrees: BTreeMap<String, WorktreeInfo>,
    ) -> Result<bool, StoreError> {
        self.replace_git_projection_with_identities(
            git_badges,
            worktrees,
            self.repo_identities.clone(),
        )
    }

    pub fn replace_git_projection_with_identities(
        &mut self,
        git_badges: BTreeMap<String, GitBadge>,
        worktrees: BTreeMap<String, WorktreeInfo>,
        repo_identities: BTreeMap<String, crate::category::RepoIdentity>,
    ) -> Result<bool, StoreError> {
        if self.git_badges == git_badges
            && self.worktrees == worktrees
            && self.repo_identities == repo_identities
        {
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
        self.repo_identities = repo_identities;
        Ok(true)
    }

    pub fn replace_category_state(
        &mut self,
        category_state: crate::category::CategoryState,
    ) -> Result<bool, StoreError> {
        category_state.validate().map_err(StoreError::Random)?;
        if self.category_state == category_state {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        self.leased.runtime = runtime;
        self.category_state = category_state;
        Ok(true)
    }

    pub fn effective_category_model(&self) -> crate::category::EffectiveCategoryModel {
        let repos = self
            .repo_identities
            .values()
            .map(|repo| (repo.key.clone(), repo.clone()))
            .collect::<BTreeMap<_, _>>()
            .into_values();
        crate::category::EffectiveCategoryModel::build(
            &self.projection_config,
            &self.category_state,
            repos,
        )
        .expect("validated config and category state must build an effective model")
    }

    pub fn replace_sidebar_preferences(
        &mut self,
        preferences: SidebarPreferences,
    ) -> Result<bool, StoreError> {
        preferences.validate().map_err(StoreError::Random)?;
        if self.sidebar_preferences == preferences {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let previous = std::mem::replace(&mut self.sidebar_preferences, preferences);
        let snapshot = self.resolved_snapshot_from(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        );
        self.sidebar_preferences = previous;
        preflight_resolved_snapshot(&snapshot)?;
        self.leased.runtime = runtime;
        self.sidebar_preferences = snapshot.sidebar_model.preferences;
        Ok(true)
    }

    pub fn replace_sidebar_navigation(
        &mut self,
        selection: Option<String>,
        scroll: usize,
        manual_scroll: bool,
    ) -> Result<bool, StoreError> {
        if self.sidebar_navigation.selection == selection
            && self.sidebar_navigation.scroll == scroll
            && self.sidebar_navigation.manual_scroll == manual_scroll
        {
            return Ok(false);
        }
        let mut runtime = self.leased.runtime.clone();
        runtime.mark_projection_changed()?;
        let mut navigation = self.sidebar_navigation.clone();
        navigation.revision = navigation.revision.saturating_add(1);
        navigation.selection = selection;
        navigation.scroll = scroll;
        navigation.manual_scroll = manual_scroll;
        let previous = std::mem::replace(&mut self.sidebar_navigation, navigation);
        let snapshot = self.resolved_snapshot_from(
            &runtime,
            &self.topology,
            self.hook_diagnostic.as_ref(),
            &self.global_diagnostics,
        );
        self.sidebar_navigation = previous;
        preflight_resolved_snapshot(&snapshot)?;
        self.leased.runtime = runtime;
        self.sidebar_navigation = snapshot.sidebar_model.navigation;
        Ok(true)
    }

    pub fn replace_status_metadata(
        &mut self,
        metadata: StatusProjectionMetadata,
    ) -> Result<bool, StoreError> {
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
            StatusContext::Global,
            &metadata,
            &self.projection_config,
        );
        preflight_status_snapshot(&status)?;
        self.leased.runtime = runtime;
        self.status_metadata = metadata;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::v2::AttentionEntry;
    use crate::daemon::session_badge::BadgeState;
    use crate::daemon::topology::ServerIdentity;
    use crate::pane_state::{EventId, PaneEvent};
    use crate::sidebar::state::SidebarState;

    use super::projection::preflight_resolved_snapshot_against_runtime;
    use crate::daemon::SidebarModel;
    use crate::daemon::protocol::v2::{
        PanePresentation, ResolvedSnapshot, ServerMessage, SessionLinkPresentation,
    };
    use crate::pane_state::{ClientWitness, StoredStateDescriptor, WaitReason};

    #[test]
    fn canonical_bootstrap_acquires_writer_lease_before_loading_state() {
        let root = std::env::temp_dir().join(format!(
            "vde-runtime-bootstrap-{}-{}",
            std::process::id(),
            EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let namespace = root.join("server");
        let first =
            LeasedCanonicalPaneStateRuntime::bootstrap(&namespace, || Ok(BTreeMap::new())).unwrap();
        assert_eq!(first.runtime.snapshot_revision(), 0);
        let mut loader_called = false;
        let second = LeasedCanonicalPaneStateRuntime::bootstrap(&namespace, || {
            loader_called = true;
            Ok(BTreeMap::new())
        });
        assert!(matches!(second, Err(StoreError::WriterLeaseHeld)));
        assert!(!loader_called);
        drop(first);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hook_health_projection_changes_revision_and_active_diagnostic_once() {
        let root = std::env::temp_dir().join(format!(
            "vde-runtime-hook-health-{}-{}",
            std::process::id(),
            EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let leased = LeasedCanonicalPaneStateRuntime::acquire(&root.join("server")).unwrap();
        let mut state = CanonicalCoordinatorState::new(
            leased,
            TopologySnapshot {
                server_identity: ServerIdentity {
                    pid: 1,
                    start_time: 2,
                },
                panes: Vec::new(),
            },
            crate::daemon::view_hooks::CurrentClientViews::default(),
            SidebarPreferences::default(),
        );

        assert!(
            state
                .set_hook_health(HookHealth::Degraded, Some("foreign hook".to_string()),)
                .unwrap()
        );
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);
        assert_eq!(state.resolved_snapshot().diagnostics.len(), 1);
        assert!(
            !state
                .set_hook_health(HookHealth::Degraded, Some("foreign hook".to_string()),)
                .unwrap()
        );
        assert_eq!(state.leased.runtime.snapshot_revision(), 1);
        assert!(state.set_hook_health(HookHealth::Healthy, None).unwrap());
        assert_eq!(state.leased.runtime.snapshot_revision(), 2);
        assert!(state.resolved_snapshot().diagnostics.is_empty());

        state
            .leased
            .runtime
            .set_snapshot_revision_for_test(u64::MAX);
        let error = state
            .set_hook_health(HookHealth::Degraded, Some("must not publish".to_string()))
            .unwrap_err();
        assert!(matches!(error, StoreError::CounterOverflow(_)));
        assert_eq!(state.hook_health, HookHealth::Healthy);
        assert!(state.hook_diagnostic.is_none());
        assert_eq!(state.leased.runtime.snapshot_revision(), u64::MAX);

        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn latest_unread_candidates_are_global_newest_first_and_drop_read_panes() {
        let (mut state, root) = canonical_sidebar_fixture();
        assert_eq!(
            state.latest_unread_candidates(),
            vec![
                PaneInstance {
                    pane_id: "%2".to_string(),
                    pane_pid: 102,
                },
                PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
            ]
        );

        apply_history_event(
            &mut state,
            "codex",
            "test-session",
            PaneEvent::MarkPaneRead { through_order: 102 },
        );
        assert_eq!(
            state.latest_unread_candidates(),
            vec![PaneInstance {
                pane_id: "%2".to_string(),
                pane_pid: 102,
            }]
        );

        remove_canonical_sidebar_fixture(state, root);
    }

    fn status_link(
        session_id: &str,
        session_name: &str,
        window_index: i64,
        window_active: bool,
    ) -> SessionLinkPresentation {
        SessionLinkPresentation {
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
        links: Vec<SessionLinkPresentation>,
        badge: Option<BadgeState>,
        active: bool,
    ) -> PanePresentation {
        let pane_instance = PaneInstance {
            pane_id: pane_id.to_string(),
            pane_pid,
        };
        let resolved = badge.map(|badge| crate::pane_state::ResolvedPaneState {
            canonical: PaneState {
                schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
                state_id: crate::pane_state::StateId::parse(format!("{pane_pid:032x}")).unwrap(),
                revision: 1,
                pane_instance: pane_instance.clone(),
                agent: crate::pane_state::AgentKind::parse("codex").unwrap(),
                agent_session_id: None,
                agent_process: None,
                agent_epoch: 1,
                agent_present: true,
                scan_verified: true,
                synthetic_completion_armed: false,
                lifecycle: crate::pane_state::LifecycleState::Idle,
                run_seq: 1,
                current_run: None,
                completed_seq: 1,
                unread: crate::pane_state::UnreadState::default(),
                started_at: Some(1),
                completed_at: Some(2),
                prompt: None,
                latest_response: None,
                task_context: crate::pane_state::TaskContextState::default(),
                tasks: crate::pane_state::TaskState::default(),
                subagents: Vec::new(),
                worktree_activity: None,
                background_process: None,
                listening_ports: Vec::new(),
            },
            window_id: window_id.to_string(),
            pane_id: pane_id.to_string(),
            current_path: "/tmp".to_string(),
            badge,
        });
        PanePresentation {
            pane_instance,
            session_links: links,
            window_id: window_id.to_string(),
            window_name: window_name.to_string(),
            current_path: "/tmp".to_string(),
            current_command: if resolved.is_some() { "codex" } else { "zsh" }.to_string(),
            pane_width: 80,
            active,
            focused: active,
            agent_process: None,
            stored: resolved
                .as_ref()
                .map(|resolved| StoredStateDescriptor::Canonical {
                    version: resolved.canonical.version(),
                }),
            resolved,
            retained_state: None,
        }
    }

    fn canonical_sidebar_fixture() -> (CanonicalCoordinatorState, std::path::PathBuf) {
        use crate::pane_state::{
            AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneInstance, PaneState,
            PromptState, StateId, TaskState, WaitReason,
        };

        let root = std::env::temp_dir().join(format!(
            "vde-runtime-sidebar-{}-{}",
            std::process::id(),
            EventId::generate().unwrap().as_str()
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
                    agent_process: None,
                    agent_epoch: 1,
                    agent_present: true,
                    scan_verified: true,
                    synthetic_completion_armed: false,
                    lifecycle: LifecycleState::Waiting {
                        reason: WaitReason::PermissionPrompt,
                    },
                    run_seq: 1,
                    current_run: None,
                    completed_seq: 0,
                    unread: crate::pane_state::UnreadState {
                        occurrence_seq: 1,
                        read_seq: 0,
                        latest: Some(crate::pane_state::UnreadOccurrence {
                            seq: 1,
                            order: u64::from(pane_pid),
                            reason: crate::pane_state::UnreadReason::Waiting,
                            occurred_at: 1,
                        }),
                    },
                    started_at: Some(1),
                    completed_at: None,
                    prompt: Some(PromptState {
                        text: format!("prompt for {pane_id}"),
                        source: "test".to_string(),
                        digest: None,
                    }),
                    latest_response: None,
                    task_context: crate::pane_state::TaskContextState::default(),
                    tasks: TaskState::default(),
                    subagents: Vec::new(),
                    worktree_activity: None,
                    background_process: None,
                    listening_ports: Vec::new(),
                };
                (pane_instance, state)
            })
            .collect::<BTreeMap<_, _>>();
        leased.hydrate(active).unwrap();
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
                editprompt_is_editor: false,
                editprompt_target_panes: Vec::new(),
                editprompt_editor_pane: None,
            }
        };
        let topology = TopologySnapshot {
            server_identity: ServerIdentity {
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
                crate::daemon::view_hooks::CurrentClientViews::default(),
                SidebarPreferences::default(),
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
    fn absent_usage_limited_state_is_retained_but_not_resolved() {
        let (mut state, root) = canonical_sidebar_fixture();
        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let mut records = state.leased.runtime.records_snapshot();
        let limited = records.get_mut(&pane).unwrap();
        limited.agent_present = false;
        limited.scan_verified = true;
        limited.lifecycle = crate::pane_state::LifecycleState::Waiting {
            reason: crate::pane_state::WaitReason::usage_limit(),
        };
        limited.unread = crate::pane_state::UnreadState::default();
        state.leased.hydrate(records).unwrap();

        let snapshot = state.resolved_snapshot();
        let retained = snapshot
            .panes
            .iter()
            .find(|candidate| candidate.pane_instance == pane)
            .unwrap();
        assert!(retained.resolved.is_none());
        assert!(retained.retained_state.is_some());
        assert!(!state.git_polling_paths().contains("/tmp/alpha"));

        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn contains_pane_matches_resolved_snapshot_membership() {
        let (state, root) = canonical_sidebar_fixture();
        assert!(state.contains_pane(&PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        }));
        assert!(!state.contains_pane(&PaneInstance {
            pane_id: "%missing".to_string(),
            pane_pid: 999,
        }));
        remove_canonical_sidebar_fixture(state, root);
    }

    fn client_witness(client_pid: u32, pane_id: &str, pane_pid: u32) -> ClientWitness {
        ClientWitness {
            client_pid,
            session_id: "$1".to_string(),
            window_id: "@1".to_string(),
            active_pane: PaneInstance {
                pane_id: pane_id.to_string(),
                pane_pid,
            },
            control_mode: false,
            active_pane_flag: false,
        }
    }

    #[test]
    fn active_peek_suppresses_only_the_owning_clients_read_authority() {
        let (mut state, root) = canonical_sidebar_fixture();
        let first = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let second = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        state.begin_peek(10, first, [second.clone()], 1);
        state.activate_peek(10, 1, second.clone(), 0);

        let owner = client_witness(10, "%2", 102);
        state.reconcile_peek_leases(std::slice::from_ref(&owner), 1);
        assert!(
            !state
                .read_authorized_panes(std::slice::from_ref(&owner))
                .contains(&second)
        );

        let observer = client_witness(20, "%2", 102);
        state.reconcile_peek_leases(&[owner.clone(), observer.clone()], 2);
        assert!(
            state
                .read_authorized_panes(&[owner.clone(), observer.clone()])
                .contains(&second)
        );

        state.begin_peek(20, second.clone(), [second.clone()], 2);
        state.activate_peek(20, 2, second.clone(), 0);
        state.reconcile_peek_leases(&[owner.clone(), observer.clone()], 3);
        assert!(
            !state
                .read_authorized_panes(&[owner.clone(), observer.clone()])
                .contains(&second)
        );

        let mut control = client_witness(30, "%2", 102);
        control.control_mode = true;
        let mut active_pane = client_witness(40, "%2", 102);
        active_pane.active_pane_flag = true;
        assert!(
            !state
                .read_authorized_panes(&[owner.clone(), observer.clone(), control, active_pane])
                .contains(&second)
        );

        let normal = client_witness(50, "%2", 102);
        assert!(
            state
                .read_authorized_panes(&[owner, observer, normal])
                .contains(&second)
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn focus_proxy_uses_the_same_client_scoped_peek_read_authority() {
        let (mut state, root) = canonical_sidebar_fixture();
        let target = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        let editor = state
            .topology
            .panes
            .iter_mut()
            .find(|pane| pane.pane_instance.pane_id == "%shell")
            .unwrap();
        editor.editprompt_is_editor = true;
        editor.editprompt_target_panes = vec![target.pane_id.clone()];
        state
            .topology
            .panes
            .iter_mut()
            .find(|pane| pane.pane_instance == target)
            .unwrap()
            .editprompt_editor_pane = Some("%shell".to_string());

        let proxy_owner = client_witness(10, "%shell", 103);
        state.begin_peek(10, proxy_owner.active_pane.clone(), [target.clone()], 1);
        state.activate_peek(10, 1, target.clone(), 0);
        assert!(
            !state
                .read_authorized_panes(std::slice::from_ref(&proxy_owner))
                .contains(&target)
        );

        let proxy_observer = client_witness(20, "%shell", 103);
        assert!(
            state
                .read_authorized_panes(&[proxy_owner, proxy_observer])
                .contains(&target)
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn pending_peek_protects_previous_and_candidate_until_completion() {
        let (mut state, root) = canonical_sidebar_fixture();
        let first = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let second = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        state.begin_peek(10, first.clone(), [first.clone()], 1);
        state.activate_peek(10, 1, first.clone(), 0);
        state.begin_peek(10, first.clone(), [second.clone()], 2);

        state.reconcile_peek_leases(&[client_witness(10, "%shell", 103)], 1);
        assert!(matches!(
            state.peek_leases.get(&10),
            Some(PeekLease::Pending {
                operation_seq: 2,
                ..
            })
        ));
        let owner = client_witness(10, "%1", 101);
        state.reconcile_peek_leases(std::slice::from_ref(&owner), 2);
        let authorized = state.read_authorized_panes(std::slice::from_ref(&owner));
        assert!(!authorized.contains(&first));
        let landed = client_witness(10, "%2", 102);
        state.reconcile_peek_leases(std::slice::from_ref(&landed), 3);
        assert!(!state.read_authorized_panes(&[landed]).contains(&second));

        state
            .topology
            .panes
            .retain(|pane| pane.pane_instance != second);
        state.reconcile_peek_leases(std::slice::from_ref(&owner), 4);
        assert!(matches!(
            state.peek_leases.get(&10),
            Some(PeekLease::Pending {
                previous_target: Some(previous),
                candidates,
                ..
            }) if previous == &first && candidates.is_empty()
        ));

        state.restore_peek_after_failure(10, 2, &[owner], 5);
        assert_eq!(state.active_peek_target(10), Some(&first));
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn pending_peek_cannot_be_overwritten_by_a_concurrent_operation() {
        let (mut state, root) = canonical_sidebar_fixture();
        let source = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let first_target = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        let second_target = PaneInstance {
            pane_id: "%shell".to_string(),
            pane_pid: 103,
        };

        assert!(state.begin_peek(10, source.clone(), [first_target.clone()], 1));
        assert!(!state.begin_peek(10, source, [second_target], 2));
        assert!(matches!(
            state.peek_leases.get(&10),
            Some(PeekLease::Pending {
                operation_seq: 1,
                candidates,
                ..
            }) if candidates == &BTreeSet::from([first_target])
        ));
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn failed_peek_restores_previous_target_only_with_a_matching_fresh_witness() {
        let (mut state, root) = canonical_sidebar_fixture();
        let previous = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let candidate = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };

        assert!(state.begin_peek(10, previous.clone(), [previous.clone()], 1));
        state.activate_peek(10, 1, previous.clone(), 0);
        assert!(state.begin_peek(10, previous.clone(), [candidate.clone()], 2));
        state.restore_peek_after_failure(10, 2, &[client_witness(10, "%shell", 103)], 1);
        assert!(state.active_peek_target(10).is_none());

        assert!(state.begin_peek(10, previous.clone(), [previous.clone()], 3));
        state.activate_peek(10, 3, previous.clone(), 0);
        assert!(state.begin_peek(10, previous.clone(), [candidate], 4));
        state.restore_peek_after_failure(10, 4, &[client_witness(10, "%1", 101)], 2);
        assert_eq!(state.active_peek_target(10), Some(&previous));
        state.reconcile_peek_leases(&[client_witness(10, "%shell", 103)], 1);
        assert_eq!(state.active_peek_target(10), Some(&previous));
        state.reconcile_peek_leases(&[client_witness(10, "%shell", 103)], 3);
        assert!(state.active_peek_target(10).is_none());
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn renewing_an_active_peek_starts_a_new_observation_interval() {
        let (mut state, root) = canonical_sidebar_fixture();
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        assert!(state.begin_peek(10, target.clone(), [target.clone()], 1));
        state.activate_peek(10, 1, target.clone(), 3);

        assert!(state.renew_active_peek(10, &target, 7));
        state.reconcile_peek_leases(&[client_witness(10, "%shell", 103)], 6);
        assert_eq!(state.active_peek_target(10), Some(&target));

        state.reconcile_peek_leases(&[client_witness(10, "%shell", 103)], 8);
        assert!(state.active_peek_target(10).is_none());
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn authoritative_topology_refresh_fences_older_observation_projections() {
        let (mut state, root) = canonical_sidebar_fixture();
        let stale = state.topology.clone();
        let mut refreshed = stale.clone();
        let mut created = refreshed.panes[0].clone();
        created.pane_instance = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 109,
        };
        created.window_id = "@9".to_string();
        refreshed.panes.push(created.clone());
        refreshed
            .panes
            .sort_by(|left, right| left.pane_instance.cmp(&right.pane_instance));

        state
            .replace_topology_and_fence_observations(refreshed, 5)
            .unwrap();
        assert!(state.contains_pane(&created.pane_instance));
        assert!(!state.apply_topology_observation(stale.clone(), 4).unwrap());
        assert!(state.contains_pane(&created.pane_instance));
        assert!(!state.apply_topology_observation(stale.clone(), 5).unwrap());
        assert!(state.contains_pane(&created.pane_instance));

        assert!(state.apply_topology_observation(stale, 6).unwrap());
        assert!(!state.contains_pane(&created.pane_instance));
        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn manual_focus_change_and_detach_end_the_peek_lease() {
        let (mut state, root) = canonical_sidebar_fixture();
        let first = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let second = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        state.begin_peek(10, first, [second.clone()], 1);
        state.activate_peek(10, 1, second, 1);
        state.reconcile_peek_leases(&[client_witness(10, "%shell", 103)], 2);
        assert!(state.active_peek_target(10).is_none());

        let first = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let second = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        state.begin_peek(10, first.clone(), [second.clone()], 2);
        state.activate_peek(10, 2, second, 5);
        let source = client_witness(10, "%1", 101);
        state.reconcile_peek_leases(std::slice::from_ref(&source), 5);
        assert!(state.active_peek_target(10).is_some());
        state.reconcile_peek_leases(std::slice::from_ref(&source), 5);
        assert!(state.active_peek_target(10).is_some());
        let landed = client_witness(10, "%2", 102);
        state.reconcile_peek_leases(std::slice::from_ref(&landed), 6);
        assert!(state.active_peek_target(10).is_some());
        state.reconcile_peek_leases(std::slice::from_ref(&source), 5);
        assert!(state.active_peek_target(10).is_some());
        state.reconcile_peek_leases(std::slice::from_ref(&source), 7);
        assert!(state.active_peek_target(10).is_none());

        let first = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        state.begin_peek(10, first.clone(), [first.clone()], 3);
        state.activate_peek(10, 3, first, 0);
        state.reconcile_peek_leases(&[], 1);
        assert!(state.active_peek_target(10).is_none());

        let first = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let second = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 102,
        };
        state.begin_peek(10, first, [second.clone()], 4);
        state.activate_peek(10, 4, second.clone(), 0);
        state.clear_peeks_for_read_panes(&BTreeSet::from([second]));
        assert!(state.active_peek_target(10).is_none());
        remove_canonical_sidebar_fixture(state, root);
    }

    struct ImmediatePaneStateIo;

    impl crate::pane_state::snapshot::PaneSnapshotStoreIo for ImmediatePaneStateIo {
        fn save(&mut self, _records: &BTreeMap<PaneInstance, PaneState>) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn apply_waiting_state(
        state: &mut CanonicalCoordinatorState,
        pane_id: &str,
        pane_pid: u32,
        started_at: i64,
        reason: WaitReason,
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
                    &PaneEventEnvelope {
                        daemon_instance_id: DaemonInstanceId::generate().unwrap(),
                        event_id: EventId::generate().unwrap(),
                        pane_instance: pane_instance.clone(),
                        agent: Some(AgentKind::parse("codex").unwrap()),
                        agent_session_id: Some(AgentSessionId::parse("test-session").unwrap()),
                        event,
                    },
                    &VisibilitySnapshot::default(),
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
            )
            .unwrap();
    }

    #[test]
    fn resolved_history_keeps_discarded_completion_under_the_old_agent_and_time() {
        use crate::pane_state::{AgentSessionSource, PaneEvent};

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

        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let Some(current) = state.leased.runtime.record(&pane) else {
            panic!("expected active pane state");
        };
        assert_eq!(current.agent.as_str(), "claude");
        let snapshot = state.resolved_snapshot();
        assert_eq!(
            snapshot
                .panes
                .iter()
                .find(|presentation| presentation.pane_instance == pane)
                .and_then(|presentation| presentation.resolved.as_ref())
                .map(|resolved| resolved.badge),
            Some(BadgeState::Done)
        );
        assert!(
            !snapshot
                .events
                .iter()
                .any(|event| event.from == Some(BadgeState::Done) && event.to == BadgeState::Idle)
        );

        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn resolved_history_keeps_same_agent_previous_session_completion_time() {
        use crate::pane_state::{AgentSessionSource, PaneEvent};

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

        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let Some(current) = state.leased.runtime.record(&pane) else {
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
        assert_eq!(
            snapshot
                .panes
                .iter()
                .find(|presentation| presentation.pane_instance == pane)
                .and_then(|presentation| presentation.resolved.as_ref())
                .map(|resolved| resolved.badge),
            Some(BadgeState::Done)
        );
        assert!(
            !snapshot
                .events
                .iter()
                .any(|event| event.from == Some(BadgeState::Done) && event.to == BadgeState::Idle)
        );

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
    fn resolved_history_exposes_begin_run_prompt_digest() {
        use crate::pane_state::{PaneEvent, PromptState};

        let (mut state, root) = canonical_sidebar_fixture();
        state.leased.runtime = CanonicalPaneStateRuntime::default();
        let digest = PromptState::digest_decoded_prompt("raw\nprompt");
        apply_history_event(
            &mut state,
            "codex",
            "same-session",
            PaneEvent::BeginRun {
                started_at: 1,
                prompt: Some(PromptState {
                    text: "raw prompt".to_string(),
                    source: "user".to_string(),
                    digest: Some(digest.clone()),
                }),
            },
        );

        assert_eq!(
            state
                .resolved_snapshot()
                .events
                .last()
                .and_then(|event| event.prompt_digest.as_ref()),
            Some(&digest)
        );

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
                insertions: 12,
                deletions: 3,
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
                insertions: 0,
                deletions: 0,
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
                insertions: 0,
                deletions: 0,
            },
        )]);
        assert!(
            state
                .replace_git_projection(oversized.clone(), BTreeMap::new())
                .unwrap()
        );
        assert_eq!(state.git_badges, oversized);
        assert_eq!(state.leased.runtime.snapshot_revision(), 3);
        let message = ServerMessage::ResolvedSnapshotResult {
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
    fn editprompt_target_is_focused_and_suppresses_attention_without_changing_physical_active() {
        use crate::pane_state::ClientWitness;

        let (mut state, root) = canonical_sidebar_fixture();
        let target = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let editor = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 109,
        };
        state
            .topology
            .panes
            .iter_mut()
            .find(|pane| pane.pane_instance == target)
            .unwrap()
            .editprompt_editor_pane = Some(editor.pane_id.clone());
        state
            .topology
            .panes
            .push(crate::daemon::topology::TopologyPane {
                pane_instance: editor.clone(),
                session_links: vec![status_link("$1", "main", 0, true)],
                window_id: "@1".to_string(),
                window_name: "@1".to_string(),
                current_path: "/tmp/alpha".to_string(),
                current_command: "node".to_string(),
                pane_width: 80,
                active: true,
                editprompt_is_editor: true,
                editprompt_target_panes: vec![target.pane_id.clone()],
                editprompt_editor_pane: None,
            });
        state
            .topology
            .panes
            .iter_mut()
            .find(|pane| pane.pane_instance == target)
            .unwrap()
            .active = false;
        state
            .views
            .reconcile(
                &[ClientWitness {
                    client_pid: 10,
                    session_id: "$1".to_string(),
                    window_id: "@1".to_string(),
                    active_pane: editor.clone(),
                    control_mode: false,
                    active_pane_flag: false,
                }],
                &BTreeMap::from([("@1".to_string(), vec![target.clone(), editor.clone()])]),
            )
            .unwrap();

        let snapshot = state.resolved_snapshot();
        let target_presentation = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_instance == target)
            .unwrap();
        assert!(!target_presentation.active);
        assert!(target_presentation.focused);
        assert!(
            snapshot
                .attention
                .iter()
                .all(|entry| entry.pane_instance != target)
        );

        state
            .topology
            .panes
            .iter_mut()
            .find(|pane| pane.pane_instance == target)
            .unwrap()
            .editprompt_editor_pane = Some("%99".to_string());
        let snapshot = state.resolved_snapshot();
        assert!(
            !snapshot
                .panes
                .iter()
                .find(|pane| pane.pane_instance == target)
                .unwrap()
                .focused
        );
        assert!(
            snapshot
                .attention
                .iter()
                .any(|entry| entry.pane_instance == target)
        );

        remove_canonical_sidebar_fixture(state, root);
    }

    #[test]
    fn resolved_snapshot_tracks_the_session_of_each_eligible_client() {
        use crate::pane_state::ClientWitness;

        let (mut state, root) = canonical_sidebar_fixture();
        set_state_categories(
            &mut state,
            &[("/repo-one", "work"), ("/repo-two", "private")],
        );
        state.status_metadata.sessions = BTreeMap::from([
            (
                "$1".to_string(),
                SessionProjectionMetadata {
                    session_name: "one".to_string(),
                    project_path: "/repo-one".to_string(),
                    ..SessionProjectionMetadata::default()
                },
            ),
            (
                "$2".to_string(),
                SessionProjectionMetadata {
                    session_name: "two".to_string(),
                    project_path: "/repo-two".to_string(),
                    ..SessionProjectionMetadata::default()
                },
            ),
        ]);
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
        assert_eq!(
            state.resolved_snapshot().sidebar_model.active_categories,
            BTreeSet::from(["work".to_string()])
        );

        state
            .views
            .reconcile(&[witness("$2")], &window_panes)
            .unwrap();
        assert_eq!(
            state.resolved_snapshot().sidebar_model.active_sessions,
            BTreeSet::from(["$2".to_string()])
        );
        assert_eq!(
            state.resolved_snapshot().sidebar_model.active_categories,
            BTreeSet::from(["private".to_string()])
        );
        remove_canonical_sidebar_fixture(state, root);
    }

    fn status_resolved_snapshot() -> ResolvedSnapshot {
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
        let mut snapshot = ResolvedSnapshot {
            snapshot_revision: 42,
            panes: vec![first.clone(), second, non_agent],
            sidebar_model: SidebarModel::default(),
            attention: vec![AttentionEntry {
                pane_instance: first.pane_instance,
                session_name: "main".to_string(),
                badge: BadgeState::Blocked,
                reason: Some("permission".to_string()),
                elapsed_seconds: 30,
            }],
            events: Vec::new(),
            diagnostics: Vec::new(),
        };
        set_resolved_categories(&mut snapshot, &[("/repo-main", "work")]);
        snapshot
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
            let Some(StoredStateDescriptor::Canonical { version }) =
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
            sessions: BTreeMap::from([
                (
                    "$1".to_string(),
                    SessionProjectionMetadata {
                        session_name: "main".to_string(),
                        project_path: "/repo-main".to_string(),
                        attached: Some(true),
                        created_at: Some(10),
                    },
                ),
                (
                    "$2".to_string(),
                    SessionProjectionMetadata {
                        session_name: "mirror".to_string(),
                        project_path: "/repo-main".to_string(),
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

    fn status_config() -> Config {
        Config::default()
    }

    fn set_resolved_categories(resolved: &mut ResolvedSnapshot, assignments: &[(&str, &str)]) {
        let mut state = crate::category::CategoryState::default();
        let mut identities = BTreeMap::new();
        let mut repos = Vec::new();
        for (path, category) in assignments {
            let repo = crate::category::RepoIdentity {
                key: crate::category::RepoKey::path(path),
                rule_path: (*path).to_string(),
                display_name: path.trim_matches('/').replace('/', "-"),
            };
            let category = if *category == crate::category::UNCATEGORIZED {
                crate::category::CategoryName::uncategorized()
            } else {
                crate::category::CategoryName::parse(*category).unwrap()
            };
            if category.as_str() != crate::category::UNCATEGORIZED {
                state.dynamic_categories.insert(category.clone());
            }
            state.repo_overrides.insert(repo.key.clone(), category);
            identities.insert((*path).to_string(), repo.clone());
            repos.push(repo);
        }
        resolved.sidebar_model.category_state = state.clone();
        resolved.sidebar_model.repo_identities = identities;
        resolved.sidebar_model.categories =
            crate::category::EffectiveCategoryModel::build(&Config::default(), &state, repos)
                .unwrap();
    }

    fn set_state_categories(state: &mut CanonicalCoordinatorState, assignments: &[(&str, &str)]) {
        let mut resolved = ResolvedSnapshot {
            snapshot_revision: 0,
            panes: Vec::new(),
            sidebar_model: SidebarModel::default(),
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        };
        set_resolved_categories(&mut resolved, assignments);
        state.category_state = resolved.sidebar_model.category_state;
        state.repo_identities = resolved.sidebar_model.repo_identities;
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
                    StatusContext::Session { session_id } => {
                        session_id.as_str()
                    }
                    StatusContext::Global => "global",
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
            &resolved.events,
            &SidebarState {
                category_scope: crate::sidebar::state::CategoryScope::All,
                ..SidebarState::default()
            },
            now_epoch_secs(),
        );
        assert_eq!(projection.counts.total, 1);
        assert_eq!(projection.counts.blocked, 0);
        assert_eq!(
            projection.counts.limited,
            usize::from(expected == BadgeState::Limited)
        );
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
            StatusContext::Global,
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
        use crate::pane_state::PaneEvent;

        let (mut state, root) = canonical_sidebar_fixture();
        state.leased.runtime = CanonicalPaneStateRuntime::default();
        state.status_metadata = StatusProjectionMetadata {
            sessions: BTreeMap::from([(
                "$1".to_string(),
                SessionProjectionMetadata {
                    session_name: "main".to_string(),
                    project_path: "/repo-main".to_string(),
                    attached: Some(true),
                    created_at: Some(1),
                },
            )]),
            windows: BTreeMap::new(),
        };
        state.projection_config = status_config();
        set_state_categories(&mut state, &[("/repo-main", "work")]);

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

        let pane = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let current = state.leased.runtime.record(&pane).unwrap();
        let through_order = current.unread.latest_unread().unwrap().order;
        apply_history_event(
            &mut state,
            "codex",
            "scenario-session",
            PaneEvent::MarkPaneRead { through_order },
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
            StatusContext::Global,
            &status_metadata(),
            &status_config(),
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
                            ..SessionProjectionMetadata::default()
                        },
                    )
                })
                .collect(),
            ..StatusProjectionMetadata::default()
        };

        let snapshot = build_status_snapshot(
            &resolved,
            StatusContext::Global,
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
        config.categories.rules.push(crate::config::CategoryRule {
            category: "private".to_string(),
            path_patterns: vec!["/repo".to_string()],
        });
        let metadata = StatusProjectionMetadata {
            sessions: BTreeMap::from([
                (
                    "$1".to_string(),
                    SessionProjectionMetadata {
                        session_name: "private".to_string(),
                        project_path: "/repo".to_string(),
                        ..SessionProjectionMetadata::default()
                    },
                ),
                (
                    "$2".to_string(),
                    SessionProjectionMetadata {
                        session_name: "uncategorized".to_string(),
                        project_path: "/repo-misc".to_string(),
                        ..SessionProjectionMetadata::default()
                    },
                ),
            ]),
            ..StatusProjectionMetadata::default()
        };
        set_resolved_categories(
            &mut resolved,
            &[("/repo", "private"), ("/repo-misc", "misc")],
        );

        let snapshot = build_status_snapshot(
            &resolved,
            StatusContext::Session {
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
                        project_path: "/repo-short".to_string(),
                        ..SessionProjectionMetadata::default()
                    },
                ),
                (
                    "$2".to_string(),
                    SessionProjectionMetadata {
                        session_name: "much-longer-session".to_string(),
                        project_path: "/repo-long".to_string(),
                        ..SessionProjectionMetadata::default()
                    },
                ),
                (
                    "$3".to_string(),
                    SessionProjectionMetadata {
                        session_name: "peer".to_string(),
                        project_path: "/repo-peer".to_string(),
                        ..SessionProjectionMetadata::default()
                    },
                ),
            ]),
            ..StatusProjectionMetadata::default()
        };
        set_resolved_categories(
            &mut resolved,
            &[
                ("/repo-short", "short"),
                ("/repo-long", "long"),
                ("/repo-peer", "long"),
            ],
        );
        let mut config = Config::default();
        config.statusline.sessions.fixed_width = true;
        config.statusline.sessions.separator = " | ".to_string();

        let short = build_status_snapshot(
            &resolved,
            StatusContext::Session {
                session_id: "$1".to_string(),
            },
            &metadata,
            &config,
        );
        let long = build_status_snapshot(
            &resolved,
            StatusContext::Session {
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
            StatusContext::Session {
                session_id: "$1".to_string(),
            },
            &status_metadata(),
            &status_config(),
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
            StatusContext::Session {
                session_id: "$1".to_string(),
            },
            &StatusProjectionMetadata::default(),
            &Config::default(),
        );

        assert_eq!(snapshot.summary.total(), 2);
        assert!(snapshot.sessions.iter().all(|session| {
            session.category.as_deref() == Some(crate::category::UNCATEGORIZED)
                && session.attached.is_none()
                && session.created_at.is_none()
        }));
        assert!(snapshot.windows.iter().all(|window| {
            window.bell.is_none() && window.activity.is_none() && window.silence.is_none()
        }));
        assert_eq!(snapshot.categories.len(), 1);
        assert_eq!(
            snapshot.categories[0].category,
            crate::category::UNCATEGORIZED
        );
    }
}

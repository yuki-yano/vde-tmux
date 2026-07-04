use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::daemon::protocol::{ServerMessage, SidebarClientEvent};
use crate::daemon::{
    DaemonSnapshot, SidebarFrame, build_snapshot_with_sidebar, render_agent_badge,
};
use crate::git::GitBadge;
use crate::options::snapshot::PaneSnapshot;
use crate::sidebar::input::{SidebarCommand, SidebarInputAction, activate_selected};
use crate::sidebar::state::{SidebarAction, SidebarState};
use crate::sidebar::tree::{SidebarRow, build_rows_with_git, row_refs};

const STATE_DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u64);

#[derive(Debug, Clone)]
pub enum DaemonEvent {
    Connect {
        client_id: ClientId,
        slot: Arc<LatestSlot<ServerMessage>>,
    },
    Disconnect {
        client_id: ClientId,
    },
    Client {
        client_id: ClientId,
        event: SidebarClientEvent,
    },
    PanesUpdated(Vec<PaneSnapshot>),
    GitStatusUpdated(BTreeMap<String, GitBadge>),
    QueryStatusline {
        reply: Sender<ServerMessage>,
    },
    DebounceCheck(Instant),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEffect {
    JumpPane(String),
    SaveState(SidebarState),
    SetSessionBadge { session: String, value: String },
    ClearSessionBadge { session: String },
}

#[derive(Debug)]
pub struct LatestSlot<T> {
    inner: Mutex<LatestSlotInner<T>>,
    ready: Condvar,
}

#[derive(Debug)]
struct LatestSlotInner<T> {
    value: Option<T>,
    closed: bool,
}

impl<T> Default for LatestSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> LatestSlot<T> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(LatestSlotInner {
                value: None,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    pub fn publish(&self, value: T) {
        let mut inner = self.inner.lock().expect("latest slot poisoned");
        if inner.closed {
            return;
        }
        inner.value = Some(value);
        self.ready.notify_one();
    }

    pub fn wait_for_update(&self) -> Option<T> {
        let mut inner = self.inner.lock().expect("latest slot poisoned");
        while inner.value.is_none() && !inner.closed {
            inner = self.ready.wait(inner).expect("latest slot poisoned");
        }
        inner.value.take()
    }

    pub fn try_take(&self) -> Option<T> {
        self.inner
            .lock()
            .expect("latest slot poisoned")
            .value
            .take()
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().expect("latest slot poisoned");
        inner.closed = true;
        self.ready.notify_all();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushFingerprint {
    state_version: u64,
    rows: Vec<String>,
}

#[derive(Debug)]
pub struct RuntimeState {
    config: Config,
    pub ui_state: SidebarState,
    pub panes: Vec<PaneSnapshot>,
    git_badges: BTreeMap<String, GitBadge>,
    rows: Vec<SidebarRow>,
    snapshot: Option<DaemonSnapshot>,
    clients: BTreeMap<ClientId, Arc<LatestSlot<ServerMessage>>>,
    last_pushed: Option<PushFingerprint>,
    running: bool,
    dirty_state_since: Option<Instant>,
    pane_was_idle: BTreeMap<String, bool>,
    unread: BTreeMap<String, bool>,
    written_badges: BTreeMap<String, String>,
}

impl RuntimeState {
    pub fn new(config: Config, ui_state: SidebarState) -> Self {
        Self {
            config,
            ui_state,
            panes: Vec::new(),
            git_badges: BTreeMap::new(),
            rows: Vec::new(),
            snapshot: None,
            clients: BTreeMap::new(),
            last_pushed: None,
            running: true,
            dirty_state_since: None,
            pane_was_idle: BTreeMap::new(),
            unread: BTreeMap::new(),
            written_badges: BTreeMap::new(),
        }
    }

    pub fn apply_event(&mut self, event: DaemonEvent) -> Vec<RuntimeEffect> {
        match event {
            DaemonEvent::Connect { client_id, slot } => {
                if let Some(snapshot) = self.snapshot.clone() {
                    slot.publish(ServerMessage::Snapshot { snapshot });
                }
                self.clients.insert(client_id, slot);
                Vec::new()
            }
            DaemonEvent::Disconnect { client_id } => {
                if let Some(slot) = self.clients.remove(&client_id) {
                    slot.close();
                }
                Vec::new()
            }
            DaemonEvent::Client { event, .. } => self.apply_client_event(event),
            DaemonEvent::PanesUpdated(panes) => {
                self.panes = panes;
                self.update_unread();
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                self.sync_session_badges()
            }
            DaemonEvent::GitStatusUpdated(git_badges) => {
                self.git_badges = git_badges;
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                Vec::new()
            }
            DaemonEvent::QueryStatusline { reply } => {
                let agent_badge = self
                    .snapshot
                    .as_ref()
                    .map(render_agent_badge)
                    .unwrap_or_default();
                let _ = reply.send(ServerMessage::Statusline { agent_badge });
                Vec::new()
            }
            DaemonEvent::DebounceCheck(now) => self.flush_state_if_due(now),
            DaemonEvent::Shutdown => {
                self.running = false;
                self.clients.values().for_each(|slot| slot.close());
                let effects = self
                    .written_badges
                    .keys()
                    .map(|session| RuntimeEffect::ClearSessionBadge {
                        session: session.clone(),
                    })
                    .collect();
                self.written_badges.clear();
                effects
            }
        }
    }

    pub fn rebuild_snapshot(&mut self) {
        self.rows =
            build_rows_with_git(&self.config, &self.panes, &self.ui_state, &self.git_badges);
        let sidebar = SidebarFrame {
            state: self.ui_state.clone(),
            rows: self.rows.clone(),
        };
        self.snapshot = Some(build_snapshot_with_sidebar(&self.panes, Some(sidebar)));
    }

    pub fn should_push(&self) -> bool {
        self.snapshot.is_some()
            && self
                .last_pushed
                .as_ref()
                .map(|last| last != &self.current_fingerprint())
                .unwrap_or(true)
    }

    pub fn current_fingerprint(&self) -> PushFingerprint {
        PushFingerprint {
            state_version: self.ui_state.version,
            rows: self
                .rows
                .iter()
                .map(|row| {
                    format!(
                        "{}:{}:{}:{:?}:{:?}",
                        row.id, row.label, row.chat_count, row.rollup, row.git
                    )
                })
                .collect(),
        }
    }

    pub fn mark_pushed(&mut self, fingerprint: PushFingerprint) {
        self.last_pushed = Some(fingerprint);
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn clients_len(&self) -> usize {
        self.clients.len()
    }

    pub fn state_dirty_since(&self) -> Option<Instant> {
        self.dirty_state_since
    }

    pub fn mark_state_dirty(&mut self, now: Instant) {
        self.dirty_state_since = Some(now);
    }

    pub fn snapshot(&self) -> Option<&DaemonSnapshot> {
        self.snapshot.as_ref()
    }

    fn apply_client_event(&mut self, event: SidebarClientEvent) -> Vec<RuntimeEffect> {
        match event {
            SidebarClientEvent::Key { key } => self.apply_key(&key),
            SidebarClientEvent::JumpPane { pane } => vec![RuntimeEffect::JumpPane(pane)],
        }
    }

    fn apply_key(&mut self, key: &str) -> Vec<RuntimeEffect> {
        let Some(action) = crate::sidebar::input::parse_key(key) else {
            return Vec::new();
        };
        let row_refs = row_refs(&self.rows);
        let changed = match action {
            SidebarInputAction::MoveNext => self.ui_state.apply(SidebarAction::MoveNext, &row_refs),
            SidebarInputAction::MovePrevious => {
                self.ui_state.apply(SidebarAction::MovePrevious, &row_refs)
            }
            SidebarInputAction::ToggleExpand => {
                self.ui_state.apply(SidebarAction::ToggleExpand, &row_refs)
            }
            SidebarInputAction::SetViewMode(view_mode) => self
                .ui_state
                .apply(SidebarAction::SetViewMode(view_mode), &row_refs),
            SidebarInputAction::Activate => {
                match activate_selected(self.ui_state.selection.as_deref(), &self.rows) {
                    Some(SidebarCommand::JumpPane(pane_id)) => {
                        return vec![RuntimeEffect::JumpPane(pane_id)];
                    }
                    Some(SidebarCommand::ToggleExpand(row_id)) => {
                        self.ui_state.selection = Some(row_id);
                        self.ui_state.apply(SidebarAction::ToggleExpand, &row_refs)
                    }
                    None => false,
                }
            }
        };
        if changed {
            self.mark_state_dirty(Instant::now());
            self.rebuild_snapshot();
            self.broadcast_if_needed();
        }
        Vec::new()
    }

    fn flush_state_if_due(&mut self, now: Instant) -> Vec<RuntimeEffect> {
        if let Some(since) = self.dirty_state_since
            && now.duration_since(since) >= STATE_DEBOUNCE
        {
            self.dirty_state_since = None;
            return vec![RuntimeEffect::SaveState(self.ui_state.clone())];
        }
        Vec::new()
    }

    fn update_unread(&mut self) {
        let mut next_was_idle = BTreeMap::new();
        let mut next_unread = BTreeMap::new();
        for pane in self
            .panes
            .iter()
            .filter(|pane| !pane.is_sidebar && !pane.agent.is_empty())
        {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let is_idle = level == crate::hook::RollupLevel::Idle;
            let was_idle = self.pane_was_idle.get(&pane.pane_id).copied();
            let mut unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            match was_idle {
                None => unread = false,
                Some(false) if is_idle => unread = true,
                _ => {}
            }
            if !is_idle {
                unread = false;
            }
            if pane.window_active && pane.session_attached {
                unread = false;
            }
            next_was_idle.insert(pane.pane_id.clone(), is_idle);
            next_unread.insert(pane.pane_id.clone(), unread);
        }
        self.pane_was_idle = next_was_idle;
        self.unread = next_unread;
    }

    fn sync_session_badges(&mut self) -> Vec<RuntimeEffect> {
        use crate::daemon::session_badge::{BadgeState, badge_state, session_badge_value};

        let badge_config = &self.config.statusline.session_badge;
        let mut desired = BTreeMap::new();
        if badge_config.enabled {
            let mut states: BTreeMap<String, Vec<BadgeState>> = BTreeMap::new();
            for pane in self
                .panes
                .iter()
                .filter(|pane| !pane.is_sidebar && !pane.agent.is_empty())
            {
                let level = crate::sidebar::tree::rollup_for_pane(pane);
                let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
                states
                    .entry(pane.session.clone())
                    .or_default()
                    .push(badge_state(level, unread));
            }
            for (session, list) in states {
                if let Some(value) = session_badge_value(list, badge_config) {
                    desired.insert(session, value);
                }
            }
        }

        let mut effects = Vec::new();
        for (session, value) in &desired {
            if self.written_badges.get(session) != Some(value) {
                effects.push(RuntimeEffect::SetSessionBadge {
                    session: session.clone(),
                    value: value.clone(),
                });
            }
        }
        for session in self.written_badges.keys() {
            if !desired.contains_key(session) {
                effects.push(RuntimeEffect::ClearSessionBadge {
                    session: session.clone(),
                });
            }
        }
        self.written_badges = desired;
        effects
    }

    fn broadcast_if_needed(&mut self) {
        if !self.should_push() {
            return;
        }
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let fingerprint = self.current_fingerprint();
        for slot in self.clients.values() {
            slot.publish(ServerMessage::Snapshot {
                snapshot: snapshot.clone(),
            });
        }
        self.mark_pushed(fingerprint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::daemon::protocol::{ServerMessage, SidebarClientEvent};
    use crate::options::snapshot::PaneSnapshot;
    use crate::sidebar::state::SidebarState;
    use std::time::{Duration, Instant};

    fn pane(pane_id: &str, path: &str, agent: &str, status: &str) -> PaneSnapshot {
        PaneSnapshot {
            session: "main".to_string(),
            window_id: "@1".to_string(),
            pane_id: pane_id.to_string(),
            current_path: path.to_string(),
            agent: agent.to_string(),
            status: status.to_string(),
            ..PaneSnapshot::default()
        }
    }

    fn agent_pane(session: &str, pane_id: &str, status: &str) -> PaneSnapshot {
        PaneSnapshot {
            session: session.to_string(),
            window_id: "@1".to_string(),
            pane_id: pane_id.to_string(),
            current_path: "/tmp".to_string(),
            current_command: "zsh".to_string(),
            window_active: false,
            session_attached: false,
            is_sidebar: false,
            agent: "codex".to_string(),
            status: status.to_string(),
            prompt: String::new(),
            prompt_source: String::new(),
            wait_reason: String::new(),
            attention: String::new(),
            started_at: String::new(),
            completed_at: String::new(),
            tasks: String::new(),
            subagents: String::new(),
        }
    }

    #[test]
    fn latest_slot_coalesces_slow_client_writes() {
        let slot = LatestSlot::new();
        slot.publish(1);
        slot.publish(2);

        assert_eq!(slot.wait_for_update(), Some(2));
        assert_eq!(slot.try_take(), None);
    }

    #[test]
    fn should_push_initial_snapshot() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.panes = vec![pane("%1", "/tmp/app", "codex", "running")];
        state.rebuild_snapshot();

        assert!(state.should_push());
    }

    #[test]
    fn should_push_when_rows_change_without_state_version() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.panes = vec![pane("%1", "/tmp/app", "codex", "running")];
        state.rebuild_snapshot();
        let first = state.current_fingerprint();
        state.mark_pushed(first);
        state.panes.push(pane("%2", "/tmp/app", "claude", "idle"));
        state.rebuild_snapshot();

        assert!(state.should_push());
    }

    #[test]
    fn client_error_does_not_stop_runtime() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let slot = std::sync::Arc::new(LatestSlot::new());
        state.apply_event(DaemonEvent::Connect {
            client_id: ClientId(7),
            slot,
        });
        state.apply_event(DaemonEvent::Disconnect {
            client_id: ClientId(7),
        });
        state.apply_event(DaemonEvent::PanesUpdated(vec![pane(
            "%1", "/tmp/app", "codex", "running",
        )]));

        assert!(state.is_running());
        assert_eq!(state.clients_len(), 0);
    }

    #[test]
    fn client_key_updates_state_and_marks_dirty() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![pane(
            "%1", "/tmp/app", "codex", "running",
        )]));
        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "j".to_string(),
            },
        });

        assert_eq!(state.ui_state.selection.as_deref(), Some("repo::misc::app"));
        assert!(state.state_dirty_since().is_some());
    }

    #[test]
    fn debounce_check_flushes_dirty_state() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let now = Instant::now();
        state.mark_state_dirty(now);

        let effects =
            state.apply_event(DaemonEvent::DebounceCheck(now + Duration::from_millis(250)));

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, RuntimeEffect::SaveState(_)))
        );
    }

    #[test]
    fn publishes_snapshot_to_each_client_slot_without_socket_write() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let slot = std::sync::Arc::new(LatestSlot::new());
        state.apply_event(DaemonEvent::Connect {
            client_id: ClientId(1),
            slot: slot.clone(),
        });
        state.apply_event(DaemonEvent::PanesUpdated(vec![pane(
            "%1", "/tmp/app", "codex", "running",
        )]));

        let pushed = slot.wait_for_update();

        assert!(matches!(pushed, Some(ServerMessage::Snapshot { .. })));
    }

    #[test]
    fn panes_updated_emits_set_session_badge_effect() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        assert_eq!(
            effects,
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "🟡 ".to_string(),
            }]
        );
    }

    #[test]
    fn unchanged_badge_emits_no_effect() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        assert!(effects.is_empty());
    }

    #[test]
    fn running_to_idle_becomes_done_until_window_viewed() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));
        assert_eq!(
            effects,
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "🔵 ".to_string(),
            }]
        );

        let mut viewed = agent_pane("main", "%1", "idle");
        viewed.window_active = true;
        viewed.session_attached = true;
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![viewed]));
        assert_eq!(
            effects,
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "🟢 ".to_string(),
            }]
        );
    }

    #[test]
    fn first_seen_idle_pane_is_not_unread() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));
        assert_eq!(
            effects,
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "🟢 ".to_string(),
            }]
        );
    }

    #[test]
    fn session_rollup_prefers_blocked_over_working() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut blocked = agent_pane("main", "%2", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "running"),
            blocked,
        ]));
        assert_eq!(
            effects,
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "🔴 ".to_string(),
            }]
        );
    }

    #[test]
    fn sessions_get_independent_badges() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("alpha", "%1", "running"),
            agent_pane("beta", "%2", "idle"),
        ]));
        assert_eq!(
            effects,
            vec![
                RuntimeEffect::SetSessionBadge {
                    session: "alpha".to_string(),
                    value: "🟡 ".to_string(),
                },
                RuntimeEffect::SetSessionBadge {
                    session: "beta".to_string(),
                    value: "🟢 ".to_string(),
                },
            ]
        );
    }

    #[test]
    fn vanished_session_emits_clear_effect() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![]));
        assert_eq!(
            effects,
            vec![RuntimeEffect::ClearSessionBadge {
                session: "main".to_string(),
            }]
        );
    }

    #[test]
    fn shutdown_clears_all_written_badges() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let effects = state.apply_event(DaemonEvent::Shutdown);
        assert_eq!(
            effects,
            vec![RuntimeEffect::ClearSessionBadge {
                session: "main".to_string(),
            }]
        );
        assert!(!state.is_running());
    }

    #[test]
    fn disabled_config_writes_no_badges() {
        let mut config = Config::default();
        config.statusline.session_badge.enabled = false;
        let mut state = RuntimeState::new(config, SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        assert!(effects.is_empty());
    }

    #[test]
    fn sidebar_and_agentless_panes_are_ignored() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut sidebar = agent_pane("main", "%9", "running");
        sidebar.is_sidebar = true;
        let mut plain = agent_pane("main", "%8", "");
        plain.agent = String::new();
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![sidebar, plain]));
        assert!(effects.is_empty());
    }
}

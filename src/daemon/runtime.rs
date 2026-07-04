use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::daemon::protocol::{ServerMessage, SidebarClientEvent};
use crate::daemon::{DaemonSnapshot, SidebarFrame, build_snapshot_with_sidebar};
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
    DebounceCheck(Instant),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEffect {
    JumpPane(String),
    SaveState(SidebarState),
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
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                Vec::new()
            }
            DaemonEvent::GitStatusUpdated(git_badges) => {
                self.git_badges = git_badges;
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                Vec::new()
            }
            DaemonEvent::DebounceCheck(now) => self.flush_state_if_due(now),
            DaemonEvent::Shutdown => {
                self.running = false;
                self.clients.values().for_each(|slot| slot.close());
                Vec::new()
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
}

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::daemon::protocol::{ServerMessage, SidebarClientEvent};
use crate::daemon::session_badge::{BadgeState, badge_state};
use crate::daemon::{
    DaemonSnapshot, SidebarFrame, TransitionEvent, build_snapshot_with_sidebar, format_attention,
    render_summary,
};
use crate::git::GitBadge;
use crate::options::snapshot::{PaneSnapshot, effective_agent, is_live_agent_pane};
use crate::sidebar::input::{SidebarCommand, SidebarInputAction, activate_selected};
use crate::sidebar::state::{RepoId, SidebarAction, SidebarState};
use crate::sidebar::tree::{
    RowBuildContext, SidebarRow, SidebarRowKind, build_rows_ctx, now_epoch_secs, row_refs,
};

const STATE_DEBOUNCE: Duration = Duration::from_millis(200);
const TRIAGE_LEAVE_POLLS: u8 = 2;
const FLASH_POLLS: u8 = 2;
const EVENT_CAP: usize = 20;

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
    QuerySummary {
        reply: Sender<ServerMessage>,
    },
    QueryAttention {
        reply: Sender<ServerMessage>,
    },
    DebounceCheck(Instant),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEffect {
    JumpPane(String),
    PreviewPane {
        pane_id: String,
        history_lines: u32,
    },
    SaveState(SidebarState),
    SetSessionBadge {
        session: String,
        value: String,
        state: String,
    },
    ClearSessionBadge {
        session: String,
    },
    Heartbeat(i64),
    ClearHeartbeat,
    Notify {
        pane_id: String,
        agent: String,
        state: BadgeState,
    },
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
    triage: BTreeSet<String>,
    calm_streak: BTreeMap<String, u8>,
    prev_badges: BTreeMap<String, BadgeState>,
    events: VecDeque<TransitionEvent>,
    flash: BTreeMap<String, u8>,
    written_badges: BTreeMap<String, (String, String)>,
    last_heartbeat: i64,
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
            triage: BTreeSet::new(),
            calm_streak: BTreeMap::new(),
            prev_badges: BTreeMap::new(),
            events: VecDeque::new(),
            flash: BTreeMap::new(),
            written_badges: BTreeMap::new(),
            last_heartbeat: 0,
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
                self.update_triage();
                let transition_effects = self.update_transitions();
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                let mut effects = transition_effects;
                effects.extend(self.sync_session_badges());
                effects.extend(self.sync_heartbeat());
                effects
            }
            DaemonEvent::GitStatusUpdated(git_badges) => {
                self.git_badges = git_badges;
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                Vec::new()
            }
            DaemonEvent::QuerySummary { reply } => {
                let text = self.render_summary_text();
                let _ = reply.send(ServerMessage::Summary { text });
                Vec::new()
            }
            DaemonEvent::QueryAttention { reply } => {
                let text = self.render_attention_text();
                let _ = reply.send(ServerMessage::Attention { text });
                Vec::new()
            }
            DaemonEvent::DebounceCheck(now) => self.flush_state_if_due(now),
            DaemonEvent::Shutdown => {
                self.running = false;
                self.clients.values().for_each(|slot| slot.close());
                let mut effects = self
                    .written_badges
                    .keys()
                    .map(|session| RuntimeEffect::ClearSessionBadge {
                        session: session.clone(),
                    })
                    .collect::<Vec<_>>();
                effects.push(RuntimeEffect::ClearHeartbeat);
                self.written_badges.clear();
                effects
            }
        }
    }

    pub fn rebuild_snapshot(&mut self) {
        self.rows = build_rows_ctx(
            &self.config,
            &self.panes,
            &self.ui_state,
            &RowBuildContext {
                git: self.git_badges.clone(),
                unread: self.unread.clone(),
                triage: self.triage.clone(),
                flash: self.flash.keys().cloned().collect(),
                now: now_epoch_secs(),
            },
        );
        let sidebar = SidebarFrame {
            state: self.ui_state.clone(),
            rows: self.rows.clone(),
        };
        let mut snapshot = build_snapshot_with_sidebar(&self.panes, Some(sidebar));
        snapshot.events = self.events.iter().cloned().collect();
        self.snapshot = Some(snapshot);
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
                        "{}:{}:{}:{:?}:{:?}:{:?}:{:?}",
                        row.id,
                        row.label,
                        row.chat_count,
                        row.rollup,
                        row.badge_state,
                        row.git,
                        row.meta
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

    pub fn notify_command(&self) -> Option<&str> {
        (self.config.notify.enabled && !self.config.notify.command.trim().is_empty())
            .then_some(self.config.notify.command.as_str())
    }

    fn apply_client_event(&mut self, event: SidebarClientEvent) -> Vec<RuntimeEffect> {
        match event {
            SidebarClientEvent::Key { key } => self.apply_key(&key),
            SidebarClientEvent::JumpPane { pane } => {
                self.unread.insert(pane.clone(), false);
                self.ui_state.selection = Some(format!("chat::{pane}"));
                self.mark_state_dirty(Instant::now());
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                vec![RuntimeEffect::JumpPane(pane)]
            }
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
            SidebarInputAction::CycleViewMode => {
                self.ui_state.apply(SidebarAction::CycleViewMode, &row_refs)
            }
            SidebarInputAction::SetFilter(filter) => self.ui_state.set_filter(filter),
            SidebarInputAction::ToggleFilter => {
                self.ui_state.apply(SidebarAction::ToggleFilter, &row_refs)
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
                    let chat_id = format!("chat::{pane_id}");
                    // fisheye は selected || manual で展開するため、detail/meta クリックは
                    // 選択解除後も親 chat を展開維持するかの固定/解除として扱う。
                    self.ui_state.selection = Some(chat_id.clone());
                    self.ui_state.toggle_expanded(&chat_id)
                } else {
                    self.ui_state.selection = Some(row_id.clone());
                    self.ui_state.toggle_expanded(&row_id)
                }
            }
            SidebarInputAction::FocusNextAttention => self.focus_attention(true),
            SidebarInputAction::FocusPreviousAttention => self.focus_attention(false),
            SidebarInputAction::ReorderUp => self.apply_reorder(true),
            SidebarInputAction::ReorderDown => self.apply_reorder(false),
            SidebarInputAction::Activate => {
                match activate_selected(self.ui_state.selection.as_deref(), &self.rows) {
                    Some(SidebarCommand::JumpPane(pane_id)) => {
                        self.unread.insert(pane_id.clone(), false);
                        self.rebuild_snapshot();
                        self.broadcast_if_needed();
                        return vec![RuntimeEffect::JumpPane(pane_id)];
                    }
                    Some(SidebarCommand::ToggleExpand(row_id)) => {
                        self.ui_state.selection = Some(row_id);
                        self.ui_state.apply(SidebarAction::ToggleExpand, &row_refs)
                    }
                    Some(SidebarCommand::PreviewPane(pane_id)) => {
                        return vec![RuntimeEffect::PreviewPane {
                            pane_id,
                            history_lines: self.config.sidebar.preview.history_lines,
                        }];
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

    fn focus_attention(&mut self, forward: bool) -> bool {
        use crate::daemon::session_badge::BadgeState;

        let blocked: Vec<&str> = self
            .rows
            .iter()
            .filter(|row| {
                row.kind == SidebarRowKind::Chat && row.badge_state == Some(BadgeState::Blocked)
            })
            .map(|row| row.id.as_str())
            .collect();
        if blocked.is_empty() {
            return false;
        }
        let current = self
            .ui_state
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
        if self.ui_state.selection.as_deref() == Some(next_id.as_str()) {
            return false;
        }
        self.ui_state.selection = Some(next_id);
        self.ui_state.version += 1;
        true
    }

    fn apply_reorder(&mut self, up: bool) -> bool {
        let Some(repo) = self.selected_repo_id() else {
            return false;
        };
        self.seed_manual_order_from_rows();
        if up {
            self.ui_state.apply(SidebarAction::ReorderUp(repo), &[])
        } else {
            self.ui_state.apply(SidebarAction::ReorderDown(repo), &[])
        }
    }

    fn selected_repo_id(&self) -> Option<RepoId> {
        let selection = self.ui_state.selection.as_deref()?;
        let row = self.rows.iter().find(|row| row.id == selection)?;
        (row.kind == SidebarRowKind::Repo)
            .then(|| RepoId::from_row_id(&row.id))
            .flatten()
    }

    fn seed_manual_order_from_rows(&mut self) {
        let mut changed = false;
        for row in self
            .rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Repo)
        {
            if let Some(repo) = RepoId::from_row_id(&row.id)
                && !self.ui_state.manual_order.contains(&repo)
            {
                self.ui_state.manual_order.push(repo);
                changed = true;
            }
        }
        if changed {
            self.ui_state.version += 1;
        }
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
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
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

    fn update_triage(&mut self) {
        let mut next_triage = BTreeSet::new();
        let mut next_streak = BTreeMap::new();
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            let blocked = badge_state(level, unread) == BadgeState::Blocked;
            if blocked {
                next_triage.insert(pane.pane_id.clone());
                next_streak.insert(pane.pane_id.clone(), 0);
            } else if self.triage.contains(&pane.pane_id) {
                let streak = self.calm_streak.get(&pane.pane_id).copied().unwrap_or(0) + 1;
                if streak < TRIAGE_LEAVE_POLLS {
                    next_triage.insert(pane.pane_id.clone());
                    next_streak.insert(pane.pane_id.clone(), streak);
                }
            }
        }
        self.triage = next_triage;
        self.calm_streak = next_streak;
    }

    fn update_transitions(&mut self) -> Vec<RuntimeEffect> {
        self.decay_flash();
        let badges = self.current_badges();
        let at_epoch = now_epoch_secs();
        let notify_enabled = self.notify_command().is_some();
        let mut effects = Vec::new();
        for (pane_id, (agent, to)) in &badges {
            let from = self.prev_badges.get(pane_id).copied();
            if from != Some(*to) {
                self.events.push_back(TransitionEvent {
                    pane_id: pane_id.clone(),
                    agent: agent.clone(),
                    from,
                    to: *to,
                    at_epoch,
                });
                self.flash.insert(pane_id.clone(), FLASH_POLLS);
                if notify_enabled && *to == BadgeState::Blocked {
                    effects.push(RuntimeEffect::Notify {
                        pane_id: pane_id.clone(),
                        agent: agent.clone(),
                        state: *to,
                    });
                }
            }
        }
        while self.events.len() > EVENT_CAP {
            self.events.pop_front();
        }
        self.prev_badges = badges
            .into_iter()
            .map(|(pane_id, (_, badge))| (pane_id, badge))
            .collect();
        effects
    }

    fn decay_flash(&mut self) {
        let mut next = BTreeMap::new();
        for (pane_id, remaining) in &self.flash {
            if *remaining > 1 {
                next.insert(pane_id.clone(), remaining - 1);
            }
        }
        self.flash = next;
    }

    fn current_badges(&self) -> BTreeMap<String, (String, BadgeState)> {
        let mut badges = BTreeMap::new();
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            badges.insert(
                pane.pane_id.clone(),
                (
                    effective_agent(pane).unwrap_or_default().to_string(),
                    badge_state(level, unread),
                ),
            );
        }
        badges
    }

    fn sync_session_badges(&mut self) -> Vec<RuntimeEffect> {
        use crate::daemon::session_badge::session_badge_value;

        let badge_config = &self.config.statusline.session_badge;
        let badge_glyphs = &self.config.badge.glyphs;
        let mut desired = BTreeMap::new();
        if badge_config.enabled {
            let mut states: BTreeMap<String, Vec<BadgeState>> = BTreeMap::new();
            for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
                let level = crate::sidebar::tree::rollup_for_pane(pane);
                let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
                states
                    .entry(pane.session.clone())
                    .or_default()
                    .push(badge_state(level, unread));
            }
            for (session, list) in states {
                let state = list.iter().copied().min().unwrap_or(BadgeState::Idle);
                if let Some(value) = session_badge_value(
                    list,
                    badge_glyphs,
                    &badge_config.suffix,
                    badge_config.hide_idle,
                ) {
                    desired.insert(session, (value, state.as_str().to_string()));
                }
            }
        }

        let mut effects = Vec::new();
        for (session, (value, state)) in &desired {
            if self.written_badges.get(session) != Some(&(value.clone(), state.clone())) {
                effects.push(RuntimeEffect::SetSessionBadge {
                    session: session.clone(),
                    value: value.clone(),
                    state: state.clone(),
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

    fn render_summary_text(&self) -> String {
        if !self.config.statusline.summary.enabled {
            return String::new();
        }
        let mut blocked = 0usize;
        let mut working = 0usize;
        let mut done = 0usize;
        let mut idle = 0usize;
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            match badge_state(level, unread) {
                BadgeState::Blocked => blocked += 1,
                BadgeState::Working => working += 1,
                BadgeState::Done => done += 1,
                BadgeState::Idle => idle += 1,
            }
        }
        if self.config.statusline.summary.hide_idle {
            idle = 0;
        }
        render_summary(
            &[
                (BadgeState::Blocked, blocked),
                (BadgeState::Working, working),
                (BadgeState::Done, done),
                (BadgeState::Idle, idle),
            ],
            &self.config.badge,
        )
    }

    fn render_attention_text(&self) -> String {
        let now = now_epoch_secs();
        let entries = self
            .panes
            .iter()
            .filter(|pane| is_live_agent_pane(pane))
            .filter(|pane| self.triage.contains(&pane.pane_id))
            .filter(|pane| !(pane.window_active && pane.session_attached))
            .map(|pane| {
                let started = pane.started_at.parse::<i64>().unwrap_or(now);
                (
                    pane.session.clone(),
                    crate::sidebar::tree::rollup_for_pane(pane),
                    (now - started).max(0),
                )
            })
            .collect::<Vec<_>>();
        format_attention(&entries)
    }

    fn sync_heartbeat(&mut self) -> Vec<RuntimeEffect> {
        let now = now_epoch_secs();
        if now == self.last_heartbeat {
            return Vec::new();
        }
        self.last_heartbeat = now;
        vec![RuntimeEffect::Heartbeat(now)]
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
            current_command: agent.to_string(),
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
            current_command: "codex".to_string(),
            pane_tty: String::new(),
            pane_pid: String::new(),
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
            agent_observed: true,
        }
    }

    fn without_heartbeat(effects: &[RuntimeEffect]) -> Vec<RuntimeEffect> {
        effects
            .iter()
            .filter(|effect| !matches!(effect, RuntimeEffect::Heartbeat(_)))
            .cloned()
            .collect()
    }

    fn chat_flash(state: &RuntimeState, pane_id: &str) -> bool {
        state
            .snapshot()
            .and_then(|snapshot| snapshot.sidebar.as_ref())
            .and_then(|sidebar| {
                sidebar
                    .rows
                    .iter()
                    .find(|row| row.id == format!("chat::{pane_id}"))
            })
            .and_then(|row| row.meta.as_ref())
            .and_then(|meta| meta.flash)
            .unwrap_or(false)
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
    fn should_push_when_unread_badge_state_changes_without_rollup_change() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));
        let first = state.current_fingerprint();
        state.mark_pushed(first);

        let mut viewed = agent_pane("main", "%1", "idle");
        viewed.window_active = true;
        viewed.session_attached = true;
        state.panes = vec![viewed];
        state.update_unread();
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
    fn stale_hook_agent_clears_session_badge() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let mut stale = agent_pane("main", "%1", "running");
        stale.current_command = "zsh".to_string();
        stale.agent_observed = false;

        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![stale]));

        assert!(
            effects.iter().any(
                |effect| matches!(effect, RuntimeEffect::ClearSessionBadge { session } if session == "main")
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn badge_transitions_are_recorded_as_events() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));

        let snapshot = state.snapshot().expect("snapshot");

        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].from, None);
        assert_eq!(
            snapshot.events[0].to,
            crate::daemon::session_badge::BadgeState::Working
        );
        assert_eq!(
            snapshot.events[1].from,
            Some(crate::daemon::session_badge::BadgeState::Working)
        );
        assert_eq!(
            snapshot.events[1].to,
            crate::daemon::session_badge::BadgeState::Done
        );
    }

    #[test]
    fn events_are_capped_at_20() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());

        for index in 0..25 {
            let status = if index % 2 == 0 { "running" } else { "idle" };
            state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
                "main", "%1", status,
            )]));
        }

        let snapshot = state.snapshot().expect("snapshot");

        assert_eq!(snapshot.events.len(), 20);
    }

    #[test]
    fn changed_rows_flash_for_two_polls() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());

        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        assert!(chat_flash(&state, "%1"));

        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        assert!(chat_flash(&state, "%1"));

        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        assert!(!chat_flash(&state, "%1"));
    }

    #[test]
    fn blocked_transition_returns_notify_effect_when_enabled() {
        let mut config = Config::default();
        config.notify.enabled = true;
        config.notify.command = "printf blocked".to_string();
        let mut state = RuntimeState::new(config, SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let mut blocked = agent_pane("main", "%1", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();

        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![blocked]));

        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::Notify { pane_id, agent, state }
                if pane_id == "%1"
                    && agent == "codex"
                    && *state == crate::daemon::session_badge::BadgeState::Blocked
        )));
    }

    #[test]
    fn blocked_transition_is_silent_by_default() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let mut blocked = agent_pane("main", "%1", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();

        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![blocked]));

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RuntimeEffect::Notify { .. }))
        );
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
    fn attention_navigation_cycles_blocked_chat_rows() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                view_mode: crate::sidebar::state::ViewMode::Flat,
                ..SidebarState::default()
            },
        );
        let mut blocked_a = agent_pane("main", "%1", "waiting");
        blocked_a.wait_reason = "permission_prompt".to_string();
        let mut blocked_b = agent_pane("main", "%3", "waiting");
        blocked_b.wait_reason = "permission_prompt".to_string();
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            blocked_a,
            agent_pane("main", "%2", "running"),
            blocked_b,
        ]));

        let key = |state: &mut RuntimeState, key: &str| {
            state.apply_event(DaemonEvent::Client {
                client_id: ClientId(1),
                event: SidebarClientEvent::Key {
                    key: key.to_string(),
                },
            });
        };

        key(&mut state, "n");
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        key(&mut state, "n");
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%3"));
        key(&mut state, "n");
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        key(&mut state, "N");
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%3"));
    }

    #[test]
    fn attention_navigation_is_noop_without_blocked_rows() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                view_mode: crate::sidebar::state::ViewMode::Flat,
                ..SidebarState::default()
            },
        );
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "n".to_string(),
            },
        });
        assert_eq!(state.ui_state.selection, None);
    }

    #[test]
    fn moving_through_expanded_chat_does_not_teleport_selection() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                view_mode: crate::sidebar::state::ViewMode::Flat,
                ..SidebarState::default()
            },
        );
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "running"),
            agent_pane("main", "%2", "running"),
        ]));
        let key = |state: &mut RuntimeState, key: &str| {
            state.apply_event(DaemonEvent::Client {
                client_id: ClientId(1),
                event: SidebarClientEvent::Key {
                    key: key.to_string(),
                },
            });
        };

        key(&mut state, "j");
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        key(&mut state, "j");
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%2"));
        key(&mut state, "j");
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%2"));
    }

    #[test]
    fn selection_follows_pane_across_triage_and_fleet() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                view_mode: crate::sidebar::state::ViewMode::Flat,
                ..SidebarState::default()
            },
        );
        let mut blocked = agent_pane("main", "%1", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            blocked,
            agent_pane("main", "%2", "running"),
        ]));
        let key = |state: &mut RuntimeState, key: &str| {
            state.apply_event(DaemonEvent::Client {
                client_id: ClientId(1),
                event: SidebarClientEvent::Key {
                    key: key.to_string(),
                },
            });
        };

        key(&mut state, "n");
        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        for _ in 0..2 {
            state.apply_event(DaemonEvent::PanesUpdated(vec![
                agent_pane("main", "%1", "running"),
                agent_pane("main", "%2", "running"),
            ]));
        }

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        assert!(state.rows.iter().any(|row| row.id == "chat::%1"));
    }

    #[test]
    fn client_move_selection_skips_detail_and_jump_rows() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut agent = pane("%1", "/tmp/app", "codex", "running");
        agent.prompt = "prompt".to_string();
        state.ui_state.toggle_expanded("chat::%1");
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent]));

        for _ in 0..3 {
            state.apply_event(DaemonEvent::Client {
                client_id: ClientId(1),
                event: SidebarClientEvent::Key {
                    key: "j".to_string(),
                },
            });
        }

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
    }

    #[test]
    fn client_move_selection_skips_subagent_detail_and_jump_rows() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut agent = pane("%1", "/tmp/app", "codex", "running");
        agent.subagents = "sub12345:Explore|ab120000:general-purpose".to_string();
        state.ui_state.toggle_expanded("chat::%1");
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent]));

        for _ in 0..3 {
            state.apply_event(DaemonEvent::Client {
                client_id: ClientId(1),
                event: SidebarClientEvent::Key {
                    key: "j".to_string(),
                },
            });
        }

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
    }

    #[test]
    fn enter_on_detail_returns_preview_effect() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut agent = pane("%1", "/tmp/app", "codex", "running");
        agent.prompt = "prompt".to_string();
        state.ui_state.toggle_expanded("chat::%1");
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent]));
        state.ui_state.selection = Some("detail::%1::state".to_string());

        let effects = state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "enter".to_string(),
            },
        });

        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::PreviewPane { pane_id, history_lines }
                if pane_id == "%1" && *history_lines == 2000
        )));
    }

    #[test]
    fn client_encoded_toggle_key_toggles_clicked_chat_without_prior_selection() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![pane(
            "%1", "/tmp/app", "codex", "running",
        )]));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "toggle:chat::%1".to_string(),
            },
        });

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        assert!(state.ui_state.is_expanded_with_default("chat::%1", false));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "toggle:chat::%1".to_string(),
            },
        });

        assert!(!state.ui_state.is_expanded_with_default("chat::%1", false));
    }

    #[test]
    fn toggle_on_detail_row_toggles_manual_expand_of_parent_chat() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![pane(
            "%1", "/tmp/app", "codex", "running",
        )]));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "toggle:detail::%1::prompt".to_string(),
            },
        });

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        assert!(state.ui_state.is_expanded_with_default("chat::%1", false));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "toggle:detail::%1::prompt".to_string(),
            },
        });

        assert!(!state.ui_state.is_expanded_with_default("chat::%1", false));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "toggle:meta::%1".to_string(),
            },
        });

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
        assert!(state.ui_state.is_expanded_with_default("chat::%1", false));
    }

    #[test]
    fn client_filter_key_rebuilds_rows_to_attention_only() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            pane("%1", "/tmp/calm", "codex", "idle"),
            pane("%2", "/tmp/active", "codex", "running"),
        ]));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "tab".to_string(),
            },
        });

        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        assert!(rows.iter().all(|row| !row.id.contains("%1")));
        assert!(rows.iter().any(|row| row.id.contains("%2")));
    }

    #[test]
    fn space_on_chat_row_toggles_expand() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        state.ui_state.selection = Some("chat::%1".to_string());

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "space".to_string(),
            },
        });

        assert!(state.ui_state.is_expanded_with_default("chat::%1", false));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "space".to_string(),
            },
        });

        assert!(!state.ui_state.is_expanded_with_default("chat::%1", false));
    }

    #[test]
    fn space_on_repo_row_still_toggles_collapse() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![pane(
            "%1", "/tmp/app", "codex", "running",
        )]));
        state.ui_state.selection = Some("repo::misc::app".to_string());

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "space".to_string(),
            },
        });

        assert!(!state.ui_state.is_expanded("repo::misc::app"));
    }

    #[test]
    fn blocked_pane_enters_triage_immediately() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut blocked = agent_pane("main", "%1", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();

        state.apply_event(DaemonEvent::PanesUpdated(vec![blocked]));

        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        assert_eq!(rows[0].id, "zone::triage");
    }

    #[test]
    fn pane_leaves_triage_after_two_calm_polls() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut blocked = agent_pane("main", "%1", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        state.apply_event(DaemonEvent::PanesUpdated(vec![blocked]));

        let calm = || agent_pane("main", "%1", "running");
        state.apply_event(DaemonEvent::PanesUpdated(vec![calm()]));
        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        assert_eq!(rows[0].id, "zone::triage");

        state.apply_event(DaemonEvent::PanesUpdated(vec![calm()]));
        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        assert!(rows.iter().all(|row| row.id != "zone::triage"));
    }

    #[test]
    fn client_reorder_key_seeds_and_moves_manual_order() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            pane("%1", "/tmp/alpha", "codex", "idle"),
            pane("%2", "/tmp/zeta", "codex", "idle"),
        ]));
        state.ui_state.selection = Some("repo::misc::zeta".to_string());

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "K".to_string(),
            },
        });

        assert_eq!(
            state.ui_state.manual_order,
            vec![RepoId::new("misc", "zeta"), RepoId::new("misc", "alpha")]
        );
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
            without_heartbeat(&effects),
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "●".to_string(),
                state: "working".to_string(),
            }]
        );
    }

    #[test]
    fn session_badge_effect_carries_structured_state() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::SetSessionBadge { session, value, state }
                if session == "main" && value.starts_with('●') && state == "working"
        )));
    }

    #[test]
    fn hook_agent_option_keeps_session_badge_when_command_is_shell() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let mut hook_marked = agent_pane("main", "%1", "running");
        hook_marked.current_command = "zsh".to_string();

        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![hook_marked]));

        assert!(!effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::ClearSessionBadge { session } if session == "main"
        )));
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
        assert!(without_heartbeat(&effects).is_empty());
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
            without_heartbeat(&effects),
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "✓".to_string(),
                state: "done".to_string(),
            }]
        );

        let mut viewed = agent_pane("main", "%1", "idle");
        viewed.window_active = true;
        viewed.session_attached = true;
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![viewed]));
        assert_eq!(
            without_heartbeat(&effects),
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "○".to_string(),
                state: "idle".to_string(),
            }]
        );
    }

    #[test]
    fn jump_clears_unread_immediately() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));

        let effects = state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::JumpPane {
                pane: "%1".to_string(),
            },
        });

        assert!(effects.contains(&RuntimeEffect::JumpPane("%1".to_string())));
        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert_eq!(
            chat.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Idle)
        );
    }

    #[test]
    fn first_seen_idle_pane_is_not_unread() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));
        assert_eq!(
            without_heartbeat(&effects),
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "○".to_string(),
                state: "idle".to_string(),
            }]
        );
    }

    #[test]
    fn hide_idle_config_clears_idle_session_badge() {
        let mut config = Config::default();
        config.statusline.session_badge.hide_idle = true;
        let mut state = RuntimeState::new(config, SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));
        assert!(
            !without_heartbeat(&effects)
                .iter()
                .any(|effect| matches!(effect, RuntimeEffect::SetSessionBadge { .. }))
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
            without_heartbeat(&effects),
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "▲".to_string(),
                state: "blocked".to_string(),
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
            without_heartbeat(&effects),
            vec![
                RuntimeEffect::SetSessionBadge {
                    session: "alpha".to_string(),
                    value: "●".to_string(),
                    state: "working".to_string(),
                },
                RuntimeEffect::SetSessionBadge {
                    session: "beta".to_string(),
                    value: "○".to_string(),
                    state: "idle".to_string(),
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
            without_heartbeat(&effects),
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
            vec![
                RuntimeEffect::ClearSessionBadge {
                    session: "main".to_string(),
                },
                RuntimeEffect::ClearHeartbeat
            ]
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
        assert!(without_heartbeat(&effects).is_empty());
    }

    #[test]
    fn sidebar_and_agentless_panes_are_ignored() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut sidebar = agent_pane("main", "%9", "running");
        sidebar.is_sidebar = true;
        let mut plain = agent_pane("main", "%8", "");
        plain.agent = String::new();
        plain.current_command = "zsh".to_string();
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![sidebar, plain]));
        assert!(without_heartbeat(&effects).is_empty());
    }

    #[test]
    fn summary_query_hides_idle_when_configured() {
        let mut config = Config::default();
        config.statusline.summary.hide_idle = true;
        let mut state = RuntimeState::new(config, SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "idle"),
            agent_pane("main", "%2", "running"),
        ]));
        let (reply, receiver) = std::sync::mpsc::channel();
        state.apply_event(DaemonEvent::QuerySummary { reply });
        let ServerMessage::Summary { text } = receiver.recv().unwrap() else {
            panic!("expected summary");
        };
        assert_eq!(text, "#[fg=#4fd08a]●1#[default]");
    }

    #[test]
    fn summary_query_counts_unread_as_done() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "idle"),
            agent_pane("main", "%2", "running"),
        ]));

        let (reply, receiver) = std::sync::mpsc::channel();
        state.apply_event(DaemonEvent::QuerySummary { reply });
        let message = receiver.recv().unwrap();
        assert_eq!(
            message,
            ServerMessage::Summary {
                text: "#[fg=#4fd08a]●1#[default] #[fg=#45cbe6]✓1#[default]".to_string()
            }
        );
    }

    #[test]
    fn summary_query_returns_empty_when_disabled() {
        let mut config = Config::default();
        config.statusline.summary.enabled = false;
        let mut state = RuntimeState::new(config, SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));

        let (reply, receiver) = std::sync::mpsc::channel();
        state.apply_event(DaemonEvent::QuerySummary { reply });

        assert_eq!(
            receiver.recv().unwrap(),
            ServerMessage::Summary {
                text: String::new()
            }
        );
    }

    #[test]
    fn attention_names_oldest_hidden_blocked_session() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let now = crate::sidebar::tree::now_epoch_secs();
        let mut blocked_old = agent_pane("proxy", "%1", "waiting");
        blocked_old.wait_reason = "permission_prompt".to_string();
        blocked_old.started_at = (now - 120).to_string();
        let mut blocked_new = agent_pane("etl", "%2", "waiting");
        blocked_new.wait_reason = "permission_prompt".to_string();
        blocked_new.started_at = (now - 30).to_string();
        let mut visible = agent_pane("main", "%3", "waiting");
        visible.wait_reason = "permission_prompt".to_string();
        visible.window_active = true;
        visible.session_attached = true;
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            blocked_old,
            blocked_new,
            visible,
        ]));

        let (reply, receiver) = std::sync::mpsc::channel();
        state.apply_event(DaemonEvent::QueryAttention { reply });
        let ServerMessage::Attention { text } = receiver.recv().unwrap() else {
            panic!("expected attention");
        };
        assert!(text.contains("▲ proxy · perm 2m"), "{text}");
        assert!(text.contains("+1"), "{text}");
        assert!(!text.contains("main"), "{text}");
    }

    #[test]
    fn attention_is_empty_without_hidden_blocked() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut visible = agent_pane("main", "%1", "waiting");
        visible.wait_reason = "permission_prompt".to_string();
        visible.window_active = true;
        visible.session_attached = true;
        state.apply_event(DaemonEvent::PanesUpdated(vec![visible]));

        let (reply, receiver) = std::sync::mpsc::channel();
        state.apply_event(DaemonEvent::QueryAttention { reply });

        assert_eq!(
            receiver.recv().unwrap(),
            ServerMessage::Attention {
                text: String::new()
            }
        );
    }

    #[test]
    fn panes_updated_emits_heartbeat_effect() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![]));

        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::Heartbeat(epoch) if *epoch > 0
        )));
    }

    #[test]
    fn panes_updated_deduplicates_heartbeat_within_same_epoch() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let first = state.apply_event(DaemonEvent::PanesUpdated(vec![]));
        let second = state.apply_event(DaemonEvent::PanesUpdated(vec![]));

        assert_eq!(
            first
                .iter()
                .filter(|effect| matches!(effect, RuntimeEffect::Heartbeat(_)))
                .count(),
            1
        );
        assert!(
            !second
                .iter()
                .any(|effect| matches!(effect, RuntimeEffect::Heartbeat(_))),
            "{second:?}"
        );
    }
}

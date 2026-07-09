use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::category::resolve_dynamic_category_for_session;
use crate::config::{Config, DoneClearOn};
use crate::daemon::protocol::{ServerMessage, SidebarClientEvent};
use crate::daemon::session_badge::{BadgeState, BadgeStateCounts, badge_state};
use crate::daemon::{
    DaemonSnapshot, SidebarFrame, TransitionEvent, build_snapshot_with_sidebar, format_attention,
    render_summary,
};
use crate::git::{GitBadge, WorktreeInfo};
use crate::options::snapshot::{PaneSnapshot, effective_agent, has_pane_state, is_live_agent_pane};
use crate::session::SessionInfo;
use crate::sidebar::input::{SidebarCommand, SidebarInputAction, activate_selected};
use crate::sidebar::state::{RepoId, SidebarAction, SidebarState};
use crate::sidebar::tree::{
    BadgeCounts, RowBuildContext, SidebarRow, SidebarRowKind, build_rows_ctx,
    category_row_id_for_pane, chat_row_id, now_epoch_secs, repo_row_id_for_pane, row_refs,
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
    GitStatusUpdated {
        badges: BTreeMap<String, GitBadge>,
        worktrees: BTreeMap<String, WorktreeInfo>,
    },
    QuerySummary {
        reply: Sender<ServerMessage>,
    },
    QueryAttention {
        reply: Sender<ServerMessage>,
    },
    RefreshPanes {
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
    MarkPaneDone {
        pane_id: String,
        completed_at: i64,
    },
    SaveState(SidebarState),
    SetSessionBadge {
        session: String,
        value: String,
        state: String,
    },
    SetSessionProjectPath {
        session: String,
        path: String,
    },
    SetSessionCategory {
        session: String,
        category: String,
    },
    SetSessionAgentCounts {
        session: String,
        counts: String,
    },
    ClearSessionBadge {
        session: String,
    },
    ClearSessionAgentCounts {
        session: String,
    },
    SetWindowBadge {
        window: String,
        value: String,
        state: String,
        counts: String,
    },
    ClearWindowBadge {
        window: String,
    },
    ClearPaneState {
        pane_id: String,
    },
    Notify {
        pane_id: String,
        agent: String,
        state: BadgeState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionContext {
    pane: Option<String>,
    session: Option<String>,
}

impl SelectionContext {
    fn new(pane: Option<String>, session: Option<String>) -> Option<Self> {
        let pane = normalize_context_value(pane);
        let session = normalize_context_value(session);
        (pane.is_some() || session.is_some()).then_some(Self { pane, session })
    }
}

fn normalize_context_value(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn apply_manual_done_to_pane(pane: &mut PaneSnapshot, completed_at: i64) {
    pane.status = "idle".to_string();
    pane.wait_reason.clear();
    pane.attention = "1".to_string();
    pane.completed_at = completed_at.to_string();
    pane.tasks.clear();
    pane.task_items.clear();
    pane.subagents.clear();
}

fn pane_is_better_session_representative(candidate: &PaneSnapshot, current: &PaneSnapshot) -> bool {
    pane_representative_rank(candidate) > pane_representative_rank(current)
}

fn pane_representative_rank(pane: &PaneSnapshot) -> u8 {
    match (pane.window_active, pane.pane_active) {
        (true, true) => 3,
        (true, false) => 2,
        (false, true) => 1,
        (false, false) => 0,
    }
}

fn pane_has_acknowledged_done(pane: &PaneSnapshot, clear_on: DoneClearOn) -> bool {
    pane.session_attached
        && pane.window_active
        && match clear_on {
            DoneClearOn::Window => true,
            DoneClearOn::Pane => pane.pane_active,
        }
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
    counts: BadgeCounts,
    rows: Vec<String>,
}

#[derive(Debug)]
pub struct RuntimeState {
    config: Config,
    pub ui_state: SidebarState,
    pub panes: Vec<PaneSnapshot>,
    git_badges: BTreeMap<String, GitBadge>,
    worktrees: BTreeMap<String, WorktreeInfo>,
    rows: Vec<SidebarRow>,
    counts: BadgeCounts,
    snapshot: Option<DaemonSnapshot>,
    clients: BTreeMap<ClientId, Arc<LatestSlot<ServerMessage>>>,
    last_pushed: Option<PushFingerprint>,
    running: bool,
    dirty_state_since: Option<Instant>,
    pane_was_idle: BTreeMap<String, bool>,
    unread: BTreeMap<String, bool>,
    manual_done: BTreeMap<String, i64>,
    triage: BTreeSet<String>,
    calm_streak: BTreeMap<String, u8>,
    prev_badges: BTreeMap<String, BadgeState>,
    events: VecDeque<TransitionEvent>,
    flash: BTreeMap<String, u8>,
    written_badges: BTreeMap<String, (String, String)>,
    written_session_counts: BTreeMap<String, String>,
    written_window_badges: BTreeMap<String, (String, String, String)>,
    pending_selection_context: Option<SelectionContext>,
}

impl RuntimeState {
    pub fn new(config: Config, ui_state: SidebarState) -> Self {
        Self {
            config,
            ui_state,
            panes: Vec::new(),
            git_badges: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            rows: Vec::new(),
            counts: BadgeCounts::default(),
            snapshot: None,
            clients: BTreeMap::new(),
            last_pushed: None,
            running: true,
            dirty_state_since: None,
            pane_was_idle: BTreeMap::new(),
            unread: BTreeMap::new(),
            manual_done: BTreeMap::new(),
            triage: BTreeSet::new(),
            calm_streak: BTreeMap::new(),
            prev_badges: BTreeMap::new(),
            events: VecDeque::new(),
            flash: BTreeMap::new(),
            written_badges: BTreeMap::new(),
            written_session_counts: BTreeMap::new(),
            written_window_badges: BTreeMap::new(),
            pending_selection_context: None,
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
                let mut panes = panes;
                self.apply_manual_done_overrides(&mut panes);
                self.panes = panes;
                let clear_pane_state_effects = self.clear_stale_pane_state_effects();
                self.update_unread();
                self.update_triage();
                let transition_effects = self.update_transitions();
                self.rebuild_snapshot();
                if self.apply_pending_selection_context() {
                    self.mark_state_dirty(Instant::now());
                    self.rebuild_snapshot();
                }
                self.broadcast_if_needed();
                let mut effects = clear_pane_state_effects;
                effects.extend(self.sync_session_categories());
                effects.extend(transition_effects);
                effects.extend(self.sync_session_badges());
                effects.extend(self.sync_window_badges());
                effects
            }
            DaemonEvent::GitStatusUpdated { badges, worktrees } => {
                self.git_badges = badges;
                self.worktrees = worktrees;
                self.rebuild_snapshot();
                if self.apply_pending_selection_context() {
                    self.mark_state_dirty(Instant::now());
                    self.rebuild_snapshot();
                }
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
            DaemonEvent::RefreshPanes { reply } => {
                let _ = reply.send(ServerMessage::Error {
                    message: "refresh_panes requires runtime loop".to_string(),
                });
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
                effects.extend(self.written_session_counts.keys().map(|session| {
                    RuntimeEffect::ClearSessionAgentCounts {
                        session: session.clone(),
                    }
                }));
                effects.extend(self.written_window_badges.keys().map(|window| {
                    RuntimeEffect::ClearWindowBadge {
                        window: window.clone(),
                    }
                }));
                self.written_badges.clear();
                self.written_session_counts.clear();
                self.written_window_badges.clear();
                effects
            }
        }
    }

    pub fn rebuild_snapshot(&mut self) {
        let (rows, counts) = build_rows_ctx(
            &self.config,
            &self.panes,
            &self.ui_state,
            &RowBuildContext {
                git: self.git_badges.clone(),
                worktrees: self.worktrees.clone(),
                unread: self.unread.clone(),
                triage: self.triage.clone(),
                flash: self.flash.keys().cloned().collect(),
                now: now_epoch_secs(),
            },
        );
        self.rows = rows;
        self.counts = counts;
        let sidebar = SidebarFrame {
            state: self.ui_state.clone(),
            counts: self.counts,
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
            counts: self.counts,
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
                self.manual_done.remove(&pane);
                self.unread.insert(pane.clone(), false);
                self.ui_state.selection = Some(format!("chat::{pane}"));
                self.mark_state_dirty(Instant::now());
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                vec![RuntimeEffect::JumpPane(pane)]
            }
            SidebarClientEvent::MarkDone { pane } => {
                let completed_at = now_epoch_secs();
                if !self.mark_pane_done(&pane, completed_at) {
                    return Vec::new();
                }
                self.triage.remove(&pane);
                self.calm_streak.remove(&pane);
                self.update_triage();
                let mut effects = vec![RuntimeEffect::MarkPaneDone {
                    pane_id: pane,
                    completed_at,
                }];
                effects.extend(self.update_transitions());
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                effects.extend(self.sync_session_badges());
                effects.extend(self.sync_window_badges());
                effects
            }
            SidebarClientEvent::SelectContext { pane, session } => {
                let Some(context) = SelectionContext::new(pane, session) else {
                    return Vec::new();
                };
                self.pending_selection_context = Some(context);
                if self.apply_pending_selection_context() {
                    self.mark_state_dirty(Instant::now());
                    self.rebuild_snapshot();
                    self.broadcast_if_needed();
                }
                Vec::new()
            }
        }
    }

    fn mark_pane_done(&mut self, pane_id: &str, completed_at: i64) -> bool {
        let Some(pane) = self.panes.iter_mut().find(|pane| pane.pane_id == pane_id) else {
            return false;
        };
        apply_manual_done_to_pane(pane, completed_at);
        self.manual_done.insert(pane_id.to_string(), completed_at);
        self.unread.insert(pane_id.to_string(), true);
        self.pane_was_idle.insert(pane_id.to_string(), true);
        true
    }

    fn apply_manual_done_overrides(&mut self, panes: &mut [PaneSnapshot]) {
        let mut next_manual_done = BTreeMap::new();
        for pane in panes {
            let Some(&completed_at) = self.manual_done.get(&pane.pane_id) else {
                continue;
            };
            if !is_live_agent_pane(pane) {
                continue;
            }
            let started_after_mark_done = pane
                .started_at
                .trim()
                .parse::<i64>()
                .is_ok_and(|started_at| started_at > completed_at);
            if started_after_mark_done {
                continue;
            }
            apply_manual_done_to_pane(pane, completed_at);
            next_manual_done.insert(pane.pane_id.clone(), completed_at);
        }
        self.manual_done = next_manual_done;
    }

    fn apply_pending_selection_context(&mut self) -> bool {
        let Some(context) = self.pending_selection_context.clone() else {
            return false;
        };
        let Some(selection) = self.selection_for_context(&context) else {
            return false;
        };
        self.pending_selection_context = None;
        if self.ui_state.selection.as_deref() == Some(selection.as_str()) {
            return false;
        }
        self.ui_state.selection = Some(selection);
        self.ui_state.version += 1;
        true
    }

    fn selection_for_context(&self, context: &SelectionContext) -> Option<String> {
        if let Some(pane_id) = context.pane.as_deref()
            && let Some(selection) = self.selection_for_pane(pane_id)
        {
            return Some(selection);
        }
        let session = context.session.as_deref()?;
        if let Some(row) = self.rows.iter().find(|row| {
            row.kind == SidebarRowKind::Chat
                && row
                    .pane_id
                    .as_deref()
                    .and_then(|pane_id| self.pane_for_id(pane_id))
                    .map(|pane| pane.session == session)
                    .unwrap_or(false)
        }) {
            return Some(row.id.clone());
        }
        self.panes
            .iter()
            .filter(|pane| pane.session == session && is_live_agent_pane(pane))
            .find_map(|pane| self.selection_for_pane(&pane.pane_id))
    }

    fn selection_for_pane(&self, pane_id: &str) -> Option<String> {
        let pane = self.pane_for_id(pane_id)?;
        if !is_live_agent_pane(pane) {
            return None;
        }
        let candidates = [
            chat_row_id(&pane.pane_id),
            repo_row_id_for_pane(&self.config, pane),
            category_row_id_for_pane(&self.config, pane),
        ];
        candidates.into_iter().find(|candidate| {
            self.rows
                .iter()
                .any(|row| row.id.as_str() == candidate.as_str())
        })
    }

    fn pane_for_id(&self, pane_id: &str) -> Option<&PaneSnapshot> {
        self.panes.iter().find(|pane| pane.pane_id == pane_id)
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
            SidebarInputAction::SetFilter(filter) => {
                if self.counts.filter_is_available(filter) {
                    self.ui_state.set_filter(filter)
                } else {
                    false
                }
            }
            SidebarInputAction::ToggleFilter => {
                let filter = next_available_filter(self.ui_state.filter, self.counts);
                self.ui_state.set_filter(filter)
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
                        self.manual_done.remove(&pane_id);
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
        if let Some(pane_id) = self.selected_chat_pane_id() {
            self.seed_manual_chat_order_from_rows();
            return if up {
                self.ui_state.manual_chat_move_up(&pane_id)
            } else {
                self.ui_state.manual_chat_move_down(&pane_id)
            };
        }
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

    fn selected_chat_pane_id(&self) -> Option<String> {
        let selection = self.ui_state.selection.as_deref()?;
        let row = self.rows.iter().find(|row| row.id == selection)?;
        (row.kind == SidebarRowKind::Chat)
            .then(|| row.pane_id.clone())
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

    fn seed_manual_chat_order_from_rows(&mut self) {
        let mut changed = false;
        for row in self
            .rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Chat)
        {
            if let Some(pane_id) = row.pane_id.as_deref()
                && !self
                    .ui_state
                    .manual_chat_order
                    .iter()
                    .any(|existing| existing == pane_id)
            {
                self.ui_state.manual_chat_order.push(pane_id.to_string());
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
        let mut next_manual_done = BTreeMap::new();
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let is_idle = level == crate::hook::RollupLevel::Idle;
            let manual_done_completed_at = self.manual_done.get(&pane.pane_id).copied();
            let manual_done = manual_done_completed_at.is_some() && is_idle;
            let was_idle = self.pane_was_idle.get(&pane.pane_id).copied();
            let mut unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            match was_idle {
                None => unread = false,
                Some(false) if is_idle => unread = true,
                _ => {}
            }
            if manual_done {
                unread = true;
                next_manual_done.insert(pane.pane_id.clone(), manual_done_completed_at.unwrap());
            }
            if !is_idle {
                unread = false;
            }
            if pane_has_acknowledged_done(pane, self.config.daemon.done_clear_on) && !manual_done {
                unread = false;
            }
            next_was_idle.insert(pane.pane_id.clone(), is_idle);
            next_unread.insert(pane.pane_id.clone(), unread);
        }
        self.pane_was_idle = next_was_idle;
        self.unread = next_unread;
        self.manual_done = next_manual_done;
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
        use crate::daemon::session_badge::badge_value_from_counts;

        let badge_config = &self.config.statusline.session_badge;
        let badge_glyphs = &self.config.badge.glyphs;
        let mut desired = BTreeMap::new();
        let mut desired_counts = BTreeMap::new();
        let session_counts =
            self.badge_counts_by(|pane| (!pane.session.is_empty()).then_some(pane.session.clone()));

        if badge_config.enabled {
            for (session, counts) in &session_counts {
                let state = counts.rollup_state().unwrap_or(BadgeState::Idle);
                if let Some(value) = badge_value_from_counts(
                    *counts,
                    badge_glyphs,
                    badge_config.mode,
                    &badge_config.suffix,
                    badge_config.hide_idle,
                ) {
                    desired.insert(session.clone(), (value, state.as_str().to_string()));
                }
            }
        }
        if self.config.statusline.category.agent_badge.enabled {
            desired_counts.extend(
                session_counts
                    .into_iter()
                    .filter(|(_, counts)| counts.total() > 0)
                    .map(|(session, counts)| (session, counts.encode())),
            );
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
        for (session, counts) in &desired_counts {
            if self.written_session_counts.get(session) != Some(counts) {
                effects.push(RuntimeEffect::SetSessionAgentCounts {
                    session: session.clone(),
                    counts: counts.clone(),
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
        for session in self.written_session_counts.keys() {
            if !desired_counts.contains_key(session) {
                effects.push(RuntimeEffect::ClearSessionAgentCounts {
                    session: session.clone(),
                });
            }
        }
        self.written_badges = desired;
        self.written_session_counts = desired_counts;
        effects
    }

    fn sync_session_categories(&self) -> Vec<RuntimeEffect> {
        let mut by_session = BTreeMap::<String, &PaneSnapshot>::new();
        for pane in self.panes.iter().filter(|pane| {
            !pane.session.is_empty()
                && !pane.current_path.is_empty()
                && !pane.is_sidebar
                && pane.window_active
                && pane.pane_active
                && pane.session_category_override.is_empty()
        }) {
            by_session
                .entry(pane.session.clone())
                .and_modify(|current| {
                    if pane_is_better_session_representative(pane, current) {
                        *current = pane;
                    }
                })
                .or_insert(pane);
        }

        let mut effects = Vec::new();
        for (session, pane) in by_session {
            let session_info = SessionInfo {
                name: session.clone(),
                category: pane.session_category.clone(),
                project_path: pane.current_path.clone(),
                category_override: pane.session_category_override.clone(),
                ..SessionInfo::default()
            };
            let category = resolve_dynamic_category_for_session(&self.config, &session_info);
            if pane.session_project_path != pane.current_path {
                effects.push(RuntimeEffect::SetSessionProjectPath {
                    session: session.clone(),
                    path: pane.current_path.clone(),
                });
            }
            if pane.session_category != category {
                effects.push(RuntimeEffect::SetSessionCategory { session, category });
            }
        }
        effects
    }

    fn sync_window_badges(&mut self) -> Vec<RuntimeEffect> {
        use crate::daemon::session_badge::agent_badge_value_from_counts;

        let badge_config = &self.config.statusline.windows.agent_badge;
        let mut desired = BTreeMap::new();
        if badge_config.enabled {
            for (window, counts) in self.badge_counts_by(|pane| {
                (!pane.window_id.is_empty()).then_some(pane.window_id.clone())
            }) {
                let state = counts.rollup_state().unwrap_or(BadgeState::Idle);
                if let Some(value) =
                    agent_badge_value_from_counts(counts, &self.config.badge.glyphs, badge_config)
                {
                    desired.insert(window, (value, state.as_str().to_string(), counts.encode()));
                }
            }
        }

        let mut effects = Vec::new();
        for (window, (value, state, counts)) in &desired {
            if self.written_window_badges.get(window)
                != Some(&(value.clone(), state.clone(), counts.clone()))
            {
                effects.push(RuntimeEffect::SetWindowBadge {
                    window: window.clone(),
                    value: value.clone(),
                    state: state.clone(),
                    counts: counts.clone(),
                });
            }
        }
        for window in self.written_window_badges.keys() {
            if !desired.contains_key(window) {
                effects.push(RuntimeEffect::ClearWindowBadge {
                    window: window.clone(),
                });
            }
        }
        self.written_window_badges = desired;
        effects
    }

    fn badge_counts_by(
        &self,
        key_for_pane: impl Fn(&PaneSnapshot) -> Option<String>,
    ) -> BTreeMap<String, BadgeStateCounts> {
        let mut counts = BTreeMap::new();
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
            let Some(key) = key_for_pane(pane) else {
                continue;
            };
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            counts
                .entry(key)
                .or_insert_with(BadgeStateCounts::default)
                .push(badge_state(level, unread));
        }
        counts
    }

    fn clear_stale_pane_state_effects(&self) -> Vec<RuntimeEffect> {
        self.panes
            .iter()
            .filter(|pane| !pane.is_sidebar)
            .filter(|pane| !is_live_agent_pane(pane))
            .filter(|pane| has_pane_state(pane))
            .map(|pane| RuntimeEffect::ClearPaneState {
                pane_id: pane.pane_id.clone(),
            })
            .collect()
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

fn next_available_filter(
    current: crate::sidebar::state::StatusFilter,
    counts: BadgeCounts,
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
    use crate::config::Config;
    use crate::daemon::protocol::{ServerMessage, SidebarClientEvent};
    use crate::options::snapshot::PaneSnapshot;
    use crate::sidebar::state::{SidebarState, StatusFilter};
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
            pane_active: false,
            session_attached: false,
            is_sidebar: false,
            session_category: String::new(),
            session_project_path: String::new(),
            session_category_override: String::new(),
            agent: "codex".to_string(),
            status: status.to_string(),
            prompt: String::new(),
            prompt_source: String::new(),
            wait_reason: String::new(),
            attention: String::new(),
            started_at: String::new(),
            completed_at: String::new(),
            tasks: String::new(),
            task_items: String::new(),
            subagents: String::new(),
            worktree_activity: String::new(),
            agent_observed: true,
        }
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

    fn client_key(state: &mut RuntimeState, key: &str) {
        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: key.to_string(),
            },
        });
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
    fn git_status_updated_stores_worktree_info() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let worktree = WorktreeInfo {
            name: "feature".to_string(),
            path: "/tmp/worktrees/feature".to_string(),
            source: crate::git::WorktreeSource::GitLinked,
            branch: None,
            dirty: None,
            locked: None,
        };

        state.apply_event(DaemonEvent::GitStatusUpdated {
            badges: BTreeMap::new(),
            worktrees: BTreeMap::from([("/tmp/worktrees/feature".to_string(), worktree.clone())]),
        });

        assert_eq!(state.worktrees["/tmp/worktrees/feature"], worktree);
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
    fn stale_agent_pane_state_emits_clear_effect() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut stale = agent_pane("main", "%1", "running");
        stale.current_command = "zsh".to_string();
        stale.agent_observed = false;
        stale.prompt = "old prompt".to_string();
        stale.started_at = "1720000000".to_string();
        stale.tasks = "1/2".to_string();

        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![stale]));

        assert!(effects.contains(&RuntimeEffect::ClearPaneState {
            pane_id: "%1".to_string(),
        }));
    }

    #[test]
    fn plain_shell_pane_without_agent_state_does_not_emit_clear_effect() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![PaneSnapshot {
            session: "main".to_string(),
            window_id: "@1".to_string(),
            pane_id: "%1".to_string(),
            current_path: "/tmp/app".to_string(),
            current_command: "zsh".to_string(),
            ..PaneSnapshot::default()
        }]));

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RuntimeEffect::ClearPaneState { .. }))
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
    fn select_context_prefers_exact_visible_pane() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "running"),
            agent_pane("main", "%2", "running"),
        ]));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::SelectContext {
                pane: Some("%2".to_string()),
                session: Some("main".to_string()),
            },
        });

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%2"));
        assert!(state.state_dirty_since().is_some());
    }

    #[test]
    fn select_context_falls_back_to_session_agent_when_pane_is_not_sidebar_row() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            PaneSnapshot {
                session: "main".to_string(),
                window_id: "@1".to_string(),
                pane_id: "%shell".to_string(),
                current_path: "/tmp/shell".to_string(),
                current_command: "zsh".to_string(),
                ..PaneSnapshot::default()
            },
            agent_pane("main", "%agent", "running"),
            agent_pane("other", "%other", "running"),
        ]));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::SelectContext {
                pane: Some("%shell".to_string()),
                session: Some("main".to_string()),
            },
        });

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%agent"));
    }

    #[test]
    fn select_context_waits_for_rows_when_panes_are_not_loaded_yet() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::SelectContext {
                pane: Some("%1".to_string()),
                session: Some("main".to_string()),
            },
        });
        assert_eq!(state.ui_state.selection, None);

        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));

        assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
    }

    #[test]
    fn select_context_uses_visible_group_when_chat_row_is_collapsed() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                collapsed: BTreeSet::from(["repo::misc::tmp".to_string()]),
                ..SidebarState::default()
            },
        );
        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::SelectContext {
                pane: Some("%1".to_string()),
                session: Some("main".to_string()),
            },
        });

        assert_eq!(state.ui_state.selection.as_deref(), Some("repo::misc::tmp"));
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
        state.ui_state.selection = Some("detail::%1::prompt".to_string());

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
        let mut blocked = pane("%2", "/tmp/active", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            pane("%1", "/tmp/calm", "codex", "idle"),
            blocked,
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
    fn set_filter_ignores_zero_count_status_filter() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                filter: StatusFilter::WorkingOnly,
                ..SidebarState::default()
            },
        );
        state.counts = BadgeCounts {
            total: 1,
            working: 1,
            done: 0,
            ..BadgeCounts::default()
        };

        client_key(&mut state, "done");

        assert_eq!(state.ui_state.filter, StatusFilter::WorkingOnly);
        assert_eq!(state.ui_state.version, 0);
        assert!(state.state_dirty_since().is_none());
    }

    #[test]
    fn set_filter_all_is_always_allowed() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                filter: StatusFilter::WorkingOnly,
                ..SidebarState::default()
            },
        );
        state.counts = BadgeCounts::default();

        client_key(&mut state, "all");

        assert_eq!(state.ui_state.filter, StatusFilter::All);
        assert_eq!(state.ui_state.version, 1);
    }

    #[test]
    fn toggle_filter_skips_zero_count_statuses() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                filter: StatusFilter::WorkingOnly,
                ..SidebarState::default()
            },
        );
        state.counts = BadgeCounts {
            total: 2,
            working: 1,
            done: 0,
            idle: 1,
            ..BadgeCounts::default()
        };

        client_key(&mut state, "tab");

        assert_eq!(state.ui_state.filter, StatusFilter::IdleOnly);
    }

    #[test]
    fn attention_filter_is_unavailable_when_only_working_panes_match() {
        let counts = BadgeCounts {
            total: 2,
            attention: 0,
            blocked: 0,
            working: 2,
            ..BadgeCounts::default()
        };

        assert!(!counts.filter_is_available(StatusFilter::AttentionOnly));
    }

    #[test]
    fn toggle_filter_skips_attention_when_only_working_panes_match() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                filter: StatusFilter::All,
                ..SidebarState::default()
            },
        );
        state.counts = BadgeCounts {
            total: 2,
            attention: 0,
            blocked: 0,
            working: 2,
            ..BadgeCounts::default()
        };

        client_key(&mut state, "tab");

        assert_eq!(state.ui_state.filter, StatusFilter::WorkingOnly);
    }

    #[test]
    fn toggle_filter_converges_to_all_when_all_status_counts_are_zero() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                filter: StatusFilter::WorkingOnly,
                ..SidebarState::default()
            },
        );
        state.counts = BadgeCounts::default();

        client_key(&mut state, "tab");

        assert_eq!(state.ui_state.filter, StatusFilter::All);
    }

    #[test]
    fn filter_is_not_reset_when_active_filter_count_becomes_zero() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                filter: StatusFilter::AttentionOnly,
                ..SidebarState::default()
            },
        );

        state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));

        assert_eq!(state.ui_state.filter, StatusFilter::AttentionOnly);
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
    fn client_reorder_key_seeds_and_moves_manual_chat_order() {
        let mut state = RuntimeState::new(
            Config::default(),
            SidebarState {
                view_mode: crate::sidebar::state::ViewMode::Flat,
                ..SidebarState::default()
            },
        );
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            pane("%1", "/tmp/app", "codex", "idle"),
            pane("%2", "/tmp/app", "claude", "idle"),
        ]));
        state.ui_state.selection = Some("chat::%2".to_string());

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: "K".to_string(),
            },
        });

        assert_eq!(state.ui_state.manual_chat_order, vec!["%2", "%1"]);
        let chat_ids = state
            .rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Chat)
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(chat_ids, vec!["chat::%2", "chat::%1"]);
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
                value: "✓".to_string(),
                state: "done".to_string(),
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
                value: "○".to_string(),
                state: "idle".to_string(),
            }]
        );
    }

    #[test]
    fn pane_done_clear_requires_pane_focus_when_configured() {
        let mut config = Config::default();
        config.daemon.done_clear_on = DoneClearOn::Pane;
        let mut state = RuntimeState::new(config, SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "idle",
        )]));

        let mut viewed_window = agent_pane("main", "%1", "idle");
        viewed_window.window_active = true;
        viewed_window.session_attached = true;
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![viewed_window]));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::SetSessionBadge { state, .. } if state == "idle"
        )));
        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert_eq!(
            chat.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Done)
        );

        let mut focused_pane = agent_pane("main", "%1", "idle");
        focused_pane.window_active = true;
        focused_pane.pane_active = true;
        focused_pane.session_attached = true;
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![focused_pane]));
        assert!(effects.contains(&RuntimeEffect::SetSessionBadge {
            session: "main".to_string(),
            value: "○".to_string(),
            state: "idle".to_string(),
        }));
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
    fn mark_done_updates_pane_and_resets_task_state() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut pane = agent_pane("main", "%1", "running");
        pane.window_active = true;
        pane.session_attached = true;
        pane.wait_reason = "permission_prompt".to_string();
        pane.tasks = "1/3".to_string();
        pane.task_items = "[]".to_string();
        pane.subagents = "agent1234:Explore".to_string();
        state.apply_event(DaemonEvent::PanesUpdated(vec![pane]));

        let effects = state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::MarkDone {
                pane: "%1".to_string(),
            },
        });

        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::MarkPaneDone { pane_id, completed_at }
                if pane_id == "%1" && *completed_at > 0
        )));
        let pane = state
            .panes
            .iter()
            .find(|pane| pane.pane_id == "%1")
            .unwrap();
        assert_eq!(pane.status, "idle");
        assert_eq!(pane.wait_reason, "");
        assert_eq!(pane.tasks, "");
        assert_eq!(pane.task_items, "");
        assert_eq!(pane.subagents, "");
        assert!(!pane.completed_at.is_empty());

        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert_eq!(
            chat.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Done)
        );

        let refreshed_panes = state.panes.clone();
        state.apply_event(DaemonEvent::PanesUpdated(refreshed_panes));
        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert_eq!(
            chat.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Done)
        );

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::JumpPane {
                pane: "%1".to_string(),
            },
        });
        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert_eq!(
            chat.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Idle)
        );
    }

    #[test]
    fn mark_done_keeps_done_across_stale_running_update() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut running = agent_pane("main", "%1", "running");
        running.started_at = "100".to_string();
        state.apply_event(DaemonEvent::PanesUpdated(vec![running.clone()]));

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::MarkDone {
                pane: "%1".to_string(),
            },
        });
        let completed_at = state.panes[0].completed_at.parse::<i64>().unwrap();

        state.apply_event(DaemonEvent::PanesUpdated(vec![running]));
        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert_eq!(
            chat.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Done)
        );

        let mut new_running = agent_pane("main", "%1", "running");
        new_running.started_at = (completed_at + 1).to_string();
        state.apply_event(DaemonEvent::PanesUpdated(vec![new_running]));
        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert_eq!(
            chat.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Working)
        );
    }

    #[test]
    fn mark_done_removes_blocked_pane_from_triage_immediately() {
        let mut state = RuntimeState::new(Config::default(), SidebarState::default());
        let mut blocked = agent_pane("main", "%1", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        state.apply_event(DaemonEvent::PanesUpdated(vec![blocked]));
        assert!(
            state
                .snapshot()
                .unwrap()
                .sidebar
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .any(|row| row.id == "zone::triage")
        );

        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::MarkDone {
                pane: "%1".to_string(),
            },
        });

        let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
        assert!(rows.iter().all(|row| row.id != "zone::triage"));
        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert_eq!(
            chat.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Done)
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
            !effects
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
            effects,
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "▲".to_string(),
                state: "blocked".to_string(),
            }]
        );
    }

    #[test]
    fn session_badge_counts_mode_writes_state_counts() {
        let mut config = Config::default();
        config.statusline.session_badge.mode = crate::config::SessionBadgeMode::Counts;
        let mut state = RuntimeState::new(config, SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "waiting"),
            agent_pane("main", "%2", "waiting"),
            agent_pane("main", "%3", "running"),
            agent_pane("main", "%4", "idle"),
        ]));

        assert_eq!(
            effects,
            vec![RuntimeEffect::SetSessionBadge {
                session: "main".to_string(),
                value: "▲ 2 ● 1 ○ 1".to_string(),
                state: "blocked".to_string(),
            }]
        );
    }

    #[test]
    fn category_badge_enabled_writes_session_agent_counts() {
        let mut config = Config::default();
        config.statusline.category.agent_badge.enabled = true;
        let mut state = RuntimeState::new(config, SidebarState::default());
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "running"),
            agent_pane("main", "%2", "idle"),
        ]));

        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::SetSessionAgentCounts { session, counts }
                if session == "main" && counts.contains(r#""working":1"#) && counts.contains(r#""idle":1"#)
        )));
    }

    #[test]
    fn active_pane_path_updates_session_project_path_and_category() {
        let mut config = Config::default();
        config.categories.default_category = Some("public".to_string());
        config.categories.rules.push(crate::config::CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["github.com/acme/*".to_string()],
        });
        let mut state = RuntimeState::new(config, SidebarState::default());
        let mut pane = agent_pane("main", "%1", "running");
        pane.window_active = true;
        pane.pane_active = true;
        pane.current_path = "/Users/me/repos/github.com/acme/app".to_string();
        pane.session_category = "public".to_string();
        pane.session_project_path = "/Users/me".to_string();

        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![pane]));

        assert!(effects.contains(&RuntimeEffect::SetSessionProjectPath {
            session: "main".to_string(),
            path: "/Users/me/repos/github.com/acme/app".to_string(),
        }));
        assert!(effects.contains(&RuntimeEffect::SetSessionCategory {
            session: "main".to_string(),
            category: "work".to_string(),
        }));
    }

    #[test]
    fn active_pane_path_can_move_session_back_to_default_category() {
        let mut config = Config::default();
        config.categories.default_category = Some("public".to_string());
        let mut state = RuntimeState::new(config, SidebarState::default());
        let mut pane = agent_pane("main", "%1", "running");
        pane.window_active = true;
        pane.pane_active = true;
        pane.current_path = "/Users/me".to_string();
        pane.session_category = "work".to_string();
        pane.session_project_path = "/Users/me/repos/github.com/acme/app".to_string();

        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![pane]));

        assert!(effects.contains(&RuntimeEffect::SetSessionProjectPath {
            session: "main".to_string(),
            path: "/Users/me".to_string(),
        }));
        assert!(effects.contains(&RuntimeEffect::SetSessionCategory {
            session: "main".to_string(),
            category: "public".to_string(),
        }));
    }

    #[test]
    fn session_category_sync_respects_manual_override() {
        let mut config = Config::default();
        config.categories.default_category = Some("public".to_string());
        config.categories.rules.push(crate::config::CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["github.com/acme/*".to_string()],
        });
        let mut state = RuntimeState::new(config, SidebarState::default());
        let mut pane = agent_pane("main", "%1", "running");
        pane.window_active = true;
        pane.pane_active = true;
        pane.current_path = "/Users/me/repos/github.com/acme/app".to_string();
        pane.session_category = "private".to_string();
        pane.session_project_path = "/Users/me".to_string();
        pane.session_category_override = "private".to_string();

        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![pane]));

        assert!(!effects.iter().any(|effect| {
            matches!(
                effect,
                RuntimeEffect::SetSessionProjectPath { .. }
                    | RuntimeEffect::SetSessionCategory { .. }
            )
        }));
    }

    #[test]
    fn window_badge_enabled_writes_window_badge() {
        let mut config = Config::default();
        config.statusline.windows.agent_badge.enabled = true;
        config.statusline.windows.agent_badge.mode = crate::config::SessionBadgeMode::Counts;
        let mut state = RuntimeState::new(config, SidebarState::default());
        let mut waiting = agent_pane("main", "%2", "waiting");
        waiting.wait_reason = "permission_prompt".to_string();
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "running"),
            waiting,
        ]));

        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::SetWindowBadge { window, value, state, counts }
                if window == "@1"
                    && value == "▲ 1 ● 1"
                    && state == "blocked"
                    && counts.contains(r#""blocked":1"#)
                    && counts.contains(r#""working":1"#)
        )));
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
    fn shutdown_clears_category_and_window_agent_badges() {
        let mut config = Config::default();
        config.statusline.category.agent_badge.enabled = true;
        config.statusline.windows.agent_badge.enabled = true;
        let mut state = RuntimeState::new(config, SidebarState::default());
        let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
            "main", "%1", "running",
        )]));
        let effects = state.apply_event(DaemonEvent::Shutdown);

        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::ClearSessionBadge { session } if session == "main"
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::ClearSessionAgentCounts { session } if session == "main"
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RuntimeEffect::ClearWindowBadge { window } if window == "@1"
        )));
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
        plain.current_command = "zsh".to_string();
        let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![sidebar, plain]));
        assert!(effects.is_empty());
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
}

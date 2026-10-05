//! Persistent acknowledgements of question issuance, independent of agent execution state.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pane_state::{AgentProcessIdentity, PaneInstance};

pub mod capture;
pub mod ingress;
pub mod journal;
pub mod profile;
pub mod reply;
pub mod resolver;
mod storage;
pub mod text;
pub use storage::CommitResult;
#[cfg(test)]
mod tests;
pub mod turn_order;

pub const MAX_OWNERS: usize = 512;
pub const MAX_KEYS_PER_OWNER: usize = 4096;
pub const MAX_KEYS: usize = 65536;
pub const MAX_ANCESTORS: usize = 64;
pub const REPLY_COMMIT_RESERVE: Duration = Duration::from_millis(40);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const MAX_TEXT_NOTICES: usize = 2048;
const MAX_TEXT_NOTICES_PER_OWNER: usize = 512;
const MAX_CAPTURE_FINGERPRINTS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoticeReason {
    InvalidPayload,
    OriginUnverified,
    AncestorNotInPane,
    OwnerUnverified,
    CapacityExceeded,
    PersistencePending,
    InvalidSidecar,
    QuestionAckDirectoryFsyncFailed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrackingHealth {
    #[default]
    Healthy,
    Degraded,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionNoticeSummary {
    pub unacknowledged: bool,
    pub owner_ref: Option<String>,
    pub latest_order: u64,
    pub acknowledged_order: u64,
    pub last_issued_at: Option<i64>,
    pub tracking_health: TrackingHealth,
    pub reason: Option<NoticeReason>,
}

impl QuestionNoticeSummary {
    pub fn degraded(&self) -> bool {
        self.tracking_health == TrackingHealth::Degraded
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionNoticeInput {
    Issued {
        session_id: String,
        turn_id: String,
        tool_use_id: String,
        ancestors: Vec<AgentProcessIdentity>,
        questions: text::QuestionEvidence,
    },
    Rejected {
        reason: NoticeReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeDisposition {
    Applied,
    Duplicate,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeDurability {
    Persisted,
    MemoryOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeResult {
    pub disposition: NoticeDisposition,
    pub durability: Option<NoticeDurability>,
    pub reason: Option<NoticeReason>,
}

impl NoticeResult {
    pub fn rejected(reason: NoticeReason) -> Self {
        Self {
            disposition: NoticeDisposition::Rejected,
            durability: None,
            reason: Some(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionNoticeState {
    pane: PaneInstance,
    process: AgentProcessIdentity,
    owner_ref: String,
    latest_order: u64,
    acknowledged_order: u64,
    last_issued_at: i64,
    seen: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct ReplyItems {
    session: String,
    items: BTreeSet<String>,
    pending: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub struct QuestionNotices {
    pub resolver: resolver::Resolver,
    save_lock: std::sync::Mutex<()>,
    owners: BTreeMap<String, QuestionNoticeState>,
    bound: BTreeMap<PaneInstance, AgentProcessIdentity>,
    diagnostics: BTreeMap<PaneInstance, NoticeReason>,
    path: Option<PathBuf>,
    server_hash: String,
    invalid_sidecar: bool,
    dirty: bool,
    last_write_attempt: Option<Instant>,
    memory_only: BTreeSet<(String, String)>,
    // Runtime only; absence after restart must never be treated as resolution.
    question_text: BTreeMap<(String, u64), (String, text::QuestionEvidence)>,
    // Sparse replies are runtime-only. A missing entry always blocks prefix advancement.
    reply_items: BTreeMap<(String, u64), ReplyItems>,
    // Retain used call identities until owner death, so a delayed reply cannot
    // acknowledge a newly reused call ID even after the old notice was acked.
    reply_calls: BTreeMap<String, BTreeSet<String>>,
    // If identity history cannot be retained, disable reply ack for this owner.
    reply_blocked_owners: BTreeSet<String>,
    #[cfg(test)]
    storage_fault: Option<storage::FaultPoint>,
}

fn digest(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("notice tuple is serializable"))
    )
}

impl QuestionNotices {
    #[cfg(test)]
    pub(crate) fn inject_reply_commit_failure_for_test(&mut self) {
        self.storage_fault = Some(storage::FaultPoint::BeforeRename);
    }
    pub fn remember_reply_items(
        &mut self,
        owner: &str,
        order: u64,
        session: &str,
        call: &str,
        count: usize,
    ) {
        if self.reply_blocked_owners.contains(owner)
            || !self.owners.get(owner).is_some_and(|state| {
                order > state.acknowledged_order && order == state.latest_order
            })
        {
            return;
        }
        let session_digest = turn_order::identifier_digest(session);
        let call_digest = digest(&(&session_digest, call));
        if self
            .reply_calls
            .get(owner)
            .is_some_and(|calls| calls.contains(&call_digest))
        {
            let ambiguous: BTreeSet<_> = (0..reply::MAX_ISSUED_ITEMS)
                .map(|index| reply::item_digest(call, index))
                .collect();
            self.reply_items.retain(|(bound_owner, _), items| {
                bound_owner != owner
                    || items.session != session_digest
                    || items.items.is_disjoint(&ambiguous)
            });
            return;
        }
        // Record every accepted call, even when text/count or active capacity is
        // unavailable. Q freeing an untracked notice must not permit ID reuse.
        if self.reply_calls.values().map(BTreeSet::len).sum::<usize>() >= MAX_KEYS
            || self
                .reply_calls
                .get(owner)
                .is_some_and(|calls| calls.len() >= MAX_KEYS_PER_OWNER)
        {
            self.reply_blocked_owners.insert(owner.to_owned());
            self.reply_items
                .retain(|(bound_owner, _), _| bound_owner != owner);
            return;
        }
        self.reply_calls
            .entry(owner.to_string())
            .or_default()
            .insert(call_digest);
        if count == 0
            || count > reply::MAX_ISSUED_ITEMS
            || self.reply_items.len() >= MAX_TEXT_NOTICES
            || self
                .reply_items
                .range((owner.to_string(), 0)..=(owner.to_string(), u64::MAX))
                .count()
                >= MAX_TEXT_NOTICES_PER_OWNER
        {
            return;
        }
        let items: BTreeSet<_> = (0..count)
            .map(|index| reply::item_digest(call, index))
            .collect();
        self.reply_items.insert(
            (owner.to_string(), order),
            ReplyItems {
                session: session_digest,
                pending: items.clone(),
                items,
            },
        );
    }

    /// Freeze exact ID -> order bindings at acceptance, before any IO wait.
    pub fn match_reply_orders(
        &self,
        pane: &PaneInstance,
        owner: &str,
        session: &str,
        reply: &reply::ReplyEvidence,
    ) -> Result<Vec<u64>, &'static str> {
        if !reply.valid() || !self.tracking_healthy() {
            return Err("invalid_reply_evidence");
        }
        let notice = self
            .owners
            .get(owner)
            .filter(|state| &state.pane == pane)
            .ok_or("stale_notice_owner")?;
        if notice.acknowledged_order >= notice.latest_order {
            return Err("unknown_reply_item");
        }
        reply
            .items
            .iter()
            .map(|id| {
                self.reply_items
                    .range(
                        (
                            owner.to_owned(),
                            notice.acknowledged_order.saturating_add(1),
                        )..=(owner.to_owned(), notice.latest_order),
                    )
                    .find(|(_, items)| items.session == session && items.items.contains(id))
                    .map(|((_, order), _)| *order)
                    .ok_or("unknown_reply_item")
            })
            .collect()
    }

    #[cfg(test)]
    pub fn acknowledge_reply(
        &mut self,
        pane: &PaneInstance,
        owner: &str,
        session: &str,
        reply: &reply::ReplyEvidence,
        deadline: Instant,
    ) -> Result<bool, &'static str> {
        if self.owners.get(owner).is_some_and(|notice| {
            &notice.pane == pane && notice.acknowledged_order >= notice.latest_order
        }) {
            return Ok(false);
        }
        let orders = self.match_reply_orders(pane, owner, session, reply)?;
        self.acknowledge_reply_orders(pane, owner, session, reply, &orders, deadline)
    }

    /// Apply only the ID/order pairs frozen at acceptance; then advance the answered prefix.
    pub fn acknowledge_reply_orders(
        &mut self,
        pane: &PaneInstance,
        owner: &str,
        session: &str,
        reply: &reply::ReplyEvidence,
        orders: &[u64],
        deadline: Instant,
    ) -> Result<bool, &'static str> {
        if deadline.saturating_duration_since(Instant::now()) < REPLY_COMMIT_RESERVE {
            return Err("reply_deadline");
        }
        if !reply.valid()
            || orders.len() != reply.items.len()
            || orders.contains(&0)
            || !self.tracking_healthy()
        {
            return Err("invalid_reply_evidence");
        }
        let notice = self
            .owners
            .get(owner)
            .filter(|state| &state.pane == pane)
            .ok_or("stale_notice_owner")?;
        let acknowledged = notice.acknowledged_order;
        let latest = notice.latest_order;
        if acknowledged >= latest {
            return Ok(false);
        }
        // Already acknowledged pairs were verified when the job was accepted.
        // Q/ordinary completion may consume them while it waits; never rebind
        // those IDs to newly issued questions, and still apply its remaining pairs.
        if orders.iter().all(|order| *order <= acknowledged) {
            return Ok(false);
        }
        let range =
            (owner.to_string(), acknowledged.saturating_add(1))..=(owner.to_string(), latest);
        let before: BTreeMap<_, _> = self
            .reply_items
            .range(range.clone())
            .map(|(key, items)| (key.clone(), items.clone()))
            .collect();
        // Reject the whole reply if any ID is unknown or belongs to another session.
        // Already consumed pairs from this accepted job are harmless; fresh
        // ingress still rejects IDs from fully acknowledged notices.
        if reply.items.iter().zip(orders).any(|(id, order)| {
            *order > acknowledged
                && !before
                    .get(&(owner.to_owned(), *order))
                    .is_some_and(|items| items.session == session && items.items.contains(id))
        }) {
            return Err("unknown_reply_item");
        }
        for (id, order) in reply.items.iter().zip(orders) {
            if *order <= acknowledged {
                continue;
            }
            self.reply_items
                .get_mut(&(owner.to_owned(), *order))
                .expect("validated reply entry")
                .pending
                .remove(id);
        }
        let mut through = acknowledged;
        for order in acknowledged.saturating_add(1)..=latest {
            if !self
                .reply_items
                .get(&(owner.to_string(), order))
                .is_some_and(|items| items.pending.is_empty())
            {
                break;
            }
            through = order;
        }
        if through == acknowledged {
            if Instant::now() >= deadline {
                self.reply_items.extend(before);
                return Err("reply_deadline");
            }
            return Ok(false);
        }
        match self.acknowledge_until(pane, owner, through, Some(deadline)) {
            Ok(changed) => Ok(changed),
            Err(error) => {
                self.reply_items.extend(before);
                Err(error)
            }
        }
    }

    pub fn remember_questions(
        &mut self,
        owner: String,
        order: u64,
        session: &str,
        questions: text::QuestionEvidence,
    ) {
        if !self
            .owners
            .get(&owner)
            .is_some_and(|state| order > state.acknowledged_order && order == state.latest_order)
            || !questions.valid()
            || self.question_text.len() >= MAX_TEXT_NOTICES
            || self
                .question_text
                .range((owner.clone(), 0)..=(owner.clone(), u64::MAX))
                .count()
                >= MAX_TEXT_NOTICES_PER_OWNER
        {
            return;
        }
        self.question_text.insert(
            (owner, order),
            (turn_order::identifier_digest(session), questions),
        );
    }

    pub fn capture_questions(
        &self,
        owner: &str,
        session: &str,
        acknowledged: u64,
        latest: u64,
    ) -> text::QuestionEvidence {
        use text::QuestionEvidence;
        if latest.saturating_sub(acknowledged) > MAX_TEXT_NOTICES_PER_OWNER as u64 {
            return QuestionEvidence::Unavailable;
        }
        let mut questions = BTreeSet::new();
        for order in acknowledged.saturating_add(1)..=latest {
            let Some((bound_session, QuestionEvidence::Fingerprints(contents))) =
                self.question_text.get(&(owner.to_string(), order))
            else {
                return QuestionEvidence::Unavailable;
            };
            if bound_session != session {
                return QuestionEvidence::Unavailable;
            }
            questions.extend(contents.iter().cloned());
            if questions.len() > MAX_CAPTURE_FINGERPRINTS {
                return QuestionEvidence::Unavailable;
            }
        }
        if questions.is_empty() {
            QuestionEvidence::Unavailable
        } else {
            QuestionEvidence::Fingerprints(questions.into_iter().collect())
        }
    }

    pub fn open(path: PathBuf, server_hash: String) -> Self {
        let mut store = Self {
            path: Some(path),
            server_hash,
            ..Self::default()
        };
        match store.load() {
            Ok(owners) => store.owners = owners,
            Err(_) => store.invalid_sidecar = true,
        }
        store
    }

    pub fn owner_ref(&self, pane: &PaneInstance, process: &AgentProcessIdentity) -> String {
        format!("vtqn1:{}", digest(&(&self.server_hash, pane, process)))
    }

    pub fn reject(&mut self, pane: &PaneInstance, reason: NoticeReason) -> NoticeResult {
        if self.diagnostics.contains_key(pane) || self.diagnostics.len() < MAX_OWNERS {
            self.diagnostics.insert(pane.clone(), reason);
        }
        NoticeResult::rejected(reason)
    }

    pub fn issue(
        &mut self,
        pane: PaneInstance,
        process: AgentProcessIdentity,
        identifiers: (&str, &str, &str),
        now: i64,
    ) -> NoticeResult {
        self.bind(pane.clone(), process.clone());
        if self.invalid_sidecar {
            return self.reject(&pane, NoticeReason::InvalidSidecar);
        }
        let key = digest(&("codex", identifiers));
        let owner_ref = self.owner_ref(&pane, &process);
        if self
            .owners
            .get(&owner_ref)
            .is_some_and(|owner| owner.seen.contains(&key))
        {
            if self.memory_only.contains(&(owner_ref.clone(), key.clone())) {
                let _ = self.persist();
            }
            let persisted = !self.memory_only.contains(&(owner_ref, key));
            return NoticeResult {
                disposition: NoticeDisposition::Duplicate,
                durability: Some(if persisted {
                    NoticeDurability::Persisted
                } else {
                    NoticeDurability::MemoryOnly
                }),
                reason: (!persisted).then_some(NoticeReason::PersistencePending),
            };
        }
        let key_count: usize = self.owners.values().map(|owner| owner.seen.len()).sum();
        if key_count >= MAX_KEYS
            || self.owners.get(&owner_ref).is_some_and(|owner| {
                owner.seen.len() >= MAX_KEYS_PER_OWNER || owner.latest_order == u64::MAX
            })
            || (!self.owners.contains_key(&owner_ref) && self.owners.len() >= MAX_OWNERS)
        {
            return self.reject(&pane, NoticeReason::CapacityExceeded);
        }
        self.memory_only.insert((owner_ref.clone(), key.clone()));
        self.resolver.cancel(&owner_ref);
        let owner = self
            .owners
            .entry(owner_ref.clone())
            .or_insert_with(|| QuestionNoticeState {
                pane: pane.clone(),
                process,
                owner_ref,
                latest_order: 0,
                acknowledged_order: 0,
                last_issued_at: now,
                seen: BTreeSet::new(),
            });
        owner.latest_order += 1;
        owner.last_issued_at = now;
        owner.seen.insert(key);
        self.diagnostics.remove(&pane);
        self.dirty = true;
        // A failed disk must not be hammered by subsequent hook deliveries.
        let persisted = self
            .last_write_attempt
            .is_none_or(|at| at.elapsed() >= RETRY_INTERVAL)
            && self.persist().is_ok();
        NoticeResult {
            disposition: NoticeDisposition::Applied,
            durability: Some(if persisted {
                NoticeDurability::Persisted
            } else {
                NoticeDurability::MemoryOnly
            }),
            reason: (!persisted).then_some(NoticeReason::PersistencePending),
        }
    }

    pub fn summary(
        &self,
        pane: &PaneInstance,
        process: Option<&AgentProcessIdentity>,
    ) -> QuestionNoticeSummary {
        let process = process.or_else(|| self.bound.get(pane));
        let owner = process.and_then(|process| self.owners.get(&self.owner_ref(pane, process)));
        let reason = if self.invalid_sidecar {
            Some(NoticeReason::InvalidSidecar)
        } else if self.dirty {
            Some(NoticeReason::PersistencePending)
        } else if owner.is_none() && self.owners.values().any(|owner| &owner.pane == pane) {
            Some(NoticeReason::OwnerUnverified)
        } else {
            self.diagnostics.get(pane).copied()
        };
        QuestionNoticeSummary {
            unacknowledged: owner
                .is_some_and(|owner| owner.latest_order > owner.acknowledged_order),
            owner_ref: owner.map(|owner| owner.owner_ref.clone()),
            latest_order: owner.map_or(0, |owner| owner.latest_order),
            acknowledged_order: owner.map_or(0, |owner| owner.acknowledged_order),
            last_issued_at: owner.map(|owner| owner.last_issued_at),
            tracking_health: if reason.is_some() {
                TrackingHealth::Degraded
            } else {
                TrackingHealth::Healthy
            },
            reason,
        }
    }

    pub fn owner_process(
        &self,
        pane: &PaneInstance,
        owner_ref: &str,
    ) -> Option<&AgentProcessIdentity> {
        self.owners
            .get(owner_ref)
            .filter(|owner| &owner.pane == pane)
            .map(|owner| &owner.process)
    }

    pub fn bind(&mut self, pane: PaneInstance, process: AgentProcessIdentity) {
        if self.bound.contains_key(&pane) || self.bound.len() < MAX_OWNERS {
            self.bound.insert(pane, process);
        }
    }

    pub fn retain_panes(&mut self, present: &BTreeSet<PaneInstance>) {
        self.diagnostics.retain(|pane, _| present.contains(pane));
    }

    pub fn acknowledge(
        &mut self,
        pane: &PaneInstance,
        owner_ref: &str,
        through_order: u64,
    ) -> Result<bool, &'static str> {
        self.acknowledge_until(pane, owner_ref, through_order, None)
    }

    pub fn acknowledge_until(
        &mut self,
        pane: &PaneInstance,
        owner_ref: &str,
        through_order: u64,
        deadline: Option<Instant>,
    ) -> Result<bool, &'static str> {
        if self.invalid_sidecar {
            return Err("invalid_sidecar");
        }
        self.resolver.cancel(owner_ref);
        let owner = self
            .owners
            .get_mut(owner_ref)
            .filter(|owner| &owner.pane == pane)
            .ok_or("stale_notice_owner")?;
        if through_order > owner.latest_order {
            return Err("future_notice_order");
        }
        if through_order <= owner.acknowledged_order {
            return Ok(false);
        }
        let was_dirty = self.dirty;
        let last_attempt = self.last_write_attempt;
        let before = owner.acknowledged_order;
        owner.acknowledged_order = through_order;
        match self.persist_commit_until(deadline) {
            CommitResult::PreCommitFailed => {
                self.owners
                    .get_mut(owner_ref)
                    .expect("owner retained")
                    .acknowledged_order = before;
                // The old sidecar still represents the restored state. Do not
                // turn an uncommitted ack into a global write backlog/rate limit.
                self.dirty = was_dirty;
                self.last_write_attempt = last_attempt;
                self.diagnostics
                    .insert(pane.clone(), NoticeReason::PersistencePending);
                return Err("persistence_pending");
            }
            CommitResult::CommittedDurabilityUnknown => {
                self.diagnostics
                    .insert(pane.clone(), NoticeReason::QuestionAckDirectoryFsyncFailed);
            }
            CommitResult::Committed => {
                self.diagnostics.remove(pane);
            }
        }
        self.resolver.acknowledged(owner_ref, through_order);
        self.question_text
            .retain(|(owner, order), _| owner != owner_ref || *order > through_order);
        self.reply_items
            .retain(|(owner, order), _| owner != owner_ref || *order > through_order);
        Ok(true)
    }

    /// Called on the existing coordinator observation/mutation path, never a second writer.
    pub fn reconcile(
        &mut self,
        mut owner_alive: impl FnMut(&PaneInstance, &AgentProcessIdentity) -> bool,
    ) -> bool {
        let old_len = self.owners.len();
        let old_owners: BTreeSet<_> = self.owners.keys().cloned().collect();
        self.owners
            .retain(|_, owner| owner_alive(&owner.pane, &owner.process));
        for owner in old_owners
            .into_iter()
            .filter(|owner| !self.owners.contains_key(owner))
        {
            self.resolver.invalidate_owner(&owner);
        }
        self.memory_only
            .retain(|(owner, _)| self.owners.contains_key(owner));
        self.question_text
            .retain(|(owner, _), _| self.owners.contains_key(owner));
        self.reply_items
            .retain(|(owner, _), _| self.owners.contains_key(owner));
        self.reply_calls
            .retain(|owner, _| self.owners.contains_key(owner));
        self.reply_blocked_owners
            .retain(|owner| self.owners.contains_key(owner));
        self.bound
            .retain(|pane, process| owner_alive(pane, process));
        let removed = old_len != self.owners.len();
        self.dirty |= removed;
        let was_dirty = self.dirty;
        if self.dirty
            && self
                .last_write_attempt
                .is_none_or(|at| at.elapsed() >= RETRY_INTERVAL)
        {
            let _ = self.persist();
        }
        removed || was_dirty != self.dirty
    }

    fn persist(&mut self) -> Result<(), ()> {
        match self.persist_commit() {
            CommitResult::Committed => Ok(()),
            CommitResult::PreCommitFailed | CommitResult::CommittedDurabilityUnknown => Err(()),
        }
    }

    pub fn tracking_healthy(&self) -> bool {
        !self.invalid_sidecar
    }

    fn persist_commit(&mut self) -> CommitResult {
        self.persist_commit_until(None)
    }

    fn persist_commit_until(&mut self, deadline: Option<Instant>) -> CommitResult {
        self.last_write_attempt = Some(Instant::now());
        let result = self.save(deadline);
        if result == CommitResult::PreCommitFailed {
            return result;
        }
        self.dirty = false;
        if result == CommitResult::Committed {
            self.memory_only.clear();
            self.diagnostics
                .retain(|_, reason| *reason != NoticeReason::PersistencePending);
        }
        self.last_write_attempt = None;
        result
    }
}

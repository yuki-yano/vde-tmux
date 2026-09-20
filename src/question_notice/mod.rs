//! Persistent acknowledgements of question issuance, independent of agent execution state.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pane_state::{AgentProcessIdentity, PaneInstance};

pub mod ingress;
mod storage;
#[cfg(test)]
mod tests;

pub const MAX_OWNERS: usize = 512;
pub const MAX_KEYS_PER_OWNER: usize = 4096;
pub const MAX_KEYS: usize = 65536;
pub const MAX_ANCESTORS: usize = 64;
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

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
    },
    Rejected {
        reason: NoticeReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeDisposition {
    Persisted,
    MemoryOnly,
    Duplicate,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeResult {
    pub disposition: NoticeDisposition,
    pub reason: Option<NoticeReason>,
}

impl NoticeResult {
    pub fn rejected(reason: NoticeReason) -> Self {
        Self {
            disposition: NoticeDisposition::Rejected,
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

#[derive(Debug, Default)]
pub struct QuestionNotices {
    owners: BTreeMap<String, QuestionNoticeState>,
    bound: BTreeMap<PaneInstance, AgentProcessIdentity>,
    diagnostics: BTreeMap<PaneInstance, NoticeReason>,
    path: Option<PathBuf>,
    server_hash: String,
    invalid_sidecar: bool,
    dirty: bool,
    last_write_attempt: Option<Instant>,
}

fn digest(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("notice tuple is serializable"))
    )
}

impl QuestionNotices {
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

    fn owner_ref(&self, pane: &PaneInstance, process: &AgentProcessIdentity) -> String {
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
            return NoticeResult {
                disposition: NoticeDisposition::Duplicate,
                reason: None,
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
            disposition: if persisted {
                NoticeDisposition::Persisted
            } else {
                NoticeDisposition::MemoryOnly
            },
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
        if self.invalid_sidecar {
            return Err("invalid_sidecar");
        }
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
        let before = owner.acknowledged_order;
        owner.acknowledged_order = through_order;
        if self.persist().is_err() {
            self.owners
                .get_mut(owner_ref)
                .expect("owner retained")
                .acknowledged_order = before;
            self.dirty = true; // Rewrite the retained notification, never retry the failed ack.
            return Err("persistence_pending");
        }
        Ok(true)
    }

    /// Called on the existing coordinator observation/mutation path, never a second writer.
    pub fn reconcile(
        &mut self,
        mut owner_alive: impl FnMut(&PaneInstance, &AgentProcessIdentity) -> bool,
    ) -> bool {
        let old_len = self.owners.len();
        self.owners
            .retain(|_, owner| owner_alive(&owner.pane, &owner.process));
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
        self.last_write_attempt = Some(Instant::now());
        self.save().map_err(|_| ())?;
        self.dirty = false;
        self.last_write_attempt = None;
        Ok(())
    }
}

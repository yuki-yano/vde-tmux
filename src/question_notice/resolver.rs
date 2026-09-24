//! Runtime-only resolution policy. Neither this state nor its cursor is hydrated from disk.
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::ingress::{InputClass, SessionSource, TranscriptLocator};
use super::profile::{CodexProfile, ProfileRequest};
use super::turn_order::identifier_digest;
use crate::pane_state::{AgentProcessIdentity, PaneInstance};

const SESSION_LIMIT: usize = 4096;
const SESSION_INGRESS_LIMIT: usize = 4096;
const INGRESS_LIMIT: usize = 65536;
const ARMED_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);
const PROBE_LIMIT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub owner: String,
    pub pane: PaneInstance,
    pub process: AgentProcessIdentity,
    pub executable: ProfileRequest,
    pub profile: CodexProfile,
    pub locator: TranscriptLocator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    HistoryUnknown,
}

#[derive(Debug)]
struct Session {
    trust: Trust,
    binding: Option<Binding>,
    epoch: u64,
    ingresses: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct Order {
    session: String,
    turn: String,
    generation: u64,
    resolved: bool,
    home: String,
    epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateStage {
    CheckingOrder,
    Armed,
    Probing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fence {
    pub generation: u64,
    pub ingress: String,
    pub binding: Binding,
    pub home: String,
    pub session: String,
    pub turn: String,
    pub epoch: u64,
    pub acknowledged: u64,
    pub latest: u64,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub eligible_run: Option<String>,
    pub last_evaluated_projection: Option<String>,
    pub fence: Fence,
    pub stage: CandidateStage,
    pub through: u64,
    pub created: Instant,
    pub deadline: Instant,
    pub next_sample: Option<Instant>,
    pub samples: u8,
    pub consecutive_normal: u8,
    pub probe_id: u64,
    pub pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    ActiveQuestion,
    NormalComposer,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderCheck {
    pub fence: Fence,
    pub issued_turns: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct NoticeFence {
    pub acknowledged: u64,
    pub latest: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct JournalView {
    pub epoch: u64,
    pub veto: bool,
    pub session_dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetainReason {
    ReaderBudget,
    ReaderDeadline,
    ReaderUnavailable,
    OrderUnproven,
    HistoryUnknown,
    Owner,
    JournalAcquire,
    JournalEvaluation,
    JournalVeto,
    Epoch,
    CheckingGuardUnavailable,
    CheckingJournalVeto,
    Fence,
    SampleJournalVeto,
    Active,
    Ambiguous,
    Deadline,
    Interrupted,
    ProbeOwner,
    ProbeDeadline,
    ProbeJournalAcquire,
    ProbeJournalEvaluation,
    ProbeJournalVeto,
    ProbeEpoch,
    CommitOwner,
    CommitDeadline,
    CommitJournalAcquire,
    CommitJournalEvaluation,
    CommitJournalVeto,
    CommitEpoch,
    SampleTransferUnavailable,
    CommitTransferUnavailable,
    CommitGuardVeto,
    FinalGuard,
    CommitRejected,
    OrderQueueUnavailable,
    ProbeQueueUnavailable,
    CommitQueueUnavailable,
    ProbeBindingChanged,
    MutationBusy,
}

#[derive(Debug)]
pub struct Resolver {
    generation: u64,
    next_candidate: u64,
    disabled: bool,
    sessions: BTreeMap<(String, String), Session>,
    orders: BTreeMap<(String, u64), Order>,
    candidates: BTreeMap<String, Candidate>,
    total_ingresses: usize,
    pub retained: u64,
    retained_by: BTreeMap<RetainReason, u64>,
    pub non_authoritative: u64,
    pub session_start_unobserved: u64,
    home_failures: BTreeMap<String, (u64, Instant)>,
    next_failure: u64,
    pub invalid_observations: [u64; 10],
    journal_failures: BTreeMap<super::journal::JournalFailure, u64>,
    pub candidates_started: u64,
    pub candidates_armed: u64,
    pub candidates_acked: u64,
}

impl Default for Resolver {
    fn default() -> Self {
        // Generation is process local; restored orders have no entry in `orders` at all.
        Self {
            generation: 1,
            next_candidate: 0,
            disabled: false,
            sessions: BTreeMap::new(),
            orders: BTreeMap::new(),
            candidates: BTreeMap::new(),
            total_ingresses: 0,
            retained: 0,
            retained_by: BTreeMap::new(),
            non_authoritative: 0,
            session_start_unobserved: 0,
            home_failures: BTreeMap::new(),
            next_failure: 0,
            invalid_observations: [0; 10],
            journal_failures: BTreeMap::new(),
            candidates_started: 0,
            candidates_armed: 0,
            candidates_acked: 0,
        }
    }
}

impl Resolver {
    pub fn note_journal_failure(&mut self, reason: Option<super::journal::JournalFailure>) {
        if let Some(reason) = reason {
            let count = self.journal_failures.entry(reason).or_default();
            *count = count.saturating_add(1);
        }
    }

    pub fn note_retained(&mut self, reason: RetainReason) {
        let value = self.retained_by.entry(reason).or_default();
        *value = value.saturating_add(1);
    }

    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({
            "trusted": self.sessions.values().filter(|s| s.trust == Trust::Trusted).count(),
            "unknown": self.sessions.values().filter(|s| s.trust == Trust::HistoryUnknown).count(),
            "orders": self.orders.len(),
            "resolved": self.orders.values().filter(|order| order.resolved).count(),
            "checking": self.candidates.values().filter(|c| c.stage == CandidateStage::CheckingOrder).count(),
            "armed": self.candidates.values().filter(|c| c.stage == CandidateStage::Armed).count(),
            "probing": self.candidates.values().filter(|c| c.stage == CandidateStage::Probing).count(),
            "retained": self.retained,
            "retained_by": self.retained_by,
            "non_authoritative": self.non_authoritative,
            "session_start_unobserved": self.session_start_unobserved,
            "home_failures": self.home_failures.len(),
            "disabled": self.disabled,
            "invalid_observations": self.invalid_observations,
            "journal_failures": self.journal_failures,
            "candidates_started": self.candidates_started,
            "candidates_armed": self.candidates_armed,
            "candidates_acked": self.candidates_acked,
        })
    }
    /// A failed journal write without a verified session blocks this entire home
    /// until a worker durably advances its shared epoch. No histories are evicted.
    /// Repeated evidence of the same pending root mismatch needs one durable bump.
    pub fn root_mismatch(&mut self, home: &str) {
        if !self.home_failures.contains_key(home) {
            self.journal_failed(Some(home), None);
        }
    }

    pub fn journal_failed(&mut self, home: Option<&str>, session: Option<&str>) {
        let Some(home) = home else {
            self.disable();
            return;
        };
        if let Some(session) = session {
            self.unknown(home, session);
            return;
        }
        if !self.home_failures.contains_key(home) && self.home_failures.len() >= SESSION_LIMIT {
            self.disable();
            return;
        }
        self.next_failure = self.next_failure.saturating_add(1);
        self.home_failures
            .insert(home.to_owned(), (self.next_failure, Instant::now()));
        self.candidates
            .retain(|_, candidate| candidate.fence.home != home);
    }

    pub fn recovery_due(&mut self, now: Instant) -> Vec<(String, u64)> {
        self.home_failures
            .iter_mut()
            .filter_map(|(home, (ticket, due))| {
                if now < *due {
                    return None;
                }
                *due = now + Duration::from_millis(250);
                Some((home.clone(), *ticket))
            })
            .collect()
    }

    pub fn next_wakeup(&self) -> Option<Instant> {
        self.home_failures
            .values()
            .map(|(_, due)| *due)
            .chain(self.candidates.values().map(|candidate| {
                if candidate.stage == CandidateStage::Probing && !candidate.pending {
                    candidate
                        .next_sample
                        .unwrap_or(candidate.deadline)
                        .min(candidate.deadline)
                } else {
                    candidate.deadline
                }
            }))
            .min()
    }

    pub fn tick_due(&self, now: Instant) -> bool {
        self.home_failures.values().any(|(_, due)| now >= *due)
            || self.candidates.values().any(|candidate| {
                now >= candidate.deadline
                    || (candidate.stage == CandidateStage::Probing
                        && !candidate.pending
                        && candidate.next_sample.is_some_and(|due| now >= due))
            })
    }

    pub fn cursor_sessions(&self) -> BTreeSet<(String, String)> {
        self.sessions
            .iter()
            .filter(|(_, session)| !self.disabled && session.trust == Trust::Trusted)
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub fn recovered(&mut self, home: &str, ticket: u64) {
        if self
            .home_failures
            .get(home)
            .is_none_or(|(current, _)| *current != ticket)
        {
            return;
        }
        self.home_failures.remove(home);
        for ((key, _), session) in &mut self.sessions {
            if key == home {
                session.trust = Trust::HistoryUnknown;
            }
        }
        self.candidates
            .retain(|_, candidate| candidate.fence.home != home);
    }

    pub fn trust(&self, home: &str, session: &str) -> Trust {
        self.sessions
            .get(&(home.to_owned(), session.to_owned()))
            .map_or(Trust::HistoryUnknown, |session| session.trust)
    }

    pub fn candidate(&self, owner: &str) -> Option<&Candidate> {
        self.candidates.get(owner)
    }
    pub fn candidates(&self) -> impl Iterator<Item = &Candidate> {
        self.candidates.values()
    }
    pub fn disabled(&self) -> bool {
        self.disabled
    }

    pub fn session_epoch(&self, home: &str, session: &str) -> Option<u64> {
        self.sessions
            .get(&(home.to_owned(), session.to_owned()))
            .map(|state| state.epoch)
    }

    pub fn session_binding(&self, home: &str, session: &str) -> Option<&Binding> {
        self.sessions
            .get(&(home.to_owned(), session.to_owned()))
            .and_then(|state| state.binding.as_ref())
    }

    fn disable(&mut self) {
        self.disabled = true;
        self.candidates.clear();
    }

    pub fn unknown(&mut self, home: &str, session: &str) {
        let key = (home.to_string(), session.to_string());
        if !self.sessions.contains_key(&key) {
            self.session_start_unobserved = self.session_start_unobserved.saturating_add(1);
        }
        if !self.sessions.contains_key(&key) && self.sessions.len() >= SESSION_LIMIT {
            self.disable();
            return;
        }
        let state = self.sessions.entry(key).or_insert_with(|| Session {
            trust: Trust::HistoryUnknown,
            binding: None,
            epoch: 0,
            ingresses: BTreeSet::new(),
        });
        state.trust = Trust::HistoryUnknown;
        self.candidates.retain(|_, candidate| {
            candidate.fence.home != home || candidate.fence.session != session
        });
    }

    /// `verified` includes the startup header, exact ancestry, process and sidecar health.
    pub fn session_start(
        &mut self,
        home: String,
        session: String,
        source: SessionSource,
        binding: Option<Binding>,
        journal: JournalView,
        verified: bool,
    ) {
        if self.disabled || self.home_failures.contains_key(&home) {
            return;
        }
        let key = (home.clone(), session.clone());
        if let Some(existing) = self.sessions.get(&key) {
            let same = existing.binding.as_ref() == binding.as_ref()
                && binding.is_some()
                && verified
                && existing.epoch == journal.epoch
                && !journal.session_dirty;
            if !same || !matches!(source, SessionSource::Startup | SessionSource::Compact) {
                self.unknown(&home, &session);
            }
            // A duplicate startup and current-owner Compact never create fresh trust.
            return;
        }
        if self.sessions.len() >= SESSION_LIMIT {
            self.disable();
            return;
        }
        if let Some(binding) = &binding {
            self.cancel(&binding.owner);
            if source != SessionSource::Startup {
                self.invalidate_owner(&binding.owner);
            }
        }
        let trust = if source == SessionSource::Startup
            && verified
            && binding.is_some()
            && !journal.session_dirty
        {
            Trust::Trusted
        } else {
            Trust::HistoryUnknown
        };
        self.sessions.insert(
            key,
            Session {
                trust,
                binding,
                epoch: journal.epoch,
                ingresses: BTreeSet::new(),
            },
        );
    }

    pub fn cancel(&mut self, owner: &str) {
        if self.candidates.remove(owner).is_some() {
            self.retained = self.retained.saturating_add(1);
        }
    }

    pub fn invalidate_owner(&mut self, owner: &str) {
        self.cancel(owner);
        for state in self.sessions.values_mut() {
            if state
                .binding
                .as_ref()
                .is_some_and(|binding| binding.owner == owner)
            {
                state.trust = Trust::HistoryUnknown;
            }
        }
    }

    pub fn issue(
        &mut self,
        binding: &Binding,
        home: &str,
        session: &str,
        turn: &str,
        order: u64,
        epoch: u64,
    ) {
        self.cancel(&binding.owner);
        let key = (home.to_owned(), session.to_owned());
        if !self.sessions.get(&key).is_some_and(|state| {
            state.trust == Trust::Trusted
                && state.binding.as_ref() == Some(binding)
                && state.epoch == epoch
        }) {
            self.unknown(home, session);
            return;
        }
        self.orders.insert(
            (binding.owner.clone(), order),
            Order {
                session: session.to_owned(),
                turn: turn.to_owned(),
                generation: self.generation,
                resolved: false,
                home: home.to_owned(),
                epoch,
            },
        );
    }

    /// Called at the initial accepted mutation, before journal or order evidence can wait.
    #[allow(clippy::too_many_arguments)]
    pub fn ordinary(
        &mut self,
        class: InputClass,
        binding: &Binding,
        session: &str,
        ingress: &str,
        turn: &str,
        notice: NoticeFence,
        journal: JournalView,
        now: Instant,
    ) -> Option<OrderCheck> {
        if class == InputClass::NonAuthoritativeInput {
            self.non_authoritative = self.non_authoritative.saturating_add(1);
            return None;
        }
        if self.disabled {
            return None;
        }
        let home = binding.locator.home_digest();
        let key = (home.clone(), session.to_owned());
        if !self.sessions.contains_key(&key) {
            self.unknown(&home, session);
        }
        let state = self.sessions.get_mut(&key)?;
        if state.ingresses.contains(ingress) {
            return None;
        }
        if state.ingresses.len() >= SESSION_INGRESS_LIMIT {
            self.unknown(&home, session);
            return None;
        }
        if self.total_ingresses >= INGRESS_LIMIT {
            self.disable();
            return None;
        }
        state.ingresses.insert(ingress.to_string());
        self.total_ingresses += 1;
        let valid = state.trust == Trust::Trusted
            && state.binding.as_ref() == Some(binding)
            && state.epoch == journal.epoch;
        self.cancel(&binding.owner);
        if !valid {
            self.unknown(&home, session);
            return None;
        }
        if self.home_failures.contains_key(&home)
            || journal.veto
            || notice.acknowledged >= notice.latest
        {
            return None;
        }
        let turns = self
            .orders
            .range(
                (binding.owner.clone(), notice.acknowledged + 1)
                    ..=(binding.owner.clone(), notice.latest),
            )
            .filter(|(_, order)| {
                order.session == session
                    && order.turn != turn
                    && order.generation == self.generation
                    && order.home == home
                    && order.epoch == journal.epoch
            })
            .map(|(_, order)| order.turn.clone())
            .collect::<BTreeSet<_>>();
        if turns.is_empty() {
            return None;
        }
        self.next_candidate = self.next_candidate.checked_add(1).or_else(|| {
            self.disable();
            None
        })?;
        let fence = Fence {
            generation: self.next_candidate,
            ingress: ingress.to_owned(),
            binding: binding.clone(),
            home,
            session: session.to_owned(),
            turn: turn.to_owned(),
            epoch: journal.epoch,
            acknowledged: notice.acknowledged,
            latest: notice.latest,
        };
        self.candidates_started = self.candidates_started.saturating_add(1);
        self.candidates.insert(
            binding.owner.clone(),
            Candidate {
                eligible_run: None,
                last_evaluated_projection: None,
                fence: fence.clone(),
                stage: CandidateStage::CheckingOrder,
                through: notice.acknowledged,
                created: now,
                deadline: now + PROBE_LIMIT,
                next_sample: None,
                samples: 0,
                consecutive_normal: 0,
                probe_id: 0,
                pending: false,
            },
        );
        Some(OrderCheck {
            fence,
            issued_turns: turns,
        })
    }

    pub fn fence_current(&self, fence: &Fence) -> bool {
        !self.disabled
            && !self.home_failures.contains_key(&fence.home)
            && self
                .candidates
                .get(&fence.binding.owner)
                .is_some_and(|candidate| &candidate.fence == fence)
            && self
                .sessions
                .get(&(fence.home.clone(), fence.session.clone()))
                .is_some_and(|session| {
                    session.trust == Trust::Trusted
                        && session.binding.as_ref() == Some(&fence.binding)
                        && session.epoch == fence.epoch
                })
    }

    pub fn checked(&mut self, check: &OrderCheck, proven: &BTreeSet<String>, now: Instant) -> bool {
        if !self.fence_current(&check.fence) {
            return false;
        }
        let fence = &check.fence;
        if self.candidates[&fence.binding.owner].stage != CandidateStage::CheckingOrder
            || now >= self.candidates[&fence.binding.owner].deadline
        {
            self.cancel(&fence.binding.owner);
            return false;
        }
        for ((owner, number), order) in &mut self.orders {
            if owner == &fence.binding.owner
                && *number > fence.acknowledged
                && *number <= fence.latest
                && order.session == fence.session
                && order.home == fence.home
                && order.epoch == fence.epoch
                && order.generation == self.generation
                && order.turn != fence.turn
                && proven.contains(&order.turn)
                && check.issued_turns.contains(&order.turn)
            {
                order.resolved = true;
            }
        }
        let through = self.prefix(fence);
        if through == fence.acknowledged {
            self.cancel(&fence.binding.owner);
            return false;
        }
        let candidate = self
            .candidates
            .get_mut(&fence.binding.owner)
            .expect("current candidate");
        candidate.through = through;
        candidate.stage = CandidateStage::Armed;
        self.candidates_armed = self.candidates_armed.saturating_add(1);
        candidate.created = now;
        candidate.deadline = now + ARMED_LIMIT;
        true
    }

    fn prefix(&self, fence: &Fence) -> u64 {
        let mut through = fence.acknowledged;
        for number in (fence.acknowledged + 1)..=fence.latest {
            let Some(order) = self.orders.get(&(fence.binding.owner.clone(), number)) else {
                break;
            };
            if !order.resolved
                || order.generation != self.generation
                || order.home != fence.home
                || order.epoch != fence.epoch
                || !self
                    .sessions
                    .get(&(order.home.clone(), order.session.clone()))
                    .is_some_and(|state| {
                        state.trust == Trust::Trusted
                            && state.epoch == fence.epoch
                            && state
                                .binding
                                .as_ref()
                                .is_some_and(|binding| same_owner(binding, &fence.binding))
                    })
            {
                break;
            }
            through = number;
        }
        through
    }

    pub fn eligible(
        &mut self,
        owner: &str,
        turn: &str,
        done: bool,
        interrupted: bool,
        now: Instant,
    ) {
        let Some(candidate) = self.candidates.get_mut(owner) else {
            return;
        };
        if candidate.fence.turn != turn {
            return;
        }
        if interrupted || now >= candidate.deadline {
            self.note_retained(if interrupted {
                RetainReason::Interrupted
            } else {
                RetainReason::Deadline
            });
            self.cancel(owner);
            return;
        }
        if candidate.stage == CandidateStage::Armed && done {
            candidate.stage = CandidateStage::Probing;
            candidate.deadline = now + PROBE_LIMIT;
            candidate.next_sample = Some(now + Duration::from_millis(150));
        }
    }

    pub fn due(&mut self, now: Instant) -> Vec<(Fence, u64)> {
        let mut jobs = Vec::new();
        self.candidates.retain(|_, candidate| {
            if now >= candidate.deadline {
                self.retained = self.retained.saturating_add(1);
                let value = self.retained_by.entry(RetainReason::Deadline).or_default();
                *value = value.saturating_add(1);
                return false;
            }
            if candidate.stage == CandidateStage::Probing
                && !candidate.pending
                && candidate.next_sample.is_some_and(|at| now >= at)
                && candidate.samples < 3
            {
                candidate.samples += 1;
                candidate.probe_id += 1;
                candidate.pending = true;
                candidate.next_sample = None;
                jobs.push((candidate.fence.clone(), candidate.probe_id));
            }
            true
        });
        jobs
    }

    pub fn record_eligibility(&mut self, owner: &str, run: String) {
        if let Some(candidate) = self.candidates.get_mut(owner)
            && candidate.stage == CandidateStage::Probing
            && candidate.eligible_run.is_none()
        {
            candidate.eligible_run = Some(run);
        }
    }

    pub fn record_evaluated_projection(&mut self, owner: &str, projection: String) {
        if let Some(candidate) = self.candidates.get_mut(owner)
            && candidate.stage == CandidateStage::Armed
        {
            candidate.last_evaluated_projection = Some(projection);
        }
    }

    /// Returns a fixed prefix only after two positive veto checks. It does not acknowledge.
    pub fn sample(
        &mut self,
        fence: &Fence,
        probe_id: u64,
        sample: Sample,
        now: Instant,
    ) -> Option<u64> {
        if !self.fence_current(fence) {
            return None;
        }
        let candidate = self.candidates.get_mut(&fence.binding.owner)?;
        if candidate.stage != CandidateStage::Probing
            || candidate.probe_id != probe_id
            || !candidate.pending
        {
            return None;
        }
        if now >= candidate.deadline || sample != Sample::NormalComposer {
            self.cancel(&fence.binding.owner);
            return None;
        }
        candidate.pending = false;
        candidate.consecutive_normal += 1;
        if candidate.consecutive_normal == 2 {
            candidate.pending = true;
            return Some(candidate.through);
        }
        candidate.next_sample = Some(now + Duration::from_millis(300));
        None
    }

    pub fn final_guard(
        &self,
        fence: &Fence,
        through: u64,
        notice: NoticeFence,
        journal: JournalView,
        now: Instant,
    ) -> bool {
        self.fence_current(fence)
            && notice.latest == fence.latest
            && notice.acknowledged == fence.acknowledged
            && journal.epoch == fence.epoch
            && !journal.veto
            && self.prefix(fence) == through
            && self
                .candidates
                .get(&fence.binding.owner)
                .is_some_and(|candidate| {
                    candidate.stage == CandidateStage::Probing
                        && candidate.consecutive_normal == 2
                        && candidate.pending
                        && candidate.through == through
                        && now < candidate.deadline
                })
    }

    pub fn acknowledged(&mut self, owner: &str, through: u64) {
        self.cancel(owner);
        self.orders
            .retain(|(key, number), _| key != owner || *number > through);
        // Ingress dedup deliberately survives Q, order pruning and all Run retention.
    }
}

pub fn session_key(value: &str) -> String {
    identifier_digest(value)
}

pub fn lifecycle_fence(record: &crate::pane_state::PaneState) -> String {
    super::digest(&(
        &record.state_id,
        record.agent_epoch,
        record.run_seq,
        &record.current_run,
    ))
}

pub fn evaluation_fence(record: &crate::pane_state::PaneState) -> String {
    super::digest(&(lifecycle_fence(record), &record.lifecycle))
}

fn same_owner(a: &Binding, b: &Binding) -> bool {
    a.owner == b.owner
        && a.pane == b.pane
        && a.process == b.process
        && a.executable == b.executable
        && a.profile == b.profile
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question_notice::profile::ExecutableFingerprint;

    fn binding() -> Binding {
        let process = AgentProcessIdentity {
            pid: 71,
            start_token: "1:2".into(),
        };
        Binding {
            owner: "owner".into(),
            pane: PaneInstance {
                pane_id: "%7".into(),
                pane_pid: 70,
            },
            process: process.clone(),
            executable: ProfileRequest {
                process,
                executable: ExecutableFingerprint {
                    dev: 1,
                    ino: 2,
                    size: 3,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    ctime_sec: 0,
                    ctime_nsec: 0,
                },
            },
            profile: CodexProfile::V01561,
            locator: TranscriptLocator {
                home: "/synthetic".into(),
                transcript: "/synthetic/sessions/transcript.jsonl".into(),
                dev: 1,
                ino: 2,
            },
        }
    }
    fn clean() -> JournalView {
        JournalView {
            epoch: 0,
            veto: false,
            session_dirty: false,
        }
    }
    fn notice() -> NoticeFence {
        NoticeFence {
            acknowledged: 0,
            latest: 1,
        }
    }
    fn trusted() -> (Resolver, Binding) {
        let binding = binding();
        let mut r = Resolver::default();
        r.session_start(
            binding.locator.home_digest(),
            session_key("session"),
            SessionSource::Startup,
            Some(binding.clone()),
            clean(),
            true,
        );
        (r, binding)
    }
    fn issue(r: &mut Resolver, binding: &Binding, turn: &str, order: u64) {
        r.issue(
            binding,
            &binding.locator.home_digest(),
            &session_key("session"),
            &session_key(turn),
            order,
            0,
        );
    }
    fn ordinary(
        r: &mut Resolver,
        binding: &Binding,
        ingress: &str,
        turn: &str,
        fence: NoticeFence,
        now: Instant,
    ) -> Option<OrderCheck> {
        r.ordinary(
            InputClass::OrdinaryPrompt,
            binding,
            &session_key("session"),
            &session_key(ingress),
            &session_key(turn),
            fence,
            clean(),
            now,
        )
    }
    fn arm(r: &mut Resolver, binding: &Binding, now: Instant) -> OrderCheck {
        let check = ordinary(r, binding, "input", "b", notice(), now).unwrap();
        assert!(r.checked(&check, &BTreeSet::from([session_key("a")]), now));
        check
    }

    #[test]
    fn direct_and_accepted_queue_follow_identical_policy_and_registration_is_inert() {
        for _route in ["direct", "queue", "queued_before_question"] {
            let (mut r, binding) = trusted();
            issue(&mut r, &binding, "a", 1);
            let now = Instant::now();
            // Queue registration emits no hook, so there is no mutation to invoke.
            assert!(r.candidate(&binding.owner).is_none());
            let check = arm(&mut r, &binding, now);
            assert_eq!(r.candidate(&binding.owner).unwrap().through, 1);
            r.eligible(&binding.owner, &session_key("b"), true, false, now);
            assert!(r.due(now + Duration::from_millis(149)).is_empty());
            let first = r.due(now + Duration::from_millis(150)).pop().unwrap();
            assert_eq!(
                r.sample(
                    &first.0,
                    first.1,
                    Sample::NormalComposer,
                    now + Duration::from_millis(150)
                ),
                None
            );
            assert!(r.due(now + Duration::from_millis(449)).is_empty());
            let second = r.due(now + Duration::from_millis(450)).pop().unwrap();
            assert_eq!(
                r.sample(
                    &second.0,
                    second.1,
                    Sample::NormalComposer,
                    now + Duration::from_millis(450)
                ),
                Some(1)
            );
            assert!(r.final_guard(
                &check.fence,
                1,
                notice(),
                clean(),
                now + Duration::from_millis(451)
            ));
            assert!(!r.final_guard(
                &check.fence,
                1,
                notice(),
                JournalView {
                    veto: true,
                    ..clean()
                },
                now + Duration::from_millis(451)
            ));
        }
    }

    #[test]
    fn non_authoritative_same_turn_other_session_and_other_owner_do_not_resolve() {
        let (mut r, binding) = trusted();
        issue(&mut r, &binding, "a", 1);
        let now = Instant::now();
        assert!(ordinary(&mut r, &binding, "same-turn", "a", notice(), now).is_none());
        let check = arm(&mut r, &binding, now);
        assert!(
            r.ordinary(
                InputClass::NonAuthoritativeInput,
                &binding,
                "unknown",
                "answer",
                "next",
                notice(),
                clean(),
                now
            )
            .is_none()
        );
        assert!(r.fence_current(&check.fence));
        assert!(
            r.ordinary(
                InputClass::OrdinaryPrompt,
                &binding,
                "other-session",
                "other-input",
                "next",
                notice(),
                clean(),
                now
            )
            .is_none()
        );
        let mut other = binding.clone();
        other.process.start_token = "replaced".into();
        assert!(ordinary(&mut r, &other, "changed-owner", "next", notice(), now).is_none());
        assert_eq!(
            r.trust(&binding.locator.home_digest(), &session_key("session")),
            Trust::HistoryUnknown
        );
    }

    #[test]
    fn each_issued_turn_is_proven_and_prefix_never_crosses_a_barrier() {
        let (mut r, binding) = trusted();
        issue(&mut r, &binding, "a1", 1);
        issue(&mut r, &binding, "a2", 2);
        let now = Instant::now();
        let fence = NoticeFence {
            latest: 2,
            ..notice()
        };
        let check = ordinary(&mut r, &binding, "input", "b", fence, now).unwrap();
        assert_eq!(check.issued_turns.len(), 2);
        assert!(r.checked(&check, &BTreeSet::from([session_key("a1")]), now));
        assert_eq!(r.candidate(&binding.owner).unwrap().through, 1);
        let (mut r, binding) = trusted();
        issue(&mut r, &binding, "a2", 2); // order 1 is legacy.
        let check = ordinary(&mut r, &binding, "input", "b", fence, now).unwrap();
        assert!(!r.checked(&check, &check.issued_turns, now));
        assert!(r.candidate(&binding.owner).is_none());
    }

    #[test]
    fn delayed_and_replayed_inputs_never_resolve_new_issues_even_after_ack_pruning() {
        let (mut r, binding) = trusted();
        issue(&mut r, &binding, "a", 1);
        let now = Instant::now();
        let check = ordinary(&mut r, &binding, "input", "b", notice(), now).unwrap();
        issue(&mut r, &binding, "c", 2);
        assert!(!r.checked(&check, &check.issued_turns, now));
        r.acknowledged(&binding.owner, 1);
        assert!(
            ordinary(
                &mut r,
                &binding,
                "input",
                "b",
                NoticeFence {
                    acknowledged: 1,
                    latest: 2
                },
                now
            )
            .is_none()
        );
        let delayed = ordinary(
            &mut r,
            &binding,
            "first-late-delivery",
            "b",
            NoticeFence {
                acknowledged: 1,
                latest: 2,
            },
            now,
        )
        .unwrap();
        // The structural reader did not prove Complete(C) < Start(B).
        assert!(!r.checked(&delayed, &BTreeSet::new(), now));
        assert!(!r.orders[&(binding.owner.clone(), 2)].resolved);
    }

    #[test]
    fn consume_ingress_even_with_no_orders_or_inflight_and_never_recover_unknown_session() {
        let (mut r, binding) = trusted();
        let now = Instant::now();
        assert!(
            ordinary(
                &mut r,
                &binding,
                "input",
                "b",
                NoticeFence {
                    latest: 0,
                    acknowledged: 0
                },
                now
            )
            .is_none()
        );
        issue(&mut r, &binding, "a", 1);
        assert!(ordinary(&mut r, &binding, "input", "b", notice(), now).is_none());
        assert!(
            r.ordinary(
                InputClass::OrdinaryPrompt,
                &binding,
                &session_key("session"),
                "inflight",
                "b",
                notice(),
                JournalView {
                    veto: true,
                    ..clean()
                },
                now
            )
            .is_none()
        );
        assert!(
            r.ordinary(
                InputClass::OrdinaryPrompt,
                &binding,
                &session_key("session"),
                "inflight",
                "b",
                notice(),
                clean(),
                now
            )
            .is_none()
        );
        r.unknown(&binding.locator.home_digest(), &session_key("session"));
        r.session_start(
            binding.locator.home_digest(),
            session_key("session"),
            SessionSource::Startup,
            Some(binding.clone()),
            clean(),
            true,
        );
        assert_eq!(
            r.trust(&binding.locator.home_digest(), &session_key("session")),
            Trust::HistoryUnknown
        );
    }

    #[test]
    fn stop_order_long_turn_timeout_veto_and_stale_probe_are_fenced() {
        for stop_first in [false, true] {
            let (mut r, binding) = trusted();
            issue(&mut r, &binding, "a", 1);
            let now = Instant::now();
            let check = ordinary(&mut r, &binding, "input", "b", notice(), now).unwrap();
            if stop_first {
                r.eligible(&binding.owner, &session_key("b"), true, false, now);
            }
            assert!(r.checked(&check, &check.issued_turns, now));
            let done_at = now + Duration::from_secs(40);
            r.eligible(
                &binding.owner,
                &session_key("other-turn"),
                true,
                false,
                done_at,
            );
            assert!(r.due(done_at).is_empty());
            // The integration invokes this also on Armed entry if Stop already arrived.
            r.eligible(&binding.owner, &session_key("b"), true, false, done_at);
            let job = r.due(done_at + Duration::from_millis(150)).pop().unwrap();
            for veto in [Sample::ActiveQuestion, Sample::Ambiguous] {
                assert_eq!(
                    r.sample(&job.0, job.1, veto, done_at + Duration::from_millis(150)),
                    None
                );
            }
            assert!(r.candidate(&binding.owner).is_none());
            assert_eq!(
                r.sample(
                    &job.0,
                    job.1,
                    Sample::NormalComposer,
                    done_at + Duration::from_millis(450)
                ),
                None
            );
        }
        let (mut r, binding) = trusted();
        issue(&mut r, &binding, "a", 1);
        let now = Instant::now();
        arm(&mut r, &binding, now);
        r.due(now + ARMED_LIMIT);
        assert!(r.candidate(&binding.owner).is_none());
    }

    #[test]
    fn compact_is_not_a_fresh_start_and_resume_fork_clear_never_restore_trust() {
        for source in [
            SessionSource::Resume,
            SessionSource::Fork,
            SessionSource::Clear,
            SessionSource::Unknown,
        ] {
            let (mut r, binding) = trusted();
            r.session_start(
                binding.locator.home_digest(),
                session_key("session"),
                source,
                Some(binding.clone()),
                clean(),
                true,
            );
            assert_eq!(
                r.trust(&binding.locator.home_digest(), &session_key("session")),
                Trust::HistoryUnknown
            );
        }
        let (mut r, binding) = trusted();
        r.session_start(
            binding.locator.home_digest(),
            session_key("session"),
            SessionSource::Compact,
            Some(binding.clone()),
            clean(),
            true,
        );
        assert_eq!(
            r.trust(&binding.locator.home_digest(), &session_key("session")),
            Trust::Trusted
        );
        r.session_start(
            binding.locator.home_digest(),
            session_key("first-compact"),
            SessionSource::Compact,
            Some(binding.clone()),
            clean(),
            true,
        );
        assert_eq!(
            r.trust(
                &binding.locator.home_digest(),
                &session_key("first-compact")
            ),
            Trust::HistoryUnknown
        );
    }

    #[test]
    fn capacity_failures_keep_history_and_never_allow_reenrollment() {
        let (mut r, binding) = trusted();
        let now = Instant::now();
        for i in 0..=SESSION_INGRESS_LIMIT {
            ordinary(
                &mut r,
                &binding,
                &i.to_string(),
                "b",
                NoticeFence {
                    latest: 0,
                    acknowledged: 0,
                },
                now,
            );
        }
        assert_eq!(
            r.trust(&binding.locator.home_digest(), &session_key("session")),
            Trust::HistoryUnknown
        );
        assert_eq!(r.total_ingresses, SESSION_INGRESS_LIMIT);
        for i in 0..SESSION_LIMIT {
            r.unknown(&binding.locator.home_digest(), &session_key(&i.to_string()));
        }
        assert!(r.disabled());
        assert_eq!(r.sessions.len(), SESSION_LIMIT);
    }

    #[test]
    fn armed_candidate_waits_without_scheduling_ticks_and_unknown_releases_cursor_state() {
        let (mut r, binding) = trusted();
        let now = Instant::now();
        issue(&mut r, &binding, "a", 1);
        arm(&mut r, &binding, now);
        assert!(!r.tick_due(now + Duration::from_secs(31)));
        assert!(!r.tick_due(now + ARMED_LIMIT - Duration::from_millis(1)));
        assert!(r.tick_due(now + ARMED_LIMIT));
        assert_eq!(r.cursor_sessions().len(), 1);
        r.invalidate_owner(&binding.owner);
        assert!(r.cursor_sessions().is_empty());
    }

    #[test]
    fn repeated_pending_root_mismatch_coalesces_but_new_loss_reports_keep_ticket_fences() {
        let (mut r, binding) = trusted();
        let home = binding.locator.home_digest();
        r.root_mismatch(&home);
        let first = r.next_failure;
        r.root_mismatch(&home);
        assert_eq!(r.next_failure, first);
        r.journal_failed(Some(&home), None);
        assert!(r.next_failure > first);
        r.recovered(&home, first);
        assert_eq!(r.home_failures.len(), 1);
    }

    #[test]
    fn journal_failure_scope_recovery_and_old_completion_are_fenced() {
        let (mut r, binding) = trusted();
        let home = binding.locator.home_digest();
        let now = Instant::now();
        issue(&mut r, &binding, "a", 1);
        let check = arm(&mut r, &binding, now);
        r.journal_failed(Some(&home), None);
        assert!(!r.fence_current(&check.fence));
        assert_eq!(r.trust(&home, &session_key("session")), Trust::Trusted);
        let first = r.recovery_due(Instant::now())[0].1;
        assert!(ordinary(&mut r, &binding, "during-failure", "b", notice(), now).is_none());
        r.journal_failed(Some(&home), None);
        r.recovered(&home, first);
        assert!(
            !r.home_failures.is_empty(),
            "old completion cleared a newer failure"
        );
        let current = r.recovery_due(Instant::now())[0].1;
        r.recovered(&home, current);
        assert!(r.home_failures.is_empty());
        assert_eq!(
            r.trust(&home, &session_key("session")),
            Trust::HistoryUnknown
        );
        r.session_start(
            home.clone(),
            session_key("new"),
            SessionSource::Startup,
            Some(binding.clone()),
            clean(),
            true,
        );
        r.journal_failed(Some(&home), Some(&session_key("session")));
        assert_eq!(r.trust(&home, &session_key("new")), Trust::Trusted);
        assert!(
            r.home_failures.is_empty(),
            "verified session failure changed unrelated home epoch"
        );
        r.journal_failed(None, None);
        assert!(r.disabled());
    }
}

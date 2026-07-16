use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::DoneClearOn;
use crate::options::KEY_PANE_STATE;
use crate::tmux::TmuxRunner;

use super::model::*;
use super::reducer::{ReduceError, ReductionOutcome, reduce};

pub const STORE_RECOVERY_DEADLINE: Duration = Duration::from_secs(5);
pub const STORE_RECOVERY_RETRY_INTERVAL: Duration = Duration::from_millis(25);
pub const MAX_DIAGNOSTICS: usize = 256;

const SERVER_MISMATCH_SENTINEL: &str = "__VDE_PANE_STATE_SERVER_MISMATCH__";
const PANE_MISMATCH_SENTINEL: &str = "__VDE_PANE_STATE_PANE_MISMATCH__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    StateTooLarge,
    StateLoad(PaneStateLoadError),
    InvalidPaneInstance,
    StaleStateIdentity,
    WriterLeaseHeld,
    PersistPending,
    PersistFailed(String),
    ExternalWriter(PaneStateLoadError),
    FailStop(String),
    CounterOverflow(&'static str),
    Reduce(ReduceError),
    Random(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateTooLarge => f.write_str("serialized pane state exceeds 256 KiB"),
            Self::StateLoad(error) | Self::ExternalWriter(error) => f.write_str(&error.message),
            Self::InvalidPaneInstance => f.write_str("invalid pane instance"),
            Self::StaleStateIdentity => f.write_str("stale state identity"),
            Self::WriterLeaseHeld => f.write_str("pane state writer lease is already held"),
            Self::PersistPending => f.write_str("pane state outcome is pending recovery"),
            Self::PersistFailed(message) | Self::FailStop(message) | Self::Random(message) => {
                f.write_str(message)
            }
            Self::CounterOverflow(counter) => write!(f, "{counter} counter overflow"),
            Self::Reduce(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for StoreError {}

impl StoreError {
    pub fn requires_daemon_exit(&self) -> bool {
        matches!(
            self,
            Self::FailStop(_)
                | Self::CounterOverflow(_)
                | Self::Reduce(ReduceError::CounterOverflow(_))
        )
    }
}

impl From<ReduceError> for StoreError {
    fn from(error: ReduceError) -> Self {
        Self::Reduce(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum LoadedPaneRecord {
    Missing,
    Uninitialized { raw: String },
    Valid(StoredPaneRecord),
    Quarantined(QuarantinedPaneRecord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedPaneRecord {
    pub error: PaneStateLoadError,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPaneRecord {
    pub pane_instance: PaneInstance,
    pub raw: Option<String>,
}

pub fn serialize_record(record: &StoredPaneRecord) -> Result<String, StoreError> {
    record
        .validate()
        .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
    let raw = serde_json::to_string(record)
        .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
    if raw.len() > MAX_STORED_RECORD_BYTES {
        return Err(StoreError::StateTooLarge);
    }
    Ok(raw)
}

pub fn load_record(entry: RawPaneRecord) -> LoadedPaneRecord {
    let Some(raw) = entry.raw else {
        return LoadedPaneRecord::Missing;
    };
    if raw.len() > MAX_STORED_RECORD_BYTES {
        return LoadedPaneRecord::Quarantined(quarantine(
            entry.pane_instance,
            raw,
            "serialized pane state exceeds 256 KiB".to_string(),
        ));
    }
    let record = match serde_json::from_str::<StoredPaneRecord>(&raw) {
        Ok(record) => record,
        Err(error) => {
            return LoadedPaneRecord::Quarantined(quarantine(
                entry.pane_instance,
                raw,
                error.to_string(),
            ));
        }
    };
    if record.pane_instance().pane_id == entry.pane_instance.pane_id
        && record.pane_instance().pane_pid != entry.pane_instance.pane_pid
    {
        return LoadedPaneRecord::Uninitialized { raw };
    }
    if let Err(error) = record.validate() {
        return LoadedPaneRecord::Quarantined(quarantine(
            entry.pane_instance,
            raw,
            error.to_string(),
        ));
    }
    if record.pane_instance() != &entry.pane_instance {
        return LoadedPaneRecord::Quarantined(quarantine(
            entry.pane_instance,
            raw,
            "stored pane instance does not match current pane".to_string(),
        ));
    }
    LoadedPaneRecord::Valid(record)
}

pub fn quarantine_id(raw: &[u8]) -> String {
    let digest = Sha256::digest(raw);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn quarantine(pane_instance: PaneInstance, raw: String, message: String) -> QuarantinedPaneRecord {
    QuarantinedPaneRecord {
        error: PaneStateLoadError {
            pane_instance,
            quarantine_id: quarantine_id(raw.as_bytes()),
            message,
        },
        raw,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteAttempt {
    ReadBack(Option<String>),
    OutcomeUnknown(String),
    PaneMissing,
    ServerMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndependentRead {
    Value(Option<String>),
    PaneMissing,
    Unavailable(String),
    ServerMismatch,
}

pub trait PaneStateStoreIo {
    fn write_candidate(&mut self, pane: &PaneInstance, candidate: &str) -> WriteAttempt;

    fn read_independent(&mut self, pane: &PaneInstance) -> IndependentRead;
}

pub trait RecoveryClock {
    fn elapsed(&self) -> Duration;
}

#[derive(Debug)]
pub struct SystemRecoveryClock {
    started: Instant,
}

impl SystemRecoveryClock {
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl RecoveryClock for SystemRecoveryClock {
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistOutcome {
    CandidateConfirmed,
    ExpectedConfirmed,
    ThirdValue(String),
    PaneMissing,
    FailStop(String),
}

fn classify_read_back(
    read_back: Option<String>,
    expected: Option<&str>,
    candidate: &str,
) -> PersistOutcome {
    if read_back.as_deref() == Some(candidate) {
        PersistOutcome::CandidateConfirmed
    } else if read_back.as_deref() == expected {
        PersistOutcome::ExpectedConfirmed
    } else {
        PersistOutcome::ThirdValue(read_back.unwrap_or_default())
    }
}

pub struct TmuxPaneStateStoreIo<'a> {
    runner: &'a dyn TmuxRunner,
    server_pid: u32,
    server_start_time: i64,
}

impl<'a> TmuxPaneStateStoreIo<'a> {
    pub fn new(runner: &'a dyn TmuxRunner, server_pid: u32, server_start_time: i64) -> Self {
        Self {
            runner,
            server_pid,
            server_start_time,
        }
    }
}

impl PaneStateStoreIo for TmuxPaneStateStoreIo<'_> {
    fn write_candidate(&mut self, pane: &PaneInstance, candidate: &str) -> WriteAttempt {
        let pane_guard = format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid);
        let set_and_read = format!(
            "set-option -p -t {} {} {} ; show-options -pqv -t {} {}",
            pane.pane_id,
            KEY_PANE_STATE,
            quote_tmux_command_argument(candidate),
            pane.pane_id,
            KEY_PANE_STATE
        );
        let pane_command = format!(
            "if-shell -F -t {} {} {} {}",
            pane.pane_id,
            quote_tmux_command_argument(&pane_guard),
            quote_tmux_command_argument(&set_and_read),
            quote_tmux_command_argument(&format!("display-message -p '{PANE_MISMATCH_SENTINEL}'")),
        );
        let guarded = server_guarded_command_args(
            self.server_pid,
            self.server_start_time,
            pane_command,
            SERVER_MISMATCH_SENTINEL,
        );
        let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
        let result = self.runner.run(&refs);
        match result {
            Ok(output) if output.trim() == SERVER_MISMATCH_SENTINEL => WriteAttempt::ServerMismatch,
            Ok(output) if output.trim() == PANE_MISMATCH_SENTINEL => WriteAttempt::PaneMissing,
            Ok(output) => {
                let value = output.trim_end();
                WriteAttempt::ReadBack((!value.is_empty()).then(|| value.to_string()))
            }
            Err(error) => WriteAttempt::OutcomeUnknown(error.to_string()),
        }
    }

    fn read_independent(&mut self, pane: &PaneInstance) -> IndependentRead {
        let header = "#{pid}|#{start_time}|#{pane_pid}|#{@vde_pane_state}";
        let pane_guard = format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid);
        let read_command = tmux_command_string(&[
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane.pane_id.clone(),
            header.to_string(),
        ]);
        let pane_command = format!(
            "if-shell -F -t {} {} {} {}",
            pane.pane_id,
            quote_tmux_command_argument(&pane_guard),
            quote_tmux_command_argument(&read_command),
            quote_tmux_command_argument(&format!("display-message -p '{PANE_MISMATCH_SENTINEL}'")),
        );
        let guarded = server_guarded_command_args(
            self.server_pid,
            self.server_start_time,
            pane_command,
            SERVER_MISMATCH_SENTINEL,
        );
        let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
        match self.runner.run(&refs) {
            Ok(output) if output.trim() == SERVER_MISMATCH_SENTINEL => {
                IndependentRead::ServerMismatch
            }
            Ok(output) if output.trim() == PANE_MISMATCH_SENTINEL => IndependentRead::PaneMissing,
            Ok(output) => {
                let mut fields = output.trim_end().splitn(4, '|');
                let identity = (
                    fields.next().and_then(|value| value.parse::<u32>().ok()),
                    fields.next().and_then(|value| value.parse::<i64>().ok()),
                    fields.next().and_then(|value| value.parse::<u32>().ok()),
                );
                if identity.0 != Some(self.server_pid) || identity.1 != Some(self.server_start_time)
                {
                    return IndependentRead::ServerMismatch;
                }
                if identity.2 != Some(pane.pane_pid) {
                    return IndependentRead::PaneMissing;
                }
                let value = fields
                    .next()
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                IndependentRead::Value(value)
            }
            Err(error) => IndependentRead::Unavailable(error.to_string()),
        }
    }
}

pub fn quote_tmux_command_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn server_guarded_command_args(
    server_pid: u32,
    server_start_time: i64,
    true_command: String,
    mismatch_sentinel: &str,
) -> Vec<String> {
    vec![
        "if-shell".to_string(),
        "-F".to_string(),
        format!(
            "#{{&&:#{{==:#{{pid}},{server_pid}}},#{{==:#{{start_time}},{server_start_time}}}}}"
        ),
        true_command,
        format!(
            "display-message -p {}",
            quote_tmux_command_argument(mismatch_sentinel)
        ),
    ]
}

pub fn tmux_command_string(args: &[String]) -> String {
    args.iter()
        .map(|argument| {
            if argument == ";" {
                ";".to_string()
            } else {
                quote_tmux_command_argument(argument)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneStateDiagnostic {
    pub pane_instance: PaneInstance,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalTransition {
    pub pane_instance: PaneInstance,
    pub agent: Option<AgentKind>,
    pub from: Option<crate::daemon::session_badge::BadgeState>,
    pub to: Option<crate::daemon::session_badge::BadgeState>,
    pub at_epoch: i64,
    pub state_version: Option<StateVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalNotification {
    pub pane_instance: PaneInstance,
    pub state_version: StateVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
    pub outcome: ReductionOutcome,
    pub state_version: Option<StateVersion>,
    pub snapshot_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingSuccess {
    Apply(ApplyResult),
    Reset(StoredStateDescriptor),
}

#[derive(Debug, Clone)]
struct PendingTransaction {
    pane: PaneInstance,
    expected: Option<String>,
    candidate: String,
    draft: Box<CanonicalStateRuntime>,
    started_at: Duration,
    success: PendingSuccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingResolution {
    StillPending,
    Applied(ApplyResult),
    Reset(StoredStateDescriptor),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryPaneAvailability {
    Ready(Option<StoredStateDescriptor>),
    NotReady,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationDispatchSnapshot {
    pub pane_instance: PaneInstance,
    pub base: Option<StoredStateDescriptor>,
    pub tracker: CaptureTrackerSnapshot,
    pub state: Option<PaneState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewBatchFailure {
    pub pane_instance: PaneInstance,
    pub error: StoreError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewBatchApplyResult {
    pub committed: usize,
    pub failed: Vec<ViewBatchFailure>,
    pub snapshot_revision: u64,
}

#[derive(Debug)]
pub struct ViewBatchContinuation {
    batch_id: u64,
    working: Box<CanonicalStateRuntime>,
    remaining: VecDeque<PaneEventEnvelope>,
    pending_pane: PaneInstance,
    committed: usize,
    failed: Vec<ViewBatchFailure>,
    projection_changed: bool,
    revision_before: u64,
    done_clear_on: DoneClearOn,
}

impl ViewBatchContinuation {
    pub fn pending_recovery_remaining(&self, clock: &dyn RecoveryClock) -> Option<Duration> {
        self.working.pending_recovery_remaining(clock)
    }
}

#[derive(Debug)]
pub enum ViewBatchProgress {
    Complete(ViewBatchApplyResult),
    Pending(ViewBatchContinuation),
    Blocked(StoreError),
    Fatal(StoreError),
}

#[derive(Debug, Clone, Default)]
pub struct CanonicalStateRuntime {
    records: BTreeMap<PaneInstance, StoredPaneRecord>,
    quarantined: BTreeMap<PaneInstance, QuarantinedPaneRecord>,
    quarantine_observed_total: u64,
    uninitialized_raw: BTreeMap<PaneInstance, String>,
    trackers: BTreeMap<PaneInstance, CaptureTrackerSnapshot>,
    diagnostics: VecDeque<PaneStateDiagnostic>,
    transitions: VecDeque<CanonicalTransition>,
    notification_jobs: VecDeque<CanonicalNotification>,
    notification_queue_drops: u64,
    triage: BTreeMap<PaneInstance, crate::daemon::session_badge::BadgeState>,
    triage_calm_polls: BTreeMap<PaneInstance, u8>,
    flash: BTreeMap<PaneInstance, u8>,
    snapshot_revision: u64,
    snapshot_frame_too_large: bool,
    fail_stopped: bool,
    pending: Option<Box<PendingTransaction>>,
    next_view_batch_id: u64,
    active_view_batch_id: Option<u64>,
}

impl CanonicalStateRuntime {
    pub fn hydrate(entries: impl IntoIterator<Item = RawPaneRecord>) -> Self {
        let mut runtime = Self::default();
        for entry in entries {
            let pane = entry.pane_instance.clone();
            match load_record(entry) {
                LoadedPaneRecord::Missing => {}
                LoadedPaneRecord::Uninitialized { raw } => {
                    runtime.uninitialized_raw.insert(pane, raw);
                }
                LoadedPaneRecord::Valid(record) => {
                    if let StoredPaneRecord::Active(state) = &record {
                        runtime.trackers.insert(
                            pane.clone(),
                            CaptureTrackerSnapshot {
                                epoch: Some((state.state_id.clone(), state.agent_epoch)),
                                ..CaptureTrackerSnapshot::default()
                            },
                        );
                    }
                    runtime.records.insert(pane, record);
                }
                LoadedPaneRecord::Quarantined(record) => {
                    runtime.quarantined.insert(pane, record);
                    runtime.quarantine_observed_total =
                        runtime.quarantine_observed_total.saturating_add(1);
                }
            }
        }
        runtime
    }

    pub fn record(&self, pane: &PaneInstance) -> Option<&StoredPaneRecord> {
        self.records.get(pane)
    }

    pub fn records_snapshot(&self) -> BTreeMap<PaneInstance, StoredPaneRecord> {
        self.records.clone()
    }

    pub fn tracked_panes(&self) -> Vec<PaneInstance> {
        let mut panes = self
            .records
            .keys()
            .chain(self.quarantined.keys())
            .chain(self.uninitialized_raw.keys())
            .chain(self.trackers.keys())
            .cloned()
            .collect::<Vec<_>>();
        panes.sort();
        panes.dedup();
        panes
    }

    pub fn quarantined(&self, pane: &PaneInstance) -> Option<&QuarantinedPaneRecord> {
        self.quarantined.get(pane)
    }

    pub fn quarantine_count(&self) -> usize {
        self.quarantined.len()
    }

    pub fn quarantine_observed_total(&self) -> u64 {
        self.quarantine_observed_total
    }

    pub fn tracker(&self, pane: &PaneInstance) -> CaptureTrackerSnapshot {
        self.trackers.get(pane).cloned().unwrap_or_default()
    }

    pub fn freeze_observation_dispatch(
        &self,
        panes: impl IntoIterator<Item = PaneInstance>,
    ) -> Vec<ObservationDispatchSnapshot> {
        let mut snapshots = panes
            .into_iter()
            .map(|pane_instance| ObservationDispatchSnapshot {
                base: self.descriptor(&pane_instance),
                tracker: self.tracker(&pane_instance),
                state: match self.records.get(&pane_instance) {
                    Some(StoredPaneRecord::Active(state)) => Some(state.clone()),
                    _ => None,
                },
                pane_instance,
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.pane_instance.cmp(&right.pane_instance));
        snapshots
    }

    pub fn diagnostics(&self) -> &VecDeque<PaneStateDiagnostic> {
        &self.diagnostics
    }

    pub fn transitions(&self) -> &VecDeque<CanonicalTransition> {
        &self.transitions
    }

    pub fn triage_panes(&self) -> impl Iterator<Item = &PaneInstance> {
        self.triage.keys()
    }

    pub fn triage_entries(
        &self,
    ) -> impl Iterator<Item = (&PaneInstance, crate::daemon::session_badge::BadgeState)> {
        self.triage.iter().map(|(pane, badge)| (pane, *badge))
    }

    pub fn flashing_panes(&self) -> impl Iterator<Item = &PaneInstance> {
        self.flash.keys()
    }

    pub fn notification_jobs(&self) -> &VecDeque<CanonicalNotification> {
        &self.notification_jobs
    }

    pub fn drain_notification_jobs(&mut self) -> Vec<CanonicalNotification> {
        self.notification_jobs.drain(..).collect()
    }

    pub fn notification_queue_drops(&self) -> u64 {
        self.notification_queue_drops
    }

    pub fn advance_poll_projection(&mut self) -> Result<bool, StoreError> {
        let mut draft = self.clone();
        let mut visible_changed = false;
        let triaged = draft.triage.keys().cloned().collect::<Vec<_>>();
        for pane in triaged {
            if record_badge(draft.records.get(&pane))
                == Some(crate::daemon::session_badge::BadgeState::Blocked)
            {
                draft.triage_calm_polls.remove(&pane);
                continue;
            }
            let calm = draft.triage_calm_polls.entry(pane.clone()).or_default();
            *calm = calm.saturating_add(1);
            if *calm >= 2 {
                draft.triage.remove(&pane);
                draft.triage_calm_polls.remove(&pane);
                visible_changed = true;
            }
        }
        let flashing = draft.flash.keys().cloned().collect::<Vec<_>>();
        for pane in flashing {
            let remaining = draft.flash.get(&pane).copied().unwrap_or_default();
            if remaining <= 1 {
                draft.flash.remove(&pane);
                visible_changed = true;
            } else {
                draft.flash.insert(pane, remaining - 1);
            }
        }
        if visible_changed {
            draft.bump_snapshot_revision()?;
        }
        draft.validate_projection()?;
        let _ = draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;
        *self = draft;
        Ok(visible_changed)
    }

    pub fn add_diagnostic(
        &mut self,
        pane_instance: PaneInstance,
        message: impl Into<String>,
    ) -> Result<(), StoreError> {
        self.push_diagnostic(pane_instance, message.into())
    }

    pub fn finish_sequenced_projection(
        &mut self,
        diagnostic_pane: Option<&PaneInstance>,
        messages: impl IntoIterator<Item = String>,
        projection_changed: bool,
        revision_before: u64,
    ) -> Result<u64, StoreError> {
        let mut draft = self.clone();
        let mut changed = projection_changed;
        if let Some(pane) = diagnostic_pane {
            for message in messages {
                draft.diagnostics.push_back(PaneStateDiagnostic {
                    pane_instance: pane.clone(),
                    message,
                });
                while draft.diagnostics.len() > MAX_DIAGNOSTICS {
                    draft.diagnostics.pop_front();
                }
                changed = true;
            }
        }
        if changed && draft.snapshot_revision == revision_before {
            draft.bump_snapshot_revision()?;
        }
        draft.validate_projection()?;
        let preflight_changed = draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;
        if preflight_changed && draft.snapshot_revision == revision_before {
            draft.bump_snapshot_revision()?;
            let _ = draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;
        }
        let revision = draft.snapshot_revision;
        *self = draft;
        Ok(revision)
    }

    pub fn mark_projection_changed(&mut self) -> Result<u64, StoreError> {
        let mut draft = self.clone();
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        let _ = draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;
        let revision = draft.snapshot_revision;
        *self = draft;
        Ok(revision)
    }

    pub fn remove_absent_pane(
        &mut self,
        pane: &PaneInstance,
        expected: Option<&StoredStateDescriptor>,
    ) -> Result<bool, StoreError> {
        if self.fail_stopped {
            return Err(StoreError::FailStop("daemon is fail-stopped".to_string()));
        }
        if self.sequenced_mutations_paused() {
            return Err(StoreError::PersistPending);
        }
        if self.descriptor(pane).as_ref() != expected {
            return Ok(false);
        }
        let mut draft = self.clone();
        let removed = draft.records.remove(pane).is_some()
            | draft.quarantined.remove(pane).is_some()
            | draft.uninitialized_raw.remove(pane).is_some()
            | draft.trackers.remove(pane).is_some();
        if !removed {
            return Ok(false);
        }
        draft.triage.remove(pane);
        draft.triage_calm_polls.remove(pane);
        draft.flash.remove(pane);
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        let _ = draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;
        *self = draft;
        Ok(true)
    }

    pub fn snapshot_frame_too_large(&self) -> bool {
        self.snapshot_frame_too_large
    }

    pub fn snapshot_revision(&self) -> u64 {
        self.snapshot_revision
    }

    #[cfg(test)]
    pub(crate) fn set_snapshot_revision_for_test(&mut self, revision: u64) {
        self.snapshot_revision = revision;
    }

    pub fn is_fail_stopped(&self) -> bool {
        self.fail_stopped
    }

    pub fn sequenced_mutations_paused(&self) -> bool {
        self.pending.is_some() || self.active_view_batch_id.is_some()
    }

    pub fn pending_recovery_remaining(&self, clock: &dyn RecoveryClock) -> Option<Duration> {
        self.pending.as_ref().map(|transaction| {
            STORE_RECOVERY_DEADLINE
                .saturating_sub(clock.elapsed().saturating_sub(transaction.started_at))
        })
    }

    pub fn query_committed_snapshot_revision(&self) -> u64 {
        self.snapshot_revision
    }

    pub fn query_pane_while_paused(
        &self,
        pane: &PaneInstance,
        presentation_cache_hit: bool,
    ) -> QueryPaneAvailability {
        if presentation_cache_hit || !self.sequenced_mutations_paused() {
            QueryPaneAvailability::Ready(self.descriptor(pane))
        } else {
            QueryPaneAvailability::NotReady
        }
    }

    pub fn apply_event(
        &mut self,
        io: &mut dyn PaneStateStoreIo,
        clock: &mut dyn RecoveryClock,
        envelope: &PaneEventEnvelope,
        visibility: &VisibilitySnapshot,
        done_clear_on: DoneClearOn,
    ) -> Result<ApplyResult, StoreError> {
        if self.fail_stopped {
            return Err(StoreError::FailStop("daemon is fail-stopped".to_string()));
        }
        if self.sequenced_mutations_paused() {
            return Err(StoreError::PersistPending);
        }
        if self.quarantined.contains_key(&envelope.pane_instance) {
            return Err(StoreError::StateLoad(
                self.quarantined[&envelope.pane_instance].error.clone(),
            ));
        }
        let current = self.records.get(&envelope.pane_instance);
        let tracker = self.tracker(&envelope.pane_instance);
        let new_state_id = if event_can_create_record(current, &envelope.event) {
            Some(StateId::generate().map_err(|error| StoreError::Random(error.to_string()))?)
        } else {
            None
        };
        let reduction = reduce(
            current,
            envelope,
            ReductionContext {
                done_clear_on,
                visibility,
                tracker: &tracker,
                new_state_id,
            },
        )?;
        if reduction.outcome != ReductionOutcome::CanonicalChanged {
            if let Some(delta) = reduction.tracker_delta {
                self.trackers
                    .insert(envelope.pane_instance.clone(), delta.next);
            }
            return Ok(ApplyResult {
                outcome: reduction.outcome,
                state_version: self.active_version(&envelope.pane_instance),
                snapshot_revision: self.snapshot_revision,
            });
        }

        let candidate = reduction.record.expect("canonical mutation has a record");
        let candidate_raw = serialize_record(&candidate)?;
        let expected_raw = match current {
            Some(record) => Some(serialize_record(record)?),
            None => self.uninitialized_raw.get(&envelope.pane_instance).cloned(),
        };
        let mut draft = self.clone();
        draft
            .records
            .insert(envelope.pane_instance.clone(), candidate.clone());
        draft.quarantined.remove(&envelope.pane_instance);
        draft.uninitialized_raw.remove(&envelope.pane_instance);
        if let Some(delta) = reduction.tracker_delta {
            draft
                .trackers
                .insert(envelope.pane_instance.clone(), delta.next);
        }
        draft.derive_candidate_projection(
            &envelope.pane_instance,
            current,
            Some(&candidate),
            transition_at_epoch(&envelope.event, Some(&candidate)),
        );
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        let _ = draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;

        let success = ApplyResult {
            outcome: ReductionOutcome::CanonicalChanged,
            state_version: draft.active_version(&envelope.pane_instance),
            snapshot_revision: draft.snapshot_revision,
        };
        let transaction = PendingTransaction {
            pane: envelope.pane_instance.clone(),
            expected: expected_raw,
            candidate: candidate_raw,
            draft: Box::new(draft),
            started_at: clock.elapsed(),
            success: PendingSuccess::Apply(success),
        };
        match io.write_candidate(&transaction.pane, &transaction.candidate) {
            WriteAttempt::ReadBack(read_back) => {
                let outcome = classify_read_back(
                    read_back,
                    transaction.expected.as_deref(),
                    &transaction.candidate,
                );
                match self.finish_pending_outcome(transaction, outcome)? {
                    PendingResolution::Applied(result) => Ok(result),
                    _ => unreachable!(),
                }
            }
            WriteAttempt::PaneMissing => Err(StoreError::InvalidPaneInstance),
            WriteAttempt::ServerMismatch => {
                self.fail_stopped = true;
                Err(StoreError::FailStop(
                    "tmux server incarnation changed".to_string(),
                ))
            }
            WriteAttempt::OutcomeUnknown(_) => {
                self.pending = Some(Box::new(transaction));
                Err(StoreError::PersistPending)
            }
        }
    }

    pub fn apply_view_acknowledgement_batch(
        &mut self,
        io: &mut dyn PaneStateStoreIo,
        clock: &mut dyn RecoveryClock,
        envelopes: &[PaneEventEnvelope],
        done_clear_on: DoneClearOn,
    ) -> ViewBatchProgress {
        if self.fail_stopped {
            return ViewBatchProgress::Fatal(StoreError::FailStop(
                "daemon is fail-stopped".to_string(),
            ));
        }
        if self.sequenced_mutations_paused() {
            return ViewBatchProgress::Blocked(StoreError::PersistPending);
        }
        let Some(batch_id) = self.next_view_batch_id.checked_add(1) else {
            self.fail_stopped = true;
            return ViewBatchProgress::Fatal(StoreError::CounterOverflow("view batch ID"));
        };
        self.next_view_batch_id = batch_id;
        self.active_view_batch_id = Some(batch_id);
        let mut working = self.clone();
        working.active_view_batch_id = None;
        let continuation = ViewBatchContinuation {
            batch_id,
            working: Box::new(working),
            remaining: envelopes.iter().cloned().collect(),
            pending_pane: PaneInstance {
                pane_id: "%0".to_string(),
                pane_pid: 1,
            },
            committed: 0,
            failed: Vec::new(),
            projection_changed: false,
            revision_before: self.snapshot_revision,
            done_clear_on,
        };
        self.advance_view_batch(io, clock, continuation)
    }

    pub fn resume_view_acknowledgement_batch(
        &mut self,
        io: &mut dyn PaneStateStoreIo,
        clock: &mut dyn RecoveryClock,
        mut continuation: ViewBatchContinuation,
    ) -> ViewBatchProgress {
        if self.active_view_batch_id != Some(continuation.batch_id)
            || self.snapshot_revision != continuation.revision_before
        {
            self.fail_stopped = true;
            self.active_view_batch_id = None;
            return ViewBatchProgress::Fatal(StoreError::FailStop(
                "invalid or stale view batch continuation".to_string(),
            ));
        }
        match continuation.working.resolve_pending(io, clock) {
            Ok(PendingResolution::StillPending) => {
                return ViewBatchProgress::Pending(continuation);
            }
            Ok(PendingResolution::Applied(result)) => {
                continuation.projection_changed |=
                    continuation.working.snapshot_revision != continuation.revision_before;
                if result.outcome == ReductionOutcome::CanonicalChanged {
                    continuation.committed += 1;
                }
            }
            Ok(PendingResolution::Reset(_)) => unreachable!("view batch cannot reset pane state"),
            Err(error) if error.requires_daemon_exit() => {
                self.fail_stopped = true;
                self.active_view_batch_id = None;
                return ViewBatchProgress::Fatal(error);
            }
            Err(error) => {
                if let Err(fatal) = continuation.working.push_diagnostic(
                    continuation.pending_pane.clone(),
                    format!("view acknowledgment failed: {error}"),
                ) {
                    self.fail_stopped = true;
                    self.active_view_batch_id = None;
                    return ViewBatchProgress::Fatal(fatal);
                }
                continuation.projection_changed = true;
                continuation.failed.push(ViewBatchFailure {
                    pane_instance: continuation.pending_pane.clone(),
                    error,
                });
            }
        }
        self.advance_view_batch(io, clock, continuation)
    }

    fn advance_view_batch(
        &mut self,
        io: &mut dyn PaneStateStoreIo,
        clock: &mut dyn RecoveryClock,
        mut continuation: ViewBatchContinuation,
    ) -> ViewBatchProgress {
        while let Some(envelope) = continuation.remaining.pop_front() {
            continuation.working.snapshot_revision = continuation.revision_before;
            if !matches!(envelope.event, PaneEvent::AcknowledgeView { .. }) {
                let error = StoreError::Reduce(ReduceError::InvalidRequest(
                    "view batch accepts only AcknowledgeView events".to_string(),
                ));
                if let Err(fatal) = continuation.working.push_diagnostic(
                    envelope.pane_instance.clone(),
                    format!("view acknowledgment failed: {error}"),
                ) {
                    self.fail_stopped = true;
                    self.active_view_batch_id = None;
                    return ViewBatchProgress::Fatal(fatal);
                }
                continuation.projection_changed = true;
                continuation.failed.push(ViewBatchFailure {
                    pane_instance: envelope.pane_instance,
                    error,
                });
                continue;
            }
            match continuation.working.apply_event(
                io,
                clock,
                &envelope,
                &VisibilitySnapshot::default(),
                continuation.done_clear_on,
            ) {
                Ok(result) => {
                    continuation.projection_changed |=
                        continuation.working.snapshot_revision != continuation.revision_before;
                    if result.outcome == ReductionOutcome::CanonicalChanged {
                        continuation.committed += 1;
                    }
                }
                Err(StoreError::PersistPending) => {
                    continuation.pending_pane = envelope.pane_instance;
                    return ViewBatchProgress::Pending(continuation);
                }
                Err(error) if error.requires_daemon_exit() => {
                    self.fail_stopped = true;
                    self.active_view_batch_id = None;
                    return ViewBatchProgress::Fatal(error);
                }
                Err(error) => {
                    if let Err(fatal) = continuation.working.push_diagnostic(
                        envelope.pane_instance.clone(),
                        format!("view acknowledgment failed: {error}"),
                    ) {
                        self.fail_stopped = true;
                        self.active_view_batch_id = None;
                        return ViewBatchProgress::Fatal(fatal);
                    }
                    continuation.projection_changed = true;
                    continuation.failed.push(ViewBatchFailure {
                        pane_instance: envelope.pane_instance,
                        error,
                    });
                }
            }
        }
        continuation.working.snapshot_revision = continuation.revision_before;
        if continuation.projection_changed {
            let Some(revision) = continuation.revision_before.checked_add(1) else {
                self.fail_stopped = true;
                self.active_view_batch_id = None;
                return ViewBatchProgress::Fatal(StoreError::CounterOverflow("snapshot revision"));
            };
            continuation.working.snapshot_revision = revision;
        }
        if let Err(error) = continuation.working.validate_projection() {
            self.fail_stopped = true;
            self.active_view_batch_id = None;
            return ViewBatchProgress::Fatal(error);
        }
        let preflight_changed = match continuation
            .working
            .preflight_projection(MAX_RESPONSE_FRAME_BYTES)
        {
            Ok(changed) => changed,
            Err(error) => {
                self.fail_stopped = true;
                self.active_view_batch_id = None;
                return ViewBatchProgress::Fatal(error);
            }
        };
        if preflight_changed && !continuation.projection_changed {
            let Some(revision) = continuation.revision_before.checked_add(1) else {
                self.fail_stopped = true;
                self.active_view_batch_id = None;
                return ViewBatchProgress::Fatal(StoreError::CounterOverflow("snapshot revision"));
            };
            continuation.working.snapshot_revision = revision;
            if let Err(error) = continuation
                .working
                .preflight_projection(MAX_RESPONSE_FRAME_BYTES)
            {
                self.fail_stopped = true;
                self.active_view_batch_id = None;
                return ViewBatchProgress::Fatal(error);
            }
        }
        continuation.projection_changed |= preflight_changed;
        let result = ViewBatchApplyResult {
            committed: continuation.committed,
            failed: continuation.failed,
            snapshot_revision: continuation.working.snapshot_revision,
        };
        continuation.working.active_view_batch_id = None;
        *self = *continuation.working;
        ViewBatchProgress::Complete(result)
    }

    pub fn reset(
        &mut self,
        io: &mut dyn PaneStateStoreIo,
        clock: &mut dyn RecoveryClock,
        pane: &PaneInstance,
        expected: &StoredStateDescriptor,
        reset_at: i64,
        tombstone_id: ResetTombstoneId,
    ) -> Result<StoredStateDescriptor, StoreError> {
        if self.fail_stopped {
            return Err(StoreError::FailStop("daemon is fail-stopped".to_string()));
        }
        if self.sequenced_mutations_paused() {
            return Err(StoreError::PersistPending);
        }
        let current_descriptor = self
            .descriptor(pane)
            .ok_or(StoreError::StaleStateIdentity)?;
        if &current_descriptor != expected {
            return Err(StoreError::StaleStateIdentity);
        }
        if matches!(current_descriptor, StoredStateDescriptor::Reset { .. }) {
            return Ok(current_descriptor);
        }
        let expected_raw = if let Some(record) = self.records.get(pane) {
            serialize_record(record)?
        } else {
            self.quarantined
                .get(pane)
                .ok_or(StoreError::StaleStateIdentity)?
                .raw
                .clone()
        };
        let tombstone = StoredPaneRecord::Reset(ResetTombstone {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            tombstone_id,
            pane_instance: pane.clone(),
            reset_at,
        });
        let candidate = serialize_record(&tombstone)?;
        let descriptor = tombstone.descriptor();
        let mut draft = self.clone();
        draft.pending = None;
        draft.records.insert(pane.clone(), tombstone.clone());
        draft.quarantined.remove(pane);
        draft.trackers.remove(pane);
        draft.derive_candidate_projection(pane, self.records.get(pane), Some(&tombstone), reset_at);
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        let _ = draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;
        let transaction = PendingTransaction {
            pane: pane.clone(),
            expected: Some(expected_raw),
            candidate,
            draft: Box::new(draft),
            started_at: clock.elapsed(),
            success: PendingSuccess::Reset(descriptor),
        };
        match io.write_candidate(&transaction.pane, &transaction.candidate) {
            WriteAttempt::ReadBack(read_back) => {
                let outcome = classify_read_back(
                    read_back,
                    transaction.expected.as_deref(),
                    &transaction.candidate,
                );
                match self.finish_pending_outcome(transaction, outcome)? {
                    PendingResolution::Reset(descriptor) => Ok(descriptor),
                    _ => unreachable!(),
                }
            }
            WriteAttempt::PaneMissing => Err(StoreError::InvalidPaneInstance),
            WriteAttempt::ServerMismatch => {
                self.fail_stopped = true;
                Err(StoreError::FailStop(
                    "tmux server incarnation changed".to_string(),
                ))
            }
            WriteAttempt::OutcomeUnknown(_) => {
                self.pending = Some(Box::new(transaction));
                Err(StoreError::PersistPending)
            }
        }
    }

    pub fn resolve_pending(
        &mut self,
        io: &mut dyn PaneStateStoreIo,
        clock: &dyn RecoveryClock,
    ) -> Result<PendingResolution, StoreError> {
        if self.fail_stopped {
            return Err(StoreError::FailStop("daemon is fail-stopped".to_string()));
        }
        let Some(transaction) = self.pending.take().map(|pending| *pending) else {
            return Ok(PendingResolution::StillPending);
        };
        let elapsed = clock.elapsed().saturating_sub(transaction.started_at);
        if elapsed >= STORE_RECOVERY_DEADLINE {
            return self.finish_pending_outcome(
                transaction,
                PersistOutcome::FailStop(
                    "pane state outcome remained unknown for 5 seconds".to_string(),
                ),
            );
        }
        let outcome = match io.read_independent(&transaction.pane) {
            IndependentRead::Value(read_back) => classify_read_back(
                read_back,
                transaction.expected.as_deref(),
                &transaction.candidate,
            ),
            IndependentRead::PaneMissing => PersistOutcome::PaneMissing,
            IndependentRead::ServerMismatch => {
                PersistOutcome::FailStop("tmux server incarnation changed".to_string())
            }
            IndependentRead::Unavailable(message) => {
                let elapsed = clock.elapsed().saturating_sub(transaction.started_at);
                if elapsed < STORE_RECOVERY_DEADLINE {
                    self.pending = Some(Box::new(transaction));
                    return Ok(PendingResolution::StillPending);
                }
                PersistOutcome::FailStop(format!(
                    "pane state outcome remained unknown for 5 seconds: {message}"
                ))
            }
        };
        self.finish_pending_outcome(transaction, outcome)
    }

    fn finish_pending_outcome(
        &mut self,
        transaction: PendingTransaction,
        outcome: PersistOutcome,
    ) -> Result<PendingResolution, StoreError> {
        match outcome {
            PersistOutcome::CandidateConfirmed => {
                *self = *transaction.draft;
                match transaction.success {
                    PendingSuccess::Apply(result) => Ok(PendingResolution::Applied(result)),
                    PendingSuccess::Reset(descriptor) => Ok(PendingResolution::Reset(descriptor)),
                }
            }
            PersistOutcome::ExpectedConfirmed => {
                let message = match transaction.success {
                    PendingSuccess::Apply(_) => "pane state write did not commit",
                    PendingSuccess::Reset(_) => "pane state reset did not commit",
                };
                let mut draft = self.clone();
                draft.pending = None;
                draft.push_diagnostic(transaction.pane, message.to_string())?;
                *self = draft;
                Err(StoreError::PersistFailed(message.to_string()))
            }
            PersistOutcome::ThirdValue(raw) => {
                let message = match transaction.success {
                    PendingSuccess::Apply(_) => "pane state read-back returned a third value",
                    PendingSuccess::Reset(_) => "pane state reset read-back returned a third value",
                };
                let quarantined = quarantine(transaction.pane.clone(), raw, message.to_string());
                let mut draft = self.clone();
                draft.pending = None;
                draft.records.remove(&transaction.pane);
                draft.trackers.remove(&transaction.pane);
                draft.uninitialized_raw.remove(&transaction.pane);
                let newly_observed = draft
                    .quarantined
                    .insert(transaction.pane.clone(), quarantined.clone())
                    .is_none();
                if newly_observed {
                    draft.quarantine_observed_total =
                        draft.quarantine_observed_total.saturating_add(1);
                }
                draft.push_diagnostic(transaction.pane, quarantined.error.message.clone())?;
                *self = draft;
                Err(StoreError::ExternalWriter(quarantined.error))
            }
            PersistOutcome::PaneMissing => Err(StoreError::InvalidPaneInstance),
            PersistOutcome::FailStop(message) => {
                self.fail_stopped = true;
                Err(StoreError::FailStop(message))
            }
        }
    }

    fn active_version(&self, pane: &PaneInstance) -> Option<StateVersion> {
        match self.records.get(pane) {
            Some(StoredPaneRecord::Active(state)) => Some(state.version()),
            _ => None,
        }
    }

    pub fn descriptor(&self, pane: &PaneInstance) -> Option<StoredStateDescriptor> {
        self.records
            .get(pane)
            .map(StoredPaneRecord::descriptor)
            .or_else(|| {
                self.quarantined
                    .get(pane)
                    .map(|record| StoredStateDescriptor::Quarantined {
                        quarantine_id: record.error.quarantine_id.clone(),
                    })
            })
    }

    fn derive_candidate_projection(
        &mut self,
        pane: &PaneInstance,
        previous: Option<&StoredPaneRecord>,
        current: Option<&StoredPaneRecord>,
        at_epoch: i64,
    ) {
        let previous_badge = record_badge(previous);
        let current_badge = record_badge(current);
        let discarded_completion = match (previous, current) {
            (Some(StoredPaneRecord::Active(previous)), Some(StoredPaneRecord::Active(current))) => {
                previous_badge == Some(crate::daemon::session_badge::BadgeState::Done)
                    && (previous.state_id != current.state_id
                        || previous.agent_epoch != current.agent_epoch
                        || previous.agent != current.agent
                        || previous.agent_session_id != current.agent_session_id)
            }
            _ => false,
        };
        let (agent, at_epoch) = if discarded_completion {
            let previous = match previous {
                Some(StoredPaneRecord::Active(previous)) => previous,
                _ => unreachable!("discarded completion requires a previous active state"),
            };
            (
                Some(previous.agent.clone()),
                previous
                    .completed_at
                    .or(previous.started_at)
                    .unwrap_or(at_epoch),
            )
        } else {
            (
                current
                    .and_then(record_agent)
                    .or_else(|| previous.and_then(record_agent))
                    .cloned(),
                at_epoch,
            )
        };
        let state_version = match current {
            Some(StoredPaneRecord::Active(state)) => Some(state.version()),
            _ => None,
        };
        self.transitions.push_back(CanonicalTransition {
            pane_instance: pane.clone(),
            agent,
            from: previous_badge,
            to: current_badge,
            at_epoch,
            state_version: state_version.clone(),
        });
        while self.transitions.len() > MAX_DIAGNOSTICS {
            self.transitions.pop_front();
        }
        if previous_badge != current_badge {
            self.flash.insert(pane.clone(), 2);
            if current_badge == Some(crate::daemon::session_badge::BadgeState::Blocked)
                && let Some(state_version) = state_version
            {
                self.notification_jobs.push_back(CanonicalNotification {
                    pane_instance: pane.clone(),
                    state_version,
                });
                while self.notification_jobs.len() > 64 {
                    self.notification_jobs.pop_front();
                    self.notification_queue_drops = self.notification_queue_drops.saturating_add(1);
                    self.diagnostics.push_back(PaneStateDiagnostic {
                        pane_instance: pane.clone(),
                        message: "notification_queue_overflow: dropped_oldest".to_string(),
                    });
                    while self.diagnostics.len() > MAX_DIAGNOSTICS {
                        self.diagnostics.pop_front();
                    }
                }
            }
        }
        if let Some(badge @ crate::daemon::session_badge::BadgeState::Blocked) = current_badge {
            self.triage.insert(pane.clone(), badge);
            self.triage_calm_polls.remove(pane);
        }
    }

    fn preflight_projection(&mut self, response_limit: usize) -> Result<bool, StoreError> {
        #[derive(Serialize)]
        struct ProjectionPreflight<'a> {
            snapshot_revision: u64,
            records: Vec<&'a StoredPaneRecord>,
            transitions: &'a VecDeque<CanonicalTransition>,
            diagnostics: &'a VecDeque<PaneStateDiagnostic>,
            triage: Vec<(&'a PaneInstance, crate::daemon::session_badge::BadgeState)>,
            flash: Vec<(&'a PaneInstance, u8)>,
        }

        let bytes = serde_json::to_vec(&ProjectionPreflight {
            snapshot_revision: self.snapshot_revision,
            records: self.records.values().collect(),
            transitions: &self.transitions,
            diagnostics: &self.diagnostics,
            triage: self
                .triage
                .iter()
                .map(|(pane, badge)| (pane, *badge))
                .collect(),
            flash: self
                .flash
                .iter()
                .map(|(pane, polls)| (pane, *polls))
                .collect(),
        })
        .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
        let previous_frame_too_large = self.snapshot_frame_too_large;
        let diagnostics_before = self.diagnostics.clone();
        self.snapshot_frame_too_large = bytes.len() > response_limit;
        if self.snapshot_frame_too_large
            && !self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "resolved snapshot exceeds frame limit")
        {
            let pane_instance = self.records.keys().next().cloned().unwrap_or(PaneInstance {
                pane_id: "%0".to_string(),
                pane_pid: 1,
            });
            self.diagnostics.push_back(PaneStateDiagnostic {
                pane_instance,
                message: "resolved snapshot exceeds frame limit".to_string(),
            });
            while self.diagnostics.len() > MAX_DIAGNOSTICS {
                self.diagnostics.pop_front();
            }
        }
        Ok(previous_frame_too_large != self.snapshot_frame_too_large
            || diagnostics_before != self.diagnostics)
    }

    fn bump_snapshot_revision(&mut self) -> Result<(), StoreError> {
        self.snapshot_revision = self
            .snapshot_revision
            .checked_add(1)
            .ok_or(StoreError::CounterOverflow("snapshot revision"))?;
        Ok(())
    }

    fn push_diagnostic(
        &mut self,
        pane_instance: PaneInstance,
        message: String,
    ) -> Result<(), StoreError> {
        let mut draft = self.clone();
        draft.diagnostics.push_back(PaneStateDiagnostic {
            pane_instance,
            message,
        });
        while draft.diagnostics.len() > MAX_DIAGNOSTICS {
            draft.diagnostics.pop_front();
        }
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        let _ = draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;
        *self = draft;
        Ok(())
    }

    fn validate_projection(&self) -> Result<(), StoreError> {
        for (pane, record) in &self.records {
            if record.pane_instance() != pane {
                return Err(StoreError::PersistFailed(
                    "canonical cache key and pane instance disagree".to_string(),
                ));
            }
            record
                .validate()
                .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
        }
        if self.transitions.len() > MAX_DIAGNOSTICS
            || self.diagnostics.len() > MAX_DIAGNOSTICS
            || self.notification_jobs.len() > 64
            || self.triage.len() > MAX_DIAGNOSTICS
            || self.triage_calm_polls.len() > MAX_DIAGNOSTICS
            || self.flash.len() > MAX_DIAGNOSTICS
        {
            return Err(StoreError::PersistFailed(
                "projection collection exceeds configured bound".to_string(),
            ));
        }
        Ok(())
    }
}

fn record_badge(
    record: Option<&StoredPaneRecord>,
) -> Option<crate::daemon::session_badge::BadgeState> {
    match record {
        Some(StoredPaneRecord::Active(state)) => Some(crate::pane_state::resolve_badge(state)),
        _ => None,
    }
}

fn record_agent(record: &StoredPaneRecord) -> Option<&AgentKind> {
    match record {
        StoredPaneRecord::Active(state) => Some(&state.agent),
        StoredPaneRecord::Reset(_) => None,
    }
}

fn transition_at_epoch(event: &PaneEvent, current: Option<&StoredPaneRecord>) -> i64 {
    match event {
        PaneEvent::AgentSessionStarted { observed_at, .. }
        | PaneEvent::ActivityObserved { observed_at }
        | PaneEvent::ActivityAndProgressObserved { observed_at, .. }
        | PaneEvent::WaitRequested { observed_at, .. }
        | PaneEvent::FailRun { observed_at, .. }
        | PaneEvent::ProgressUpdated { observed_at, .. }
        | PaneEvent::ObservationBatch { observed_at, .. } => *observed_at,
        PaneEvent::BeginRun { started_at, .. } => *started_at,
        PaneEvent::CompleteRun { completed_at } | PaneEvent::MarkDone { completed_at, .. } => {
            *completed_at
        }
        PaneEvent::ExplicitStateReported { report } => report.observed_at,
        PaneEvent::AcknowledgeView { .. } | PaneEvent::PaneRemoved { .. } => current
            .and_then(|record| match record {
                StoredPaneRecord::Active(state) => state.completed_at.or(state.started_at),
                StoredPaneRecord::Reset(tombstone) => Some(tombstone.reset_at),
            })
            .unwrap_or_default(),
    }
}

fn event_can_create_record(current: Option<&StoredPaneRecord>, event: &PaneEvent) -> bool {
    let missing = current.is_none();
    let reset = matches!(current, Some(StoredPaneRecord::Reset(_)));
    if !missing && !reset {
        return false;
    }
    match event {
        PaneEvent::AgentSessionStarted { .. }
        | PaneEvent::BeginRun { .. }
        | PaneEvent::ActivityObserved { .. }
        | PaneEvent::ActivityAndProgressObserved { .. }
        | PaneEvent::WaitRequested { .. }
        | PaneEvent::FailRun { .. } => true,
        PaneEvent::CompleteRun { .. } if missing => true,
        PaneEvent::ExplicitStateReported { report } => match &report.lifecycle {
            Some(ReportedLifecycle::Running)
            | Some(ReportedLifecycle::Waiting { .. })
            | Some(ReportedLifecycle::Error { .. }) => true,
            Some(ReportedLifecycle::Idle) if missing => {
                report.completed_at.is_some() || report.attention
            }
            _ => false,
        },
        PaneEvent::ObservationBatch {
            presence: AgentPresenceObservation::Present(_),
            ..
        } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crate::daemon::session_badge::BadgeState;

    use super::*;

    const STATE_ID: &str = "00112233445566778899aabbccddeeff";
    const EVENT_ID: &str = "102132435465768798a9bacbdcedfe0f";
    const DAEMON_ID: &str = "ffeeddccbbaa99887766554433221100";

    #[derive(Debug, Default)]
    struct FakeIo {
        writes: Vec<String>,
        write_results: VecDeque<WriteAttempt>,
        reads: VecDeque<IndependentRead>,
    }

    impl PaneStateStoreIo for FakeIo {
        fn write_candidate(&mut self, _pane: &PaneInstance, candidate: &str) -> WriteAttempt {
            self.writes.push(candidate.to_string());
            self.write_results
                .pop_front()
                .unwrap_or_else(|| WriteAttempt::ReadBack(Some(candidate.to_string())))
        }

        fn read_independent(&mut self, _pane: &PaneInstance) -> IndependentRead {
            self.reads
                .pop_front()
                .unwrap_or_else(|| IndependentRead::Unavailable("offline".to_string()))
        }
    }

    struct ExpectedPaneIo {
        fail_pane_id: String,
        expected: String,
    }

    impl PaneStateStoreIo for ExpectedPaneIo {
        fn write_candidate(&mut self, pane: &PaneInstance, candidate: &str) -> WriteAttempt {
            if pane.pane_id == self.fail_pane_id {
                WriteAttempt::ReadBack(Some(self.expected.clone()))
            } else {
                WriteAttempt::ReadBack(Some(candidate.to_string()))
            }
        }

        fn read_independent(&mut self, _pane: &PaneInstance) -> IndependentRead {
            IndependentRead::Unavailable("unused".to_string())
        }
    }

    #[derive(Debug, Default)]
    struct FakeClock(Duration);

    impl RecoveryClock for FakeClock {
        fn elapsed(&self) -> Duration {
            self.0
        }
    }

    fn pane() -> PaneInstance {
        PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 100,
        }
    }

    fn envelope(event: PaneEvent) -> PaneEventEnvelope {
        PaneEventEnvelope {
            daemon_instance_id: DaemonInstanceId::parse(DAEMON_ID).unwrap(),
            event_id: EventId::parse(EVENT_ID).unwrap(),
            pane_instance: pane(),
            agent: Some(AgentKind::parse("codex").unwrap()),
            agent_session_id: Some(AgentSessionId::parse("session").unwrap()),
            event,
        }
    }

    fn envelope_for(pane_instance: PaneInstance, event: PaneEvent) -> PaneEventEnvelope {
        PaneEventEnvelope {
            pane_instance,
            ..envelope(event)
        }
    }

    fn begin_event() -> PaneEventEnvelope {
        envelope(PaneEvent::BeginRun {
            started_at: 1,
            prompt: None,
        })
    }

    fn apply_begin(runtime: &mut CanonicalStateRuntime, io: &mut FakeIo) -> ApplyResult {
        runtime
            .apply_event(
                io,
                &mut FakeClock::default(),
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap()
    }

    #[test]
    fn record_roundtrip_is_single_json_and_strict() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        apply_begin(&mut runtime, &mut io);
        let raw = io.writes.last().unwrap();
        let loaded = load_record(RawPaneRecord {
            pane_instance: pane(),
            raw: Some(raw.clone()),
        });
        assert!(matches!(loaded, LoadedPaneRecord::Valid(_)));
        assert!(!raw.contains('\n'));
    }

    #[test]
    fn malformed_record_quarantines_only_that_pane() {
        let other = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 200,
        };
        let mut runtime = CanonicalStateRuntime::hydrate([
            RawPaneRecord {
                pane_instance: pane(),
                raw: Some("not json".to_string()),
            },
            RawPaneRecord {
                pane_instance: other,
                raw: None,
            },
        ]);
        assert!(runtime.quarantined(&pane()).is_some());
        assert_eq!(runtime.quarantine_count(), 1);
        assert_eq!(runtime.quarantine_observed_total(), 1);
        assert!(!runtime.is_fail_stopped());
        let expected = runtime.descriptor(&pane());
        assert!(
            runtime
                .remove_absent_pane(&pane(), expected.as_ref())
                .unwrap()
        );
        assert_eq!(runtime.quarantine_count(), 0);
        assert_eq!(runtime.quarantine_observed_total(), 1);
    }

    #[test]
    fn hydrate_pid_mismatch_is_uninitialized_and_preserves_first_write_expected_value() {
        let mut source = CanonicalStateRuntime::default();
        let mut source_io = FakeIo::default();
        apply_begin(&mut source, &mut source_io);
        let StoredPaneRecord::Active(mut stale_state) = source.record(&pane()).unwrap().clone()
        else {
            unreachable!();
        };
        stale_state.pane_instance.pane_pid = 99;
        let stale_raw = serialize_record(&StoredPaneRecord::Active(stale_state)).unwrap();

        let mut runtime = CanonicalStateRuntime::hydrate([RawPaneRecord {
            pane_instance: pane(),
            raw: Some(stale_raw.clone()),
        }]);
        assert!(runtime.record(&pane()).is_none());
        assert!(runtime.quarantined(&pane()).is_none());
        assert!(runtime.descriptor(&pane()).is_none());
        assert_eq!(runtime.tracked_panes(), vec![pane()]);

        let mut unchanged_io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::ReadBack(Some(stale_raw))]),
            ..FakeIo::default()
        };
        let error = runtime
            .apply_event(
                &mut unchanged_io,
                &mut FakeClock::default(),
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert_eq!(
            error,
            StoreError::PersistFailed("pane state write did not commit".to_string())
        );
        assert!(runtime.record(&pane()).is_none());
        assert!(runtime.quarantined(&pane()).is_none());

        apply_begin(&mut runtime, &mut FakeIo::default());
        assert!(runtime.record(&pane()).is_some());
        assert!(!runtime.uninitialized_raw.contains_key(&pane()));
    }

    #[test]
    fn hydrate_preserves_full_state_version_without_incrementing_it() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        let applied = apply_begin(&mut runtime, &mut io);
        let hydrated = CanonicalStateRuntime::hydrate([RawPaneRecord {
            pane_instance: pane(),
            raw: Some(io.writes.last().unwrap().clone()),
        }]);
        let StoredPaneRecord::Active(state) = hydrated.record(&pane()).unwrap() else {
            unreachable!();
        };
        assert_eq!(Some(state.version()), applied.state_version);
        assert_eq!(hydrated.snapshot_revision(), 0);
    }

    fn restart_from(runtime: &CanonicalStateRuntime) -> CanonicalStateRuntime {
        CanonicalStateRuntime::hydrate([RawPaneRecord {
            pane_instance: pane(),
            raw: runtime
                .record(&pane())
                .map(serialize_record)
                .transpose()
                .unwrap(),
        }])
    }

    #[test]
    fn restart_preserves_hidden_done_reconciles_visible_done_and_allows_next_done() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        let mut clock = FakeClock::default();
        apply_begin(&mut runtime, &mut io);
        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &envelope(PaneEvent::CompleteRun { completed_at: 2 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();

        let mut restarted = restart_from(&runtime);
        let StoredPaneRecord::Active(hidden) = restarted.record(&pane()).unwrap() else {
            panic!("expected active state");
        };
        assert_eq!(crate::pane_state::resolve_badge(hidden), BadgeState::Done);
        assert_eq!(hidden.completed_seq, 1);
        assert_eq!(hidden.acknowledged_seq, 0);

        let acknowledge_visible = envelope(PaneEvent::AcknowledgeView {
            expected_state_id: hidden.state_id.clone(),
            expected_agent_epoch: hidden.agent_epoch,
            through_seq: hidden.completed_seq,
        });
        let ViewBatchProgress::Complete(result) = restarted.apply_view_acknowledgement_batch(
            &mut FakeIo::default(),
            &mut clock,
            &[acknowledge_visible],
            DoneClearOn::Pane,
        ) else {
            panic!("expected initial reconciliation to complete");
        };
        assert_eq!(result.committed, 1);
        let StoredPaneRecord::Active(visible) = restarted.record(&pane()).unwrap() else {
            panic!("expected active state");
        };
        assert_eq!(crate::pane_state::resolve_badge(visible), BadgeState::Idle);

        let mut restarted_again = restart_from(&restarted);
        let StoredPaneRecord::Active(acknowledged) = restarted_again.record(&pane()).unwrap()
        else {
            panic!("expected active state");
        };
        assert_eq!(
            crate::pane_state::resolve_badge(acknowledged),
            BadgeState::Idle
        );
        assert_eq!(acknowledged.completed_seq, acknowledged.acknowledged_seq);
        restarted_again
            .apply_event(
                &mut FakeIo::default(),
                &mut clock,
                &envelope(PaneEvent::BeginRun {
                    started_at: 3,
                    prompt: None,
                }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        restarted_again
            .apply_event(
                &mut FakeIo::default(),
                &mut clock,
                &envelope(PaneEvent::CompleteRun { completed_at: 4 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        let StoredPaneRecord::Active(next_done) = restarted_again.record(&pane()).unwrap() else {
            panic!("expected active state");
        };
        assert_eq!(
            crate::pane_state::resolve_badge(next_done),
            BadgeState::Done
        );
        assert_eq!(next_done.completed_seq, 2);
        assert_eq!(next_done.acknowledged_seq, 1);
    }

    #[test]
    fn restart_hydrated_reset_rejects_delayed_complete_idle_and_progress() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        apply_begin(&mut runtime, &mut io);
        let expected = runtime.descriptor(&pane()).unwrap();
        runtime
            .reset(
                &mut io,
                &mut FakeClock::default(),
                &pane(),
                &expected,
                10,
                ResetTombstoneId::parse(STATE_ID).unwrap(),
            )
            .unwrap();
        let delayed = [
            PaneEvent::CompleteRun { completed_at: 11 },
            PaneEvent::ExplicitStateReported {
                report: ExplicitStateReport {
                    observed_at: 11,
                    lifecycle: Some(ReportedLifecycle::Idle),
                    started_at: None,
                    completed_at: Some(11),
                    prompt: None,
                    tasks: None,
                    subagents: None,
                    attention: false,
                },
            },
            PaneEvent::ProgressUpdated {
                observed_at: 11,
                operations: vec![ProgressOperation::ClearPrompt],
            },
        ];
        for event in delayed {
            let mut restarted = restart_from(&runtime);
            let descriptor = restarted.descriptor(&pane()).unwrap();
            let mut event_io = FakeIo::default();
            let result = restarted
                .apply_event(
                    &mut event_io,
                    &mut FakeClock::default(),
                    &envelope(event),
                    &VisibilitySnapshot::default(),
                    DoneClearOn::Pane,
                )
                .unwrap();
            assert_eq!(result.outcome, ReductionOutcome::Noop);
            assert_eq!(restarted.descriptor(&pane()), Some(descriptor));
            assert!(event_io.writes.is_empty());
        }
    }

    #[test]
    fn restart_requires_a_new_session_start_before_hooks_are_authoritative_again() {
        let mut runtime = CanonicalStateRuntime::default();
        runtime
            .apply_event(
                &mut FakeIo::default(),
                &mut FakeClock::default(),
                &envelope(PaneEvent::AgentSessionStarted {
                    observed_at: 1,
                    source: AgentSessionSource::Startup,
                    resumed_prompt: None,
                }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        assert!(runtime.tracker(&pane()).hook_authoritative);

        let restarted = restart_from(&runtime);
        assert!(!restarted.tracker(&pane()).hook_authoritative);
    }

    #[test]
    fn restart_first_capture_is_a_new_baseline_without_stale_completion() {
        let mut runtime = CanonicalStateRuntime::default();
        apply_begin(&mut runtime, &mut FakeIo::default());
        let mut restarted = restart_from(&runtime);
        let dispatched = restarted.freeze_observation_dispatch([pane()]).remove(0);
        assert_eq!(dispatched.tracker.fingerprint, None);
        assert_eq!(dispatched.tracker.last_change_at, None);
        let StoredPaneRecord::Active(state_before_capture) = restarted.record(&pane()).unwrap()
        else {
            panic!("expected active state");
        };
        let capture = crate::daemon::workers::infer_capture(
            Some(state_before_capture),
            &dispatched.tracker,
            "first tail after restart\n",
            400,
        );
        assert_eq!(capture.inference, CaptureInference::NoChange);
        let fingerprint = capture.observed_fingerprint.unwrap();
        let result = restarted
            .apply_event(
                &mut FakeIo::default(),
                &mut FakeClock::default(),
                &envelope(PaneEvent::ObservationBatch {
                    base: dispatched.base,
                    tracker_generation: dispatched.tracker.generation,
                    observed_at: 400,
                    presence: AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
                    capture: Some(capture),
                }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(result.outcome, ReductionOutcome::CanonicalChanged);
        assert_eq!(restarted.tracker(&pane()).fingerprint, Some(fingerprint));
        let StoredPaneRecord::Active(state) = restarted.record(&pane()).unwrap() else {
            panic!("expected active state");
        };
        assert_eq!(crate::pane_state::resolve_badge(state), BadgeState::Working);
        assert_eq!(state.completed_seq, 0);
    }

    #[test]
    fn restart_capture_changes_do_not_replace_hydrated_idle_state_with_a_new_run() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        runtime
            .apply_event(
                &mut io,
                &mut FakeClock::default(),
                &envelope(PaneEvent::BeginRun {
                    started_at: 10,
                    prompt: Some(PromptState {
                        text: "preserved prompt".to_string(),
                        source: "user".to_string(),
                    }),
                }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        runtime
            .apply_event(
                &mut io,
                &mut FakeClock::default(),
                &envelope(PaneEvent::ProgressUpdated {
                    observed_at: 11,
                    operations: vec![ProgressOperation::ReplaceTasks {
                        progress: TaskProgress { done: 0, total: 1 },
                        items: vec![TaskItemState {
                            id: Some("task-1".to_string()),
                            step: "preserved task".to_string(),
                            status: TaskItemStatus::Pending,
                        }],
                    }],
                }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        runtime
            .apply_event(
                &mut io,
                &mut FakeClock::default(),
                &envelope(PaneEvent::CompleteRun { completed_at: 12 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();

        let mut restarted = restart_from(&runtime);
        for (observed_at, tail) in [(20, "restart baseline\n"), (21, "changed after restart\n")] {
            let dispatched = restarted.freeze_observation_dispatch([pane()]).remove(0);
            let StoredPaneRecord::Active(state) = restarted.record(&pane()).unwrap() else {
                panic!("expected active state");
            };
            let capture = crate::daemon::workers::infer_capture(
                Some(state),
                &dispatched.tracker,
                tail,
                observed_at,
            );
            restarted
                .apply_event(
                    &mut FakeIo::default(),
                    &mut FakeClock::default(),
                    &envelope(PaneEvent::ObservationBatch {
                        base: dispatched.base,
                        tracker_generation: dispatched.tracker.generation,
                        observed_at,
                        presence: AgentPresenceObservation::Present(
                            AgentKind::parse("codex").unwrap(),
                        ),
                        capture: Some(capture),
                    }),
                    &VisibilitySnapshot::default(),
                    DoneClearOn::Pane,
                )
                .unwrap();
        }

        let StoredPaneRecord::Active(state) = restarted.record(&pane()).unwrap() else {
            panic!("expected active state");
        };
        assert!(matches!(state.lifecycle, LifecycleState::Idle));
        assert_eq!(state.run_seq, state.completed_seq);
        assert_eq!(state.started_at, Some(10));
        assert_eq!(state.completed_at, Some(12));
        assert_eq!(
            state.prompt.as_ref().map(|prompt| prompt.text.as_str()),
            Some("preserved prompt")
        );
        assert_eq!(state.tasks.items[0].step, "preserved task");
    }

    #[test]
    fn state_size_is_preflighted_before_store_io() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        apply_begin(&mut runtime, &mut io);
        let StoredPaneRecord::Active(mut state) = runtime.record(&pane()).unwrap().clone() else {
            unreachable!();
        };
        state.tasks.items = (0..MAX_TASK_ITEMS)
            .map(|index| TaskItemState {
                id: Some(index.to_string()),
                step: "x".repeat(BODY_MAX_BYTES),
                status: TaskItemStatus::Pending,
            })
            .collect();
        state.tasks.progress = TaskProgress {
            done: 0,
            total: MAX_TASK_ITEMS as u64,
        };
        assert_eq!(
            serialize_record(&StoredPaneRecord::Active(state)).unwrap_err(),
            StoreError::StateTooLarge
        );
    }

    #[test]
    fn oversized_snapshot_marks_frame_error_without_reverting_canonical_state() {
        let mut runtime = CanonicalStateRuntime::default();
        apply_begin(&mut runtime, &mut FakeIo::default());
        runtime.preflight_projection(1).unwrap();
        assert!(runtime.snapshot_frame_too_large());
        assert!(runtime.record(&pane()).is_some());
        assert!(
            runtime
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("frame limit"))
        );
    }

    #[test]
    fn expected_readback_does_not_commit_memory() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::ReadBack(None)]),
            ..FakeIo::default()
        };
        let error = runtime
            .apply_event(
                &mut io,
                &mut FakeClock::default(),
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::PersistFailed(_)));
        assert!(runtime.record(&pane()).is_none());
        assert_eq!(runtime.snapshot_revision(), 1);
        assert!(runtime.transitions().is_empty());
        assert!(runtime.notification_jobs().is_empty());
    }

    #[test]
    fn blocked_notification_is_derived_in_draft_and_published_after_persist() {
        let mut runtime = CanonicalStateRuntime::default();
        apply_begin(&mut runtime, &mut FakeIo::default());
        let record_before = runtime.record(&pane()).cloned();
        let tracker_before = runtime.tracker(&pane());
        let transitions_before = runtime.transitions().clone();
        let notifications_before = runtime.notification_jobs().clone();
        let triage_before = runtime
            .triage_entries()
            .map(|(pane, badge)| (pane.clone(), badge))
            .collect::<Vec<_>>();
        let flashing_before = runtime.flashing_panes().cloned().collect::<Vec<_>>();
        let diagnostics_before = runtime.diagnostics().len();
        let revision_before = runtime.snapshot_revision();
        let expected_raw = runtime
            .record(&pane())
            .map(serialize_record)
            .transpose()
            .unwrap();
        let mut failed_io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::ReadBack(expected_raw)]),
            ..FakeIo::default()
        };
        let waiting = envelope(PaneEvent::WaitRequested {
            observed_at: 2,
            reason: WaitReason::PermissionPrompt,
        });
        let error = runtime
            .apply_event(
                &mut failed_io,
                &mut FakeClock::default(),
                &waiting,
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::PersistFailed(_)));
        assert_eq!(runtime.record(&pane()), record_before.as_ref());
        assert_eq!(runtime.tracker(&pane()), tracker_before);
        assert_eq!(runtime.transitions(), &transitions_before);
        assert_eq!(runtime.notification_jobs(), &notifications_before);
        assert_eq!(
            runtime
                .triage_entries()
                .map(|(pane, badge)| (pane.clone(), badge))
                .collect::<Vec<_>>(),
            triage_before
        );
        assert_eq!(
            runtime.flashing_panes().cloned().collect::<Vec<_>>(),
            flashing_before
        );
        assert_eq!(runtime.snapshot_revision(), revision_before + 1);
        assert_eq!(runtime.diagnostics().len(), diagnostics_before + 1);
        assert_eq!(runtime.diagnostics().back().unwrap().pane_instance, pane());
        assert_eq!(
            runtime.diagnostics().back().unwrap().message,
            "pane state write did not commit"
        );

        runtime
            .apply_event(
                &mut FakeIo::default(),
                &mut FakeClock::default(),
                &waiting,
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(runtime.transitions().len(), transitions_before.len() + 1);
        assert_eq!(runtime.notification_jobs().len(), 1);
    }

    #[test]
    fn duplicate_view_acknowledgement_is_idempotent_in_store() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        let mut clock = FakeClock::default();
        apply_begin(&mut runtime, &mut io);
        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &envelope(PaneEvent::CompleteRun { completed_at: 2 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        let StoredPaneRecord::Active(state) = runtime.record(&pane()).unwrap() else {
            panic!("expected active pane state");
        };
        let acknowledgement = envelope(PaneEvent::AcknowledgeView {
            expected_state_id: state.state_id.clone(),
            expected_agent_epoch: state.agent_epoch,
            through_seq: state.completed_seq,
        });

        let ViewBatchProgress::Complete(first) = runtime.apply_view_acknowledgement_batch(
            &mut io,
            &mut clock,
            std::slice::from_ref(&acknowledgement),
            DoneClearOn::Pane,
        ) else {
            panic!("expected completed first view batch");
        };
        assert_eq!(first.committed, 1);
        assert!(first.failed.is_empty());
        let revision_after_first = runtime.snapshot_revision();
        let record_after_first = runtime.record(&pane()).cloned();
        let transitions_after_first = runtime.transitions().clone();

        let ViewBatchProgress::Complete(duplicate) = runtime.apply_view_acknowledgement_batch(
            &mut io,
            &mut clock,
            &[acknowledgement],
            DoneClearOn::Pane,
        ) else {
            panic!("expected completed duplicate view batch");
        };
        assert_eq!(duplicate.committed, 0);
        assert!(duplicate.failed.is_empty());
        assert_eq!(duplicate.snapshot_revision, revision_after_first);
        assert_eq!(runtime.record(&pane()), record_after_first.as_ref());
        assert_eq!(runtime.transitions(), &transitions_after_first);
    }

    #[test]
    fn same_agent_new_session_keeps_completed_transition_identity_and_time() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        let mut clock = FakeClock::default();
        let visibility = VisibilitySnapshot::default();
        apply_begin(&mut runtime, &mut io);
        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &envelope(PaneEvent::CompleteRun { completed_at: 20 }),
                &visibility,
                DoneClearOn::Pane,
            )
            .unwrap();

        let completed = runtime.transitions().back().unwrap().clone();
        assert_eq!(
            completed.agent.as_ref().map(AgentKind::as_str),
            Some("codex")
        );
        assert_eq!(completed.at_epoch, 20);
        assert_eq!(
            completed.to,
            Some(crate::daemon::session_badge::BadgeState::Done)
        );

        let mut restarted = envelope(PaneEvent::AgentSessionStarted {
            observed_at: 30,
            source: AgentSessionSource::Startup,
            resumed_prompt: None,
        });
        restarted.agent_session_id = Some(AgentSessionId::parse("next-session").unwrap());
        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &restarted,
                &visibility,
                DoneClearOn::Pane,
            )
            .unwrap();

        let StoredPaneRecord::Active(current) = runtime.record(&pane()).unwrap() else {
            panic!("expected active pane state");
        };
        assert_eq!(
            current
                .agent_session_id
                .as_ref()
                .map(AgentSessionId::as_str),
            Some("next-session")
        );
        assert!(runtime.transitions().contains(&completed));
        let discarded = runtime.transitions().back().unwrap();
        assert_eq!(
            discarded.agent.as_ref().map(AgentKind::as_str),
            Some("codex")
        );
        assert_eq!(discarded.at_epoch, 20);
        assert_eq!(
            discarded.from,
            Some(crate::daemon::session_badge::BadgeState::Done)
        );
        assert_eq!(
            discarded.to,
            Some(crate::daemon::session_badge::BadgeState::Idle)
        );
    }

    #[test]
    fn triage_and_flash_leave_after_two_calm_poll_projections() {
        let mut runtime = CanonicalStateRuntime::default();
        apply_begin(&mut runtime, &mut FakeIo::default());
        runtime
            .apply_event(
                &mut FakeIo::default(),
                &mut FakeClock::default(),
                &envelope(PaneEvent::WaitRequested {
                    observed_at: 2,
                    reason: WaitReason::PermissionPrompt,
                }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        runtime
            .apply_event(
                &mut FakeIo::default(),
                &mut FakeClock::default(),
                &envelope(PaneEvent::ActivityObserved { observed_at: 3 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(runtime.triage_panes().count(), 1);
        assert_eq!(
            runtime
                .triage_entries()
                .map(|(pane, badge)| (pane.clone(), badge))
                .collect::<Vec<_>>(),
            vec![(pane(), crate::daemon::session_badge::BadgeState::Blocked)]
        );
        assert_eq!(runtime.flashing_panes().count(), 1);
        let revision = runtime.snapshot_revision();

        assert!(!runtime.advance_poll_projection().unwrap());
        assert_eq!(runtime.snapshot_revision(), revision);
        assert_eq!(runtime.triage_panes().count(), 1);
        assert!(runtime.advance_poll_projection().unwrap());
        assert_eq!(runtime.snapshot_revision(), revision + 1);
        assert_eq!(runtime.triage_panes().count(), 0);
        assert_eq!(runtime.flashing_panes().count(), 0);
    }

    #[test]
    fn notifications_are_emitted_on_blocked_transition_even_when_pane_is_focused() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        apply_begin(&mut runtime, &mut io);
        let visibility = VisibilitySnapshot {
            pane_visible_to_eligible_client: true,
            ..VisibilitySnapshot::default()
        };
        let mut clock = FakeClock::default();

        let waiting = envelope(PaneEvent::WaitRequested {
            observed_at: 2,
            reason: WaitReason::PermissionPrompt,
        });
        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &waiting,
                &visibility,
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(runtime.notification_jobs().len(), 1);

        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &waiting,
                &visibility,
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(runtime.notification_jobs().len(), 1);

        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &envelope(PaneEvent::ActivityObserved { observed_at: 3 }),
                &visibility,
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(runtime.notification_jobs().len(), 1);

        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &envelope(PaneEvent::WaitRequested {
                    observed_at: 4,
                    reason: WaitReason::Other("input".to_string()),
                }),
                &visibility,
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(runtime.notification_jobs().len(), 2);

        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &envelope(PaneEvent::CompleteRun { completed_at: 5 }),
                &visibility,
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(runtime.notification_jobs().len(), 2);
    }

    #[test]
    fn notification_queue_overflow_is_bounded_counted_and_diagnosed() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        apply_begin(&mut runtime, &mut io);
        let visibility = VisibilitySnapshot {
            pane_visible_to_eligible_client: true,
            ..VisibilitySnapshot::default()
        };
        let mut clock = FakeClock::default();

        for index in 0..66_i64 {
            runtime
                .apply_event(
                    &mut io,
                    &mut clock,
                    &envelope(PaneEvent::WaitRequested {
                        observed_at: 10 + index * 2,
                        reason: WaitReason::Other("input".to_string()),
                    }),
                    &visibility,
                    DoneClearOn::Pane,
                )
                .unwrap();
            runtime
                .apply_event(
                    &mut io,
                    &mut clock,
                    &envelope(PaneEvent::ActivityObserved {
                        observed_at: 11 + index * 2,
                    }),
                    &visibility,
                    DoneClearOn::Pane,
                )
                .unwrap();
        }

        assert_eq!(runtime.notification_jobs().len(), 64);
        assert_eq!(runtime.notification_queue_drops(), 2);
        assert!(runtime.diagnostics().iter().any(|diagnostic| {
            diagnostic.message == "notification_queue_overflow: dropped_oldest"
        }));
    }

    #[test]
    fn window_view_batch_commits_successes_keeps_failures_and_publishes_once() {
        let first = pane();
        let second = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 200,
        };
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        let mut clock = FakeClock::default();
        for pane_instance in [&first, &second] {
            runtime
                .apply_event(
                    &mut io,
                    &mut clock,
                    &envelope_for(
                        pane_instance.clone(),
                        PaneEvent::BeginRun {
                            started_at: 1,
                            prompt: None,
                        },
                    ),
                    &VisibilitySnapshot::default(),
                    DoneClearOn::Window,
                )
                .unwrap();
            runtime
                .apply_event(
                    &mut io,
                    &mut clock,
                    &envelope_for(
                        pane_instance.clone(),
                        PaneEvent::CompleteRun { completed_at: 2 },
                    ),
                    &VisibilitySnapshot::default(),
                    DoneClearOn::Window,
                )
                .unwrap();
        }
        let revision_before = runtime.snapshot_revision();
        let expected_second = serialize_record(runtime.record(&second).unwrap()).unwrap();
        let acknowledgements = [&first, &second]
            .into_iter()
            .map(|pane_instance| {
                let StoredPaneRecord::Active(state) = runtime.record(pane_instance).unwrap() else {
                    unreachable!();
                };
                envelope_for(
                    pane_instance.clone(),
                    PaneEvent::AcknowledgeView {
                        expected_state_id: state.state_id.clone(),
                        expected_agent_epoch: state.agent_epoch,
                        through_seq: state.completed_seq,
                    },
                )
            })
            .collect::<Vec<_>>();

        let progress = runtime.apply_view_acknowledgement_batch(
            &mut ExpectedPaneIo {
                fail_pane_id: second.pane_id.clone(),
                expected: expected_second,
            },
            &mut clock,
            &acknowledgements,
            DoneClearOn::Window,
        );
        let ViewBatchProgress::Complete(result) = progress else {
            panic!("expected completed partial batch");
        };

        assert_eq!(result.committed, 1);
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].pane_instance, second);
        assert_eq!(result.snapshot_revision, revision_before + 1);
        let StoredPaneRecord::Active(first_state) = runtime.record(&first).unwrap() else {
            unreachable!();
        };
        assert_eq!(first_state.acknowledged_seq, first_state.completed_seq);
        let StoredPaneRecord::Active(second_state) =
            runtime.record(&result.failed[0].pane_instance).unwrap()
        else {
            unreachable!();
        };
        assert_eq!(second_state.acknowledged_seq, 0);
        assert!(runtime.diagnostics().iter().any(|diagnostic| {
            diagnostic.pane_instance == result.failed[0].pane_instance
                && diagnostic.message.contains("view acknowledgment failed")
        }));
    }

    #[test]
    fn window_view_batch_keeps_live_snapshot_hidden_while_outcome_is_pending() {
        struct PendingBatchIo {
            candidate: Option<String>,
        }

        impl PaneStateStoreIo for PendingBatchIo {
            fn write_candidate(&mut self, _pane: &PaneInstance, candidate: &str) -> WriteAttempt {
                self.candidate = Some(candidate.to_string());
                WriteAttempt::OutcomeUnknown("timeout".to_string())
            }

            fn read_independent(&mut self, _pane: &PaneInstance) -> IndependentRead {
                IndependentRead::Value(self.candidate.clone())
            }
        }

        let mut runtime = CanonicalStateRuntime::default();
        let mut setup_io = FakeIo::default();
        let mut clock = FakeClock::default();
        runtime
            .apply_event(
                &mut setup_io,
                &mut clock,
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Window,
            )
            .unwrap();
        runtime
            .apply_event(
                &mut setup_io,
                &mut clock,
                &envelope(PaneEvent::CompleteRun { completed_at: 2 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Window,
            )
            .unwrap();
        let StoredPaneRecord::Active(state) = runtime.record(&pane()).unwrap() else {
            unreachable!();
        };
        let acknowledgement = envelope(PaneEvent::AcknowledgeView {
            expected_state_id: state.state_id.clone(),
            expected_agent_epoch: state.agent_epoch,
            through_seq: state.completed_seq,
        });
        let revision_before = runtime.snapshot_revision();
        let mut pending_io = PendingBatchIo { candidate: None };

        let ViewBatchProgress::Pending(continuation) = runtime.apply_view_acknowledgement_batch(
            &mut pending_io,
            &mut clock,
            &[acknowledgement],
            DoneClearOn::Window,
        ) else {
            panic!("expected pending view batch");
        };
        assert!(runtime.sequenced_mutations_paused());
        assert_eq!(
            runtime.query_pane_while_paused(
                &PaneInstance {
                    pane_id: "%99".to_string(),
                    pane_pid: 999,
                },
                false,
            ),
            QueryPaneAvailability::NotReady
        );
        assert_eq!(
            runtime
                .apply_event(
                    &mut setup_io,
                    &mut clock,
                    &begin_event(),
                    &VisibilitySnapshot::default(),
                    DoneClearOn::Window,
                )
                .unwrap_err(),
            StoreError::PersistPending
        );
        let StoredPaneRecord::Active(state) = runtime.record(&pane()).unwrap() else {
            unreachable!();
        };
        assert_eq!(state.acknowledged_seq, 0);
        assert_eq!(runtime.snapshot_revision(), revision_before);

        let ViewBatchProgress::Complete(result) =
            runtime.resume_view_acknowledgement_batch(&mut pending_io, &mut clock, continuation)
        else {
            panic!("expected resolved view batch");
        };
        assert_eq!(result.committed, 1);
        assert!(result.failed.is_empty());
        assert_eq!(runtime.snapshot_revision(), revision_before + 1);
        let StoredPaneRecord::Active(state) = runtime.record(&pane()).unwrap() else {
            unreachable!();
        };
        assert_eq!(state.acknowledged_seq, state.completed_seq);
        assert!(!runtime.sequenced_mutations_paused());
    }

    #[test]
    fn multi_pane_view_batch_resolves_late_pending_and_publishes_once() {
        struct LatePendingIo {
            pending_pane: String,
            candidate: Option<String>,
        }

        impl PaneStateStoreIo for LatePendingIo {
            fn write_candidate(&mut self, pane: &PaneInstance, candidate: &str) -> WriteAttempt {
                if pane.pane_id == self.pending_pane {
                    self.candidate = Some(candidate.to_string());
                    WriteAttempt::OutcomeUnknown("timeout".to_string())
                } else {
                    WriteAttempt::ReadBack(Some(candidate.to_string()))
                }
            }

            fn read_independent(&mut self, _pane: &PaneInstance) -> IndependentRead {
                IndependentRead::Value(self.candidate.clone())
            }
        }

        let panes = [
            pane(),
            PaneInstance {
                pane_id: "%2".to_string(),
                pane_pid: 200,
            },
        ];
        let mut runtime = CanonicalStateRuntime::default();
        let mut setup_io = FakeIo::default();
        let mut clock = FakeClock::default();
        for pane_instance in &panes {
            for event in [
                PaneEvent::BeginRun {
                    started_at: 1,
                    prompt: None,
                },
                PaneEvent::CompleteRun { completed_at: 2 },
            ] {
                runtime
                    .apply_event(
                        &mut setup_io,
                        &mut clock,
                        &envelope_for(pane_instance.clone(), event),
                        &VisibilitySnapshot::default(),
                        DoneClearOn::Window,
                    )
                    .unwrap();
            }
        }
        let acknowledgements = panes
            .iter()
            .map(|pane_instance| {
                let StoredPaneRecord::Active(state) = runtime.record(pane_instance).unwrap() else {
                    unreachable!();
                };
                envelope_for(
                    pane_instance.clone(),
                    PaneEvent::AcknowledgeView {
                        expected_state_id: state.state_id.clone(),
                        expected_agent_epoch: state.agent_epoch,
                        through_seq: state.completed_seq,
                    },
                )
            })
            .collect::<Vec<_>>();
        let revision_before = runtime.snapshot_revision();
        let mut io = LatePendingIo {
            pending_pane: panes[1].pane_id.clone(),
            candidate: None,
        };
        let ViewBatchProgress::Pending(continuation) = runtime.apply_view_acknowledgement_batch(
            &mut io,
            &mut clock,
            &acknowledgements,
            DoneClearOn::Window,
        ) else {
            panic!("expected pending second pane");
        };
        assert_eq!(runtime.snapshot_revision(), revision_before);
        for pane_instance in &panes {
            let StoredPaneRecord::Active(state) = runtime.record(pane_instance).unwrap() else {
                unreachable!();
            };
            assert_eq!(state.acknowledged_seq, 0);
        }
        let ViewBatchProgress::Complete(result) =
            runtime.resume_view_acknowledgement_batch(&mut io, &mut clock, continuation)
        else {
            panic!("expected completed multi-pane batch");
        };
        assert_eq!(result.committed, 2);
        assert_eq!(runtime.snapshot_revision(), revision_before + 1);
        for pane_instance in &panes {
            let StoredPaneRecord::Active(state) = runtime.record(pane_instance).unwrap() else {
                unreachable!();
            };
            assert_eq!(state.acknowledged_seq, state.completed_seq);
        }
    }

    #[test]
    fn normal_pending_transaction_blocks_view_batch_start() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut pending_io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::OutcomeUnknown("timeout".to_string())]),
            ..FakeIo::default()
        };
        let mut clock = FakeClock::default();
        assert_eq!(
            runtime
                .apply_event(
                    &mut pending_io,
                    &mut clock,
                    &begin_event(),
                    &VisibilitySnapshot::default(),
                    DoneClearOn::Window,
                )
                .unwrap_err(),
            StoreError::PersistPending
        );
        assert!(matches!(
            runtime.apply_view_acknowledgement_batch(
                &mut pending_io,
                &mut clock,
                &[],
                DoneClearOn::Window,
            ),
            ViewBatchProgress::Blocked(StoreError::PersistPending)
        ));
    }

    #[test]
    fn observation_dispatched_before_explicit_event_cannot_change_state_or_tracker() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        let mut clock = FakeClock::default();
        apply_begin(&mut runtime, &mut io);
        let dispatched = runtime.freeze_observation_dispatch([pane()]).remove(0);

        let duplicate_begin = begin_event();
        let result = runtime
            .apply_event(
                &mut io,
                &mut clock,
                &duplicate_begin,
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(result.outcome, ReductionOutcome::TrackerOnly);
        let tracker_after_explicit = runtime.tracker(&pane());
        assert!(tracker_after_explicit.generation > dispatched.tracker.generation);
        let record_after_explicit = runtime.record(&pane()).cloned();

        let stale_observation = envelope(PaneEvent::ObservationBatch {
            base: dispatched.base,
            tracker_generation: dispatched.tracker.generation,
            observed_at: 2,
            presence: AgentPresenceObservation::Absent,
            capture: Some(CaptureObservation {
                inference: CaptureInference::ActivityObserved,
                observed_fingerprint: Some([7; 32]),
            }),
        });
        let stale_result = runtime
            .apply_event(
                &mut io,
                &mut clock,
                &stale_observation,
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(stale_result.outcome, ReductionOutcome::Noop);
        assert_eq!(runtime.record(&pane()), record_after_explicit.as_ref());
        assert_eq!(runtime.tracker(&pane()), tracker_after_explicit);
    }

    #[test]
    fn window_view_batch_counter_overflow_is_fatal() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        let mut clock = FakeClock::default();
        apply_begin(&mut runtime, &mut io);
        runtime
            .apply_event(
                &mut io,
                &mut clock,
                &envelope(PaneEvent::CompleteRun { completed_at: 2 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Window,
            )
            .unwrap();
        let StoredPaneRecord::Active(state) = runtime.record(&pane()).unwrap() else {
            unreachable!();
        };
        let acknowledgement = envelope(PaneEvent::AcknowledgeView {
            expected_state_id: state.state_id.clone(),
            expected_agent_epoch: state.agent_epoch,
            through_seq: state.completed_seq,
        });
        runtime.snapshot_revision = u64::MAX;

        assert!(matches!(
            runtime.apply_view_acknowledgement_batch(
                &mut io,
                &mut clock,
                &[acknowledgement],
                DoneClearOn::Window,
            ),
            ViewBatchProgress::Fatal(StoreError::CounterOverflow("snapshot revision"))
        ));
        assert!(runtime.is_fail_stopped());
    }

    #[test]
    fn no_op_view_batch_bumps_revision_when_preflight_adds_diagnostic() {
        let mut runtime = CanonicalStateRuntime {
            diagnostics: (0..MAX_DIAGNOSTICS)
                .map(|index| PaneStateDiagnostic {
                    pane_instance: PaneInstance {
                        pane_id: format!("%{}", index + 1),
                        pane_pid: index as u32 + 1,
                    },
                    message: "x".repeat(70_000),
                })
                .collect(),
            ..CanonicalStateRuntime::default()
        };
        let ViewBatchProgress::Complete(result) = runtime.apply_view_acknowledgement_batch(
            &mut FakeIo::default(),
            &mut FakeClock::default(),
            &[],
            DoneClearOn::Pane,
        ) else {
            panic!("expected completed empty batch");
        };
        assert_eq!(result.snapshot_revision, 1);
        assert!(runtime.snapshot_frame_too_large());
        assert!(
            runtime
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message == "resolved snapshot exceeds frame limit")
        );
    }

    #[test]
    fn diagnostic_overflow_does_not_partially_mutate_projection() {
        let mut runtime = CanonicalStateRuntime {
            snapshot_revision: u64::MAX,
            ..CanonicalStateRuntime::default()
        };

        let error = runtime
            .add_diagnostic(pane(), "must not remain")
            .unwrap_err();

        assert_eq!(error, StoreError::CounterOverflow("snapshot revision"));
        assert_eq!(runtime.snapshot_revision(), u64::MAX);
        assert!(runtime.diagnostics().is_empty());
    }

    #[test]
    fn projection_validation_failure_does_not_advance_revision() {
        let mut runtime = CanonicalStateRuntime {
            diagnostics: std::iter::repeat_with(|| PaneStateDiagnostic {
                pane_instance: pane(),
                message: "overflow".to_string(),
            })
            .take(MAX_DIAGNOSTICS + 1)
            .collect(),
            ..CanonicalStateRuntime::default()
        };

        let error = runtime.mark_projection_changed().unwrap_err();

        assert!(error.to_string().contains("projection collection exceeds"));
        assert_eq!(runtime.snapshot_revision(), 0);
        assert_eq!(runtime.diagnostics().len(), MAX_DIAGNOSTICS + 1);
    }

    #[test]
    fn finish_projection_overflow_does_not_partially_apply_diagnostics() {
        let mut runtime = CanonicalStateRuntime {
            snapshot_revision: u64::MAX,
            ..CanonicalStateRuntime::default()
        };

        let error = runtime
            .finish_sequenced_projection(
                Some(&pane()),
                ["must not remain".to_string()],
                false,
                u64::MAX,
            )
            .unwrap_err();

        assert_eq!(error, StoreError::CounterOverflow("snapshot revision"));
        assert_eq!(runtime.snapshot_revision(), u64::MAX);
        assert!(runtime.diagnostics().is_empty());
    }

    #[test]
    fn absent_pane_removal_overflow_does_not_partially_remove_state() {
        let mut runtime = CanonicalStateRuntime::default();
        apply_begin(&mut runtime, &mut FakeIo::default());
        let expected = runtime.descriptor(&pane());
        let record_before = runtime.record(&pane()).cloned();
        let tracker_before = runtime.tracker(&pane());
        runtime.snapshot_revision = u64::MAX;

        let error = runtime
            .remove_absent_pane(&pane(), expected.as_ref())
            .unwrap_err();

        assert_eq!(error, StoreError::CounterOverflow("snapshot revision"));
        assert_eq!(runtime.snapshot_revision(), u64::MAX);
        assert_eq!(runtime.record(&pane()), record_before.as_ref());
        assert_eq!(runtime.tracker(&pane()), tracker_before);
    }

    #[test]
    fn third_value_quarantines_external_writer_result() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::ReadBack(Some("third".to_string()))]),
            ..FakeIo::default()
        };
        let error = runtime
            .apply_event(
                &mut io,
                &mut FakeClock::default(),
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::ExternalWriter(_)));
        assert!(runtime.quarantined(&pane()).is_some());
    }

    #[test]
    fn ambiguous_candidate_confirmation_commits_original_transaction() {
        let mut runtime = CanonicalStateRuntime::default();
        // The fake needs the candidate generated by write_candidate, so resolve it on read.
        struct CandidateIo(FakeIo);
        impl PaneStateStoreIo for CandidateIo {
            fn write_candidate(&mut self, pane: &PaneInstance, candidate: &str) -> WriteAttempt {
                self.0.writes.push(candidate.to_string());
                let _ = pane;
                WriteAttempt::OutcomeUnknown("timeout".to_string())
            }
            fn read_independent(&mut self, _pane: &PaneInstance) -> IndependentRead {
                IndependentRead::Value(Some(self.0.writes[0].clone()))
            }
        }
        let mut candidate_io = CandidateIo(FakeIo::default());
        let error = runtime
            .apply_event(
                &mut candidate_io,
                &mut FakeClock::default(),
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert_eq!(error, StoreError::PersistPending);
        assert!(runtime.sequenced_mutations_paused());
        assert!(runtime.record(&pane()).is_none());
        let result = match runtime
            .resolve_pending(&mut candidate_io, &FakeClock::default())
            .unwrap()
        {
            PendingResolution::Applied(result) => result,
            other => panic!("unexpected resolution: {other:?}"),
        };
        assert_eq!(result.outcome, ReductionOutcome::CanonicalChanged);
        assert!(runtime.record(&pane()).is_some());
    }

    #[test]
    fn ambiguous_pane_disappearance_is_fourth_terminal_outcome() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::OutcomeUnknown("timeout".to_string())]),
            reads: VecDeque::from([IndependentRead::PaneMissing]),
            ..FakeIo::default()
        };
        let error = runtime
            .apply_event(
                &mut io,
                &mut FakeClock::default(),
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert_eq!(error, StoreError::PersistPending);
        let error = runtime
            .resolve_pending(&mut io, &FakeClock::default())
            .unwrap_err();
        assert_eq!(error, StoreError::InvalidPaneInstance);
        assert!(!runtime.is_fail_stopped());
    }

    #[test]
    fn ambiguous_read_deadline_fail_stops_runtime() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::OutcomeUnknown("timeout".to_string())]),
            reads: VecDeque::from([IndependentRead::Value(None)]),
            ..FakeIo::default()
        };
        let mut clock = FakeClock::default();
        let error = runtime
            .apply_event(
                &mut io,
                &mut clock,
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert_eq!(error, StoreError::PersistPending);
        clock.0 = STORE_RECOVERY_DEADLINE;
        let error = runtime.resolve_pending(&mut io, &clock).unwrap_err();
        assert!(matches!(error, StoreError::FailStop(_)));
        assert!(runtime.is_fail_stopped());
        assert!(error.requires_daemon_exit());
        assert_eq!(io.reads.len(), 1, "expired recovery must not start a read");
    }

    #[test]
    fn independent_read_that_exhausts_deadline_fail_stops_immediately() {
        use std::sync::{Arc, Mutex};

        struct SharedClock(Arc<Mutex<Duration>>);
        impl RecoveryClock for SharedClock {
            fn elapsed(&self) -> Duration {
                *self.0.lock().unwrap()
            }
        }
        struct DeadlineReadIo(Arc<Mutex<Duration>>);
        impl PaneStateStoreIo for DeadlineReadIo {
            fn write_candidate(&mut self, _pane: &PaneInstance, _candidate: &str) -> WriteAttempt {
                unreachable!()
            }

            fn read_independent(&mut self, _pane: &PaneInstance) -> IndependentRead {
                *self.0.lock().unwrap() = STORE_RECOVERY_DEADLINE;
                IndependentRead::Unavailable("timeout".to_string())
            }
        }

        let mut runtime = CanonicalStateRuntime::default();
        let mut initial_io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::OutcomeUnknown("timeout".to_string())]),
            ..FakeIo::default()
        };
        runtime
            .apply_event(
                &mut initial_io,
                &mut FakeClock::default(),
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        let elapsed = Arc::new(Mutex::new(Duration::from_millis(4_999)));
        let clock = SharedClock(elapsed.clone());
        let error = runtime
            .resolve_pending(&mut DeadlineReadIo(elapsed), &clock)
            .unwrap_err();

        assert!(matches!(error, StoreError::FailStop(_)));
        assert!(runtime.is_fail_stopped());
    }

    #[test]
    fn pending_outcome_serves_committed_queries_and_rejects_cache_miss() {
        let mut runtime = CanonicalStateRuntime::default();
        apply_begin(&mut runtime, &mut FakeIo::default());
        let committed_revision = runtime.snapshot_revision();
        let committed_descriptor = runtime.record(&pane()).unwrap().descriptor();
        let tracker = runtime.tracker(&pane());
        let mut io = FakeIo {
            write_results: VecDeque::from([WriteAttempt::OutcomeUnknown("timeout".to_string())]),
            ..FakeIo::default()
        };
        let error = runtime
            .apply_event(
                &mut io,
                &mut FakeClock::default(),
                &envelope(PaneEvent::CompleteRun { completed_at: 2 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert_eq!(error, StoreError::PersistPending);
        assert_eq!(
            runtime.query_committed_snapshot_revision(),
            committed_revision
        );
        assert_eq!(
            runtime.query_pane_while_paused(&pane(), true),
            QueryPaneAvailability::Ready(Some(committed_descriptor))
        );
        assert_eq!(runtime.tracker(&pane()), tracker);
        let missing = PaneInstance {
            pane_id: "%9".to_string(),
            pane_pid: 900,
        };
        assert_eq!(
            runtime.query_pane_while_paused(&missing, false),
            QueryPaneAvailability::NotReady
        );
        assert_eq!(
            runtime.query_pane_while_paused(&missing, true),
            QueryPaneAvailability::Ready(None)
        );
        let second = runtime
            .apply_event(
                &mut FakeIo::default(),
                &mut FakeClock::default(),
                &envelope(PaneEvent::ActivityObserved { observed_at: 3 }),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();
        assert_eq!(second, StoreError::PersistPending);
    }

    #[test]
    fn reset_requires_matching_descriptor_and_commits_tombstone() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        apply_begin(&mut runtime, &mut io);
        let expected = runtime.record(&pane()).unwrap().descriptor();
        let result = runtime
            .reset(
                &mut FakeIo::default(),
                &mut FakeClock::default(),
                &pane(),
                &expected,
                10,
                ResetTombstoneId::parse(STATE_ID).unwrap(),
            )
            .unwrap();
        assert!(matches!(result, StoredStateDescriptor::Reset { .. }));
    }

    #[test]
    fn reset_rejects_stale_descriptor_before_store_io() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = FakeIo::default();
        apply_begin(&mut runtime, &mut io);
        let stale = StoredStateDescriptor::Quarantined {
            quarantine_id: "stale".to_string(),
        };
        let mut reset_io = FakeIo::default();
        let error = runtime
            .reset(
                &mut reset_io,
                &mut FakeClock::default(),
                &pane(),
                &stale,
                10,
                ResetTombstoneId::parse(STATE_ID).unwrap(),
            )
            .unwrap_err();
        assert_eq!(error, StoreError::StaleStateIdentity);
        assert!(reset_io.writes.is_empty());
    }

    #[test]
    fn tmux_writer_pid_guard_rejects_reused_pane_as_invalid_instance() {
        #[derive(Default)]
        struct ReusedPaneRunner {
            calls: std::cell::RefCell<Vec<Vec<String>>>,
        }

        impl TmuxRunner for ReusedPaneRunner {
            fn run(&self, args: &[&str]) -> anyhow::Result<String> {
                self.calls
                    .borrow_mut()
                    .push(args.iter().map(|arg| (*arg).to_string()).collect());
                Ok(PANE_MISMATCH_SENTINEL.to_string())
            }
        }

        let runner = ReusedPaneRunner::default();
        let mut store_io = TmuxPaneStateStoreIo::new(&runner, 7_001, 1_234_567);
        let mut runtime = CanonicalStateRuntime::default();
        let error = runtime
            .apply_event(
                &mut store_io,
                &mut FakeClock::default(),
                &begin_event(),
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap_err();

        assert_eq!(error, StoreError::InvalidPaneInstance);
        assert!(runtime.record(&pane()).is_none());
        assert_eq!(runtime.snapshot_revision(), 0);
        assert!(runtime.transitions().is_empty());
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "if-shell");
        assert_eq!(
            calls[0][2],
            "#{&&:#{==:#{pid},7001},#{==:#{start_time},1234567}}"
        );
        assert!(calls[0][3].contains("if-shell -F -t %1"));
        assert!(calls[0][3].contains("#{==:#{pane_pid},100}"));
        assert!(calls[0][3].contains("set-option -p -t %1 @vde_pane_state"));
        assert!(calls[0][3].contains(PANE_MISMATCH_SENTINEL));
    }

    #[test]
    fn independent_read_uses_typed_if_shell_pane_missing_sentinel() {
        #[derive(Default)]
        struct MissingPaneRunner {
            calls: std::cell::RefCell<Vec<Vec<String>>>,
        }

        impl TmuxRunner for MissingPaneRunner {
            fn run(&self, args: &[&str]) -> anyhow::Result<String> {
                self.calls
                    .borrow_mut()
                    .push(args.iter().map(|arg| (*arg).to_string()).collect());
                Ok(PANE_MISMATCH_SENTINEL.to_string())
            }
        }

        let runner = MissingPaneRunner::default();
        let mut store_io = TmuxPaneStateStoreIo::new(&runner, 7_001, 1_234_567);
        assert_eq!(
            store_io.read_independent(&pane()),
            IndependentRead::PaneMissing
        );

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "if-shell");
        assert_eq!(
            calls[0][2],
            "#{&&:#{==:#{pid},7001},#{==:#{start_time},1234567}}"
        );
        assert!(calls[0][3].contains("if-shell -F -t %1"));
        assert!(calls[0][3].contains("#{==:#{pane_pid},100}"));
        assert!(calls[0][3].contains(PANE_MISMATCH_SENTINEL));
    }

    #[test]
    fn independent_read_does_not_parse_tmux_error_text_as_pane_missing() {
        struct ErrorRunner;

        impl TmuxRunner for ErrorRunner {
            fn run(&self, _args: &[&str]) -> anyhow::Result<String> {
                anyhow::bail!("can't find pane: %1")
            }
        }

        let mut store_io = TmuxPaneStateStoreIo::new(&ErrorRunner, 7_001, 1_234_567);
        assert_eq!(
            store_io.read_independent(&pane()),
            IndependentRead::Unavailable("can't find pane: %1".to_string())
        );
    }

    #[test]
    fn tmux_quote_preserves_apostrophes_as_one_argument() {
        assert_eq!(quote_tmux_command_argument("a'b"), "'a'\\''b'");
    }
}

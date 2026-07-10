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
        matches!(self, Self::FailStop(_))
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
    let loaded = if raw.len() > MAX_STORED_RECORD_BYTES {
        Err("serialized pane state exceeds 256 KiB".to_string())
    } else {
        serde_json::from_str::<StoredPaneRecord>(&raw)
            .map_err(|error| error.to_string())
            .and_then(|record| {
                record.validate().map_err(|error| error.to_string())?;
                if record.pane_instance() != &entry.pane_instance {
                    return Err("stored pane instance does not match current pane".to_string());
                }
                Ok(record)
            })
    };
    match loaded {
        Ok(record) => LoadedPaneRecord::Valid(record),
        Err(message) => {
            LoadedPaneRecord::Quarantined(quarantine(entry.pane_instance, raw, message))
        }
    }
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
        let server_guard = format!(
            "#{{&&:#{{==:#{{pid}},{}}},#{{==:#{{start_time}},{}}}}}",
            self.server_pid, self.server_start_time
        );
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
        let result = self.runner.run(&[
            "if-shell",
            "-F",
            &server_guard,
            &pane_command,
            &format!("display-message -p '{SERVER_MISMATCH_SENTINEL}'"),
        ]);
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
        match self
            .runner
            .run(&["display-message", "-p", "-t", &pane.pane_id, header])
        {
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
            Err(error) => {
                let message = error.to_string();
                if message.contains("can't find pane") || message.contains("no such pane") {
                    IndependentRead::PaneMissing
                } else {
                    IndependentRead::Unavailable(message)
                }
            }
        }
    }
}

pub fn quote_tmux_command_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneStateDiagnostic {
    pub pane_instance: PaneInstance,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalTransition {
    pub pane_instance: PaneInstance,
    pub from: Option<crate::daemon::session_badge::BadgeState>,
    pub to: Option<crate::daemon::session_badge::BadgeState>,
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

#[derive(Debug, Clone, Default)]
pub struct CanonicalStateRuntime {
    records: BTreeMap<PaneInstance, StoredPaneRecord>,
    quarantined: BTreeMap<PaneInstance, QuarantinedPaneRecord>,
    trackers: BTreeMap<PaneInstance, CaptureTrackerSnapshot>,
    diagnostics: VecDeque<PaneStateDiagnostic>,
    transitions: VecDeque<CanonicalTransition>,
    notification_jobs: VecDeque<CanonicalNotification>,
    triage: BTreeMap<PaneInstance, crate::daemon::session_badge::BadgeState>,
    flash: BTreeMap<PaneInstance, u8>,
    snapshot_revision: u64,
    snapshot_frame_too_large: bool,
    fail_stopped: bool,
    pending: Option<Box<PendingTransaction>>,
}

impl CanonicalStateRuntime {
    pub fn hydrate(entries: impl IntoIterator<Item = RawPaneRecord>) -> Self {
        let mut runtime = Self::default();
        for entry in entries {
            let pane = entry.pane_instance.clone();
            match load_record(entry) {
                LoadedPaneRecord::Missing => {}
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
                }
            }
        }
        runtime
    }

    pub fn record(&self, pane: &PaneInstance) -> Option<&StoredPaneRecord> {
        self.records.get(pane)
    }

    pub fn quarantined(&self, pane: &PaneInstance) -> Option<&QuarantinedPaneRecord> {
        self.quarantined.get(pane)
    }

    pub fn tracker(&self, pane: &PaneInstance) -> CaptureTrackerSnapshot {
        self.trackers.get(pane).cloned().unwrap_or_default()
    }

    pub fn diagnostics(&self) -> &VecDeque<PaneStateDiagnostic> {
        &self.diagnostics
    }

    pub fn transitions(&self) -> &VecDeque<CanonicalTransition> {
        &self.transitions
    }

    pub fn notification_jobs(&self) -> &VecDeque<CanonicalNotification> {
        &self.notification_jobs
    }

    pub fn snapshot_frame_too_large(&self) -> bool {
        self.snapshot_frame_too_large
    }

    pub fn snapshot_revision(&self) -> u64 {
        self.snapshot_revision
    }

    pub fn is_fail_stopped(&self) -> bool {
        self.fail_stopped
    }

    pub fn sequenced_mutations_paused(&self) -> bool {
        self.pending.is_some()
    }

    pub fn query_committed_snapshot_revision(&self) -> u64 {
        self.snapshot_revision
    }

    pub fn query_pane_while_paused(
        &self,
        pane: &PaneInstance,
        presentation_cache_hit: bool,
    ) -> QueryPaneAvailability {
        if presentation_cache_hit || self.pending.is_none() {
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
        if self.pending.is_some() {
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
        let expected_raw = current.map(serialize_record).transpose()?;
        let mut draft = self.clone();
        draft
            .records
            .insert(envelope.pane_instance.clone(), candidate.clone());
        draft.quarantined.remove(&envelope.pane_instance);
        if let Some(delta) = reduction.tracker_delta {
            draft
                .trackers
                .insert(envelope.pane_instance.clone(), delta.next);
        }
        draft.derive_candidate_projection(&envelope.pane_instance, current, Some(&candidate));
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;

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
        if self.pending.is_some() {
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
        draft.derive_candidate_projection(pane, self.records.get(pane), Some(&tombstone));
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        draft.preflight_projection(MAX_RESPONSE_FRAME_BYTES)?;
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
                draft
                    .quarantined
                    .insert(transaction.pane.clone(), quarantined.clone());
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

    fn descriptor(&self, pane: &PaneInstance) -> Option<StoredStateDescriptor> {
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
    ) {
        let previous_badge = record_badge(previous);
        let current_badge = record_badge(current);
        let state_version = match current {
            Some(StoredPaneRecord::Active(state)) => Some(state.version()),
            _ => None,
        };
        self.transitions.push_back(CanonicalTransition {
            pane_instance: pane.clone(),
            from: previous_badge,
            to: current_badge,
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
                }
            }
        }
        if let Some(badge @ crate::daemon::session_badge::BadgeState::Blocked) = current_badge {
            self.triage.insert(pane.clone(), badge);
        }
    }

    fn preflight_projection(&mut self, response_limit: usize) -> Result<(), StoreError> {
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
        Ok(())
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
        self.diagnostics.push_back(PaneStateDiagnostic {
            pane_instance,
            message,
        });
        while self.diagnostics.len() > MAX_DIAGNOSTICS {
            self.diagnostics.pop_front();
        }
        self.bump_snapshot_revision()?;
        self.validate_projection()?;
        self.preflight_projection(MAX_RESPONSE_FRAME_BYTES)
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
        let runtime = CanonicalStateRuntime::hydrate([
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
        assert!(!runtime.is_fail_stopped());
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
        let transition_count = runtime.transitions().len();
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
        assert_eq!(runtime.transitions().len(), transition_count);
        assert!(runtime.notification_jobs().is_empty());

        runtime
            .apply_event(
                &mut FakeIo::default(),
                &mut FakeClock::default(),
                &waiting,
                &VisibilitySnapshot::default(),
                DoneClearOn::Pane,
            )
            .unwrap();
        assert_eq!(runtime.transitions().len(), transition_count + 1);
        assert_eq!(runtime.notification_jobs().len(), 1);
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
    fn tmux_quote_preserves_apostrophes_as_one_argument() {
        assert_eq!(quote_tmux_command_argument("a'b"), "'a'\\''b'");
    }
}

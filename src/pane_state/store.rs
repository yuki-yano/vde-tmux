use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use serde::Serialize;

use crate::config::DoneClearOn;

use super::model::*;
use super::reducer::{ReduceError, ReductionOutcome, reduce};
use super::snapshot::PaneSnapshotStoreIo;

pub const MAX_DIAGNOSTICS: usize = 256;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    StateTooLarge,
    InvalidPaneInstance,
    StaleStateIdentity,
    WriterLeaseHeld,
    PersistFailed(String),
    FailStop(String),
    CounterOverflow(&'static str),
    Reduce(ReduceError),
    Random(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateTooLarge => f.write_str("serialized pane snapshot exceeds 16 MiB"),
            Self::InvalidPaneInstance => f.write_str("invalid pane instance"),
            Self::StaleStateIdentity => f.write_str("stale state identity"),
            Self::WriterLeaseHeld => f.write_str("pane state writer lease is already held"),
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
pub struct ObservationDispatchSnapshot {
    pub pane_instance: PaneInstance,
    pub base: Option<StoredStateDescriptor>,
    pub tracker: CaptureTrackerSnapshot,
    pub state: Option<PaneState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewAcknowledgementApplyResult {
    pub committed: usize,
    pub snapshot_revision: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CanonicalStateRuntime {
    records: BTreeMap<PaneInstance, PaneState>,
    trackers: BTreeMap<PaneInstance, CaptureTrackerSnapshot>,
    diagnostics: VecDeque<PaneStateDiagnostic>,
    transitions: VecDeque<CanonicalTransition>,
    notification_jobs: VecDeque<CanonicalNotification>,
    triage: BTreeMap<PaneInstance, crate::daemon::session_badge::BadgeState>,
    triage_calm_polls: BTreeMap<PaneInstance, u8>,
    flash: BTreeMap<PaneInstance, u8>,
    snapshot_revision: u64,
    snapshot_frame_too_large: bool,
    fail_stopped: bool,
}

impl CanonicalStateRuntime {
    pub fn hydrate(records: BTreeMap<PaneInstance, PaneState>) -> Result<Self, StoreError> {
        let mut runtime = Self {
            records,
            ..Self::default()
        };
        runtime.validate_projection()?;
        for (pane, state) in &runtime.records {
            runtime.trackers.insert(
                pane.clone(),
                CaptureTrackerSnapshot {
                    epoch: Some((state.state_id.clone(), state.agent_epoch)),
                    ..CaptureTrackerSnapshot::default()
                },
            );
            if super::resolve_badge(state) == crate::daemon::session_badge::BadgeState::Blocked {
                runtime.triage.insert(
                    pane.clone(),
                    crate::daemon::session_badge::BadgeState::Blocked,
                );
            }
        }
        Ok(runtime)
    }

    pub fn record(&self, pane: &PaneInstance) -> Option<&PaneState> {
        self.records.get(pane)
    }

    pub fn records_snapshot(&self) -> BTreeMap<PaneInstance, PaneState> {
        self.records.clone()
    }

    pub fn tracked_panes(&self) -> Vec<PaneInstance> {
        let mut panes = self
            .records
            .keys()
            .chain(self.trackers.keys())
            .cloned()
            .collect::<Vec<_>>();
        panes.sort();
        panes.dedup();
        panes
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
                state: self.records.get(&pane_instance).cloned(),
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

    pub fn advance_poll_projection(&mut self) -> Result<bool, StoreError> {
        let mut draft = self.clone();
        let mut visible_changed = false;
        for pane in draft.triage.keys().cloned().collect::<Vec<_>>() {
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
        for pane in draft.flash.keys().cloned().collect::<Vec<_>>() {
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
        let _ = draft.preflight_projection(super::MAX_RESPONSE_FRAME_BYTES)?;
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
        let preflight_changed = draft.preflight_projection(super::MAX_RESPONSE_FRAME_BYTES)?;
        if preflight_changed && draft.snapshot_revision == revision_before {
            draft.bump_snapshot_revision()?;
            let _ = draft.preflight_projection(super::MAX_RESPONSE_FRAME_BYTES)?;
        }
        let revision = draft.snapshot_revision;
        *self = draft;
        Ok(revision)
    }

    pub fn mark_projection_changed(&mut self) -> Result<u64, StoreError> {
        let mut draft = self.clone();
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        let _ = draft.preflight_projection(super::MAX_RESPONSE_FRAME_BYTES)?;
        let revision = draft.snapshot_revision;
        *self = draft;
        Ok(revision)
    }

    pub fn remove_absent_pane(
        &mut self,
        io: &mut dyn PaneSnapshotStoreIo,
        pane: &PaneInstance,
        expected: Option<&StoredStateDescriptor>,
    ) -> Result<bool, StoreError> {
        if self.fail_stopped {
            return Err(StoreError::FailStop("daemon is fail-stopped".to_string()));
        }
        if self.descriptor(pane).as_ref() != expected {
            return Ok(false);
        }
        let mut draft = self.clone();
        let removed = draft.records.remove(pane).is_some() | draft.trackers.remove(pane).is_some();
        if !removed {
            return Ok(false);
        }
        draft.triage.remove(pane);
        draft.triage_calm_polls.remove(pane);
        draft.flash.remove(pane);
        draft.bump_snapshot_revision()?;
        draft.validate_projection()?;
        let _ = draft.preflight_projection(super::MAX_RESPONSE_FRAME_BYTES)?;
        io.save(&draft.records)?;
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

    pub fn apply_event(
        &mut self,
        io: &mut dyn PaneSnapshotStoreIo,
        envelope: &PaneEventEnvelope,
        visibility: &VisibilitySnapshot,
        done_clear_on: DoneClearOn,
    ) -> Result<ApplyResult, StoreError> {
        if self.fail_stopped {
            return Err(StoreError::FailStop("daemon is fail-stopped".to_string()));
        }
        let mut draft = self.clone();
        let result = draft.apply_event_in_memory(envelope, visibility, done_clear_on)?;
        if result.outcome == ReductionOutcome::CanonicalChanged {
            io.save(&draft.records)?;
        }
        *self = draft;
        Ok(result)
    }

    pub fn apply_view_acknowledgements(
        &mut self,
        io: &mut dyn PaneSnapshotStoreIo,
        envelopes: &[PaneEventEnvelope],
        done_clear_on: DoneClearOn,
    ) -> Result<ViewAcknowledgementApplyResult, StoreError> {
        if self.fail_stopped {
            return Err(StoreError::FailStop("daemon is fail-stopped".to_string()));
        }
        let revision_before = self.snapshot_revision;
        let mut working = self.clone();
        let mut committed = 0;
        let mut projection_changed = false;
        for envelope in envelopes {
            working.snapshot_revision = revision_before;
            if !matches!(envelope.event, PaneEvent::AcknowledgeView { .. }) {
                return Err(StoreError::Reduce(ReduceError::InvalidRequest(
                    "view acknowledgment commit accepts only AcknowledgeView events".to_string(),
                )));
            }
            match working.apply_event_in_memory(
                envelope,
                &VisibilitySnapshot::default(),
                done_clear_on,
            ) {
                Ok(result) => {
                    projection_changed |= result.outcome != ReductionOutcome::Noop;
                    if result.outcome == ReductionOutcome::CanonicalChanged {
                        committed += 1;
                    }
                }
                Err(error) if error.requires_daemon_exit() => {
                    self.fail_stopped = true;
                    return Err(error);
                }
                Err(_) => continue,
            }
        }
        working.snapshot_revision = revision_before;
        if projection_changed && let Err(error) = working.bump_snapshot_revision() {
            self.fail_stopped = true;
            return Err(error);
        }
        if let Err(error) = working.validate_projection() {
            self.fail_stopped = true;
            return Err(error);
        }
        if let Err(error) = working.preflight_projection(super::MAX_RESPONSE_FRAME_BYTES) {
            self.fail_stopped = true;
            return Err(error);
        }
        if committed > 0
            && let Err(error) = io.save(&working.records)
        {
            return Err(error);
        }
        let result = ViewAcknowledgementApplyResult {
            committed,
            snapshot_revision: working.snapshot_revision,
        };
        *self = working;
        Ok(result)
    }

    pub fn descriptor(&self, pane: &PaneInstance) -> Option<StoredStateDescriptor> {
        self.records
            .get(pane)
            .map(|state| StoredStateDescriptor::Canonical {
                version: state.version(),
            })
    }

    fn apply_event_in_memory(
        &mut self,
        envelope: &PaneEventEnvelope,
        visibility: &VisibilitySnapshot,
        done_clear_on: DoneClearOn,
    ) -> Result<ApplyResult, StoreError> {
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
                state_version: self
                    .records
                    .get(&envelope.pane_instance)
                    .map(PaneState::version),
                snapshot_revision: self.snapshot_revision,
            });
        }
        let candidate = reduction.record.expect("canonical mutation has a state");
        let previous = self.records.get(&envelope.pane_instance).cloned();
        self.records
            .insert(envelope.pane_instance.clone(), candidate.clone());
        if let Some(delta) = reduction.tracker_delta {
            self.trackers
                .insert(envelope.pane_instance.clone(), delta.next);
        }
        self.derive_candidate_projection(
            &envelope.pane_instance,
            previous.as_ref(),
            Some(&candidate),
            transition_at_epoch(&envelope.event, Some(&candidate)),
        );
        self.bump_snapshot_revision()?;
        self.validate_projection()?;
        let _ = self.preflight_projection(super::MAX_RESPONSE_FRAME_BYTES)?;
        Ok(ApplyResult {
            outcome: ReductionOutcome::CanonicalChanged,
            state_version: Some(candidate.version()),
            snapshot_revision: self.snapshot_revision,
        })
    }

    fn derive_candidate_projection(
        &mut self,
        pane: &PaneInstance,
        previous: Option<&PaneState>,
        current: Option<&PaneState>,
        at_epoch: i64,
    ) {
        let previous_badge = record_badge(previous);
        let current_badge = record_badge(current);
        let discarded_completion = previous.zip(current).is_some_and(|(previous, current)| {
            previous_badge == Some(crate::daemon::session_badge::BadgeState::Done)
                && (previous.state_id != current.state_id
                    || previous.agent_epoch != current.agent_epoch
                    || previous.agent != current.agent
                    || previous.agent_session_id != current.agent_session_id)
        });
        let (agent, at_epoch) = if discarded_completion {
            let previous = previous.expect("discarded completion has previous state");
            (
                Some(previous.agent.clone()),
                previous
                    .completed_at
                    .or(previous.started_at)
                    .unwrap_or(at_epoch),
            )
        } else {
            (
                current.or(previous).map(|state| state.agent.clone()),
                at_epoch,
            )
        };
        let state_version = current.map(PaneState::version);
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
            records: Vec<&'a PaneState>,
            transitions: &'a VecDeque<CanonicalTransition>,
            diagnostics: &'a VecDeque<PaneStateDiagnostic>,
        }
        let bytes = serde_json::to_vec(&ProjectionPreflight {
            snapshot_revision: self.snapshot_revision,
            records: self.records.values().collect(),
            transitions: &self.transitions,
            diagnostics: &self.diagnostics,
        })
        .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
        let previous = self.snapshot_frame_too_large;
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
        }
        while self.diagnostics.len() > MAX_DIAGNOSTICS {
            self.diagnostics.pop_front();
        }
        Ok(previous != self.snapshot_frame_too_large)
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
        let _ = draft.preflight_projection(super::MAX_RESPONSE_FRAME_BYTES)?;
        *self = draft;
        Ok(())
    }

    fn validate_projection(&self) -> Result<(), StoreError> {
        for (pane, state) in &self.records {
            if &state.pane_instance != pane {
                return Err(StoreError::PersistFailed(
                    "canonical cache key and pane instance disagree".to_string(),
                ));
            }
            state
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

fn record_badge(state: Option<&PaneState>) -> Option<crate::daemon::session_badge::BadgeState> {
    state.map(crate::pane_state::resolve_badge)
}

fn transition_at_epoch(event: &PaneEvent, current: Option<&PaneState>) -> i64 {
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
            .and_then(|state| state.completed_at.or(state.started_at))
            .unwrap_or_default(),
    }
}

fn event_can_create_record(current: Option<&PaneState>, event: &PaneEvent) -> bool {
    if current.is_some() {
        return false;
    }
    match event {
        PaneEvent::AgentSessionStarted { .. }
        | PaneEvent::BeginRun { .. }
        | PaneEvent::ActivityObserved { .. }
        | PaneEvent::ActivityAndProgressObserved { .. }
        | PaneEvent::WaitRequested { .. }
        | PaneEvent::FailRun { .. }
        | PaneEvent::CompleteRun { .. } => true,
        PaneEvent::ExplicitStateReported { report } => match &report.lifecycle {
            Some(ReportedLifecycle::Running)
            | Some(ReportedLifecycle::Waiting { .. })
            | Some(ReportedLifecycle::Error { .. }) => true,
            Some(ReportedLifecycle::Idle) => report.completed_at.is_some() || report.attention,
            None => false,
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
    use super::*;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct RecordingStore {
        fail: bool,
        saves: usize,
        last: BTreeMap<PaneInstance, PaneState>,
        encoded_size: usize,
    }

    impl PaneSnapshotStoreIo for RecordingStore {
        fn save(&mut self, records: &BTreeMap<PaneInstance, PaneState>) -> Result<(), StoreError> {
            if self.fail {
                return Err(StoreError::PersistFailed(
                    "injected save failure".to_string(),
                ));
            }
            let identity = crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            };
            self.encoded_size = super::super::snapshot::encode_snapshot(&identity, records)?.len();
            self.saves += 1;
            self.last = records.clone();
            Ok(())
        }
    }

    fn pane(index: u32) -> PaneInstance {
        PaneInstance {
            pane_id: format!("%{index}"),
            pane_pid: 1000 + index,
        }
    }

    fn envelope(pane_instance: PaneInstance, event: PaneEvent) -> PaneEventEnvelope {
        PaneEventEnvelope {
            daemon_instance_id: DaemonInstanceId::generate().unwrap(),
            event_id: EventId::generate().unwrap(),
            pane_instance,
            agent: Some(AgentKind::parse("codex").unwrap()),
            agent_session_id: Some(AgentSessionId::parse("test-session").unwrap()),
            event,
        }
    }

    fn apply(
        runtime: &mut CanonicalStateRuntime,
        io: &mut dyn PaneSnapshotStoreIo,
        pane_instance: PaneInstance,
        event: PaneEvent,
    ) -> Result<ApplyResult, StoreError> {
        runtime.apply_event(
            io,
            &envelope(pane_instance, event),
            &VisibilitySnapshot::default(),
            DoneClearOn::Pane,
        )
    }

    #[test]
    fn save_failure_does_not_advance_memory_or_projection() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = RecordingStore {
            fail: true,
            ..RecordingStore::default()
        };
        let target = pane(1);
        assert!(
            apply(
                &mut runtime,
                &mut io,
                target.clone(),
                PaneEvent::BeginRun {
                    started_at: 1,
                    prompt: Some(PromptState {
                        text: "preserve me".to_string(),
                        source: "test".to_string(),
                    }),
                }
            )
            .is_err()
        );
        assert!(runtime.record(&target).is_none());
        assert_eq!(runtime.snapshot_revision(), 0);

        io.fail = false;
        apply(
            &mut runtime,
            &mut io,
            target.clone(),
            PaneEvent::BeginRun {
                started_at: 1,
                prompt: None,
            },
        )
        .unwrap();
        let before = runtime.record(&target).unwrap().clone();
        let revision = runtime.snapshot_revision();
        io.fail = true;
        assert!(
            apply(
                &mut runtime,
                &mut io,
                target.clone(),
                PaneEvent::WaitRequested {
                    observed_at: 2,
                    reason: WaitReason::PermissionPrompt,
                }
            )
            .is_err()
        );
        assert_eq!(runtime.record(&target), Some(&before));
        assert_eq!(runtime.snapshot_revision(), revision);
    }

    #[test]
    fn multi_pane_acknowledgement_persists_candidate_map_once() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = RecordingStore::default();
        for target in [pane(1), pane(2)] {
            apply(
                &mut runtime,
                &mut io,
                target.clone(),
                PaneEvent::BeginRun {
                    started_at: 1,
                    prompt: None,
                },
            )
            .unwrap();
            apply(
                &mut runtime,
                &mut io,
                target,
                PaneEvent::CompleteRun { completed_at: 2 },
            )
            .unwrap();
        }
        io.saves = 0;
        let acknowledgements = [pane(1), pane(2)]
            .into_iter()
            .map(|target| {
                let state = runtime.record(&target).unwrap();
                envelope(
                    target,
                    PaneEvent::AcknowledgeView {
                        expected_state_id: state.state_id.clone(),
                        expected_agent_epoch: state.agent_epoch,
                        through_seq: state.completed_seq,
                    },
                )
            })
            .collect::<Vec<_>>();
        let result = runtime
            .apply_view_acknowledgements(&mut io, &acknowledgements, DoneClearOn::Window)
            .unwrap();
        assert_eq!(result.committed, 2);
        assert_eq!(io.saves, 1);
        for target in [pane(1), pane(2)] {
            let state = runtime.record(&target).unwrap();
            assert_eq!(state.acknowledged_seq, state.completed_seq);
        }
    }

    #[test]
    fn fifty_active_states_and_multi_pane_ack_fit_hook_deadlines() {
        let mut runtime = CanonicalStateRuntime::default();
        let mut io = RecordingStore::default();
        let typed_started = Instant::now();
        for index in 1..=50 {
            apply(
                &mut runtime,
                &mut io,
                pane(index),
                PaneEvent::BeginRun {
                    started_at: index as i64,
                    prompt: Some(PromptState {
                        text: format!("prompt-{index}"),
                        source: "load-test".to_string(),
                    }),
                },
            )
            .unwrap();
        }
        let typed_elapsed = typed_started.elapsed();
        assert!(typed_elapsed < Duration::from_secs(2));
        assert_eq!(runtime.records_snapshot().len(), 50);
        assert!(io.encoded_size < super::super::MAX_RESPONSE_FRAME_BYTES);

        for index in 1..=50 {
            apply(
                &mut runtime,
                &mut io,
                pane(index),
                PaneEvent::CompleteRun {
                    completed_at: 100 + index as i64,
                },
            )
            .unwrap();
        }
        let acknowledgements = (1..=50)
            .map(|index| {
                let target = pane(index);
                let state = runtime.record(&target).unwrap();
                envelope(
                    target,
                    PaneEvent::AcknowledgeView {
                        expected_state_id: state.state_id.clone(),
                        expected_agent_epoch: state.agent_epoch,
                        through_seq: state.completed_seq,
                    },
                )
            })
            .collect::<Vec<_>>();
        let view_started = Instant::now();
        let result =
            runtime.apply_view_acknowledgements(&mut io, &acknowledgements, DoneClearOn::Window);
        let view_elapsed = view_started.elapsed();
        assert!(result.is_ok());
        assert!(view_elapsed < Duration::from_millis(500));
        eprintln!(
            "50-state snapshot metrics: bytes={} typed_mutations_ms={:.3} view_ack_ms={:.3}",
            io.encoded_size,
            typed_elapsed.as_secs_f64() * 1000.0,
            view_elapsed.as_secs_f64() * 1000.0,
        );
    }
}

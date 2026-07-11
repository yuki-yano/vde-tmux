use std::fmt;

use super::model::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReductionOutcome {
    Noop,
    TrackerOnly,
    CanonicalChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduction {
    pub record: Option<StoredPaneRecord>,
    pub tracker_delta: Option<CaptureTrackerDelta>,
    pub outcome: ReductionOutcome,
}

impl Reduction {
    fn unchanged(current: Option<&StoredPaneRecord>) -> Self {
        Self {
            record: current.cloned(),
            tracker_delta: None,
            outcome: ReductionOutcome::Noop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReduceError {
    InvalidRequest(String),
    InvalidPaneInstance,
    StaleStateIdentity,
    StaleSelection,
    StaleAgentEvent,
    InvalidProgressOperation(String),
    StateInvariantViolation(String),
    CounterOverflow(&'static str),
    MissingStateId,
}

impl fmt::Display for ReduceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message)
            | Self::InvalidProgressOperation(message)
            | Self::StateInvariantViolation(message) => f.write_str(message),
            Self::InvalidPaneInstance => f.write_str("invalid pane instance"),
            Self::StaleStateIdentity => f.write_str("stale state identity"),
            Self::StaleSelection => f.write_str("stale sidebar selection"),
            Self::StaleAgentEvent => f.write_str("stale agent event"),
            Self::CounterOverflow(counter) => write!(f, "{counter} counter overflow"),
            Self::MissingStateId => f.write_str("a preallocated state ID is required"),
        }
    }
}

impl std::error::Error for ReduceError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpochSource {
    Explicit,
    Process,
    ProcessVerifiedExplicitHandover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingIdentity {
    ExactPresent,
    ExactAbsent,
    UnboundPresent,
    UnboundAbsent,
    MismatchPresent,
    MismatchAbsent,
}

pub fn reduce(
    current: Option<&StoredPaneRecord>,
    envelope: &PaneEventEnvelope,
    context: ReductionContext<'_>,
) -> Result<Reduction, ReduceError> {
    envelope
        .pane_instance
        .validate()
        .map_err(|error| ReduceError::InvalidRequest(error.to_string()))?;
    if current.is_some_and(|record| record.pane_instance() != &envelope.pane_instance) {
        return Err(ReduceError::InvalidPaneInstance);
    }
    if envelope.event.is_semantically_empty() {
        return Ok(Reduction::unchanged(current));
    }

    match &envelope.event {
        PaneEvent::ObservationBatch { .. } => reduce_observation(current, envelope, context),
        PaneEvent::PaneRemoved { .. } => Ok(Reduction::unchanged(current)),
        PaneEvent::AcknowledgeView { .. } | PaneEvent::MarkDone { .. } => {
            reduce_internal_state_event(current, envelope, context)
        }
        event if event.is_external() => reduce_explicit(current, envelope, context),
        _ => Err(ReduceError::InvalidRequest(
            "unsupported reducer event".to_string(),
        )),
    }
}

fn reduce_internal_state_event(
    current: Option<&StoredPaneRecord>,
    envelope: &PaneEventEnvelope,
    context: ReductionContext<'_>,
) -> Result<Reduction, ReduceError> {
    let Some(StoredPaneRecord::Active(existing)) = current else {
        return match envelope.event {
            PaneEvent::MarkDone { .. } => Err(ReduceError::StaleSelection),
            _ => Ok(Reduction::unchanged(current)),
        };
    };
    let mut state = existing.clone();
    let completed_before = state.completed_seq;
    match &envelope.event {
        PaneEvent::AcknowledgeView {
            expected_state_id,
            expected_agent_epoch,
            through_seq,
        } => {
            if &state.state_id != expected_state_id || state.agent_epoch != *expected_agent_epoch {
                return Ok(Reduction::unchanged(current));
            }
            state.acknowledged_seq = state
                .acknowledged_seq
                .max((*through_seq).min(state.completed_seq));
        }
        PaneEvent::MarkDone {
            expected,
            completed_at,
        } => {
            if state.version() != *expected {
                return Err(ReduceError::StaleSelection);
            }
            mark_done(&mut state, *completed_at)?;
        }
        _ => unreachable!(),
    }
    let mut tracker = context.tracker.clone();
    let completed_outside_capture = state.completed_seq > completed_before
        || matches!(envelope.event, PaneEvent::MarkDone { .. });
    if completed_outside_capture {
        tracker.rebaseline_pending = true;
    }
    finish_state_reduction(
        current,
        state,
        tracker,
        completed_outside_capture.then_some(context.tracker),
    )
}

fn reduce_explicit(
    current: Option<&StoredPaneRecord>,
    envelope: &PaneEventEnvelope,
    context: ReductionContext<'_>,
) -> Result<Reduction, ReduceError> {
    let agent = envelope.agent.as_ref().ok_or_else(|| {
        ReduceError::InvalidRequest("explicit event requires an agent".to_string())
    })?;
    let session = envelope.agent_session_id.as_ref().ok_or_else(|| {
        ReduceError::InvalidRequest("explicit event requires an agent session ID".to_string())
    })?;

    let was_reset = matches!(current, Some(StoredPaneRecord::Reset(_)));
    if was_reset && is_completion(&envelope.event) {
        return Ok(Reduction::unchanged(current));
    }
    let mut state = match current {
        Some(StoredPaneRecord::Active(state)) => state.clone(),
        Some(StoredPaneRecord::Reset(_)) | None => {
            let Some(mut state) = initial_explicit_state(envelope, context.new_state_id.clone())?
            else {
                return Ok(Reduction::unchanged(current));
            };
            apply_initial_explicit_event(&mut state, &envelope.event, context.visibility)?;
            let completed_outside_capture = state.completed_seq > 0;
            let mut tracker = reset_tracker_for_state(context.tracker, &state)?;
            tracker.rebaseline_pending = completed_outside_capture;
            bump_tracker(&mut tracker)?;
            return finish_state_reduction(current, state, tracker, Some(context.tracker));
        }
    };

    if matches!(envelope.event, PaneEvent::AgentSessionStarted { .. }) {
        begin_agent_epoch(
            &mut state,
            agent.clone(),
            Some(session.clone()),
            EpochSource::Explicit,
        )?;
        apply_agent_session_started(&mut state, &envelope.event)?;
        let mut tracker = reset_tracker_for_state(context.tracker, &state)?;
        bump_tracker(&mut tracker)?;
        return finish_state_reduction(current, state, tracker, Some(context.tracker));
    }

    let tracker_matches_epoch = tracker_matches_state(context.tracker, &state);
    let identity = classify_identity(&state, agent, session);
    let epoch_evidence = is_epoch_start_evidence(&envelope.event);
    let completion = is_completion_for_state(&state, &envelope.event);
    let completed_before = state.completed_seq;
    match identity {
        ExistingIdentity::ExactPresent => {
            apply_regular_explicit_event(&mut state, &envelope.event, context.visibility)?;
        }
        ExistingIdentity::ExactAbsent => {
            if epoch_evidence {
                begin_agent_epoch(
                    &mut state,
                    agent.clone(),
                    Some(session.clone()),
                    EpochSource::Explicit,
                )?;
                apply_regular_explicit_event(&mut state, &envelope.event, context.visibility)?;
            } else if completion {
                apply_regular_explicit_event(&mut state, &envelope.event, context.visibility)?;
                state.agent_present = false;
                state.scan_verified = true;
            } else {
                return Err(ReduceError::StaleAgentEvent);
            }
        }
        ExistingIdentity::UnboundPresent => {
            if epoch_evidence || (completion && state.synthetic_completion_armed) {
                state.agent_session_id = Some(session.clone());
                apply_regular_explicit_event(&mut state, &envelope.event, context.visibility)?;
            }
        }
        ExistingIdentity::UnboundAbsent => {
            if epoch_evidence {
                begin_agent_epoch(
                    &mut state,
                    agent.clone(),
                    Some(session.clone()),
                    EpochSource::Explicit,
                )?;
                apply_regular_explicit_event(&mut state, &envelope.event, context.visibility)?;
            } else {
                return Err(ReduceError::StaleAgentEvent);
            }
        }
        ExistingIdentity::MismatchPresent => {
            let safe_handover = tracker_matches_epoch
                && epoch_evidence
                && context.tracker.replacement_streak >= 2
                && context.tracker.replacement_kind.as_ref() == Some(agent);
            if !safe_handover {
                return Err(ReduceError::StaleAgentEvent);
            }
            begin_agent_epoch(
                &mut state,
                agent.clone(),
                Some(session.clone()),
                EpochSource::ProcessVerifiedExplicitHandover,
            )?;
            apply_regular_explicit_event(&mut state, &envelope.event, context.visibility)?;
        }
        ExistingIdentity::MismatchAbsent => {
            if !epoch_evidence {
                return Err(ReduceError::StaleAgentEvent);
            }
            begin_agent_epoch(
                &mut state,
                agent.clone(),
                Some(session.clone()),
                EpochSource::Explicit,
            )?;
            apply_regular_explicit_event(&mut state, &envelope.event, context.visibility)?;
        }
    }

    let identity_changed = state.state_id != existing_state(current).unwrap().state_id
        || state.agent_epoch != existing_state(current).unwrap().agent_epoch;
    let mut tracker = if identity_changed || !tracker_matches_epoch {
        reset_tracker_for_state(context.tracker, &state)?
    } else {
        context.tracker.clone()
    };
    tracker.absence_count = 0;
    tracker.replacement_kind = None;
    tracker.replacement_streak = 0;
    if state.completed_seq > completed_before {
        tracker.rebaseline_pending = true;
    }
    bump_tracker(&mut tracker)?;
    finish_state_reduction(current, state, tracker, Some(context.tracker))
}

fn reduce_observation(
    current: Option<&StoredPaneRecord>,
    envelope: &PaneEventEnvelope,
    context: ReductionContext<'_>,
) -> Result<Reduction, ReduceError> {
    let PaneEvent::ObservationBatch {
        base,
        tracker_generation,
        observed_at,
        presence,
        capture,
    } = &envelope.event
    else {
        unreachable!();
    };
    if base.as_ref() != current.map(StoredPaneRecord::descriptor).as_ref()
        || *tracker_generation != context.tracker.generation
    {
        return Ok(Reduction::unchanged(current));
    }
    if matches!(current, Some(StoredPaneRecord::Reset(_)))
        && !matches!(presence, AgentPresenceObservation::Present(_))
    {
        return Ok(Reduction::unchanged(current));
    }

    let mut tracker = context.tracker.clone();
    let mut created = false;
    let mut state = match current {
        Some(StoredPaneRecord::Active(state)) => state.clone(),
        Some(StoredPaneRecord::Reset(_)) | None => match presence {
            AgentPresenceObservation::Present(agent) => {
                created = true;
                new_state(
                    envelope,
                    agent.clone(),
                    None,
                    true,
                    current.is_none(),
                    context.new_state_id.clone(),
                )?
            }
            AgentPresenceObservation::Absent | AgentPresenceObservation::Unknown => {
                return Ok(Reduction::unchanged(current));
            }
        },
    };

    if created {
        tracker = reset_tracker_for_state(&tracker, &state)?;
        tracker.fingerprint = capture
            .as_ref()
            .and_then(|capture| capture.observed_fingerprint);
        tracker.last_change_at = tracker.fingerprint.map(|_| *observed_at);
        bump_tracker(&mut tracker)?;
        return finish_state_reduction(current, state, tracker, Some(context.tracker));
    }

    match presence {
        AgentPresenceObservation::Unknown => {
            tracker.absence_count = 0;
            tracker.replacement_kind = None;
            tracker.replacement_streak = 0;
        }
        AgentPresenceObservation::Absent => {
            tracker.replacement_kind = None;
            tracker.replacement_streak = 0;
            if state.scan_verified && supports_process_detection(&state.agent) {
                tracker.absence_count = tracker
                    .absence_count
                    .checked_add(1)
                    .ok_or(ReduceError::CounterOverflow("absence"))?;
                if tracker.absence_count >= 2 {
                    confirm_absent(&mut state, *observed_at, context.visibility)?;
                    tracker.absence_count = 0;
                }
            }
        }
        AgentPresenceObservation::Present(observed_agent) if observed_agent == &state.agent => {
            tracker.absence_count = 0;
            tracker.replacement_kind = None;
            tracker.replacement_streak = 0;
            if state.agent_present {
                state.scan_verified = true;
                apply_capture(
                    &mut state,
                    capture.as_ref(),
                    &mut tracker,
                    *observed_at,
                    context.visibility,
                )?;
            } else {
                begin_agent_epoch(
                    &mut state,
                    observed_agent.clone(),
                    None,
                    EpochSource::Process,
                )?;
                tracker = reset_tracker_for_state(&tracker, &state)?;
                tracker.fingerprint = capture
                    .as_ref()
                    .and_then(|capture| capture.observed_fingerprint);
                tracker.last_change_at = tracker.fingerprint.map(|_| *observed_at);
            }
        }
        AgentPresenceObservation::Present(observed_agent) => {
            if state.agent_present {
                if state.scan_verified {
                    tracker.absence_count = tracker
                        .absence_count
                        .checked_add(1)
                        .ok_or(ReduceError::CounterOverflow("absence"))?;
                    if tracker.absence_count >= 2 {
                        confirm_absent(&mut state, *observed_at, context.visibility)?;
                        tracker.absence_count = 0;
                    }
                }
                if tracker.replacement_kind.as_ref() == Some(observed_agent) {
                    tracker.replacement_streak = tracker
                        .replacement_streak
                        .checked_add(1)
                        .ok_or(ReduceError::CounterOverflow("replacement streak"))?;
                } else {
                    tracker.replacement_kind = Some(observed_agent.clone());
                    tracker.replacement_streak = 1;
                }
            } else {
                begin_agent_epoch(
                    &mut state,
                    observed_agent.clone(),
                    None,
                    EpochSource::Process,
                )?;
                tracker = reset_tracker_for_state(&tracker, &state)?;
                tracker.fingerprint = capture
                    .as_ref()
                    .and_then(|capture| capture.observed_fingerprint);
                tracker.last_change_at = tracker.fingerprint.map(|_| *observed_at);
            }
        }
    }
    bump_tracker(&mut tracker)?;
    finish_state_reduction(current, state, tracker, Some(context.tracker))
}

fn existing_state(current: Option<&StoredPaneRecord>) -> Option<&PaneState> {
    match current {
        Some(StoredPaneRecord::Active(state)) => Some(state),
        _ => None,
    }
}

fn initial_explicit_state(
    envelope: &PaneEventEnvelope,
    new_state_id: Option<StateId>,
) -> Result<Option<PaneState>, ReduceError> {
    if !can_create_from_explicit(&envelope.event) {
        return Ok(None);
    }
    let agent = envelope.agent.clone().ok_or_else(|| {
        ReduceError::InvalidRequest("explicit event requires an agent".to_string())
    })?;
    let session = envelope.agent_session_id.clone().ok_or_else(|| {
        ReduceError::InvalidRequest("explicit event requires an agent session ID".to_string())
    })?;
    new_state(envelope, agent, Some(session), false, false, new_state_id).map(Some)
}

fn new_state(
    envelope: &PaneEventEnvelope,
    agent: AgentKind,
    session: Option<AgentSessionId>,
    scan_verified: bool,
    synthetic_completion_armed: bool,
    new_state_id: Option<StateId>,
) -> Result<PaneState, ReduceError> {
    Ok(PaneState {
        schema_version: PANE_STATE_SCHEMA_VERSION,
        state_id: new_state_id.ok_or(ReduceError::MissingStateId)?,
        revision: 0,
        pane_instance: envelope.pane_instance.clone(),
        agent,
        agent_session_id: session,
        agent_epoch: 1,
        agent_present: true,
        scan_verified,
        synthetic_completion_armed,
        lifecycle: LifecycleState::Idle,
        run_seq: 0,
        completed_seq: 0,
        acknowledged_seq: 0,
        started_at: None,
        completed_at: None,
        prompt: None,
        tasks: TaskState::default(),
        subagents: Vec::new(),
        worktree_activity: None,
    })
}

fn can_create_from_explicit(event: &PaneEvent) -> bool {
    is_epoch_start_evidence(event) || is_completion(event)
}

fn apply_initial_explicit_event(
    state: &mut PaneState,
    event: &PaneEvent,
    visibility: &VisibilitySnapshot,
) -> Result<(), ReduceError> {
    match event {
        PaneEvent::AgentSessionStarted { .. } => apply_agent_session_started(state, event),
        PaneEvent::CompleteRun { completed_at } => {
            complete_run(state, *completed_at, visibility, true)
        }
        PaneEvent::ExplicitStateReported { report }
            if matches!(report.lifecycle, Some(ReportedLifecycle::Idle)) =>
        {
            complete_run(
                state,
                report.completed_at.unwrap_or(report.observed_at),
                visibility,
                true,
            )?;
            apply_report_fields(state, report)
        }
        _ => apply_regular_explicit_event(state, event, visibility),
    }
}

fn apply_regular_explicit_event(
    state: &mut PaneState,
    event: &PaneEvent,
    visibility: &VisibilitySnapshot,
) -> Result<(), ReduceError> {
    match event {
        PaneEvent::AgentSessionStarted { .. } => apply_agent_session_started(state, event),
        PaneEvent::BeginRun { started_at, prompt } => begin_run(state, *started_at, prompt.clone()),
        PaneEvent::ActivityObserved { observed_at } => activity_observed(state, *observed_at),
        PaneEvent::WaitRequested {
            observed_at,
            reason,
        } => wait_requested(state, *observed_at, reason.clone()),
        PaneEvent::CompleteRun { completed_at } => {
            complete_run(state, *completed_at, visibility, false)
        }
        PaneEvent::FailRun {
            observed_at,
            reason,
        } => fail_run(state, *observed_at, reason.clone()),
        PaneEvent::ProgressUpdated { operations, .. } => apply_progress(state, operations),
        PaneEvent::ExplicitStateReported { report } => {
            apply_explicit_report(state, report, visibility)
        }
        _ => Err(ReduceError::InvalidRequest(
            "event is not an explicit agent event".to_string(),
        )),
    }
}

fn apply_agent_session_started(
    state: &mut PaneState,
    event: &PaneEvent,
) -> Result<(), ReduceError> {
    let PaneEvent::AgentSessionStarted {
        source,
        resumed_prompt,
        ..
    } = event
    else {
        unreachable!();
    };
    state.prompt = if *source == AgentSessionSource::Resume {
        resumed_prompt.clone()
    } else {
        None
    };
    Ok(())
}

fn apply_explicit_report(
    state: &mut PaneState,
    report: &ExplicitStateReport,
    visibility: &VisibilitySnapshot,
) -> Result<(), ReduceError> {
    match &report.lifecycle {
        Some(ReportedLifecycle::Running) => {
            let started_at = report.started_at.unwrap_or(report.observed_at);
            activity_observed(state, started_at)?;
        }
        Some(ReportedLifecycle::Waiting { reason }) => {
            wait_requested(state, report.observed_at, reason.clone())?;
        }
        Some(ReportedLifecycle::Idle)
            if state.run_seq > state.completed_seq
                || (state.synthetic_completion_armed
                    && state.run_seq == 0
                    && (report.completed_at.is_some() || report.attention)) =>
        {
            complete_run(
                state,
                report.completed_at.unwrap_or(report.observed_at),
                visibility,
                false,
            )?;
        }
        Some(ReportedLifecycle::Idle) => {}
        Some(ReportedLifecycle::Error { reason }) => {
            fail_run(state, report.observed_at, reason.clone())?;
        }
        None => {}
    }
    apply_report_fields(state, report)
}

fn apply_report_fields(
    state: &mut PaneState,
    report: &ExplicitStateReport,
) -> Result<(), ReduceError> {
    if let Some(update) = &report.prompt {
        state.prompt = match update {
            FieldUpdate::Set(prompt) => Some(prompt.clone()),
            FieldUpdate::Clear => None,
        };
    }
    if let Some(update) = &report.tasks {
        match update {
            FieldUpdate::Set(progress) => {
                state.tasks.progress = progress.clone();
                state.tasks.items.clear();
            }
            FieldUpdate::Clear => state.tasks = TaskState::default(),
        }
    }
    if let Some(update) = &report.subagents {
        state.subagents = match update {
            FieldUpdate::Set(subagents) => subagents.clone(),
            FieldUpdate::Clear => Vec::new(),
        };
    }
    Ok(())
}

fn classify_identity(
    state: &PaneState,
    agent: &AgentKind,
    session: &AgentSessionId,
) -> ExistingIdentity {
    if &state.agent == agent {
        match (&state.agent_session_id, state.agent_present) {
            (Some(current), true) if current == session => ExistingIdentity::ExactPresent,
            (Some(current), false) if current == session => ExistingIdentity::ExactAbsent,
            (None, true) => ExistingIdentity::UnboundPresent,
            (None, false) => ExistingIdentity::UnboundAbsent,
            (_, true) => ExistingIdentity::MismatchPresent,
            (_, false) => ExistingIdentity::MismatchAbsent,
        }
    } else if state.agent_present {
        ExistingIdentity::MismatchPresent
    } else {
        ExistingIdentity::MismatchAbsent
    }
}

fn is_epoch_start_evidence(event: &PaneEvent) -> bool {
    matches!(
        event,
        PaneEvent::AgentSessionStarted { .. }
            | PaneEvent::BeginRun { .. }
            | PaneEvent::ActivityObserved { .. }
            | PaneEvent::WaitRequested { .. }
            | PaneEvent::FailRun { .. }
    ) || matches!(
        event,
        PaneEvent::ExplicitStateReported {
            report: ExplicitStateReport {
                lifecycle: Some(
                    ReportedLifecycle::Running
                        | ReportedLifecycle::Waiting { .. }
                        | ReportedLifecycle::Error { .. }
                ),
                ..
            }
        }
    )
}

fn is_completion(event: &PaneEvent) -> bool {
    matches!(event, PaneEvent::CompleteRun { .. })
        || matches!(
            event,
            PaneEvent::ExplicitStateReported {
                report: ExplicitStateReport {
                    lifecycle: Some(ReportedLifecycle::Idle),
                    completed_at: Some(_),
                    ..
                }
            }
        )
        || matches!(
            event,
            PaneEvent::ExplicitStateReported {
                report: ExplicitStateReport {
                    lifecycle: Some(ReportedLifecycle::Idle),
                    attention: true,
                    ..
                }
            }
        )
}

fn is_completion_for_state(state: &PaneState, event: &PaneEvent) -> bool {
    matches!(event, PaneEvent::CompleteRun { .. })
        || matches!(
            event,
            PaneEvent::ExplicitStateReported {
                report: ExplicitStateReport {
                    lifecycle: Some(ReportedLifecycle::Idle),
                    ..
                }
            } if state.run_seq > state.completed_seq
                || (state.synthetic_completion_armed && is_completion(event))
        )
}

fn begin_agent_epoch(
    state: &mut PaneState,
    agent: AgentKind,
    session: Option<AgentSessionId>,
    source: EpochSource,
) -> Result<(), ReduceError> {
    state.agent_epoch = state
        .agent_epoch
        .checked_add(1)
        .ok_or(ReduceError::CounterOverflow("agent epoch"))?;
    state.agent = agent;
    state.agent_session_id = session;
    state.agent_present = true;
    state.scan_verified = matches!(
        source,
        EpochSource::Process | EpochSource::ProcessVerifiedExplicitHandover
    );
    state.synthetic_completion_armed = false;
    state.lifecycle = LifecycleState::Idle;
    state.run_seq = 0;
    state.completed_seq = 0;
    state.acknowledged_seq = 0;
    state.started_at = None;
    state.completed_at = None;
    state.prompt = None;
    state.tasks = TaskState::default();
    state.subagents.clear();
    state.worktree_activity = None;
    Ok(())
}

fn start_new_run(state: &mut PaneState, started_at: i64) -> Result<(), ReduceError> {
    if state.run_seq == state.completed_seq {
        state.run_seq = state
            .run_seq
            .checked_add(1)
            .ok_or(ReduceError::CounterOverflow("run sequence"))?;
        state.started_at = Some(started_at);
        state.prompt = None;
        state.tasks = TaskState::default();
        state.subagents.clear();
        state.worktree_activity = None;
    }
    state.synthetic_completion_armed = false;
    Ok(())
}

fn begin_run(
    state: &mut PaneState,
    started_at: i64,
    prompt: Option<PromptState>,
) -> Result<(), ReduceError> {
    start_new_run(state, started_at)?;
    state.lifecycle = LifecycleState::Running;
    state.prompt = prompt;
    Ok(())
}

fn activity_observed(state: &mut PaneState, observed_at: i64) -> Result<(), ReduceError> {
    start_new_run(state, observed_at)?;
    state.lifecycle = LifecycleState::Running;
    Ok(())
}

fn wait_requested(
    state: &mut PaneState,
    observed_at: i64,
    reason: WaitReason,
) -> Result<(), ReduceError> {
    start_new_run(state, observed_at)?;
    state.lifecycle = LifecycleState::Waiting { reason };
    Ok(())
}

fn fail_run(
    state: &mut PaneState,
    observed_at: i64,
    reason: Option<String>,
) -> Result<(), ReduceError> {
    start_new_run(state, observed_at)?;
    state.lifecycle = LifecycleState::Error { reason };
    Ok(())
}

fn complete_run(
    state: &mut PaneState,
    completed_at: i64,
    visibility: &VisibilitySnapshot,
    allow_unarmed_synthetic: bool,
) -> Result<(), ReduceError> {
    if state.run_seq == 0 {
        if !state.synthetic_completion_armed && !allow_unarmed_synthetic {
            return Ok(());
        }
        state.run_seq = 1;
        state.started_at = Some(completed_at);
    } else if state.run_seq == state.completed_seq {
        return Ok(());
    }
    state.completed_seq = state.run_seq;
    state.lifecycle = LifecycleState::Idle;
    state.completed_at = Some(completed_at);
    state.synthetic_completion_armed = false;
    state.subagents.clear();
    state.worktree_activity = None;
    if visibility.pane_visible_to_eligible_client {
        state.acknowledged_seq = state.completed_seq;
    }
    Ok(())
}

fn mark_done(state: &mut PaneState, completed_at: i64) -> Result<(), ReduceError> {
    if state.run_seq == state.completed_seq {
        state.run_seq = state
            .run_seq
            .checked_add(1)
            .ok_or(ReduceError::CounterOverflow("run sequence"))?;
        state.started_at = Some(completed_at);
    }
    state.completed_seq = state.run_seq;
    state.lifecycle = LifecycleState::Idle;
    state.completed_at = Some(completed_at);
    state.synthetic_completion_armed = false;
    state.tasks = TaskState::default();
    state.subagents.clear();
    state.worktree_activity = None;
    Ok(())
}

fn confirm_absent(
    state: &mut PaneState,
    observed_at: i64,
    visibility: &VisibilitySnapshot,
) -> Result<(), ReduceError> {
    if state.run_seq > state.completed_seq {
        complete_run(state, observed_at, visibility, false)?;
    }
    state.agent_present = false;
    state.scan_verified = true;
    Ok(())
}

fn apply_progress(
    state: &mut PaneState,
    operations: &[ProgressOperation],
) -> Result<(), ReduceError> {
    for operation in operations {
        validate_progress_operation(operation)?;
        match operation {
            ProgressOperation::SetPrompt(prompt) => state.prompt = Some(prompt.clone()),
            ProgressOperation::ClearPrompt => state.prompt = None,
            ProgressOperation::TaskCreated => {
                state.tasks.items.clear();
                state.tasks.progress.total = state
                    .tasks
                    .progress
                    .total
                    .checked_add(1)
                    .ok_or(ReduceError::CounterOverflow("task total"))?;
            }
            ProgressOperation::TaskCompleted => {
                state.tasks.items.clear();
                state.tasks.progress.done = state
                    .tasks
                    .progress
                    .done
                    .checked_add(1)
                    .ok_or(ReduceError::CounterOverflow("task completed"))?;
                if state.tasks.progress.done > state.tasks.progress.total {
                    return Err(ReduceError::InvalidProgressOperation(
                        "task completion exceeds total".to_string(),
                    ));
                }
            }
            ProgressOperation::ReplaceTasks { progress, items } => {
                state.tasks = TaskState {
                    progress: progress.clone(),
                    items: items.clone(),
                };
                derive_task_progress(&mut state.tasks);
            }
            ProgressOperation::UpsertTaskItem { id, step } => {
                if let Some(item) = state
                    .tasks
                    .items
                    .iter_mut()
                    .find(|item| item.id.as_ref() == Some(id))
                {
                    item.step = step.clone();
                    item.status = TaskItemStatus::Pending;
                } else {
                    state.tasks.items.push(TaskItemState {
                        id: Some(id.clone()),
                        step: step.clone(),
                        status: TaskItemStatus::Pending,
                    });
                }
                derive_task_progress(&mut state.tasks);
            }
            ProgressOperation::UpdateTaskItemStatus { id, status } => {
                let Some(item) = state
                    .tasks
                    .items
                    .iter_mut()
                    .find(|item| item.id.as_ref() == Some(id))
                else {
                    return Err(ReduceError::InvalidProgressOperation(format!(
                        "unknown task item ID {id:?}"
                    )));
                };
                item.status = *status;
                derive_task_progress(&mut state.tasks);
            }
            ProgressOperation::ClearTasks => state.tasks = TaskState::default(),
            ProgressOperation::UpsertSubagent(subagent) => {
                if let Some(existing) = state
                    .subagents
                    .iter_mut()
                    .find(|existing| existing.agent_id == subagent.agent_id)
                {
                    *existing = subagent.clone();
                } else {
                    state.subagents.push(subagent.clone());
                }
            }
            ProgressOperation::RemoveSubagent { agent_id } => {
                state
                    .subagents
                    .retain(|subagent| subagent.agent_id != *agent_id);
            }
            ProgressOperation::ReplaceSubagents(subagents) => {
                state.subagents = subagents.clone();
            }
            ProgressOperation::ClearSubagents => state.subagents.clear(),
            ProgressOperation::SetWorktreeActivity(activity) => {
                state.worktree_activity = Some(activity.clone());
            }
            ProgressOperation::ClearWorktreeActivity => state.worktree_activity = None,
        }
    }
    validate_tasks(&state.tasks)
        .and_then(|_| validate_subagents(&state.subagents))
        .map_err(|error| ReduceError::InvalidProgressOperation(error.to_string()))?;
    Ok(())
}

fn validate_progress_operation(operation: &ProgressOperation) -> Result<(), ReduceError> {
    let invalid = |error: ModelError| ReduceError::InvalidProgressOperation(error.to_string());
    match operation {
        ProgressOperation::SetPrompt(prompt) => prompt.validate().map_err(invalid),
        ProgressOperation::ClearPrompt
        | ProgressOperation::TaskCreated
        | ProgressOperation::TaskCompleted
        | ProgressOperation::ClearTasks
        | ProgressOperation::ClearSubagents
        | ProgressOperation::ClearWorktreeActivity => Ok(()),
        ProgressOperation::ReplaceTasks { progress, items } => {
            let mut tasks = TaskState {
                progress: progress.clone(),
                items: items.clone(),
            };
            derive_task_progress(&mut tasks);
            validate_tasks(&tasks).map_err(invalid)
        }
        ProgressOperation::UpsertTaskItem { id, step } => {
            validate_required_text(id, "task item ID", IDENTIFIER_MAX_BYTES)
                .and_then(|_| validate_required_text(step, "task step", BODY_MAX_BYTES))
                .map_err(invalid)
        }
        ProgressOperation::UpdateTaskItemStatus { id, .. } => {
            validate_required_text(id, "task item ID", IDENTIFIER_MAX_BYTES).map_err(invalid)
        }
        ProgressOperation::UpsertSubagent(subagent) => {
            validate_subagents(std::slice::from_ref(subagent)).map_err(invalid)
        }
        ProgressOperation::RemoveSubagent { agent_id } => {
            validate_required_text(agent_id, "subagent ID", IDENTIFIER_MAX_BYTES).map_err(invalid)
        }
        ProgressOperation::ReplaceSubagents(subagents) => {
            validate_subagents(subagents).map_err(invalid)
        }
        ProgressOperation::SetWorktreeActivity(activity) => {
            validate_required_text(&activity.name, "worktree name", BODY_MAX_BYTES)
                .and_then(|_| {
                    validate_required_text(&activity.path, "worktree path", PATH_MAX_BYTES)
                })
                .and_then(|_| {
                    validate_required_text(&activity.command, "worktree command", BODY_MAX_BYTES)
                })
                .map_err(invalid)
        }
    }
}

fn derive_task_progress(tasks: &mut TaskState) {
    if tasks.items.is_empty() {
        return;
    }
    tasks.progress = TaskProgress {
        done: tasks
            .items
            .iter()
            .filter(|item| item.status == TaskItemStatus::Completed)
            .count() as u64,
        total: tasks.items.len() as u64,
    };
}

fn apply_capture(
    state: &mut PaneState,
    capture: Option<&CaptureObservation>,
    tracker: &mut CaptureTrackerSnapshot,
    observed_at: i64,
    visibility: &VisibilitySnapshot,
) -> Result<(), ReduceError> {
    let Some(capture) = capture else {
        return Ok(());
    };
    let fingerprint_changed = capture.observed_fingerprint.is_some()
        && capture.observed_fingerprint != tracker.fingerprint;
    if tracker.rebaseline_pending {
        if capture.observed_fingerprint.is_some() {
            tracker.fingerprint = capture.observed_fingerprint;
            tracker.last_change_at = Some(observed_at);
            tracker.rebaseline_pending = false;
        }
        return Ok(());
    }
    match &capture.inference {
        CaptureInference::PermissionWait { reason } => {
            wait_requested(state, observed_at, reason.clone())?;
        }
        CaptureInference::ActivityObserved => {
            if tracker.fingerprint.is_some() {
                activity_observed(state, observed_at)?;
            }
        }
        CaptureInference::StaleRunCompleted => {
            if matches!(state.lifecycle, LifecycleState::Running) {
                complete_run(state, observed_at, visibility, false)?;
            }
        }
        CaptureInference::NoChange => {}
    }
    if capture.observed_fingerprint.is_some() {
        tracker.fingerprint = capture.observed_fingerprint;
        if fingerprint_changed || tracker.last_change_at.is_none() {
            tracker.last_change_at = Some(observed_at);
        }
    }
    Ok(())
}

fn supports_process_detection(agent: &AgentKind) -> bool {
    matches!(agent.as_str(), "claude" | "codex" | "opencode")
}

fn tracker_matches_state(tracker: &CaptureTrackerSnapshot, state: &PaneState) -> bool {
    tracker.epoch.as_ref() == Some(&(state.state_id.clone(), state.agent_epoch))
}

fn reset_tracker_for_state(
    tracker: &CaptureTrackerSnapshot,
    state: &PaneState,
) -> Result<CaptureTrackerSnapshot, ReduceError> {
    Ok(CaptureTrackerSnapshot {
        generation: tracker.generation,
        epoch: Some((state.state_id.clone(), state.agent_epoch)),
        absence_count: 0,
        replacement_kind: None,
        replacement_streak: 0,
        fingerprint: None,
        last_change_at: None,
        rebaseline_pending: false,
    })
}

fn bump_tracker(tracker: &mut CaptureTrackerSnapshot) -> Result<(), ReduceError> {
    tracker.generation = tracker
        .generation
        .checked_add(1)
        .ok_or(ReduceError::CounterOverflow("capture tracker generation"))?;
    Ok(())
}

fn finish_state_reduction(
    current: Option<&StoredPaneRecord>,
    mut state: PaneState,
    tracker: CaptureTrackerSnapshot,
    previous_tracker: Option<&CaptureTrackerSnapshot>,
) -> Result<Reduction, ReduceError> {
    let canonical_changed = existing_state(current) != Some(&state)
        || !matches!(current, Some(StoredPaneRecord::Active(_)));
    if canonical_changed {
        state.revision = match existing_state(current) {
            Some(existing) => existing
                .revision
                .checked_add(1)
                .ok_or(ReduceError::CounterOverflow("revision"))?,
            None => 1,
        };
        state
            .validate()
            .map_err(|error| ReduceError::StateInvariantViolation(error.to_string()))?;
    }
    let tracker_changed = previous_tracker.is_some_and(|previous| previous != &tracker);
    let outcome = if canonical_changed {
        ReductionOutcome::CanonicalChanged
    } else if tracker_changed {
        ReductionOutcome::TrackerOnly
    } else {
        ReductionOutcome::Noop
    };
    Ok(Reduction {
        record: Some(StoredPaneRecord::Active(state)),
        tracker_delta: tracker_changed.then_some(CaptureTrackerDelta { next: tracker }),
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::daemon::session_badge::BadgeState;
    use crate::pane_state::resolve_badge;

    const STATE_ID: &str = "00112233445566778899aabbccddeeff";
    const OTHER_STATE_ID: &str = "112233445566778899aabbccddeeff00";
    const EVENT_ID: &str = "102132435465768798a9bacbdcedfe0f";
    const DAEMON_ID: &str = "ffeeddccbbaa99887766554433221100";

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
            agent_session_id: Some(AgentSessionId::parse("session-1").unwrap()),
            event,
        }
    }

    fn explicit_envelope(agent: &str, session: &str, event: PaneEvent) -> PaneEventEnvelope {
        let mut envelope = envelope(event);
        envelope.agent = Some(AgentKind::parse(agent).unwrap());
        envelope.agent_session_id = Some(AgentSessionId::parse(session).unwrap());
        envelope
    }

    fn observation_envelope(event: PaneEvent) -> PaneEventEnvelope {
        let mut envelope = envelope(event);
        envelope.agent = None;
        envelope.agent_session_id = None;
        envelope
    }

    fn context<'a>(
        tracker: &'a CaptureTrackerSnapshot,
        visibility: &'a VisibilitySnapshot,
    ) -> ReductionContext<'a> {
        ReductionContext {
            done_clear_on: crate::config::DoneClearOn::Pane,
            visibility,
            tracker,
            new_state_id: Some(StateId::parse(STATE_ID).unwrap()),
        }
    }

    fn active(result: &Reduction) -> &PaneState {
        match result.record.as_ref().unwrap() {
            StoredPaneRecord::Active(state) => state,
            StoredPaneRecord::Reset(_) => panic!("expected active state"),
        }
    }

    fn reduce_once(
        current: Option<&StoredPaneRecord>,
        event: PaneEvent,
        tracker: &CaptureTrackerSnapshot,
    ) -> Reduction {
        reduce(
            current,
            &envelope(event),
            context(tracker, &VisibilitySnapshot::default()),
        )
        .unwrap()
    }

    fn reduce_explicit_once(
        current: Option<&StoredPaneRecord>,
        agent: &str,
        session: &str,
        event: PaneEvent,
        tracker: &CaptureTrackerSnapshot,
    ) -> Result<Reduction, ReduceError> {
        reduce(
            current,
            &explicit_envelope(agent, session, event),
            context(tracker, &VisibilitySnapshot::default()),
        )
    }

    fn begin(current: Option<&StoredPaneRecord>, tracker: &CaptureTrackerSnapshot) -> Reduction {
        reduce_once(
            current,
            PaneEvent::BeginRun {
                started_at: 10,
                prompt: None,
            },
            tracker,
        )
    }

    fn progress(current: &Reduction, operations: Vec<ProgressOperation>) -> Reduction {
        reduce_once(
            current.record.as_ref(),
            PaneEvent::ProgressUpdated {
                observed_at: 20,
                operations,
            },
            &current.tracker_delta.as_ref().unwrap().next,
        )
    }

    fn reset_record() -> StoredPaneRecord {
        StoredPaneRecord::Reset(ResetTombstone {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            tombstone_id: ResetTombstoneId::parse(OTHER_STATE_ID).unwrap(),
            pane_instance: pane(),
            reset_at: 1,
        })
    }

    fn observe(
        current: Option<&StoredPaneRecord>,
        tracker: &CaptureTrackerSnapshot,
        presence: AgentPresenceObservation,
        capture: Option<CaptureObservation>,
        observed_at: i64,
    ) -> Reduction {
        let event = PaneEvent::ObservationBatch {
            base: current.map(StoredPaneRecord::descriptor),
            tracker_generation: tracker.generation,
            observed_at,
            presence,
            capture,
        };
        reduce(
            current,
            &observation_envelope(event),
            context(tracker, &VisibilitySnapshot::default()),
        )
        .unwrap()
    }

    fn discover(agent: &str) -> Reduction {
        observe(
            None,
            &CaptureTrackerSnapshot::default(),
            AgentPresenceObservation::Present(AgentKind::parse(agent).unwrap()),
            None,
            1,
        )
    }

    fn report(lifecycle: Option<ReportedLifecycle>) -> PaneEvent {
        PaneEvent::ExplicitStateReported {
            report: ExplicitStateReport {
                observed_at: 1,
                lifecycle,
                started_at: None,
                completed_at: None,
                prompt: None,
                tasks: None,
                subagents: None,
                attention: false,
            },
        }
    }

    #[test]
    fn initial_event_matrix_matches_missing_and_reset_rules() {
        let mut completing_report = match report(Some(ReportedLifecycle::Idle)) {
            PaneEvent::ExplicitStateReported { report } => report,
            _ => unreachable!(),
        };
        completing_report.completed_at = Some(9);
        let creating_events = vec![
            (
                "session",
                PaneEvent::AgentSessionStarted {
                    observed_at: 1,
                    source: AgentSessionSource::Startup,
                    resumed_prompt: None,
                },
                "idle",
                0,
            ),
            (
                "begin",
                PaneEvent::BeginRun {
                    started_at: 2,
                    prompt: None,
                },
                "running",
                0,
            ),
            (
                "activity",
                PaneEvent::ActivityObserved { observed_at: 3 },
                "running",
                0,
            ),
            (
                "wait",
                PaneEvent::WaitRequested {
                    observed_at: 4,
                    reason: WaitReason::PermissionPrompt,
                },
                "waiting",
                0,
            ),
            (
                "fail",
                PaneEvent::FailRun {
                    observed_at: 5,
                    reason: Some("failed".to_string()),
                },
                "error",
                0,
            ),
            (
                "running report",
                report(Some(ReportedLifecycle::Running)),
                "running",
                0,
            ),
            (
                "waiting report",
                report(Some(ReportedLifecycle::Waiting {
                    reason: WaitReason::PermissionPrompt,
                })),
                "waiting",
                0,
            ),
            (
                "error report",
                report(Some(ReportedLifecycle::Error { reason: None })),
                "error",
                0,
            ),
            (
                "completion",
                PaneEvent::CompleteRun { completed_at: 8 },
                "idle",
                1,
            ),
            (
                "completing idle report",
                PaneEvent::ExplicitStateReported {
                    report: completing_report,
                },
                "idle",
                1,
            ),
        ];

        for (name, event, expected_lifecycle, expected_completed) in creating_events {
            let tracker = CaptureTrackerSnapshot::default();
            let missing =
                reduce_explicit_once(None, "codex", "session-1", event.clone(), &tracker).unwrap();
            let state = active(&missing);
            assert_eq!(state.revision, 1, "{name}");
            assert_eq!(state.agent_epoch, 1, "{name}");
            assert!(state.agent_present, "{name}");
            assert!(!state.scan_verified, "{name}");
            assert!(!state.synthetic_completion_armed, "{name}");
            assert_eq!(state.completed_seq, expected_completed, "{name}");
            assert_eq!(
                match state.lifecycle {
                    LifecycleState::Idle => "idle",
                    LifecycleState::Running => "running",
                    LifecycleState::Waiting { .. } => "waiting",
                    LifecycleState::Error { .. } => "error",
                },
                expected_lifecycle,
                "{name}"
            );

            let reset = reset_record();
            let after_reset =
                reduce_explicit_once(Some(&reset), "codex", "session-1", event, &tracker).unwrap();
            if expected_completed == 1 {
                assert_eq!(after_reset.record, Some(reset), "{name}");
                assert_eq!(after_reset.outcome, ReductionOutcome::Noop, "{name}");
            } else {
                assert!(
                    matches!(after_reset.record, Some(StoredPaneRecord::Active(_))),
                    "{name}"
                );
            }
        }

        let non_creating = vec![
            PaneEvent::ProgressUpdated {
                observed_at: 1,
                operations: vec![ProgressOperation::ClearPrompt],
            },
            report(Some(ReportedLifecycle::Idle)),
            report(None),
        ];
        for event in non_creating {
            let tracker = CaptureTrackerSnapshot::default();
            let missing =
                reduce_explicit_once(None, "codex", "session-1", event.clone(), &tracker).unwrap();
            assert_eq!(missing.record, None);
            assert_eq!(missing.outcome, ReductionOutcome::Noop);
            let reset = reset_record();
            let after_reset =
                reduce_explicit_once(Some(&reset), "codex", "session-1", event, &tracker).unwrap();
            assert_eq!(after_reset.record, Some(reset));
            assert_eq!(after_reset.outcome, ReductionOutcome::Noop);
        }

        let discovered = discover("codex");
        assert!(active(&discovered).scan_verified);
        assert!(active(&discovered).synthetic_completion_armed);
        let reset = reset_record();
        let reset_discovered = observe(
            Some(&reset),
            &CaptureTrackerSnapshot::default(),
            AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
            None,
            1,
        );
        assert!(active(&reset_discovered).scan_verified);
        assert!(!active(&reset_discovered).synthetic_completion_armed);
    }

    #[test]
    fn explicit_identity_decision_table_rows_are_applied_in_priority_order() {
        let discovered = discover("codex");
        let discovered_tracker = discovered.tracker_delta.as_ref().unwrap().next.clone();
        let bound = reduce_explicit_once(
            discovered.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::BeginRun {
                started_at: 2,
                prompt: None,
            },
            &discovered_tracker,
        )
        .unwrap();
        let bound_tracker = bound.tracker_delta.as_ref().unwrap().next.clone();

        // Row 1: AgentSessionStarted is authoritative even for a mismatched identity.
        let authoritative = reduce_explicit_once(
            bound.record.as_ref(),
            "opencode",
            "session-2",
            PaneEvent::AgentSessionStarted {
                observed_at: 3,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
            &bound_tracker,
        )
        .unwrap();
        assert_eq!(active(&authoritative).agent.as_str(), "opencode");
        assert_eq!(active(&authoritative).agent_epoch, 2);
        assert!(!active(&authoritative).scan_verified);

        // Row 2: exact and present applies field-only progress normally.
        let exact = reduce_explicit_once(
            bound.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::ProgressUpdated {
                observed_at: 3,
                operations: vec![ProgressOperation::SetPrompt(PromptState {
                    text: "exact".to_string(),
                    source: "test".to_string(),
                })],
            },
            &bound_tracker,
        )
        .unwrap();
        assert_eq!(active(&exact).prompt.as_ref().unwrap().text, "exact");

        let absent_once = observe(
            bound.record.as_ref(),
            &bound_tracker,
            AgentPresenceObservation::Absent,
            None,
            4,
        );
        let absent = observe(
            absent_once.record.as_ref(),
            &absent_once.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Absent,
            None,
            5,
        );
        assert!(!active(&absent).agent_present);
        let absent_tracker = absent.tracker_delta.as_ref().unwrap().next.clone();

        // Row 3: exact and absent starts a new epoch only with epoch evidence.
        let restarted = reduce_explicit_once(
            absent.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::WaitRequested {
                observed_at: 6,
                reason: WaitReason::PermissionPrompt,
            },
            &absent_tracker,
        )
        .unwrap();
        assert_eq!(active(&restarted).agent_epoch, 2);
        assert!(matches!(
            active(&restarted).lifecycle,
            LifecycleState::Waiting { .. }
        ));
        assert!(!active(&restarted).scan_verified);
        let late_completion = reduce_explicit_once(
            absent.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::CompleteRun { completed_at: 6 },
            &absent_tracker,
        )
        .unwrap();
        assert!(!active(&late_completion).agent_present);
        assert_eq!(active(&late_completion).agent_epoch, 1);
        let absent_progress = reduce_explicit_once(
            absent.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::ProgressUpdated {
                observed_at: 6,
                operations: vec![ProgressOperation::ClearTasks],
            },
            &absent_tracker,
        );
        assert_eq!(absent_progress.unwrap_err(), ReduceError::StaleAgentEvent);
        assert_eq!(
            reduce_explicit_once(
                absent.record.as_ref(),
                "codex",
                "session-1",
                report(Some(ReportedLifecycle::Idle)),
                &absent_tracker,
            )
            .unwrap_err(),
            ReduceError::StaleAgentEvent
        );

        // Row 4: an unbound present scan epoch binds only on epoch evidence or armed completion.
        let unbound = discover("codex");
        let unbound_tracker = unbound.tracker_delta.as_ref().unwrap().next.clone();
        let bound_same_epoch = reduce_explicit_once(
            unbound.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::ActivityObserved { observed_at: 2 },
            &unbound_tracker,
        )
        .unwrap();
        assert_eq!(active(&bound_same_epoch).agent_epoch, 1);
        assert_eq!(
            active(&bound_same_epoch)
                .agent_session_id
                .as_ref()
                .unwrap()
                .as_str(),
            "session-1"
        );
        assert!(matches!(
            active(&bound_same_epoch).lifecycle,
            LifecycleState::Running
        ));
        let synthetic = reduce_explicit_once(
            unbound.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::CompleteRun { completed_at: 2 },
            &unbound_tracker,
        )
        .unwrap();
        assert_eq!(active(&synthetic).completed_seq, 1);
        assert!(active(&synthetic).agent_session_id.is_some());
        let ignored_progress = reduce_explicit_once(
            unbound.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::ProgressUpdated {
                observed_at: 2,
                operations: vec![ProgressOperation::TaskCreated],
            },
            &unbound_tracker,
        )
        .unwrap();
        assert!(active(&ignored_progress).agent_session_id.is_none());
        assert_eq!(active(&ignored_progress).tasks, TaskState::default());

        // Row 5: unbound and absent requires epoch evidence.
        let mut unbound_absent_record = unbound.record.clone().unwrap();
        let StoredPaneRecord::Active(unbound_absent) = &mut unbound_absent_record else {
            unreachable!();
        };
        unbound_absent.agent_present = false;
        unbound_absent.synthetic_completion_armed = false;
        let row5 = reduce_explicit_once(
            Some(&unbound_absent_record),
            "codex",
            "session-1",
            PaneEvent::FailRun {
                observed_at: 3,
                reason: Some("stopped".to_string()),
            },
            &unbound_tracker,
        )
        .unwrap();
        assert_eq!(active(&row5).agent_epoch, 2);
        assert!(matches!(
            active(&row5).lifecycle,
            LifecycleState::Error { .. }
        ));
        assert!(!active(&row5).scan_verified);
        assert_eq!(
            reduce_explicit_once(
                Some(&unbound_absent_record),
                "codex",
                "session-1",
                PaneEvent::CompleteRun { completed_at: 3 },
                &unbound_tracker,
            )
            .unwrap_err(),
            ReduceError::StaleAgentEvent
        );

        // Row 6: mismatched present is stale unless two matching scans authorize handover.
        assert_eq!(
            reduce_explicit_once(
                bound.record.as_ref(),
                "opencode",
                "session-2",
                PaneEvent::BeginRun {
                    started_at: 7,
                    prompt: None,
                },
                &bound_tracker,
            )
            .unwrap_err(),
            ReduceError::StaleAgentEvent
        );
        let unverified_current = begin(None, &CaptureTrackerSnapshot::default());
        assert!(!active(&unverified_current).scan_verified);
        let replacement_once = observe(
            unverified_current.record.as_ref(),
            &unverified_current.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            None,
            7,
        );
        let replacement_twice = observe(
            replacement_once.record.as_ref(),
            &replacement_once.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            None,
            8,
        );
        assert!(active(&replacement_twice).agent_present);
        assert_eq!(
            replacement_twice
                .tracker_delta
                .as_ref()
                .unwrap()
                .next
                .replacement_streak,
            2
        );
        let handover = reduce_explicit_once(
            replacement_twice.record.as_ref(),
            "opencode",
            "session-2",
            PaneEvent::BeginRun {
                started_at: 9,
                prompt: None,
            },
            &replacement_twice.tracker_delta.as_ref().unwrap().next,
        )
        .unwrap();
        assert_eq!(active(&handover).agent_epoch, 2);
        assert_eq!(active(&handover).agent.as_str(), "opencode");
        assert!(active(&handover).scan_verified);
        assert!(matches!(
            active(&handover).lifecycle,
            LifecycleState::Running
        ));

        // Row 7: mismatched absent accepts only epoch evidence.
        let row7 = reduce_explicit_once(
            absent.record.as_ref(),
            "opencode",
            "session-2",
            PaneEvent::BeginRun {
                started_at: 8,
                prompt: None,
            },
            &absent_tracker,
        )
        .unwrap();
        assert_eq!(active(&row7).agent_epoch, 2);
        assert_eq!(active(&row7).agent.as_str(), "opencode");
        assert!(!active(&row7).scan_verified);
        assert!(matches!(active(&row7).lifecycle, LifecycleState::Running));
        let row7_report = reduce_explicit_once(
            absent.record.as_ref(),
            "opencode",
            "session-2",
            report(Some(ReportedLifecycle::Error {
                reason: Some("reported".to_string()),
            })),
            &absent_tracker,
        )
        .unwrap();
        assert_eq!(active(&row7_report).agent_epoch, 2);
        assert!(matches!(
            active(&row7_report).lifecycle,
            LifecycleState::Error { .. }
        ));
        for stale in [
            PaneEvent::CompleteRun { completed_at: 8 },
            PaneEvent::ProgressUpdated {
                observed_at: 8,
                operations: vec![ProgressOperation::ClearTasks],
            },
        ] {
            assert_eq!(
                reduce_explicit_once(
                    absent.record.as_ref(),
                    "opencode",
                    "session-2",
                    stale,
                    &absent_tracker,
                )
                .unwrap_err(),
                ReduceError::StaleAgentEvent
            );
        }

        let authoritative_tracker = authoritative.tracker_delta.as_ref().unwrap().next.clone();
        assert_eq!(
            reduce_explicit_once(
                authoritative.record.as_ref(),
                "codex",
                "session-1",
                PaneEvent::BeginRun {
                    started_at: 9,
                    prompt: None,
                },
                &authoritative_tracker,
            )
            .unwrap_err(),
            ReduceError::StaleAgentEvent,
            "a delayed BeginRun from the previous session must not rewind identity"
        );
    }

    #[test]
    fn presence_matrix_converges_only_after_confirmed_absence() {
        let explicit = reduce_once(
            None,
            PaneEvent::AgentSessionStarted {
                observed_at: 1,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
            &CaptureTrackerSnapshot::default(),
        );
        assert!(!active(&explicit).scan_verified);
        let verified = observe(
            explicit.record.as_ref(),
            &explicit.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
            None,
            2,
        );
        assert!(active(&verified).scan_verified);
        assert_eq!(active(&verified).agent_epoch, 1);

        let first_other = observe(
            verified.record.as_ref(),
            &verified.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            Some(CaptureObservation {
                inference: CaptureInference::ActivityObserved,
                observed_fingerprint: Some([1; 32]),
            }),
            3,
        );
        assert_eq!(active(&first_other).agent.as_str(), "codex");
        assert!(active(&first_other).agent_present);
        let first_tracker = &first_other.tracker_delta.as_ref().unwrap().next;
        assert_eq!(first_tracker.absence_count, 1);
        assert_eq!(first_tracker.replacement_streak, 1);
        assert!(active(&first_other).started_at.is_none());

        let confirmed = observe(
            first_other.record.as_ref(),
            first_tracker,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            None,
            4,
        );
        assert_eq!(active(&confirmed).agent.as_str(), "codex");
        assert!(!active(&confirmed).agent_present);
        assert_eq!(active(&confirmed).agent_epoch, 1);
        assert_eq!(
            confirmed
                .tracker_delta
                .as_ref()
                .unwrap()
                .next
                .replacement_streak,
            2
        );

        let replacement = observe(
            confirmed.record.as_ref(),
            &confirmed.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            Some(CaptureObservation {
                inference: CaptureInference::ActivityObserved,
                observed_fingerprint: Some([2; 32]),
            }),
            5,
        );
        assert_eq!(active(&replacement).agent.as_str(), "opencode");
        assert_eq!(active(&replacement).agent_epoch, 2);
        assert!(active(&replacement).agent_present);
        assert!(active(&replacement).scan_verified);
        assert!(!active(&replacement).synthetic_completion_armed);
        assert!(matches!(
            active(&replacement).lifecycle,
            LifecycleState::Idle
        ));
        let replacement_tracker = &replacement.tracker_delta.as_ref().unwrap().next;
        assert_eq!(replacement_tracker.absence_count, 0);
        assert_eq!(replacement_tracker.replacement_kind, None);
        assert_eq!(replacement_tracker.fingerprint, Some([2; 32]));

        let rediscovered = observe(
            confirmed.record.as_ref(),
            &confirmed.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
            None,
            5,
        );
        assert_eq!(active(&rediscovered).agent.as_str(), "codex");
        assert_eq!(active(&rediscovered).agent_epoch, 2);
        assert!(active(&rediscovered).agent_session_id.is_none());
    }

    #[test]
    fn mixed_presence_sequences_follow_absence_and_replacement_tracker_table() {
        fn run_sequence(sequence: Vec<AgentPresenceObservation>) -> Reduction {
            let initial = discover("codex");
            let mut current = initial.record;
            let mut tracker = initial.tracker_delta.unwrap().next;
            let mut last = None;
            for (index, presence) in sequence.into_iter().enumerate() {
                let result = observe(current.as_ref(), &tracker, presence, None, index as i64 + 2);
                if let Some(delta) = &result.tracker_delta {
                    tracker = delta.next.clone();
                }
                current = result.record.clone();
                last = Some(result);
            }
            last.unwrap()
        }

        let absent_unknown_absent = run_sequence(vec![
            AgentPresenceObservation::Absent,
            AgentPresenceObservation::Unknown,
            AgentPresenceObservation::Absent,
        ]);
        assert!(active(&absent_unknown_absent).agent_present);
        let tracker = &absent_unknown_absent.tracker_delta.as_ref().unwrap().next;
        assert_eq!(tracker.absence_count, 1);
        assert_eq!(tracker.replacement_kind, None);

        let other_unknown_other = run_sequence(vec![
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            AgentPresenceObservation::Unknown,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
        ]);
        assert!(active(&other_unknown_other).agent_present);
        let tracker = &other_unknown_other.tracker_delta.as_ref().unwrap().next;
        assert_eq!(tracker.absence_count, 1);
        assert_eq!(tracker.replacement_streak, 1);

        let absent_then_other = run_sequence(vec![
            AgentPresenceObservation::Absent,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
        ]);
        assert!(!active(&absent_then_other).agent_present);
        let tracker = &absent_then_other.tracker_delta.as_ref().unwrap().next;
        assert_eq!(tracker.absence_count, 0);
        assert_eq!(tracker.replacement_streak, 1);

        let other_then_absent = run_sequence(vec![
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            AgentPresenceObservation::Absent,
        ]);
        assert!(!active(&other_then_absent).agent_present);
        let tracker = &other_then_absent.tracker_delta.as_ref().unwrap().next;
        assert_eq!(tracker.absence_count, 0);
        assert_eq!(tracker.replacement_kind, None);

        let unverified = reduce_once(
            None,
            PaneEvent::AgentSessionStarted {
                observed_at: 1,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
            &CaptureTrackerSnapshot::default(),
        );
        let first = observe(
            unverified.record.as_ref(),
            &unverified.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Absent,
            None,
            2,
        );
        let second = observe(
            first.record.as_ref(),
            &first.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Absent,
            None,
            3,
        );
        assert!(active(&second).agent_present);
        assert_eq!(second.tracker_delta.as_ref().unwrap().next.absence_count, 0);
    }

    #[test]
    fn explicit_activity_resets_presence_evidence_and_invalidates_old_poll() {
        let initial = discover("codex");
        let first_absence = observe(
            initial.record.as_ref(),
            &initial.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Absent,
            None,
            2,
        );
        let before_explicit = first_absence.tracker_delta.as_ref().unwrap().next.clone();
        assert_eq!(before_explicit.absence_count, 1);
        let old_base = first_absence.record.as_ref().unwrap().descriptor();
        let progress = reduce_explicit_once(
            first_absence.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::ProgressUpdated {
                observed_at: 3,
                operations: vec![ProgressOperation::ClearPrompt],
            },
            &before_explicit,
        )
        .unwrap();
        let after_explicit = progress.tracker_delta.as_ref().unwrap().next.clone();
        assert_eq!(after_explicit.absence_count, 0);
        assert_eq!(after_explicit.replacement_kind, None);
        assert!(after_explicit.generation > before_explicit.generation);

        let stale_poll = PaneEvent::ObservationBatch {
            base: Some(old_base),
            tracker_generation: before_explicit.generation,
            observed_at: 4,
            presence: AgentPresenceObservation::Absent,
            capture: None,
        };
        let rejected = reduce(
            progress.record.as_ref(),
            &observation_envelope(stale_poll),
            context(&after_explicit, &VisibilitySnapshot::default()),
        )
        .unwrap();
        assert_eq!(rejected.record, progress.record);
        assert_eq!(rejected.tracker_delta, None);
        assert_eq!(rejected.outcome, ReductionOutcome::Noop);

        let next_absence = observe(
            progress.record.as_ref(),
            &after_explicit,
            AgentPresenceObservation::Absent,
            None,
            5,
        );
        assert!(active(&next_absence).agent_present);
        assert_eq!(
            next_absence
                .tracker_delta
                .as_ref()
                .unwrap()
                .next
                .absence_count,
            1
        );
    }

    #[test]
    fn descriptor_and_tracker_boundaries_reject_stale_observations() {
        let initial = discover("codex");
        let initial_tracker = initial.tracker_delta.as_ref().unwrap().next.clone();
        let old_descriptor = initial.record.as_ref().unwrap().descriptor();
        let changed = reduce_explicit_once(
            initial.record.as_ref(),
            "codex",
            "session-1",
            PaneEvent::BeginRun {
                started_at: 2,
                prompt: None,
            },
            &initial_tracker,
        )
        .unwrap();
        let changed_tracker = changed.tracker_delta.as_ref().unwrap().next.clone();
        let stale_descriptor = PaneEvent::ObservationBatch {
            base: Some(old_descriptor),
            tracker_generation: changed_tracker.generation,
            observed_at: 3,
            presence: AgentPresenceObservation::Absent,
            capture: None,
        };
        let rejected = reduce(
            changed.record.as_ref(),
            &observation_envelope(stale_descriptor),
            context(&changed_tracker, &VisibilitySnapshot::default()),
        )
        .unwrap();
        assert_eq!(rejected.outcome, ReductionOutcome::Noop);
        assert_eq!(rejected.tracker_delta, None);

        let stale_missing = PaneEvent::ObservationBatch {
            base: None,
            tracker_generation: changed_tracker.generation,
            observed_at: 3,
            presence: AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            capture: None,
        };
        let rejected = reduce(
            changed.record.as_ref(),
            &observation_envelope(stale_missing),
            context(&changed_tracker, &VisibilitySnapshot::default()),
        )
        .unwrap();
        assert_eq!(rejected.record, changed.record);
        assert_eq!(rejected.tracker_delta, None);

        let stale_generation = PaneEvent::ObservationBatch {
            base: changed.record.as_ref().map(StoredPaneRecord::descriptor),
            tracker_generation: initial_tracker.generation,
            observed_at: 3,
            presence: AgentPresenceObservation::Absent,
            capture: None,
        };
        let rejected = reduce(
            changed.record.as_ref(),
            &observation_envelope(stale_generation),
            context(&changed_tracker, &VisibilitySnapshot::default()),
        )
        .unwrap();
        assert_eq!(rejected.outcome, ReductionOutcome::Noop);
        assert_eq!(rejected.tracker_delta, None);

        let reset_a = reset_record();
        let old_reset_descriptor = reset_a.descriptor();
        let reset_b = StoredPaneRecord::Reset(ResetTombstone {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            tombstone_id: ResetTombstoneId::parse(EVENT_ID).unwrap(),
            pane_instance: pane(),
            reset_at: 2,
        });
        let stale_reset = PaneEvent::ObservationBatch {
            base: Some(old_reset_descriptor),
            tracker_generation: 0,
            observed_at: 3,
            presence: AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
            capture: None,
        };
        let rejected = reduce(
            Some(&reset_b),
            &observation_envelope(stale_reset),
            context(
                &CaptureTrackerSnapshot::default(),
                &VisibilitySnapshot::default(),
            ),
        )
        .unwrap();
        assert_eq!(rejected.record, Some(reset_b));
        assert_eq!(rejected.tracker_delta, None);

        let stale_active_to_reset = PaneEvent::ObservationBatch {
            base: changed.record.as_ref().map(StoredPaneRecord::descriptor),
            tracker_generation: 0,
            observed_at: 3,
            presence: AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
            capture: None,
        };
        let rejected = reduce(
            Some(&reset_record()),
            &observation_envelope(stale_active_to_reset),
            context(
                &CaptureTrackerSnapshot::default(),
                &VisibilitySnapshot::default(),
            ),
        )
        .unwrap();
        assert_eq!(rejected.outcome, ReductionOutcome::Noop);
        assert_eq!(rejected.tracker_delta, None);
    }

    #[test]
    fn reset_recreation_and_new_epoch_clear_old_tracker_state() {
        let reset = reset_record();
        let dirty_tracker = CaptureTrackerSnapshot {
            generation: 7,
            epoch: Some((StateId::parse(OTHER_STATE_ID).unwrap(), 9)),
            absence_count: 1,
            replacement_kind: Some(AgentKind::parse("claude").unwrap()),
            replacement_streak: 2,
            fingerprint: Some([1; 32]),
            last_change_at: Some(1),
            rebaseline_pending: true,
        };
        let recreated = observe(
            Some(&reset),
            &dirty_tracker,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            Some(CaptureObservation {
                inference: CaptureInference::ActivityObserved,
                observed_fingerprint: Some([2; 32]),
            }),
            10,
        );
        let state = active(&recreated);
        let tracker = &recreated.tracker_delta.as_ref().unwrap().next;
        assert_eq!(
            tracker.epoch,
            Some((state.state_id.clone(), state.agent_epoch))
        );
        assert_eq!(tracker.generation, 8);
        assert_eq!(tracker.absence_count, 0);
        assert_eq!(tracker.replacement_kind, None);
        assert_eq!(tracker.replacement_streak, 0);
        assert_eq!(tracker.fingerprint, Some([2; 32]));
        assert_eq!(tracker.last_change_at, Some(10));
        assert!(!tracker.rebaseline_pending);
        assert!(matches!(state.lifecycle, LifecycleState::Idle));

        let mut dirty_again = tracker.clone();
        dirty_again.absence_count = 1;
        dirty_again.replacement_kind = Some(AgentKind::parse("claude").unwrap());
        dirty_again.replacement_streak = 2;
        dirty_again.fingerprint = Some([3; 32]);
        let new_epoch = reduce_explicit_once(
            recreated.record.as_ref(),
            "claude",
            "session-2",
            PaneEvent::AgentSessionStarted {
                observed_at: 11,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
            &dirty_again,
        )
        .unwrap();
        let tracker = &new_epoch.tracker_delta.as_ref().unwrap().next;
        assert_eq!(tracker.absence_count, 0);
        assert_eq!(tracker.replacement_kind, None);
        assert_eq!(tracker.replacement_streak, 0);
        assert_eq!(tracker.fingerprint, None);
        assert_eq!(tracker.last_change_at, None);
        assert!(!tracker.rebaseline_pending);
    }

    #[test]
    fn begin_complete_ack_focus_out_and_next_completion_is_monotonic() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        assert_eq!(resolve_badge(active(&begun)), BadgeState::Working);

        let completed = reduce_once(
            begun.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 20 },
            &begun.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(resolve_badge(active(&completed)), BadgeState::Done);

        let state = active(&completed);
        let acknowledged = reduce_once(
            completed.record.as_ref(),
            PaneEvent::AcknowledgeView {
                expected_state_id: state.state_id.clone(),
                expected_agent_epoch: state.agent_epoch,
                through_seq: state.completed_seq,
            },
            &completed.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(resolve_badge(active(&acknowledged)), BadgeState::Idle);

        let focus_out = Reduction::unchanged(acknowledged.record.as_ref());
        assert_eq!(resolve_badge(active(&focus_out)), BadgeState::Idle);

        let second_begin = begin(
            acknowledged.record.as_ref(),
            &completed.tracker_delta.as_ref().unwrap().next,
        );
        let second_done = reduce_once(
            second_begin.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 40 },
            &second_begin.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&second_done).run_seq, 2);
        assert_eq!(resolve_badge(active(&second_done)), BadgeState::Done);
    }

    #[test]
    fn duplicate_begin_and_complete_do_not_advance_sequences() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let duplicate = begin(
            begun.record.as_ref(),
            &begun.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&duplicate).run_seq, 1);
        let completed = reduce_once(
            duplicate.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 20 },
            &duplicate.tracker_delta.as_ref().unwrap().next,
        );
        let duplicate_complete = reduce_once(
            completed.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 30 },
            &completed.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&duplicate_complete).completed_seq, 1);
    }

    #[test]
    fn process_discovery_arms_only_first_synthetic_completion() {
        let tracker = CaptureTrackerSnapshot::default();
        let event = PaneEvent::ObservationBatch {
            base: None,
            tracker_generation: 0,
            observed_at: 1,
            presence: AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
            capture: None,
        };
        let mut observation = envelope(event);
        observation.agent = None;
        observation.agent_session_id = None;
        let discovered = reduce(
            None,
            &observation,
            context(&tracker, &VisibilitySnapshot::default()),
        )
        .unwrap();
        assert!(active(&discovered).synthetic_completion_armed);
        let completed = reduce_once(
            discovered.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 2 },
            &discovered.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&completed).completed_seq, 1);
        assert!(!active(&completed).synthetic_completion_armed);
        assert!(completed.tracker_delta.unwrap().next.rebaseline_pending);
    }

    #[test]
    fn explicit_session_start_does_not_arm_synthetic_completion() {
        let tracker = CaptureTrackerSnapshot::default();
        let started = reduce_once(
            None,
            PaneEvent::AgentSessionStarted {
                observed_at: 1,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
            &tracker,
        );
        let completion = reduce_once(
            started.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 2 },
            &started.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&completion).run_seq, 0);
        assert_eq!(resolve_badge(active(&completion)), BadgeState::Idle);
    }

    #[test]
    fn visible_completion_is_acknowledged_before_publication() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let visible = VisibilitySnapshot {
            pane_visible_to_eligible_client: true,
        };
        let result = reduce(
            begun.record.as_ref(),
            &envelope(PaneEvent::CompleteRun { completed_at: 20 }),
            context(&begun.tracker_delta.as_ref().unwrap().next, &visible),
        )
        .unwrap();
        assert_eq!(active(&result).acknowledged_seq, 1);
        assert_eq!(resolve_badge(active(&result)), BadgeState::Idle);
    }

    #[test]
    fn stale_ack_cannot_change_new_epoch() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let old = active(&begun).version();
        let restarted = reduce_once(
            begun.record.as_ref(),
            PaneEvent::AgentSessionStarted {
                observed_at: 2,
                source: AgentSessionSource::Resume,
                resumed_prompt: None,
            },
            &begun.tracker_delta.as_ref().unwrap().next,
        );
        let stale = reduce_once(
            restarted.record.as_ref(),
            PaneEvent::AcknowledgeView {
                expected_state_id: old.state_id,
                expected_agent_epoch: old.agent_epoch,
                through_seq: u64::MAX,
            },
            &restarted.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&stale).agent_epoch, 2);
        assert_eq!(active(&stale).acknowledged_seq, 0);
    }

    #[test]
    fn old_view_through_sequence_cannot_acknowledge_a_later_run() {
        let tracker = CaptureTrackerSnapshot::default();
        let first_begin = begin(None, &tracker);
        let first_complete = reduce_once(
            first_begin.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 2 },
            &first_begin.tracker_delta.as_ref().unwrap().next,
        );
        let first_version = active(&first_complete).version();
        let second_begin = reduce_once(
            first_complete.record.as_ref(),
            PaneEvent::BeginRun {
                started_at: 3,
                prompt: None,
            },
            &first_complete.tracker_delta.as_ref().unwrap().next,
        );
        let second_complete = reduce_once(
            second_begin.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 4 },
            &second_begin.tracker_delta.as_ref().unwrap().next,
        );
        let acknowledged = reduce_once(
            second_complete.record.as_ref(),
            PaneEvent::AcknowledgeView {
                expected_state_id: first_version.state_id,
                expected_agent_epoch: first_version.agent_epoch,
                through_seq: 1,
            },
            &second_complete.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&acknowledged).completed_seq, 2);
        assert_eq!(active(&acknowledged).acknowledged_seq, 1);
        assert_eq!(resolve_badge(active(&acknowledged)), BadgeState::Done);
    }

    #[test]
    fn two_consecutive_absences_confirm_agent_exit() {
        let tracker = CaptureTrackerSnapshot::default();
        let mut discovered = reduce_once(
            None,
            PaneEvent::BeginRun {
                started_at: 1,
                prompt: None,
            },
            &tracker,
        );
        if let StoredPaneRecord::Active(state) = discovered.record.as_mut().unwrap() {
            state.scan_verified = true;
        }
        let base = discovered.record.as_ref().unwrap().descriptor();
        let first_tracker = discovered.tracker_delta.as_ref().unwrap().next.clone();
        let first = reduce_once(
            discovered.record.as_ref(),
            PaneEvent::ObservationBatch {
                base: Some(base),
                tracker_generation: first_tracker.generation,
                observed_at: 2,
                presence: AgentPresenceObservation::Absent,
                capture: None,
            },
            &first_tracker,
        );
        assert!(active(&first).agent_present);
        let second_tracker = first.tracker_delta.as_ref().unwrap().next.clone();
        let second = reduce_once(
            first.record.as_ref(),
            PaneEvent::ObservationBatch {
                base: Some(first.record.as_ref().unwrap().descriptor()),
                tracker_generation: second_tracker.generation,
                observed_at: 3,
                presence: AgentPresenceObservation::Absent,
                capture: None,
            },
            &second_tracker,
        );
        assert!(!active(&second).agent_present);
        assert_eq!(resolve_badge(active(&second)), BadgeState::Done);
    }

    #[test]
    fn stale_agent_session_is_rejected() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let mut stale = envelope(PaneEvent::CompleteRun { completed_at: 2 });
        stale.agent_session_id = Some(AgentSessionId::parse("old-session").unwrap());
        let error = reduce(
            begun.record.as_ref(),
            &stale,
            context(
                &begun.tracker_delta.as_ref().unwrap().next,
                &VisibilitySnapshot::default(),
            ),
        )
        .unwrap_err();
        assert_eq!(error, ReduceError::StaleAgentEvent);
    }

    #[test]
    fn waiting_error_and_running_override_unread_badge_priority() {
        let tracker = CaptureTrackerSnapshot::default();
        let waiting = reduce_once(
            None,
            PaneEvent::WaitRequested {
                observed_at: 1,
                reason: WaitReason::PermissionPrompt,
            },
            &tracker,
        );
        assert_eq!(resolve_badge(active(&waiting)), BadgeState::Blocked);
        let completed = reduce_once(
            waiting.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 2 },
            &waiting.tracker_delta.as_ref().unwrap().next,
        );
        let running = reduce_once(
            completed.record.as_ref(),
            PaneEvent::BeginRun {
                started_at: 3,
                prompt: None,
            },
            &completed.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&running).acknowledged_seq, 0);
        assert_eq!(resolve_badge(active(&running)), BadgeState::Working);
        let failed = reduce_once(
            running.record.as_ref(),
            PaneEvent::FailRun {
                observed_at: 4,
                reason: Some("failed".to_string()),
            },
            &running.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(resolve_badge(active(&failed)), BadgeState::Blocked);
    }

    #[test]
    fn resume_starts_new_epoch_and_only_resume_keeps_prompt() {
        let tracker = CaptureTrackerSnapshot::default();
        let started = reduce_once(
            None,
            PaneEvent::AgentSessionStarted {
                observed_at: 1,
                source: AgentSessionSource::Startup,
                resumed_prompt: None,
            },
            &tracker,
        );
        let prompt = PromptState {
            text: "continue".to_string(),
            source: "transcript".to_string(),
        };
        let resumed = reduce_once(
            started.record.as_ref(),
            PaneEvent::AgentSessionStarted {
                observed_at: 2,
                source: AgentSessionSource::Resume,
                resumed_prompt: Some(prompt.clone()),
            },
            &started.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&resumed).agent_epoch, 2);
        assert_eq!(active(&resumed).prompt.as_ref(), Some(&prompt));
        let cleared = reduce_once(
            resumed.record.as_ref(),
            PaneEvent::AgentSessionStarted {
                observed_at: 3,
                source: AgentSessionSource::Clear,
                resumed_prompt: Some(prompt),
            },
            &resumed.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(active(&cleared).agent_epoch, 3);
        assert!(active(&cleared).prompt.is_none());
    }

    #[test]
    fn progress_operations_are_atomic_and_derive_item_counts() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let updated = reduce_once(
            begun.record.as_ref(),
            PaneEvent::ProgressUpdated {
                observed_at: 2,
                operations: vec![
                    ProgressOperation::UpsertTaskItem {
                        id: "one".to_string(),
                        step: "first".to_string(),
                    },
                    ProgressOperation::UpdateTaskItemStatus {
                        id: "one".to_string(),
                        status: TaskItemStatus::Completed,
                    },
                    ProgressOperation::UpsertSubagent(SubagentState {
                        agent_id: "worker-1".to_string(),
                        agent_type: "review".to_string(),
                        display_name: Some("Reviewer".to_string()),
                    }),
                ],
            },
            &begun.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(
            active(&updated).tasks.progress,
            TaskProgress { done: 1, total: 1 }
        );
        assert_eq!(active(&updated).subagents.len(), 1);
    }

    #[test]
    fn every_progress_operation_variant_has_the_specified_effect() {
        let begun = begin(None, &CaptureTrackerSnapshot::default());

        let prompted = progress(
            &begun,
            vec![ProgressOperation::SetPrompt(PromptState {
                text: "prompt".to_string(),
                source: "test".to_string(),
            })],
        );
        assert_eq!(active(&prompted).prompt.as_ref().unwrap().text, "prompt");
        let prompt_cleared = progress(&prompted, vec![ProgressOperation::ClearPrompt]);
        assert!(active(&prompt_cleared).prompt.is_none());

        let created = progress(&prompt_cleared, vec![ProgressOperation::TaskCreated]);
        assert_eq!(
            active(&created).tasks.progress,
            TaskProgress { done: 0, total: 1 }
        );
        let completed = progress(&created, vec![ProgressOperation::TaskCompleted]);
        assert_eq!(
            active(&completed).tasks.progress,
            TaskProgress { done: 1, total: 1 }
        );

        let replaced = progress(
            &completed,
            vec![ProgressOperation::ReplaceTasks {
                progress: TaskProgress {
                    done: 99,
                    total: 100,
                },
                items: vec![
                    TaskItemState {
                        id: Some("one".to_string()),
                        step: "first".to_string(),
                        status: TaskItemStatus::Completed,
                    },
                    TaskItemState {
                        id: Some("two".to_string()),
                        step: "second".to_string(),
                        status: TaskItemStatus::Pending,
                    },
                ],
            }],
        );
        assert_eq!(
            active(&replaced).tasks.progress,
            TaskProgress { done: 1, total: 2 },
            "non-empty items derive progress instead of trusting payload counts"
        );
        let upserted = progress(
            &replaced,
            vec![
                ProgressOperation::UpsertTaskItem {
                    id: "one".to_string(),
                    step: "first revised".to_string(),
                },
                ProgressOperation::UpsertTaskItem {
                    id: "three".to_string(),
                    step: "third".to_string(),
                },
            ],
        );
        assert_eq!(active(&upserted).tasks.items.len(), 3);
        assert_eq!(active(&upserted).tasks.items[0].step, "first revised");
        assert_eq!(
            active(&upserted).tasks.items[0].status,
            TaskItemStatus::Pending
        );
        assert_eq!(
            active(&upserted).tasks.progress,
            TaskProgress { done: 0, total: 3 }
        );
        let status_updated = progress(
            &upserted,
            vec![ProgressOperation::UpdateTaskItemStatus {
                id: "two".to_string(),
                status: TaskItemStatus::Completed,
            }],
        );
        assert_eq!(
            active(&status_updated).tasks.progress,
            TaskProgress { done: 1, total: 3 }
        );
        let tasks_cleared = progress(&status_updated, vec![ProgressOperation::ClearTasks]);
        assert_eq!(active(&tasks_cleared).tasks, TaskState::default());

        let subagent = SubagentState {
            agent_id: "worker-1".to_string(),
            agent_type: "review".to_string(),
            display_name: Some("Reviewer".to_string()),
        };
        let subagent_upserted = progress(
            &tasks_cleared,
            vec![ProgressOperation::UpsertSubagent(subagent.clone())],
        );
        let mut revised_subagent = subagent;
        revised_subagent.display_name = Some("Revised".to_string());
        let subagent_revised = progress(
            &subagent_upserted,
            vec![ProgressOperation::UpsertSubagent(revised_subagent)],
        );
        assert_eq!(active(&subagent_revised).subagents.len(), 1);
        assert_eq!(
            active(&subagent_revised).subagents[0]
                .display_name
                .as_deref(),
            Some("Revised")
        );
        let subagents_replaced = progress(
            &subagent_revised,
            vec![ProgressOperation::ReplaceSubagents(vec![
                SubagentState {
                    agent_id: "worker-2".to_string(),
                    agent_type: "test".to_string(),
                    display_name: None,
                },
                SubagentState {
                    agent_id: "worker-3".to_string(),
                    agent_type: "docs".to_string(),
                    display_name: None,
                },
            ])],
        );
        assert_eq!(active(&subagents_replaced).subagents.len(), 2);
        let subagent_removed = progress(
            &subagents_replaced,
            vec![ProgressOperation::RemoveSubagent {
                agent_id: "worker-2".to_string(),
            }],
        );
        assert_eq!(active(&subagent_removed).subagents.len(), 1);
        assert_eq!(active(&subagent_removed).subagents[0].agent_id, "worker-3");
        let subagents_cleared =
            progress(&subagent_removed, vec![ProgressOperation::ClearSubagents]);
        assert!(active(&subagents_cleared).subagents.is_empty());

        let activity = WorktreeActivity {
            kind: WorktreeActivityKind::VwExec,
            name: "pane-state".to_string(),
            path: "/tmp/pane-state".to_string(),
            command: "cargo test".to_string(),
            observed_at: 21,
        };
        let activity_set = progress(
            &subagents_cleared,
            vec![ProgressOperation::SetWorktreeActivity(activity.clone())],
        );
        assert_eq!(active(&activity_set).worktree_activity, Some(activity));
        let activity_cleared = progress(
            &activity_set,
            vec![ProgressOperation::ClearWorktreeActivity],
        );
        assert!(active(&activity_cleared).worktree_activity.is_none());
    }

    #[test]
    fn progress_batch_is_atomic_when_a_later_operation_is_invalid() {
        let begun = begin(None, &CaptureTrackerSnapshot::default());
        let before = begun.record.clone();
        let error = reduce(
            begun.record.as_ref(),
            &envelope(PaneEvent::ProgressUpdated {
                observed_at: 2,
                operations: vec![
                    ProgressOperation::TaskCreated,
                    ProgressOperation::TaskCompleted,
                    ProgressOperation::TaskCompleted,
                ],
            }),
            context(
                &begun.tracker_delta.as_ref().unwrap().next,
                &VisibilitySnapshot::default(),
            ),
        )
        .unwrap_err();
        assert!(matches!(error, ReduceError::InvalidProgressOperation(_)));
        assert_eq!(begun.record, before);
    }

    #[test]
    fn opencode_capture_activity_starts_running_after_baseline() {
        let baseline = observe(
            None,
            &CaptureTrackerSnapshot::default(),
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            Some(CaptureObservation {
                inference: CaptureInference::NoChange,
                observed_fingerprint: Some([1; 32]),
            }),
            1,
        );
        assert!(matches!(active(&baseline).lifecycle, LifecycleState::Idle));
        let activity = observe(
            baseline.record.as_ref(),
            &baseline.tracker_delta.as_ref().unwrap().next,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap()),
            Some(CaptureObservation {
                inference: CaptureInference::ActivityObserved,
                observed_fingerprint: Some([2; 32]),
            }),
            2,
        );
        assert!(matches!(
            active(&activity).lifecycle,
            LifecycleState::Running
        ));
        assert_eq!(active(&activity).run_seq, 1);
        assert_eq!(active(&activity).started_at, Some(2));
        assert_eq!(
            activity.tracker_delta.as_ref().unwrap().next.fingerprint,
            Some([2; 32])
        );
    }

    #[test]
    fn checked_counter_overflow_is_fatal() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let mut overflow = active(&begun).clone();
        overflow.lifecycle = LifecycleState::Idle;
        overflow.run_seq = u64::MAX;
        overflow.completed_seq = u64::MAX;
        overflow.acknowledged_seq = u64::MAX;
        overflow.completed_at = Some(2);
        let record = StoredPaneRecord::Active(overflow);
        let error = reduce(
            Some(&record),
            &envelope(PaneEvent::BeginRun {
                started_at: 3,
                prompt: None,
            }),
            context(&tracker, &VisibilitySnapshot::default()),
        )
        .unwrap_err();
        assert_eq!(error, ReduceError::CounterOverflow("run sequence"));
    }

    #[test]
    fn unknown_observation_breaks_absence_streak() {
        let tracker = CaptureTrackerSnapshot::default();
        let mut begun = begin(None, &tracker);
        let StoredPaneRecord::Active(state) = begun.record.as_mut().unwrap() else {
            unreachable!();
        };
        state.scan_verified = true;
        let mut current = begun.record;
        let mut current_tracker = begun.tracker_delta.unwrap().next;
        for presence in [
            AgentPresenceObservation::Absent,
            AgentPresenceObservation::Unknown,
            AgentPresenceObservation::Absent,
        ] {
            let result = reduce_once(
                current.as_ref(),
                PaneEvent::ObservationBatch {
                    base: Some(current.as_ref().unwrap().descriptor()),
                    tracker_generation: current_tracker.generation,
                    observed_at: 2,
                    presence,
                    capture: None,
                },
                &current_tracker,
            );
            current = result.record;
            current_tracker = result.tracker_delta.unwrap().next;
        }
        let StoredPaneRecord::Active(state) = current.unwrap() else {
            unreachable!();
        };
        assert!(state.agent_present);
        assert_eq!(current_tracker.absence_count, 1);
    }

    #[test]
    fn completion_requests_capture_rebaseline() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let completed = reduce_once(
            begun.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 2 },
            &begun.tracker_delta.as_ref().unwrap().next,
        );
        assert!(completed.tracker_delta.unwrap().next.rebaseline_pending);
    }

    #[test]
    fn empty_capture_tail_does_not_clear_rebaseline() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let completed = reduce_once(
            begun.record.as_ref(),
            PaneEvent::CompleteRun { completed_at: 2 },
            &begun.tracker_delta.as_ref().unwrap().next,
        );
        let pending = completed.tracker_delta.as_ref().unwrap().next.clone();
        let result = reduce_once(
            completed.record.as_ref(),
            PaneEvent::ObservationBatch {
                base: Some(completed.record.as_ref().unwrap().descriptor()),
                tracker_generation: pending.generation,
                observed_at: 3,
                presence: AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
                capture: Some(CaptureObservation {
                    inference: CaptureInference::NoChange,
                    observed_fingerprint: None,
                }),
            },
            &pending,
        );
        assert!(result.tracker_delta.unwrap().next.rebaseline_pending);
    }

    #[test]
    fn stale_capture_completion_uses_current_capture_as_baseline() {
        let tracker = CaptureTrackerSnapshot::default();
        let mut begun = begin(None, &tracker);
        let StoredPaneRecord::Active(state) = begun.record.as_mut().unwrap() else {
            unreachable!();
        };
        state.scan_verified = true;
        let mut capture_tracker = begun.tracker_delta.unwrap().next;
        capture_tracker.fingerprint = Some([1; 32]);
        let result = reduce_once(
            begun.record.as_ref(),
            PaneEvent::ObservationBatch {
                base: Some(begun.record.as_ref().unwrap().descriptor()),
                tracker_generation: capture_tracker.generation,
                observed_at: 301,
                presence: AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap()),
                capture: Some(CaptureObservation {
                    inference: CaptureInference::StaleRunCompleted,
                    observed_fingerprint: Some([2; 32]),
                }),
            },
            &capture_tracker,
        );
        let next_tracker = result.tracker_delta.as_ref().unwrap().next.clone();
        assert!(!next_tracker.rebaseline_pending);
        assert_eq!(next_tracker.fingerprint, Some([2; 32]));
        assert_eq!(resolve_badge(active(&result)), BadgeState::Done);
    }

    #[test]
    fn invalid_progress_payload_has_specific_error() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let error = reduce(
            begun.record.as_ref(),
            &envelope(PaneEvent::ProgressUpdated {
                observed_at: 2,
                operations: vec![ProgressOperation::UpsertTaskItem {
                    id: String::new(),
                    step: "step".to_string(),
                }],
            }),
            context(
                &begun.tracker_delta.as_ref().unwrap().next,
                &VisibilitySnapshot::default(),
            ),
        )
        .unwrap_err();
        assert!(matches!(error, ReduceError::InvalidProgressOperation(_)));
    }

    #[test]
    fn explicit_event_reinitializes_tracker_for_hydrated_epoch() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let result = begin(begun.record.as_ref(), &CaptureTrackerSnapshot::default());
        let state = active(&result);
        assert_eq!(
            result.tracker_delta.as_ref().unwrap().next.epoch,
            Some((state.state_id.clone(), state.agent_epoch))
        );
    }

    #[test]
    fn reset_rejects_delayed_completion() {
        let tracker = CaptureTrackerSnapshot::default();
        let reset = reset_record();
        let visibility = VisibilitySnapshot::default();
        for event in [
            PaneEvent::CompleteRun { completed_at: 2 },
            PaneEvent::ExplicitStateReported {
                report: ExplicitStateReport {
                    observed_at: 2,
                    lifecycle: Some(ReportedLifecycle::Idle),
                    started_at: None,
                    completed_at: Some(2),
                    prompt: None,
                    tasks: None,
                    subagents: None,
                    attention: false,
                },
            },
        ] {
            let result = reduce(
                Some(&reset),
                &envelope(event),
                ReductionContext {
                    done_clear_on: crate::config::DoneClearOn::Pane,
                    visibility: &visibility,
                    tracker: &tracker,
                    new_state_id: None,
                },
            )
            .expect("delayed completion after reset must not require a state ID");
            assert_eq!(result.record, Some(reset.clone()));
            assert_eq!(result.outcome, ReductionOutcome::Noop);
        }
    }

    #[test]
    fn mark_done_uses_full_version_guard() {
        let tracker = CaptureTrackerSnapshot::default();
        let begun = begin(None, &tracker);
        let expected = active(&begun).version();
        let done = reduce_once(
            begun.record.as_ref(),
            PaneEvent::MarkDone {
                expected: expected.clone(),
                completed_at: 2,
            },
            &begun.tracker_delta.as_ref().unwrap().next,
        );
        assert_eq!(resolve_badge(active(&done)), BadgeState::Done);
        let error = reduce(
            done.record.as_ref(),
            &envelope(PaneEvent::MarkDone {
                expected,
                completed_at: 3,
            }),
            context(
                &begun.tracker_delta.as_ref().unwrap().next,
                &VisibilitySnapshot::default(),
            ),
        )
        .unwrap_err();
        assert_eq!(error, ReduceError::StaleSelection);
    }

    proptest! {
        #[test]
        fn valid_begin_complete_ack_sequences_preserve_invariants(
            operations in prop::collection::vec(0_u8..11, 0..128),
        ) {
            let mut current: Option<StoredPaneRecord> = None;
            let mut tracker = CaptureTrackerSnapshot::default();
            for (index, operation) in operations.into_iter().enumerate() {
                let timestamp = index as i64 + 1;
                let event = match operation {
                    0 => PaneEvent::BeginRun { started_at: timestamp, prompt: None },
                    1 => PaneEvent::ActivityObserved { observed_at: timestamp },
                    2 => PaneEvent::WaitRequested {
                        observed_at: timestamp,
                        reason: WaitReason::PermissionPrompt,
                    },
                    3 => PaneEvent::FailRun {
                        observed_at: timestamp,
                        reason: Some("generated".to_string()),
                    },
                    4 => PaneEvent::CompleteRun { completed_at: timestamp },
                    5 => {
                        let Some(StoredPaneRecord::Active(state)) = current.as_ref() else {
                            continue;
                        };
                        PaneEvent::AcknowledgeView {
                            expected_state_id: state.state_id.clone(),
                            expected_agent_epoch: state.agent_epoch,
                            through_seq: state.completed_seq,
                        }
                    },
                    6 => PaneEvent::ProgressUpdated {
                        observed_at: timestamp,
                        operations: vec![ProgressOperation::SetPrompt(PromptState {
                            text: format!("prompt-{timestamp}"),
                            source: "property".to_string(),
                        })],
                    },
                    7 => PaneEvent::ProgressUpdated {
                        observed_at: timestamp,
                        operations: vec![
                            ProgressOperation::ClearTasks,
                            ProgressOperation::ClearSubagents,
                            ProgressOperation::ClearWorktreeActivity,
                        ],
                    },
                    8 => PaneEvent::AgentSessionStarted {
                        observed_at: timestamp,
                        source: AgentSessionSource::Startup,
                        resumed_prompt: None,
                    },
                    9 => report(Some(ReportedLifecycle::Running)),
                    _ => {
                        let Some(StoredPaneRecord::Active(state)) = current.as_ref() else {
                            continue;
                        };
                        PaneEvent::MarkDone {
                            expected: state.version(),
                            completed_at: timestamp,
                        }
                    },
                };
                let previous = current.clone();
                let result = reduce(
                    current.as_ref(),
                    &envelope(event),
                    context(&tracker, &VisibilitySnapshot::default()),
                ).expect("generated explicit event sequence must be reducible");
                if let Some(delta) = result.tracker_delta {
                    tracker = delta.next;
                }
                current = result.record;
                if let Some(StoredPaneRecord::Active(state)) = current.as_ref() {
                    prop_assert!(state.validate().is_ok());
                    prop_assert!(state.run_seq - state.completed_seq <= 1);
                    if let Some(StoredPaneRecord::Active(previous)) = previous.as_ref() {
                        if previous.state_id == state.state_id {
                            prop_assert!(state.revision >= previous.revision);
                        }
                        if previous.state_id == state.state_id
                            && previous.agent_epoch == state.agent_epoch
                        {
                            prop_assert!(state.run_seq >= previous.run_seq);
                            prop_assert!(state.completed_seq >= previous.completed_seq);
                            prop_assert!(state.acknowledged_seq >= previous.acknowledged_seq);
                        }
                    }
                }
            }
        }
    }
}

use std::collections::BTreeSet;

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::detect::{detect_codex_wait_reason, detect_usage_limit};
use crate::pane_state::{
    AgentKind, AgentPresenceObservation, CaptureInference, CaptureObservation,
    CaptureTrackerSnapshot, DaemonInstanceId, EventId, LifecycleState, ObservationDispatchSnapshot,
    PaneEvent, PaneEventEnvelope, PaneInstance, PaneState, StoredStateDescriptor, WaitReason,
};

use super::capture::{CaptureBatchError, CaptureSource};
use super::process::AgentProcessSnapshot;

pub const STALE_CAPTURE_SECONDS: i64 = 300;
pub const USAGE_LIMIT_CAPTURE_INTERVAL_SECONDS: i64 = 5;

pub fn classify_presence(
    current: Option<&PaneState>,
    detected_agents: &BTreeSet<AgentKind>,
    scan_complete: bool,
) -> AgentPresenceObservation {
    if !scan_complete {
        return AgentPresenceObservation::Unknown;
    }
    if let Some(current) = current
        && detected_agents.contains(&current.agent)
    {
        return AgentPresenceObservation::Present(current.agent.clone());
    }
    match detected_agents.len() {
        0 => match current {
            Some(state) if !supports_process_detection(&state.agent) || !state.scan_verified => {
                AgentPresenceObservation::Unknown
            }
            _ => AgentPresenceObservation::Absent,
        },
        1 => AgentPresenceObservation::Present(
            detected_agents
                .iter()
                .next()
                .expect("one detected agent")
                .clone(),
        ),
        _ => AgentPresenceObservation::Unknown,
    }
}

pub fn infer_capture(
    state: Option<&PaneState>,
    tracker: &CaptureTrackerSnapshot,
    tail: &str,
    observed_at: i64,
) -> CaptureObservation {
    let observed_fingerprint = capture_sha256(tail);
    let inference = if state.is_some_and(|state| {
        matches!(state.agent.as_str(), "claude" | "codex") && detect_usage_limit(tail)
    }) {
        CaptureInference::UsageLimit
    } else if observed_fingerprint.is_none()
        || tracker.rebaseline_pending
        || tracker.fingerprint.is_none()
    {
        CaptureInference::NoChange
    } else if let Some(reason) = detect_codex_wait_reason(tail) {
        CaptureInference::PermissionWait {
            reason: if reason == "permission_prompt" {
                WaitReason::PermissionPrompt
            } else {
                WaitReason::Other(reason.to_string())
            },
        }
    } else if observed_fingerprint != tracker.fingerprint {
        CaptureInference::ActivityObserved
    } else if state.is_some_and(|state| {
        matches!(state.lifecycle, LifecycleState::Running)
            && observed_at.saturating_sub(
                state
                    .started_at
                    .into_iter()
                    .chain(tracker.last_change_at)
                    .max()
                    .unwrap_or(observed_at),
            ) >= STALE_CAPTURE_SECONDS
    }) {
        CaptureInference::StaleRunCompleted
    } else {
        CaptureInference::NoChange
    };
    CaptureObservation {
        inference,
        observed_fingerprint,
    }
}

fn infer_usage_limit_capture(tail: &str) -> CaptureObservation {
    CaptureObservation {
        inference: if detect_usage_limit(tail) {
            CaptureInference::UsageLimit
        } else {
            CaptureInference::NoChange
        },
        observed_fingerprint: capture_sha256(tail),
    }
}

pub fn capture_sha256(tail: &str) -> Option<[u8; 32]> {
    if tail.trim().is_empty() {
        return None;
    }
    Some(Sha256::digest(tail.as_bytes()).into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationSample {
    pub observed_at: i64,
    pub presence: AgentPresenceObservation,
    pub capture: Option<CaptureObservation>,
    pub process: Option<crate::pane_state::ProcessObservation>,
}

pub fn observation_envelope(
    daemon_instance_id: DaemonInstanceId,
    pane_instance: PaneInstance,
    base: Option<StoredStateDescriptor>,
    tracker: &CaptureTrackerSnapshot,
    sample: ObservationSample,
) -> Result<PaneEventEnvelope> {
    let capture = (!matches!(sample.presence, AgentPresenceObservation::Unknown))
        .then_some(sample.capture)
        .flatten();
    Ok(PaneEventEnvelope {
        daemon_instance_id,
        event_id: EventId::generate()?,
        pane_instance,
        agent: None,
        agent_session_id: None,
        event: PaneEvent::ObservationBatch {
            base,
            tracker_generation: tracker.generation,
            observed_at: sample.observed_at,
            presence: sample.presence,
            capture,
            process: sample.process,
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationPollResult {
    pub envelopes: Vec<PaneEventEnvelope>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationPollError {
    UnverifiedServerIdentity(CaptureBatchError),
    Event(String),
}

impl std::fmt::Display for ObservationPollError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnverifiedServerIdentity(error) => write!(formatter, "{error}"),
            Self::Event(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ObservationPollError {}

impl ObservationPollError {
    pub fn requires_daemon_exit(&self) -> bool {
        matches!(self, Self::UnverifiedServerIdentity(_))
    }
}

pub fn run_observation_poll(
    source: &dyn CaptureSource,
    dispatch: &[ObservationDispatchSnapshot],
    processes: &AgentProcessSnapshot,
    daemon_instance_id: &DaemonInstanceId,
    observed_at: i64,
) -> std::result::Result<ObservationPollResult, ObservationPollError> {
    let mut diagnostics = Vec::new();
    let detections = dispatch
        .iter()
        .map(|snapshot| {
            let detection = processes.detect_from_pid_tree(snapshot.pane_instance.pane_pid);
            if detection.complete && detection.agents.len() > 1 {
                diagnostics.push(format!(
                    "ambiguous_agent_processes: {}",
                    snapshot.pane_instance.pane_id
                ));
            }
            detection
        })
        .collect::<Vec<_>>();
    let observations = dispatch
        .iter()
        .zip(&detections)
        .map(|(snapshot, detection)| {
            Some(classify_presence(
                snapshot.state.as_ref(),
                &detection.agents,
                detection.complete,
            ))
        })
        .collect::<Vec<_>>();
    let capture_modes = observations
        .iter()
        .enumerate()
        .map(|(index, presence)| {
            presence
                .as_ref()
                .and_then(|presence| capture_mode(&dispatch[index], presence, observed_at))
        })
        .collect::<Vec<_>>();
    let capture_indices = capture_modes
        .iter()
        .enumerate()
        .filter_map(|(index, mode)| mode.is_some().then_some(index))
        .collect::<Vec<_>>();
    let capture_panes = capture_indices
        .iter()
        .map(|index| dispatch[*index].pane_instance.clone())
        .collect::<Vec<_>>();
    let mut tails_by_index = vec![None; dispatch.len()];
    if !capture_panes.is_empty() {
        match source.capture_plain_tails(&capture_panes) {
            Ok(tails) => {
                if tails.len() == capture_indices.len() {
                    for (index, tail) in capture_indices.into_iter().zip(tails) {
                        tails_by_index[index] = Some(tail);
                    }
                } else {
                    diagnostics.push(format!(
                        "capture_batch_discarded: {}",
                        CaptureBatchError::DelimiterMismatch {
                            expected: capture_indices.len(),
                            actual: tails.len(),
                        }
                    ));
                }
            }
            Err(error @ CaptureBatchError::InvalidIdentityHeader)
            | Err(error @ CaptureBatchError::IdentityMismatch { .. }) => {
                return Err(ObservationPollError::UnverifiedServerIdentity(error));
            }
            Err(error) => diagnostics.push(format!("capture_batch_discarded: {error}")),
        }
    }
    let mut envelopes = Vec::new();
    for (index, (snapshot, presence)) in dispatch.iter().zip(observations).enumerate() {
        let Some(presence) = presence else {
            continue;
        };
        let capture = tails_by_index[index].as_deref().map(|tail| {
            match capture_modes[index].expect("a captured tail has an inference mode") {
                CaptureMode::FullInference => infer_capture(
                    snapshot.state.as_ref(),
                    &snapshot.tracker,
                    tail,
                    observed_at,
                ),
                CaptureMode::UsageLimitOnly => infer_usage_limit_capture(tail),
            }
        });
        let process = processes.process_observation(
            snapshot.pane_instance.pane_pid,
            snapshot
                .state
                .as_ref()
                .and_then(|state| state.background_process.as_ref())
                .map(|process| process.command.as_str()),
            matches!(presence, AgentPresenceObservation::Present(_)),
            match &presence {
                AgentPresenceObservation::Present(agent) => {
                    detections[index].exact_agent_process(agent)
                }
                AgentPresenceObservation::Absent | AgentPresenceObservation::Unknown => None,
            },
        );
        envelopes.push(
            observation_envelope(
                daemon_instance_id.clone(),
                snapshot.pane_instance.clone(),
                snapshot.base.clone(),
                &snapshot.tracker,
                ObservationSample {
                    observed_at,
                    presence,
                    capture,
                    process,
                },
            )
            .map_err(|error| ObservationPollError::Event(error.to_string()))?,
        );
    }
    Ok(ObservationPollResult {
        envelopes,
        diagnostics,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    FullInference,
    UsageLimitOnly,
}

fn capture_mode(
    snapshot: &ObservationDispatchSnapshot,
    presence: &AgentPresenceObservation,
    observed_at: i64,
) -> Option<CaptureMode> {
    let fallback_needed = match presence {
        AgentPresenceObservation::Present(observed_agent) => {
            snapshot.state.as_ref().is_none_or(|state| {
                let tracker_matches_epoch =
                    snapshot
                        .tracker
                        .epoch
                        .as_ref()
                        .is_some_and(|(state_id, agent_epoch)| {
                            state_id == &state.state_id && *agent_epoch == state.agent_epoch
                        });
                !state.agent_present
                    || &state.agent != observed_agent
                    || !tracker_matches_epoch
                    || !snapshot.tracker.hook_authoritative
            })
        }
        AgentPresenceObservation::Absent | AgentPresenceObservation::Unknown => false,
    };
    if fallback_needed {
        return Some(CaptureMode::FullInference);
    }

    let state = snapshot.state.as_ref()?;
    if !matches!(state.agent.as_str(), "claude" | "codex")
        || state.run_seq == state.completed_seq
        || state.lifecycle.is_usage_limited()
    {
        return None;
    }
    if matches!(presence, AgentPresenceObservation::Absent) {
        return Some(CaptureMode::UsageLimitOnly);
    }
    (matches!(presence, AgentPresenceObservation::Present(agent) if agent == &state.agent)
        && matches!(state.lifecycle, LifecycleState::Running)
        && snapshot
            .tracker
            .last_semantic_scan_at
            .is_none_or(|last_scan| {
                observed_at.saturating_sub(last_scan) >= USAGE_LIMIT_CAPTURE_INTERVAL_SECONDS
            }))
    .then_some(CaptureMode::UsageLimitOnly)
}

pub fn pane_removal_envelopes(
    daemon_instance_id: &DaemonInstanceId,
    previous: &[ObservationDispatchSnapshot],
    current: &BTreeSet<PaneInstance>,
    topology_complete: bool,
) -> Result<Vec<PaneEventEnvelope>> {
    if !topology_complete {
        return Ok(Vec::new());
    }
    previous
        .iter()
        .filter(|snapshot| !current.contains(&snapshot.pane_instance))
        .map(|snapshot| {
            Ok(PaneEventEnvelope {
                daemon_instance_id: daemon_instance_id.clone(),
                event_id: EventId::generate()?,
                pane_instance: snapshot.pane_instance.clone(),
                agent: None,
                agent_session_id: None,
                event: PaneEvent::PaneRemoved {
                    expected: snapshot.base.clone(),
                },
            })
        })
        .collect()
}

fn supports_process_detection(agent: &AgentKind) -> bool {
    matches!(agent.as_str(), "claude" | "codex" | "opencode")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use crate::daemon::workers::tests::pane_instance;

    struct MockCaptureSource {
        plain_calls: Mutex<usize>,
        requested_panes: Mutex<Vec<Vec<PaneInstance>>>,
        tails: Vec<String>,
    }

    impl CaptureSource for MockCaptureSource {
        fn capture_plain_tails(
            &self,
            panes: &[PaneInstance],
        ) -> std::result::Result<Vec<String>, CaptureBatchError> {
            *self.plain_calls.lock().unwrap() += 1;
            self.requested_panes.lock().unwrap().push(panes.to_vec());
            Ok(self.tails.clone())
        }
    }

    fn canonical_state(agent: &str) -> PaneState {
        PaneState {
            schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
            state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            revision: 1,
            pane_instance: pane_instance("%1", 11),
            agent: AgentKind::parse(agent).unwrap(),
            agent_session_id: None,
            agent_process: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Running,
            run_seq: 1,
            current_run: None,
            completed_seq: 0,
            unread: crate::pane_state::UnreadState::default(),
            started_at: Some(100),
            completed_at: None,
            prompt: None,
            latest_response: None,
            task_context: crate::pane_state::TaskContextState::default(),
            tasks: crate::pane_state::TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
        }
    }

    #[test]
    fn presence_is_three_state_and_prefers_current_kind() {
        let state = canonical_state("codex");
        let agents = BTreeSet::from([
            AgentKind::parse("claude").unwrap(),
            AgentKind::parse("codex").unwrap(),
        ]);
        assert_eq!(
            classify_presence(Some(&state), &agents, true),
            AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap())
        );
        assert_eq!(
            classify_presence(None, &agents, true),
            AgentPresenceObservation::Unknown
        );
        assert_eq!(
            classify_presence(Some(&state), &BTreeSet::new(), false),
            AgentPresenceObservation::Unknown
        );
        let mut generic = canonical_state("generic");
        generic.scan_verified = false;
        assert_eq!(
            classify_presence(Some(&generic), &BTreeSet::new(), true),
            AgentPresenceObservation::Unknown
        );
    }

    #[test]
    fn capture_inference_handles_baseline_change_rebaseline_and_stale() {
        let state = canonical_state("opencode");
        let baseline = infer_capture(
            Some(&state),
            &CaptureTrackerSnapshot::default(),
            "first\n",
            100,
        );
        assert_eq!(baseline.inference, CaptureInference::NoChange);
        let permission_baseline = infer_capture(
            Some(&state),
            &CaptureTrackerSnapshot::default(),
            "Allow command execution?\n1. Yes\n2. No\n",
            100,
        );
        assert_eq!(permission_baseline.inference, CaptureInference::NoChange);
        let mut tracker = CaptureTrackerSnapshot {
            fingerprint: baseline.observed_fingerprint,
            last_change_at: Some(100),
            ..CaptureTrackerSnapshot::default()
        };
        assert_eq!(
            infer_capture(Some(&state), &tracker, "changed\n", 101).inference,
            CaptureInference::ActivityObserved
        );
        tracker.rebaseline_pending = true;
        assert_eq!(
            infer_capture(Some(&state), &tracker, "changed\n", 500).inference,
            CaptureInference::NoChange
        );
        tracker.rebaseline_pending = false;
        assert_eq!(
            infer_capture(Some(&state), &tracker, "first\n", 500).inference,
            CaptureInference::StaleRunCompleted
        );
        assert!(
            infer_capture(Some(&state), &tracker, "   \n", 500)
                .observed_fingerprint
                .is_none()
        );
    }

    #[test]
    fn usage_limit_inference_is_direct_evidence_even_without_a_capture_baseline() {
        let state = canonical_state("codex");
        let observed = infer_capture(
            Some(&state),
            &CaptureTrackerSnapshot::default(),
            "You've hit your usage limit. Try again at 6:15 PM.\n",
            100,
        );

        assert_eq!(observed.inference, CaptureInference::UsageLimit);
        assert!(observed.observed_fingerprint.is_some());
        let opencode = canonical_state("opencode");
        assert_eq!(
            infer_capture(
                Some(&opencode),
                &CaptureTrackerSnapshot::default(),
                "You've hit your usage limit.\n",
                100,
            )
            .inference,
            CaptureInference::NoChange
        );
        assert_eq!(
            infer_usage_limit_capture("Allow command execution?\n1. Yes\n2. No\n").inference,
            CaptureInference::NoChange
        );
        assert_eq!(
            infer_usage_limit_capture("unchanged output\n").inference,
            CaptureInference::NoChange
        );
    }

    #[test]
    fn usage_limit_supplementary_capture_is_throttled_but_absence_is_immediate() {
        let state = canonical_state("codex");
        let present = AgentPresenceObservation::Present(state.agent.clone());
        let dispatch = ObservationDispatchSnapshot {
            pane_instance: state.pane_instance.clone(),
            base: Some(StoredStateDescriptor::Canonical {
                version: state.version(),
            }),
            tracker: CaptureTrackerSnapshot {
                epoch: Some((state.state_id.clone(), state.agent_epoch)),
                hook_authoritative: true,
                last_semantic_scan_at: Some(100),
                ..CaptureTrackerSnapshot::default()
            },
            state: Some(state),
        };

        assert_eq!(capture_mode(&dispatch, &present, 104), None);
        assert_eq!(
            capture_mode(&dispatch, &present, 105),
            Some(CaptureMode::UsageLimitOnly)
        );
        assert_eq!(
            capture_mode(&dispatch, &AgentPresenceObservation::Absent, 101),
            Some(CaptureMode::UsageLimitOnly)
        );
    }

    #[test]
    fn unknown_presence_drops_capture_from_observation_envelope() {
        let tracker = CaptureTrackerSnapshot::default();
        let envelope = observation_envelope(
            DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            pane_instance("%1", 11),
            None,
            &tracker,
            ObservationSample {
                observed_at: 100,
                presence: AgentPresenceObservation::Unknown,
                capture: Some(CaptureObservation {
                    inference: CaptureInference::ActivityObserved,
                    observed_fingerprint: Some([1; 32]),
                }),
                process: None,
            },
        )
        .unwrap();
        let PaneEvent::ObservationBatch { capture, .. } = envelope.event else {
            panic!("expected observation batch");
        };
        assert!(capture.is_none());
    }

    #[test]
    fn observation_poll_connects_frozen_dispatch_process_scan_and_single_capture() {
        let state = canonical_state("opencode");
        let tracker = CaptureTrackerSnapshot {
            epoch: Some((state.state_id.clone(), state.agent_epoch)),
            fingerprint: capture_sha256("before\n"),
            last_change_at: Some(100),
            ..CaptureTrackerSnapshot::default()
        };
        let dispatch = vec![ObservationDispatchSnapshot {
            pane_instance: state.pane_instance.clone(),
            base: Some(StoredStateDescriptor::Canonical {
                version: state.version(),
            }),
            tracker,
            state: Some(state),
        }];
        let source = MockCaptureSource {
            plain_calls: Mutex::new(0),
            requested_panes: Mutex::new(Vec::new()),
            tails: vec!["after\n".to_string()],
        };
        let processes = AgentProcessSnapshot::parse("11 1 11 11 opencode\n", true);
        let result = run_observation_poll(
            &source,
            &dispatch,
            &processes,
            &DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            200,
        )
        .unwrap();
        assert_eq!(*source.plain_calls.lock().unwrap(), 1);
        assert_eq!(
            *source.requested_panes.lock().unwrap(),
            vec![vec![pane_instance("%1", 11)]]
        );
        let PaneEvent::ObservationBatch {
            presence, capture, ..
        } = &result.envelopes[0].event
        else {
            panic!("expected observation batch");
        };
        assert_eq!(
            *presence,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap())
        );
        assert_eq!(
            capture.as_ref().unwrap().inference,
            CaptureInference::ActivityObserved
        );
    }

    #[test]
    fn observation_poll_captures_only_process_detected_agents_without_session_start() {
        let mut hook_managed = canonical_state("codex");
        hook_managed.pane_instance = pane_instance("%2", 22);
        hook_managed.agent_session_id =
            Some(crate::pane_state::AgentSessionId::parse("codex-session").unwrap());
        let mut fallback = canonical_state("opencode");
        fallback.pane_instance = pane_instance("%3", 33);
        fallback.agent_session_id =
            Some(crate::pane_state::AgentSessionId::parse("other-hook-session").unwrap());
        let dispatch = vec![
            ObservationDispatchSnapshot {
                pane_instance: pane_instance("%1", 11),
                base: None,
                tracker: CaptureTrackerSnapshot::default(),
                state: None,
            },
            ObservationDispatchSnapshot {
                pane_instance: hook_managed.pane_instance.clone(),
                base: Some(StoredStateDescriptor::Canonical {
                    version: hook_managed.version(),
                }),
                tracker: CaptureTrackerSnapshot {
                    epoch: Some((hook_managed.state_id.clone(), hook_managed.agent_epoch)),
                    hook_authoritative: true,
                    last_semantic_scan_at: Some(199),
                    ..CaptureTrackerSnapshot::default()
                },
                state: Some(hook_managed),
            },
            ObservationDispatchSnapshot {
                pane_instance: fallback.pane_instance.clone(),
                base: Some(StoredStateDescriptor::Canonical {
                    version: fallback.version(),
                }),
                tracker: CaptureTrackerSnapshot::default(),
                state: Some(fallback),
            },
        ];
        let source = MockCaptureSource {
            plain_calls: Mutex::new(0),
            requested_panes: Mutex::new(Vec::new()),
            tails: vec!["fallback tail\n".to_string()],
        };
        let processes = AgentProcessSnapshot::parse(
            "11 1 11 11 zsh\n22 1 22 22 codex\n33 1 33 33 opencode\n",
            true,
        );

        let result = run_observation_poll(
            &source,
            &dispatch,
            &processes,
            &DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            200,
        )
        .unwrap();

        assert_eq!(*source.plain_calls.lock().unwrap(), 1);
        assert_eq!(
            *source.requested_panes.lock().unwrap(),
            vec![vec![pane_instance("%3", 33)]]
        );
        assert_eq!(result.envelopes.len(), 3);
        let captures = result
            .envelopes
            .iter()
            .map(|envelope| match &envelope.event {
                PaneEvent::ObservationBatch { capture, .. } => capture.is_some(),
                _ => panic!("expected observation batch"),
            })
            .collect::<Vec<_>>();
        assert_eq!(captures, vec![false, false, true]);
    }

    #[test]
    fn observation_poll_skips_capture_when_only_non_agents_and_hook_sessions_exist() {
        let mut hook_managed = canonical_state("codex");
        hook_managed.pane_instance = pane_instance("%2", 22);
        hook_managed.agent_session_id =
            Some(crate::pane_state::AgentSessionId::parse("codex-session").unwrap());
        let dispatch = vec![
            ObservationDispatchSnapshot {
                pane_instance: pane_instance("%1", 11),
                base: None,
                tracker: CaptureTrackerSnapshot::default(),
                state: None,
            },
            ObservationDispatchSnapshot {
                pane_instance: hook_managed.pane_instance.clone(),
                base: Some(StoredStateDescriptor::Canonical {
                    version: hook_managed.version(),
                }),
                tracker: CaptureTrackerSnapshot {
                    epoch: Some((hook_managed.state_id.clone(), hook_managed.agent_epoch)),
                    hook_authoritative: true,
                    last_semantic_scan_at: Some(199),
                    ..CaptureTrackerSnapshot::default()
                },
                state: Some(hook_managed),
            },
        ];
        let source = MockCaptureSource {
            plain_calls: Mutex::new(0),
            requested_panes: Mutex::new(Vec::new()),
            tails: Vec::new(),
        };
        let processes = AgentProcessSnapshot::parse("11 1 11 11 zsh\n22 1 22 22 codex\n", true);

        let result = run_observation_poll(
            &source,
            &dispatch,
            &processes,
            &DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            200,
        )
        .unwrap();

        assert_eq!(*source.plain_calls.lock().unwrap(), 0);
        assert!(source.requested_panes.lock().unwrap().is_empty());
        assert_eq!(result.envelopes.len(), 2);
        assert!(result.envelopes.iter().all(|envelope| {
            matches!(
                &envelope.event,
                PaneEvent::ObservationBatch { capture: None, .. }
            )
        }));
    }

    #[test]
    fn observation_poll_discards_sparse_capture_when_tail_count_mismatches() {
        let mut first = canonical_state("codex");
        first.pane_instance = pane_instance("%1", 11);
        let mut second = canonical_state("opencode");
        second.pane_instance = pane_instance("%2", 22);
        let dispatch = [first, second]
            .into_iter()
            .map(|state| ObservationDispatchSnapshot {
                pane_instance: state.pane_instance.clone(),
                base: Some(StoredStateDescriptor::Canonical {
                    version: state.version(),
                }),
                tracker: CaptureTrackerSnapshot::default(),
                state: Some(state),
            })
            .collect::<Vec<_>>();
        let source = MockCaptureSource {
            plain_calls: Mutex::new(0),
            requested_panes: Mutex::new(Vec::new()),
            tails: vec!["only one tail\n".to_string()],
        };
        let processes =
            AgentProcessSnapshot::parse("11 1 11 11 codex\n22 1 22 22 opencode\n", true);

        let result = run_observation_poll(
            &source,
            &dispatch,
            &processes,
            &DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            200,
        )
        .unwrap();

        assert_eq!(
            *source.requested_panes.lock().unwrap(),
            vec![vec![pane_instance("%1", 11), pane_instance("%2", 22)]]
        );
        assert!(result.envelopes.iter().all(|envelope| {
            matches!(
                &envelope.event,
                PaneEvent::ObservationBatch { capture: None, .. }
            )
        }));
        assert!(result.diagnostics.iter().any(|message| {
            message.contains("capture delimiter count mismatch: expected 2, received 1")
        }));
    }

    #[test]
    fn incomplete_topology_never_emits_pane_removal() {
        let state = canonical_state("codex");
        let previous = vec![ObservationDispatchSnapshot {
            pane_instance: state.pane_instance.clone(),
            base: Some(StoredStateDescriptor::Canonical {
                version: state.version(),
            }),
            tracker: CaptureTrackerSnapshot::default(),
            state: Some(state),
        }];
        let daemon = DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap();
        assert!(
            pane_removal_envelopes(&daemon, &previous, &BTreeSet::new(), false)
                .unwrap()
                .is_empty()
        );
        let removed = pane_removal_envelopes(&daemon, &previous, &BTreeSet::new(), true).unwrap();
        assert!(matches!(removed[0].event, PaneEvent::PaneRemoved { .. }));
    }
}

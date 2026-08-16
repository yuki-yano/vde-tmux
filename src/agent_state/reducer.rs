use crate::agent_state::model::{
    ArtifactStoreCompleteness, ExecutionPhase, OperationId, PRIVATE_STATE_FORMAT_VERSION,
    ProviderCompleteness, ProviderEventReference, ResolutionId, ResolutionKind,
    ResponseArtifactMetadata, RunEvidenceSummary, RunRecord, RunResolution, SemanticOutcome,
    Sha256Digest, StableRunId, StateGeneration,
};
use crate::hook::provider::{
    ProviderCompleteness as ObservedCompleteness, ProviderHookKind, ProviderObservation,
    ResponseCandidate,
};

use super::model::{AgentBinding, ModelError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDisposition {
    Applied,
    Duplicate,
}

pub fn new_run_from_prompt(
    generation: StateGeneration,
    binding: AgentBinding,
    run_seq: u64,
    operation_id: Option<OperationId>,
    observation: &ProviderObservation,
) -> Result<RunRecord, ModelError> {
    if observation.hook_kind != ProviderHookKind::UserPromptSubmit {
        return Err(ModelError(
            "new run allocation requires UserPromptSubmit".to_string(),
        ));
    }
    require_observation_binding(&binding, observation)?;
    let mut record = RunRecord {
        state_format_version: PRIVATE_STATE_FORMAT_VERSION,
        generation,
        run_id: StableRunId::generate()?,
        run_seq,
        revision: 1,
        binding,
        provider_turn_key: observation.provider_turn_key.clone(),
        operation_id,
        execution_phase: ExecutionPhase::Running,
        semantic_outcome: SemanticOutcome::Unresolved,
        evidence: RunEvidenceSummary::default(),
        resolution: None,
        artifact: None,
        created_at: observation.observed_at,
        updated_at: observation.observed_at,
    };
    append_provider_reference(&mut record, observation, "run_created")?;
    record.validate()?;
    Ok(record)
}

pub fn apply_observation(
    record: &mut RunRecord,
    observation: &ProviderObservation,
) -> Result<ApplyDisposition, ModelError> {
    require_observation_binding(&record.binding, observation)?;
    if let (Some(expected), Some(actual)) = (
        record.provider_turn_key.as_deref(),
        observation.provider_turn_key.as_deref(),
    ) && expected != actual
    {
        return Err(ModelError(
            "provider observation belongs to another turn".to_string(),
        ));
    }
    if let Some(existing) = find_stable_provider_reference(record, observation) {
        let digest = Sha256Digest::parse(observation.payload_digest.clone())?;
        if existing.payload_digest != digest {
            return Err(ModelError(
                "provider event reference was reused with another payload".to_string(),
            ));
        }
        existing.count = existing
            .count
            .checked_add(1)
            .ok_or_else(|| ModelError("provider retry count overflow".to_string()))?;
        existing.last_observed_at = existing.last_observed_at.max(observation.observed_at);
        record.updated_at = record.updated_at.max(observation.observed_at);
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or_else(|| ModelError("run revision overflow".to_string()))?;
        record.validate()?;
        return Ok(ApplyDisposition::Duplicate);
    }

    match observation.hook_kind {
        ProviderHookKind::SessionStart => {
            return Err(ModelError(
                "SessionStart cannot be attributed to an Agent Run".to_string(),
            ));
        }
        ProviderHookKind::UserPromptSubmit => {
            return Err(ModelError(
                "a distinct UserPromptSubmit must allocate a new run".to_string(),
            ));
        }
        ProviderHookKind::Activity => {
            record.evidence.activity_count = record
                .evidence
                .activity_count
                .checked_add(1)
                .ok_or_else(|| ModelError("activity evidence overflow".to_string()))?;
            if record.semantic_outcome == SemanticOutcome::Unresolved
                && record.execution_phase != ExecutionPhase::Ended
            {
                record.execution_phase = ExecutionPhase::Running;
            }
        }
        ProviderHookKind::Waiting => {
            record.evidence.permission_request_count = record
                .evidence
                .permission_request_count
                .checked_add(1)
                .ok_or_else(|| ModelError("permission evidence overflow".to_string()))?;
            if record.semantic_outcome == SemanticOutcome::Unresolved
                && record.execution_phase != ExecutionPhase::Ended
            {
                record.execution_phase = ExecutionPhase::Waiting;
            }
        }
        ProviderHookKind::Stop => {
            if record.semantic_outcome == SemanticOutcome::Unresolved {
                record.execution_phase = ExecutionPhase::Ended;
                record.semantic_outcome = SemanticOutcome::Completed;
                record.resolution = Some(RunResolution {
                    resolution_id: ResolutionId::generate()?,
                    kind: ResolutionKind::ProviderCompleted,
                    resolved_at: observation.observed_at,
                    operator_audit: None,
                });
            }
        }
    }
    append_provider_reference(record, observation, "applied")?;
    record.updated_at = record.updated_at.max(observation.observed_at);
    record.revision = record
        .revision
        .checked_add(1)
        .ok_or_else(|| ModelError("run revision overflow".to_string()))?;
    record.validate()?;
    Ok(ApplyDisposition::Applied)
}

pub fn artifact_metadata(
    record: &RunRecord,
    observation: &ProviderObservation,
    candidate: Option<&ResponseCandidate>,
) -> Result<ResponseArtifactMetadata, ModelError> {
    if observation.hook_kind != ProviderHookKind::Stop
        || record.semantic_outcome != SemanticOutcome::Completed
    {
        return Err(ModelError(
            "response artifact requires a completed Stop observation".to_string(),
        ));
    }
    let metadata = match candidate {
        Some(candidate) => ResponseArtifactMetadata {
            run_id: record.run_id.clone(),
            operation_id: record.operation_id.clone(),
            provider_session_id: record.binding.provider_session_id.clone(),
            observed_process: record.binding.process.clone(),
            original_byte_count: candidate.original_bytes,
            stored_byte_count: candidate.stored_bytes,
            original_digest: Sha256Digest::parse(candidate.original_digest.clone())?,
            stored_digest: Some(Sha256Digest::parse(candidate.stored_digest.clone())?),
            provider_completeness: match candidate.provider_completeness {
                ObservedCompleteness::Complete => ProviderCompleteness::Complete,
                ObservedCompleteness::Unknown => ProviderCompleteness::Unknown,
            },
            store_completeness: if candidate.truncated {
                ArtifactStoreCompleteness::Truncated
            } else {
                ArtifactStoreCompleteness::Complete
            },
            source: "provider_hook".to_string(),
            encoding: "utf-8".to_string(),
            observed_at: observation.observed_at,
            file_name: Some(super::model::artifact_file_name(&record.run_id)),
        },
        None => ResponseArtifactMetadata {
            run_id: record.run_id.clone(),
            operation_id: record.operation_id.clone(),
            provider_session_id: record.binding.provider_session_id.clone(),
            observed_process: record.binding.process.clone(),
            original_byte_count: 0,
            stored_byte_count: 0,
            original_digest: Sha256Digest::of(&[]),
            stored_digest: None,
            provider_completeness: ProviderCompleteness::Unknown,
            store_completeness: ArtifactStoreCompleteness::Unavailable,
            source: "provider_hook".to_string(),
            encoding: "utf-8".to_string(),
            observed_at: observation.observed_at,
            file_name: None,
        },
    };
    metadata.validate()?;
    Ok(metadata)
}

fn require_observation_binding(
    binding: &AgentBinding,
    observation: &ProviderObservation,
) -> Result<(), ModelError> {
    if binding.agent_kind != observation.provider
        || binding.provider_session_id != observation.session_id
    {
        return Err(ModelError(
            "provider observation does not match the Agent Binding".to_string(),
        ));
    }
    Ok(())
}

fn find_stable_provider_reference<'a>(
    record: &'a mut RunRecord,
    observation: &ProviderObservation,
) -> Option<&'a mut ProviderEventReference> {
    let event_ref = correctness_event_ref(observation)?;
    record
        .evidence
        .provider_events
        .iter_mut()
        .find(|existing| existing.event_ref == event_ref)
}

fn append_provider_reference(
    record: &mut RunRecord,
    observation: &ProviderObservation,
    disposition: &str,
) -> Result<(), ModelError> {
    if let Some(event_ref) = correctness_event_ref(observation) {
        if record.evidence.provider_events.len()
            >= crate::agent_state::model::RUN_EVENT_REFERENCE_MAX_COUNT
        {
            return Err(ModelError(
                "run provider event reference capacity exceeded".to_string(),
            ));
        }
        record
            .evidence
            .provider_events
            .push(ProviderEventReference {
                event_ref: event_ref.to_string(),
                ingress_request_id: observation.ingress_request_id.as_str().to_string(),
                payload_digest: Sha256Digest::parse(observation.payload_digest.clone())?,
                disposition: disposition.to_string(),
                receipt: format!("run_revision_{}", record.revision),
                count: 1,
                first_observed_at: observation.observed_at,
                last_observed_at: observation.observed_at,
            });
    }
    record.evidence.first_observed_at = Some(
        record
            .evidence
            .first_observed_at
            .map_or(observation.observed_at, |value| {
                value.min(observation.observed_at)
            }),
    );
    record.evidence.last_observed_at = Some(
        record
            .evidence
            .last_observed_at
            .map_or(observation.observed_at, |value| {
                value.max(observation.observed_at)
            }),
    );
    Ok(())
}

fn correctness_event_ref(observation: &ProviderObservation) -> Option<&str> {
    matches!(
        observation.hook_kind,
        ProviderHookKind::UserPromptSubmit | ProviderHookKind::Stop
    )
    .then(|| {
        observation
            .provider_event_ref
            .as_deref()
            .unwrap_or(observation.ingress_request_id.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::topology::ServerIdentity;
    use crate::pane_state::{
        AgentKind, AgentProcessIdentity, AgentSessionId, EventId, PaneInstance, StateId,
    };

    fn binding() -> AgentBinding {
        AgentBinding {
            server_identity: ServerIdentity {
                pid: 44,
                start_time: 55,
            },
            pane_instance: PaneInstance {
                pane_id: "%7".to_string(),
                pane_pid: 77,
            },
            pane_state_id: StateId::generate().unwrap(),
            agent_epoch: 1,
            agent_kind: AgentKind::parse("codex").unwrap(),
            provider_session_id: AgentSessionId::parse("session-reducer-test").unwrap(),
            process: AgentProcessIdentity {
                pid: 88,
                start_token: "process-start-token".to_string(),
            },
        }
    }

    fn observation(
        kind: ProviderHookKind,
        ingress_request_id: &str,
        event_ref: &str,
        payload: &str,
        observed_at: i64,
    ) -> ProviderObservation {
        ProviderObservation {
            ingress_request_id: EventId::parse(ingress_request_id).unwrap(),
            provider: AgentKind::parse("codex").unwrap(),
            session_id: AgentSessionId::parse("session-reducer-test").unwrap(),
            hook_kind: kind,
            provider_turn_key: Some("turn-1".to_string()),
            provider_event_ref: Some(Sha256Digest::of(event_ref.as_bytes()).as_str().to_string()),
            payload_digest: Sha256Digest::of(payload.as_bytes()).as_str().to_string(),
            prompt_digest: (kind == ProviderHookKind::UserPromptSubmit)
                .then(|| Sha256Digest::of(payload.as_bytes()).as_str().to_string()),
            response: None,
            observed_at,
        }
    }

    fn run() -> RunRecord {
        let prompt = observation(
            ProviderHookKind::UserPromptSubmit,
            "00112233445566778899aabbccddeeff",
            "prompt-event",
            "prompt-payload",
            1,
        );
        new_run_from_prompt(
            StateGeneration::generate().unwrap(),
            binding(),
            1,
            None,
            &prompt,
        )
        .unwrap()
    }

    #[test]
    fn repeated_activity_with_the_same_provider_reference_is_applied_each_time() {
        let mut record = run();
        let first = observation(
            ProviderHookKind::Activity,
            "11112233445566778899aabbccddeeff",
            "activity-event",
            "activity-payload",
            2,
        );
        let second = observation(
            ProviderHookKind::Activity,
            "22222233445566778899aabbccddeeff",
            "activity-event",
            "activity-payload",
            3,
        );

        assert_eq!(
            apply_observation(&mut record, &first).unwrap(),
            ApplyDisposition::Applied
        );
        assert_eq!(
            apply_observation(&mut record, &second).unwrap(),
            ApplyDisposition::Applied
        );
        assert_eq!(record.evidence.activity_count, 2);
        assert_eq!(record.evidence.provider_events.len(), 1);
    }

    #[test]
    fn activity_after_waiting_returns_the_run_to_running() {
        let mut record = run();
        let waiting = observation(
            ProviderHookKind::Waiting,
            "11112233445566778899aabbccddeeff",
            "waiting-event",
            "waiting-payload",
            2,
        );
        let activity = observation(
            ProviderHookKind::Activity,
            "22222233445566778899aabbccddeeff",
            "activity-event",
            "activity-payload",
            3,
        );

        assert_eq!(
            apply_observation(&mut record, &waiting).unwrap(),
            ApplyDisposition::Applied
        );
        assert_eq!(record.execution_phase, ExecutionPhase::Waiting);
        assert_eq!(
            apply_observation(&mut record, &activity).unwrap(),
            ApplyDisposition::Applied
        );
        assert_eq!(record.execution_phase, ExecutionPhase::Running);
        assert_eq!(record.evidence.permission_request_count, 1);
        assert_eq!(record.evidence.activity_count, 1);
    }

    #[test]
    fn stable_prompt_and_stop_retries_reject_payload_conflicts() {
        let mut prompt_record = run();
        let conflicting_prompt = observation(
            ProviderHookKind::UserPromptSubmit,
            "11112233445566778899aabbccddeeff",
            "prompt-event",
            "different-prompt-payload",
            2,
        );
        assert_eq!(
            apply_observation(&mut prompt_record, &conflicting_prompt)
                .unwrap_err()
                .to_string(),
            "provider event reference was reused with another payload"
        );

        let mut stop_record = run();
        let stop = observation(
            ProviderHookKind::Stop,
            "11112233445566778899aabbccddeeff",
            "stop-event",
            "stop-payload",
            2,
        );
        let conflicting_stop = observation(
            ProviderHookKind::Stop,
            "22222233445566778899aabbccddeeff",
            "stop-event",
            "different-stop-payload",
            3,
        );
        assert_eq!(
            apply_observation(&mut stop_record, &stop).unwrap(),
            ApplyDisposition::Applied
        );
        assert_eq!(
            apply_observation(&mut stop_record, &conflicting_stop)
                .unwrap_err()
                .to_string(),
            "provider event reference was reused with another payload"
        );
    }

    #[test]
    fn stop_retry_after_later_lifecycle_evidence_is_deduped_without_reopening_the_run() {
        let mut record = run();
        let stop = observation(
            ProviderHookKind::Stop,
            "11112233445566778899aabbccddeeff",
            "stop-replay-event",
            "stop-replay-payload",
            2,
        );
        assert_eq!(
            apply_observation(&mut record, &stop).unwrap(),
            ApplyDisposition::Applied
        );
        let resolution = record.resolution.clone().unwrap();

        let later_activity = observation(
            ProviderHookKind::Activity,
            "22222233445566778899aabbccddeeff",
            "activity-after-stop",
            "activity-after-stop-payload",
            3,
        );
        assert_eq!(
            apply_observation(&mut record, &later_activity).unwrap(),
            ApplyDisposition::Applied
        );
        assert_eq!(record.execution_phase, ExecutionPhase::Ended);
        assert_eq!(record.semantic_outcome, SemanticOutcome::Completed);
        assert_eq!(record.resolution.as_ref(), Some(&resolution));

        let retry_revision = record.revision;
        let mut retried_stop = stop;
        retried_stop.ingress_request_id =
            EventId::parse("33332233445566778899aabbccddeeff").unwrap();
        retried_stop.observed_at = 4;
        assert_eq!(
            apply_observation(&mut record, &retried_stop).unwrap(),
            ApplyDisposition::Duplicate
        );
        assert_eq!(record.revision, retry_revision + 1);
        assert_eq!(record.execution_phase, ExecutionPhase::Ended);
        assert_eq!(record.semantic_outcome, SemanticOutcome::Completed);
        assert_eq!(record.resolution.as_ref(), Some(&resolution));
        let stop_reference = record
            .evidence
            .provider_events
            .iter()
            .find(|event| event.event_ref == retried_stop.provider_event_ref.as_deref().unwrap())
            .unwrap();
        assert_eq!(stop_reference.count, 2);
        assert_eq!(stop_reference.first_observed_at, 2);
        assert_eq!(stop_reference.last_observed_at, 4);
    }
}

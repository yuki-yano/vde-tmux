use std::collections::BTreeMap;
use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

use super::model::{
    AgentBinding, DispatchState, ExecutionPhase, OperationBinding, OperationId, OperationRecord,
    OperationRef, OperationResultReceipt, OperatorAudit, PRIVATE_STATE_FORMAT_VERSION,
    RecoveryPaneFence, RecoveryPrecondition, RecoveryProcessExpectation,
    RecoveryViewportFingerprint, ResolutionId, ResolutionKind, RunRecord, RunRef, RunResolution,
    SemanticOutcome, Sha256Digest, StableRunId,
};
use super::reducer::{ApplyDisposition, apply_observation, artifact_metadata, new_run_from_prompt};
use super::store::{AgentStateStore, RUN_STORE_MAX_RECORDS, RunRetentionReserve, StoreError};
use crate::hook::provider::{ProviderHookKind, ProviderObservation};

pub const PROMPT_CONFIRMATION_TIMEOUT_SECONDS: i64 = 10;

#[derive(Debug, Clone)]
pub struct ProviderApplyResult {
    pub run: Option<RunRecord>,
    pub operation: Option<OperationRecord>,
    pub disposition: ApplyDisposition,
}

#[derive(Debug, Clone)]
pub enum PrepareOperationResult {
    Existing(OperationRecord),
    Created(OperationRecord),
}

pub struct AgentRuntime {
    server_identity: String,
    store: AgentStateStore,
    current_by_binding: BTreeMap<String, StableRunId>,
    current_by_pane: BTreeMap<crate::pane_state::PaneInstance, StableRunId>,
    turn_index: BTreeMap<String, StableRunId>,
    event_index: BTreeMap<String, StableRunId>,
    in_flight_by_binding: BTreeMap<String, OperationId>,
}

impl AgentRuntime {
    pub fn open(root: PathBuf, server_identity: String) -> Result<Self, StoreError> {
        let store = AgentStateStore::open_or_initialize(root)?;
        let mut runtime = Self {
            server_identity,
            store,
            current_by_binding: BTreeMap::new(),
            current_by_pane: BTreeMap::new(),
            turn_index: BTreeMap::new(),
            event_index: BTreeMap::new(),
            in_flight_by_binding: BTreeMap::new(),
        };
        runtime.rebuild_indexes()?;
        runtime.reconcile_in_flight_after_restart()?;
        Ok(runtime)
    }

    pub fn store(&self) -> &AgentStateStore {
        &self.store
    }

    pub fn provider_event_run(
        &self,
        observation: &ProviderObservation,
    ) -> Result<Option<RunRecord>, StoreError> {
        let Some(event_ref) = observation.provider_event_ref.as_deref() else {
            return Ok(None);
        };
        self.event_index
            .get(event_ref)
            .map(|run_id| self.required_run(run_id))
            .transpose()
    }

    pub fn request_fingerprint(
        target_agent_ref: &str,
        prompt_digest: &Sha256Digest,
        dispatch_option: &str,
    ) -> Sha256Digest {
        let mut hasher = Sha256::new();
        for field in [
            b"vde-tmux:agent-operation-request:v1".as_slice(),
            target_agent_ref.as_bytes(),
            prompt_digest.as_str().as_bytes(),
            dispatch_option.as_bytes(),
        ] {
            hasher.update((field.len() as u64).to_be_bytes());
            hasher.update(field);
        }
        Sha256Digest::of(&hasher.finalize())
    }

    pub fn lookup_operation_request(
        &self,
        operation_id: &OperationId,
        request_fingerprint: &Sha256Digest,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let Some(existing) = self.store.load_operation(operation_id)? else {
            return Ok(None);
        };
        if &existing.request_fingerprint != request_fingerprint {
            return Err(StoreError::OperationConflict(format!(
                "operation ID {} was reused with another request",
                operation_id.as_str()
            )));
        }
        Ok(Some(existing))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare_operation<B: Into<OperationBinding>>(
        &mut self,
        operation_id: OperationId,
        target_agent_ref: String,
        prompt: &[u8],
        prompt_digest: Sha256Digest,
        dispatch_option: String,
        binding: B,
        expected_pane_version: crate::pane_state::StateVersion,
        expected_current_run: Option<crate::pane_state::CurrentDurableRunProjection>,
        expected_run_seq: u64,
        observed_at: i64,
    ) -> Result<PrepareOperationResult, StoreError> {
        let binding = binding.into();
        let decoded_prompt = std::str::from_utf8(prompt)
            .map_err(|_| StoreError::Invalid("prompt staging must be UTF-8".to_string()))?;
        let expected_prompt_digest = Sha256Digest::parse(
            crate::pane_state::PromptState::digest_decoded_prompt(decoded_prompt),
        )
        .map_err(|error| StoreError::Invalid(error.to_string()))?;
        if prompt_digest != expected_prompt_digest {
            return Err(StoreError::Invalid(
                "prompt staging does not match the domain-separated prompt digest".to_string(),
            ));
        }
        let request_fingerprint =
            Self::request_fingerprint(&target_agent_ref, &prompt_digest, &dispatch_option);
        if let Some(existing) =
            self.lookup_operation_request(&operation_id, &request_fingerprint)?
        {
            return Ok(PrepareOperationResult::Existing(existing));
        }
        let key = operation_target_key(&binding)?;
        if self.in_flight_by_binding.contains_key(&key) {
            return Err(StoreError::PromptDispatchBusy(
                "another dispatch operation is active or delivery-ambiguous for this Agent Binding"
                    .to_string(),
            ));
        }
        if let Err(error) = self.store.stage_prompt(&operation_id, prompt) {
            return Err(match error {
                StoreError::Capacity(message) => StoreError::OperationStoreFull(message),
                error => error,
            });
        }
        let record = OperationRecord {
            state_format_version: PRIVATE_STATE_FORMAT_VERSION,
            generation: self.store.generation().clone(),
            operation_id: operation_id.clone(),
            revision: 1,
            request_fingerprint,
            target_agent_ref,
            prompt_digest,
            dispatch_option,
            binding,
            expected_pane_version,
            expected_current_run,
            expected_run_seq,
            confirmation_deadline_at: observed_at
                .checked_add(PROMPT_CONFIRMATION_TIMEOUT_SECONDS)
                .ok_or_else(|| {
                    StoreError::Invalid("operation confirmation deadline overflow".to_string())
                })?,
            dispatch_state: DispatchState::Prepared,
            run_id: None,
            result_receipt: None,
            created_at: observed_at,
            updated_at: observed_at,
        };
        if let Err(error) = self.store.save_operation(&record) {
            let _ = self.store.delete_prompt(&operation_id);
            return Err(match error {
                StoreError::Capacity(message) => StoreError::OperationStoreFull(message),
                error => error,
            });
        }
        self.in_flight_by_binding.insert(key, operation_id);
        Ok(PrepareOperationResult::Created(record))
    }

    pub fn mark_dispatch_started(
        &mut self,
        operation_id: &OperationId,
        observed_at: i64,
    ) -> Result<OperationRecord, StoreError> {
        let mut record = self.required_operation(operation_id)?;
        if record.dispatch_state != DispatchState::Prepared {
            return Ok(record);
        }
        record.dispatch_state = DispatchState::DispatchStarted;
        advance_operation_revision(&mut record, observed_at)?;
        self.store.save_operation(&record)?;
        Ok(record)
    }

    pub fn reject_prepared_retry_if_expired(
        &mut self,
        operation_id: &OperationId,
        observed_at: i64,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let record = self.required_operation(operation_id)?;
        if record.dispatch_state != DispatchState::Prepared
            || record.confirmation_deadline_at > observed_at
        {
            return Ok(None);
        }
        self.settle_dispatch(
            operation_id,
            DispatchState::Rejected,
            "prepared_dispatch_timeout",
            observed_at,
        )
        .map(Some)
    }

    pub fn settle_dispatch(
        &mut self,
        operation_id: &OperationId,
        state: DispatchState,
        code: &str,
        observed_at: i64,
    ) -> Result<OperationRecord, StoreError> {
        if !matches!(
            state,
            DispatchState::DeliveryUnknown | DispatchState::Rejected
        ) {
            return Err(StoreError::Invalid(
                "dispatch settlement must be delivery_unknown or rejected".to_string(),
            ));
        }
        let mut record = self.required_operation(operation_id)?;
        if record.dispatch_state == state {
            if state == DispatchState::Rejected {
                self.in_flight_by_binding
                    .remove(&operation_target_key(&record.binding)?);
            }
            self.store.delete_prompt(operation_id)?;
            return Ok(record);
        }
        if !matches!(
            (record.dispatch_state, state),
            (DispatchState::Prepared, DispatchState::Rejected)
                | (
                    DispatchState::DispatchStarted,
                    DispatchState::DeliveryUnknown | DispatchState::Rejected
                )
        ) {
            return Err(StoreError::OperationConflict(format!(
                "cannot settle operation from {:?} to {:?}",
                record.dispatch_state, state
            )));
        }
        record.dispatch_state = state;
        record.result_receipt = Some(OperationResultReceipt {
            code: code.to_string(),
            observed_at,
            confirmation_basis: None,
            source_attribution: None,
        });
        advance_operation_revision(&mut record, observed_at)?;
        self.store.save_operation(&record)?;
        if state == DispatchState::Rejected {
            self.in_flight_by_binding
                .remove(&operation_target_key(&record.binding)?);
        } else {
            self.in_flight_by_binding
                .insert(operation_target_key(&record.binding)?, operation_id.clone());
        }
        self.store.delete_prompt(operation_id)?;
        Ok(record)
    }

    pub fn settle_expired_dispatches(
        &mut self,
        observed_at: i64,
    ) -> Result<Vec<OperationRecord>, StoreError> {
        let expired = self
            .in_flight_by_binding
            .values()
            .map(|operation_id| self.required_operation(operation_id))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|operation| {
                matches!(
                    operation.dispatch_state,
                    DispatchState::Prepared | DispatchState::DispatchStarted
                ) && operation.confirmation_deadline_at <= observed_at
            })
            .map(|operation| (operation.operation_id, operation.dispatch_state))
            .collect::<Vec<_>>();
        expired
            .iter()
            .map(|(operation_id, state)| {
                let (target, code) = if *state == DispatchState::Prepared {
                    (DispatchState::Rejected, "prepared_dispatch_timeout")
                } else {
                    (
                        DispatchState::DeliveryUnknown,
                        "prompt_confirmation_timeout",
                    )
                };
                self.settle_dispatch(operation_id, target, code, observed_at)
            })
            .collect()
    }

    pub fn has_expired_dispatch(&self, observed_at: i64) -> Result<bool, StoreError> {
        for operation_id in self.in_flight_by_binding.values() {
            let operation = self.required_operation(operation_id)?;
            if matches!(
                operation.dispatch_state,
                DispatchState::Prepared | DispatchState::DispatchStarted
            ) && operation.confirmation_deadline_at <= observed_at
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn apply_provider_observation(
        &mut self,
        binding: AgentBinding,
        run_seq: u64,
        observation: &ProviderObservation,
    ) -> Result<ProviderApplyResult, StoreError> {
        if observation.hook_kind == ProviderHookKind::SessionStart {
            return Ok(ProviderApplyResult {
                run: None,
                operation: None,
                disposition: ApplyDisposition::Applied,
            });
        }
        if let Some(event_ref) = &observation.provider_event_ref
            && let Some(run_id) = self.event_index.get(event_ref).cloned()
        {
            let mut run = self.required_run(&run_id)?;
            let disposition = apply_observation(&mut run, observation)
                .map_err(|error| StoreError::ProviderEventConflict(error.to_string()))?;
            self.store.save_run(&run)?;
            return self.finish_provider_observation(run, observation, disposition);
        }
        self.retire_replaced_bindings_for_pane(&binding, observation.observed_at)?;

        let key = binding_key(&binding)?;
        let run_id = if observation.hook_kind == ProviderHookKind::UserPromptSubmit {
            if let Some(previous_id) = self.current_by_binding.get(&key).cloned() {
                let mut previous = self.required_run(&previous_id)?;
                if previous.run_seq >= run_seq {
                    return Err(StoreError::ProviderEventConflict(format!(
                        "run sequence {} is already allocated for this Agent Binding",
                        run_seq
                    )));
                }
                if previous.execution_active() {
                    previous.execution_phase = ExecutionPhase::Ended;
                    previous.revision = previous
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| StoreError::Invalid("run revision overflow".to_string()))?;
                    previous.updated_at = previous.updated_at.max(observation.observed_at);
                    self.store.save_run(&previous)?;
                }
            }
            let matching_operation = self.matching_operation(&binding, run_seq, observation)?;
            self.collect_run_retention(
                observation.observed_at,
                RunRetentionReserve {
                    records: 1,
                    bytes: super::RUN_RECORD_MAX_BYTES as u64,
                },
                Some(&binding.pane_instance),
            )?;
            let run = new_run_from_prompt(
                self.store.generation().clone(),
                binding,
                run_seq,
                matching_operation
                    .as_ref()
                    .map(|record| record.operation_id.clone()),
                observation,
            )
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
            let run_id = run.run_id.clone();
            self.store.save_run(&run)?;
            self.index_run(&run)?;
            run_id
        } else if let Some(turn_key) = observation.provider_turn_key.as_deref() {
            self.turn_index
                .get(&turn_index_key(observation, turn_key))
                .cloned()
                .ok_or_else(|| {
                    StoreError::NotFound(
                        "provider event could not be attributed to a retained run".to_string(),
                    )
                })?
        } else {
            self.current_by_binding.get(&key).cloned().ok_or_else(|| {
                StoreError::NotFound(
                    "provider event has no unique execution-active run".to_string(),
                )
            })?
        };

        let mut run = self.required_run(&run_id)?;
        let disposition = if observation.hook_kind == ProviderHookKind::UserPromptSubmit {
            ApplyDisposition::Applied
        } else {
            apply_observation(&mut run, observation)
                .map_err(|error| StoreError::ProviderEventConflict(error.to_string()))?
        };
        if observation.hook_kind != ProviderHookKind::UserPromptSubmit {
            self.store.save_run(&run)?;
            self.index_run(&run)?;
        }

        self.finish_provider_observation(run, observation, disposition)
    }

    fn finish_provider_observation(
        &mut self,
        mut run: RunRecord,
        observation: &ProviderObservation,
        disposition: ApplyDisposition,
    ) -> Result<ProviderApplyResult, StoreError> {
        let mut operation = self.confirm_linked_operation(&run, observation.observed_at)?;
        if observation.hook_kind == ProviderHookKind::Stop {
            if let Some(response) = &observation.response {
                self.store.store_artifact_candidate(
                    &run.run_id,
                    response,
                    "provider_hook".to_string(),
                    observation.observed_at,
                )?;
            } else if run.artifact.is_none() {
                let metadata = artifact_metadata(&run, observation, None)
                    .map_err(|error| StoreError::Invalid(error.to_string()))?;
                run.artifact = Some(metadata);
                run.revision = run
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| StoreError::Invalid("run revision overflow".to_string()))?;
                self.store.save_run(&run)?;
            }
            run = self.required_run(&run.run_id)?;
            self.index_run(&run)?;
        }
        if operation.is_none() && run.operation_id.is_some() {
            operation = run
                .operation_id
                .as_ref()
                .map(|operation_id| self.required_operation(operation_id))
                .transpose()?;
        }
        Ok(ProviderApplyResult {
            run: Some(run),
            operation,
            disposition,
        })
    }

    pub fn current_runs(&self) -> Result<Vec<RunRecord>, StoreError> {
        self.current_by_binding
            .values()
            .map(|run_id| self.required_run(run_id))
            .collect()
    }

    pub fn reconcile_process_for_pane(
        &mut self,
        pane: &crate::pane_state::PaneInstance,
        process_checked: bool,
        observed_process: Option<&crate::pane_state::AgentProcessIdentity>,
        observed_at: i64,
    ) -> Result<Option<RunRecord>, StoreError> {
        if !process_checked {
            return Ok(None);
        }
        let Some(run_id) = self.current_by_pane.get(pane).cloned() else {
            return Ok(None);
        };
        let mut run = self.required_run(&run_id)?;
        if observed_process == Some(&run.binding.process) {
            return Ok(None);
        }
        let was_execution_active = run.execution_active();
        if was_execution_active {
            self.end_unresolved_run(&mut run, observed_at)?;
        }
        self.remove_current_binding(&run)?;
        Ok(was_execution_active.then_some(run))
    }

    pub fn reconcile_panes_after_restart(
        &mut self,
        panes: &BTreeMap<crate::pane_state::PaneInstance, crate::pane_state::PaneState>,
        observed_at: i64,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let runs = self.current_runs()?;
        for mut run in runs {
            if !run.execution_active() {
                continue;
            }
            let exact_binding_is_live = panes
                .get(&run.binding.pane_instance)
                .is_some_and(|pane| pane_matches_binding(pane, &run.binding));
            if !exact_binding_is_live {
                self.end_unresolved_run(&mut run, observed_at)?;
            }
        }
        let mut live_binding_keys = std::collections::BTreeSet::new();
        for (key, run_id) in &self.current_by_binding {
            let run = self.required_run(run_id)?;
            if panes
                .get(&run.binding.pane_instance)
                .is_some_and(|pane| pane_matches_binding(pane, &run.binding))
            {
                live_binding_keys.insert(key.clone());
            }
        }
        self.current_by_binding
            .retain(|key, _| live_binding_keys.contains(key));
        self.current_by_pane.clear();
        for run_id in self.current_by_binding.values() {
            let run = self.required_run(run_id)?;
            self.current_by_pane
                .insert(run.binding.pane_instance.clone(), run.run_id.clone());
        }
        self.current_runs()
    }

    fn retire_replaced_bindings_for_pane(
        &mut self,
        binding: &AgentBinding,
        observed_at: i64,
    ) -> Result<(), StoreError> {
        let replaced = self
            .current_by_pane
            .get(&binding.pane_instance)
            .cloned()
            .map(|run_id| self.required_run(&run_id))
            .transpose()?
            .filter(|run| run.binding != *binding);
        if let Some(mut run) = replaced {
            self.end_unresolved_run(&mut run, observed_at)?;
            self.remove_current_binding(&run)?;
        }
        Ok(())
    }

    fn remove_current_binding(&mut self, run: &RunRecord) -> Result<(), StoreError> {
        let key = binding_key(&run.binding)?;
        if self
            .current_by_binding
            .get(&key)
            .is_some_and(|run_id| run_id == &run.run_id)
        {
            self.current_by_binding.remove(&key);
        }
        if self
            .current_by_pane
            .get(&run.binding.pane_instance)
            .is_some_and(|run_id| run_id == &run.run_id)
        {
            self.current_by_pane.remove(&run.binding.pane_instance);
        }
        Ok(())
    }

    fn end_unresolved_run(
        &mut self,
        run: &mut RunRecord,
        observed_at: i64,
    ) -> Result<(), StoreError> {
        if !run.execution_active() {
            return Ok(());
        }
        run.execution_phase = ExecutionPhase::Ended;
        run.revision = run
            .revision
            .checked_add(1)
            .ok_or_else(|| StoreError::Invalid("run revision overflow".to_string()))?;
        run.updated_at = run.updated_at.max(observed_at);
        self.store.save_run(run)?;
        self.index_run(run)
    }

    pub fn get_run(&self, reference: &RunRef) -> Result<RunRecord, StoreError> {
        self.validate_run_ref(reference)?;
        self.required_run(&reference.run_id)
    }

    pub fn current_run_for_binding(
        &self,
        binding: &AgentBinding,
    ) -> Result<Option<RunRecord>, StoreError> {
        let Some(run_id) = self.current_by_binding.get(&binding_key(binding)?) else {
            return Ok(None);
        };
        if self.current_by_pane.get(&binding.pane_instance) != Some(run_id) {
            return Ok(None);
        }
        let run = self.required_run(run_id)?;
        if run.binding != *binding {
            return Err(StoreError::Corrupt(
                "current run index points to another Agent Binding".to_string(),
            ));
        }
        Ok(Some(run))
    }

    pub fn evidence_digest(run: &RunRecord) -> Result<Sha256Digest, StoreError> {
        serde_json::to_vec(&run.evidence)
            .map(|encoded| Sha256Digest::of(&encoded))
            .map_err(|error| StoreError::Invalid(format!("failed to encode run evidence: {error}")))
    }

    pub fn lookup_operator_completion(
        &self,
        reference: &RunRef,
        resolution_id: &ResolutionId,
        reason: &str,
    ) -> Result<Option<RunRecord>, StoreError> {
        self.validate_run_ref(reference)?;
        let run = self.required_run(&reference.run_id)?;
        let Some(existing) = &run.resolution else {
            return Ok(None);
        };
        if &existing.resolution_id == resolution_id {
            let reason_digest = Sha256Digest::of(reason.as_bytes());
            if existing.kind == ResolutionKind::OperatorCompleted
                && existing
                    .operator_audit
                    .as_ref()
                    .is_some_and(|audit| audit.reason_digest == reason_digest)
            {
                return Ok(Some(run));
            }
            return Err(StoreError::ResolutionConflict(
                "resolution ID was reused with another outcome or reason".to_string(),
            ));
        }
        Err(StoreError::RunAlreadyResolved(
            "run was already resolved by another resolution ID".to_string(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve_operator_completed(
        &mut self,
        reference: &RunRef,
        precondition: &RecoveryPrecondition,
        resolution_id: ResolutionId,
        reason: String,
        actor_uid: u32,
        actor_pid: u32,
        observed_at: i64,
        fresh_pane: &RecoveryPaneFence,
        fresh_process: Option<crate::pane_state::AgentProcessIdentity>,
        fresh_viewport_fingerprint: Option<&RecoveryViewportFingerprint>,
    ) -> Result<RunRecord, StoreError> {
        self.validate_run_ref(reference)?;
        if let Some(run) = self.lookup_operator_completion(reference, &resolution_id, &reason)? {
            return Ok(run);
        }
        let mut run = self.required_run(&reference.run_id)?;
        let reason_digest = Sha256Digest::of(reason.as_bytes());

        precondition
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let encoded_ref = reference
            .encode()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        if precondition.run_ref != encoded_ref
            || precondition.binding != run.binding
            || precondition.run_revision != run.revision
            || precondition.evidence_digest != Self::evidence_digest(&run)?
            || precondition.pane.current_run.run_id != run.run_id.as_str()
            || precondition.pane.current_run.run_seq != run.run_seq
            || precondition.pane.current_run.run_revision != run.revision
        {
            return Err(StoreError::StalePrecondition(
                "run identity, revision, binding, or evidence changed".to_string(),
            ));
        }
        if observed_at < precondition.issued_at || observed_at > precondition.expires_at {
            return Err(StoreError::StalePrecondition(
                "recovery precondition expired or is not yet valid".to_string(),
            ));
        }
        if run.semantic_outcome != SemanticOutcome::Unresolved {
            return Err(StoreError::RecoveryNotAllowed(
                "only an unresolved run may be operator-completed".to_string(),
            ));
        }
        if fresh_pane != &precondition.pane {
            return Err(StoreError::StalePrecondition(
                "fresh pane state disagrees with recovery precondition".to_string(),
            ));
        }
        let expectation_matches = match (
            &precondition.process_expectation,
            &fresh_process,
            fresh_viewport_fingerprint,
        ) {
            (RecoveryProcessExpectation::ExactAbsent, None, None) => true,
            (RecoveryProcessExpectation::ReplacedBy { process: expected }, Some(actual), None) => {
                actual == expected && actual != &run.binding.process
            }
            (
                RecoveryProcessExpectation::ExactPresentStable { process: expected },
                Some(actual),
                Some(fingerprint),
            ) => {
                actual == expected
                    && actual == &run.binding.process
                    && Some(fingerprint) == precondition.viewport_fingerprint.as_ref()
            }
            _ => false,
        };
        if !expectation_matches {
            return Err(StoreError::StalePrecondition(
                "fresh process observation disagrees with recovery precondition".to_string(),
            ));
        }

        let pre_revision = run.revision;
        let post_revision = pre_revision
            .checked_add(1)
            .ok_or_else(|| StoreError::Invalid("run revision overflow".to_string()))?;
        run.revision = post_revision;
        run.execution_phase = ExecutionPhase::Ended;
        run.semantic_outcome = SemanticOutcome::Completed;
        run.updated_at = run.updated_at.max(observed_at);
        run.resolution = Some(RunResolution {
            resolution_id,
            kind: ResolutionKind::OperatorCompleted,
            resolved_at: observed_at,
            operator_audit: Some(OperatorAudit {
                actor_uid,
                actor_pid,
                reason,
                reason_digest,
                pre_revision,
                post_revision,
                observed_at,
            }),
        });
        run.validate()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        self.store.save_run(&run)?;
        self.index_run(&run)?;
        Ok(run)
    }

    pub fn get_operation(&self, reference: &OperationRef) -> Result<OperationRecord, StoreError> {
        self.validate_operation_ref(reference)?;
        self.required_operation(&reference.operation_id)
    }

    pub fn read_response(&self, reference: &RunRef) -> Result<String, StoreError> {
        self.validate_run_ref(reference)?;
        self.store.read_artifact(&reference.run_id)
    }

    pub fn run_ref(&self, run_id: StableRunId) -> RunRef {
        RunRef {
            server_identity: self.server_identity.clone(),
            generation: self.store.generation().clone(),
            run_id,
        }
    }

    pub fn operation_ref(&self, operation_id: OperationId) -> OperationRef {
        OperationRef {
            server_identity: self.server_identity.clone(),
            generation: self.store.generation().clone(),
            operation_id,
        }
    }

    fn rebuild_indexes(&mut self) -> Result<(), StoreError> {
        for run in self.store.list_runs()? {
            self.index_run(&run)?;
        }
        self.store.for_each_operation(|operation| {
            if matches!(
                operation.dispatch_state,
                DispatchState::Prepared
                    | DispatchState::DispatchStarted
                    | DispatchState::DeliveryUnknown
            ) {
                self.in_flight_by_binding.insert(
                    operation_target_key(&operation.binding)?,
                    operation.operation_id.clone(),
                );
            }
            Ok(())
        })
    }

    fn reconcile_in_flight_after_restart(&mut self) -> Result<(), StoreError> {
        let runs = self.store.list_runs()?;
        for run in &runs {
            if run.operation_id.is_some() {
                self.confirm_linked_operation(run, run.updated_at)?;
            }
        }
        let operation_ids = self
            .in_flight_by_binding
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for operation_id in operation_ids {
            let operation = self.required_operation(&operation_id)?;
            let observed_at = epoch_seconds();
            match operation.dispatch_state {
                DispatchState::Prepared => {
                    if operation.confirmation_deadline_at <= observed_at {
                        self.settle_dispatch(
                            &operation_id,
                            DispatchState::Rejected,
                            "prepared_dispatch_timeout_after_restart",
                            observed_at,
                        )?;
                    }
                }
                DispatchState::DispatchStarted => {
                    self.settle_dispatch(
                        &operation_id,
                        DispatchState::DeliveryUnknown,
                        "daemon_restarted_during_dispatch",
                        observed_at,
                    )?;
                }
                DispatchState::DeliveryUnknown
                | DispatchState::PromptConfirmed
                | DispatchState::Rejected => {}
            }
        }
        Ok(())
    }

    fn collect_run_retention(
        &mut self,
        observed_at: i64,
        reserve: RunRetentionReserve,
        incoming_pane: Option<&crate::pane_state::PaneInstance>,
    ) -> Result<(), StoreError> {
        let protected = self.current_by_pane.values().cloned().collect::<Vec<_>>();
        let cleanup =
            self.store
                .collect_run_retention(observed_at, protected, reserve, incoming_pane)?;
        if cleanup.removed_run_ids.is_empty() {
            return Ok(());
        }
        let removed = cleanup
            .removed_run_ids
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        self.turn_index
            .retain(|_, run_id| !removed.contains(run_id));
        self.event_index
            .retain(|_, run_id| !removed.contains(run_id));
        self.current_by_binding
            .retain(|_, run_id| !removed.contains(run_id));
        self.current_by_pane
            .retain(|_, run_id| !removed.contains(run_id));
        if self.event_index.len() > RUN_STORE_MAX_RECORDS * super::RUN_EVENT_REFERENCE_MAX_COUNT {
            return Err(StoreError::Capacity(
                "provider event index exceeds the declared global bound".to_string(),
            ));
        }
        Ok(())
    }

    fn index_run(&mut self, run: &RunRecord) -> Result<(), StoreError> {
        let key = binding_key(&run.binding)?;
        let replace_current = match self.current_by_binding.get(&key) {
            Some(existing_id) => match self.store.load_run(existing_id)? {
                Some(existing)
                    if existing.run_seq == run.run_seq && existing.run_id != run.run_id =>
                {
                    return Err(StoreError::Corrupt(format!(
                        "Agent Binding has duplicate run sequence {}",
                        run.run_seq
                    )));
                }
                Some(existing) => existing.run_seq < run.run_seq || existing.run_id == run.run_id,
                None => true,
            },
            None => true,
        };
        if let Some(turn_key) = &run.provider_turn_key {
            let key = turn_index_key_for_binding(&run.binding, turn_key);
            if self
                .turn_index
                .get(&key)
                .is_some_and(|existing| existing != &run.run_id)
            {
                return Err(StoreError::Corrupt(
                    "provider turn key is attributed to multiple runs".to_string(),
                ));
            }
        }
        if run.evidence.provider_events.iter().any(|event| {
            self.event_index
                .get(&event.event_ref)
                .is_some_and(|existing| existing != &run.run_id)
        }) {
            return Err(StoreError::Corrupt(
                "provider event reference is attributed to multiple runs".to_string(),
            ));
        }
        let additional_event_references = run
            .evidence
            .provider_events
            .iter()
            .filter(|event| !self.event_index.contains_key(&event.event_ref))
            .count();
        let event_index_limit = RUN_STORE_MAX_RECORDS * super::RUN_EVENT_REFERENCE_MAX_COUNT;
        if self
            .event_index
            .len()
            .checked_add(additional_event_references)
            .is_none_or(|next_len| next_len > event_index_limit)
        {
            return Err(StoreError::Capacity(format!(
                "provider event index exceeds the {event_index_limit}-entry limit"
            )));
        }
        if replace_current {
            self.current_by_binding.insert(key, run.run_id.clone());
            let replace_pane_current = match self.current_by_pane.get(&run.binding.pane_instance) {
                Some(existing_id) => match self.store.load_run(existing_id)? {
                    Some(existing) => {
                        (
                            existing.binding.agent_epoch,
                            existing.updated_at,
                            existing.run_seq,
                            existing.run_id.as_str(),
                        ) <= (
                            run.binding.agent_epoch,
                            run.updated_at,
                            run.run_seq,
                            run.run_id.as_str(),
                        )
                    }
                    None => true,
                },
                None => true,
            };
            if replace_pane_current {
                self.current_by_pane
                    .insert(run.binding.pane_instance.clone(), run.run_id.clone());
            }
        }
        if let Some(turn_key) = &run.provider_turn_key {
            let key = turn_index_key_for_binding(&run.binding, turn_key);
            self.turn_index.insert(key, run.run_id.clone());
        }
        for event in &run.evidence.provider_events {
            self.event_index
                .insert(event.event_ref.clone(), run.run_id.clone());
        }
        Ok(())
    }

    fn matching_operation(
        &mut self,
        binding: &AgentBinding,
        run_seq: u64,
        observation: &ProviderObservation,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let prompt_digest = observation
            .prompt_digest
            .as_ref()
            .ok_or_else(|| StoreError::Invalid("prompt digest is absent".to_string()))?;
        let key = operation_target_key_for_agent(binding)?;
        let Some(operation_id) = self.in_flight_by_binding.get(&key) else {
            return Ok(None);
        };
        let operation = self.required_operation(operation_id)?;
        let matches = operation_binding_matches_agent(&operation.binding, binding)
            && operation.expected_run_seq == run_seq
            && operation.prompt_digest.as_str() == prompt_digest
            && observation.observed_at >= operation.updated_at
            && observation.observed_at <= operation.confirmation_deadline_at
            && matches!(
                operation.dispatch_state,
                DispatchState::DispatchStarted | DispatchState::DeliveryUnknown
            );
        if !matches {
            return Ok(None);
        }
        Ok(Some(operation))
    }

    fn confirm_linked_operation(
        &mut self,
        run: &RunRecord,
        observed_at: i64,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let Some(operation_id) = run.operation_id.as_ref() else {
            return Ok(None);
        };
        let mut operation = self.required_operation(operation_id)?;
        if operation.dispatch_state == DispatchState::PromptConfirmed {
            if operation.run_id.as_ref() != Some(&run.run_id) {
                return Err(StoreError::Corrupt(
                    "confirmed operation points to another run".to_string(),
                ));
            }
            self.in_flight_by_binding
                .remove(&operation_target_key(&operation.binding)?);
            self.store.delete_prompt(operation_id)?;
            return Ok(Some(operation));
        }
        if !matches!(
            operation.dispatch_state,
            DispatchState::DispatchStarted | DispatchState::DeliveryUnknown
        ) {
            return Err(StoreError::OperationConflict(format!(
                "linked operation cannot be confirmed from {:?}",
                operation.dispatch_state
            )));
        }
        if operation.binding.is_provider_session_pending() {
            if !operation_binding_matches_agent(&operation.binding, &run.binding) {
                return Err(StoreError::OperationConflict(
                    "pending provider session does not match the confirmed run binding".to_string(),
                ));
            }
            operation.binding = OperationBinding::from(run.binding.clone());
            operation.expected_pane_version = crate::pane_state::StateVersion {
                state_id: run.binding.pane_state_id.clone(),
                agent_epoch: run.binding.agent_epoch,
                revision: operation.expected_pane_version.revision,
            };
        }
        operation.dispatch_state = DispatchState::PromptConfirmed;
        operation.run_id = Some(run.run_id.clone());
        operation.result_receipt = Some(OperationResultReceipt {
            code: "prompt_confirmed".to_string(),
            observed_at,
            confirmation_basis: Some("guarded_window_digest".to_string()),
            source_attribution: Some("non_exclusive".to_string()),
        });
        advance_operation_revision(&mut operation, observed_at)?;
        self.store.save_operation(&operation)?;
        self.in_flight_by_binding
            .remove(&operation_target_key(&operation.binding)?);
        self.store.delete_prompt(operation_id)?;
        Ok(Some(operation))
    }

    fn required_run(&self, run_id: &StableRunId) -> Result<RunRecord, StoreError> {
        self.store.load_run(run_id)?.ok_or_else(|| {
            StoreError::RunNotFound(format!("run {} was not found", run_id.as_str()))
        })
    }

    fn required_operation(
        &self,
        operation_id: &OperationId,
    ) -> Result<OperationRecord, StoreError> {
        self.store.load_operation(operation_id)?.ok_or_else(|| {
            StoreError::OperationNotFound(format!(
                "operation {} was not found",
                operation_id.as_str()
            ))
        })
    }

    fn validate_run_ref(&self, reference: &RunRef) -> Result<(), StoreError> {
        if reference.server_identity != self.server_identity
            || reference.generation != *self.store.generation()
        {
            return Err(StoreError::RunGenerationReplaced(
                "run reference belongs to another server generation".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_operation_ref(&self, reference: &OperationRef) -> Result<(), StoreError> {
        if reference.server_identity != self.server_identity
            || reference.generation != *self.store.generation()
        {
            return Err(StoreError::OperationGenerationReplaced(
                "operation reference belongs to another server generation".to_string(),
            ));
        }
        Ok(())
    }
}

fn advance_operation_revision(
    operation: &mut OperationRecord,
    observed_at: i64,
) -> Result<(), StoreError> {
    operation.revision = operation
        .revision
        .checked_add(1)
        .ok_or_else(|| StoreError::Invalid("operation revision overflow".to_string()))?;
    operation.updated_at = operation.updated_at.max(observed_at);
    Ok(())
}

fn binding_key(binding: &AgentBinding) -> Result<String, StoreError> {
    serde_json::to_vec(binding)
        .map(|encoded| format!("{:x}", Sha256::digest(encoded)))
        .map_err(|error| StoreError::Invalid(format!("failed to encode Agent Binding: {error}")))
}

fn operation_target_key(binding: &OperationBinding) -> Result<String, StoreError> {
    serde_json::to_vec(&(
        &binding.server_identity,
        &binding.pane_instance,
        &binding.pane_state_id,
        &binding.agent_kind,
        &binding.process,
    ))
    .map(|encoded| format!("{:x}", Sha256::digest(encoded)))
    .map_err(|error| {
        StoreError::Invalid(format!(
            "failed to encode operation dispatch target: {error}"
        ))
    })
}

fn operation_target_key_for_agent(binding: &AgentBinding) -> Result<String, StoreError> {
    operation_target_key(&OperationBinding::from(binding.clone()))
}

fn operation_binding_matches_agent(operation: &OperationBinding, agent: &AgentBinding) -> bool {
    let epoch_matches = if operation.provider_session_id.is_some() {
        operation.agent_epoch == agent.agent_epoch
    } else {
        operation.agent_epoch == agent.agent_epoch
            || operation.agent_epoch.checked_add(1) == Some(agent.agent_epoch)
    };
    operation.server_identity == agent.server_identity
        && operation.pane_instance == agent.pane_instance
        && operation.pane_state_id == agent.pane_state_id
        && epoch_matches
        && operation.agent_kind == agent.agent_kind
        && operation
            .provider_session_id
            .as_ref()
            .is_none_or(|session| session == &agent.provider_session_id)
        && operation.process == agent.process
}

fn turn_index_key(observation: &ProviderObservation, turn_key: &str) -> String {
    let mut hasher = Sha256::new();
    for field in [
        observation.provider.as_str().as_bytes(),
        observation.session_id.as_str().as_bytes(),
        turn_key.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

fn turn_index_key_for_binding(binding: &AgentBinding, turn_key: &str) -> String {
    let mut hasher = Sha256::new();
    for field in [
        binding.agent_kind.as_str().as_bytes(),
        binding.provider_session_id.as_str().as_bytes(),
        turn_key.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

fn pane_matches_binding(pane: &crate::pane_state::PaneState, binding: &AgentBinding) -> bool {
    pane.pane_instance == binding.pane_instance
        && pane.state_id == binding.pane_state_id
        && pane.agent_epoch == binding.agent_epoch
        && pane.agent == binding.agent_kind
        && pane.agent_session_id.as_ref() == Some(&binding.provider_session_id)
        && pane.agent_process.as_ref() == Some(&binding.process)
        && pane.agent_present
}

fn epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::topology::ServerIdentity;
    use crate::hook::provider::{ProviderCompleteness, ResponseCandidate};
    use crate::pane_state::{
        AgentKind, AgentProcessIdentity, AgentSessionId, CurrentDurableRunProjection, EventId,
        LifecycleState, PaneInstance, StateId, StateVersion,
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
            agent_kind: AgentKind::parse("claude").unwrap(),
            provider_session_id: AgentSessionId::parse("session-runtime-test").unwrap(),
            process: AgentProcessIdentity {
                pid: 88,
                start_token: "process-start-token".to_string(),
            },
        }
    }

    fn pane_version(binding: &AgentBinding) -> StateVersion {
        StateVersion {
            state_id: binding.pane_state_id.clone(),
            agent_epoch: binding.agent_epoch,
            revision: 7,
        }
    }

    fn prompt_digest(prompt: &[u8]) -> Sha256Digest {
        Sha256Digest::parse(crate::pane_state::PromptState::digest_decoded_prompt(
            std::str::from_utf8(prompt).unwrap(),
        ))
        .unwrap()
    }

    fn observation(
        kind: ProviderHookKind,
        turn: &str,
        prompt: Option<&[u8]>,
        response: Option<&str>,
        observed_at: i64,
    ) -> ProviderObservation {
        let binding = binding();
        let event_ref = Sha256Digest::of(format!("{turn}:{kind:?}").as_bytes());
        ProviderObservation {
            ingress_request_id: EventId::generate().unwrap(),
            provider: binding.agent_kind,
            session_id: binding.provider_session_id,
            hook_kind: kind,
            provider_turn_key: Some(turn.to_string()),
            provider_event_ref: Some(event_ref.as_str().to_string()),
            payload_digest: Sha256Digest::of(format!("payload:{turn}:{kind:?}").as_bytes())
                .as_str()
                .to_string(),
            prompt_digest: prompt.map(|body| prompt_digest(body).as_str().to_string()),
            response: response
                .map(|body| ResponseCandidate::from_body(body, ProviderCompleteness::Complete)),
            observed_at,
        }
    }

    fn temp_root() -> PathBuf {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        std::env::temp_dir().join(format!(
            "vde-tmux-agent-runtime-test-{}-{:016x}",
            std::process::id(),
            u64::from_be_bytes(random)
        ))
    }

    fn pane_fence(run: &RunRecord) -> RecoveryPaneFence {
        RecoveryPaneFence {
            state_id: run.binding.pane_state_id.clone(),
            revision: 7,
            current_run: CurrentDurableRunProjection {
                run_id: run.run_id.as_str().to_string(),
                run_seq: run.run_seq,
                run_revision: run.revision,
            },
            lifecycle: LifecycleState::Running,
            subagent_count: 0,
        }
    }

    #[test]
    fn operation_is_confirmed_by_prompt_and_stop_persists_response() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let prompt = b"review this";
        let operation_id = OperationId::parse("operation_runtime_0001").unwrap();
        let created = runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                10,
            )
            .unwrap();
        assert!(matches!(created, PrepareOperationResult::Created(_)));
        runtime.mark_dispatch_started(&operation_id, 11).unwrap();

        let begin = runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-1",
                    Some(prompt),
                    None,
                    12,
                ),
            )
            .unwrap();
        let operation = begin.operation.unwrap();
        assert_eq!(operation.dispatch_state, DispatchState::PromptConfirmed);
        assert!(matches!(
            runtime.store.read_prompt(&operation_id),
            Err(StoreError::NotFound(_))
        ));
        let run_id = begin.run.unwrap().run_id;
        assert_eq!(
            runtime
                .current_run_for_binding(&agent_binding)
                .unwrap()
                .unwrap()
                .run_id,
            run_id
        );
        let mut replaced_binding = agent_binding.clone();
        replaced_binding.process.pid += 1;
        assert!(
            runtime
                .current_run_for_binding(&replaced_binding)
                .unwrap()
                .is_none()
        );

        let stopped = runtime
            .apply_provider_observation(
                agent_binding,
                1,
                &observation(
                    ProviderHookKind::Stop,
                    "turn-1",
                    None,
                    Some("final response"),
                    13,
                ),
            )
            .unwrap()
            .run
            .unwrap();
        assert_eq!(
            stopped.semantic_outcome,
            super::super::SemanticOutcome::Completed
        );
        let reference = runtime.run_ref(run_id);
        assert_eq!(runtime.read_response(&reference).unwrap(), "final response");

        drop(runtime);
        let reopened = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        assert_eq!(
            reopened.get_run(&reference).unwrap().semantic_outcome,
            super::super::SemanticOutcome::Completed
        );
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_prompt_binds_a_pending_provider_session_to_the_same_process() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let mut confirmed_binding = binding();
        confirmed_binding.agent_kind = AgentKind::parse("codex").unwrap();
        confirmed_binding.agent_epoch = 2;
        let mut pending_binding = OperationBinding::from(confirmed_binding.clone());
        pending_binding.agent_epoch = 1;
        pending_binding.provider_session_id = None;
        let prompt = b"first prompt";
        let operation_id = OperationId::parse("operation_pending_session_0001").unwrap();
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:unbound-exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                pending_binding,
                StateVersion {
                    state_id: confirmed_binding.pane_state_id.clone(),
                    agent_epoch: 1,
                    revision: 7,
                },
                None,
                1,
                10,
            )
            .unwrap();
        runtime.mark_dispatch_started(&operation_id, 11).unwrap();

        let mut first_prompt = observation(
            ProviderHookKind::UserPromptSubmit,
            "turn-first",
            Some(prompt),
            None,
            12,
        );
        first_prompt.provider = confirmed_binding.agent_kind.clone();
        first_prompt.session_id = confirmed_binding.provider_session_id.clone();
        let applied = runtime
            .apply_provider_observation(confirmed_binding.clone(), 1, &first_prompt)
            .unwrap();
        let operation = applied.operation.unwrap();
        assert_eq!(operation.dispatch_state, DispatchState::PromptConfirmed);
        assert_eq!(
            operation.binding.provider_session_id.as_ref(),
            Some(&confirmed_binding.provider_session_id)
        );
        assert_eq!(operation.binding.agent_epoch, confirmed_binding.agent_epoch);
        assert_eq!(
            operation.expected_pane_version.agent_epoch,
            confirmed_binding.agent_epoch
        );
        assert_eq!(
            applied.run.unwrap().binding.provider_session_id,
            confirmed_binding.provider_session_id
        );

        drop(runtime);
        let reopened = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let reference = reopened.operation_ref(operation_id);
        assert_eq!(
            reopened.get_operation(&reference).unwrap().dispatch_state,
            DispatchState::PromptConfirmed
        );
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_staging_capacity_is_reported_as_operation_store_full() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        for ordinal in 0..super::super::store::PROMPT_STORE_MAX_RECORDS {
            let operation_id =
                OperationId::parse(format!("operation_prompt_capacity_{ordinal:04}")).unwrap();
            runtime
                .store
                .stage_prompt(&operation_id, b"occupied")
                .unwrap();
        }

        let prompt = b"one prompt too many";
        let agent_binding = binding();
        assert!(matches!(
            runtime.prepare_operation(
                OperationId::parse("operation_prompt_capacity_overflow").unwrap(),
                "vta1:exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                10,
            ),
            Err(StoreError::OperationStoreFull(_))
        ));

        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operation_retry_conflict_parallel_dispatch_and_human_interleave_are_fail_closed() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let operation_id = OperationId::parse("operation_runtime_retry").unwrap();
        let prompt = b"agent-owned prompt";

        let created = runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                10,
            )
            .unwrap();
        assert!(matches!(created, PrepareOperationResult::Created(_)));

        let retry = runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                11,
            )
            .unwrap();
        assert!(matches!(retry, PrepareOperationResult::Existing(_)));

        let conflicting_prompt = b"different request";
        assert!(matches!(
            runtime.prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                conflicting_prompt,
                prompt_digest(conflicting_prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                12,
            ),
            Err(StoreError::OperationConflict(_))
        ));

        assert!(matches!(
            runtime.prepare_operation(
                OperationId::parse("operation_runtime_parallel").unwrap(),
                "vta1:exact-target".to_string(),
                b"parallel request",
                prompt_digest(b"parallel request"),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                13,
            ),
            Err(StoreError::PromptDispatchBusy(_))
        ));

        assert!(matches!(
            runtime.prepare_operation(
                OperationId::parse("operation_runtime_bad_digest").unwrap(),
                "vta1:exact-target".to_string(),
                b"digest guarded",
                Sha256Digest::of(b"not the domain-separated prompt digest"),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                14,
            ),
            Err(StoreError::Invalid(_))
        ));

        let interleaved = runtime
            .apply_provider_observation(
                agent_binding,
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-human-interleave",
                    Some(b"human prompt"),
                    None,
                    15,
                ),
            )
            .unwrap();
        assert!(interleaved.operation.is_none());
        assert!(interleaved.run.unwrap().operation_id.is_none());
        let operation = runtime
            .store
            .load_operation(&operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(operation.dispatch_state, DispatchState::Prepared);
        assert!(operation.run_id.is_none());

        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dispatch_started_human_interleave_digest_mismatch_never_confirms_the_operation() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let operation_id = OperationId::parse("operation_dispatch_interleave").unwrap();
        let prompt = b"agent-owned prompt";
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                10,
            )
            .unwrap();
        runtime.mark_dispatch_started(&operation_id, 11).unwrap();

        let interleaved = runtime
            .apply_provider_observation(
                agent_binding,
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-human-after-dispatch-started",
                    Some(b"human prompt"),
                    None,
                    12,
                ),
            )
            .unwrap();
        assert!(interleaved.operation.is_none());
        assert!(interleaved.run.unwrap().operation_id.is_none());
        let operation = runtime
            .store
            .load_operation(&operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(operation.dispatch_state, DispatchState::DispatchStarted);
        assert!(operation.run_id.is_none());

        let settled = runtime.settle_expired_dispatches(20).unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].dispatch_state, DispatchState::DeliveryUnknown);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_retry_at_deadline_is_rejected_without_starting_dispatch() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let operation_id = OperationId::parse("operation_expired_prepared_retry").unwrap();
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                b"expires",
                prompt_digest(b"expires"),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                10,
            )
            .unwrap();

        assert!(
            runtime
                .reject_prepared_retry_if_expired(&operation_id, 19)
                .unwrap()
                .is_none()
        );
        let rejected = runtime
            .reject_prepared_retry_if_expired(&operation_id, 20)
            .unwrap()
            .unwrap();
        assert_eq!(rejected.dispatch_state, DispatchState::Rejected);
        assert_eq!(
            rejected.result_receipt.unwrap().code,
            "prepared_dispatch_timeout"
        );
        assert!(matches!(
            runtime.store.read_prompt(&operation_id),
            Err(StoreError::NotFound(_))
        ));
        assert!(
            runtime
                .prepare_operation(
                    OperationId::parse("operation_after_expired_prepared").unwrap(),
                    "vta1:exact-target".to_string(),
                    b"next",
                    prompt_digest(b"next"),
                    "paste_enter".to_string(),
                    agent_binding.clone(),
                    pane_version(&agent_binding),
                    None,
                    1,
                    21,
                )
                .is_ok()
        );
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delivery_unknown_can_be_confirmed_late_without_redispatch() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let prompt = b"late evidence";
        let operation_id = OperationId::parse("operation_runtime_0002").unwrap();
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                20,
            )
            .unwrap();
        runtime.mark_dispatch_started(&operation_id, 21).unwrap();
        runtime
            .settle_dispatch(
                &operation_id,
                DispatchState::DeliveryUnknown,
                "ambiguous",
                22,
            )
            .unwrap();
        let applied = runtime
            .apply_provider_observation(
                agent_binding,
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-late",
                    Some(prompt),
                    None,
                    23,
                ),
            )
            .unwrap();
        assert_eq!(
            applied.operation.unwrap().dispatch_state,
            DispatchState::PromptConfirmed
        );
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stable_turn_mismatch_does_not_fall_back_to_current_run() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-current",
                    Some(b"manual"),
                    None,
                    30,
                ),
            )
            .unwrap();
        let error = runtime
            .apply_provider_observation(
                agent_binding,
                1,
                &observation(
                    ProviderHookKind::Stop,
                    "turn-other",
                    None,
                    Some("wrong"),
                    31,
                ),
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::NotFound(_)));
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_stop_finishes_operation_and_artifact_side_effects_idempotently() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let prompt = b"duplicate completion";
        let operation_id = OperationId::parse("operation_runtime_duplicate").unwrap();
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                10,
            )
            .unwrap();
        runtime.mark_dispatch_started(&operation_id, 11).unwrap();
        let prompt_observation = observation(
            ProviderHookKind::UserPromptSubmit,
            "turn-duplicate",
            Some(prompt),
            None,
            12,
        );
        let stop = observation(
            ProviderHookKind::Stop,
            "turn-duplicate",
            None,
            Some("stable response"),
            13,
        );

        // Reproduce a crash after the completed Run and Stop evidence are durable,
        // but before the linked Operation and response artifact are repaired.
        let mut partial = new_run_from_prompt(
            runtime.store.generation().clone(),
            agent_binding.clone(),
            1,
            Some(operation_id.clone()),
            &prompt_observation,
        )
        .unwrap();
        assert_eq!(
            apply_observation(&mut partial, &stop).unwrap(),
            ApplyDisposition::Applied
        );
        runtime.store.save_run(&partial).unwrap();
        runtime.index_run(&partial).unwrap();
        assert!(partial.artifact.is_none());
        assert_eq!(
            runtime
                .store
                .load_operation(&operation_id)
                .unwrap()
                .unwrap()
                .dispatch_state,
            DispatchState::DispatchStarted
        );

        let duplicate = runtime
            .apply_provider_observation(agent_binding, 1, &stop)
            .unwrap();
        assert_eq!(duplicate.disposition, ApplyDisposition::Duplicate);
        assert_eq!(
            duplicate.operation.unwrap().dispatch_state,
            DispatchState::PromptConfirmed
        );
        let run = duplicate.run.unwrap();
        assert!(run.artifact.is_some());
        assert_eq!(
            runtime
                .read_response(&runtime.run_ref(partial.run_id))
                .unwrap(),
            "stable response"
        );
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_confirms_an_operation_already_linked_by_a_durable_run() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let prompt = b"persist before projection";
        let operation_id = OperationId::parse("operation_runtime_restart").unwrap();
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                prompt,
                prompt_digest(prompt),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                10,
            )
            .unwrap();
        runtime.mark_dispatch_started(&operation_id, 11).unwrap();
        let prompt_observation = observation(
            ProviderHookKind::UserPromptSubmit,
            "turn-restart",
            Some(prompt),
            None,
            12,
        );
        let run = new_run_from_prompt(
            runtime.store.generation().clone(),
            agent_binding,
            1,
            Some(operation_id.clone()),
            &prompt_observation,
        )
        .unwrap();
        runtime.store.save_run(&run).unwrap();
        drop(runtime);

        let reopened = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let operation = reopened
            .store
            .load_operation(&operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(operation.dispatch_state, DispatchState::PromptConfirmed);
        assert_eq!(operation.run_id, Some(run.run_id));
        assert!(matches!(
            reopened.store.read_prompt(&operation_id),
            Err(StoreError::NotFound(_))
        ));
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmation_timeout_settles_and_releases_prompt_staging() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let operation_id = OperationId::parse("operation_runtime_timeout").unwrap();
        let agent_binding = binding();
        runtime
            .prepare_operation(
                operation_id.clone(),
                "vta1:exact-target".to_string(),
                b"timeout",
                prompt_digest(b"timeout"),
                "paste_enter".to_string(),
                agent_binding.clone(),
                pane_version(&agent_binding),
                None,
                1,
                10,
            )
            .unwrap();
        runtime.mark_dispatch_started(&operation_id, 11).unwrap();
        let settled = runtime.settle_expired_dispatches(20).unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].dispatch_state, DispatchState::DeliveryUnknown);
        assert!(matches!(
            runtime.store.read_prompt(&operation_id),
            Err(StoreError::NotFound(_))
        ));
        let replacement = runtime.prepare_operation(
            OperationId::parse("operation_runtime_after_timeout").unwrap(),
            "vta1:exact-target".to_string(),
            b"next",
            prompt_digest(b"next"),
            "paste_enter".to_string(),
            agent_binding.clone(),
            pane_version(&agent_binding),
            None,
            1,
            21,
        );
        assert!(matches!(
            replacement,
            Err(StoreError::PromptDispatchBusy(_))
        ));
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_run_creation_applies_per_pane_retention_and_prunes_indexes() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        for run_seq in 1..=(super::super::RUN_RETENTION_PER_PANE as u64 + 8) {
            let prompt = format!("prompt-{run_seq}");
            runtime
                .apply_provider_observation(
                    agent_binding.clone(),
                    run_seq,
                    &observation(
                        ProviderHookKind::UserPromptSubmit,
                        &format!("turn-{run_seq}"),
                        Some(prompt.as_bytes()),
                        None,
                        run_seq as i64,
                    ),
                )
                .unwrap();
        }

        assert_eq!(
            runtime.store.list_runs().unwrap().len(),
            super::super::RUN_RETENTION_PER_PANE + 1
        );
        assert!(
            runtime.event_index.len()
                <= (super::super::RUN_RETENTION_PER_PANE + 1)
                    * super::super::RUN_EVENT_REFERENCE_MAX_COUNT
        );
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_event_index_reaches_but_never_exceeds_the_declared_global_bound() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let per_run = super::super::RUN_EVENT_REFERENCE_MAX_COUNT;

        for ordinal in 0..RUN_STORE_MAX_RECORDS {
            let mut agent_binding = binding();
            agent_binding.pane_instance = PaneInstance {
                pane_id: format!("%{}", ordinal + 1_000),
                pane_pid: (ordinal + 10_000) as u32,
            };
            agent_binding.pane_state_id = StateId::generate().unwrap();
            agent_binding.process = AgentProcessIdentity {
                pid: (ordinal + 20_000) as u32,
                start_token: format!("process-start-token-{ordinal}"),
            };
            let turn = format!("turn-stress-{ordinal}");
            let prompt = observation(
                ProviderHookKind::UserPromptSubmit,
                &turn,
                Some(b"stress"),
                None,
                1,
            );
            let mut run = new_run_from_prompt(
                runtime.store.generation().clone(),
                agent_binding,
                1,
                None,
                &prompt,
            )
            .unwrap();

            for event_ordinal in 1..per_run {
                let mut stop = observation(ProviderHookKind::Stop, &turn, None, None, 2);
                stop.provider_event_ref = Some(
                    Sha256Digest::of(format!("stress-event-{ordinal}-{event_ordinal}").as_bytes())
                        .as_str()
                        .to_string(),
                );
                stop.payload_digest = Sha256Digest::of(
                    format!("stress-payload-{ordinal}-{event_ordinal}").as_bytes(),
                )
                .as_str()
                .to_string();
                apply_observation(&mut run, &stop).unwrap();
            }
            assert_eq!(run.evidence.provider_events.len(), per_run);
            runtime.index_run(&run).unwrap();
        }

        assert_eq!(
            runtime.event_index.len(),
            RUN_STORE_MAX_RECORDS * super::super::RUN_EVENT_REFERENCE_MAX_COUNT
        );

        let mut extra_binding = binding();
        extra_binding.pane_instance = PaneInstance {
            pane_id: "%999999".to_string(),
            pane_pid: 999_999,
        };
        extra_binding.pane_state_id = StateId::generate().unwrap();
        extra_binding.process = AgentProcessIdentity {
            pid: 999_998,
            start_token: "process-start-token-over-global-bound".to_string(),
        };
        let extra_prompt = observation(
            ProviderHookKind::UserPromptSubmit,
            "turn-over-global-event-index-bound",
            Some(b"stress"),
            None,
            1,
        );
        let extra_run = new_run_from_prompt(
            runtime.store.generation().clone(),
            extra_binding,
            1,
            None,
            &extra_prompt,
        )
        .unwrap();
        assert!(matches!(
            runtime.index_run(&extra_run),
            Err(StoreError::Capacity(_))
        ));
        assert_eq!(
            runtime.event_index.len(),
            RUN_STORE_MAX_RECORDS * super::super::RUN_EVENT_REFERENCE_MAX_COUNT
        );
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_exit_ends_the_run_without_resolving_it() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-process-exit",
                    Some(b"manual"),
                    None,
                    10,
                ),
            )
            .unwrap();
        let ended = runtime
            .reconcile_process_for_pane(&agent_binding.pane_instance, true, None, 11)
            .unwrap()
            .unwrap();
        assert_eq!(ended.execution_phase, ExecutionPhase::Ended);
        assert_eq!(ended.semantic_outcome, SemanticOutcome::Unresolved);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_removal_releases_a_completed_run_from_the_current_retention_index() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-completed-removal",
                    Some(b"manual"),
                    None,
                    10,
                ),
            )
            .unwrap();
        runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::Stop,
                    "turn-completed-removal",
                    None,
                    Some("done"),
                    11,
                ),
            )
            .unwrap();
        assert!(
            runtime
                .current_run_for_binding(&agent_binding)
                .unwrap()
                .is_some()
        );

        let ended = runtime
            .reconcile_process_for_pane(&agent_binding.pane_instance, true, None, 12)
            .unwrap();
        assert!(
            ended.is_none(),
            "a completed run does not need an Ended transition"
        );
        assert!(
            runtime
                .current_run_for_binding(&agent_binding)
                .unwrap()
                .is_none(),
            "a removed pane must not protect a completed historical run forever"
        );
        assert_eq!(runtime.store.list_runs().unwrap().len(), 1);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn historical_provider_retry_does_not_retire_the_replacement_binding() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let original = binding();
        let original_run = runtime
            .apply_provider_observation(
                original.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-original",
                    Some(b"original"),
                    None,
                    10,
                ),
            )
            .unwrap()
            .run
            .unwrap();
        let original_ref = runtime.run_ref(original_run.run_id.clone());
        let original_stop = observation(
            ProviderHookKind::Stop,
            "turn-original",
            None,
            Some("done"),
            11,
        );
        runtime
            .apply_provider_observation(original.clone(), 1, &original_stop)
            .unwrap();

        let mut replacement = original.clone();
        replacement.pane_state_id = StateId::generate().unwrap();
        replacement.agent_epoch = 2;
        replacement.process = AgentProcessIdentity {
            pid: 99,
            start_token: "replacement-process-start-token".to_string(),
        };
        runtime
            .apply_provider_observation(
                replacement.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-replacement",
                    Some(b"replacement"),
                    None,
                    12,
                ),
            )
            .unwrap();

        let duplicate = runtime
            .apply_provider_observation(original.clone(), 1, &original_stop)
            .unwrap();
        assert_eq!(duplicate.disposition, ApplyDisposition::Duplicate);
        let replacement_run = runtime
            .current_run_for_binding(&replacement)
            .unwrap()
            .expect("replacement binding must remain current");
        assert!(replacement_run.execution_active());
        assert!(
            runtime
                .current_run_for_binding(&original)
                .unwrap()
                .is_none(),
            "a historical retry must not become current again"
        );
        let historical = runtime.get_run(&original_ref).unwrap();
        assert_eq!(historical.run_id, original_run.run_id);
        assert_eq!(historical.semantic_outcome, SemanticOutcome::Completed);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_second_run_cannot_reuse_the_same_binding_sequence() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-first",
                    Some(b"first"),
                    None,
                    10,
                ),
            )
            .unwrap();
        let error = runtime
            .apply_provider_observation(
                agent_binding,
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-second",
                    Some(b"second"),
                    None,
                    11,
                ),
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::ProviderEventConflict(_)));
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operator_completion_is_cas_guarded_and_idempotent() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let run = runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-recovery",
                    Some(b"manual"),
                    None,
                    40,
                ),
            )
            .unwrap()
            .run
            .unwrap();
        let reference = runtime.run_ref(run.run_id.clone());
        let pane = pane_fence(&run);
        let precondition = RecoveryPrecondition {
            run_ref: reference.encode().unwrap(),
            binding: agent_binding,
            run_revision: run.revision,
            evidence_digest: AgentRuntime::evidence_digest(&run).unwrap(),
            pane: pane.clone(),
            viewport_fingerprint: None,
            process_expectation: RecoveryProcessExpectation::ExactAbsent,
            issued_at: 41,
            expires_at: 101,
        };
        let resolution_id = ResolutionId::parse("resolution_runtime_0001").unwrap();
        let stale = runtime
            .resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id.clone(),
                "missing Stop hook".to_string(),
                501,
                502,
                42,
                &pane,
                Some(binding().process),
                None,
            )
            .unwrap_err();
        assert!(matches!(stale, StoreError::StalePrecondition(_)));
        assert_eq!(runtime.get_run(&reference).unwrap().revision, run.revision);

        let completed = runtime
            .resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id.clone(),
                "missing Stop hook".to_string(),
                501,
                502,
                42,
                &pane,
                None,
                None,
            )
            .unwrap();
        assert_eq!(completed.semantic_outcome, SemanticOutcome::Completed);
        assert_eq!(completed.execution_phase, ExecutionPhase::Ended);
        assert_eq!(completed.revision, run.revision + 1);

        let retried = runtime
            .resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id.clone(),
                "missing Stop hook".to_string(),
                501,
                502,
                200,
                &pane,
                None,
                None,
            )
            .unwrap();
        assert_eq!(retried, completed);
        let conflict = runtime
            .resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id,
                "another reason".to_string(),
                501,
                502,
                42,
                &pane,
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(conflict, StoreError::ResolutionConflict(_)));
        let already_resolved = runtime
            .resolve_operator_completed(
                &reference,
                &precondition,
                ResolutionId::parse("resolution_runtime_0002").unwrap(),
                "missing Stop hook".to_string(),
                501,
                502,
                42,
                &pane,
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            already_resolved,
            StoreError::RunAlreadyResolved(_)
        ));
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_present_completion_requires_the_stable_pane_and_viewport_fences() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let run = runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-exact-present",
                    Some(b"manual"),
                    None,
                    40,
                ),
            )
            .unwrap()
            .run
            .unwrap();
        let reference = runtime.run_ref(run.run_id.clone());
        let pane = pane_fence(&run);
        let viewport = RecoveryViewportFingerprint {
            convention_version: crate::agent_state::VIEWPORT_FINGERPRINT_CONVENTION_VERSION,
            pane_width: 80,
            pane_height: 24,
            digest: Sha256Digest::of(b"stable viewport"),
        };
        let precondition = RecoveryPrecondition {
            run_ref: reference.encode().unwrap(),
            binding: agent_binding.clone(),
            run_revision: run.revision,
            evidence_digest: AgentRuntime::evidence_digest(&run).unwrap(),
            pane: pane.clone(),
            viewport_fingerprint: Some(viewport.clone()),
            process_expectation: RecoveryProcessExpectation::ExactPresentStable {
                process: agent_binding.process.clone(),
            },
            issued_at: 41,
            expires_at: 101,
        };
        let resolution_id = ResolutionId::parse("resolution_exact_present_01").unwrap();

        let mut changed_pane = pane.clone();
        changed_pane.revision += 1;
        assert!(matches!(
            runtime.resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id.clone(),
                "lost provider completion".to_string(),
                501,
                502,
                42,
                &changed_pane,
                Some(agent_binding.process.clone()),
                Some(&viewport),
            ),
            Err(StoreError::StalePrecondition(_))
        ));

        let changed_viewport = RecoveryViewportFingerprint {
            digest: Sha256Digest::of(b"changed viewport"),
            ..viewport.clone()
        };
        assert!(matches!(
            runtime.resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id.clone(),
                "lost provider completion".to_string(),
                501,
                502,
                42,
                &pane,
                Some(agent_binding.process.clone()),
                Some(&changed_viewport),
            ),
            Err(StoreError::StalePrecondition(_))
        ));

        let mut subagent_changed = pane.clone();
        subagent_changed.subagent_count = 1;
        assert!(matches!(
            runtime.resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id.clone(),
                "lost provider completion".to_string(),
                501,
                502,
                42,
                &subagent_changed,
                Some(agent_binding.process.clone()),
                Some(&viewport),
            ),
            Err(StoreError::StalePrecondition(_))
        ));

        let mut pointer_changed = pane.clone();
        pointer_changed.current_run.run_revision += 1;
        assert!(matches!(
            runtime.resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id.clone(),
                "lost provider completion".to_string(),
                501,
                502,
                42,
                &pointer_changed,
                Some(agent_binding.process.clone()),
                Some(&viewport),
            ),
            Err(StoreError::StalePrecondition(_))
        ));

        let completed = runtime
            .resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id,
                "lost provider completion".to_string(),
                501,
                502,
                42,
                &pane,
                Some(agent_binding.process),
                Some(&viewport),
            )
            .unwrap();
        assert_eq!(completed.semantic_outcome, SemanticOutcome::Completed);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaced_process_completion_requires_the_exact_observed_replacement() {
        let root = temp_root();
        let mut runtime = AgentRuntime::open(root.clone(), "server-a".to_string()).unwrap();
        let agent_binding = binding();
        let run = runtime
            .apply_provider_observation(
                agent_binding.clone(),
                1,
                &observation(
                    ProviderHookKind::UserPromptSubmit,
                    "turn-replaced-recovery",
                    Some(b"manual"),
                    None,
                    40,
                ),
            )
            .unwrap()
            .run
            .unwrap();
        let reference = runtime.run_ref(run.run_id.clone());
        let pane = pane_fence(&run);
        let replacement = AgentProcessIdentity {
            pid: agent_binding.process.pid + 1,
            start_token: "replacement-process-start-token".to_string(),
        };
        let precondition = RecoveryPrecondition {
            run_ref: reference.encode().unwrap(),
            binding: agent_binding,
            run_revision: run.revision,
            evidence_digest: AgentRuntime::evidence_digest(&run).unwrap(),
            pane: pane.clone(),
            viewport_fingerprint: None,
            process_expectation: RecoveryProcessExpectation::ReplacedBy {
                process: replacement.clone(),
            },
            issued_at: 41,
            expires_at: 101,
        };
        let resolution_id = ResolutionId::parse("resolution_replaced_process").unwrap();

        let another_process = AgentProcessIdentity {
            pid: replacement.pid + 1,
            start_token: "another-process-start-token".to_string(),
        };
        assert!(matches!(
            runtime.resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id.clone(),
                "the original process was replaced".to_string(),
                501,
                502,
                42,
                &pane,
                Some(another_process),
                None,
            ),
            Err(StoreError::StalePrecondition(_))
        ));

        let completed = runtime
            .resolve_operator_completed(
                &reference,
                &precondition,
                resolution_id,
                "the original process was replaced".to_string(),
                501,
                502,
                42,
                &pane,
                Some(replacement),
                None,
            )
            .unwrap();
        assert_eq!(completed.semantic_outcome, SemanticOutcome::Completed);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }
}

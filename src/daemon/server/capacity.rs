//! Recovery is a private capability, separate from public idle prompt dispatch.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

use super::{ProductionV2Coordinator, V2InternalMutation, epoch_seconds};
use crate::agent_state::{
    AgentBinding, DispatchState, ExecutionPhase, OperationId, RunRecord, SemanticOutcome,
    StableRunId,
};
use crate::codex_capacity::{self as policy, CapacityConfig};
use crate::hook::provider::{ProviderHookKind, ProviderObservation};
use crate::pane_state::{PaneEvent, PaneEventEnvelope, PaneInstance, PaneState};
use crate::question_notice::ingress::TranscriptLocator;
use crate::tmux::TmuxRunner;
use ansi_to_tui::IntoText as _;
use base64::Engine as _;
use sha2::{Digest as _, Sha256};

#[derive(Clone)]
struct BoundTurn {
    locator: TranscriptLocator,
    size: u64,
    eligible: bool,
}
#[derive(Clone)]
struct Chain {
    run: RunRecord,
    prompt: String,
    attempts: usize,
    due: Option<Instant>,
    frame: Option<ReadyFrame>,
    failed_at: i64,
    pending: Option<OperationId>,
    reason: Option<String>,
}
#[derive(Default)]
pub(super) struct CapacityState {
    pub config: CapacityConfig,
    bound: BTreeMap<StableRunId, BoundTurn>,
    chains: BTreeMap<PaneInstance, Chain>,
    last_send: Option<Instant>,
    failures: u64,
    missing_locator: BTreeSet<StableRunId>,
    versions: BTreeMap<String, bool>,
}
impl CapacityState {
    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"enabled": self.config.enabled, "failures": self.failures,
            "chains": self.chains.iter().map(|(pane,c)| serde_json::json!({
                "pane_id": pane.pane_id, "run_id": c.run.run_id, "attempts": c.attempts,
                "state": if c.reason.is_some() {"stopped"} else if c.pending.is_some() {"awaiting_confirmation"} else if c.due.is_some() {"waiting"} else {"running"},
                "next_in_seconds": c.due.map(|t| t.saturating_duration_since(Instant::now()).as_secs()),
                "stop_reason": c.reason, "operation_id": c.pending,
            })).collect::<Vec<_>>()})
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReadyFrame {
    digest: [u8; 32],
    shape: String,
}
/// Only this module can construct a recovery capability.
pub(super) struct RecoveryCandidate {
    failed: RunRecord,
    locator: TranscriptLocator,
    size: u64,
    frame: ReadyFrame,
    failed_at: i64,
}
impl RecoveryCandidate {
    pub(super) fn matches(&self, record: &PaneState) -> bool {
        current(record, &self.failed)
            && matches!(&record.lifecycle, crate::pane_state::LifecycleState::Error { reason } if reason.as_deref() == Some(policy::REASON))
            && self.failed.execution_phase == ExecutionPhase::Error
            && self.failed.semantic_outcome == SemanticOutcome::Unresolved
    }
    pub(super) fn verify(
        &self,
        coordinator: &ProductionV2Coordinator,
        runner: &dyn TmuxRunner,
        record: &PaneState,
    ) -> Result<(), String> {
        if !self.matches(record) {
            return Err("failed run changed".into());
        }
        let tracker = coordinator
            .state
            .lock()
            .expect("state lock poisoned")
            .as_ref()
            .ok_or("daemon hydrating")?
            .leased
            .runtime
            .tracker(&record.pane_instance);
        if !tracker.hook_authoritative {
            return Err("hooks are not authoritative".into());
        }
        let active = runner
            .codex_active_transcript(
                &self.failed.binding.process,
                self.failed.binding.provider_session_id.as_str(),
            )
            .map_err(|_| "current Codex session is unavailable or changed")?;
        if active != self.locator {
            return Err("active Codex transcript changed".into());
        }
        let mut size = self.size;
        if !policy::read_failure(
            &self.locator,
            self.failed
                .provider_turn_key
                .as_deref()
                .ok_or("turn absent")?,
            &mut size,
        )
        .map_err(|e| e.to_string())?
        {
            return Err("terminal failure changed".into());
        }
        let actual = ready_frame(runner, &record.pane_instance)?;
        if actual != self.frame {
            return Err("viewport changed".into());
        }
        verify_attention(coordinator, runner, record, self.failed_at)
    }
}

fn current(record: &PaneState, run: &RunRecord) -> bool {
    record.agent_present
        && record.pane_instance == run.binding.pane_instance
        && record.state_id == run.binding.pane_state_id
        && record.agent_epoch == run.binding.agent_epoch
        && record.agent_session_id.as_ref() == Some(&run.binding.provider_session_id)
        && record.agent_process.as_ref() == Some(&run.binding.process)
        && record
            .current_run
            .as_ref()
            .is_some_and(|r| r.run_id == run.run_id.as_str())
}
fn record(coordinator: &ProductionV2Coordinator, pane: &PaneInstance) -> Option<PaneState> {
    coordinator
        .state
        .lock()
        .ok()?
        .as_ref()?
        .leased
        .runtime
        .record(pane)
        .cloned()
}
fn binding(coordinator: &ProductionV2Coordinator, record: &PaneState) -> Option<AgentBinding> {
    Some(AgentBinding {
        server_identity: coordinator.incarnation.identity.clone(),
        pane_instance: record.pane_instance.clone(),
        pane_state_id: record.state_id.clone(),
        agent_epoch: record.agent_epoch,
        agent_kind: record.agent.clone(),
        provider_session_id: record.agent_session_id.clone()?,
        process: record.agent_process.clone()?,
    })
}
pub(super) fn auto_input(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
    observation: &ProviderObservation,
) -> bool {
    let Some(record) = record(coordinator, &envelope.pane_instance) else {
        return false;
    };
    let Some(binding) = binding(coordinator, &record) else {
        return false;
    };
    coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned")
        .as_ref()
        .is_some_and(|r| r.capacity_input(&binding, observation).unwrap_or(false))
}

pub(super) fn waiting_panes(coordinator: &ProductionV2Coordinator) -> BTreeSet<PaneInstance> {
    let state = coordinator.capacity.lock().expect("capacity lock poisoned");
    if !state.config.enabled {
        return BTreeSet::new();
    }
    state
        .chains
        .iter()
        .filter(|(_, c)| c.reason.is_none() && c.due.is_some())
        .map(|(pane, _)| pane.clone())
        .collect()
}

pub(super) fn observe(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
    observation: &ProviderObservation,
    automatic: bool,
) {
    let pane = &envelope.pane_instance;
    if observation.hook_kind == ProviderHookKind::SessionStart {
        let mut state = coordinator.capacity.lock().expect("capacity lock poisoned");
        if let Some(chain) = state.chains.get_mut(pane) {
            chain.reason = Some(
                if chain.pending.is_some() {
                    "session_rollover_after_send"
                } else {
                    "session_start"
                }
                .into(),
            );
            chain.due = None;
        }
        return;
    }
    if observation.hook_kind != ProviderHookKind::UserPromptSubmit {
        return;
    }
    let Some(record) = record(coordinator, pane) else {
        return;
    };
    let Some(binding) = binding(coordinator, &record) else {
        return;
    };
    let run = coordinator
        .agent_runtime
        .lock()
        .expect("runtime lock poisoned")
        .as_ref()
        .and_then(|r| r.current_run_for_binding(&binding).ok().flatten());
    let Some(run) = run.filter(|r| r.provider_turn_key == observation.provider_turn_key) else {
        return;
    };
    let metadata = observation.question_resolver.as_ref();
    let daemon = coordinator
        .router
        .lock()
        .expect("router lock poisoned")
        .daemon_instance_id()
        .clone();
    let eligible = metadata.is_some_and(|m| {
        m.parent_origin_verified && m.daemon_generation.as_ref() == Some(&daemon) && !automatic
    });
    let active: BTreeSet<_> = coordinator
        .agent_runtime
        .lock()
        .expect("runtime lock poisoned")
        .as_ref()
        .and_then(|r| r.current_runs().ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.execution_phase != ExecutionPhase::Ended)
        .map(|r| r.run_id)
        .collect();
    let mut state = coordinator.capacity.lock().expect("capacity lock poisoned");
    if !automatic {
        state.chains.remove(pane);
    } else if let Some(chain) = state.chains.get_mut(pane) {
        let matches = chain.reason.is_none()
            && run.binding == chain.run.binding
            && run.operation_id == chain.pending
            && chain.pending.is_some();
        if matches {
            chain.run = run.clone();
            chain.pending = None;
            chain.due = None;
            chain.frame = None;
        } else {
            chain
                .reason
                .get_or_insert_with(|| "session_rollover_after_send".into());
            chain.due = None;
        }
    }
    // Each pane retains only its current Run's locator. Active chains keep their own binding.
    let keep: BTreeSet<_> = state
        .chains
        .values()
        .filter(|c| c.reason.is_none())
        .map(|c| c.run.run_id.clone())
        .collect();
    state
        .bound
        .retain(|id, _| active.contains(id) || keep.contains(id));
    state
        .missing_locator
        .retain(|id| active.contains(id) || keep.contains(id));
    if let Some(locator) = metadata
        .filter(|m| m.parent_origin_verified)
        .and_then(|m| m.locator.clone())
        && (state.bound.len() < 512 || state.bound.contains_key(&run.run_id))
    {
        state.bound.insert(
            run.run_id.clone(),
            BoundTurn {
                locator,
                size: 0,
                eligible,
            },
        );
    }
}

/// Existing observation captures initiate bounded transcript verification; no extra polling of every pane.
pub(super) fn detect(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    samples: &[policy::CapacitySample],
) {
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    detect_with(
        coordinator,
        &runner,
        accepted_seq,
        samples,
        Instant::now(),
        epoch_seconds(),
        jitter(),
        &|process| supported_process(coordinator, process),
    );
}
#[allow(clippy::too_many_arguments)]
fn detect_with(
    coordinator: &ProductionV2Coordinator,
    runner: &dyn TmuxRunner,
    accepted_seq: u64,
    samples: &[policy::CapacitySample],
    now: Instant,
    epoch: i64,
    random: u64,
    supported: &dyn Fn(&crate::pane_state::AgentProcessIdentity) -> bool,
) {
    for sample in samples {
        let pane = &sample.pane;
        let Some(record) = record(coordinator, pane) else {
            continue;
        };
        let Some(binding) = binding(coordinator, &record) else {
            continue;
        };
        let run = coordinator
            .agent_runtime
            .lock()
            .expect("runtime lock poisoned")
            .as_ref()
            .and_then(|r| r.current_run_for_binding(&binding).ok().flatten());
        let Some(run) = run.filter(|r| {
            current(&record, r)
                && r.execution_phase != ExecutionPhase::Ended
                && r.semantic_outcome == SemanticOutcome::Unresolved
        }) else {
            continue;
        };
        let chain = coordinator
            .capacity
            .lock()
            .expect("capacity lock poisoned")
            .chains
            .get(pane)
            .cloned();
        if let Some(chain) = chain
            .as_ref()
            .filter(|c| c.reason.is_none() && c.due.is_some())
            && chain.frame.as_ref().map(|f| f.digest) != sample.frame
        {
            stop(coordinator, pane, "viewport_changed");
        }
        let running_chain = chain
            .as_ref()
            .is_some_and(|c| c.reason.is_none() && c.due.is_none() && c.pending.is_none());
        if !sample.failure_hint && !(running_chain && !sample.working) {
            continue;
        }
        let Some(turn) = run.provider_turn_key.as_deref() else {
            continue;
        };
        let existing = coordinator
            .capacity
            .lock()
            .expect("capacity lock poisoned")
            .bound
            .get(&run.run_id)
            .cloned();
        let bound = if let Some(bound) = existing {
            Some(bound)
        } else {
            let checked = {
                let state = coordinator.capacity.lock().expect("capacity lock poisoned");
                state.missing_locator.contains(&run.run_id) || state.missing_locator.len() >= 512
            };
            if checked {
                None
            } else {
                let locator = recovered_locator(coordinator, &run);
                if locator.is_none() {
                    coordinator
                        .capacity
                        .lock()
                        .expect("capacity lock poisoned")
                        .missing_locator
                        .insert(run.run_id.clone());
                }
                locator.map(|locator| BoundTurn {
                    locator,
                    size: 0,
                    eligible: false,
                })
            }
        };
        let Some(mut bound) = bound else {
            continue;
        };
        let terminal = policy::read_terminal(&bound.locator, turn, &mut bound.size);
        {
            let mut state = coordinator.capacity.lock().expect("capacity lock poisoned");
            if state.bound.len() < 512 || state.bound.contains_key(&run.run_id) {
                state.bound.insert(run.run_id.clone(), bound.clone());
            }
        }
        match terminal {
            Ok(policy::TerminalOutcome::OtherError) => {
                stop(coordinator, pane, "other_error");
                continue;
            }
            Ok(policy::TerminalOutcome::Success) => {
                stop(coordinator, pane, "completed");
                continue;
            }
            Ok(policy::TerminalOutcome::Pending) => continue,
            Err(_) => {
                stop(coordinator, pane, "transcript_invalid");
                continue;
            }
            Ok(policy::TerminalOutcome::Capacity) => {}
        }
        if run.execution_phase == ExecutionPhase::Error
            && matches!(&record.lifecycle, crate::pane_state::LifecycleState::Error { reason } if reason.as_deref() == Some(policy::REASON))
        {
            continue;
        }
        let failed = coordinator
            .agent_runtime
            .lock()
            .expect("runtime lock poisoned")
            .as_mut()
            .and_then(|r| r.capacity_failure(&run.run_id, epoch).ok());
        let Some(failed) = failed else {
            coordinator.log_daemon_error("capacity failure persistence rejected");
            continue;
        };
        let daemon_instance_id = coordinator
            .router
            .lock()
            .expect("router lock poisoned")
            .daemon_instance_id()
            .clone();
        let envelope = PaneEventEnvelope {
            daemon_instance_id,
            event_id: crate::pane_state::EventId::generate().expect("OS random"),
            pane_instance: pane.clone(),
            agent: Some(run.binding.agent_kind.clone()),
            agent_session_id: Some(run.binding.provider_session_id.clone()),
            event: PaneEvent::FailRun {
                observed_at: epoch,
                reason: Some(policy::REASON.into()),
            },
        };
        let response = super::mutations::pane::apply_pane_event_mutation(
            coordinator,
            accepted_seq,
            envelope,
            true,
            Some(failed.clone()),
            None,
        );
        if !matches!(
            response,
            crate::daemon::protocol::v2::ServerMessage::PaneEventResult { .. }
        ) {
            continue;
        }
        let eligible = {
            let mut state = coordinator.capacity.lock().expect("capacity lock poisoned");
            state.failures = state.failures.saturating_add(1);
            state.config.enabled
                && (state
                    .chains
                    .get(pane)
                    .is_some_and(|c| c.reason.is_none() && c.run.run_id == failed.run_id)
                    || bound.eligible)
        };
        if !eligible {
            continue;
        }
        // No lock is held across process probing or tmux capture.
        let supported = supported(&failed.binding.process);
        let frame = if supported {
            ready_frame(runner, pane)
        } else {
            Err("unsupported_codex_version".into())
        };
        let mut state = coordinator.capacity.lock().expect("capacity lock poisoned");
        if !state.chains.contains_key(pane) && bound.eligible {
            let prompt = state.config.prompt.trim_end_matches('\n').to_owned();
            if state.chains.len() >= 512 {
                state.chains.retain(|_, c| c.reason.is_none());
            }
            if state.chains.len() < 512 {
                state.chains.insert(
                    pane.clone(),
                    Chain {
                        run: failed.clone(),
                        prompt,
                        attempts: 0,
                        due: None,
                        frame: None,
                        failed_at: epoch,
                        pending: None,
                        reason: None,
                    },
                );
            }
        }
        let Some(chain) = state.chains.get_mut(pane) else {
            continue;
        };
        if chain.reason.is_some() || chain.run.run_id != failed.run_id {
            continue;
        }
        chain.run = failed;
        chain.failed_at = epoch;
        if chain.attempts >= 3 {
            chain.reason = Some("attempt_limit".into());
            continue;
        }
        match frame {
            Ok(frame) => {
                chain.frame = Some(frame);
                chain.due = Some(now + Duration::from_secs(policy::delay(chain.attempts, random)));
            }
            Err(reason) => chain.reason = Some(reason),
        }
    }
}
fn supported_process(
    coordinator: &ProductionV2Coordinator,
    process: &crate::pane_state::AgentProcessIdentity,
) -> bool {
    let Some(profile) = crate::question_notice::profile::ProfileRequest::capture(process.clone())
    else {
        return false;
    };
    if !profile.matches_embedded_process() {
        return false;
    }
    let key = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&profile).expect("profile serialization"))
    );
    if let Some(supported) = coordinator
        .capacity
        .lock()
        .expect("capacity lock poisoned")
        .versions
        .get(&key)
        .copied()
    {
        return supported;
    }
    let supported = crate::question_notice::profile::capacity_version(&profile);
    let mut state = coordinator.capacity.lock().expect("capacity lock poisoned");
    if state.versions.len() >= 64 {
        state.versions.pop_first();
    }
    state.versions.insert(key, supported);
    supported
}
fn stop(coordinator: &ProductionV2Coordinator, pane: &PaneInstance, reason: &str) {
    if let Some(c) = coordinator
        .capacity
        .lock()
        .expect("capacity lock poisoned")
        .chains
        .get_mut(pane)
    {
        c.reason = Some(reason.into());
        c.due = None;
    }
}
fn recovered_locator(
    coordinator: &ProductionV2Coordinator,
    run: &RunRecord,
) -> Option<TranscriptLocator> {
    let home = coordinator
        .env
        .get("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            coordinator
                .env
                .get("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".codex"))
        })?;
    let path = crate::hook::origin::find_codex_session_file(
        &home.join("sessions"),
        run.binding.provider_session_id.as_str(),
    )?;
    let locator = TranscriptLocator::capture(&home, &path)?;
    crate::hook::origin::codex_hook_origin_from_payload(
        Some(run.binding.provider_session_id.as_str()),
        None,
        locator.transcript.to_str(),
        Some(&home),
    )
    .is_parent()
    .then_some(locator)
}
fn jitter() -> u64 {
    let mut n = [0u8; 8];
    getrandom::fill(&mut n).expect("OS random");
    u64::from_ne_bytes(n)
}

pub(super) fn start(coordinator: Arc<ProductionV2Coordinator>) {
    let weak = Arc::downgrade(&coordinator);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(c) = weak.upgrade() else {
                break;
            };
            if c.shutdown.load(Ordering::Acquire) {
                break;
            }
            if c.capacity
                .lock()
                .expect("capacity lock poisoned")
                .chains
                .values()
                .all(|s| s.reason.is_some())
            {
                continue;
            }
            if !c.capacity_tick_pending.swap(true, Ordering::AcqRel)
                && !c.enqueue_internal(V2InternalMutation::CapacityTick)
            {
                c.capacity_tick_pending.store(false, Ordering::Release);
            }
        }
    });
}
pub(super) fn tick(coordinator: &ProductionV2Coordinator) {
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3));
    tick_with(coordinator, &runner, Instant::now(), epoch_seconds());
}
fn tick_with(
    coordinator: &ProductionV2Coordinator,
    runner: &dyn TmuxRunner,
    now: Instant,
    epoch: i64,
) {
    let chains = coordinator
        .capacity
        .lock()
        .expect("capacity lock poisoned")
        .chains
        .clone();
    for (pane, chain) in chains {
        if chain.reason.is_some() {
            continue;
        }
        let Some(record) = record(coordinator, &pane).filter(|r| current(r, &chain.run)) else {
            stop(coordinator, &pane, "agent_identity_changed");
            continue;
        };
        let state = coordinator.capacity.lock().expect("capacity lock poisoned");
        if !state.config.enabled {
            drop(state);
            stop(coordinator, &pane, "disabled");
            continue;
        }
        let bound = state.bound.get(&chain.run.run_id).cloned();
        let interval_ready = state
            .last_send
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(2));
        drop(state);
        if let Some(id) = chain.pending.as_ref() {
            let op = coordinator
                .agent_runtime
                .lock()
                .expect("runtime lock poisoned")
                .as_ref()
                .and_then(|r| r.store().load_operation(id).ok().flatten());
            match op.map(|o| o.dispatch_state) {
                Some(DispatchState::DispatchStarted) => {}
                Some(DispatchState::PromptConfirmed) => {
                    stop(coordinator, &pane, "session_rollover_after_send")
                }
                Some(DispatchState::DeliveryUnknown) => {
                    stop(coordinator, &pane, "delivery_unknown")
                }
                _ => stop(coordinator, &pane, "dispatch_rejected"),
            }
            continue;
        }
        if chain.due.is_none() {
            if matches!(record.lifecycle, crate::pane_state::LifecycleState::Idle) {
                stop(coordinator, &pane, "completed");
            } else if matches!(
                record.lifecycle,
                crate::pane_state::LifecycleState::Error { .. }
            ) {
                stop(coordinator, &pane, "other_error");
            }
            continue;
        }
        // Waiting only reads canonical identity. Fresh IO happens exactly once in final pre-dispatch verification.
        if !interval_ready || chain.due.is_some_and(|due| due > now) {
            continue;
        }
        let Some(bound) = bound else {
            stop(coordinator, &pane, "locator_unavailable");
            continue;
        };
        let Some(frame) = chain.frame.clone() else {
            stop(coordinator, &pane, "frame_unavailable");
            continue;
        };
        let candidate = RecoveryCandidate {
            failed: chain.run.clone(),
            locator: bound.locator,
            size: bound.size,
            frame,
            failed_at: chain.failed_at,
        };
        let process = &chain.run.binding.process;
        let target = format!(
            "vta1:{}:{}:{}:{}:{}:{}:{:x}",
            coordinator.incarnation.hash,
            pane.pane_id.trim_start_matches('%'),
            pane.pane_pid,
            chain.run.binding.pane_state_id.as_str(),
            chain.run.binding.agent_epoch,
            process.pid,
            Sha256::digest(process.start_token.as_bytes())
        );
        let id = OperationId::generate().expect("OS random");
        let digest = crate::agent_state::Sha256Digest::parse(
            crate::pane_state::PromptState::digest_decoded_prompt(&chain.prompt),
        )
        .expect("digest");
        let response = super::mutations::agent::apply_capacity_prompt_at(
            coordinator,
            runner,
            crate::pane_state::EventId::generate().expect("OS random"),
            target,
            id.clone(),
            base64::engine::general_purpose::STANDARD.encode(chain.prompt.as_bytes()),
            digest,
            &candidate,
            epoch,
        );
        let mut state = coordinator.capacity.lock().expect("capacity lock poisoned");
        if !state.chains.contains_key(&pane) {
            continue;
        }
        match response {
            crate::daemon::protocol::v2::ServerMessage::AgentPromptResult { operation, .. } => {
                state.last_send = Some((coordinator.dispatch_clock)());
                let Some(c) = state.chains.get_mut(&pane) else {
                    continue;
                };
                c.attempts += 1;
                c.due = None;
                c.pending = Some(id);
                if operation.dispatch_state == DispatchState::Rejected {
                    c.reason = Some("dispatch_rejected".into());
                }
                if operation.dispatch_state == DispatchState::DeliveryUnknown {
                    c.reason = Some("delivery_unknown".into());
                }
            }
            crate::daemon::protocol::v2::ServerMessage::Error { code, .. } => {
                let reason = if code == crate::daemon::protocol::v2::ErrorCode::PromptDispatchBusy {
                    "binding_ambiguous"
                } else {
                    "dispatch_rejected"
                };
                let Some(c) = state.chains.get_mut(&pane) else {
                    continue;
                };
                c.reason = Some(reason.into());
                c.due = None;
            }
            _ => {
                let Some(c) = state.chains.get_mut(&pane) else {
                    continue;
                };
                c.reason = Some("dispatch_rejected".into());
                c.due = None;
            }
        }
    }
}

fn verify_attention(
    coordinator: &ProductionV2Coordinator,
    runner: &dyn TmuxRunner,
    record: &PaneState,
    failed_at: i64,
) -> Result<(), String> {
    let summary = coordinator
        .state
        .lock()
        .expect("state lock poisoned")
        .as_ref()
        .ok_or("daemon hydrating")?
        .question_notices
        .summary(&record.pane_instance, record.agent_process.as_ref());
    if summary.unacknowledged || summary.reason.is_some() {
        return Err("question_attention".into());
    }
    let clients = runner
        .run_bounded(
            &[
                "list-clients",
                "-F",
                "#{client_control_mode}:#{client_activity}:#{pane_id}",
            ],
            65536,
        )
        .map_err(|e| e.to_string())?;
    if clients.truncated {
        return Err("client_activity_unavailable".into());
    }
    for row in clients.text.lines() {
        let fields: Vec<_> = row.split(':').collect();
        if fields.len() != 3 {
            return Err("client_activity_unavailable".into());
        }
        if fields[0] == "1" {
            continue;
        }
        if fields[0] != "0" {
            return Err("client_activity_unavailable".into());
        }
        let activity = fields[1]
            .parse::<i64>()
            .map_err(|_| "client_activity_unavailable")?;
        if fields[2] == record.pane_instance.pane_id && activity >= failed_at {
            return Err("client_activity".into());
        }
    }
    Ok(())
}
fn ready_frame(runner: &dyn TmuxRunner, pane: &PaneInstance) -> Result<ReadyFrame, String> {
    let format = "__vde_capacity__#{pane_pid}:#{pane_width}:#{pane_height}:#{cursor_x}:#{cursor_y}:#{pane_in_mode}";
    let output = runner
        .run_bounded(
            &[
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                format,
                ";",
                "capture-pane",
                "-p",
                "-e",
                "-t",
                &pane.pane_id,
                ";",
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                format,
            ],
            512 * 1024,
        )
        .map_err(|e| e.to_string())?;
    if output.truncated {
        return Err("capture_truncated".into());
    }
    parse_frame(&output.text, pane.pane_pid)
}
fn parse_frame(output: &str, pane_pid: u32) -> Result<ReadyFrame, String> {
    let lines: Vec<_> = output.lines().collect();
    let header = lines
        .first()
        .filter(|first| lines.len() >= 3 && Some(*first) == lines.last())
        .ok_or("capture_changed")?;
    let shape = header
        .strip_prefix("__vde_capacity__")
        .ok_or("capture_unverified")?;
    let fields = shape
        .split(':')
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "capture_unverified")?;
    if fields.len() != 6
        || fields[0] != pane_pid as usize
        || fields[1] == 0
        || fields[2] != lines.len() - 2
        || fields[3] >= fields[1]
        || fields[4] >= fields[2]
        || fields[5] != 0
    {
        return Err("pane_mode_or_shape".into());
    }
    let ansi = policy::strip_terminal_links(&lines[1..lines.len() - 1].join("\n"))
        .ok_or("capture_invalid_osc")?;
    let styled = ansi
        .as_bytes()
        .into_text()
        .map_err(|_| "capture_invalid_ansi")?;
    let plain = styled
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let (row, digest) = policy::frame(&plain).ok_or("composer_or_failure_changed")?;
    let composer = plain.lines().nth(row).ok_or("composer_unavailable")?;
    if row != fields[4]
        || fields[3] != composer.len() - composer.trim_start().len() + 2
        || !policy::styled_placeholder_at(&ansi, row)
    {
        return Err("composer_not_empty".into());
    }
    Ok(ReadyFrame {
        digest,
        shape: shape.to_owned(),
    })
}

#[cfg(test)]
mod tests;

use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
use crate::pane_state::{AgentKind, EventId, PaneEventEnvelope, PaneInstance};
use crate::question_notice::{MAX_ANCESTORS, NoticeReason, NoticeResult, QuestionNoticeInput};
use crate::tmux::TmuxRunner;

use super::super::ProductionV2Coordinator;

struct ResolverWake<'a>(&'a std::sync::atomic::AtomicBool);
impl Drop for ResolverWake<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

pub(in crate::daemon::server) fn journal_failed(
    coordinator: &ProductionV2Coordinator,
    pane: &PaneInstance,
    session: Option<&str>,
    input: &crate::question_notice::ingress::ResolverInput,
) {
    note_unreported_journal_failure(coordinator, input);
    apply_journal_failure(coordinator, pane, session, input);
}

fn note_unreported_journal_failure(
    coordinator: &ProductionV2Coordinator,
    input: &crate::question_notice::ingress::ResolverInput,
) {
    if input.journal_failure.is_some()
        && !input.journal_failure_reported
        && let Some(state) = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_mut()
    {
        state
            .question_notices
            .resolver
            .note_journal_failure(input.journal_failure);
    }
}

fn apply_journal_failure(
    coordinator: &ProductionV2Coordinator,
    pane: &PaneInstance,
    session: Option<&str>,
    input: &crate::question_notice::ingress::ResolverInput,
) {
    let _wake = ResolverWake(&coordinator.question_wake);
    let process = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_ref()
        .and_then(|state| state.leased.runtime.record(pane))
        .and_then(|record| record.agent_process.clone());
    let verified = journal_root(coordinator, input) != JournalRoot::Mismatch
        && input.parent_origin_verified
        && process.as_ref().is_some_and(|process| {
            crate::question_notice::ingress::verify_ancestors(&input.ancestors, process)
                && input.process.as_ref().is_some_and(|request| {
                    &request.process == process && request.matches_embedded_process()
                })
        });
    if let Some(state) = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_mut()
    {
        let known = verified
            && state.leased.runtime.record(pane).is_some_and(|record| {
                record.agent_present
                    && record.agent_process == process
                    && record.agent_session_id.as_ref().is_some_and(|id| {
                        Some(crate::question_notice::resolver::session_key(id.as_str())).as_deref()
                            == session
                    })
            });
        state
            .question_notices
            .resolver
            .journal_failed(input.home_digest.as_deref(), session.filter(|_| known));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalRoot {
    Match,
    Mismatch,
    Unavailable,
}
fn journal_root(
    coordinator: &ProductionV2Coordinator,
    input: &crate::question_notice::ingress::ResolverInput,
) -> JournalRoot {
    let daemon_root = input
        .home_digest
        .as_ref()
        .and_then(|home| {
            crate::question_notice::journal::JournalLocation::new(&coordinator.env, home.clone())
                .ok()
        })
        .and_then(|location| location.root_digest().ok());
    match (daemon_root.as_ref(), input.journal_root_digest.as_ref()) {
        (Some(daemon), Some(hook)) if daemon == hook => JournalRoot::Match,
        (Some(_), Some(_)) => JournalRoot::Mismatch,
        _ => JournalRoot::Unavailable,
    }
}

#[derive(Clone)]
pub(in crate::daemon::server) struct ResolverObservation {
    pub pane: PaneInstance,
    pub daemon: crate::pane_state::DaemonInstanceId,
    pub session: String,
    pub turn: Option<String>,
    pub ingress: Option<String>,
    pub kind: crate::hook::provider::ProviderHookKind,
    pub metadata: Option<crate::question_notice::ingress::ResolverInput>,
}

impl ResolverObservation {
    pub fn from_provider(
        envelope: &PaneEventEnvelope,
        observation: &crate::hook::provider::ProviderObservation,
    ) -> Self {
        Self {
            pane: envelope.pane_instance.clone(),
            daemon: envelope.daemon_instance_id.clone(),
            session: crate::question_notice::resolver::session_key(observation.session_id.as_str()),
            turn: observation
                .provider_turn_key
                .as_deref()
                .map(crate::question_notice::resolver::session_key),
            ingress: observation.provider_event_ref.clone(),
            kind: observation.hook_kind,
            metadata: observation.question_resolver.clone(),
        }
    }
}

pub(in crate::daemon::server) fn observe(
    coordinator: &ProductionV2Coordinator,
    observation: ResolverObservation,
    issue: Option<&NoticeResult>,
    accepted: bool,
    runner: &dyn TmuxRunner,
) {
    let _wake = ResolverWake(&coordinator.question_wake);
    use crate::hook::provider::ProviderHookKind;
    use crate::question_notice::{
        NoticeDisposition,
        ingress::InputClass,
        profile::CodexProfile,
        resolver::{Binding, JournalView, NoticeFence},
    };
    let Some(input) = observation.metadata.as_ref() else {
        return;
    };
    // Count a delivered failure report once across RPC and ordinary delivery,
    // including early-return paths. A lost RPC receipt may still be observed twice;
    // this diagnostic counts observations, not distinct failures or lost sessions.
    note_unreported_journal_failure(coordinator, input);
    let Some(home) = input.home_digest.as_deref() else {
        return;
    };
    if observation.kind == ProviderHookKind::UserPromptSubmit
        && input.input_class == InputClass::NonAuthoritativeInput
    {
        if let Some(state) = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_mut()
        {
            state.question_notices.resolver.non_authoritative = state
                .question_notices
                .resolver
                .non_authoritative
                .saturating_add(1);
        }
        return;
    }
    if observation.kind == ProviderHookKind::SessionStart && input.journal_failure.is_some() {
        if let Some(state) = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_mut()
        {
            state
                .question_notices
                .resolver
                .unknown(home, &observation.session);
        }
        return;
    }
    let root = journal_root(coordinator, input);
    if root == JournalRoot::Unavailable {
        if observation.kind == ProviderHookKind::Activity
            && input.journal_failure.is_some()
            && !input.journal_failure_reported
        {
            apply_journal_failure(
                coordinator,
                &observation.pane,
                Some(&observation.session),
                input,
            );
        }
        if let Some(state) = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_mut()
        {
            state
                .question_notices
                .resolver
                .unknown(home, &observation.session);
        }
        return;
    }
    if root == JournalRoot::Mismatch {
        if let Some(state) = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_mut()
        {
            // The hook's dirty entry lives in another state root. Treat it as an
            // unknown-session loss for this home, never as a clean local journal.
            state.question_notices.resolver.root_mismatch(home);
            state
                .question_notices
                .resolver
                .unknown(home, &observation.session);
        }
        return;
    }
    let starting = observation.kind == ProviderHookKind::SessionStart;
    let process = if starting {
        runner
            .resolve_agent_process(
                observation.pane.pane_pid,
                &AgentKind::parse("codex").expect("constant kind"),
            )
            .ok()
            .flatten()
    } else {
        coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_ref()
            .and_then(|state| state.leased.runtime.record(&observation.pane))
            .and_then(|record| record.agent_process.clone())
    };
    let process_verified = process.as_ref().is_some_and(|process| {
        crate::question_notice::ingress::verify_ancestors(&input.ancestors, process)
            && input.process.as_ref().is_some_and(|request| {
                &request.process == process && request.matches_embedded_process()
            })
    });
    // SessionStart may precede the next cached topology poll. Verify the current
    // incarnation directly, instead of declaring that fresh session permanently
    // unknown merely because the projection has not caught up yet.
    let pane_verified = !starting
        || runner
            .run(&[
                "display-message",
                "-p",
                "-t",
                &observation.pane.pane_id,
                "#{pane_id}\t#{pane_pid}",
            ])
            .is_ok_and(|value| {
                value.trim()
                    == format!(
                        "{}\t{}",
                        observation.pane.pane_id, observation.pane.pane_pid
                    )
            });
    let mut guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = guard.as_mut() else {
        return;
    };
    let known = state
        .question_notices
        .resolver
        .session_binding(home, &observation.session);
    let profile_verified = input.process.as_ref().is_some_and(|request| {
        coordinator.question_profiles.cached(request) == Some(input.profile)
            || known.is_some_and(|binding| {
                binding.executable == *request && binding.profile == input.profile
            })
    });
    let valid = input.parent_origin_verified
        && input.daemon_generation.as_ref() == Some(&observation.daemon)
        && process_verified
        && profile_verified
        && input.profile != CodexProfile::Unknown
        && input
            .locator
            .as_ref()
            .is_some_and(|locator| locator.home_digest() == home && locator.matches_current_file())
        && input.journal_failure.is_none()
        && state.question_notices.tracking_healthy()
        && pane_verified
        && state
            .leased
            .runtime
            .record(&observation.pane)
            .is_some_and(|record| record.agent_present && record.agent.as_str() == "codex");
    if !valid {
        // Fixed positions: origin, generation, ancestry/process, profile cache,
        // version, locator, journal, sidecar. No hook or capture text is retained.
        for (index, valid) in [
            input.parent_origin_verified,
            input.daemon_generation.as_ref() == Some(&observation.daemon),
            process_verified,
            profile_verified,
            input.profile != CodexProfile::Unknown,
            input.locator.as_ref().is_some_and(|locator| {
                locator.home_digest() == home && locator.matches_current_file()
            }),
            input.journal_failure.is_none(),
            state.question_notices.tracking_healthy(),
            pane_verified,
            state
                .leased
                .runtime
                .record(&observation.pane)
                .is_some_and(|record| record.agent_present && record.agent.as_str() == "codex"),
        ]
        .into_iter()
        .enumerate()
        {
            if !valid {
                state.question_notices.resolver.invalid_observations[index] =
                    state.question_notices.resolver.invalid_observations[index].saturating_add(1);
            }
        }
        state
            .question_notices
            .resolver
            .unknown(home, &observation.session);
        return;
    }
    let process = process.expect("verified process");
    let binding = Binding {
        owner: state
            .question_notices
            .owner_ref(&observation.pane, &process),
        pane: observation.pane.clone(),
        process,
        executable: input.process.clone().expect("verified executable"),
        profile: input.profile,
        locator: input.locator.clone().expect("verified locator"),
    };
    let epoch = state
        .question_notices
        .resolver
        .session_epoch(home, &observation.session)
        .unwrap_or(0);
    if observation.kind == ProviderHookKind::SessionStart {
        let startup = input.startup_journal;
        let verified = accepted
            && (input.source != crate::question_notice::ingress::SessionSource::Startup
                || (input.startup_header_verified && startup.is_some()));
        state.question_notices.resolver.session_start(
            home.to_owned(),
            observation.session,
            input.source,
            Some(binding),
            JournalView {
                epoch: startup.map_or(epoch, |journal| journal.epoch),
                veto: false,
                session_dirty: startup.is_some_and(|journal| journal.session_dirty),
            },
            verified,
        );
        return;
    }
    if issue.is_some_and(|result| result.disposition == NoticeDisposition::Applied) {
        let summary = state
            .question_notices
            .summary(&observation.pane, Some(&binding.process));
        if let Some(turn) = &observation.turn {
            state.question_notices.resolver.issue(
                &binding,
                home,
                &observation.session,
                turn,
                summary.latest_order,
                epoch,
            );
        }
    }
    if observation.kind == ProviderHookKind::UserPromptSubmit && accepted {
        let summary = state
            .question_notices
            .summary(&observation.pane, Some(&binding.process));
        if let Some((turn, ingress)) = observation
            .turn
            .as_deref()
            .zip(observation.ingress.as_deref())
        {
            let now = std::time::Instant::now();
            if let Some(check) = state.question_notices.resolver.ordinary(
                input.input_class,
                &binding,
                &observation.session,
                ingress,
                turn,
                NoticeFence {
                    acknowledged: summary.acknowledged_order,
                    latest: summary.latest_order,
                },
                JournalView {
                    epoch,
                    veto: false,
                    session_dirty: false,
                },
                now,
            ) {
                let submitted = coordinator
                    .question_orders
                    .lock()
                    .expect("question worker lock poisoned")
                    .as_ref()
                    .is_some_and(|worker| {
                        worker.submit(crate::daemon::workers::question::OrderJob {
                            check,
                            deadline: now + std::time::Duration::from_millis(1500),
                        })
                    });
                if !submitted {
                    state.question_notices.resolver.note_retained(
                        crate::question_notice::resolver::RetainReason::OrderQueueUnavailable,
                    );
                    state.question_notices.resolver.cancel(&binding.owner);
                }
            }
        }
    }
    drop(guard);
    // The enclosing mutation's maintain pass evaluates the final provider state once.
}

pub(in crate::daemon::server) fn start_workers(
    coordinator: std::sync::Arc<ProductionV2Coordinator>,
    capture: crate::daemon::workers::CaptureCoordinatorHandle,
) {
    use std::time::Duration;
    *coordinator
        .question_capture
        .lock()
        .expect("question capture lock poisoned") = Some(capture.clone());
    let current = coordinator.clone();
    let verify_owner: crate::daemon::workers::question::VerifyOwner =
        std::sync::Arc::new(move |binding, check| {
            let snapshot = crate::daemon::workers::read_agent_process_snapshot(
                Duration::from_millis(300),
                false,
            );
            let detection = snapshot.detect_from_pid_tree(binding.pane.pane_pid);
            if !detection.complete
                || !detection.process_identities_complete
                || detection
                    .exact_agent_process(&AgentKind::parse("codex").expect("constant kind"))
                    .as_ref()
                    != Some(&binding.process)
                || snapshot.is_foreground_process_owner(binding.pane.pane_pid, binding.process.pid)
                    != Some(true)
            {
                return false;
            }
            if check == crate::daemon::workers::question::OwnerCheck::CapturedPane {
                // The capture command verifies server incarnation and pane PID
                // both before and after capture. Process ownership is still fresh here.
                return true;
            }
            current
                .status_push_runner(Duration::from_millis(100))
                .run(&[
                    "display-message",
                    "-p",
                    "-t",
                    &binding.pane.pane_id,
                    "#{pid}:#{start_time}:#{pane_id}:#{pane_pid}",
                ])
                .is_ok_and(|value| {
                    value.trim()
                        == format!(
                            "{}:{}:{}:{}",
                            current.incarnation.identity.pid,
                            current.incarnation.identity.start_time,
                            binding.pane.pane_id,
                            binding.pane.pane_pid
                        )
                })
        });
    // Either bounded completion queue wakes the dispatcher immediately. The single
    // coalesced notification contains no guard or identity and cannot grow with load.
    let (completion_ready, ready) = std::sync::mpsc::sync_channel(1);
    let current = coordinator.clone();
    let mutation_busy: crate::daemon::workers::question::MutationBusy =
        std::sync::Arc::new(move || {
            let queue = current.queue.lock().expect("v2 queue lock poisoned");
            queue.in_flight || !queue.items.is_empty()
        });
    let (probes, probe_completed) = crate::daemon::workers::question::start_probe_workers(
        coordinator.env.clone(),
        capture.clone(),
        verify_owner.clone(),
        completion_ready.clone(),
        mutation_busy.clone(),
    );
    *coordinator
        .question_probes
        .lock()
        .expect("question probe lock poisoned") = Some(probes);
    let current = coordinator.clone();
    let live_sessions = std::sync::Arc::new(move || {
        current
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_ref()
            .map(|state| state.question_notices.resolver.cursor_sessions())
            .unwrap_or_default()
    });
    let (worker, completed) = crate::daemon::workers::question::start_order_worker(
        coordinator.env.clone(),
        std::sync::Arc::new(move || capture.normal_busy()),
        live_sessions,
        verify_owner,
        completion_ready,
        mutation_busy,
    );
    *coordinator
        .question_orders
        .lock()
        .expect("question worker lock poisoned") = Some(worker);
    std::thread::spawn(move || {
        let mut next_tick: Option<std::time::Instant> = None;
        while !coordinator
            .shutdown
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let _ = ready.recv_timeout(Duration::from_millis(25));
            for completion in completed.try_iter() {
                coordinator.enqueue_internal(
                    super::super::V2InternalMutation::QuestionOrderCompleted(completion),
                );
            }
            for completion in probe_completed.try_iter() {
                coordinator.enqueue_internal(
                    super::super::V2InternalMutation::QuestionProbeCompleted(completion),
                );
            }
            let now = std::time::Instant::now();
            if !coordinator
                .question_wake
                .swap(false, std::sync::atomic::Ordering::AcqRel)
                && next_tick.is_none_or(|deadline| now < deadline)
            {
                continue;
            }
            let active = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned")
                .as_ref()
                .is_some_and(|state| {
                    next_tick = state.question_notices.resolver.next_wakeup();
                    state.question_notices.resolver.tick_due(now)
                });
            if active {
                enqueue_tick_once(&coordinator.question_tick_pending, || {
                    coordinator.enqueue_internal(super::super::V2InternalMutation::QuestionTick)
                });
            }
        }
    });
}

fn enqueue_tick_once(pending: &std::sync::atomic::AtomicBool, enqueue: impl FnOnce() -> bool) {
    use std::sync::atomic::Ordering;
    if !pending.swap(true, Ordering::AcqRel) && !enqueue() {
        pending.store(false, Ordering::Release);
    }
}

#[cfg(test)]
#[test]
fn question_tick_coalesces_backlog_and_retries_only_failed_enqueue() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let pending = AtomicBool::new(false);
    let count = std::cell::Cell::new(0);
    for _ in 0..100 {
        enqueue_tick_once(&pending, || {
            count.set(count.get() + 1);
            true
        });
    }
    assert_eq!(count.get(), 1);
    pending.store(false, Ordering::Release); // Worker starts the accepted tick.
    enqueue_tick_once(&pending, || false);
    assert!(!pending.load(Ordering::Acquire));
    enqueue_tick_once(&pending, || {
        count.set(count.get() + 1);
        true
    });
    assert_eq!(count.get(), 2);
}

pub(in crate::daemon::server) fn internal_ack(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
) -> ServerMessage {
    ServerMessage::SnapshotAck {
        event_id: EventId::generate().expect("OS random source failed after daemon startup"),
        accepted_seq,
        snapshot_revision: coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_ref()
            .map_or(0, |state| state.leased.runtime.snapshot_revision()),
    }
}

pub(in crate::daemon::server) fn order_completed(
    coordinator: &ProductionV2Coordinator,
    completion: crate::daemon::workers::question::OrderCompletion,
) {
    let _wake = ResolverWake(&coordinator.question_wake);
    use crate::daemon::workers::question::OrderOutcome;
    use crate::question_notice::resolver::RetainReason;
    let fence = &completion.check.fence;
    let mut guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    // Keep the transfer under the deadline reaper while waiting for state.
    let mut locked = completion.guard.as_ref().and_then(|guard| guard.take());
    let Some(state) = guard.as_mut() else {
        return;
    };
    if !state.question_notices.resolver.fence_current(fence) {
        return;
    }
    if let Some(reason) = completion.reason {
        state.question_notices.resolver.note_retained(reason);
    }
    if completion.outcome == OrderOutcome::HistoryUnknown {
        state
            .question_notices
            .resolver
            .unknown(&fence.home, &fence.session);
        return;
    }
    let summary = state
        .question_notices
        .summary(&fence.binding.pane, Some(&fence.binding.process));
    let guard_available = locked.as_ref().is_some_and(|guard| guard.is_current());
    let clean = locked.as_mut().is_some_and(|guard| {
        guard.is_current()
            && guard
                .evaluate(
                    Some(&fence.session),
                    crate::question_notice::journal::writer_state,
                )
                .is_ok_and(|journal| !journal.veto && journal.epoch == fence.epoch)
    });
    if completion.outcome != OrderOutcome::Checked
        || !clean
        || !owner_binding_current(state, fence)
        || summary.latest_order != fence.latest
        || summary.acknowledged_order != fence.acknowledged
    {
        if completion.outcome == OrderOutcome::Checked {
            let reason = if !guard_available {
                RetainReason::CheckingGuardUnavailable
            } else if !clean {
                RetainReason::CheckingJournalVeto
            } else if !owner_binding_current(state, fence) {
                RetainReason::Owner
            } else {
                RetainReason::Fence
            };
            state.question_notices.resolver.note_retained(reason);
        }
        state.question_notices.resolver.cancel(&fence.binding.owner);
        return;
    }
    state.question_notices.resolver.checked(
        &completion.check,
        &completion.proven,
        std::time::Instant::now(),
    );
    drop(guard);
    drop(locked);
    reevaluate(coordinator);
}

fn reevaluate(coordinator: &ProductionV2Coordinator) {
    let _wake = ResolverWake(&coordinator.question_wake);
    use crate::question_notice::resolver::{CandidateStage, lifecycle_fence, session_key};
    // Extract only identity and lifecycle metadata; never clone a PaneState prompt into
    // the question resolver path.
    let pending = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_ref()
        .map(|state| {
            state
                .question_notices
                .resolver
                .candidates()
                .filter(|candidate| candidate.stage == CandidateStage::Armed)
                .filter_map(|candidate| {
                    let record = state.leased.runtime.record(&candidate.fence.binding.pane)?;
                    let session = record.agent_session_id.as_ref()?;
                    if session_key(session.as_str()) != candidate.fence.session
                        || record.agent_process.as_ref() != Some(&candidate.fence.binding.process)
                    {
                        return None;
                    }
                    let evaluation_key = crate::question_notice::resolver::evaluation_fence(record);
                    if candidate.last_evaluated_projection.as_ref() == Some(&evaluation_key) {
                        return None;
                    }
                    let binding = crate::agent_state::AgentBinding {
                        server_identity: coordinator.incarnation.identity.clone(),
                        pane_instance: record.pane_instance.clone(),
                        pane_state_id: record.state_id.clone(),
                        agent_epoch: record.agent_epoch,
                        agent_kind: record.agent.clone(),
                        provider_session_id: session.clone(),
                        process: candidate.fence.binding.process.clone(),
                    };
                    Some((
                        candidate.fence.binding.owner.clone(),
                        binding,
                        record.current_run.clone(),
                        evaluation_key,
                        lifecycle_fence(record),
                        matches!(record.lifecycle, crate::pane_state::LifecycleState::Idle),
                        matches!(
                            record.lifecycle,
                            crate::pane_state::LifecycleState::Error { .. }
                        ),
                    ))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (owner, binding, projection, evaluation_key, run_fence, done, interrupted) in pending {
        let run = coordinator
            .agent_runtime
            .lock()
            .expect("agent runtime lock poisoned")
            .as_ref()
            .and_then(|runtime| runtime.current_run_for_binding(&binding).ok().flatten());
        let Some(turn) = run
            .as_ref()
            .and_then(|run| run.provider_turn_key.as_deref())
            .map(session_key)
        else {
            continue;
        };
        if let Some(state) = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned")
            .as_mut()
        {
            state.question_notices.resolver.eligible(
                &owner,
                &turn,
                done,
                interrupted
                    || run.as_ref().is_some_and(|run| {
                        run.execution_phase == crate::agent_state::ExecutionPhase::Error
                            || (run.execution_phase == crate::agent_state::ExecutionPhase::Ended
                                && run.semantic_outcome
                                    == crate::agent_state::SemanticOutcome::Unresolved)
                    }),
                std::time::Instant::now(),
            );
            state
                .question_notices
                .resolver
                .record_eligibility(&owner, run_fence);
            // Only memoize a successfully loaded, exactly projected Run. A new
            // Run revision or lifecycle triggers a fresh read; failures never do.
            if let (Some(run), Some(projection)) = (&run, projection)
                && projection.run_id == run.run_id.as_str()
                && projection.run_seq == run.run_seq
                && projection.run_revision == run.revision
            {
                state
                    .question_notices
                    .resolver
                    .record_evaluated_projection(&owner, evaluation_key);
            }
        }
    }
}

pub(in crate::daemon::server) fn tick(coordinator: &ProductionV2Coordinator) {
    let _wake = ResolverWake(&coordinator.question_wake);
    if let Some(state) = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_mut()
    {
        for (home, ticket) in state
            .question_notices
            .resolver
            .recovery_due(std::time::Instant::now())
        {
            if let Some(worker) = coordinator
                .question_probes
                .lock()
                .expect("question probe lock poisoned")
                .as_ref()
            {
                worker.submit(crate::daemon::workers::question::ProbeJob::RecoverHome {
                    home,
                    ticket,
                    deadline: std::time::Instant::now()
                        + crate::question_notice::journal::LOCK_BUDGET,
                });
            }
        }
        for (fence, probe_id) in state
            .question_notices
            .resolver
            .due(std::time::Instant::now())
        {
            let deadline = state
                .question_notices
                .resolver
                .candidate(&fence.binding.owner)
                .expect("due candidate")
                .deadline;
            let valid = probe_binding_current(state, &fence);
            let submitted = valid
                && coordinator
                    .question_probes
                    .lock()
                    .expect("question probe lock poisoned")
                    .as_ref()
                    .is_some_and(|worker| {
                        worker.submit(crate::daemon::workers::question::ProbeJob::Sample {
                            fence: fence.clone(),
                            probe_id,
                            deadline,
                        })
                    });
            if !submitted {
                state.question_notices.resolver.note_retained(if valid {
                    crate::question_notice::resolver::RetainReason::ProbeQueueUnavailable
                } else {
                    crate::question_notice::resolver::RetainReason::ProbeBindingChanged
                });
                state.question_notices.resolver.cancel(&fence.binding.owner);
            }
        }
    }
}

fn probe_binding_current(
    state: &crate::daemon::runtime::CanonicalCoordinatorState,
    fence: &crate::question_notice::resolver::Fence,
) -> bool {
    owner_binding_current(state, fence)
        && state
            .leased
            .runtime
            .record(&fence.binding.pane)
            .is_some_and(|record| {
                matches!(record.lifecycle, crate::pane_state::LifecycleState::Idle)
                    && state
                        .question_notices
                        .resolver
                        .candidate(&fence.binding.owner)
                        .is_some_and(|candidate| {
                            candidate.eligible_run.as_ref()
                                == Some(&crate::question_notice::resolver::lifecycle_fence(record))
                        })
            })
}

// Executable and transcript IO was validated by the worker before acquiring the
// journal. Here only canonical identity metadata is inspected under that lock.
fn owner_binding_current(
    state: &crate::daemon::runtime::CanonicalCoordinatorState,
    fence: &crate::question_notice::resolver::Fence,
) -> bool {
    let binding = &fence.binding;
    state.question_notices.tracking_healthy()
        && state
            .topology
            .panes
            .iter()
            .any(|pane| pane.pane_instance == binding.pane)
        && state
            .leased
            .runtime
            .record(&binding.pane)
            .is_some_and(|record| {
                record.agent_present
                    && record.agent.as_str() == "codex"
                    && record.agent_process.as_ref() == Some(&binding.process)
                    && record.agent_session_id.as_ref().is_some_and(|session| {
                        crate::question_notice::resolver::session_key(session.as_str())
                            == fence.session
                    })
            })
}

pub(in crate::daemon::server) fn probe_completed(
    coordinator: &ProductionV2Coordinator,
    completion: crate::daemon::workers::question::ProbeCompletion,
) {
    let _wake = ResolverWake(&coordinator.question_wake);
    use crate::daemon::workers::question::{ProbeCompletion, ProbeJob};
    use crate::question_notice::resolver::{JournalView, NoticeFence, RetainReason, Sample};
    if let ProbeCompletion::RecoverHome {
        home,
        ticket,
        epoch,
    } = &completion
    {
        // The epoch bump is already durable. Queue delay cannot undo it and must
        // not cause repeated disk writes; the failure ticket fences newer reports.
        if epoch.is_some()
            && let Some(state) = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned")
                .as_mut()
        {
            state.question_notices.resolver.recovered(home, *ticket);
        }
        return;
    }
    let is_commit = matches!(&completion, ProbeCompletion::Commit { .. });
    let (fence, transfer, failure) = match &completion {
        ProbeCompletion::Sample {
            fence,
            guard,
            failure,
            ..
        }
        | ProbeCompletion::Commit {
            fence,
            guard,
            failure,
            ..
        } => (fence, guard, *failure),
        ProbeCompletion::RecoverHome { .. } => unreachable!("handled recovery"),
    };
    let mut locked = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let mut journal_guard = transfer.as_ref().and_then(|guard| guard.take());
    let Some(state) = locked.as_mut() else {
        return;
    };
    if !state.question_notices.resolver.fence_current(fence) {
        return;
    }
    if let Some(reason) = failure {
        state.question_notices.resolver.note_retained(reason);
        state.question_notices.resolver.cancel(&fence.binding.owner);
        return;
    }
    let guard_available = journal_guard
        .as_ref()
        .is_some_and(|guard| guard.is_current());
    let evaluation = journal_guard.as_mut().and_then(|guard| {
        guard
            .evaluate(
                Some(&fence.session),
                crate::question_notice::journal::writer_state,
            )
            .ok()
    });
    let valid = evaluation
        .as_ref()
        .is_some_and(|value| !value.veto && value.epoch == fence.epoch)
        && probe_binding_current(state, fence);
    if !valid {
        state
            .question_notices
            .resolver
            .note_retained(if !guard_available {
                if is_commit {
                    RetainReason::CommitTransferUnavailable
                } else {
                    RetainReason::SampleTransferUnavailable
                }
            } else if !probe_binding_current(state, fence) {
                RetainReason::ProbeBindingChanged
            } else {
                if is_commit {
                    RetainReason::CommitGuardVeto
                } else {
                    RetainReason::SampleJournalVeto
                }
            });
        state.question_notices.resolver.cancel(&fence.binding.owner);
        return;
    }
    match completion {
        ProbeCompletion::RecoverHome { .. } => unreachable!("handled recovery"),
        ProbeCompletion::Sample {
            fence,
            probe_id,
            sample,
            ..
        } => {
            let through = state.question_notices.resolver.sample(
                &fence,
                probe_id,
                sample,
                std::time::Instant::now(),
            );
            if let Some(through) = through {
                // Release this sample's lock before dispatching the independent final guard.
                drop(journal_guard);
                let deadline = state
                    .question_notices
                    .resolver
                    .candidate(&fence.binding.owner)
                    .expect("current candidate")
                    .deadline;
                let sent = coordinator
                    .question_probes
                    .lock()
                    .expect("question probe lock poisoned")
                    .as_ref()
                    .is_some_and(|worker| {
                        worker.submit(ProbeJob::Commit {
                            fence: fence.clone(),
                            through,
                            deadline,
                        })
                    });
                if !sent {
                    state
                        .question_notices
                        .resolver
                        .note_retained(RetainReason::CommitQueueUnavailable);
                    state.question_notices.resolver.cancel(&fence.binding.owner);
                }
            } else if sample != Sample::NormalComposer {
                state.question_notices.resolver.note_retained(
                    if sample == Sample::ActiveQuestion {
                        RetainReason::Active
                    } else {
                        RetainReason::Ambiguous
                    },
                );
                state.question_notices.resolver.cancel(&fence.binding.owner);
            }
        }
        ProbeCompletion::Commit { fence, through, .. } => {
            let summary = state
                .question_notices
                .summary(&fence.binding.pane, Some(&fence.binding.process));
            let journal = evaluation.expect("valid journal");
            let allowed = state.question_notices.resolver.final_guard(
                &fence,
                through,
                NoticeFence {
                    acknowledged: summary.acknowledged_order,
                    latest: summary.latest_order,
                },
                JournalView {
                    epoch: journal.epoch,
                    veto: journal.veto,
                    session_dirty: journal.session_dirty,
                },
                std::time::Instant::now(),
            );
            if !allowed {
                state
                    .question_notices
                    .resolver
                    .note_retained(RetainReason::FinalGuard);
                state.question_notices.resolver.cancel(&fence.binding.owner);
                return;
            }
            let deadline = journal_guard.as_ref().expect("validated guard").deadline();
            let commit = state.question_notices.acknowledge_until(
                &fence.binding.pane,
                &fence.binding.owner,
                through,
                Some(deadline),
            );
            if commit.is_err() {
                state
                    .question_notices
                    .resolver
                    .note_retained(RetainReason::CommitRejected);
            }
            if commit == Ok(true) {
                state.question_notices.resolver.candidates_acked = state
                    .question_notices
                    .resolver
                    .candidates_acked
                    .saturating_add(1);
            }
            // Keep the journal guard through rename, directory fsync and memory result handling.
            // Projection preflight needs only the canonical state lock, not the home flock.
            drop(journal_guard);
            if summary
                != state
                    .question_notices
                    .summary(&fence.binding.pane, Some(&fence.binding.process))
                && let Err(error) = state.leased.runtime.mark_projection_changed()
            {
                coordinator.fail_stop(error.to_string());
            }
        }
    }
}

pub(in crate::daemon::server) fn apply(
    coordinator: &ProductionV2Coordinator,
    envelope: &PaneEventEnvelope,
    observation: &crate::hook::provider::ProviderObservation,
    input: QuestionNoticeInput,
    runner: &dyn TmuxRunner,
) -> NoticeResult {
    let verified = (|| {
        let QuestionNoticeInput::Issued {
            session_id,
            turn_id,
            tool_use_id,
            ancestors,
        } = input
        else {
            let QuestionNoticeInput::Rejected { reason } = input else {
                unreachable!()
            };
            return Err(reason);
        };
        if observation.provider.as_str() != "codex"
            || observation.hook_kind != crate::hook::provider::ProviderHookKind::Activity
            || session_id != observation.session_id.as_str()
            || observation.provider_turn_key.as_deref() != Some(&turn_id)
            || ![session_id.as_str(), turn_id.as_str(), tool_use_id.as_str()]
                .into_iter()
                .all(crate::question_notice::ingress::valid_identifier)
            || ancestors.is_empty()
            || ancestors.len() > MAX_ANCESTORS
            || ancestors
                .iter()
                .any(|ancestor| ancestor.validate().is_err())
        {
            return Err(NoticeReason::InvalidPayload);
        }
        {
            let state = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state.as_ref().ok_or(NoticeReason::OwnerUnverified)?;
            if !state
                .topology
                .panes
                .iter()
                .any(|pane| pane.pane_instance == envelope.pane_instance)
                || !state
                    .leased
                    .runtime
                    .record(&envelope.pane_instance)
                    .is_some_and(|record| record.agent.as_str() == "codex" && record.agent_present)
            {
                return Err(NoticeReason::OwnerUnverified);
            }
        }
        // Do not reuse provider_binding_record: sessions/epochs are deliberately not notice owners.
        let process = runner
            .resolve_agent_process(
                envelope.pane_instance.pane_pid,
                &AgentKind::parse("codex").expect("constant kind"),
            )
            .map_err(|_| NoticeReason::OwnerUnverified)?
            .ok_or(NoticeReason::OwnerUnverified)?;
        if !ancestors.contains(&process) {
            return Err(NoticeReason::AncestorNotInPane);
        }
        Ok((process, session_id, turn_id, tool_use_id))
    })();
    let mut guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = guard.as_mut() else {
        return NoticeResult::rejected(NoticeReason::OwnerUnverified);
    };
    let before = state
        .question_notices
        .summary(&envelope.pane_instance, None);
    let result = match verified {
        Ok((process, session, turn, tool)) => state.question_notices.issue(
            envelope.pane_instance.clone(),
            process,
            (&session, &turn, &tool),
            super::super::epoch_seconds(),
        ),
        Err(reason) => state
            .question_notices
            .reject(&envelope.pane_instance, reason),
    };
    if before
        != state
            .question_notices
            .summary(&envelope.pane_instance, None)
        && let Err(error) = state.leased.runtime.mark_projection_changed()
    {
        coordinator.fail_stop(error.to_string());
    }
    result
}

pub(in crate::daemon::server) fn acknowledge(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    pane: PaneInstance,
    owner_ref: String,
    through_order: u64,
) -> ServerMessage {
    let runner = coordinator.status_push_runner(std::time::Duration::from_secs(1));
    acknowledge_with_runner(
        coordinator,
        accepted_seq,
        event_id,
        pane,
        owner_ref,
        through_order,
        &runner,
    )
}

pub(in crate::daemon::server) fn acknowledge_with_runner(
    coordinator: &ProductionV2Coordinator,
    accepted_seq: u64,
    event_id: EventId,
    pane: PaneInstance,
    owner_ref: String,
    through_order: u64,
    runner: &dyn TmuxRunner,
) -> ServerMessage {
    let _wake = ResolverWake(&coordinator.question_wake);
    let process = runner
        .resolve_agent_process(
            pane.pane_pid,
            &AgentKind::parse("codex").expect("constant kind"),
        )
        .ok()
        .flatten();
    let mut guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = guard.as_mut() else {
        return ServerMessage::error(ErrorCode::NotReady, "daemon is hydrating", Some(event_id));
    };
    if !state
        .topology
        .panes
        .iter()
        .any(|entry| entry.pane_instance == pane)
        || process.is_none()
        || state.question_notices.owner_process(&pane, &owner_ref) != process.as_ref()
    {
        return ServerMessage::error(
            ErrorCode::StaleAgentEvent,
            "owner_unverified: question notice owner is no longer exact",
            Some(event_id),
        );
    }
    let before = state.question_notices.summary(&pane, None);
    state
        .question_notices
        .bind(pane.clone(), process.expect("verified process"));
    let result = state
        .question_notices
        .acknowledge(&pane, &owner_ref, through_order);
    // Include write failures: the health indicator is part of the projection.
    if before != state.question_notices.summary(&pane, None)
        && let Err(error) = state.leased.runtime.mark_projection_changed()
    {
        return ServerMessage::error(
            ErrorCode::StateInvariantViolation,
            error.to_string(),
            Some(event_id),
        );
    }
    match result {
        Ok(_) => ServerMessage::SnapshotAck {
            event_id,
            accepted_seq,
            snapshot_revision: state.leased.runtime.snapshot_revision(),
        },
        Err(reason) => ServerMessage::error(
            if reason == "persistence_pending" {
                ErrorCode::PersistFailed
            } else {
                ErrorCode::InvalidRequest
            },
            reason,
            Some(event_id),
        ),
    }
}

pub(in crate::daemon::server) fn maintain(coordinator: &ProductionV2Coordinator) {
    let _wake = ResolverWake(&coordinator.question_wake);
    let mut guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let Some(state) = guard.as_mut() else {
        return;
    };
    let current: std::collections::BTreeMap<_, _> = state
        .topology
        .panes
        .iter()
        .map(|pane| {
            let process = state
                .leased
                .runtime
                .tracker(&pane.pane_instance)
                .agent_process
                .filter(|process| {
                    crate::daemon::lifecycle::agent_process_start_token(process.pid)
                        .is_ok_and(|token| token == process.start_token)
                });
            (pane.pane_instance.clone(), process)
        })
        .collect();
    let before: Vec<_> = current
        .keys()
        .map(|pane| state.question_notices.summary(pane, None))
        .collect();
    state
        .question_notices
        .retain_panes(&current.keys().cloned().collect());
    for (pane, process) in &current {
        if let Some(process) = process {
            state.question_notices.bind(pane.clone(), process.clone());
        }
    }
    let changed = state.question_notices.reconcile(|pane, process| {
        let Some(current_process) = current.get(pane) else {
            return false;
        };
        if current_process
            .as_ref()
            .is_some_and(|current| current != process)
        {
            return false;
        }
        match crate::daemon::lifecycle::agent_process_start_token(process.pid) {
            Ok(token) => token == process.start_token,
            // An observation failure is not proof of death.
            Err(_) => {
                // SAFETY: signal 0 checks existence without sending a signal.
                unsafe {
                    libc::kill(process.pid as i32, 0) == 0
                        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                }
            }
        }
    });
    let after: Vec<_> = current
        .keys()
        .map(|pane| state.question_notices.summary(pane, None))
        .collect();
    if (changed || before != after)
        && let Err(error) = state.leased.runtime.mark_projection_changed()
    {
        coordinator.fail_stop(error.to_string());
    }
    drop(guard);
    reevaluate(coordinator);
}

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::daemon::protocol::v2::{ErrorCode, HookHealth, ServerMessage};
use crate::pane_state::{EventId, PaneInstance};
use crate::tmux::TmuxRunner;

use super::router::{ObservationBatchPayload, ObservationPollProjection, V2InternalMutation};
use super::state_helpers::{
    pane_snapshot_store, persist_pruned_sidebar_pins, production_store_error_response,
};
use super::{ProductionV2Coordinator, epoch_seconds};

#[cfg(test)]
mod tests;

pub(super) fn start_canonical_observation_worker(
    coordinator: Arc<ProductionV2Coordinator>,
    poll: Duration,
    capture: crate::daemon::workers::CaptureCoordinatorHandle,
) {
    thread::spawn(move || {
        let mut last_hook_check = Instant::now();
        let mut last_port_scan = None;
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let (dispatch, view_base, through_unread_order) = {
                let state_guard = coordinator
                    .state
                    .lock()
                    .expect("canonical state lock poisoned");
                let Some(state) = state_guard.as_ref() else {
                    thread::sleep(poll);
                    continue;
                };
                let mut panes = state
                    .topology
                    .panes
                    .iter()
                    .map(|pane| pane.pane_instance.clone())
                    .collect::<Vec<_>>();
                panes.extend(state.leased.runtime.tracked_panes());
                panes.sort();
                panes.dedup();
                (
                    state.leased.runtime.freeze_observation_dispatch(panes),
                    state.views.clone(),
                    state.leased.runtime.latest_unread_order(),
                )
            };
            let daemon_instance_id = coordinator
                .router
                .lock()
                .expect("v2 router lock poisoned")
                .daemon_instance_id()
                .clone();
            let mut projection =
                match query_observation_poll_projection(&coordinator, Duration::from_secs(1)) {
                    Ok(projection) => projection,
                    Err(error) if error.requires_daemon_exit() => {
                        coordinator.fail_stop(error.to_string());
                        break;
                    }
                    Err(error) => {
                        for snapshot in &dispatch {
                            match crate::daemon::workers::observation_envelope(
                                daemon_instance_id.clone(),
                                snapshot.pane_instance.clone(),
                                snapshot.base.clone(),
                                &snapshot.tracker,
                                crate::daemon::workers::ObservationSample {
                                    observed_at: epoch_seconds(),
                                    presence: crate::pane_state::AgentPresenceObservation::Unknown,
                                    capture: None,
                                    process: None,
                                },
                            ) {
                                Ok(envelope) => {
                                    let _ = coordinator.enqueue_internal(
                                        V2InternalMutation::PaneEvent(Box::new(envelope)),
                                    );
                                }
                                Err(build_error) => {
                                    coordinator.fail_stop(build_error.to_string());
                                    return;
                                }
                            }
                        }
                        let pane = dispatch
                            .first()
                            .map(|snapshot| snapshot.pane_instance.clone());
                        let _ = coordinator.enqueue_internal(
                            V2InternalMutation::DiagnosticProjection {
                                pane_instance: pane,
                                message: format!("observation_projection_failed: {error}"),
                            },
                        );
                        thread::sleep(poll);
                        continue;
                    }
                };
            projection.observation_bases = dispatch
                .iter()
                .map(|snapshot| (snapshot.pane_instance.clone(), snapshot.base.clone()))
                .collect();
            projection.view_base = view_base;
            projection.through_unread_order = through_unread_order;
            let nvim_markers = coordinator.query_nvim_pane_markers();
            let scan_ports = last_port_scan
                .is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(10));
            let processes = crate::daemon::workers::read_agent_process_snapshot(
                Duration::from_secs(1),
                scan_ports,
            );
            if scan_ports {
                last_port_scan = Some(Instant::now());
            }
            if let Some(markers) = nvim_markers {
                coordinator.cleanup_stale_nvim_pane_markers(&markers, &processes);
            }
            let poll_result = crate::daemon::workers::run_observation_poll(
                &capture,
                &dispatch,
                &processes,
                &daemon_instance_id,
                epoch_seconds(),
            );
            match poll_result {
                Ok(result) => {
                    let current = projection
                        .topology
                        .panes
                        .iter()
                        .map(|pane| pane.pane_instance.clone())
                        .collect::<std::collections::BTreeSet<_>>();
                    let first_pane = dispatch
                        .first()
                        .map(|snapshot| snapshot.pane_instance.clone());
                    let mut diagnostics = Vec::new();
                    let removals = match crate::daemon::workers::pane_removal_envelopes(
                        &daemon_instance_id,
                        &dispatch,
                        &current,
                        true,
                    ) {
                        Ok(removals) => removals,
                        Err(error) => {
                            diagnostics.push((
                                first_pane.clone(),
                                format!("pane_removal_build_failed: {error}"),
                            ));
                            Vec::new()
                        }
                    };
                    diagnostics.extend(
                        result
                            .diagnostics
                            .into_iter()
                            .map(|message| (first_pane.clone(), message)),
                    );
                    let _ = coordinator.enqueue_internal(V2InternalMutation::ObservationBatch(
                        Box::new(ObservationBatchPayload {
                            projection: Box::new(projection),
                            observations: result.envelopes,
                            removals,
                            diagnostics,
                        }),
                    ));
                }
                Err(error) if error.requires_daemon_exit() => {
                    coordinator.fail_stop(error.to_string());
                    break;
                }
                Err(error) => {
                    let _ =
                        coordinator.enqueue_internal(V2InternalMutation::DiagnosticProjection {
                            pane_instance: dispatch
                                .first()
                                .map(|snapshot| snapshot.pane_instance.clone()),
                            message: format!("observation_poll_failed: {error}"),
                        });
                }
            }
            if last_hook_check.elapsed() >= Duration::from_secs(10) {
                let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(1));
                match crate::daemon::view_hooks::monitor_hooks(
                    &runner,
                    &coordinator.incarnation.identity,
                ) {
                    Ok(health) => {
                        let _ = coordinator.enqueue_internal(
                            V2InternalMutation::HookHealthProjection {
                                health,
                                diagnostic: None,
                            },
                        );
                    }
                    Err(crate::daemon::view_hooks::HookError::ServerMismatch) => {
                        coordinator
                            .fail_stop("tmux server incarnation changed during hook monitor");
                        break;
                    }
                    Err(error) => {
                        let _ = coordinator.enqueue_internal(
                            V2InternalMutation::HookHealthProjection {
                                health: HookHealth::Degraded,
                                diagnostic: Some(format!("hook_health_degraded: {error}")),
                            },
                        );
                    }
                }
                last_hook_check = Instant::now();
            }
            thread::sleep(poll);
        }
    });
}

pub(super) fn query_full_topology(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
) -> Result<crate::daemon::topology::TopologySnapshot, crate::daemon::topology::TopologyError> {
    let framing = crate::daemon::topology::QueryFraming::generate()?;
    let args = crate::daemon::topology::poll_query_args(&framing);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout)
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
    let output = runner
        .run(&refs)
        .map_err(|error| crate::daemon::topology::TopologyError::Query(error.to_string()))?;
    crate::daemon::topology::parse_topology(&output, &framing, &coordinator.incarnation.identity)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservationPollFraming {
    pub(super) query: crate::daemon::topology::QueryFraming,
    pub(super) topology_end: String,
    pub(super) status_end: String,
    pub(super) client_end: String,
    pub(super) final_end: String,
}

impl ObservationPollFraming {
    fn generate() -> Result<Self, crate::daemon::topology::TopologyError> {
        Self::from_query(crate::daemon::topology::QueryFraming::generate()?)
    }

    pub(super) fn from_query(
        query: crate::daemon::topology::QueryFraming,
    ) -> Result<Self, crate::daemon::topology::TopologyError> {
        let token = query.token();
        if token.is_empty() {
            return Err(crate::daemon::topology::TopologyError::InvalidFraming(
                "observation poll query token is empty".to_string(),
            ));
        }
        Ok(Self {
            topology_end: format!("__vde_poll_topology_end_{token}__"),
            status_end: format!("__vde_poll_status_end_{token}__"),
            client_end: format!("__vde_poll_client_end_{token}__"),
            final_end: format!("__vde_poll_final_end_{token}__"),
            query,
        })
    }

    pub(super) fn query_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        append_tmux_command(
            &mut args,
            crate::daemon::topology::guarded_poll_query_args(&self.query),
        );
        append_tmux_display_marker(&mut args, &self.topology_end);
        append_tmux_command(
            &mut args,
            crate::daemon::topology::status_metadata_query_args(&self.query),
        );
        append_tmux_display_marker(&mut args, &self.status_end);
        append_tmux_command(
            &mut args,
            crate::daemon::view_hooks::guarded_client_view_query_args(self.query.token()),
        );
        append_tmux_display_marker(&mut args, &self.client_end);
        append_tmux_display_marker(&mut args, &self.final_end);
        args
    }
}

pub(super) fn append_tmux_command(args: &mut Vec<String>, command: Vec<String>) {
    if !args.is_empty() {
        args.push(";".to_string());
    }
    args.extend(command);
}

pub(super) fn append_tmux_display_marker(args: &mut Vec<String>, marker: &str) {
    append_tmux_command(
        args,
        vec![
            "display-message".to_string(),
            "-p".to_string(),
            marker.to_string(),
        ],
    );
}

#[derive(Debug)]
pub(super) enum ObservationPollQueryError {
    Framing(String),
    Topology(crate::daemon::topology::TopologyError),
    Client(crate::daemon::view_hooks::FreshVisibilityError),
}

impl ObservationPollQueryError {
    fn requires_daemon_exit(&self) -> bool {
        match self {
            Self::Framing(_) => false,
            Self::Topology(error) => error.requires_daemon_exit(),
            Self::Client(error) => error.requires_daemon_exit(),
        }
    }
}

impl std::fmt::Display for ObservationPollQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Framing(message) => formatter.write_str(message),
            Self::Topology(error) => write!(formatter, "{error}"),
            Self::Client(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ObservationPollQueryError {}

pub(super) fn query_observation_poll_projection(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
) -> Result<ObservationPollProjection, ObservationPollQueryError> {
    let observation_seq = coordinator.begin_witness_observation();
    let framing =
        ObservationPollFraming::generate().map_err(ObservationPollQueryError::Topology)?;
    let args = framing.query_args();
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout)
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
    let output = runner.run(&refs).map_err(|error| {
        ObservationPollQueryError::Topology(crate::daemon::topology::TopologyError::Query(
            error.to_string(),
        ))
    })?;
    let mut projection =
        parse_observation_poll_projection(&output, &framing, &coordinator.incarnation.identity)?;
    projection.observation_seq = observation_seq;
    Ok(projection)
}

pub(super) fn parse_observation_poll_projection(
    output: &str,
    framing: &ObservationPollFraming,
    expected_identity: &crate::daemon::topology::ServerIdentity,
) -> Result<ObservationPollProjection, ObservationPollQueryError> {
    crate::daemon::topology::ensure_query_output_size(output)
        .map_err(ObservationPollQueryError::Topology)?;
    let (topology_frame, remainder) =
        split_observation_poll_frame(output, &framing.topology_end, "topology")?;
    let (status_frame, remainder) =
        split_observation_poll_frame(remainder, &framing.status_end, "status")?;
    let (client_frame, remainder) =
        split_observation_poll_frame(remainder, &framing.client_end, "client")?;
    let expected_final = format!("{}\n", framing.final_end);
    if remainder != expected_final {
        return Err(ObservationPollQueryError::Framing(
            "observation poll final marker is missing or not final".to_string(),
        ));
    }

    let topology_frame = format!("{topology_frame}\n");
    let status_frame = format!("{status_frame}\n");
    let client_frame = format!("{client_frame}\n");
    let topology =
        crate::daemon::topology::parse_topology(&topology_frame, &framing.query, expected_identity)
            .map_err(ObservationPollQueryError::Topology)?;
    let status = crate::daemon::topology::parse_status_metadata(
        &status_frame,
        &framing.query,
        expected_identity,
    )
    .map_err(ObservationPollQueryError::Topology)?;
    let witnesses = crate::daemon::view_hooks::parse_client_view_query(
        &client_frame,
        framing.query.token(),
        expected_identity,
    )
    .map_err(ObservationPollQueryError::Client)?;

    Ok(ObservationPollProjection {
        observation_seq: 0,
        topology,
        status_metadata: status_projection_metadata(status, &witnesses),
        witnesses,
        observation_bases: BTreeMap::new(),
        view_base: crate::daemon::view_hooks::CurrentClientViews::default(),
        through_unread_order: 0,
    })
}

pub(super) fn split_observation_poll_frame<'a>(
    output: &'a str,
    marker: &str,
    section: &str,
) -> Result<(&'a str, &'a str), ObservationPollQueryError> {
    let delimiter = format!("\n{marker}\n");
    let Some((frame, remainder)) = output.split_once(&delimiter) else {
        return Err(ObservationPollQueryError::Framing(format!(
            "observation poll {section} marker is missing"
        )));
    };
    if remainder.starts_with(&format!("{marker}\n")) || remainder.contains(&delimiter) {
        return Err(ObservationPollQueryError::Framing(format!(
            "observation poll {section} marker is duplicated"
        )));
    }
    Ok((frame, remainder))
}

pub(super) fn query_status_projection_metadata(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
    witnesses: &[crate::pane_state::ClientWitness],
) -> Result<crate::daemon::runtime::StatusProjectionMetadata, crate::daemon::topology::TopologyError>
{
    let framing = crate::daemon::topology::QueryFraming::generate()?;
    let args = crate::daemon::topology::status_metadata_query_args(&framing);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout)
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
    let output = runner
        .run(&refs)
        .map_err(|error| crate::daemon::topology::TopologyError::Query(error.to_string()))?;
    let snapshot = crate::daemon::topology::parse_status_metadata(
        &output,
        &framing,
        &coordinator.incarnation.identity,
    )?;
    Ok(status_projection_metadata(snapshot, witnesses))
}

pub(super) fn status_projection_metadata(
    snapshot: crate::daemon::topology::StatusMetadataSnapshot,
    witnesses: &[crate::pane_state::ClientWitness],
) -> crate::daemon::runtime::StatusProjectionMetadata {
    let attached_sessions = crate::session::regular_client_session_ids(witnesses);
    let mut metadata = crate::daemon::runtime::StatusProjectionMetadata::default();
    for session in snapshot.sessions {
        let attached = attached_sessions.contains(&session.session_id);
        metadata.sessions.insert(
            session.session_id,
            crate::daemon::runtime::SessionProjectionMetadata {
                session_name: session.session_name,
                project_path: session.project_path,
                attached: Some(attached),
                created_at: Some(session.created_at),
            },
        );
    }
    for window in snapshot.windows {
        metadata.windows.insert(
            window.window_id,
            crate::daemon::runtime::WindowProjectionMetadata {
                bell: Some(window.bell),
                activity: Some(window.activity),
                silence: Some(window.silence),
            },
        );
    }
    metadata
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WitnessObservation {
    pub(super) seq: u64,
    pub(super) witnesses: Vec<crate::pane_state::ClientWitness>,
}

pub(super) fn query_client_witnesses(
    coordinator: &ProductionV2Coordinator,
    timeout: Duration,
) -> Result<WitnessObservation, crate::daemon::view_hooks::FreshVisibilityError> {
    let seq = coordinator.begin_witness_observation();
    let token = EventId::generate()
        .map_err(|error| crate::daemon::view_hooks::FreshVisibilityError::Query(error.to_string()))?
        .as_str()
        .to_string();
    let args = crate::daemon::view_hooks::client_view_query_args(&token);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let runner = crate::tmux::SystemTmuxRunner::from_env(timeout);
    let output = runner.run(&refs).map_err(|error| {
        crate::daemon::view_hooks::FreshVisibilityError::Query(error.to_string())
    })?;
    let witnesses = crate::daemon::view_hooks::parse_client_view_query(
        &output,
        &token,
        &coordinator.incarnation.identity,
    )?;
    Ok(WitnessObservation { seq, witnesses })
}

pub(super) fn refresh_full_topology(
    coordinator: &ProductionV2Coordinator,
) -> Result<u64, crate::pane_state::store::StoreError> {
    let topology = query_full_topology(coordinator, Duration::from_secs(1)).map_err(|error| {
        if error.requires_daemon_exit() {
            crate::pane_state::store::StoreError::FailStop(error.to_string())
        } else {
            crate::pane_state::store::StoreError::PersistFailed(error.to_string())
        }
    })?;
    let observation_floor = coordinator.witness_observation_seq.load(Ordering::SeqCst);
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard.as_mut().ok_or_else(|| {
        crate::pane_state::store::StoreError::PersistFailed("daemon is hydrating".to_string())
    })?;
    state.replace_topology_and_fence_observations(topology, observation_floor)?;
    persist_pruned_sidebar_pins(coordinator, state)?;
    Ok(state.leased.runtime.snapshot_revision())
}

pub(super) fn apply_observation_poll_projection(
    coordinator: &ProductionV2Coordinator,
    projection: ObservationPollProjection,
) -> Result<u64> {
    {
        let mut state_guard = coordinator
            .state
            .lock()
            .expect("canonical state lock poisoned");
        let state = state_guard
            .as_mut()
            .context("state initialized before observation projection")?;
        if state.apply_topology_observation(projection.topology, projection.observation_seq)? {
            persist_pruned_sidebar_pins(coordinator, state)?;
            state.replace_status_metadata(projection.status_metadata)?;
        }
    }
    reconcile_views_with_witnesses(
        coordinator,
        projection.observation_seq,
        &projection.witnesses,
        projection.through_unread_order,
        Some(&projection.observation_bases),
        Some(&projection.view_base),
    )?;
    Ok(coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_ref()
        .map_or(0, |state| state.leased.runtime.snapshot_revision()))
}

pub(super) fn observation_poll_error_response(
    coordinator: &ProductionV2Coordinator,
    error: anyhow::Error,
) -> ServerMessage {
    match error.downcast::<crate::pane_state::store::StoreError>() {
        Ok(store_error) => production_store_error_response(coordinator, store_error, None),
        Err(error) => ServerMessage::error(ErrorCode::InternalError, error.to_string(), None),
    }
}

pub(super) fn targeted_pane_refresh_response(
    coordinator: &ProductionV2Coordinator,
    pane_id: &str,
) -> ServerMessage {
    let io = crate::daemon::topology::SystemTargetedRefreshIo::new(
        coordinator
            .env
            .get("VDE_TMUX_SOCKET_NAME")
            .cloned()
            .filter(|value| !value.trim().is_empty()),
    );
    let outcome =
        crate::daemon::topology::targeted_refresh(&io, pane_id, &coordinator.incarnation.identity);
    targeted_pane_refresh_outcome_response(coordinator, pane_id, outcome)
}

pub(super) fn targeted_pane_refresh_outcome_response(
    coordinator: &ProductionV2Coordinator,
    pane_id: &str,
    outcome: Result<
        crate::daemon::topology::TargetedRefreshOutcome,
        crate::daemon::topology::TopologyError,
    >,
) -> ServerMessage {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    match outcome {
        Ok(crate::daemon::topology::TargetedRefreshOutcome::NotFound) => {
            ServerMessage::error(ErrorCode::PaneNotFound, "pane was not found", None)
        }
        Ok(crate::daemon::topology::TargetedRefreshOutcome::Found(pane)) => {
            let observation_floor = coordinator.witness_observation_seq.load(Ordering::SeqCst);
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            let state = state_guard
                .as_mut()
                .expect("state initialized before targeted refresh");
            let mut topology = state.topology.clone();
            topology
                .panes
                .retain(|existing| existing.pane_instance.pane_id != pane_id);
            topology.panes.push(*pane);
            topology
                .panes
                .sort_by(|left, right| left.pane_instance.cmp(&right.pane_instance));
            if let Err(error) =
                state.replace_topology_and_fence_observations(topology, observation_floor)
            {
                return production_store_error_response(coordinator, error, None);
            }
            match state.pane_presentation(pane_id) {
                Some(pane) => ServerMessage::PaneResult {
                    snapshot_revision: state.leased.runtime.snapshot_revision(),
                    pane,
                },
                None => ServerMessage::error(
                    ErrorCode::InternalError,
                    "targeted refresh did not populate pane cache",
                    None,
                ),
            }
        }
        Err(error) => {
            if matches!(
                &error,
                crate::daemon::topology::TopologyError::Query(_)
                    | crate::daemon::topology::TopologyError::Deadline
            ) {
                let diagnostic_result = {
                    let mut state_guard = coordinator
                        .state
                        .lock()
                        .expect("canonical state lock poisoned");
                    state_guard
                        .as_mut()
                        .expect("state initialized before targeted refresh")
                        .add_global_diagnostic(
                            ErrorCode::InternalError,
                            format!("targeted pane refresh for {pane_id} failed: {error}"),
                        )
                };
                if let Err(store_error) = diagnostic_result {
                    return production_store_error_response(coordinator, store_error, None);
                }
            }
            if error.requires_daemon_exit() {
                coordinator.fail_stop(error.to_string());
            }
            ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
        }
    }
}

pub(super) fn reconcile_views_with_witnesses(
    coordinator: &ProductionV2Coordinator,
    observation_seq: u64,
    witnesses: &[crate::pane_state::ClientWitness],
    through_unread_order: u64,
    _observation_bases: Option<
        &BTreeMap<PaneInstance, Option<crate::pane_state::StoredStateDescriptor>>,
    >,
    view_base: Option<&crate::daemon::view_hooks::CurrentClientViews>,
) -> Result<()> {
    let daemon_instance_id = coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .daemon_instance_id()
        .clone();
    let mut state_guard = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned");
    let state = state_guard
        .as_mut()
        .expect("state initialized before reconciliation");
    if !observation_view_base_matches(&state.views, view_base) {
        return Ok(());
    }
    state.reconcile_peek_leases(witnesses, observation_seq);
    let read_event_id = EventId::generate()?;
    let focused_panes = state.read_authorized_panes(witnesses);
    let pane_reads = crate::daemon::view_hooks::pane_read_envelopes_for_panes(
        &daemon_instance_id,
        &read_event_id,
        &focused_panes,
        through_unread_order,
        &state.records_snapshot(),
    )?;
    let window_panes = state.window_panes();
    let revision_before = state.leased.runtime.snapshot_revision();
    if !pane_reads.is_empty() {
        let read_panes = pane_reads
            .iter()
            .map(|envelope| envelope.pane_instance.clone())
            .collect::<BTreeSet<_>>();
        let mut io = pane_snapshot_store(coordinator);
        state
            .leased
            .runtime
            .apply_pane_reads(&mut io, &pane_reads)?;
        state.clear_peeks_for_read_panes(&read_panes);
    }
    let mut next_views = state.views.clone();
    let registry_changed = crate::daemon::view_hooks::reconcile_current_views(
        &mut next_views,
        witnesses,
        &window_panes,
    )?;
    state.views = next_views;
    state.leased.runtime.finish_sequenced_projection(
        None,
        std::iter::empty(),
        registry_changed,
        revision_before,
    )?;
    Ok(())
}

pub(super) fn observation_view_base_matches(
    current: &crate::daemon::view_hooks::CurrentClientViews,
    observation_base: Option<&crate::daemon::view_hooks::CurrentClientViews>,
) -> bool {
    observation_base.is_none_or(|base| current == base)
}

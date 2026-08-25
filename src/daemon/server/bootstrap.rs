use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::pane_state::PaneInstance;
use crate::tmux::TmuxRunner;

use super::effects::{
    start_agent_prompt_timeout_worker, start_canonical_git_worker,
    start_sidebar_completion_forwarder, start_status_push_worker,
    start_task_summary_completion_forwarder, start_tmux_server_liveness_monitor,
};
use super::framing::{V2ConnectionThreadLimiter, write_v2_overload_response};
use super::mutations::pane::pane_belongs_to_run_epoch;
use super::observation::{
    query_client_witnesses, query_full_topology, query_status_projection_metadata,
    reconcile_views_with_witnesses, start_canonical_observation_worker,
};
use super::router::{V2InternalMutation, V2Route};
use super::{
    ProductionV2Coordinator, SHUTDOWN_SIGNAL_WRITE_FD, StatusPushTrigger,
    V2_CONNECTION_THREAD_CAPACITY, V2_RESERVED_NON_STREAMING_CONNECTION_CAPACITY,
    apply_production_mutation, epoch_seconds, handle_v2_runtime_stream, start_v2_mutation_worker,
};

#[cfg(test)]
mod tests;

pub fn run_runtime_daemon_server(
    config: crate::config::Config,
    socket_path: &Path,
    env: &std::collections::BTreeMap<String, String>,
    incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
) -> Result<()> {
    incarnation.verify(
        &crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3)),
        env,
    )?;
    if let Some(parent) = socket_path.parent() {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    let writer_namespace = crate::daemon::writer_lease_namespace(&incarnation.hash);
    if let Some(parent) = writer_namespace.parent() {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    let leased =
        crate::daemon::runtime::LeasedCanonicalPaneStateRuntime::acquire(&writer_namespace)
            .map_err(anyhow::Error::new)?;
    let Some((listener, _instance_lock, socket_cleanup)) = bind_daemon_listener(socket_path)?
    else {
        return Ok(());
    };

    let (coordinator, mut runtime_cleanup) = initialize_runtime_daemon_post_bind(
        &config,
        socket_path,
        env,
        incarnation,
        socket_cleanup,
    )?;
    install_shutdown_signal_handler(coordinator.clone())?;
    let listener_coordinator = coordinator.clone();
    let connection_limiter = Arc::new(V2ConnectionThreadLimiter::new(
        V2_CONNECTION_THREAD_CAPACITY,
        V2_RESERVED_NON_STREAMING_CONNECTION_CAPACITY,
    ));
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let Some(mut connection_permit) = connection_limiter.try_acquire() else {
                        // Report overload without creating another connection thread. The
                        // short write deadline keeps overload handling itself bounded.
                        write_v2_overload_response(&mut stream);
                        drop(stream);
                        continue;
                    };
                    let coordinator = listener_coordinator.clone();
                    if let Err(error) = thread::Builder::new().spawn(move || {
                        if let Err(error) = handle_v2_runtime_stream(
                            coordinator.clone(),
                            stream,
                            &mut connection_permit,
                        ) {
                            coordinator
                                .log_daemon_error(&format!("daemon connection error: {error:#}"));
                        }
                    }) {
                        listener_coordinator.log_daemon_error(&format!(
                            "daemon connection thread spawn failed: {error}"
                        ));
                    }
                }
                Err(error) => {
                    listener_coordinator
                        .log_daemon_error(&format!("daemon listener error: {error:#}"));
                    break;
                }
            }
        }
    });

    bootstrap_v2_runtime(&coordinator, leased, env, &config)?;
    start_pane_switch_trigger_worker(coordinator.clone());
    start_tmux_server_liveness_monitor(coordinator.clone())?;
    start_v2_mutation_worker(coordinator.clone());
    start_agent_prompt_timeout_worker(coordinator.clone());
    start_sidebar_completion_forwarder(coordinator.clone());
    start_task_summary_completion_forwarder(coordinator.clone());
    let capture = crate::daemon::workers::start_capture_coordinator(
        Arc::new(crate::daemon::workers::SystemObservationWorkerIo::new(
            coordinator
                .env
                .get("VDE_TMUX_SOCKET_NAME")
                .cloned()
                .filter(|value| !value.trim().is_empty()),
        )),
        coordinator.incarnation.identity.clone(),
    );
    start_canonical_observation_worker(
        coordinator.clone(),
        Duration::from_millis(config.daemon.poll_ms),
        capture.clone(),
    );
    start_canonical_git_worker(
        coordinator.clone(),
        Duration::from_millis(config.daemon.git.poll_interval_ms),
        Duration::from_millis(config.daemon.git.timeout_ms),
    );
    start_status_push_worker(coordinator.clone());

    coordinator.wait_for_shutdown();
    runtime_cleanup.cleanup()?;
    Ok(())
}

pub(super) fn start_pane_switch_trigger_worker(coordinator: Arc<ProductionV2Coordinator>) {
    const TRIGGER_QUEUE_CAPACITY: usize = 32;
    let (triggers, receiver) = mpsc::sync_channel(TRIGGER_QUEUE_CAPACITY);
    let waiter = coordinator.clone();
    thread::spawn(move || {
        while !waiter.pane_switch_trigger_shutdown.load(Ordering::SeqCst) {
            let status = Command::new("tmux")
                .arg("-S")
                .arg(&waiter.incarnation.socket_path)
                .arg("wait-for")
                .arg(waiter.pane_switch_channel())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if waiter.pane_switch_trigger_shutdown.load(Ordering::SeqCst) {
                break;
            }
            match status {
                Ok(status) if status.success() => {
                    let _ = triggers.try_send(());
                }
                _ => thread::sleep(Duration::from_millis(100)),
            }
        }
    });
    thread::spawn(move || {
        while receiver.recv().is_ok() {
            if coordinator
                .pane_switch_trigger_shutdown
                .load(Ordering::SeqCst)
                || coordinator.shutdown.load(Ordering::SeqCst)
            {
                return;
            }
            coordinator.handle_pane_switch_trigger();
        }
    });
}

pub(super) fn initialize_runtime_daemon_post_bind(
    config: &crate::config::Config,
    socket_path: &Path,
    env: &std::collections::BTreeMap<String, String>,
    incarnation: crate::daemon::lifecycle::TmuxServerIncarnation,
    mut socket_cleanup: BoundDaemonSocketCleanup,
) -> Result<(Arc<ProductionV2Coordinator>, RuntimeDaemonCleanup)> {
    let notification_command = (config.notify.enabled && !config.notify.command.trim().is_empty())
        .then(|| config.notify.command.clone());
    let coordinator = Arc::new(ProductionV2Coordinator::new_with_task_summary(
        incarnation,
        env.clone(),
        notification_command,
        config.sidebar.task_summary.clone(),
    )?);
    coordinator.configure_health(config);
    let daemon_instance_id = coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .daemon_instance_id()
        .clone();
    let process_identity =
        crate::daemon::lifecycle::daemon_process_identity(socket_path, &daemon_instance_id)?;
    socket_cleanup.verify_process_identity(&process_identity)?;
    crate::daemon::lifecycle::update_lifecycle_record(
        env,
        &coordinator.incarnation.hash,
        |record| {
            record.process = Some(process_identity.clone());
            record.health = crate::daemon::lifecycle::LifecycleHealth::Stable;
            record.last_transition_error = None;
            Ok(())
        },
    )?;
    let runtime_cleanup = RuntimeDaemonCleanup::new(
        env,
        &coordinator.incarnation.hash,
        socket_path,
        process_identity,
    );
    socket_cleanup.disarm();
    Ok((coordinator, runtime_cleanup))
}

pub(super) struct BoundDaemonSocketCleanup {
    socket_path: PathBuf,
    socket_device: u64,
    socket_inode: u64,
    active: bool,
}

impl BoundDaemonSocketCleanup {
    pub(super) fn new(socket_path: &Path) -> Result<Self> {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

        let metadata = fs::symlink_metadata(socket_path)
            .with_context(|| format!("failed to stat bound socket {}", socket_path.display()))?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            bail!(
                "bound daemon socket identity is invalid: {}",
                socket_path.display()
            );
        }
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
            active: true,
        })
    }

    fn disarm(&mut self) {
        self.active = false;
    }

    pub(super) fn verify_process_identity(
        &self,
        process_identity: &crate::daemon::lifecycle::DaemonProcessIdentity,
    ) -> Result<()> {
        if process_identity.socket_device != self.socket_device
            || process_identity.socket_inode != self.socket_inode
        {
            bail!(
                "daemon socket identity changed during post-bind initialization: {}",
                self.socket_path.display()
            );
        }
        Ok(())
    }

    fn cleanup(&mut self) {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

        if !self.active {
            return;
        }
        self.active = false;
        let Ok(metadata) = fs::symlink_metadata(&self.socket_path) else {
            return;
        };
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.dev() != self.socket_device
            || metadata.ino() != self.socket_inode
        {
            return;
        }
        if fs::remove_file(&self.socket_path).is_ok()
            && let Some(parent) = self.socket_path.parent()
        {
            let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
        }
    }
}

impl Drop for BoundDaemonSocketCleanup {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub(super) struct RuntimeDaemonCleanup {
    env: std::collections::BTreeMap<String, String>,
    incarnation_hash: String,
    socket_path: PathBuf,
    process_identity: crate::daemon::lifecycle::DaemonProcessIdentity,
    active: bool,
}

impl RuntimeDaemonCleanup {
    pub(super) fn new(
        env: &std::collections::BTreeMap<String, String>,
        incarnation_hash: &str,
        socket_path: &Path,
        process_identity: crate::daemon::lifecycle::DaemonProcessIdentity,
    ) -> Self {
        Self {
            env: env.clone(),
            incarnation_hash: incarnation_hash.to_string(),
            socket_path: socket_path.to_path_buf(),
            process_identity,
            active: true,
        }
    }

    fn cleanup(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        crate::daemon::lifecycle::remove_force_stopped_socket(
            &self.socket_path,
            &self.process_identity,
        )?;
        crate::daemon::lifecycle::update_lifecycle_record(
            &self.env,
            &self.incarnation_hash,
            |record| {
                if record.process.as_ref() == Some(&self.process_identity) {
                    record.process = None;
                }
                Ok(())
            },
        )?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RuntimeDaemonCleanup {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub(super) fn bootstrap_v2_runtime(
    coordinator: &ProductionV2Coordinator,
    mut leased: crate::daemon::runtime::LeasedCanonicalPaneStateRuntime,
    env: &std::collections::BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<()> {
    let agent_state_root = crate::agent_state::state_root(env, &coordinator.incarnation.hash);
    let agent_runtime = crate::agent_state::runtime::AgentRuntime::open(
        agent_state_root.clone(),
        coordinator.incarnation.hash.clone(),
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "agent state {} is invalid: {error}; use the explicit agent storage recovery command",
            agent_state_root.display()
        )
    })?;
    *coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned") = Some(agent_runtime);
    let runner = crate::tmux::SystemTmuxRunner::from_env(Duration::from_secs(3))
        .with_max_output_bytes(crate::daemon::topology::MAX_TMUX_QUERY_OUTPUT_BYTES);
    crate::daemon::agent_dispatch::cleanup_stale_prompt_buffers(&runner).map_err(|error| {
        anyhow::anyhow!("failed to clean stale guarded prompt buffers: {error}")
    })?;
    crate::daemon::view_hooks::install_hooks(&runner, &coordinator.incarnation.identity)
        .map_err(|error| anyhow::anyhow!("failed to install pane-state hooks: {error}"))?;
    coordinator
        .router
        .lock()
        .expect("v2 router lock poisoned")
        .begin_hydration()
        .map_err(anyhow::Error::msg)?;

    let session_framing = crate::daemon::topology::QueryFraming::generate()?;
    let session_args = crate::daemon::topology::targeted_session_query_args(&session_framing);
    let session_refs = session_args.iter().map(String::as_str).collect::<Vec<_>>();
    let session_output = runner.run(&session_refs)?;
    let session_count = crate::daemon::topology::parse_session_count(
        &session_output,
        &session_framing,
        &coordinator.incarnation.identity,
    )?;
    let (topology, witnesses) = if session_count == 0 {
        (
            crate::daemon::topology::TopologySnapshot {
                server_identity: coordinator.incarnation.identity.clone(),
                panes: Vec::new(),
            },
            Vec::new(),
        )
    } else {
        let topology = query_full_topology(coordinator, Duration::from_secs(1))?;
        let observation = query_client_witnesses(coordinator, Duration::from_secs(1))?;
        (topology, observation.witnesses)
    };
    let snapshot_path =
        crate::pane_state::snapshot::snapshot_path(env, &coordinator.incarnation.hash);
    let mut records = crate::pane_state::snapshot::load_snapshot(
        &snapshot_path,
        &coordinator.incarnation.identity,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "pane snapshot {} is invalid: {error}; remove {} to reset all pane state",
            snapshot_path.display(),
            snapshot_path.display()
        )
    })?;
    let record_count = records.len();
    crate::pane_state::snapshot::retain_topology_records(
        &mut records,
        topology.panes.iter().map(|pane| pane.pane_instance.clone()),
    );
    let mut records_changed = records.len() != record_count;
    let reconciled_runs = coordinator
        .agent_runtime
        .lock()
        .expect("agent runtime lock poisoned")
        .as_mut()
        .expect("agent runtime initialized before pane reconciliation")
        .reconcile_panes_after_restart(&records, epoch_seconds())
        .map_err(|error| anyhow::anyhow!("failed to reconcile durable runs at startup: {error}"))?;
    let mut latest_unread_order = records
        .values()
        .filter_map(|pane| pane.unread.latest.as_ref().map(|latest| latest.order))
        .max()
        .unwrap_or_default();
    for pane in records.values_mut() {
        let current = reconciled_runs
            .iter()
            .filter(|run| pane_belongs_to_run_epoch(pane, run))
            .max_by_key(|run| run.run_seq);
        let changed = if let Some(run) = current {
            pane.reconcile_current_run(
                crate::pane_state::CurrentDurableRunProjection {
                    run_id: run.run_id.as_str().to_string(),
                    run_seq: run.run_seq,
                    run_revision: run.revision,
                },
                run.execution_active(),
                run.updated_at,
                latest_unread_order,
            )
        } else {
            pane.clear_current_run()
        }
        .map_err(|error| {
            anyhow::anyhow!("failed to repair pane durable run projection: {error}")
        })?;
        latest_unread_order = latest_unread_order.max(
            pane.unread
                .latest
                .as_ref()
                .map(|latest| latest.order)
                .unwrap_or_default(),
        );
        records_changed |= changed;
    }
    if records_changed {
        crate::pane_state::snapshot::save_snapshot(
            &snapshot_path,
            &coordinator.incarnation.identity,
            &records,
        )?;
    }
    leased.hydrate(records)?;
    let mut views = crate::daemon::view_hooks::CurrentClientViews::default();
    let mut window_panes = BTreeMap::<String, Vec<PaneInstance>>::new();
    for pane in &topology.panes {
        window_panes
            .entry(pane.window_id.clone())
            .or_default()
            .push(pane.pane_instance.clone());
    }
    views
        .reconcile(&witnesses, &window_panes)
        .map_err(|error| anyhow::anyhow!("failed to build initial view registry: {error}"))?;
    let status_metadata =
        query_status_projection_metadata(coordinator, Duration::from_secs(1), &witnesses)?;
    let state_path = crate::sidebar::store::state_path(env, &coordinator.incarnation.socket_path);
    let mut sidebar_preferences = crate::sidebar::store::load_state(&state_path)?;
    let present_panes = topology
        .panes
        .iter()
        .map(|pane| pane.pane_instance.clone())
        .collect::<BTreeSet<_>>();
    if sidebar_preferences.retain_panes(&present_panes) {
        crate::sidebar::store::save_state(&state_path, &sidebar_preferences)?;
    }
    let category_state_path =
        crate::category::store::state_path(env, &coordinator.incarnation.socket_path);
    let category_state = crate::category::store::load_state(&category_state_path)?;
    let mut canonical = crate::daemon::runtime::CanonicalCoordinatorState::new(
        leased,
        topology,
        views,
        sidebar_preferences,
    );
    canonical.status_metadata = status_metadata;
    canonical.category_state = category_state;
    canonical.projection_config = config.clone();
    *coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned") = Some(canonical);
    let daemon_socket =
        crate::daemon::daemon_socket_path_for_incarnation(env, None, &coordinator.incarnation.hash);
    crate::daemon::lifecycle::publish_runtime_context(
        &runner,
        &coordinator.incarnation,
        &daemon_socket,
    )?;

    let mut initial_reconciliation_queued = false;
    loop {
        let queued = coordinator
            .router
            .lock()
            .expect("v2 router lock poisoned")
            .take_bootstrap_fifo();
        for mutation in queued {
            let accepted_seq = mutation.accepted_seq;
            let response = apply_production_mutation(coordinator, mutation);
            coordinator.publish_resolved_snapshot()?;
            if !coordinator.is_deferred_response(accepted_seq) {
                coordinator.complete(accepted_seq, response);
            }
        }
        if !initial_reconciliation_queued {
            let mut router = coordinator.router.lock().expect("v2 router lock poisoned");
            for mutation in [
                V2InternalMutation::RefreshTopology,
                V2InternalMutation::ReconcileViews,
            ] {
                if matches!(router.accept_internal(mutation), V2Route::Fatal(_)) {
                    bail!("accepted sequence overflow during initial reconciliation");
                }
            }
            initial_reconciliation_queued = true;
            continue;
        }
        coordinator.publish_resolved_snapshot()?;
        coordinator.drive_status_push(StatusPushTrigger::Snapshot)?;
        let mut router = coordinator.router.lock().expect("v2 router lock poisoned");
        if router.enter_serving_if_bootstrap_empty() {
            drop(router);
            // Attaching earlier would let a bootstrap failure emit a client-detached hook that
            // starts another enabled daemon. Serving is the first point at which the control
            // client's lifecycle is allowed to affect tmux hooks.
            coordinator.start_tmux_control();
            break;
        }
    }
    Ok(())
}

pub(super) fn initial_view_reconciliation(coordinator: &ProductionV2Coordinator) -> Result<()> {
    let through_unread_order = coordinator
        .state
        .lock()
        .expect("canonical state lock poisoned")
        .as_ref()
        .map_or(0, |state| state.leased.runtime.latest_unread_order());
    let observation = match query_client_witnesses(coordinator, Duration::from_millis(250)) {
        Ok(observation) => observation,
        Err(error) if error.requires_daemon_exit() => return Err(error.into()),
        Err(error) => {
            let mut state_guard = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned");
            if let Some(state) = state_guard.as_mut()
                && let Some(pane) = state.topology.panes.first()
            {
                state.leased.runtime.add_diagnostic(
                    pane.pane_instance.clone(),
                    format!("initial_view_reconciliation_failed: {error}"),
                )?;
            }
            return Ok(());
        }
    };
    reconcile_views_with_witnesses(
        coordinator,
        observation.seq,
        &observation.witnesses,
        through_unread_order,
        None,
        None,
    )
}

pub(super) fn bind_daemon_listener(
    socket_path: &Path,
) -> Result<
    Option<(
        UnixListener,
        crate::daemon::lifecycle::DaemonFileLock,
        BoundDaemonSocketCleanup,
    )>,
> {
    if let Some(parent) = socket_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        crate::daemon::lifecycle::ensure_secure_socket_dir(parent)?;
    }
    let Some(instance_lock) =
        crate::daemon::lifecycle::try_acquire_daemon_instance_lock(socket_path)?
    else {
        return Ok(None);
    };
    if socket_path.exists() {
        crate::daemon::lifecycle::verify_stale_socket_can_be_removed(
            socket_path,
            Instant::now() + Duration::from_secs(3),
        )?;
        fs::remove_file(socket_path)
            .with_context(|| format!("failed to remove {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    let socket_cleanup = BoundDaemonSocketCleanup::new(socket_path)?;
    Ok(Some((listener, instance_lock, socket_cleanup)))
}

pub(super) fn install_shutdown_signal_handler(
    coordinator: Arc<ProductionV2Coordinator>,
) -> Result<()> {
    let mut fds = [0; 2];
    // SAFETY: `pipe` writes two valid file descriptors into `fds` on success.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!(
            "failed to create shutdown signal pipe: {}",
            std::io::Error::last_os_error()
        );
    }
    SHUTDOWN_SIGNAL_WRITE_FD.store(fds[1], Ordering::SeqCst);
    install_shutdown_signal(libc::SIGTERM)?;
    install_shutdown_signal(libc::SIGINT)?;
    // SAFETY: `fds[0]` is a fresh read end returned by `pipe` and is now owned by `File`.
    let reader = unsafe { fs::File::from_raw_fd(fds[0]) };
    spawn_shutdown_forwarder(reader, coordinator);
    Ok(())
}

pub(super) fn install_shutdown_signal(signum: libc::c_int) -> Result<()> {
    // SAFETY: zeroed `sigaction` is immediately initialized with a handler, empty mask, and flags.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = shutdown_signal_handler as *const () as usize;
    action.sa_flags = 0;
    // SAFETY: `action.sa_mask` is a valid signal set field to initialize.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    // SAFETY: `sigaction` installs a plain async-signal-safe handler for the given signal.
    if unsafe { libc::sigaction(signum, &action, std::ptr::null_mut()) } != 0 {
        bail!(
            "failed to install signal handler for {signum}: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

extern "C" fn shutdown_signal_handler(_signum: libc::c_int) {
    let fd = SHUTDOWN_SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = [1_u8];
    // SAFETY: `write` is async-signal-safe; fd is the stored pipe write end.
    unsafe {
        let _ = libc::write(fd, byte.as_ptr().cast(), byte.len());
    }
}

pub(super) fn spawn_shutdown_forwarder<R>(mut reader: R, coordinator: Arc<ProductionV2Coordinator>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut byte = [0_u8; 1];
        if reader.read(&mut byte).is_ok() {
            coordinator.begin_signal_shutdown();
        }
    });
}

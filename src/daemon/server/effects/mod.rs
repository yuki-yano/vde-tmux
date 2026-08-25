use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::daemon::protocol::v2::ErrorCode;
use crate::pane_state::EventId;

use super::contracts::{SidebarEffectCompletion, SidebarEffectResult};
use super::router::V2InternalMutation;
use super::{
    ProductionV2Coordinator, StatusPushTrigger, TMUX_SERVER_LIVENESS_POLL_INTERVAL, epoch_seconds,
};

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct NotificationWorkerJob {
    pub(super) pane_id: String,
    pub(super) agent: String,
}

pub(super) struct SidebarTmuxJob {
    pub(super) effect: crate::daemon::runtime::CanonicalSidebarEffect,
    pub(super) original_accepted_seq: u64,
    pub(super) event_id: EventId,
    pub(super) snapshot_revision: u64,
}

pub(super) const NVIM_PROCESS_PID_OPTION: &str = "@vde_nvim_process_pid";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NvimPaneMarker {
    pub(super) pane_id: String,
    pub(super) pane_pid: u32,
    pub(super) process_pid: u32,
}

pub(super) fn parse_nvim_pane_markers(output: &str) -> Vec<NvimPaneMarker> {
    let mut markers = BTreeMap::new();
    for line in output.lines() {
        let mut fields = line.split('\u{1f}');
        let (Some(pane_id), Some(pane_pid), Some(process_pid), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(pane_pid) = pane_pid.parse::<u32>().ok().filter(|pid| *pid > 0) else {
            continue;
        };
        let Some(process_pid) = process_pid.parse::<u32>().ok().filter(|pid| *pid > 0) else {
            continue;
        };
        if crate::daemon::topology::validate_pane_id(pane_id).is_err() {
            continue;
        }
        let marker = NvimPaneMarker {
            pane_id: pane_id.to_string(),
            pane_pid,
            process_pid,
        };
        markers.entry(marker.pane_id.clone()).or_insert(marker);
    }
    markers.into_values().collect()
}

pub(super) fn stale_nvim_marker_cleanup_command(
    expected_server_pid: u32,
    markers: &[NvimPaneMarker],
) -> Option<String> {
    let mut arguments = Vec::new();
    for marker in markers {
        if !arguments.is_empty() {
            arguments.push(";".to_string());
        }
        let guard = format!(
            "#{{&&:#{{==:#{{pid}},{expected_server_pid}}},#{{&&:#{{==:#{{pane_pid}},{}}},#{{==:#{{{NVIM_PROCESS_PID_OPTION}}},{}}}}}}}",
            marker.pane_pid, marker.process_pid
        );
        let unset = crate::pane_state::store::tmux_command_string(&[
            "set-option".to_string(),
            "-pu".to_string(),
            "-t".to_string(),
            marker.pane_id.clone(),
            NVIM_PROCESS_PID_OPTION.to_string(),
        ]);
        arguments.extend([
            "if-shell".to_string(),
            "-F".to_string(),
            "-t".to_string(),
            marker.pane_id.clone(),
            guard,
            unset,
        ]);
    }
    (!arguments.is_empty()).then(|| crate::pane_state::store::tmux_command_string(&arguments))
}

pub(super) fn enqueue_sidebar_tmux_job(
    tx: &SyncSender<SidebarTmuxJob>,
    deferred_responses: &Mutex<BTreeSet<u64>>,
    job: SidebarTmuxJob,
) -> std::result::Result<(), ErrorCode> {
    let original_accepted_seq = job.original_accepted_seq;
    tx.try_send(job).map_err(|error| match error {
        TrySendError::Full(_) => ErrorCode::QueueFull,
        TrySendError::Disconnected(_) => ErrorCode::InternalError,
    })?;
    deferred_responses
        .lock()
        .expect("deferred response lock poisoned")
        .insert(original_accepted_seq);
    Ok(())
}

#[cfg(test)]
pub(super) fn start_notification_worker(command: String) -> SyncSender<NotificationWorkerJob> {
    start_notification_worker_with_timeout_and_log(command, Duration::from_secs(2), None)
}

pub(super) fn start_sidebar_tmux_worker(
    env: &BTreeMap<String, String>,
    expected_server: crate::daemon::topology::ServerIdentity,
    witness_observation_seq: Arc<AtomicU64>,
) -> (
    SyncSender<SidebarTmuxJob>,
    mpsc::Receiver<SidebarEffectCompletion>,
) {
    let (tx, rx) = mpsc::sync_channel::<SidebarTmuxJob>(64);
    let (completion_tx, completion_rx) = mpsc::channel::<SidebarEffectCompletion>();
    let socket_name = env
        .get("VDE_TMUX_SOCKET_NAME")
        .cloned()
        .filter(|value| !value.trim().is_empty());
    thread::spawn(move || {
        use crate::daemon::workers::WorkerIo as _;

        while let Ok(job) = rx.recv() {
            let io = crate::daemon::workers::SystemWorkerIo::new(
                socket_name.clone(),
                expected_server.clone(),
            );
            let (candidates, client_pid, source_pane) = match &job.effect {
                crate::daemon::runtime::CanonicalSidebarEffect::JumpPane {
                    pane_instance,
                    client_pid,
                    source_pane,
                } => (
                    vec![pane_instance.clone()],
                    *client_pid,
                    source_pane.clone(),
                ),
                crate::daemon::runtime::CanonicalSidebarEffect::JumpLatestUnread {
                    candidates,
                    client_pid,
                    source_pane,
                }
                | crate::daemon::runtime::CanonicalSidebarEffect::ReadPeekAdvance {
                    candidates,
                    client_pid,
                    source_pane,
                    ..
                } => (candidates.clone(), *client_pid, source_pane.clone()),
                crate::daemon::runtime::CanonicalSidebarEffect::PeekPane {
                    pane_instance,
                    client_pid,
                    source_pane,
                } => (
                    vec![pane_instance.clone()],
                    *client_pid,
                    source_pane.clone(),
                ),
            };
            let result = io.jump_to_first_available_pane(&candidates, client_pid, &source_pane);
            let result = match result {
                Ok(pane_instance) => SidebarEffectResult::Succeeded(pane_instance),
                Err(crate::daemon::workers::SidebarTmuxError::ServerIncarnationMismatch) => {
                    SidebarEffectResult::ServerIncarnationMismatch
                }
                Err(crate::daemon::workers::SidebarTmuxError::PaneInstanceMismatch(_)) => {
                    SidebarEffectResult::PaneInstanceMismatch
                }
                Err(crate::daemon::workers::SidebarTmuxError::NoAvailablePane) => {
                    SidebarEffectResult::NoAvailablePane
                }
                Err(crate::daemon::workers::SidebarTmuxError::SourceClientMismatch) => {
                    SidebarEffectResult::SourceClientMismatch
                }
                Err(error) => SidebarEffectResult::Failed(error.to_string()),
            };
            let witness_observation_floor = witness_observation_seq.load(Ordering::SeqCst);
            let _ = completion_tx.send(SidebarEffectCompletion {
                original_accepted_seq: job.original_accepted_seq,
                event_id: job.event_id,
                snapshot_revision: job.snapshot_revision,
                witness_observation_floor,
                result,
                effect: job.effect,
            });
        }
    });
    (tx, completion_rx)
}

#[cfg(test)]
pub(super) fn start_notification_worker_with_timeout_and_log(
    command: String,
    timeout: Duration,
    log_context: Option<(std::collections::BTreeMap<String, String>, String)>,
) -> SyncSender<NotificationWorkerJob> {
    start_notification_worker_with_control(
        command,
        timeout,
        log_context,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(())),
    )
}

pub(super) fn start_notification_worker_with_control(
    command: String,
    timeout: Duration,
    log_context: Option<(std::collections::BTreeMap<String, String>, String)>,
    shutdown: Arc<AtomicBool>,
    process_lock: Arc<Mutex<()>>,
) -> SyncSender<NotificationWorkerJob> {
    let (sender, receiver) = mpsc::sync_channel::<NotificationWorkerJob>(64);
    thread::spawn(move || {
        while let Ok(job) = receiver.recv() {
            let process_guard = process_lock
                .lock()
                .expect("notification process lock poisoned");
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let mut process = Command::new("/bin/sh");
            process
                .arg("-c")
                .arg(&command)
                .env("VDE_PANE_ID", &job.pane_id)
                .env("VDE_AGENT", &job.agent)
                .env("VDE_BADGE_STATE", "Blocked")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            unsafe {
                process.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = process.spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(error) => {
                    log_notification_failure(
                        log_context.as_ref(),
                        &format!(
                            "notification command spawn failed for pane {}: {error}",
                            job.pane_id
                        ),
                    );
                    continue;
                }
            };
            let notification_identity =
                match crate::daemon::lifecycle::process_start_token(child.id()) {
                    Ok(start_token) => crate::daemon::lifecycle::NotificationProcessIdentity {
                        process_group_id: child.id() as i32,
                        leader_start_token: start_token,
                    },
                    Err(error) => {
                        terminate_notification_process_group(&mut child);
                        log_notification_failure(
                            log_context.as_ref(),
                            &format!(
                                "notification process identity failed for pane {}: {error}",
                                job.pane_id
                            ),
                        );
                        continue;
                    }
                };
            if let Err(error) = record_active_notification(
                log_context.as_ref(),
                Some(notification_identity.clone()),
            ) {
                terminate_notification_process_group(&mut child);
                log_notification_failure(
                    log_context.as_ref(),
                    &format!(
                        "notification process identity persistence failed for pane {}: {error}",
                        job.pane_id
                    ),
                );
                continue;
            }
            drop(process_guard);
            let deadline = Instant::now() + timeout;
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    terminate_notification_process_group(&mut child);
                    break;
                }
                match try_wait_notification_process_group(&mut child) {
                    Ok(Some(status)) => {
                        if !status.success() {
                            log_notification_failure(
                                log_context.as_ref(),
                                &format!(
                                    "notification command exited with status {status} for pane {}",
                                    job.pane_id
                                ),
                            );
                        }
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        terminate_notification_process_group(&mut child);
                        log_notification_failure(
                            log_context.as_ref(),
                            &format!(
                                "notification command timed out after {timeout:?} for pane {}",
                                job.pane_id
                            ),
                        );
                        break;
                    }
                    Err(error) => {
                        terminate_notification_process_group(&mut child);
                        log_notification_failure(
                            log_context.as_ref(),
                            &format!(
                                "notification command wait failed for pane {}: {error}",
                                job.pane_id
                            ),
                        );
                        break;
                    }
                }
            }
            clear_active_notification(log_context.as_ref(), &notification_identity);
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
        }
    });
    sender
}

pub(super) fn record_active_notification(
    context: Option<&(std::collections::BTreeMap<String, String>, String)>,
    identity: Option<crate::daemon::lifecycle::NotificationProcessIdentity>,
) -> Result<()> {
    let Some((env, incarnation_hash)) = context else {
        return Ok(());
    };
    crate::daemon::lifecycle::update_lifecycle_record(env, incarnation_hash, |record| {
        record.active_notification = identity;
        Ok(())
    })
}

pub(super) fn clear_active_notification(
    context: Option<&(std::collections::BTreeMap<String, String>, String)>,
    identity: &crate::daemon::lifecycle::NotificationProcessIdentity,
) {
    let Some((env, incarnation_hash)) = context else {
        return;
    };
    let _ = crate::daemon::lifecycle::update_lifecycle_record(env, incarnation_hash, |record| {
        if record.active_notification.as_ref() == Some(identity) {
            record.active_notification = None;
        }
        Ok(())
    });
}

pub(super) fn log_notification_failure(
    context: Option<&(std::collections::BTreeMap<String, String>, String)>,
    message: &str,
) {
    let Some((env, incarnation_hash)) = context else {
        eprintln!("[vde-tmux] {message}");
        return;
    };
    if crate::daemon::lifecycle::append_daemon_log(
        env,
        incarnation_hash,
        &format!("notification: {message}"),
    )
    .is_err()
    {
        eprintln!("[vde-tmux] {message}");
    }
}

pub(super) fn terminate_notification_process_group(child: &mut std::process::Child) {
    let process_group = -(child.id() as i32);
    let _ = unsafe { libc::kill(process_group, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn try_wait_notification_process_group(
    child: &mut std::process::Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    child.wait().map(Some)
}

pub(super) fn start_sidebar_completion_forwarder(coordinator: Arc<ProductionV2Coordinator>) {
    let receiver = coordinator
        .sidebar_completion_rx
        .lock()
        .expect("sidebar completion receiver lock poisoned")
        .take()
        .expect("sidebar completion forwarder started once");
    thread::spawn(move || {
        while let Ok(completion) = receiver.recv() {
            if coordinator.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !coordinator.enqueue_internal(V2InternalMutation::SidebarEffectCompleted(completion))
            {
                coordinator
                    .fail_stop("sidebar completion could not enter sequenced mutation queue");
                break;
            }
        }
    });
}

pub(super) fn start_task_summary_completion_forwarder(coordinator: Arc<ProductionV2Coordinator>) {
    let Some(receiver) = coordinator
        .task_summary_completion_rx
        .lock()
        .expect("task summary completion receiver lock poisoned")
        .take()
    else {
        return;
    };
    thread::spawn(move || {
        while let Ok(completion) = receiver.recv() {
            if coordinator.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !coordinator.enqueue_internal(V2InternalMutation::TaskSummaryCompleted(completion)) {
                coordinator.log_daemon_error(
                    "task summary completion could not enter sequenced mutation queue",
                );
            }
        }
    });
}

pub(super) fn start_agent_prompt_timeout_worker(coordinator: Arc<ProductionV2Coordinator>) {
    thread::spawn(move || {
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(250)
                && !coordinator.shutdown.load(Ordering::SeqCst)
            {
                thread::sleep(Duration::from_millis(25));
            }
            if coordinator.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let observed_at = epoch_seconds();
            let expired = coordinator
                .agent_runtime
                .lock()
                .expect("agent runtime lock poisoned")
                .as_ref()
                .is_some_and(|runtime| runtime.has_expired_dispatch(observed_at).unwrap_or(true));
            if !expired {
                continue;
            }
            if !coordinator
                .enqueue_internal(V2InternalMutation::AgentPromptTimeouts { observed_at })
            {
                break;
            }
        }
    });
}

pub(super) fn start_canonical_git_worker(
    coordinator: Arc<ProductionV2Coordinator>,
    poll: Duration,
    git_timeout: Duration,
) {
    thread::spawn(move || {
        let git = crate::daemon::workers::system_git_runner(git_timeout);
        let mut poller = crate::git::GitPoller::new();
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let paths = coordinator
                .state
                .lock()
                .expect("canonical state lock poisoned")
                .as_ref()
                .map(|state| state.git_polling_paths())
                .unwrap_or_default();
            let (badges, worktrees, repo_identities) =
                poller.poll_with_identities(&git, paths.iter().map(String::as_str), Instant::now());
            let _ = coordinator.enqueue_internal(V2InternalMutation::GitProjection {
                badges,
                worktrees,
                repo_identities,
            });
            let started = Instant::now();
            while started.elapsed() < poll && !coordinator.shutdown.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100).min(poll));
            }
        }
    });
}

pub(super) fn start_status_push_worker(coordinator: Arc<ProductionV2Coordinator>) {
    thread::spawn(move || {
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            for trigger in [StatusPushTrigger::Snapshot, StatusPushTrigger::Flush] {
                if let Err(error) = coordinator.drive_status_push(trigger) {
                    if coordinator.shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    coordinator.log_status_push_error(&format!(
                        "status display projection failed: {error:#}"
                    ));
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

pub(super) fn start_tmux_server_liveness_monitor(
    coordinator: Arc<ProductionV2Coordinator>,
) -> Result<()> {
    let server_pid = coordinator.incarnation.identity.pid;
    let expected_start_token = crate::daemon::lifecycle::process_start_token(server_pid)
        .with_context(|| {
            format!("failed to identify tmux server process before starting liveness monitor: {server_pid}")
        })?;
    thread::spawn(move || {
        while !coordinator.shutdown.load(Ordering::SeqCst) {
            let started = Instant::now();
            while started.elapsed() < TMUX_SERVER_LIVENESS_POLL_INTERVAL
                && !coordinator.shutdown.load(Ordering::SeqCst)
            {
                thread::sleep(
                    Duration::from_millis(50)
                        .min(TMUX_SERVER_LIVENESS_POLL_INTERVAL.saturating_sub(started.elapsed())),
                );
            }
            if coordinator.shutdown.load(Ordering::SeqCst) {
                break;
            }
            if !crate::daemon::lifecycle::process_start_token(server_pid)
                .is_ok_and(|actual| actual == expected_start_token)
            {
                coordinator.fail_stop(format!("tmux server process exited: pid={server_pid}"));
                break;
            }
        }
    });
    Ok(())
}

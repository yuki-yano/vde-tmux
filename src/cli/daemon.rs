use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::tmux::TmuxRunner;

pub(crate) fn statusline_summary(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<String> {
    Ok(status_segments(
        runner,
        env,
        config,
        crate::daemon::protocol::v2::StatusContext::Global,
    )?
    .summary)
}

pub(crate) fn statusline_attention(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
    session_id: &str,
) -> Result<String> {
    Ok(statusline_session_segments(runner, env, config, session_id)?.attention)
}

pub(crate) fn statusline_attention_target(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    session_id: &str,
    target: &str,
) -> Result<crate::pane_state::PaneInstance> {
    let snapshot = status_snapshot(
        runner,
        env,
        crate::daemon::protocol::v2::StatusContext::Session {
            session_id: session_id.to_string(),
        },
    )?;
    crate::statusline::resolve_attention_target(&snapshot.attention, target)
}

pub(crate) fn statusline_session_segments(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
    session_id: &str,
) -> Result<crate::statusline::StructuredStatusSegments> {
    status_segments(
        runner,
        env,
        config,
        crate::daemon::protocol::v2::StatusContext::Session {
            session_id: session_id.to_string(),
        },
    )
}

pub(crate) fn statusline_pane(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
    pane_id: &str,
) -> Result<String> {
    let (incarnation, socket) =
        crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, None)?;
    let mut client = crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        &socket,
        &incarnation.hash,
        Duration::from_secs(2),
    )?;
    match client.request(&crate::daemon::protocol::v2::ClientMessage::QueryPane {
        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
        pane_id: pane_id.to_string(),
    })? {
        crate::daemon::protocol::v2::ServerMessage::PaneResult { pane, .. } => Ok(
            crate::statusline::render_structured_pane_status(config, &pane),
        ),
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon query failed ({code:?}): {message}")
        }
        other => bail!("unexpected daemon pane response: {other:?}"),
    }
}

fn status_segments(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
    context: crate::daemon::protocol::v2::StatusContext,
) -> Result<crate::statusline::StructuredStatusSegments> {
    let snapshot = status_snapshot(runner, env, context)?;
    crate::statusline::render_structured_status_snapshot(config, &snapshot)
}

fn status_snapshot(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    context: crate::daemon::protocol::v2::StatusContext,
) -> Result<crate::daemon::protocol::v2::StatusSnapshot> {
    let (incarnation, socket) =
        crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, None)?;
    let mut client = crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        &socket,
        &incarnation.hash,
        Duration::from_secs(2),
    )?;
    match client.request(
        &crate::daemon::protocol::v2::ClientMessage::QueryStatusSnapshot {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            context,
        },
    )? {
        crate::daemon::protocol::v2::ServerMessage::StatusSnapshotResult { snapshot, .. } => {
            Ok(snapshot)
        }
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon query failed ({code:?}): {message}")
        }
        other => bail!("unexpected daemon status response: {other:?}"),
    }
}

pub(crate) fn run_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
    expected_server_identity: Option<&str>,
    expected_server_pid: Option<u32>,
    expected_server_start_time: Option<i64>,
    expected_tmux_server_socket: Option<&str>,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let explicit_identity = (
        expected_server_identity,
        expected_server_pid,
        expected_server_start_time,
        expected_tmux_server_socket,
    );
    if explicit_identity.0.is_some()
        || explicit_identity.1.is_some()
        || explicit_identity.2.is_some()
        || explicit_identity.3.is_some()
    {
        let (Some(hash), Some(pid), Some(start_time), Some(tmux_socket)) = explicit_identity else {
            bail!("incomplete explicit tmux server incarnation");
        };
        let canonical_socket = std::fs::canonicalize(tmux_socket)?;
        if hash != incarnation.hash
            || pid != incarnation.identity.pid
            || start_time != incarnation.identity.start_time
            || canonical_socket != incarnation.socket_path
        {
            bail!("explicit tmux server incarnation does not match the target server");
        }
    }
    let socket_path = if expected_server_identity.is_some() {
        Path::new(socket.ok_or_else(|| anyhow::anyhow!("spawned daemon requires --socket"))?)
            .to_path_buf()
    } else {
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash)
    };
    let config = match crate::config::load::load_config_strict(env) {
        Ok(config) => config,
        Err(error) => {
            let message = format!("strict config validation failed before daemon startup: {error}");
            let _ = crate::daemon::lifecycle::append_daemon_log(env, &incarnation.hash, &message);
            let _ = crate::daemon::lifecycle::update_lifecycle_record(
                env,
                &incarnation.hash,
                |record| {
                    record.process = None;
                    record.degrade(&message);
                    Ok(())
                },
            );
            bail!(message);
        }
    };
    if let Err(error) = crate::daemon::server::run_runtime_daemon_server(
        config,
        &socket_path,
        env,
        incarnation.clone(),
    ) {
        let _ =
            crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
                record.process = None;
                record.degrade(format!("daemon runtime exited with error: {error:#}"));
                Ok(())
            });
        let _ = crate::daemon::lifecycle::append_daemon_log(
            env,
            &incarnation.hash,
            &format!("daemon runtime exited with error: {error:#}"),
        );
        return Err(error);
    }
    let _ = crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
        record.process = None;
        if record.health == crate::daemon::lifecycle::LifecycleHealth::Stable {
            record.last_transition_error = None;
        }
        Ok(())
    });
    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReachServingCommand {
    Ensure,
    Start,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonCommandState {
    Serving,
    Stopped,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReachServingAction {
    ReportServing,
    Start,
    DisabledNoop,
    DisabledError,
}

fn reach_serving_action(
    state: DaemonCommandState,
    command: ReachServingCommand,
) -> ReachServingAction {
    match (state, command) {
        (DaemonCommandState::Serving, _) => ReachServingAction::ReportServing,
        (DaemonCommandState::Stopped, _) => ReachServingAction::Start,
        (DaemonCommandState::Disabled, ReachServingCommand::Ensure) => {
            ReachServingAction::DisabledNoop
        }
        (DaemonCommandState::Disabled, ReachServingCommand::Start) => {
            ReachServingAction::DisabledError
        }
    }
}

fn daemon_command_state(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    socket_path: &Path,
) -> Result<DaemonCommandState> {
    if crate::daemon::lifecycle::tmux_desired_mode(runner, env)?
        == crate::daemon::lifecycle::DesiredMode::Disabled
        || crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)?.desired_mode
            == crate::daemon::lifecycle::DesiredMode::Disabled
    {
        return Ok(DaemonCommandState::Disabled);
    }
    Ok(
        if crate::daemon::lifecycle::probe_v2_daemon(socket_path, &incarnation.hash)
            == Some(crate::daemon::protocol::v2::DaemonPhase::Serving)
        {
            DaemonCommandState::Serving
        } else {
            DaemonCommandState::Stopped
        },
    )
}

pub(crate) fn ensure_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let socket_path =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    match reach_serving_action(
        daemon_command_state(runner, env, &incarnation, &socket_path)?,
        ReachServingCommand::Ensure,
    ) {
        ReachServingAction::ReportServing => {
            return Ok(Some(format!("daemon serving: {}", socket_path.display())));
        }
        ReachServingAction::DisabledNoop => {
            return Ok(Some("daemon disabled; ensure made no changes".to_string()));
        }
        ReachServingAction::Start => {}
        ReachServingAction::DisabledError => unreachable!("ensure never rejects disabled state"),
    }
    let (_, socket_path) = crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, socket)?;
    Ok(Some(format!("daemon serving: {}", socket_path.display())))
}

pub(crate) fn start_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let socket_path =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    match reach_serving_action(
        daemon_command_state(runner, env, &incarnation, &socket_path)?,
        ReachServingCommand::Start,
    ) {
        ReachServingAction::ReportServing => {
            return Ok(Some(format!("daemon serving: {}", socket_path.display())));
        }
        ReachServingAction::DisabledError => {
            bail!("daemon is disabled; run `vt daemon enable` first");
        }
        ReachServingAction::Start => {}
        ReachServingAction::DisabledNoop => unreachable!("start never ignores disabled state"),
    }
    crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
        record.begin_transition(crate::daemon::lifecycle::DesiredMode::Enabled)
    })?;
    match crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, socket) {
        Ok((_, socket_path)) => Ok(Some(format!("daemon serving: {}", socket_path.display()))),
        Err(error) => {
            record_transition_failure(env, &incarnation.hash, &error);
            Err(error)
        }
    }
}

pub(crate) fn status_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let socket_path =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    let record = crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)?;
    let process = format_process_identity(record.process.as_ref());
    let last_transition_error = record.last_transition_error.as_deref().unwrap_or("none");
    match crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        &socket_path,
        &incarnation.hash,
        Duration::from_millis(250),
    ) {
        Ok(mut client) => {
            let phase = client.phase();
            let hooks = client.hook_health();
            let daemon_instance = client.daemon_instance_id().as_str().to_string();
            let config_hash = match client.request(
                &crate::daemon::protocol::v2::ClientMessage::QueryRuntimeInfo {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                },
            ) {
                Ok(crate::daemon::protocol::v2::ServerMessage::RuntimeInfoResult { info }) => {
                    info.config_hash
                }
                Ok(_) | Err(_) => "unavailable".to_string(),
            };
            Ok(Some(format!(
                "daemon: running\nphase: {phase:?}\nhooks: {hooks:?}\ndaemon_instance: {daemon_instance}\nserver: {}\nsocket: {}\nprocess: {process}\nconfig_hash: {config_hash}\nlast_transition_error: {last_transition_error}",
                incarnation.hash,
                socket_path.display(),
            )))
        }
        Err(_) => Ok(Some(format!(
            "daemon: unavailable\nphase: unavailable\nhooks: unavailable\ndaemon_instance: unavailable\nserver: {}\nsocket: {}\nprocess: {process}\nconfig_hash: unavailable\nlast_transition_error: {last_transition_error}",
            incarnation.hash,
            socket_path.display(),
        ))),
    }
}

pub(crate) fn stop_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
    force: bool,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let socket_path =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
        record.begin_transition(record.desired_mode)
    })?;
    match request_shutdown(&incarnation, &socket_path) {
        Ok(false) if force => {
            let record = crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)?;
            if record.process.is_none() {
                return Ok(Some(format!(
                    "daemon is not running: {}",
                    socket_path.display()
                )));
            }
            force_stop_or_degrade(runner, env, &incarnation, &socket_path)?;
            clear_process_identity(env, &incarnation.hash);
            Ok(Some(format!(
                "daemon force-stopped: {}",
                socket_path.display()
            )))
        }
        Ok(false) => {
            let record = crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)?;
            if recorded_process_is_alive(record.process.as_ref()) {
                let error = anyhow::anyhow!(
                    "daemon is unresponsive but its recorded process is alive; run `vt daemon stop --force`"
                );
                record_transition_failure(env, &incarnation.hash, &error);
                return Err(error);
            }
            if let Some(process) = record.process.as_ref() {
                crate::daemon::lifecycle::remove_force_stopped_socket(&socket_path, process)?;
            }
            clear_process_identity(env, &incarnation.hash);
            Ok(Some(format!(
                "daemon is not running: {}",
                socket_path.display()
            )))
        }
        Ok(true) => {
            if wait_for_daemon_stop(env, &incarnation, &socket_path, Duration::from_secs(2)) {
                clear_process_identity(env, &incarnation.hash);
                return Ok(Some(format!("daemon stopped: {}", socket_path.display())));
            }
            if !force {
                let error = anyhow::anyhow!("daemon did not stop before the shutdown deadline");
                record_transition_failure(env, &incarnation.hash, &error);
                return Err(error);
            }
            force_stop_or_degrade(runner, env, &incarnation, &socket_path)?;
            clear_process_identity(env, &incarnation.hash);
            Ok(Some(format!(
                "daemon force-stopped: {}",
                socket_path.display()
            )))
        }
        Err(_error) if force => {
            force_stop_or_degrade(runner, env, &incarnation, &socket_path)?;
            clear_process_identity(env, &incarnation.hash);
            Ok(Some(format!(
                "daemon force-stopped after graceful shutdown failed: {}",
                socket_path.display()
            )))
        }
        Err(error) => {
            record_transition_failure(env, &incarnation.hash, &error);
            Err(error)
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DisabledTransitionOutcome {
    marker_disabled: bool,
    hooks_removed: bool,
    daemon_stopped: bool,
    failures: Vec<String>,
}

impl DisabledTransitionOutcome {
    fn is_complete(&self) -> bool {
        self.marker_disabled
            && self.hooks_removed
            && self.daemon_stopped
            && self.failures.is_empty()
    }
}

fn execute_disabled_transition(
    continue_after_marker_failure: bool,
    set_disabled_marker: impl FnOnce() -> Result<()>,
    remove_owned_hooks: impl FnOnce() -> Result<()>,
    shutdown_daemon: impl FnOnce() -> Result<()>,
) -> DisabledTransitionOutcome {
    let mut outcome = DisabledTransitionOutcome::default();
    if let Err(error) = set_disabled_marker() {
        outcome.failures.push(format!("{error:#}"));
        if !continue_after_marker_failure {
            return outcome;
        }
    } else {
        outcome.marker_disabled = true;
    }
    if let Err(error) = remove_owned_hooks() {
        outcome.failures.push(format!("{error:#}"));
    } else {
        outcome.hooks_removed = true;
    }
    if let Err(error) = shutdown_daemon() {
        outcome.failures.push(format!("{error:#}"));
    } else {
        outcome.daemon_stopped = true;
    }
    outcome
}

pub(crate) fn disable_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    disable_daemon_for_incarnation(runner, env, socket, &incarnation)
}

pub(crate) fn disable_daemon_for_server(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
) -> Result<()> {
    disable_daemon_for_incarnation(runner, env, None, incarnation).map(|_| ())
}

fn disable_daemon_for_incarnation(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
) -> Result<Option<String>> {
    let socket_path =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    let outcome = execute_disabled_transition(
        false,
        || {
            crate::daemon::lifecycle::set_tmux_desired_mode_for_incarnation(
                runner,
                incarnation,
                crate::daemon::lifecycle::DesiredMode::Disabled,
            )
            .context("failed to set disabled server marker")?;
            crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
                record.begin_transition(crate::daemon::lifecycle::DesiredMode::Disabled)
            })
            .context("failed to persist disabled lifecycle state")
        },
        || {
            crate::daemon::view_hooks::uninstall_hooks(runner, &incarnation.identity)
                .map_err(anyhow::Error::new)
                .context("owned hook removal failed")
        },
        || shutdown_daemon_for_disabled_transition(runner, env, incarnation, &socket_path),
    );
    if outcome.is_complete() {
        return Ok(Some("daemon disabled".to_string()));
    }
    let error = anyhow::anyhow!(outcome.failures.join("; "));
    let _ = crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
        if outcome.marker_disabled {
            record.desired_mode = crate::daemon::lifecycle::DesiredMode::Disabled;
        }
        record.degrade(format!("{error:#}"));
        Ok(())
    });
    record_transition_failure(env, &incarnation.hash, &error);
    Err(error)
}

fn finish_enable_transition(
    set_enabled_marker: impl FnOnce() -> Result<()>,
    persist_enabled_record: impl FnOnce() -> Result<()>,
    rollback: impl FnOnce(anyhow::Error) -> anyhow::Error,
) -> Result<()> {
    if let Err(error) = set_enabled_marker() {
        return Err(rollback(error));
    }
    if let Err(error) = persist_enabled_record() {
        return Err(rollback(error));
    }
    Ok(())
}

pub(crate) fn enable_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let socket_path =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
        record.begin_transition(crate::daemon::lifecycle::DesiredMode::Disabled)
    })?;
    if let Err(error) = crate::daemon::view_hooks::install_hooks(runner, &incarnation.identity) {
        let error = anyhow::anyhow!("failed to install owned hooks: {error}");
        return Err(rollback_failed_enable(
            runner,
            env,
            &incarnation,
            &socket_path,
            error,
        ));
    }
    match crate::daemon::lifecycle::start_daemon_serving_v2_while_disabled(runner, env, socket) {
        Ok((_, serving_socket_path)) => {
            finish_enable_transition(
                || {
                    crate::daemon::lifecycle::set_tmux_desired_mode(
                        runner,
                        env,
                        crate::daemon::lifecycle::DesiredMode::Enabled,
                    )
                    .context("failed to clear disabled server marker")
                },
                || {
                    crate::daemon::lifecycle::update_lifecycle_record(
                        env,
                        &incarnation.hash,
                        |record| {
                            record.desired_mode = crate::daemon::lifecycle::DesiredMode::Enabled;
                            record.health = crate::daemon::lifecycle::LifecycleHealth::Stable;
                            record.last_transition_error = None;
                            Ok(())
                        },
                    )
                    .context("failed to persist enabled lifecycle state")
                },
                |error| rollback_failed_enable(runner, env, &incarnation, &socket_path, error),
            )?;
            Ok(Some(format!(
                "daemon enabled: {}",
                serving_socket_path.display()
            )))
        }
        Err(error) => Err(rollback_failed_enable(
            runner,
            env,
            &incarnation,
            &socket_path,
            error,
        )),
    }
}

fn rollback_failed_enable(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    socket_path: &Path,
    enable_error: anyhow::Error,
) -> anyhow::Error {
    let mut outcome = execute_disabled_transition(
        true,
        || {
            crate::daemon::lifecycle::set_tmux_desired_mode(
                runner,
                env,
                crate::daemon::lifecycle::DesiredMode::Disabled,
            )
            .context("rollback failed to restore disabled server marker")
        },
        || {
            crate::daemon::view_hooks::uninstall_hooks(runner, &incarnation.identity)
                .map_err(anyhow::Error::new)
                .context("rollback failed to remove owned hooks")
        },
        || shutdown_daemon_for_disabled_transition(runner, env, incarnation, socket_path),
    );
    let rollback_complete = outcome.is_complete();
    let enable_message = format!("enable failed: {enable_error:#}");
    let diagnostic = if rollback_complete {
        format!("{enable_message}; rollback restored disabled state")
    } else {
        format!(
            "{enable_message}; rollback incomplete: {}",
            outcome.failures.join("; ")
        )
    };
    if let Err(error) =
        crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
            apply_enable_rollback_record(record, &diagnostic, rollback_complete);
            Ok(())
        })
    {
        outcome.failures.push(format!(
            "rollback failed to persist disabled lifecycle state: {error:#}"
        ));
    }
    let final_message = if outcome.is_complete() {
        diagnostic
    } else {
        format!(
            "{enable_message}; rollback incomplete: {}",
            outcome.failures.join("; ")
        )
    };
    let _ = crate::daemon::lifecycle::append_daemon_log(env, &incarnation.hash, &final_message);
    anyhow::anyhow!(final_message)
}

fn apply_enable_rollback_record(
    record: &mut crate::daemon::lifecycle::LifecycleRecord,
    diagnostic: &str,
    rollback_complete: bool,
) {
    record.desired_mode = crate::daemon::lifecycle::DesiredMode::Disabled;
    record.degrade(diagnostic);
    if rollback_complete {
        record.process = None;
        record.health = crate::daemon::lifecycle::LifecycleHealth::Stable;
    }
}

pub(crate) fn restart_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    reload_daemon(runner, env, socket)
}

pub(crate) fn reload_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    crate::config::load::load_config_strict(env).map_err(|error| {
        anyhow::anyhow!("config validation failed; daemon left unchanged: {error}")
    })?;
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    if crate::daemon::lifecycle::tmux_desired_mode(runner, env)?
        == crate::daemon::lifecycle::DesiredMode::Disabled
        || crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)?.desired_mode
            == crate::daemon::lifecycle::DesiredMode::Disabled
    {
        bail!("daemon is disabled; reload did not change runtime state");
    }
    let socket_path =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    crate::daemon::lifecycle::update_lifecycle_record(env, &incarnation.hash, |record| {
        record.begin_transition(crate::daemon::lifecycle::DesiredMode::Enabled)
    })?;
    let shutdown = match request_shutdown(&incarnation, &socket_path) {
        Ok(shutdown) => shutdown,
        Err(error) => {
            record_transition_failure(env, &incarnation.hash, &error);
            return Err(error);
        }
    };
    match shutdown {
        true if !wait_for_daemon_stop(env, &incarnation, &socket_path, Duration::from_secs(2)) => {
            let error = anyhow::anyhow!(
                "daemon did not stop before reload deadline; run `vt daemon stop --force`"
            );
            record_transition_failure(env, &incarnation.hash, &error);
            return Err(error);
        }
        false
            if crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)?
                .process
                .is_some() =>
        {
            let error = anyhow::anyhow!(
                "daemon is unresponsive and its process identity remains; run `vt daemon stop --force`"
            );
            record_transition_failure(env, &incarnation.hash, &error);
            return Err(error);
        }
        _ => {}
    }
    clear_process_identity(env, &incarnation.hash);
    match crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, socket) {
        Ok((_, socket_path)) => Ok(Some(format!("daemon reloaded: {}", socket_path.display()))),
        Err(error) => {
            let log = crate::daemon::lifecycle::daemon_log_path(env, &incarnation.hash);
            let failure = anyhow::anyhow!(
                "reload startup failed; daemon remains stopped; see {}: {error:#}",
                log.display()
            );
            record_transition_failure(env, &incarnation.hash, &failure);
            Err(failure)
        }
    }
}

fn request_shutdown(
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    socket_path: &Path,
) -> Result<bool> {
    let mut client = match crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        socket_path,
        &incarnation.hash,
        Duration::from_secs(2),
    ) {
        Ok(client) => client,
        Err(error) if crate::daemon::protocol::v2::is_protocol_version_mismatch(&error) => {
            return Err(error.context(
                "incompatible daemon is already running; stop it with the previously installed binary before replacing or reloading this version",
            ));
        }
        Err(_) => return Ok(false),
    };
    let event_id = crate::pane_state::EventId::generate()?;
    match client.request(&crate::daemon::protocol::v2::ClientMessage::Shutdown {
        proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
        daemon_instance_id: client.daemon_instance_id().clone(),
        event_id: event_id.clone(),
    })? {
        crate::daemon::protocol::v2::ServerMessage::ShutdownAccepted {
            event_id: response_id,
            ..
        } if response_id == event_id => Ok(true),
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon returned {code:?}: {message}")
        }
        response => bail!("unexpected daemon shutdown response: {response:?}"),
    }
}

fn wait_for_daemon_stop(
    env: &BTreeMap<String, String>,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    socket_path: &Path,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let process = crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)
            .ok()
            .and_then(|record| record.process);
        let process_alive = recorded_process_is_alive(process.as_ref());
        if !process_alive && let Some(process) = process.as_ref() {
            let _ = crate::daemon::lifecycle::remove_force_stopped_socket(socket_path, process);
        }
        let socket_exists = std::fs::symlink_metadata(socket_path).is_ok();
        if !process_alive
            && !socket_exists
            && crate::daemon::lifecycle::probe_v2_daemon(socket_path, &incarnation.hash).is_none()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn shutdown_daemon_for_disabled_transition(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    socket_path: &Path,
) -> Result<()> {
    let shutdown = request_shutdown(incarnation, socket_path);
    if matches!(shutdown, Ok(true))
        && wait_for_daemon_stop(env, incarnation, socket_path, Duration::from_secs(2))
    {
        return clear_process_identity_checked(env, &incarnation.hash);
    }

    let record = crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)?;
    let Some(process) = record.process else {
        return match shutdown {
            Ok(false) => Ok(()),
            Ok(true) => bail!("daemon did not stop before the shutdown deadline"),
            Err(error) => Err(error).context("daemon shutdown failed"),
        };
    };
    if crate::daemon::lifecycle::process_identity_is_alive(&process) {
        force_stop_verified(runner, env, incarnation, socket_path)
            .context("verified force-stop after graceful shutdown failed")?;
    } else {
        crate::daemon::lifecycle::remove_force_stopped_socket(socket_path, &process)
            .context("failed to remove stopped daemon socket")?;
    }
    clear_process_identity_checked(env, &incarnation.hash)
}

fn recorded_process_is_alive(
    process: Option<&crate::daemon::lifecycle::DaemonProcessIdentity>,
) -> bool {
    process.is_some_and(crate::daemon::lifecycle::process_identity_is_alive)
}

fn force_stop_verified(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    socket_path: &Path,
) -> Result<()> {
    let record = crate::daemon::lifecycle::read_lifecycle_record(env, &incarnation.hash)?;
    let expected = record
        .process
        .ok_or_else(|| anyhow::anyhow!("refusing force-stop: process identity is unknown"))?;
    if let Ok(client) = crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        socket_path,
        &incarnation.hash,
        Duration::from_millis(250),
    ) && client.daemon_instance_id().as_str() != expected.daemon_instance_id
    {
        bail!("refusing force-stop: daemon instance identity changed");
    }
    incarnation.verify(runner, env)?;
    crate::daemon::lifecycle::verify_force_stop_identity(
        env,
        &incarnation.hash,
        socket_path,
        &expected,
    )?;
    if unsafe { libc::kill(expected.pid as i32, libc::SIGSTOP) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to suspend daemon PID {}", expected.pid));
    }
    if !crate::daemon::lifecycle::process_identity_is_alive(&expected) {
        let _ = unsafe { libc::kill(expected.pid as i32, libc::SIGCONT) };
        bail!("refusing force-stop: daemon process identity changed after suspension");
    }
    if let Err(error) =
        crate::daemon::lifecycle::terminate_active_notification(env, &incarnation.hash)
    {
        let _ = unsafe { libc::kill(expected.pid as i32, libc::SIGCONT) };
        return Err(error).context("failed to terminate active notification before force-stop");
    }
    if unsafe { libc::kill(expected.pid as i32, libc::SIGKILL) } != 0 {
        let _ = unsafe { libc::kill(expected.pid as i32, libc::SIGCONT) };
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to signal daemon PID {}", expected.pid));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while crate::daemon::lifecycle::process_identity_is_alive(&expected) {
        if Instant::now() >= deadline {
            bail!("force-stopped daemon PID {} did not exit", expected.pid);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    crate::daemon::lifecycle::remove_force_stopped_socket(socket_path, &expected)?;
    Ok(())
}

fn force_stop_or_degrade(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    incarnation: &crate::daemon::lifecycle::TmuxServerIncarnation,
    socket_path: &Path,
) -> Result<()> {
    force_stop_verified(runner, env, incarnation, socket_path).inspect_err(|error| {
        record_transition_failure(env, &incarnation.hash, error);
    })
}

fn clear_process_identity(env: &BTreeMap<String, String>, incarnation_hash: &str) {
    let _ = clear_process_identity_checked(env, incarnation_hash);
}

fn clear_process_identity_checked(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
) -> Result<()> {
    crate::daemon::lifecycle::update_lifecycle_record(env, incarnation_hash, |record| {
        record.process = None;
        record.health = crate::daemon::lifecycle::LifecycleHealth::Stable;
        record.last_transition_error = None;
        Ok(())
    })
}

fn record_transition_failure(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
    error: &anyhow::Error,
) {
    let message = format!("{error:#}");
    let _ = crate::daemon::lifecycle::update_lifecycle_record(env, incarnation_hash, |record| {
        record.degrade(&message);
        Ok(())
    });
    let _ = crate::daemon::lifecycle::append_daemon_log(
        env,
        incarnation_hash,
        &format!("lifecycle transition failed: {message}"),
    );
}

fn format_process_identity(
    process: Option<&crate::daemon::lifecycle::DaemonProcessIdentity>,
) -> String {
    process.map_or_else(
        || "none".to_string(),
        |process| {
            format!(
                "pid={} start={} instance={} socket={}:{}",
                process.pid,
                process.start_token,
                process.daemon_instance_id,
                process.socket_device,
                process.socket_inode
            )
        },
    )
}

pub(crate) fn config_schema() -> Result<Option<String>> {
    Ok(Some(serde_json::to_string_pretty(
        &crate::config::schema::config_schema(),
    )?))
}

#[cfg(test)]
mod lifecycle_command_tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    #[test]
    fn reload_shutdown_probe_rejects_an_incompatible_daemon() {
        use std::io::{BufRead as _, Write as _};

        let root = std::path::PathBuf::from(format!(
            "/tmp/vt-rp-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate()
                .unwrap()
                .as_str()
                .chars()
                .take(8)
                .collect::<String>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let hello: crate::daemon::protocol::v2::ClientMessage =
                serde_json::from_str(request.trim()).unwrap();
            assert_eq!(
                hello,
                crate::daemon::protocol::v2::ClientMessage::Hello {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION
                }
            );
            serde_json::to_writer(
                &mut stream,
                &crate::daemon::protocol::v2::ServerMessage::error(
                    crate::daemon::protocol::v2::ErrorCode::UnsupportedProtocol,
                    "protocol version 2 is required",
                    None,
                ),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation {
            socket_path: root.join("tmux.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 10,
                start_time: 20,
            },
            hash: "server".to_string(),
        };

        let error = super::request_shutdown(&incarnation, &socket).unwrap_err();

        assert!(
            error.to_string().contains("incompatible daemon"),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("previously installed binary"),
            "{error:#}"
        );
        assert!(crate::daemon::protocol::v2::is_protocol_version_mismatch(
            &error
        ));
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn disabled_command_fixture(
        label: &str,
    ) -> (
        std::path::PathBuf,
        UnixListener,
        BTreeMap<String, String>,
        crate::tmux::mock::MockTmuxRunner,
    ) {
        let root =
            std::path::PathBuf::from(format!("/tmp/vt-command-{label}-{:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let tmux_socket = root.join("tmux.sock");
        let listener = UnixListener::bind(&tmux_socket).unwrap();
        let env = BTreeMap::from([
            (
                "TMUX".to_string(),
                format!("{},123,0", tmux_socket.display()),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                root.join("state").display().to_string(),
            ),
            (
                "XDG_RUNTIME_DIR".to_string(),
                root.join("runtime").display().to_string(),
            ),
        ]);
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("123\t456\t{}\n", tmux_socket.display()),
        );
        runner.stub(
            &[
                "show-option",
                "-gqv",
                crate::daemon::lifecycle::DISABLED_SERVER_OPTION,
            ],
            "1\n",
        );
        (root, listener, env, runner)
    }

    #[test]
    fn ensure_and_start_actions_cover_serving_stopped_and_disabled_states() {
        use super::{DaemonCommandState, ReachServingAction, ReachServingCommand};

        assert_eq!(
            super::reach_serving_action(DaemonCommandState::Serving, ReachServingCommand::Ensure),
            ReachServingAction::ReportServing
        );
        assert_eq!(
            super::reach_serving_action(DaemonCommandState::Stopped, ReachServingCommand::Ensure),
            ReachServingAction::Start
        );
        assert_eq!(
            super::reach_serving_action(DaemonCommandState::Disabled, ReachServingCommand::Ensure),
            ReachServingAction::DisabledNoop
        );
        assert_eq!(
            super::reach_serving_action(DaemonCommandState::Serving, ReachServingCommand::Start),
            ReachServingAction::ReportServing
        );
        assert_eq!(
            super::reach_serving_action(DaemonCommandState::Stopped, ReachServingCommand::Start),
            ReachServingAction::Start
        );
        assert_eq!(
            super::reach_serving_action(DaemonCommandState::Disabled, ReachServingCommand::Start),
            ReachServingAction::DisabledError
        );
    }

    #[test]
    fn ensure_disabled_is_a_function_level_noop() {
        let (root, listener, env, runner) = disabled_command_fixture("ensure-disabled");

        let output = super::ensure_daemon(&runner, &env, None).unwrap();

        assert_eq!(
            output.as_deref(),
            Some("daemon disabled; ensure made no changes")
        );
        assert!(
            runner
                .calls()
                .iter()
                .all(|call| !call.iter().any(|arg| arg == "set-option"))
        );
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn start_disabled_returns_a_function_level_error() {
        let (root, listener, env, runner) = disabled_command_fixture("start-disabled");

        let error = super::start_daemon(&runner, &env, None).unwrap_err();

        assert!(error.to_string().contains("daemon is disabled"));
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disable_transition_orders_marker_hooks_then_shutdown() {
        let calls = RefCell::new(Vec::new());

        let outcome = super::execute_disabled_transition(
            false,
            || {
                calls.borrow_mut().push("marker");
                Ok(())
            },
            || {
                calls.borrow_mut().push("hooks");
                Ok(())
            },
            || {
                calls.borrow_mut().push("shutdown");
                Ok(())
            },
        );

        assert!(outcome.is_complete());
        assert_eq!(*calls.borrow(), ["marker", "hooks", "shutdown"]);
    }

    #[test]
    fn enable_marker_failure_runs_complete_disabled_rollback() {
        let calls = RefCell::new(Vec::new());

        let error = super::finish_enable_transition(
            || {
                calls.borrow_mut().push("enable-marker");
                anyhow::bail!("marker write failed")
            },
            || -> anyhow::Result<()> {
                panic!("enabled record must not be persisted after marker failure")
            },
            |error| {
                calls.borrow_mut().push("rollback");
                let outcome = super::execute_disabled_transition(
                    true,
                    || {
                        calls.borrow_mut().push("disabled-marker");
                        Ok(())
                    },
                    || {
                        calls.borrow_mut().push("remove-hooks");
                        Ok(())
                    },
                    || {
                        calls.borrow_mut().push("shutdown");
                        Ok(())
                    },
                );
                assert!(outcome.is_complete());
                anyhow::anyhow!("{error:#}; rollback restored disabled state")
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("rollback restored disabled state")
        );
        assert_eq!(
            *calls.borrow(),
            [
                "enable-marker",
                "rollback",
                "disabled-marker",
                "remove-hooks",
                "shutdown"
            ]
        );
    }

    #[test]
    fn enable_rollback_record_is_stable_when_complete_and_degraded_when_incomplete() {
        let mut complete = crate::daemon::lifecycle::LifecycleRecord::initial("complete");
        super::apply_enable_rollback_record(
            &mut complete,
            "enable failed; rollback restored disabled state",
            true,
        );
        assert_eq!(
            complete.desired_mode,
            crate::daemon::lifecycle::DesiredMode::Disabled
        );
        assert_eq!(
            complete.health,
            crate::daemon::lifecycle::LifecycleHealth::Stable
        );
        assert_eq!(
            complete.last_transition_error.as_deref(),
            Some("enable failed; rollback restored disabled state")
        );

        let mut incomplete = crate::daemon::lifecycle::LifecycleRecord::initial("incomplete");
        super::apply_enable_rollback_record(
            &mut incomplete,
            "enable failed; rollback incomplete: shutdown failed",
            false,
        );
        assert_eq!(
            incomplete.desired_mode,
            crate::daemon::lifecycle::DesiredMode::Disabled
        );
        assert_eq!(
            incomplete.health,
            crate::daemon::lifecycle::LifecycleHealth::Degraded
        );
        assert_eq!(
            incomplete.last_transition_error.as_deref(),
            Some("enable failed; rollback incomplete: shutdown failed")
        );
    }

    #[test]
    fn force_stop_kills_hung_daemon_after_record_socket_and_process_revalidation() {
        let root = std::path::PathBuf::from(format!("/tmp/vtf-a-{:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let tmux_socket = root.join("tmux.sock");
        let tmux_listener = UnixListener::bind(&tmux_socket).unwrap();
        let socket = root.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let metadata = std::fs::metadata(&socket).unwrap();
        let process = crate::daemon::lifecycle::DaemonProcessIdentity {
            pid: child.id(),
            start_token: crate::daemon::lifecycle::process_start_token(child.id()).unwrap(),
            daemon_instance_id: "00112233445566778899aabbccddeeff".to_string(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
        };
        let env = BTreeMap::from([
            ("XDG_STATE_HOME".to_string(), root.display().to_string()),
            (
                "TMUX".to_string(),
                format!("{},123,0", tmux_socket.display()),
            ),
        ]);
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("123\t456\t{}\n", tmux_socket.display()),
        );
        let incarnation =
            crate::daemon::lifecycle::TmuxServerIncarnation::resolve(&runner, &env).unwrap();
        crate::daemon::lifecycle::update_lifecycle_record(&env, &incarnation.hash, |record| {
            record.process = Some(process);
            Ok(())
        })
        .unwrap();

        super::force_stop_verified(&runner, &env, &incarnation, &socket).unwrap();

        assert!(!child.wait().unwrap().success());
        assert!(
            !socket.exists(),
            "verified stale socket must be removed after SIGKILL"
        );
        drop(listener);
        drop(tmux_listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn force_stop_refuses_signal_after_tmux_server_incarnation_swap() {
        let root = std::path::PathBuf::from(format!("/tmp/vtf-b-{:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let tmux_socket = root.join("tmux.sock");
        let tmux_listener = UnixListener::bind(&tmux_socket).unwrap();
        let socket = root.join("daemon.sock");
        let daemon_listener = UnixListener::bind(&socket).unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let metadata = std::fs::metadata(&socket).unwrap();
        let process = crate::daemon::lifecycle::DaemonProcessIdentity {
            pid: child.id(),
            start_token: crate::daemon::lifecycle::process_start_token(child.id()).unwrap(),
            daemon_instance_id: "00112233445566778899aabbccddeeff".to_string(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
        };
        let hash = "7".repeat(64);
        let env = BTreeMap::from([
            ("XDG_STATE_HOME".to_string(), root.display().to_string()),
            (
                "TMUX".to_string(),
                format!("{},123,0", tmux_socket.display()),
            ),
        ]);
        crate::daemon::lifecycle::update_lifecycle_record(&env, &hash, |record| {
            record.process = Some(process);
            Ok(())
        })
        .unwrap();
        let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation {
            socket_path: tmux_socket.clone(),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 123,
                start_time: 456,
            },
            hash,
        };
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("999\t777\t{}\n", tmux_socket.display()),
        );

        let error = super::force_stop_verified(&runner, &env, &incarnation, &socket).unwrap_err();

        assert!(error.to_string().contains("incarnation mismatch"));
        assert!(
            child.try_wait().unwrap().is_none(),
            "server swap must not signal PID"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        drop((daemon_listener, tmux_listener));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stop_reports_live_unresponsive_recorded_process_as_degraded() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::thread;
        use std::time::Duration;

        let root = std::path::PathBuf::from(format!("/tmp/vtf-c-{:x}", std::process::id()));
        let runtime_root = std::path::PathBuf::from(format!("/tmp/vtr-c-{:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&runtime_root);
        std::fs::create_dir_all(&root).unwrap();
        let tmux_socket = root.join("tmux.sock");
        let tmux_listener = UnixListener::bind(&tmux_socket).unwrap();
        let env = BTreeMap::from([
            (
                "XDG_STATE_HOME".to_string(),
                root.join("state").display().to_string(),
            ),
            (
                "XDG_RUNTIME_DIR".to_string(),
                runtime_root.display().to_string(),
            ),
            (
                "TMUX".to_string(),
                format!("{},123,0", tmux_socket.display()),
            ),
        ]);
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("123\t456\t{}\n", tmux_socket.display()),
        );
        let incarnation =
            crate::daemon::lifecycle::TmuxServerIncarnation::resolve(&runner, &env).unwrap();
        let socket =
            crate::daemon::daemon_socket_path_for_incarnation(&env, None, &incarnation.hash);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::set_permissions(
            socket.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let daemon_listener = UnixListener::bind(&socket).unwrap();
        let hung_server = thread::spawn(move || {
            let (_stream, _) = daemon_listener.accept().unwrap();
            thread::sleep(Duration::from_secs(3));
        });
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let metadata = std::fs::metadata(&socket).unwrap();
        let process = crate::daemon::lifecycle::DaemonProcessIdentity {
            pid: child.id(),
            start_token: crate::daemon::lifecycle::process_start_token(child.id()).unwrap(),
            daemon_instance_id: "00112233445566778899aabbccddeeff".to_string(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
        };
        crate::daemon::lifecycle::update_lifecycle_record(&env, &incarnation.hash, |record| {
            record.process = Some(process.clone());
            Ok(())
        })
        .unwrap();

        let error = super::stop_daemon(&runner, &env, None, false).unwrap_err();

        assert!(error.to_string().contains("stop --force"));
        let record =
            crate::daemon::lifecycle::read_lifecycle_record(&env, &incarnation.hash).unwrap();
        assert_eq!(record.process, Some(process));
        assert_eq!(
            record.health,
            crate::daemon::lifecycle::LifecycleHealth::Degraded
        );
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        hung_server.join().unwrap();
        let _ = std::fs::remove_file(&socket);
        drop(tmux_listener);
        std::fs::remove_dir_all(root).unwrap();
        let _ = std::fs::remove_dir_all(runtime_root);
    }

    #[test]
    fn daemon_child_revalidates_config_after_parent_preflight() {
        let root = std::path::PathBuf::from(format!("/tmp/vtf-d-{:x}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config/vde/tmux")).unwrap();
        let tmux_socket = root.join("tmux.sock");
        let listener = UnixListener::bind(&tmux_socket).unwrap();
        let config_path = root.join("config/vde/tmux/config.yml");
        std::fs::write(&config_path, "daemon:\n  poll_ms: 100\n").unwrap();
        let env = BTreeMap::from([
            (
                "TMUX".to_string(),
                format!("{},123,0", tmux_socket.display()),
            ),
            (
                "XDG_CONFIG_HOME".to_string(),
                root.join("config").display().to_string(),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                root.join("state").display().to_string(),
            ),
        ]);
        crate::config::load::load_config_strict(&env).unwrap();
        std::fs::write(&config_path, "daemon:\n  poll_ms: invalid\n").unwrap();
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("123\t456\t{}\n", tmux_socket.display()),
        );

        let error = super::run_daemon(&runner, &env, None, None, None, None, None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("strict config validation failed")
        );
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }
}

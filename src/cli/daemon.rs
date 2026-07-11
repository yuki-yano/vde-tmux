use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

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
        crate::daemon::protocol::v2::ServerMessage::PaneResult { pane, .. } => {
            Ok(crate::statusline::render_structured_pane_status(
                config,
                &pane,
                crate::sidebar::tree::now_epoch_secs(),
            ))
        }
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
        crate::daemon::protocol::v2::ServerMessage::StatusSnapshotResult { snapshot, .. } => Ok(
            crate::statusline::render_structured_status_snapshot(config, &snapshot),
        ),
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon query failed ({code:?}): {message}")
        }
        other => bail!("unexpected daemon status response: {other:?}"),
    }
}

pub(crate) fn cleanup_legacy_state(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let (_, mut client) = pane_state_client(runner, env)?;
    let event_id = crate::pane_state::EventId::generate()?;
    match client.request(
        &crate::daemon::protocol::v2::ClientMessage::CleanupLegacyState {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            daemon_instance_id: client.daemon_instance_id().clone(),
            event_id: event_id.clone(),
        },
    )? {
        crate::daemon::protocol::v2::ServerMessage::CleanupLegacyResult {
            event_id: response_id,
            attempted,
            removed,
            failed,
            ..
        } if response_id == event_id => {
            if !failed.is_empty() {
                let details = failed
                    .iter()
                    .map(|failure| {
                        format!(
                            "{} {} {}: {}",
                            failure.scope, failure.target, failure.option, failure.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!(
                    "legacy pane-state cleanup was incomplete ({removed}/{attempted} removed): {details}"
                );
            }
            Ok(Some(format!(
                "legacy pane-state cleanup complete: {removed}/{attempted} removed"
            )))
        }
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon returned {code:?}: {message}")
        }
        response => bail!("unexpected daemon legacy cleanup response: {response:?}"),
    }
}

pub(crate) fn reset_pane_state(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    pane_id: &str,
) -> Result<Option<String>> {
    let (_, mut query_client) = pane_state_client(runner, env)?;
    let pane =
        match query_client.request(&crate::daemon::protocol::v2::ClientMessage::QueryPane {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            pane_id: pane_id.to_string(),
        })? {
            crate::daemon::protocol::v2::ServerMessage::PaneResult { pane, .. } => pane,
            crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
                bail!("daemon returned {code:?}: {message}")
            }
            response => bail!("unexpected daemon pane response: {response:?}"),
        };
    let expected = pane.stored.ok_or_else(|| {
        anyhow::anyhow!(
            "pane {} has no canonical or quarantined state to reset",
            pane_id
        )
    })?;
    drop(query_client);
    // A normal v2 connection carries exactly one request/response after Hello. Reset therefore
    // performs its guarded query and mutation on separate, freshly handshaken connections.
    let (_, mut client) = pane_state_client(runner, env)?;
    let event_id = crate::pane_state::EventId::generate()?;
    match client.request(
        &crate::daemon::protocol::v2::ClientMessage::ResetPaneState {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            daemon_instance_id: client.daemon_instance_id().clone(),
            event_id: event_id.clone(),
            pane_instance: pane.pane_instance,
            expected,
        },
    )? {
        crate::daemon::protocol::v2::ServerMessage::ResetResult {
            event_id: response_id,
            outcome,
            ..
        } if response_id == event_id => Ok(Some(format!(
            "pane state reset: {pane_id} ({})",
            match outcome {
                crate::daemon::protocol::v2::ResetOutcome::Replaced => "replaced",
                crate::daemon::protocol::v2::ResetOutcome::AlreadyReset => "already reset",
            }
        ))),
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon returned {code:?}: {message}")
        }
        response => bail!("unexpected daemon reset response: {response:?}"),
    }
}

pub(crate) fn uninstall_pane_state_hooks(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let (_, mut client) = pane_state_client(runner, env)?;
    let event_id = crate::pane_state::EventId::generate()?;
    match client.request(
        &crate::daemon::protocol::v2::ClientMessage::UninstallHooks {
            proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
            daemon_instance_id: client.daemon_instance_id().clone(),
            event_id: event_id.clone(),
        },
    )? {
        crate::daemon::protocol::v2::ServerMessage::HooksUninstalled {
            event_id: response_id,
            ..
        } if response_id == event_id => Ok(Some("pane-state hooks uninstalled".to_string())),
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon returned {code:?}: {message}")
        }
        response => bail!("unexpected daemon hook uninstall response: {response:?}"),
    }
}

fn pane_state_client(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<(
    crate::daemon::lifecycle::TmuxServerIncarnation,
    crate::daemon::protocol::v2::V2Client,
)> {
    let (incarnation, socket) =
        crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, None)?;
    let client = crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        &socket,
        &incarnation.hash,
        Duration::from_secs(2),
    )?;
    Ok((incarnation, client))
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
    let loaded = crate::config::load::load_config(env);
    for warning in loaded.warnings {
        eprintln!("{warning}");
    }
    crate::daemon::server::run_runtime_daemon_server(
        loaded.config,
        &socket_path,
        env,
        incarnation,
    )?;
    Ok(None)
}

pub(crate) fn ensure_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let (_, socket_path) = crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, socket)?;
    Ok(Some(format!("daemon serving: {}", socket_path.display())))
}

pub(crate) fn stop_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let socket_path =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    let Ok(mut client) = crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        &socket_path,
        &incarnation.hash,
        Duration::from_secs(2),
    ) else {
        return Ok(Some(format!(
            "daemon is not running: {}",
            socket_path.display()
        )));
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
        } if response_id == event_id => {
            Ok(Some(format!("daemon stopped: {}", socket_path.display())))
        }
        crate::daemon::protocol::v2::ServerMessage::Error { code, message, .. } => {
            bail!("daemon returned {code:?}: {message}")
        }
        response => bail!("unexpected daemon shutdown response: {response:?}"),
    }
}

pub(crate) fn restart_daemon(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    socket: Option<&str>,
) -> Result<Option<String>> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let previous_socket =
        crate::daemon::daemon_socket_path_for_incarnation(env, socket, &incarnation.hash);
    let _ = stop_daemon(runner, env, socket)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while crate::daemon::lifecycle::probe_v2_daemon(&previous_socket, &incarnation.hash).is_some() {
        if Instant::now() >= deadline {
            bail!("daemon did not stop before restart deadline");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let (_, socket_path) = crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, socket)?;
    Ok(Some(format!("daemon restarted: {}", socket_path.display())))
}

pub(crate) fn config_schema() -> Result<Option<String>> {
    Ok(Some(serde_json::to_string_pretty(
        &crate::config::schema::config_schema(),
    )?))
}

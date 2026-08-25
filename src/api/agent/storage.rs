use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::Result;

use super::super::common::{epoch_now, success_agent_json};
use super::super::connection::{ApiConnection, daemon_api_error};
use super::super::contract::{API_VERSION, ApiMeta, ApiResult, ApiSuccessEnvelope};
use crate::daemon::protocol::v2::{
    CLIENT_REQUEST_TIMEOUT, ClientMessage, PROTOCOL_VERSION, ServerMessage,
};
use crate::tmux::TmuxRunner;

pub fn agent_storage_status(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    connection
        .client
        .set_deadline(Instant::now() + CLIENT_REQUEST_TIMEOUT);
    let response = connection
        .client
        .request(&ClientMessage::QueryAgentStorage {
            proto: PROTOCOL_VERSION,
        })
        .map_err(|error| api_error!("daemon_query_failed", format!("{error:#}")))?;
    let usage = match response {
        ServerMessage::AgentStorageResult { proto, usage } if proto == PROTOCOL_VERSION => usage,
        ServerMessage::Error { code, message, .. } => {
            return Err(daemon_api_error(code, message).into());
        }
        other => {
            return Err(api_error!(
                "invalid_daemon_response",
                format!("unexpected agent storage result: {other:?}"),
            )
            .into());
        }
    };
    success_agent_json(&connection, observed_at, ApiResult::AgentStorage { usage })
}

pub fn agent_storage_reset_result(
    observed_at: i64,
    server_identity: String,
    previous_generation: String,
    generation: String,
) -> Result<String> {
    Ok(serde_json::to_string(&ApiSuccessEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: Some(server_identity),
            daemon_instance_id: None,
            snapshot_revision: None,
            started_at: observed_at,
            emitted_at: epoch_now(),
            diagnostic_count: 0,
        },
        result: ApiResult::AgentStorageReset {
            previous_generation,
            generation,
        },
    })?)
}

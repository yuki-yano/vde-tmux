use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Serialize;

use super::connection::ApiConnection;
use super::contract::{
    API_VERSION, AgentStatus, ApiMeta, ApiResult, ApiSuccessEnvelope, DiagnosticSummary,
};
use crate::daemon::protocol::v2::{DaemonDiagnostic, ResolvedSnapshot};

pub(in crate::api) const WAIT_POLL_INITIAL_INTERVAL: Duration = Duration::from_millis(50);

pub(in crate::api) fn success_json(
    connection: &ApiConnection,
    snapshot: &ResolvedSnapshot,
    started_at: i64,
    result: ApiResult,
) -> Result<String> {
    Ok(serde_json::to_string(&ApiSuccessEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: Some(connection.server_identity.clone()),
            daemon_instance_id: Some(connection.daemon_instance_id.clone()),
            snapshot_revision: Some(snapshot.snapshot_revision),
            started_at,
            emitted_at: epoch_now(),
            diagnostic_count: snapshot.diagnostics.len(),
        },
        result,
    })?)
}

pub(in crate::api) fn success_category_mutation_json(
    connection: &ApiConnection,
    started_at: i64,
    snapshot_revision: u64,
    result: ApiResult,
) -> Result<String> {
    Ok(serde_json::to_string(&ApiSuccessEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: Some(connection.server_identity.clone()),
            daemon_instance_id: Some(connection.daemon_instance_id.clone()),
            snapshot_revision: Some(snapshot_revision),
            started_at,
            emitted_at: epoch_now(),
            diagnostic_count: 0,
        },
        result,
    })?)
}

pub(in crate::api) fn success_agent_json(
    connection: &ApiConnection,
    started_at: i64,
    result: ApiResult,
) -> Result<String> {
    Ok(serde_json::to_string(&ApiSuccessEnvelope {
        meta: ApiMeta {
            api_version: API_VERSION,
            server_identity: Some(connection.server_identity.clone()),
            daemon_instance_id: Some(connection.daemon_instance_id.clone()),
            snapshot_revision: None,
            started_at,
            emitted_at: epoch_now(),
            diagnostic_count: 0,
        },
        result,
    })?)
}

pub(in crate::api) fn aggregate_diagnostics(
    diagnostics: &[DaemonDiagnostic],
) -> Vec<DiagnosticSummary> {
    let mut grouped = BTreeMap::<(String, String), (usize, BTreeSet<String>)>::new();
    for diagnostic in diagnostics {
        let entry = grouped
            .entry((enum_json_name(&diagnostic.code), diagnostic.message.clone()))
            .or_default();
        entry.0 += 1;
        if let Some(pane) = &diagnostic.pane_instance {
            entry.1.insert(pane.pane_id.clone());
        }
    }
    grouped
        .into_iter()
        .map(|((code, message), (count, pane_ids))| DiagnosticSummary {
            code,
            message,
            count,
            pane_ids: pane_ids.into_iter().collect(),
        })
        .collect()
}

pub(in crate::api) fn enum_json_name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

pub(in crate::api) fn format_statuses(statuses: &BTreeSet<AgentStatus>) -> String {
    statuses
        .iter()
        .map(|status| status.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

pub(in crate::api) fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

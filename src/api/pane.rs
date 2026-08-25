use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::Result;

use super::agent::projection::{agent_summary, pane_detail, pane_summary};
use super::common::{WAIT_POLL_INITIAL_INTERVAL, aggregate_diagnostics, success_json};
use super::connection::ApiConnection;
use super::contract::{
    ApiError, ApiErrorCode, ApiErrorStage, ApiResult, ApiRetryAction, ApiSideEffect,
    MAX_READ_BYTES, MAX_READ_LINES, PaneSplitOptions, PaneSplitReceipt, ReadOptions, ReadResult,
    ReadSource,
};
use super::mutation::{MUTATION_PROJECTION_TIMEOUT, refresh_canonical_topology_after_dispatch};
use super::terminal_mutation;
use crate::daemon::protocol::v2::{PanePresentation, ResolvedSnapshot};
use crate::pane_state::{EventId, PaneInstance};
use crate::tmux::TmuxRunner;

pub fn snapshot(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let panes = snapshot
        .panes
        .iter()
        .map(|pane| pane_summary(pane, &connection.server_identity))
        .collect();
    let agents = snapshot
        .panes
        .iter()
        .filter_map(|pane| agent_summary(pane, &snapshot, &connection.server_identity))
        .collect();
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::Snapshot {
            panes,
            agents,
            diagnostics: aggregate_diagnostics(&snapshot.diagnostics),
        },
    )
}

pub fn pane_list(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let panes = snapshot
        .panes
        .iter()
        .map(|pane| pane_summary(pane, &connection.server_identity))
        .collect();
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::PaneList { panes },
    )
}

pub fn pane_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
) -> Result<String> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let pane = resolve_pane(&snapshot, target, &connection.server_identity)?;
    let detail = pane_detail(pane, &snapshot, &connection.server_identity);
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::PaneGet { pane: detail },
    )
}

pub fn pane_current(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let target = env
        .get("TMUX_PANE")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| api_error!("no_current_pane", "TMUX_PANE is not set"))?;
    pane_get(runner, env, observed_at, target)
}

pub fn pane_read(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    options: ReadOptions,
) -> Result<String> {
    validate_read_options(options)?;
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let before = connection.query_snapshot()?;
    let pane = resolve_pane(&before, target, &connection.server_identity)?;
    let expected = pane.pane_instance.clone();
    let read = capture_pane_guarded(runner, env, &connection, &expected, options)?;
    let mut after_connection = connection.reconnect()?;
    let after = after_connection.query_snapshot()?;
    let pane = require_same_pane(&after, &expected)?;
    let pane_result = pane_summary(pane, &after_connection.server_identity);
    success_json(
        &after_connection,
        &after,
        observed_at,
        ApiResult::PaneRead {
            pane: pane_result,
            read,
        },
    )
}

pub fn pane_split(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    options: PaneSplitOptions<'_>,
) -> Result<String> {
    let PaneSplitOptions {
        direction,
        size_percent,
        cwd,
        focus,
    } = options;
    if !target.starts_with("vtp1:") {
        return Err(api_error!(
            "invalid_arguments",
            "pane split requires an exact pane_ref target",
        )
        .into());
    }
    if size_percent.is_some_and(|percent| !(1..=99).contains(&percent)) {
        return Err(api_error!(
            "invalid_arguments",
            "--size-percent must be between 1 and 99",
        )
        .into());
    }
    let mut connection = ApiConnection::connect(runner, env, None)?;
    let snapshot = connection.query_snapshot()?;
    let pane = resolve_pane(&snapshot, target, &connection.server_identity)?;
    let expected = pane.pane_instance.clone();
    let cwd = cwd.unwrap_or(&pane.current_path);
    validate_split_cwd(cwd)?;
    verify_live_pane(runner, env, &connection, &expected)?;
    let nonce = EventId::generate()
        .map_err(|error| api_error!("internal_error", format!("split request ID: {error}")))?;
    let created = match terminal_mutation::split_pane_guarded(
        runner,
        &connection.incarnation,
        &expected,
        terminal_mutation::SplitMutation {
            direction_flag: direction.tmux_flag(),
            size_percent,
            cwd,
            focus,
            nonce_seed: nonce.as_str(),
        },
    ) {
        terminal_mutation::SplitMutationOutcome::Applied(created) => created,
        terminal_mutation::SplitMutationOutcome::Rejected(message) => {
            return Err(api_error!("dispatch_rejected", message)
                .with_dispatch_context(
                    ApiErrorStage::BeforeDispatch,
                    ApiSideEffect::None,
                    ApiRetryAction::RefreshTarget,
                    None,
                )
                .into());
        }
        terminal_mutation::SplitMutationOutcome::DeliveryUnknown(message) => {
            return Err(api_error!("delivery_unknown", message)
                .with_dispatch_context(
                    ApiErrorStage::AfterDispatch,
                    ApiSideEffect::Possible,
                    ApiRetryAction::InspectManually,
                    None,
                )
                .into());
        }
    };
    require_live_pane_instance(runner, &created).map_err(|error| {
        api_error!(
            "delivery_unknown",
            format!(
                "split reported pane {} but its live identity could not be confirmed: {error:#}",
                created.pane_id
            ),
        )
        .with_dispatch_context(
            ApiErrorStage::AfterDispatch,
            ApiSideEffect::Confirmed,
            ApiRetryAction::InspectManually,
            None,
        )
    })?;
    let _refresh_revision = refresh_canonical_topology_after_dispatch(
        &connection,
        format!("pane {} was created", created.pane_id),
    )?;
    let projection_deadline = Instant::now() + MUTATION_PROJECTION_TIMEOUT;
    let mut after_connection = connection.reconnect()?;
    let after = loop {
        let after = after_connection.query_snapshot()?;
        if require_same_pane(&after, &created).is_ok() {
            break after;
        }
        if Instant::now() >= projection_deadline {
            return Err(ApiError::new(
                ApiErrorCode::Timeout,
                format!(
                    "pane {} was created but did not enter canonical topology within {} ms",
                    created.pane_id,
                    MUTATION_PROJECTION_TIMEOUT.as_millis()
                ),
            )
            .with_dispatch_context(
                ApiErrorStage::AfterDispatch,
                ApiSideEffect::Confirmed,
                ApiRetryAction::InspectManually,
                None,
            )
            .into());
        }
        std::thread::sleep(WAIT_POLL_INITIAL_INTERVAL);
        after_connection = connection.reconnect()?;
    };
    success_json(
        &after_connection,
        &after,
        observed_at,
        ApiResult::PaneSplit {
            split: PaneSplitReceipt {
                target_pane_ref: target.to_string(),
                pane_ref: pane_ref(&connection.server_identity, &created),
                pane_id: created.pane_id,
                pane_pid: created.pane_pid,
                direction,
                size_percent,
                cwd: cwd.to_string(),
                focused: focus,
            },
        },
    )
}

pub(in crate::api) fn validate_read_options(options: ReadOptions) -> Result<()> {
    if options.lines == 0 || options.lines > MAX_READ_LINES {
        return Err(api_error!(
            "invalid_arguments",
            format!("--lines must be between 1 and {MAX_READ_LINES}"),
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn capture_pane(
    runner: &dyn TmuxRunner,
    pane_id: &str,
    options: ReadOptions,
) -> Result<ReadResult> {
    let flag = if options.ansi { "-epJ" } else { "-pJ" };
    let start = format!("-{}", options.lines);
    let mut owned = vec![
        "capture-pane".to_string(),
        flag.to_string(),
        "-t".to_string(),
        pane_id.to_string(),
    ];
    if matches!(options.source, ReadSource::Latest) {
        owned.push("-S".to_string());
        owned.push(start);
    }
    let args = owned.iter().map(String::as_str).collect::<Vec<_>>();
    let output = runner
        .run_tail_bounded(&args, MAX_READ_BYTES)
        .map_err(|error| api_error!("capture_failed", format!("{error:#}")))?;
    let text = tail_lines(&output.text, options.lines);
    Ok(ReadResult {
        source: options.source.as_str().to_string(),
        ansi: options.ansi,
        lines_requested: options.lines,
        bytes_captured: output.total_bytes,
        bytes_returned: text.len(),
        truncated: output.truncated,
        text,
    })
}

pub(in crate::api) fn capture_pane_guarded(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    connection: &ApiConnection,
    expected: &PaneInstance,
    options: ReadOptions,
) -> Result<ReadResult> {
    verify_live_pane(runner, env, connection, expected)?;
    let read = capture_pane(runner, &expected.pane_id, options)?;
    verify_live_pane(runner, env, connection, expected)?;
    Ok(read)
}

pub(in crate::api) fn verify_live_pane(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    connection: &ApiConnection,
    expected: &PaneInstance,
) -> Result<()> {
    connection
        .incarnation
        .verify(runner, env)
        .map_err(|error| api_error!("stale_reference", format!("{error:#}")))?;
    require_live_pane_instance(runner, expected)
}

pub(in crate::api) fn require_live_pane_instance(
    runner: &dyn TmuxRunner,
    expected: &PaneInstance,
) -> Result<()> {
    let output = runner
        .run(&[
            "display-message",
            "-p",
            "-t",
            &expected.pane_id,
            "#{pane_id}\t#{pane_pid}",
        ])
        .map_err(|error| {
            api_error!(
                "stale_reference",
                format!(
                    "failed to resolve live pane {}: {error:#}",
                    expected.pane_id
                ),
            )
        })?;
    let mut fields = output.trim_end().split('\t');
    let actual = PaneInstance {
        pane_id: fields.next().unwrap_or_default().to_string(),
        pane_pid: fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or_default(),
    };
    if fields.next().is_some() || actual.validate().is_err() || actual != *expected {
        return Err(api_error!(
            "stale_reference",
            format!(
                "pane {} was replaced before or during capture",
                expected.pane_id
            ),
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn tail_lines(text: &str, lines: usize) -> String {
    let mut end = text.len();
    if text.as_bytes().last() == Some(&b'\n') {
        end = end.saturating_sub(1);
    }
    let Some(start) = text.as_bytes()[..end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .and_then(|last| {
            let mut position = last;
            for _ in 1..lines {
                let previous = text.as_bytes()[..position]
                    .iter()
                    .rposition(|byte| *byte == b'\n')?;
                position = previous;
            }
            Some(position + 1)
        })
    else {
        return text.to_string();
    };
    text[start..].to_string()
}

pub(in crate::api) fn validate_split_cwd(cwd: &str) -> Result<()> {
    let path = std::path::Path::new(cwd);
    if !path.is_absolute() {
        return Err(api_error!("invalid_arguments", "split cwd must be absolute").into());
    }
    if !path.is_dir() {
        return Err(api_error!(
            "invalid_arguments",
            format!("split cwd is not an existing directory: {cwd}"),
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn resolve_pane<'a>(
    snapshot: &'a ResolvedSnapshot,
    target: &str,
    server_identity: &str,
) -> Result<&'a PanePresentation> {
    if target.starts_with("vtp1:") {
        let expected = parse_pane_ref(target, server_identity)?;
        return require_same_pane(snapshot, &expected);
    }
    validate_pane_id(target)?;
    snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance.pane_id == target)
        .ok_or_else(|| api_error!("pane_not_found", format!("pane {target} was not found")).into())
}

pub(in crate::api) fn require_same_pane<'a>(
    snapshot: &'a ResolvedSnapshot,
    expected: &PaneInstance,
) -> Result<&'a PanePresentation> {
    let Some(pane) = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_instance.pane_id == expected.pane_id)
    else {
        return Err(api_error!(
            "stale_reference",
            format!("pane {} no longer exists", expected.pane_id),
        )
        .into());
    };
    if pane.pane_instance != *expected {
        return Err(api_error!(
            "stale_reference",
            format!("pane {} was replaced by another process", expected.pane_id),
        )
        .into());
    }
    Ok(pane)
}

pub(in crate::api) fn pane_ref(server_identity: &str, pane: &PaneInstance) -> String {
    format!(
        "vtp1:{server_identity}:{}:{}",
        pane.pane_id.trim_start_matches('%'),
        pane.pane_pid
    )
}

pub(in crate::api) fn parse_pane_ref(value: &str, server_identity: &str) -> Result<PaneInstance> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 4 || parts[0] != "vtp1" {
        return Err(api_error!("invalid_reference", "invalid pane_ref").into());
    }
    if parts[1] != server_identity {
        return Err(
            api_error!("stale_reference", "pane_ref belongs to another tmux server").into(),
        );
    }
    let pane = PaneInstance {
        pane_id: format!("%{}", parts[2]),
        pane_pid: parts[3]
            .parse()
            .map_err(|_| api_error!("invalid_reference", "invalid pane_ref pane PID"))?,
    };
    pane.validate()
        .map_err(|error| api_error!("invalid_reference", error.to_string()))?;
    Ok(pane)
}

pub(in crate::api) fn validate_pane_id(value: &str) -> Result<()> {
    let valid = value.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    if valid {
        Ok(())
    } else {
        Err(api_error!(
            "invalid_target",
            "target must be a %pane_id, pane_ref, or agent_ref",
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_line_bounds_are_enforced_before_capture() {
        assert!(
            validate_read_options(ReadOptions {
                source: ReadSource::Latest,
                lines: 0,
                ansi: false,
            })
            .is_err()
        );
        assert!(
            validate_read_options(ReadOptions {
                source: ReadSource::Latest,
                lines: MAX_READ_LINES + 1,
                ansi: false,
            })
            .is_err()
        );
    }

    #[test]
    fn latest_capture_includes_visible_rows_and_returns_the_requested_tail() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(
            &["capture-pane", "-pJ", "-t", "%1", "-S", "-2"],
            "old\none\ntwo\n",
        );

        let read = capture_pane(
            &runner,
            "%1",
            ReadOptions {
                source: ReadSource::Latest,
                lines: 2,
                ansi: false,
            },
        )
        .unwrap();

        assert_eq!(read.text, "one\ntwo\n");
        assert!(!read.truncated);
    }

    #[test]
    fn visible_capture_honors_the_requested_line_count() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(&["capture-pane", "-pJ", "-t", "%1"], "one\ntwo\nthree\n");

        let read = capture_pane(
            &runner,
            "%1",
            ReadOptions {
                source: ReadSource::Visible,
                lines: 1,
                ansi: false,
            },
        )
        .unwrap();

        assert_eq!(read.text, "three\n");
        assert_eq!(read.lines_requested, 1);
        assert_eq!(read.bytes_returned, 6);
    }

    #[test]
    fn live_pane_guard_rejects_a_replaced_process() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let expected = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        runner.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "%1",
                "#{pane_id}\t#{pane_pid}",
            ],
            "%1\t202\n",
        );

        let error = require_live_pane_instance(&runner, &expected).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "stale_reference"
        );
    }
}

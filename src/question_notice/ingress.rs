use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use super::{MAX_ANCESTORS, NoticeReason, QuestionNoticeInput};
use crate::pane_state::{AgentProcessIdentity, IDENTIFIER_MAX_BYTES};

pub fn from_payload(
    event: &str,
    raw: &str,
    codex_home: Option<&Path>,
) -> Option<QuestionNoticeInput> {
    let payload: Value = serde_json::from_str(raw).ok()?;
    if payload.get("tool_name").and_then(Value::as_str) != Some("request_user_input_async") {
        return None;
    }
    let rejected = |reason| Some(QuestionNoticeInput::Rejected { reason });
    if event != "PostToolUse" {
        return None;
    }
    let text = |key| payload.get(key).and_then(Value::as_str);
    if text("hook_event_name") != Some("PostToolUse")
        || text("tool_response")
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .is_none_or(|value| value.get("accepted") != Some(&Value::Bool(true)))
    {
        return rejected(NoticeReason::InvalidPayload);
    }
    let Some((session, turn, tool)) = text("session_id")
        .zip(text("turn_id"))
        .zip(text("tool_use_id"))
        .map(|((session, turn), tool)| (session, turn, tool))
        .filter(|(session, turn, tool)| [*session, *turn, *tool].into_iter().all(valid_identifier))
    else {
        return rejected(NoticeReason::InvalidPayload);
    };
    if !crate::hook::origin::codex_hook_origin_from_payload(
        Some(session),
        text("agent_id"),
        text("transcript_path"),
        codex_home,
    )
    .is_parent()
    {
        return rejected(NoticeReason::OriginUnverified);
    }
    Some(QuestionNoticeInput::Issued {
        session_id: session.to_string(),
        turn_id: turn.to_string(),
        tool_use_id: tool.to_string(),
        ancestors: Vec::new(),
    })
}

pub fn valid_identifier(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= IDENTIFIER_MAX_BYTES && !value.contains(['\r', '\n'])
}

/// Capture once at hook entry. Missing/reused ancestors invalidate the whole chain.
pub fn capture_ancestors() -> anyhow::Result<Vec<AgentProcessIdentity>> {
    let mut pid = std::process::id();
    let mut seen = BTreeSet::new();
    let mut chain = Vec::new();
    for _ in 0..MAX_ANCESTORS {
        if pid <= 1 {
            return Ok(chain);
        }
        anyhow::ensure!(seen.insert(pid), "cyclic hook ancestry");
        let start_token = crate::daemon::lifecycle::agent_process_start_token(pid)?;
        let parent = parent_pid(pid)?;
        anyhow::ensure!(
            crate::daemon::lifecycle::agent_process_start_token(pid)? == start_token,
            "hook ancestor replaced"
        );
        chain.push(AgentProcessIdentity { pid, start_token });
        pid = parent;
    }
    anyhow::bail!("hook ancestor depth exceeded")
}

fn parent_pid(pid: u32) -> anyhow::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_pidinfo writes a checked, fixed-size C record.
        let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let count = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        anyhow::ensure!(
            count == size && info.pbi_pid == pid,
            "hook ancestor unavailable"
        );
        Ok(info.pbi_ppid)
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields = stat
            .rsplit_once(") ")
            .ok_or_else(|| anyhow::anyhow!("invalid process stat"))?
            .1;
        Ok(fields
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("missing parent PID"))?
            .parse()?)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        anyhow::bail!("hook ancestor verification is unsupported on this OS")
    }
}

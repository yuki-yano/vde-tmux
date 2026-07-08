use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOrigin {
    Parent,
    Subagent,
}

pub fn codex_hook_origin(session_id: Option<&str>, codex_home: Option<&Path>) -> HookOrigin {
    let Some(session_id) = session_id.filter(|id| !id.trim().is_empty()) else {
        return HookOrigin::Parent;
    };
    let Some(codex_home) = codex_home else {
        return HookOrigin::Parent;
    };
    let Some(path) = find_codex_session_file(&codex_home.join("sessions"), session_id) else {
        return HookOrigin::Parent;
    };
    codex_hook_origin_from_session_file(&path)
}

pub fn claude_hook_origin(
    transcript_path: Option<&str>,
    agent_transcript_path: Option<&str>,
) -> HookOrigin {
    if agent_transcript_path.is_some_and(|path| !path.trim().is_empty()) {
        return HookOrigin::Subagent;
    }
    let Some(transcript_path) = transcript_path.filter(|path| !path.trim().is_empty()) else {
        return HookOrigin::Parent;
    };
    let path = Path::new(transcript_path);
    if path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|component| component == "subagents")
    }) {
        return HookOrigin::Subagent;
    }
    claude_hook_origin_from_transcript(path)
}

pub(crate) fn find_codex_session_file(dir: &Path, session_id: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_codex_session_file(&path, session_id) {
                return Some(found);
            }
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".jsonl") && name.contains(session_id))
        {
            return Some(path);
        }
    }
    None
}

fn codex_hook_origin_from_session_file(path: &Path) -> HookOrigin {
    let Some(value) = read_jsonl_values(path, 40)
        .into_iter()
        .find(|value| value.get("type").and_then(Value::as_str) == Some("session_meta"))
    else {
        return HookOrigin::Parent;
    };
    let payload = value.get("payload").unwrap_or(&value);
    if payload.get("thread_source").and_then(Value::as_str) == Some("subagent")
        || non_empty_str(payload.get("parent_thread_id"))
    {
        HookOrigin::Subagent
    } else {
        HookOrigin::Parent
    }
}

fn claude_hook_origin_from_transcript(path: &Path) -> HookOrigin {
    if read_jsonl_values(path, 40)
        .iter()
        .any(value_has_claude_subagent_marker)
    {
        HookOrigin::Subagent
    } else {
        HookOrigin::Parent
    }
}

fn read_jsonl_values(path: &Path, limit: usize) -> Vec<Value> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .take(limit)
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect()
}

fn value_has_claude_subagent_marker(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.get("isSidechain").and_then(Value::as_bool) == Some(true)
                || non_empty_str(map.get("parent_thread_id"))
                || map.get("thread_source").and_then(Value::as_str) == Some("subagent")
                || map
                    .get("source")
                    .and_then(|source| source.get("subagent"))
                    .is_some()
            {
                return true;
            }
            map.values().any(value_has_claude_subagent_marker)
        }
        Value::Array(items) => items.iter().any(value_has_claude_subagent_marker),
        _ => false,
    }
}

fn non_empty_str(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn codex_origin_detects_root_and_subagent_session_meta() {
        let root = unique_temp_dir("codex-origin");
        let sessions = root.join("sessions").join("2026").join("07").join("08");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("rollout-root-session.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"root-session","thread_source":"root"}}"#,
        )
        .unwrap();
        fs::write(
            sessions.join("rollout-subagent-session.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"subagent-session","thread_source":"subagent","parent_thread_id":"parent-session"}}"#,
        )
        .unwrap();

        assert_eq!(
            codex_hook_origin(Some("root-session"), Some(&root)),
            HookOrigin::Parent
        );
        assert_eq!(
            codex_hook_origin(Some("subagent-session"), Some(&root)),
            HookOrigin::Subagent
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn codex_origin_missing_session_file_is_parent() {
        let root = unique_temp_dir("codex-origin-missing");
        assert_eq!(
            codex_hook_origin(Some("missing-session"), Some(&root)),
            HookOrigin::Parent
        );
    }

    #[test]
    fn claude_origin_detects_root_and_subagent_transcript() {
        let root = unique_temp_dir("claude-origin");
        let root_transcript = root.join("root.jsonl");
        let subagents = root.join("subagents");
        let subagent_transcript = subagents.join("agent.jsonl");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(
            &root_transcript,
            r#"{"type":"user","message":{"role":"user"}}"#,
        )
        .unwrap();
        fs::write(
            &subagent_transcript,
            r#"{"type":"user","isSidechain":true}"#,
        )
        .unwrap();

        assert_eq!(
            claude_hook_origin(root_transcript.to_str(), None),
            HookOrigin::Parent
        );
        assert_eq!(
            claude_hook_origin(subagent_transcript.to_str(), None),
            HookOrigin::Subagent
        );
        assert_eq!(
            claude_hook_origin(None, subagent_transcript.to_str()),
            HookOrigin::Subagent
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn claude_origin_treats_parent_uuid_as_parent_conversation_chain() {
        let root = unique_temp_dir("claude-origin-marker");
        fs::create_dir_all(&root).unwrap();
        let transcript = root.join("agent.jsonl");
        fs::write(&transcript, r#"{"parentUuid":"parent-session"}"#).unwrap();

        assert_eq!(
            claude_hook_origin(transcript.to_str(), None),
            HookOrigin::Parent
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn claude_origin_detects_subagent_marker_in_transcript() {
        let root = unique_temp_dir("claude-origin-parent-thread");
        fs::create_dir_all(&root).unwrap();
        let transcript = root.join("agent.jsonl");
        fs::write(&transcript, r#"{"parent_thread_id":"parent-session"}"#).unwrap();

        assert_eq!(
            claude_hook_origin(transcript.to_str(), None),
            HookOrigin::Subagent
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vde-tmux-{name}-{}-{nanos}", std::process::id()))
    }
}

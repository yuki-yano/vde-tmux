//! Read-only provider transcripts. Bodies are never persisted in daemon state.
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod claude;
mod codex;
#[cfg(test)]
mod tests;

pub const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Instruction,
    Commentary,
    Final,
    ToolCall,
    ToolResult,
    CompactionSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConversationMessage {
    pub source_ref: String,
    pub order: u64,
    pub role: MessageRole,
    pub kind: MessageKind,
    pub original_role: String,
    pub text: String,
    pub text_complete: bool,
    pub text_digest: String,
    pub provider_message_id: Option<String>,
    pub provider_turn_id: Option<String>,
    pub turn_index: u64,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConversationTurn {
    pub turn_index: u64,
    pub provider_turn_id: Option<String>,
    pub user_source_refs: Vec<String>,
    pub final_source_ref: Option<String>,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryOmission {
    pub reason: String,
    pub source_ref: Option<String>,
    pub omitted_text_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ParsedConversation {
    pub messages: Vec<ConversationMessage>,
    pub turns: Vec<ConversationTurn>,
    pub omissions: Vec<HistoryOmission>,
    pub compacted: bool,
    pub partial_record: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationError(pub String);
impl std::fmt::Display for ConversationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConversationError {}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn source_ref(provider: &str, session: &str, id: &str) -> String {
    format!(
        "vtm1:{}",
        digest(&serde_json::to_vec(&(provider, session, id)).unwrap())
    )
}

/// The unterminated final record is ignored, never repaired or interpreted.
pub fn records(bytes: &[u8]) -> Result<(Vec<serde_json::Value>, bool), ConversationError> {
    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut records = Vec::new();
    for line in bytes[..end]
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        records.push(
            serde_json::from_slice(line)
                .map_err(|_| ConversationError("invalid_jsonl_record".into()))?,
        );
    }
    Ok((records, end != bytes.len()))
}

pub fn parse(
    provider: &str,
    session: &str,
    bytes: &[u8],
) -> Result<ParsedConversation, ConversationError> {
    let (records, partial) = records(bytes)?;
    if records.is_empty() {
        return Err(ConversationError("history_unavailable".into()));
    }
    let mut parsed = match provider {
        "codex" => codex::parse(session, &records)?,
        "claude" => claude::parse(session, &records)?,
        _ => return Err(ConversationError("unsupported_provider".into())),
    };
    parsed.partial_record = partial;
    if partial {
        parsed.omissions.push(HistoryOmission {
            reason: "incomplete_jsonl_tail".into(),
            source_ref: None,
            omitted_text_digest: None,
        });
    }
    for message in &mut parsed.messages {
        let truncation = parsed.omissions.iter().find(|omission| {
            omission.source_ref.as_ref() == Some(&message.source_ref)
                && omission.reason == "tool_text_truncated"
        });
        if let Some(truncation) = truncation {
            message.text_complete = false;
            message.text_digest = truncation
                .omitted_text_digest
                .clone()
                .ok_or_else(|| ConversationError("omitted_text_digest_unavailable".into()))?;
        } else {
            message.text_complete = true;
            message.text_digest = digest(message.text.as_bytes());
        }
    }
    if serde_json::to_vec(&parsed).unwrap().len() > MAX_CONTEXT_BYTES {
        return Err(ConversationError("context_size_limit".into()));
    }
    Ok(parsed)
}

/// Private hook metadata; the path never appears in the public API or Debug.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookBinding {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub provider_turn_id: Option<String>,
    pub prompt_digest: Option<String>,
    pub response_digest: Option<String>,
}
impl std::fmt::Debug for HookBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookBinding")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}
impl HookBinding {
    pub fn capture(raw: &str) -> Option<Self> {
        let payload: serde_json::Value = serde_json::from_str(raw).ok()?;
        let path = PathBuf::from(payload.get("transcript_path")?.as_str()?);
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            return None;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return None,
        };
        if metadata
            .as_ref()
            .is_some_and(|m| !m.is_file() || m.uid() != unsafe { libc::geteuid() })
        {
            return None;
        }
        Some(Self {
            path,
            device: metadata.as_ref().map_or(0, |m| m.dev()),
            inode: metadata.as_ref().map_or(0, |m| m.ino()),
            bytes: metadata.as_ref().map_or(0, |m| m.len()),
            provider_turn_id: payload
                .get("turn_id")
                .or_else(|| payload.get("prompt_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            prompt_digest: payload
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| digest(s.as_bytes())),
            response_digest: payload
                .get("last_assistant_message")
                .and_then(|v| v.as_str())
                .map(|s| digest(s.as_bytes())),
        })
    }
    pub fn valid(&self) -> bool {
        self.path.is_absolute() && self.path.as_os_str().len() <= 4096
    }
    pub fn read(&self) -> Result<Vec<u8>, ConversationError> {
        if self.inode == 0 {
            return Err(ConversationError(
                "transcript_not_materialized_at_hook".into(),
            ));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path)
            .map_err(|_| ConversationError("transcript_unavailable".into()))?;
        let metadata = file
            .metadata()
            .map_err(|_| ConversationError("transcript_unavailable".into()))?;
        if !metadata.is_file()
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.len() < self.bytes
        {
            return Err(ConversationError("transcript_replaced_or_truncated".into()));
        }
        let end = metadata.len();
        if end > MAX_TRANSCRIPT_BYTES {
            return Err(ConversationError("transcript_size_limit".into()));
        }
        let mut bytes = Vec::new();
        file.take(end)
            .read_to_end(&mut bytes)
            .map_err(|_| ConversationError("transcript_unavailable".into()))?;
        if bytes.len() as u64 != end {
            return Err(ConversationError("transcript_changed_during_read".into()));
        }
        Ok(bytes)
    }
}
use std::os::unix::fs::OpenOptionsExt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundConversation {
    pub state_id: crate::pane_state::StateId,
    pub agent_epoch: u64,
    pub session_id: String,
    pub run_seq: u64,
    pub hook: HookBinding,
    pub stop_bytes: Option<u64>,
}
pub type ConversationBindings = BTreeMap<crate::pane_state::PaneInstance, BoundConversation>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FileStamp {
    device: u64,
    inode: u64,
    bytes: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}

pub(crate) fn file_stamp(hook: &HookBinding) -> Option<FileStamp> {
    std::fs::symlink_metadata(&hook.path)
        .ok()
        .map(|m| FileStamp {
            device: m.dev(),
            inode: m.ino(),
            bytes: m.len(),
            modified_seconds: m.mtime(),
            modified_nanos: m.mtime_nsec(),
            changed_seconds: m.ctime(),
            changed_nanos: m.ctime_nsec(),
        })
}

pub(crate) fn file_stamps(
    bindings: &ConversationBindings,
) -> BTreeMap<crate::pane_state::PaneInstance, Option<FileStamp>> {
    bindings
        .iter()
        .map(|(pane, binding)| (pane.clone(), file_stamp(&binding.hook)))
        .collect()
}

pub fn observe_binding(
    bindings: &mut ConversationBindings,
    record: Option<&crate::pane_state::PaneState>,
    envelope: &crate::pane_state::PaneEventEnvelope,
    hook: Option<HookBinding>,
) {
    use crate::pane_state::{AgentSessionSource, PaneEvent};
    let Some(record) = record.filter(|record| {
        record.agent_session_id == envelope.agent_session_id
            && Some(&record.agent) == envelope.agent.as_ref()
    }) else {
        return;
    };
    if matches!(
        envelope.event,
        PaneEvent::ActivityObserved { .. } | PaneEvent::ClaudeToolResultObserved { .. }
    ) {
        if let Some(old) = bindings.get_mut(&envelope.pane_instance).filter(|old| {
            old.state_id == record.state_id
                && old.agent_epoch == record.agent_epoch
                && old.run_seq == record.run_seq
                && old.hook.inode == 0
        }) && let Some(mut hook) =
            hook.filter(|hook| hook.valid() && hook.inode != 0 && hook.path == old.hook.path)
        {
            hook.prompt_digest = old.hook.prompt_digest.clone();
            hook.provider_turn_id = old.hook.provider_turn_id.clone();
            hook.response_digest = old.hook.response_digest.clone();
            old.hook = hook;
        }
        return;
    }
    if !matches!(
        envelope.event,
        PaneEvent::AgentSessionStarted { .. }
            | PaneEvent::BeginRun { .. }
            | PaneEvent::ResponseAndCompleteRun { .. }
            | PaneEvent::CompleteRun { .. }
            | PaneEvent::ClaudeStopped { .. }
    ) {
        return;
    }
    let Some(hook) = hook.filter(HookBinding::valid) else {
        bindings.remove(&envelope.pane_instance);
        return;
    };
    let session_id = record
        .agent_session_id
        .as_ref()
        .unwrap()
        .as_str()
        .to_owned();
    let old = bindings.get(&envelope.pane_instance).filter(|old| {
        old.state_id == record.state_id
            && old.agent_epoch == record.agent_epoch
            && old.session_id == session_id
            && old.run_seq == record.run_seq
    });
    let compact = matches!(
        envelope.event,
        PaneEvent::AgentSessionStarted {
            source: AgentSessionSource::Compact,
            ..
        }
    );
    let stopping = matches!(
        envelope.event,
        PaneEvent::ResponseAndCompleteRun { .. }
            | PaneEvent::CompleteRun { .. }
            | PaneEvent::ClaudeStopped { .. }
    );
    // Claude can emit Stop while a background result is still outstanding.
    // Only canonical completion closes the conversation's turn boundary.
    if stopping
        && !(record.run_seq > 0
            && record.completed_seq == record.run_seq
            && matches!(record.lifecycle, crate::pane_state::LifecycleState::Idle))
    {
        return;
    }
    // Duplicate Stop hooks must never move the completion boundary into later data.
    if stopping && old.is_some_and(|old| old.stop_bytes.is_some()) {
        return;
    }
    let mut binding = BoundConversation {
        state_id: record.state_id.clone(),
        agent_epoch: record.agent_epoch,
        session_id,
        run_seq: record.run_seq,
        hook,
        stop_bytes: None,
    };
    if (stopping || compact) && binding.hook.provider_turn_id.is_none() {
        binding.hook.provider_turn_id = old.and_then(|old| old.hook.provider_turn_id.clone());
    }
    if stopping {
        binding.stop_bytes = Some(binding.hook.bytes);
        binding.hook.prompt_digest = old.and_then(|old| old.hook.prompt_digest.clone());
    } else if compact {
        binding.hook.prompt_digest = old.and_then(|old| old.hook.prompt_digest.clone());
    }
    bindings.insert(envelope.pane_instance.clone(), binding);
}

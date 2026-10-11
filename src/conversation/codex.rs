//! Codex rollout normalization for the paginated history format (verified on 0.162.1).
//!
//! User instructions come only from `item_completed` UserMessage items, each paired with
//! the preceding model-visible user `response_item` of the same turn. Unpaired user
//! response items are injected context. Unknown record, event, item and content shapes fail
//! closed instead of producing a complete-looking history.
use serde_json::Value;

use super::*;

const PROVIDER: &str = "codex";
/// Tool bodies are clipped with an explicit omission; user and assistant text never is.
pub(super) const TOOL_TEXT_MAX_BYTES: usize = 512;

pub(super) fn parse(
    session: &str,
    records: &[Value],
) -> Result<ParsedConversation, ConversationError> {
    let mut parser = Parser::new(session);
    for (index, record) in records.iter().enumerate() {
        parser.record(index as u64 + 1, record)?;
    }
    Ok(parser.finish())
}

pub(super) fn fail(reason: &str) -> ConversationError {
    ConversationError(reason.to_owned())
}

/// Reasons without a source are recorded once; `*_excluded` marks intentional filtering.
#[derive(Default)]
pub(super) struct Omissions(pub(super) Vec<HistoryOmission>);

impl Omissions {
    pub(super) fn note(&mut self, reason: &str) {
        if !self
            .0
            .iter()
            .any(|omission| omission.reason == reason && omission.source_ref.is_none())
        {
            self.0.push(HistoryOmission {
                reason: reason.to_owned(),
                source_ref: None,
                omitted_text_digest: None,
            });
        }
    }

    pub(super) fn at(&mut self, reason: &str, source_ref: &str) {
        self.0.push(HistoryOmission {
            reason: reason.to_owned(),
            source_ref: Some(source_ref.to_owned()),
            omitted_text_digest: None,
        });
    }
}

pub(super) fn clip_tool_text(text: String, source_ref: &str, omissions: &mut Omissions) -> String {
    if text.len() <= TOOL_TEXT_MAX_BYTES {
        return text;
    }
    let mut end = TOOL_TEXT_MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    omissions.0.push(HistoryOmission {
        reason: "tool_text_truncated".to_owned(),
        source_ref: Some(source_ref.to_owned()),
        omitted_text_digest: Some(digest(text.as_bytes())),
    });
    text[..end].to_owned()
}

pub(super) fn message(
    source_ref: String,
    role: MessageRole,
    kind: MessageKind,
    original_role: &str,
    text: String,
) -> ConversationMessage {
    ConversationMessage {
        source_ref,
        order: 0,
        role,
        kind,
        original_role: original_role.to_owned(),
        text_complete: true,
        text_digest: digest(text.as_bytes()),
        text,
        provider_message_id: None,
        provider_turn_id: None,
        turn_index: 0,
        tool_call_id: None,
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn present(value: Option<&Value>) -> bool {
    value.is_some_and(|value| !value.is_null())
}

fn object(value: Option<&Value>) -> Result<&Value, ConversationError> {
    value
        .filter(|value| value.is_object())
        .ok_or_else(|| fail("invalid_codex_record"))
}

/// Context such as `<environment_context>` or `# AGENTS.md instructions` that has no
/// UserMessage item. Only used for replacement history, where no pairing exists.
fn looks_injected(text: &str) -> bool {
    let text = text.trim_start();
    if text.starts_with("# ") {
        return true;
    }
    text.strip_prefix('<')
        .and_then(|rest| rest.split_once('>'))
        .is_some_and(|(tag, _)| {
            !tag.is_empty()
                && tag
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

// These two unpaired user fragments are verified contextual boundaries upstream.
// Other unpaired bodies stay ambiguous for rollback instead of using looks_injected.
fn verified_rollback_context(text: &str) -> bool {
    let text = text.trim();
    (text.starts_with("# AGENTS.md instructions") && text.ends_with("</INSTRUCTIONS>"))
        || (text.starts_with("<environment_context>") && text.ends_with("</environment_context>"))
}

fn image_wrapper(text: &str) -> bool {
    let text = text.trim();
    text == "</image>"
        || (text.starts_with("<image") && text.ends_with('>') && !text.contains('\n'))
}

/// The model-visible user text, without the image wrappers added around `input_image`.
fn response_user_text(content: Option<&Value>) -> Result<String, ConversationError> {
    let items = content
        .and_then(Value::as_array)
        .ok_or_else(|| fail("invalid_codex_content"))?;
    let mut parts = Vec::new();
    for item in items {
        match str_field(item, "type") {
            Some("input_text") => {
                let text = str_field(item, "text").ok_or_else(|| fail("invalid_codex_content"))?;
                if !image_wrapper(text) {
                    parts.push(text);
                }
            }
            Some("input_image") => {}
            _ => return Err(fail("unknown_codex_content")),
        }
    }
    Ok(parts.join("\n"))
}

fn user_message_text(content: Option<&Value>) -> Result<(String, bool), ConversationError> {
    let items = content
        .and_then(Value::as_array)
        .ok_or_else(|| fail("invalid_codex_content"))?;
    let mut parts = Vec::new();
    let mut image = false;
    for item in items {
        match str_field(item, "type") {
            Some("text") => {
                parts.push(str_field(item, "text").ok_or_else(|| fail("invalid_codex_content"))?)
            }
            Some("local_image") | Some("image") => image = true,
            Some("skill") => {}
            _ => return Err(fail("unknown_codex_content")),
        }
    }
    Ok((parts.join("\n"), image))
}

fn assistant_text(content: Option<&Value>) -> Result<String, ConversationError> {
    let items = content
        .and_then(Value::as_array)
        .ok_or_else(|| fail("invalid_codex_content"))?;
    let mut parts = Vec::new();
    for item in items {
        if str_field(item, "type") != Some("output_text") {
            return Err(fail("unknown_codex_content"));
        }
        parts.push(str_field(item, "text").ok_or_else(|| fail("invalid_codex_content"))?);
    }
    Ok(parts.join("\n"))
}

fn json_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Commentary,
    FinalAnswer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Open,
    Completed { last_agent_message: Option<String> },
    Failed,
    Aborted,
}

struct Turn {
    index: u64,
    id: String,
    outcome: Outcome,
    /// Closed before the latest compaction: its assistant messages are no longer effective.
    compacted: bool,
    /// A rollback removed a later instruction of this turn, so its final is gone.
    truncated: bool,
}

struct Entry {
    message: ConversationMessage,
    phase: Option<Phase>,
}

struct Parser<'a> {
    session: &'a str,
    header: bool,
    entries: Vec<Entry>,
    turns: Vec<Turn>,
    turn_ids: std::collections::BTreeSet<String>,
    open: Option<usize>,
    /// Unpaired model-visible user texts of the open turn: (line, text).
    pending_user: Vec<(u64, String)>,
    omissions: Omissions,
    compacted: bool,
    rollback_attribution_unknown: bool,
}

impl<'a> Parser<'a> {
    fn new(session: &'a str) -> Self {
        Self {
            session,
            header: false,
            entries: Vec::new(),
            turns: Vec::new(),
            turn_ids: Default::default(),
            open: None,
            pending_user: Vec::new(),
            omissions: Omissions::default(),
            compacted: false,
            rollback_attribution_unknown: false,
        }
    }

    fn source_ref(&self, id: &str) -> String {
        source_ref(PROVIDER, self.session, id)
    }

    fn record(&mut self, line: u64, record: &Value) -> Result<(), ConversationError> {
        let kind = str_field(record, "type").ok_or_else(|| fail("unknown_codex_record"))?;
        let payload = record.get("payload");
        if !self.header {
            if kind != "session_meta" {
                return Err(fail("missing_session_header"));
            }
            return self.header(object(payload)?);
        }
        match kind {
            "session_meta" => Err(fail("unexpected_session_meta")),
            "turn_context"
            | "world_state"
            | "token_usage_record"
            | "inter_agent_communication_metadata" => Ok(()),
            "realtime_item" => {
                self.omissions.note("realtime_transcript_unsupported");
                Ok(())
            }
            "response_item" => self.response_item(line, object(payload)?),
            "event_msg" => self.event(line, object(payload)?),
            "compacted" => self.compacted(line, object(payload)?),
            _ => Err(fail("unknown_codex_record")),
        }
    }

    fn header(&mut self, payload: &Value) -> Result<(), ConversationError> {
        if str_field(payload, "id") != Some(self.session) {
            return Err(fail("session_mismatch"));
        }
        if str_field(payload, "thread_source") != Some("user")
            || present(payload.get("parent_thread_id"))
        {
            return Err(fail("non_root_session"));
        }
        // A fork starts from a parent history that this file does not contain.
        if present(payload.get("forked_from_id")) || present(payload.get("history_base")) {
            return Err(fail("forked_history_unsupported"));
        }
        self.header = true;
        Ok(())
    }

    fn open_turn(&self) -> Result<&Turn, ConversationError> {
        self.open
            .map(|index| &self.turns[index])
            .ok_or_else(|| fail("turn_attribution_unknown"))
    }

    fn push(&mut self, mut message: ConversationMessage, phase: Option<Phase>) {
        if let Some(turn) = self.open.map(|index| &self.turns[index]) {
            message.turn_index = turn.index;
            message.provider_turn_id = Some(turn.id.clone());
        }
        self.entries.push(Entry { message, phase });
    }

    fn response_item(&mut self, line: u64, payload: &Value) -> Result<(), ConversationError> {
        let id = format!("l{line}");
        match str_field(payload, "type") {
            Some("message") => match str_field(payload, "role") {
                Some("user") => {
                    let text = response_user_text(payload.get("content"))?;
                    if self.open.is_some() {
                        self.pending_user.push((line, text));
                    } else {
                        self.rollback_attribution_unknown |= !verified_rollback_context(&text);
                        self.omissions.note("injected_context_excluded");
                    }
                    Ok(())
                }
                Some("developer") | Some("system") => {
                    self.omissions.note("developer_context_excluded");
                    Ok(())
                }
                Some("assistant") => {
                    let text = assistant_text(payload.get("content"))?;
                    if serde_json::from_str::<Value>(&text)
                        .ok()
                        .is_some_and(|value| {
                            str_field(&value, "author").is_some()
                                && str_field(&value, "recipient").is_some()
                                && str_field(&value, "content").is_some()
                                && value.get("trigger_turn").is_some_and(Value::is_boolean)
                        })
                    {
                        self.rollback_attribution_unknown = true;
                        self.omissions.note("inter_agent_excluded");
                        return Ok(());
                    }
                    self.open_turn()?;
                    let phase = match payload.get("phase") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(phase)) if phase == "commentary" => {
                            Some(Phase::Commentary)
                        }
                        Some(Value::String(phase)) if phase == "final_answer" => {
                            Some(Phase::FinalAnswer)
                        }
                        Some(_) => return Err(fail("unknown_assistant_phase")),
                    };
                    let mut message = message(
                        self.source_ref(&id),
                        MessageRole::Assistant,
                        MessageKind::Commentary,
                        "assistant",
                        text,
                    );
                    message.provider_message_id = str_field(payload, "id").map(str::to_owned);
                    self.push(message, phase);
                    Ok(())
                }
                _ => Err(fail("unknown_codex_message_role")),
            },
            Some("reasoning") => {
                self.omissions.note("reasoning_excluded");
                Ok(())
            }
            Some(kind @ ("function_call" | "custom_tool_call")) => {
                self.open_turn()?;
                let call_id =
                    str_field(payload, "call_id").ok_or_else(|| fail("invalid_codex_tool_call"))?;
                let name = str_field(payload, "name").unwrap_or_default();
                let input = if kind == "function_call" {
                    json_text(payload.get("arguments"))
                } else {
                    json_text(payload.get("input"))
                };
                let source_ref = self.source_ref(&id);
                let text =
                    clip_tool_text(format!("{name}\n{input}"), &source_ref, &mut self.omissions);
                let mut message = message(
                    source_ref,
                    MessageRole::Tool,
                    MessageKind::ToolCall,
                    "assistant",
                    text,
                );
                message.tool_call_id = Some(call_id.to_owned());
                message.provider_message_id = str_field(payload, "id").map(str::to_owned);
                self.push(message, None);
                Ok(())
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                self.open_turn()?;
                let call_id = str_field(payload, "call_id")
                    .ok_or_else(|| fail("invalid_codex_tool_result"))?;
                let output = match payload.get("output") {
                    Some(Value::Object(output)) => json_text(output.get("content")),
                    output => json_text(output),
                };
                let source_ref = self.source_ref(&id);
                let text = clip_tool_text(output, &source_ref, &mut self.omissions);
                let mut message = message(
                    source_ref,
                    MessageRole::Tool,
                    MessageKind::ToolResult,
                    "tool",
                    text,
                );
                message.tool_call_id = Some(call_id.to_owned());
                self.push(message, None);
                Ok(())
            }
            Some("agent_message") => {
                self.rollback_attribution_unknown = true;
                self.omissions.note("inter_agent_excluded");
                Ok(())
            }
            Some("configuration_update") => Ok(()),
            _ => Err(fail("unknown_codex_response_item")),
        }
    }

    fn event(&mut self, line: u64, payload: &Value) -> Result<(), ConversationError> {
        match str_field(payload, "type") {
            Some("task_started" | "turn_started") => {
                if self.open.is_some() {
                    return Err(fail("overlapping_turn"));
                }
                let id = str_field(payload, "turn_id")
                    .filter(|id| !id.trim().is_empty())
                    .ok_or_else(|| fail("missing_turn_id"))?;
                if !self.turn_ids.insert(id.to_owned()) {
                    return Err(fail("duplicate_turn"));
                }
                self.turns.push(Turn {
                    index: self.turns.last().map_or(1, |turn| turn.index + 1),
                    id: id.to_owned(),
                    outcome: Outcome::Open,
                    compacted: false,
                    truncated: false,
                });
                self.open = Some(self.turns.len() - 1);
                Ok(())
            }
            Some("task_complete" | "turn_complete") => {
                let outcome = if present(payload.get("error")) {
                    Outcome::Failed
                } else {
                    Outcome::Completed {
                        last_agent_message: str_field(payload, "last_agent_message")
                            .map(str::to_owned),
                    }
                };
                self.close(payload, outcome)
            }
            Some("turn_aborted") => self.close(payload, Outcome::Aborted),
            Some("item_completed") => self.item_completed(line, payload),
            Some("thread_rolled_back") => self.rollback(payload),
            Some(
                "token_count"
                | "thread_settings_applied"
                | "thread_goal_updated"
                | "context_compacted",
            ) => Ok(()),
            _ => Err(fail("unknown_codex_event")),
        }
    }

    fn close(&mut self, payload: &Value, outcome: Outcome) -> Result<(), ConversationError> {
        let index = self.open.ok_or_else(|| fail("turn_end_without_start"))?;
        if str_field(payload, "turn_id") != Some(self.turns[index].id.as_str()) {
            return Err(fail("turn_end_mismatch"));
        }
        self.turns[index].outcome = outcome;
        self.open = None;
        if !self.pending_user.is_empty() {
            self.rollback_attribution_unknown |= self
                .pending_user
                .iter()
                .any(|(_, text)| !verified_rollback_context(text));
            self.pending_user.clear();
            self.omissions.note("injected_context_excluded");
        }
        Ok(())
    }

    fn item_completed(&mut self, line: u64, payload: &Value) -> Result<(), ConversationError> {
        let item = object(payload.get("item"))?;
        match str_field(item, "type") {
            Some("UserMessage") => {}
            Some(
                "FunctionCallOutput"
                | "HookPrompt"
                | "AgentMessage"
                | "Plan"
                | "Reasoning"
                | "CommandExecution"
                | "DynamicToolCall"
                | "CollabAgentToolCall"
                | "SubAgentActivity"
                | "WebSearch"
                | "ImageView"
                | "Extension"
                | "ImageGeneration"
                | "EnteredReviewMode"
                | "ExitedReviewMode"
                | "FileChange"
                | "McpToolCall"
                | "ContextCompaction",
            ) => return Ok(()),
            _ => return Err(fail("unknown_codex_item")),
        }
        let turn = self.open_turn()?;
        if str_field(payload, "turn_id") != Some(turn.id.as_str()) {
            return Err(fail("turn_attribution_unknown"));
        }
        let (text, image) = user_message_text(item.get("content"))?;
        let paired = self
            .pending_user
            .iter()
            .rposition(|(_, pending)| *pending == text)
            .ok_or_else(|| fail("user_message_unpaired"))?;
        // Earlier unpaired texts of this turn are injected context.
        if paired > 0 {
            self.rollback_attribution_unknown |= self.pending_user[..paired]
                .iter()
                .any(|(_, text)| !verified_rollback_context(text));
            self.omissions.note("injected_context_excluded");
        }
        self.pending_user.drain(..=paired);
        if image {
            self.omissions.note("image_excluded");
        }
        let mut message = message(
            self.source_ref(&format!("l{line}")),
            MessageRole::User,
            MessageKind::Instruction,
            "user",
            text,
        );
        message.provider_message_id = str_field(item, "id").map(str::to_owned);
        self.push(message, None);
        Ok(())
    }

    /// Replacement history is the effective model history. Retained user messages are
    /// traced back to their original effective message; untraceable plain text is a summary.
    fn compacted(&mut self, line: u64, payload: &Value) -> Result<(), ConversationError> {
        let history = payload
            .get("replacement_history")
            .and_then(Value::as_array)
            .ok_or_else(|| fail("compaction_without_replacement_history"))?;
        // Retained messages are the most recent ones, so match from the end and never reuse
        // one effective message for two retained copies of the same text.
        let mut traced: Vec<Option<usize>> = vec![None; history.len()];
        let mut used = std::collections::BTreeSet::new();
        for (index, item) in history.iter().enumerate().rev() {
            if str_field(item, "type") != Some("message") || str_field(item, "role") != Some("user")
            {
                continue;
            }
            let text = response_user_text(item.get("content"))?;
            traced[index] = self
                .entries
                .iter()
                .enumerate()
                .rev()
                .find(|(position, entry)| {
                    !used.contains(position)
                        && matches!(
                            entry.message.kind,
                            MessageKind::Instruction | MessageKind::CompactionSummary
                        )
                        && entry.message.text == text
                })
                .map(|(position, _)| position);
            if let Some(found) = traced[index] {
                used.insert(found);
            }
        }
        // The latest replacement summarizes every earlier one.
        self.omissions
            .0
            .retain(|omission| omission.reason != "compaction_summary_unreadable");
        let mut rebuilt = Vec::new();
        for (index, item) in history.iter().enumerate() {
            let id = format!("l{line}#rh{index}");
            match (str_field(item, "type"), str_field(item, "role")) {
                (Some("message"), Some("user")) => {
                    if let Some(found) = traced[index] {
                        let entry = &self.entries[found];
                        rebuilt.push(Entry {
                            message: entry.message.clone(),
                            phase: None,
                        });
                        continue;
                    }
                    self.rollback_attribution_unknown = true;
                    let text = response_user_text(item.get("content"))?;
                    if looks_injected(&text) {
                        self.omissions.note("injected_context_excluded");
                    } else {
                        rebuilt.push(Entry {
                            message: message(
                                self.source_ref(&id),
                                MessageRole::Summary,
                                MessageKind::CompactionSummary,
                                "user",
                                text,
                            ),
                            phase: None,
                        });
                    }
                }
                (Some("message"), Some("developer" | "system")) => {
                    self.omissions.note("developer_context_excluded")
                }
                (Some("compaction"), _) => self
                    .omissions
                    .at("compaction_summary_unreadable", &self.source_ref(&id)),
                (Some("agent_message"), _) => {
                    self.rollback_attribution_unknown = true;
                    self.omissions.note("inter_agent_excluded");
                }
                _ => return Err(fail("unsupported_compaction_item")),
            }
        }
        if let Some(summary) = str_field(payload, "message").filter(|text| !text.is_empty()) {
            rebuilt.push(Entry {
                message: message(
                    self.source_ref(&format!("l{line}")),
                    MessageRole::Summary,
                    MessageKind::CompactionSummary,
                    "summary",
                    summary.to_owned(),
                ),
                phase: None,
            });
        }
        self.entries = rebuilt;
        for (index, turn) in self.turns.iter_mut().enumerate() {
            if Some(index) != self.open {
                turn.compacted = true;
            }
        }
        self.pending_user.clear();
        self.compacted = true;
        self.omissions.note("pre_compaction_excluded");
        Ok(())
    }

    /// Codex drops the last N user messages and everything after the earliest of them.
    fn rollback(&mut self, payload: &Value) -> Result<(), ConversationError> {
        let count = payload
            .get("num_turns")
            .and_then(Value::as_u64)
            .filter(|count| *count <= u32::MAX as u64)
            .ok_or_else(|| fail("invalid_rollback"))?;
        if count == 0 {
            return Ok(());
        }
        // Excluded/unpaired items and untraced replacement messages may count as
        // upstream rollback boundaries; never cut a guessed position.
        if self.rollback_attribution_unknown {
            return Err(fail("rollback_history_unsupported"));
        }
        if self.open.is_some() {
            return Err(fail("rollback_during_turn"));
        }
        let instructions: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.message.kind == MessageKind::Instruction)
            .map(|(index, _)| index)
            .collect();
        if instructions.is_empty() {
            return Ok(());
        }
        let count = usize::try_from(count)
            .unwrap_or(usize::MAX)
            .min(instructions.len());
        let cut = instructions[instructions.len() - count];
        let cut_turn = self.entries[cut].message.turn_index;
        self.entries.truncate(cut);
        let kept_cut_turn = self
            .entries
            .iter()
            .any(|entry| entry.message.turn_index == cut_turn);
        self.turns.retain_mut(|turn| {
            if turn.index == cut_turn && kept_cut_turn {
                turn.truncated = true;
            }
            turn.index < cut_turn || (turn.index == cut_turn && kept_cut_turn)
        });
        self.omissions.note("rolled_back_excluded");
        Ok(())
    }

    fn finish(mut self) -> ParsedConversation {
        if !self.pending_user.is_empty() {
            self.omissions.note("injected_context_excluded");
        }
        // Truncations of tool messages removed by compaction or rollback no longer apply.
        let live: std::collections::BTreeSet<&str> = self
            .entries
            .iter()
            .map(|entry| entry.message.source_ref.as_str())
            .collect();
        self.omissions.0.retain(|omission| {
            omission.reason != "tool_text_truncated"
                || omission
                    .source_ref
                    .as_deref()
                    .is_some_and(|source| live.contains(source))
        });
        let mut turns = Vec::new();
        for (position, turn) in self.turns.iter().enumerate() {
            let members: Vec<usize> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.message.turn_index == turn.index)
                .map(|(index, _)| index)
                .collect();
            if members.is_empty() && Some(position) != self.open {
                continue;
            }
            let candidate = members
                .iter()
                .rev()
                .copied()
                .find(|index| self.entries[*index].message.role == MessageRole::Assistant);
            let (completed, final_index) = match (&turn.outcome, turn.compacted || turn.truncated) {
                (_, true) => (
                    matches!(turn.outcome, Outcome::Completed { .. }) && !turn.truncated,
                    None,
                ),
                (
                    Outcome::Completed {
                        last_agent_message: None,
                    },
                    false,
                ) => (true, None),
                (
                    Outcome::Completed {
                        last_agent_message: Some(last),
                    },
                    false,
                ) => match candidate.filter(|index| self.entries[*index].message.text == *last) {
                    Some(index) => (true, Some(index)),
                    None => {
                        let source =
                            candidate.map(|index| self.entries[index].message.source_ref.clone());
                        match source {
                            Some(source) => self.omissions.at("final_message_unverified", &source),
                            None => self.omissions.note("final_message_unverified"),
                        }
                        (false, None)
                    }
                },
                (Outcome::Open, false) => (
                    false,
                    candidate
                        .filter(|index| self.entries[*index].phase == Some(Phase::FinalAnswer)),
                ),
                (Outcome::Failed | Outcome::Aborted, false) => (false, None),
            };
            if let Some(index) = final_index {
                self.entries[index].message.kind = MessageKind::Final;
            }
            turns.push(ConversationTurn {
                turn_index: turn.index,
                provider_turn_id: Some(turn.id.clone()),
                user_source_refs: members
                    .iter()
                    .filter(|index| self.entries[**index].message.kind == MessageKind::Instruction)
                    .map(|index| self.entries[*index].message.source_ref.clone())
                    .collect(),
                final_source_ref: final_index
                    .map(|index| self.entries[index].message.source_ref.clone()),
                completed,
            });
        }
        let messages = self
            .entries
            .into_iter()
            .enumerate()
            .map(|(order, entry)| ConversationMessage {
                order: order as u64 + 1,
                ..entry.message
            })
            .collect();
        ParsedConversation {
            messages,
            turns,
            omissions: self.omissions.0,
            compacted: self.compacted,
            partial_record: false,
        }
    }
}

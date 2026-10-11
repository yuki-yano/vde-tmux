//! Claude Code transcript normalization (structure verified on 2.1.287).
//!
//! The effective branch is the parent chain of the last user/assistant record. Off-chain
//! subtrees are kept only when they contain no conversation turn of their own, such as the
//! result of a parallel tool call; a sibling containing a prompt or reply is an abandoned
//! branch. Only the latest compaction boundary and the history after it are effective.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::codex::{Omissions, clip_tool_text, fail, message};
use super::*;

const PROVIDER: &str = "claude";
const CHAIN_TYPES: [&str; 4] = ["user", "assistant", "system", "attachment"];

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

struct Node<'a> {
    line: u64,
    uuid: &'a str,
    parent: Option<&'a str>,
    kind: &'a str,
    value: &'a Value,
}

enum UserClass {
    Meta,
    CompactSummary(String),
    ToolResults,
    Excluded(&'static str),
    Interrupt,
    Instruction {
        text: String,
        image: bool,
        reminder: bool,
    },
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(text[start..end].trim())
}

fn user_text(content: &Value) -> Result<(String, bool, bool), ConversationError> {
    match content {
        Value::String(text) => Ok((text.clone(), false, false)),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            let (mut image, mut reminder) = (false, false);
            for block in blocks {
                match str_field(block, "type") {
                    Some("text") => {
                        let text = str_field(block, "text")
                            .ok_or_else(|| fail("invalid_claude_content"))?;
                        if text.trim_start().starts_with("<system-reminder>") {
                            reminder = true;
                        } else {
                            parts.push(text);
                        }
                    }
                    Some("image") => image = true,
                    _ => return Err(fail("unknown_claude_content")),
                }
            }
            Ok((parts.join("\n"), image, reminder))
        }
        _ => Err(fail("invalid_claude_content")),
    }
}

fn classify_user(value: &Value) -> Result<UserClass, ConversationError> {
    if flag(value, "isMeta") {
        return Ok(UserClass::Meta);
    }
    let content = value
        .get("message")
        .and_then(|message| message.get("content"))
        .ok_or_else(|| fail("invalid_claude_record"))?;
    if flag(value, "isCompactSummary") {
        return Ok(UserClass::CompactSummary(user_text(content)?.0));
    }
    if let Value::Array(blocks) = content
        && blocks
            .iter()
            .any(|block| str_field(block, "type") == Some("tool_result"))
    {
        if !blocks
            .iter()
            .all(|block| str_field(block, "type") == Some("tool_result"))
        {
            return Err(fail("mixed_tool_result_content"));
        }
        return Ok(UserClass::ToolResults);
    }
    let (text, image, reminder) = user_text(content)?;
    let head = text.trim_start();
    for (prefix, reason) in [
        ("<task-notification>", "notification_excluded"),
        ("<local-command-stdout>", "local_command_excluded"),
        ("<local-command-stderr>", "local_command_excluded"),
        ("<local-command-caveat>", "local_command_excluded"),
        ("<bash-input>", "bash_mode_excluded"),
        ("<bash-stdout>", "bash_mode_excluded"),
        ("<bash-stderr>", "bash_mode_excluded"),
    ] {
        if head.starts_with(prefix) {
            return Ok(UserClass::Excluded(reason));
        }
    }
    if head.starts_with("[Request interrupted by user") {
        return Ok(UserClass::Interrupt);
    }
    if head.starts_with("<command-name>") || head.starts_with("<command-message>") {
        // The hook prompt of a slash command is `/name args`; keep that user-facing form.
        let name = tag(head, "command-name")
            .filter(|name| !name.is_empty())
            .ok_or_else(|| fail("invalid_command_record"))?;
        let text = match tag(head, "command-args").filter(|args| !args.is_empty()) {
            Some(args) => format!("{name} {args}"),
            None => name.to_owned(),
        };
        return Ok(UserClass::Instruction {
            text,
            image,
            reminder,
        });
    }
    match value.get("origin") {
        None | Some(Value::Null) => {}
        Some(origin) => match str_field(origin, "kind") {
            Some("human") => {}
            Some("task-notification") => return Ok(UserClass::Excluded("notification_excluded")),
            Some("auto-continuation") => {
                return Ok(UserClass::Excluded("auto_continuation_excluded"));
            }
            _ => return Err(fail("unknown_prompt_origin")),
        },
    }
    Ok(UserClass::Instruction {
        text,
        image,
        reminder,
    })
}

/// Whether a record would open or continue a conversation turn on its own.
fn conversational(node: &Node<'_>) -> Result<bool, ConversationError> {
    Ok(match node.kind {
        "assistant" => true,
        "user" => matches!(classify_user(node.value)?, UserClass::Instruction { .. }),
        _ => queued_prompt(node),
    })
}

fn queued_prompt(node: &Node<'_>) -> bool {
    let attachment = node.value.get("attachment");
    node.kind == "attachment"
        && attachment.and_then(|value| str_field(value, "type")) == Some("queued_command")
        && attachment.and_then(|value| str_field(value, "commandMode")) == Some("prompt")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    None,
    Assistant { stop: Stop, text: Option<usize> },
    Stopped,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    EndTurn,
    /// Not written yet; the text is only a candidate for the API to confirm.
    Missing,
    Other,
}

struct Turn {
    index: u64,
    prompt_id: Option<String>,
    tail: Tail,
}

struct Builder<'a> {
    session: &'a str,
    messages: Vec<ConversationMessage>,
    turns: Vec<Turn>,
    texts: BTreeMap<(u64, String), usize>,
    omissions: Omissions,
}

impl Builder<'_> {
    fn source_ref(&self, id: &str) -> String {
        source_ref(PROVIDER, self.session, id)
    }

    fn current(&self) -> (u64, Option<String>) {
        self.turns
            .last()
            .map_or((0, None), |turn| (turn.index, turn.prompt_id.clone()))
    }

    fn push(&mut self, mut message: ConversationMessage) -> usize {
        let (index, prompt_id) = self.current();
        message.turn_index = index;
        message.provider_turn_id = prompt_id;
        self.messages.push(message);
        self.messages.len() - 1
    }

    fn set_tail(&mut self, tail: Tail) {
        if let Some(turn) = self.turns.last_mut() {
            turn.tail = tail;
        }
    }

    fn instruction(&mut self, node: &Node<'_>, text: String, new_turn: bool) {
        if new_turn || self.turns.is_empty() {
            self.turns.push(Turn {
                index: self.turns.last().map_or(1, |turn| turn.index + 1),
                prompt_id: str_field(node.value, "promptId").map(str::to_owned),
                tail: Tail::None,
            });
        }
        let message = message(
            self.source_ref(node.uuid),
            MessageRole::User,
            MessageKind::Instruction,
            "user",
            text,
        );
        self.push(message);
    }

    fn user(&mut self, node: &Node<'_>) -> Result<(), ConversationError> {
        match classify_user(node.value)? {
            UserClass::Meta => self.omissions.note("meta_excluded"),
            UserClass::Excluded(reason) => self.omissions.note(reason),
            UserClass::Interrupt => {
                self.omissions.note("interrupt_marker_excluded");
                self.set_tail(Tail::Stopped);
            }
            UserClass::CompactSummary(text) => {
                let message = message(
                    self.source_ref(node.uuid),
                    MessageRole::Summary,
                    MessageKind::CompactionSummary,
                    "user",
                    text,
                );
                self.messages.push(message);
            }
            UserClass::Instruction {
                text,
                image,
                reminder,
            } => {
                if image {
                    self.omissions.note("image_excluded");
                }
                if reminder {
                    self.omissions.note("system_reminder_excluded");
                }
                self.instruction(node, text, true);
            }
            UserClass::ToolResults => {
                let blocks = node.value["message"]["content"]
                    .as_array()
                    .expect("tool results are an array");
                for (position, block) in blocks.iter().enumerate() {
                    let id = if blocks.len() > 1 {
                        format!("{}#b{position}", node.uuid)
                    } else {
                        node.uuid.to_owned()
                    };
                    let text = match block.get("content") {
                        None | Some(Value::Null) => String::new(),
                        Some(Value::String(text)) => text.clone(),
                        Some(Value::Array(parts)) => {
                            let mut texts = Vec::new();
                            for part in parts {
                                match str_field(part, "type") {
                                    Some("text") => {
                                        texts.push(str_field(part, "text").unwrap_or_default())
                                    }
                                    Some("image") => {
                                        self.omissions.note("tool_result_image_excluded")
                                    }
                                    Some("tool_reference") => {
                                        self.omissions.note("tool_reference_excluded")
                                    }
                                    _ => return Err(fail("unknown_claude_content")),
                                }
                            }
                            texts.join("\n")
                        }
                        Some(_) => return Err(fail("invalid_claude_content")),
                    };
                    let source_ref = self.source_ref(&id);
                    let text = clip_tool_text(text, &source_ref, &mut self.omissions);
                    let mut message = message(
                        source_ref,
                        MessageRole::Tool,
                        MessageKind::ToolResult,
                        "user",
                        text,
                    );
                    message.tool_call_id = str_field(block, "tool_use_id").map(str::to_owned);
                    self.push(message);
                }
            }
        }
        Ok(())
    }

    fn assistant(&mut self, node: &Node<'_>) -> Result<(), ConversationError> {
        if flag(node.value, "isApiErrorMessage") {
            self.omissions.note("api_error_excluded");
            self.set_tail(Tail::Stopped);
            return Ok(());
        }
        let message_value = node
            .value
            .get("message")
            .filter(|value| value.is_object())
            .ok_or_else(|| fail("invalid_claude_record"))?;
        let message_id = str_field(message_value, "id")
            .unwrap_or(node.uuid)
            .to_owned();
        let blocks = message_value
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| fail("invalid_claude_content"))?;
        let turn = self.current().0;
        for (position, block) in blocks.iter().enumerate() {
            let id = if blocks.len() > 1 {
                format!("{}#b{position}", node.uuid)
            } else {
                node.uuid.to_owned()
            };
            match str_field(block, "type") {
                Some("text") => {
                    let text =
                        str_field(block, "text").ok_or_else(|| fail("invalid_claude_content"))?;
                    // One API message is written as one record per content block.
                    match self.texts.get(&(turn, message_id.clone())) {
                        Some(&index) => {
                            let existing = &mut self.messages[index];
                            existing.text.push('\n');
                            existing.text.push_str(text);
                            existing.text_digest = digest(existing.text.as_bytes());
                        }
                        None => {
                            let mut message = message(
                                self.source_ref(&id),
                                MessageRole::Assistant,
                                MessageKind::Commentary,
                                "assistant",
                                text.to_owned(),
                            );
                            message.provider_message_id = Some(message_id.clone());
                            let index = self.push(message);
                            self.texts.insert((turn, message_id.clone()), index);
                        }
                    }
                }
                Some("thinking" | "redacted_thinking") => self.omissions.note("reasoning_excluded"),
                Some("tool_use" | "server_tool_use") => {
                    let name = str_field(block, "name").unwrap_or_default();
                    let input = block.get("input").map(Value::to_string).unwrap_or_default();
                    let source_ref = self.source_ref(&id);
                    let text = clip_tool_text(
                        format!("{name}\n{input}"),
                        &source_ref,
                        &mut self.omissions,
                    );
                    let mut message = message(
                        source_ref,
                        MessageRole::Tool,
                        MessageKind::ToolCall,
                        "assistant",
                        text,
                    );
                    message.tool_call_id = str_field(block, "id").map(str::to_owned);
                    message.provider_message_id = Some(message_id.clone());
                    self.push(message);
                }
                _ => return Err(fail("unknown_claude_content")),
            }
        }
        let stop = match message_value.get("stop_reason") {
            Some(Value::String(reason)) if reason == "end_turn" => Stop::EndTurn,
            None | Some(Value::Null) => Stop::Missing,
            Some(_) => Stop::Other,
        };
        // A candidate final must be the latest content, not text followed by a tool call.
        let ends_with_text = blocks
            .last()
            .is_some_and(|block| str_field(block, "type") == Some("text"));
        let text = self
            .texts
            .get(&(turn, message_id))
            .copied()
            .filter(|_| ends_with_text || stop == Stop::EndTurn);
        self.set_tail(Tail::Assistant { stop, text });
        Ok(())
    }

    fn attachment(
        &mut self,
        node: &Node<'_>,
        indexed: &BTreeMap<&str, usize>,
    ) -> Result<(), ConversationError> {
        let attachment = node
            .value
            .get("attachment")
            .ok_or_else(|| fail("invalid_claude_record"))?;
        if str_field(attachment, "type") != Some("queued_command") {
            self.omissions.note("attachment_excluded");
            return Ok(());
        }
        match str_field(attachment, "commandMode") {
            Some("prompt") => {
                if str_field(attachment, "source_uuid")
                    .is_some_and(|uuid| indexed.contains_key(uuid))
                {
                    self.omissions.note("duplicate_record_excluded");
                    return Ok(());
                }
                let (text, image, reminder) = user_text(
                    attachment
                        .get("prompt")
                        .ok_or_else(|| fail("invalid_claude_record"))?,
                )?;
                if image {
                    self.omissions.note("image_excluded");
                }
                if reminder {
                    self.omissions.note("system_reminder_excluded");
                }
                // A prompt queued while a turn runs is delivered into that turn.
                self.instruction(node, text, false);
            }
            Some("task-notification") => self.omissions.note("notification_excluded"),
            _ => return Err(fail("unknown_queued_command_mode")),
        }
        Ok(())
    }

    fn finish(mut self, compacted: bool) -> ParsedConversation {
        let mut turns = Vec::new();
        for turn in &self.turns {
            let (completed, final_index) = match turn.tail {
                Tail::Assistant {
                    stop: Stop::EndTurn,
                    text,
                } => (true, text),
                Tail::Assistant {
                    stop: Stop::Missing,
                    text: Some(index),
                } => (false, Some(index)),
                _ => (false, None),
            };
            if let Some(index) = final_index {
                self.messages[index].kind = MessageKind::Final;
            }
            turns.push(ConversationTurn {
                turn_index: turn.index,
                provider_turn_id: turn.prompt_id.clone(),
                user_source_refs: self
                    .messages
                    .iter()
                    .filter(|message| {
                        message.turn_index == turn.index && message.kind == MessageKind::Instruction
                    })
                    .map(|message| message.source_ref.clone())
                    .collect(),
                final_source_ref: final_index.map(|index| self.messages[index].source_ref.clone()),
                completed,
            });
        }
        for (order, message) in self.messages.iter_mut().enumerate() {
            message.order = order as u64 + 1;
        }
        ParsedConversation {
            messages: self.messages,
            turns,
            omissions: self.omissions.0,
            compacted,
            partial_record: false,
        }
    }
}

fn parent_index(
    nodes: &[Node<'_>],
    indexed: &BTreeMap<&str, usize>,
    index: usize,
) -> Result<Option<usize>, ConversationError> {
    match nodes[index].parent {
        None => Ok(None),
        Some(parent) => indexed
            .get(parent)
            .copied()
            .map(Some)
            .ok_or_else(|| fail("broken_chain")),
    }
}

/// Root-to-`start` chain, stopping after `stop` when given.
fn chain(
    nodes: &[Node<'_>],
    indexed: &BTreeMap<&str, usize>,
    start: usize,
    stop: Option<usize>,
) -> Result<Vec<usize>, ConversationError> {
    let mut path = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = Some(start);
    while let Some(index) = current {
        if !seen.insert(index) {
            return Err(fail("cyclic_chain"));
        }
        path.push(index);
        if Some(index) == stop {
            break;
        }
        current = parent_index(nodes, indexed, index)?;
    }
    if stop.is_some_and(|stop| path.last() != Some(&stop)) {
        return Err(fail("preserved_segment_unresolved"));
    }
    path.reverse();
    Ok(path)
}

fn preserved<'a>(node: &Node<'a>) -> (Option<&'a str>, Option<&'a str>, Option<&'a str>) {
    let segment = node
        .value
        .get("compactMetadata")
        .and_then(|metadata| metadata.get("preservedSegment"));
    let field = |key| segment.and_then(|segment| str_field(segment, key));
    (field("headUuid"), field("tailUuid"), field("anchorUuid"))
}

pub(super) fn parse(
    session: &str,
    records: &[Value],
) -> Result<ParsedConversation, ConversationError> {
    let mut omissions = Omissions::default();
    let mut nodes: Vec<Node<'_>> = Vec::new();
    let mut indexed: BTreeMap<&str, usize> = BTreeMap::new();
    let mut leaf_hint = None;
    for (position, record) in records.iter().enumerate() {
        let kind = str_field(record, "type").ok_or_else(|| fail("unknown_claude_record"))?;
        if record
            .get("sessionId")
            .is_some_and(|id| id.as_str() != Some(session))
        {
            return Err(fail("foreign_session_record"));
        }
        match kind {
            "continued-in" => return Err(fail("session_continued_elsewhere")),
            "last-prompt" => {
                leaf_hint = str_field(record, "leafUuid");
                continue;
            }
            kind if CHAIN_TYPES.contains(&kind) => {}
            // Metadata records (mode, file history, titles, costs...) carry no conversation.
            _ if record.get("uuid").is_some() || record.get("message").is_some() => {
                return Err(fail("unknown_claude_record"));
            }
            _ => continue,
        }
        if flag(record, "isSidechain") {
            omissions.note("subagent_excluded");
            continue;
        }
        let uuid = str_field(record, "uuid")
            .filter(|uuid| !uuid.is_empty())
            .ok_or_else(|| fail("invalid_claude_record"))?;
        let parent = match record.get("parentUuid") {
            None | Some(Value::Null) => None,
            Some(Value::String(parent)) => Some(parent.as_str()),
            Some(_) => return Err(fail("invalid_claude_record")),
        };
        if let Some(&existing) = indexed.get(uuid) {
            if nodes[existing].value == record {
                omissions.note("duplicate_record_excluded");
                continue;
            }
            return Err(fail("conflicting_duplicate_record"));
        }
        indexed.insert(uuid, nodes.len());
        nodes.push(Node {
            line: position as u64 + 1,
            uuid,
            parent,
            kind,
            value: record,
        });
    }
    let mut builder = Builder {
        session,
        messages: Vec::new(),
        turns: Vec::new(),
        texts: BTreeMap::new(),
        omissions,
    };
    // A prompt queued after the latest reply is newer conversation, not a side record.
    let Some(leaf) = nodes
        .iter()
        .rposition(|node| matches!(node.kind, "user" | "assistant") || queued_prompt(node))
    else {
        return Ok(builder.finish(false));
    };
    let path = chain(&nodes, &indexed, leaf, None)?;
    let on_path: BTreeSet<usize> = path.iter().copied().collect();
    if let Some(&hint) = leaf_hint.and_then(|hint| indexed.get(hint))
        && !on_path.contains(&hint)
        && !chain(&nodes, &indexed, hint, None)?.contains(&leaf)
    {
        return Err(fail("ambiguous_leaf"));
    }

    // Only the latest boundary that the effective chain passes through matters.
    let boundary = nodes.iter().enumerate().rev().find(|(index, node)| {
        node.kind == "system"
            && str_field(node.value, "subtype") == Some("compact_boundary")
            && (on_path.contains(index)
                || preserved(node)
                    .0
                    .and_then(|head| indexed.get(head))
                    .is_some_and(|head| on_path.contains(head)))
    });
    let mut effective = path.clone();
    let compacted = boundary.is_some();
    if let Some((boundary_index, boundary_node)) = boundary {
        builder.omissions.note("pre_compaction_excluded");
        let (head, tail, anchor) = preserved(boundary_node);
        let head = head
            .map(|head| {
                indexed
                    .get(head)
                    .copied()
                    .ok_or_else(|| fail("preserved_segment_unresolved"))
            })
            .transpose()?;
        if let Some(position) = path.iter().position(|index| *index == boundary_index) {
            effective = path[position + 1..].to_vec();
            if let Some(head) = head.filter(|head| !on_path.contains(head)) {
                let tail = tail
                    .and_then(|tail| indexed.get(tail).copied())
                    .ok_or_else(|| fail("preserved_segment_unresolved"))?;
                let segment = chain(&nodes, &indexed, tail, Some(head))?;
                if segment.iter().any(|index| effective.contains(index)) {
                    return Err(fail("preserved_segment_unresolved"));
                }
                let insert_at = anchor
                    .and_then(|anchor| indexed.get(anchor))
                    .and_then(|anchor| effective.iter().position(|index| index == anchor))
                    .map_or(0, |position| position + 1);
                effective.splice(insert_at..insert_at, segment);
            }
        } else {
            let head = head.expect("boundary selected through its preserved head");
            let position = path
                .iter()
                .position(|index| *index == head)
                .expect("head is on the path");
            effective = path[position..].to_vec();
            if let Some(summary) = nodes.iter().position(|node| {
                node.parent == Some(boundary_node.uuid) && flag(node.value, "isCompactSummary")
            }) {
                effective.insert(0, summary);
            }
        }
    }

    let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, node) in nodes.iter().enumerate() {
        if let Some(parent) = node.parent.and_then(|parent| indexed.get(parent)) {
            children.entry(*parent).or_default().push(index);
        }
    }
    let effective_set: BTreeSet<usize> = effective.iter().copied().collect();
    let mut ordered = Vec::new();
    for &index in &effective {
        ordered.push(index);
        for &child in children.get(&index).into_iter().flatten() {
            if effective_set.contains(&child) {
                continue;
            }
            let mut subtree = Vec::new();
            let mut stack = vec![child];
            while let Some(next) = stack.pop() {
                if effective_set.contains(&next) || subtree.contains(&next) {
                    continue;
                }
                subtree.push(next);
                stack.extend(children.get(&next).into_iter().flatten().copied());
            }
            let mut side = true;
            for member in &subtree {
                if conversational(&nodes[*member])? {
                    side = false;
                    break;
                }
            }
            if side {
                subtree.sort_by_key(|member| nodes[*member].line);
                ordered.extend(subtree);
            } else {
                builder.omissions.note("abandoned_branch_excluded");
            }
        }
    }

    for index in ordered {
        let node = &nodes[index];
        match node.kind {
            "user" => builder.user(node)?,
            "assistant" => builder.assistant(node)?,
            "attachment" => builder.attachment(node, &indexed)?,
            _ => builder.omissions.note("system_excluded"),
        }
    }
    Ok(builder.finish(compacted))
}

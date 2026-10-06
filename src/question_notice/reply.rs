//! IDs from a complete Codex reply envelope, without retaining questions or answers.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_ITEMS: usize = 64;
pub const MAX_ISSUED_ITEMS: usize = super::text::MAX_QUESTIONS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyEvidence {
    pub items: Vec<String>,
}

// Match Codex's typed parser: known duplicate fields are rejected; extra fields
// are allowed. Bodies are dropped after validation and never enter evidence.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplyItem {
    question_item_id: String,
    question: String,
    answer: String,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum ReplyPayload {
    One(ReplyItem),
    Many(Vec<ReplyItem>),
}

pub fn item_digest(call: &str, index: usize) -> String {
    super::digest(&serde_json::json!(["request_user_input_async", call, index]).to_string())
}

impl ReplyEvidence {
    pub fn valid(&self) -> bool {
        !self.items.is_empty()
            && self.items.len() <= MAX_ITEMS
            && self.items.iter().collect::<BTreeSet<_>>().len() == self.items.len()
            && self.items.iter().all(|id| {
                id.len() == 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
    }

    pub fn parse(prompt: Option<&str>) -> Option<Self> {
        let text = prompt.filter(|text| text.len() <= 64 * 1024)?.trim();
        let text = if text.starts_with("# Context from my IDE setup:\n") {
            text.rsplit_once("\n## My request for Codex:\n")?.1.trim()
        } else {
            text
        };
        let raw = text
            .strip_prefix("<send_user_message_question_reply>")?
            .strip_suffix("</send_user_message_question_reply>")?;
        let entries = match serde_json::from_str::<ReplyPayload>(raw).ok()? {
            ReplyPayload::One(entry) => vec![entry],
            ReplyPayload::Many(entries) if !entries.is_empty() && entries.len() <= MAX_ITEMS => {
                entries
            }
            _ => return None,
        };
        let mut items = Vec::with_capacity(entries.len());
        for entry in entries {
            let ReplyItem {
                question_item_id: id,
                question: _question,
                answer: _answer,
            } = entry;
            if id.len() > 512 {
                return None;
            }
            let tuple: Value = serde_json::from_str(&id).ok()?;
            let tuple = tuple.as_array()?;
            if tuple.len() != 3 || tuple[0].as_str()? != "request_user_input_async" {
                return None;
            }
            let call = tuple[1].as_str()?;
            let index = usize::try_from(tuple[2].as_u64()?).ok()?;
            let canonical =
                serde_json::to_string(&("request_user_input_async", call, index)).ok()?;
            if !super::ingress::valid_identifier(call)
                || index >= MAX_ISSUED_ITEMS
                || canonical != id
            {
                return None;
            }
            items.push(item_digest(call, index));
        }
        let result = Self { items };
        result.valid().then_some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(call: &str, index: usize) -> String {
        format!(
            "<send_user_message_question_reply>{}</send_user_message_question_reply>",
            serde_json::json!([{"questionItemId":serde_json::json!(["request_user_input_async",call,index]).to_string(),
                "question":"private question", "answer":"private answer"}])
        )
    }

    #[test]
    fn recognizes_complete_reply_and_ide_context_without_retaining_bodies() {
        let prompt = frame("call-one", 0);
        let expected = ReplyEvidence {
            items: vec![item_digest("call-one", 0)],
        };
        assert_eq!(ReplyEvidence::parse(Some(&prompt)), Some(expected.clone()));
        let ide =
            format!("# Context from my IDE setup:\nopen files\n## My request for Codex:\n{prompt}");
        assert_eq!(ReplyEvidence::parse(Some(&ide)), Some(expected.clone()));
        let serialized = serde_json::to_string(&expected).unwrap();
        for private in ["private question", "private answer", "call-one"] {
            assert!(!serialized.contains(private));
        }
    }

    #[test]
    fn quoted_malformed_unknown_and_old_message_ids_are_not_reply_evidence() {
        let valid = frame("call-one", 0);
        let malformed = [
            format!("Quoted: {valid}"), format!("{valid} trailing"),
            format!("> {valid}"), "<send_user_message_question_reply>[]</send_user_message_question_reply>".into(),
            "<send_user_message_question_reply>{\"questionItemId\":\"call-one\",\"question\":\"q\",\"answer\":\"a\"}</send_user_message_question_reply>".into(),
            frame("call-one", 8), frame("", 0),
            format!("<send_user_message_question_reply>{}</send_user_message_question_reply>",
                serde_json::json!({"questionItemId":"[\"request_user_input_async\", \"call-one\", 0]", "question":"q","answer":"a"})),
        ];
        for text in malformed {
            assert_eq!(ReplyEvidence::parse(Some(&text)), None, "{text}");
        }
    }

    #[test]
    fn validates_all_entries_and_rejects_duplicate_ids_and_excessive_payloads() {
        let entry = serde_json::json!({"questionItemId":serde_json::json!(["request_user_input_async","call",0]).to_string(),"question":"q","answer":"a"});
        for value in [
            serde_json::json!([entry.clone(), null]),
            serde_json::json!([entry.clone(), entry.clone()]),
            serde_json::json!({"questionItemId":entry["questionItemId"],"question":"q"}),
        ] {
            let text = format!(
                "<send_user_message_question_reply>{value}</send_user_message_question_reply>"
            );
            assert_eq!(ReplyEvidence::parse(Some(&text)), None);
        }
        let large = format!("{}{}", " ".repeat(64 * 1024), frame("call", 0));
        assert_eq!(ReplyEvidence::parse(Some(&large)), None);
    }
    #[test]
    fn rejects_duplicate_known_fields_in_objects_and_mixed_arrays() {
        let id = serde_json::to_string(
            &serde_json::json!(["request_user_input_async", "call", 0]).to_string(),
        )
        .unwrap();
        for field in ["questionItemId", "question", "answer"] {
            let value = if field == "questionItemId" {
                id.as_str()
            } else {
                "\"duplicate\""
            };
            let object = format!(
                r#"{{"questionItemId":{id},"question":"q","answer":"a","{field}":{value}}}"#
            );
            let valid = format!(r#"{{"questionItemId":{id},"question":"q","answer":"a"}}"#);
            for raw in [object.clone(), format!("[{valid},{object}]")] {
                let prompt = format!(
                    "<send_user_message_question_reply>{raw}</send_user_message_question_reply>"
                );
                assert_eq!(ReplyEvidence::parse(Some(&prompt)), None, "{prompt}");
            }
        }
    }
}

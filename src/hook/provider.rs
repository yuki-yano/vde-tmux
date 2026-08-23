use anyhow::{Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::pane_state::{
    AgentKind, AgentSessionId, EventId, IDENTIFIER_MAX_BYTES, ModelError, PromptState,
    validate_required_text,
};

pub const RESPONSE_ARTIFACT_BODY_MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderHookKind {
    SessionStart,
    UserPromptSubmit,
    Activity,
    Waiting,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderCompleteness {
    Complete,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseCandidate {
    pub original_bytes: u64,
    pub original_digest: String,
    pub stored_bytes: u64,
    pub stored_digest: String,
    pub body_base64: String,
    pub truncated: bool,
    pub provider_completeness: ProviderCompleteness,
}

impl ResponseCandidate {
    pub fn from_body(body: &str, completeness: ProviderCompleteness) -> Self {
        let original = body.as_bytes();
        let stored = utf8_suffix(body, RESPONSE_ARTIFACT_BODY_MAX_BYTES).as_bytes();
        Self {
            original_bytes: original.len() as u64,
            original_digest: sha256_hex(original),
            stored_bytes: stored.len() as u64,
            stored_digest: sha256_hex(stored),
            body_base64: base64::engine::general_purpose::STANDARD.encode(stored),
            truncated: stored.len() != original.len(),
            provider_completeness: completeness,
        }
    }

    pub fn decode_body(&self) -> Result<Vec<u8>> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&self.body_base64)
            .map_err(|error| anyhow::anyhow!("invalid response body base64: {error}"))?;
        if decoded.len() as u64 != self.stored_bytes || sha256_hex(&decoded) != self.stored_digest {
            bail!("response candidate body does not match its metadata");
        }
        std::str::from_utf8(&decoded)
            .map_err(|error| anyhow::anyhow!("response candidate body is not UTF-8: {error}"))?;
        Ok(decoded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderObservation {
    pub ingress_request_id: EventId,
    pub provider: AgentKind,
    pub session_id: AgentSessionId,
    pub hook_kind: ProviderHookKind,
    pub provider_turn_key: Option<String>,
    pub provider_event_ref: Option<String>,
    pub payload_digest: String,
    pub prompt_digest: Option<String>,
    pub response: Option<ResponseCandidate>,
    pub observed_at: i64,
}

impl ProviderObservation {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.observed_at < 0 {
            return Err(ModelError(
                "provider observation timestamp must not be negative".to_string(),
            ));
        }
        if self.provider.as_str() == "codex"
            && self.hook_kind != ProviderHookKind::SessionStart
            && self.provider_turn_key.is_none()
        {
            return Err(ModelError(
                "Codex durable lifecycle hook requires turn_id".to_string(),
            ));
        }
        if let Some(key) = &self.provider_turn_key {
            validate_required_text(key, "provider turn key", IDENTIFIER_MAX_BYTES)?;
        }
        for (value, label) in [
            (self.payload_digest.as_str(), "provider payload digest"),
            (
                self.provider_event_ref.as_deref().unwrap_or(""),
                "provider event reference",
            ),
            (
                self.prompt_digest.as_deref().unwrap_or(""),
                "provider prompt digest",
            ),
        ] {
            if !value.is_empty()
                && (value.len() != 64
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
            {
                return Err(ModelError(format!(
                    "{label} must be 64 lowercase hexadecimal characters"
                )));
            }
        }
        if self.hook_kind == ProviderHookKind::UserPromptSubmit && self.prompt_digest.is_none() {
            return Err(ModelError(
                "user prompt observation requires a prompt digest".to_string(),
            ));
        }
        if self.hook_kind != ProviderHookKind::Stop && self.response.is_some() {
            return Err(ModelError(
                "only a Stop observation may carry a response candidate".to_string(),
            ));
        }
        if let Some(response) = &self.response {
            response
                .decode_body()
                .map_err(|error| ModelError(error.to_string()))?;
            if response.stored_bytes > RESPONSE_ARTIFACT_BODY_MAX_BYTES as u64 {
                return Err(ModelError(
                    "response candidate exceeds the stored body limit".to_string(),
                ));
            }
            if response.truncated != (response.stored_bytes != response.original_bytes) {
                return Err(ModelError(
                    "response candidate truncation metadata is inconsistent".to_string(),
                ));
            }
        }
        Ok(())
    }
}

pub fn observation_from_json(
    provider: &str,
    requested_event: &str,
    raw_json: &str,
    ingress_request_id: EventId,
    observed_at: i64,
) -> Result<Option<ProviderObservation>> {
    let payload: Value = serde_json::from_str(raw_json.trim())?;
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or(requested_event);
    let hook_kind = match (provider, event) {
        ("claude" | "codex", "SessionStart") => ProviderHookKind::SessionStart,
        ("claude" | "codex", "UserPromptSubmit") => ProviderHookKind::UserPromptSubmit,
        ("claude", "PreToolUse" | "PostToolUse") | ("codex", "PreToolUse" | "PostToolUse") => {
            ProviderHookKind::Activity
        }
        ("claude", "Notification")
            if payload.get("notification_type").and_then(Value::as_str)
                == Some("permission_prompt") =>
        {
            ProviderHookKind::Waiting
        }
        ("codex", "PermissionRequest") => ProviderHookKind::Waiting,
        ("claude" | "codex", "Stop") => ProviderHookKind::Stop,
        _ => return Ok(None),
    };
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("InvalidRequest: hook payload requires session_id"))?;
    let provider_turn_key = match provider {
        "claude" => payload.get("prompt_id"),
        "codex" => payload.get("turn_id"),
        _ => bail!("unsupported provider {provider}"),
    }
    .and_then(Value::as_str)
    .filter(|value| !value.trim().is_empty())
    .map(str::to_string);
    if provider == "codex"
        && hook_kind != ProviderHookKind::SessionStart
        && provider_turn_key.is_none()
    {
        bail!("InvalidRequest: Codex durable lifecycle hook requires turn_id");
    }
    let prompt = (hook_kind == ProviderHookKind::UserPromptSubmit)
        .then(|| payload.get("prompt").and_then(Value::as_str))
        .flatten();
    let prompt_digest = prompt.map(PromptState::digest_decoded_prompt);
    let response = (hook_kind == ProviderHookKind::Stop)
        .then(|| {
            payload
                .get("last_assistant_message")
                .and_then(Value::as_str)
        })
        .flatten()
        .map(|body| ResponseCandidate::from_body(body, ProviderCompleteness::Complete));
    let provider_event_ref = match hook_kind {
        ProviderHookKind::UserPromptSubmit => Some(ingress_event_reference(
            provider,
            session_id,
            hook_kind,
            &ingress_request_id,
        )),
        ProviderHookKind::Stop => provider_turn_key
            .as_deref()
            .map(|turn| turn_event_reference(provider, session_id, hook_kind, turn)),
        _ => None,
    };
    let payload_digest = observation_payload_digest(
        provider,
        session_id,
        hook_kind,
        provider_turn_key.as_deref(),
        prompt_digest.as_deref(),
        response
            .as_ref()
            .map(|value| value.original_digest.as_str()),
    );
    let observation = ProviderObservation {
        ingress_request_id,
        provider: AgentKind::parse(provider)?,
        session_id: AgentSessionId::parse(session_id)?,
        hook_kind,
        provider_turn_key,
        provider_event_ref,
        payload_digest,
        prompt_digest,
        response,
        observed_at,
    };
    observation.validate()?;
    Ok(Some(observation))
}

fn turn_event_reference(
    provider: &str,
    session_id: &str,
    hook_kind: ProviderHookKind,
    turn_key: &str,
) -> String {
    let kind = serde_json::to_string(&hook_kind).expect("provider hook kind serializes");
    digest_fields(&[
        b"vde-tmux:provider-event-ref:v1",
        provider.as_bytes(),
        session_id.as_bytes(),
        kind.as_bytes(),
        turn_key.as_bytes(),
    ])
}

fn ingress_event_reference(
    provider: &str,
    session_id: &str,
    hook_kind: ProviderHookKind,
    ingress_request_id: &EventId,
) -> String {
    let kind = serde_json::to_string(&hook_kind).expect("provider hook kind serializes");
    digest_fields(&[
        b"vde-tmux:provider-event-ref:v2:ingress",
        provider.as_bytes(),
        session_id.as_bytes(),
        kind.as_bytes(),
        ingress_request_id.as_str().as_bytes(),
    ])
}

fn observation_payload_digest(
    provider: &str,
    session_id: &str,
    hook_kind: ProviderHookKind,
    turn_key: Option<&str>,
    prompt_digest: Option<&str>,
    response_digest: Option<&str>,
) -> String {
    let kind = serde_json::to_string(&hook_kind).expect("provider hook kind serializes");
    digest_fields(&[
        b"vde-tmux:provider-observation:v1",
        provider.as_bytes(),
        session_id.as_bytes(),
        kind.as_bytes(),
        turn_key.unwrap_or("").as_bytes(),
        prompt_digest.unwrap_or("").as_bytes(),
        response_digest.unwrap_or("").as_bytes(),
    ])
}

fn digest_fields(fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn utf8_suffix(value: &str, limit: usize) -> &str {
    if value.len() <= limit {
        return value;
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_id() -> EventId {
        EventId::parse("00112233445566778899aabbccddeeff").unwrap()
    }

    #[test]
    fn prompt_event_references_use_the_ingress_occurrence_not_the_turn_key() {
        let claude = observation_from_json(
            "claude",
            "UserPromptSubmit",
            r#"{"session_id":"s1","prompt_id":"p1","prompt":"hello"}"#,
            event_id(),
            1,
        )
        .unwrap()
        .unwrap();
        let codex = observation_from_json(
            "codex",
            "UserPromptSubmit",
            r#"{"session_id":"s1","turn_id":"t1","prompt":"hello"}"#,
            event_id(),
            1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(claude.provider_turn_key.as_deref(), Some("p1"));
        assert_eq!(codex.provider_turn_key.as_deref(), Some("t1"));
        assert_ne!(claude.provider_event_ref, codex.provider_event_ref);
        assert_eq!(claude.prompt_digest, codex.prompt_digest);

        let second_codex = observation_from_json(
            "codex",
            "UserPromptSubmit",
            r#"{"session_id":"s1","turn_id":"t1","prompt":"hello"}"#,
            EventId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            2,
        )
        .unwrap()
        .unwrap();
        assert_eq!(second_codex.provider_turn_key, codex.provider_turn_key);
        assert_ne!(second_codex.provider_event_ref, codex.provider_event_ref);
        assert_eq!(second_codex.prompt_digest, codex.prompt_digest);
    }

    #[test]
    fn retrying_one_prompt_ingress_reuses_the_same_event_reference() {
        let first = observation_from_json(
            "codex",
            "UserPromptSubmit",
            r#"{"session_id":"s1","turn_id":"t1","prompt":"hello"}"#,
            event_id(),
            1,
        )
        .unwrap()
        .unwrap();
        let retry = observation_from_json(
            "codex",
            "UserPromptSubmit",
            r#"{"session_id":"s1","turn_id":"t1","prompt":"hello"}"#,
            event_id(),
            2,
        )
        .unwrap()
        .unwrap();

        assert_eq!(first.provider_event_ref, retry.provider_event_ref);
        assert_eq!(first.payload_digest, retry.payload_digest);
    }

    #[test]
    fn activity_and_waiting_do_not_reuse_turn_event_references() {
        let activity = observation_from_json(
            "codex",
            "PreToolUse",
            r#"{"session_id":"s1","turn_id":"t1"}"#,
            event_id(),
            1,
        )
        .unwrap()
        .unwrap();
        let waiting = observation_from_json(
            "codex",
            "PermissionRequest",
            r#"{"session_id":"s1","turn_id":"t1"}"#,
            event_id(),
            2,
        )
        .unwrap()
        .unwrap();

        assert_eq!(activity.provider_turn_key.as_deref(), Some("t1"));
        assert_eq!(waiting.provider_turn_key.as_deref(), Some("t1"));
        assert_eq!(activity.provider_event_ref, None);
        assert_eq!(waiting.provider_event_ref, None);
    }

    #[test]
    fn codex_durable_lifecycle_rejects_missing_turn_id() {
        for event in [
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "Stop",
        ] {
            let payload = if event == "UserPromptSubmit" {
                r#"{"session_id":"s1","prompt":"hello"}"#
            } else {
                r#"{"session_id":"s1"}"#
            };
            let error = observation_from_json("codex", event, payload, event_id(), 1)
                .unwrap_err()
                .to_string();
            assert!(error.contains("requires turn_id"), "{event}: {error}");
        }

        assert!(
            observation_from_json(
                "codex",
                "SessionStart",
                r#"{"session_id":"s1"}"#,
                event_id(),
                1,
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn stop_keeps_a_utf8_suffix_and_original_digest() {
        let body = format!("{}終", "a".repeat(RESPONSE_ARTIFACT_BODY_MAX_BYTES));
        let payload = serde_json::json!({
            "session_id": "s1",
            "turn_id": "t1",
            "last_assistant_message": body,
        });
        let observation =
            observation_from_json("codex", "Stop", &payload.to_string(), event_id(), 1)
                .unwrap()
                .unwrap();
        let response = observation.response.unwrap();
        let decoded = response.decode_body().unwrap();
        assert!(response.truncated);
        assert!(decoded.len() <= RESPONSE_ARTIFACT_BODY_MAX_BYTES);
        assert!(std::str::from_utf8(&decoded).unwrap().ends_with('終'));
        assert_ne!(response.original_digest, response.stored_digest);
    }

    #[test]
    fn max_response_candidate_fits_the_daemon_request_frame_budget() {
        let body = "x".repeat(RESPONSE_ARTIFACT_BODY_MAX_BYTES);
        let payload = serde_json::json!({
            "session_id": "s1",
            "prompt_id": "p1",
            "last_assistant_message": body,
        });
        let observation =
            observation_from_json("claude", "Stop", &payload.to_string(), event_id(), 1)
                .unwrap()
                .unwrap();
        assert!(
            serde_json::to_vec(&observation).unwrap().len()
                < crate::pane_state::MAX_REQUEST_FRAME_BYTES
        );
    }
}

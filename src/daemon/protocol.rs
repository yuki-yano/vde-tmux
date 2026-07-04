//! daemon と client の NDJSON protocol。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMessage {
    StatuslineAgentBadge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    StatuslineAgentBadge { value: String },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_roundtrips_statusline_agent_badge() {
        let json = serde_json::to_string(&ClientMessage::StatuslineAgentBadge).unwrap();
        assert_eq!(json, r#"{"op":"statusline_agent_badge"}"#);
        let message: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(message, ClientMessage::StatuslineAgentBadge);
    }

    #[test]
    fn server_message_roundtrips_badge() {
        let message = ServerMessage::StatuslineAgentBadge {
            value: "running:1".to_string(),
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            r#"{"type":"statusline_agent_badge","value":"running:1"}"#
        );
        let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }
}

//! daemon と client の NDJSON protocol。

use serde::{Deserialize, Serialize};

use super::DaemonSnapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMessage {
    Query { proto: u16, what: QueryTarget },
    Subscribe { proto: u16 },
    StatuslineAgentBadge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryTarget {
    Statusline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Statusline { agent_badge: String },
    Snapshot { snapshot: DaemonSnapshot },
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

    #[test]
    fn query_statusline_uses_role_declaration_shape() {
        let message = ClientMessage::Query {
            proto: 1,
            what: QueryTarget::Statusline,
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"op":"query","proto":1,"what":"statusline"}"#);
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn subscribe_uses_proto_field() {
        let message = ClientMessage::Subscribe { proto: 1 };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"op":"subscribe","proto":1}"#);
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }
}

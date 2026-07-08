use serde::{Deserialize, Serialize};

use super::DaemonSnapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMessage {
    Query {
        proto: u16,
        what: QueryTarget,
    },
    Subscribe {
        proto: u16,
    },
    RefreshPanes {
        proto: u16,
    },
    Shutdown {
        proto: u16,
    },
    SidebarEvent {
        proto: u16,
        event: SidebarClientEvent,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidebarClientEvent {
    Key {
        key: String,
    },
    JumpPane {
        pane: String,
    },
    SelectContext {
        pane: Option<String>,
        session: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryTarget {
    Summary,
    Attention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerMessage {
    Summary { text: String },
    Attention { text: String },
    Snapshot { snapshot: DaemonSnapshot },
    Ack,
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_query_uses_role_declaration_shape() {
        let message = ClientMessage::Query {
            proto: 1,
            what: QueryTarget::Summary,
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"op":"query","proto":1,"what":"summary"}"#);
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn attention_query_roundtrips() {
        let message = ClientMessage::Query {
            proto: 1,
            what: QueryTarget::Attention,
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"op":"query","proto":1,"what":"attention"}"#);
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            message
        );
    }

    #[test]
    fn subscribe_uses_proto_field() {
        let message = ClientMessage::Subscribe { proto: 1 };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"op":"subscribe","proto":1}"#);
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn shutdown_uses_proto_field() {
        let message = ClientMessage::Shutdown { proto: 1 };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"op":"shutdown","proto":1}"#);
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn sidebar_event_roundtrips_key() {
        let message = ClientMessage::SidebarEvent {
            proto: 1,
            event: SidebarClientEvent::Key {
                key: "j".to_string(),
            },
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            r#"{"op":"sidebar_event","proto":1,"event":{"type":"key","key":"j"}}"#
        );
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn sidebar_event_roundtrips_select_context() {
        let message = ClientMessage::SidebarEvent {
            proto: 1,
            event: SidebarClientEvent::SelectContext {
                pane: Some("%1".to_string()),
                session: Some("main".to_string()),
            },
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            r#"{"op":"sidebar_event","proto":1,"event":{"type":"select_context","pane":"%1","session":"main"}}"#
        );
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn refresh_panes_roundtrips() {
        let message = ClientMessage::RefreshPanes { proto: 1 };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(json, r#"{"op":"refresh_panes","proto":1}"#);
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn ack_roundtrips() {
        let json = serde_json::to_string(&ServerMessage::Ack).unwrap();
        assert_eq!(json, r#"{"type":"ack"}"#);
        let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, ServerMessage::Ack);
    }

    #[test]
    fn snapshot_message_defaults_missing_sidebar_counts() {
        let json = r#"{"type":"snapshot","snapshot":{"agent_count":0,"rollup":"idle","panes":[],"sidebar":{"state":{"version":0},"rows":[]},"events":[]}}"#;

        let decoded: ServerMessage = serde_json::from_str(json).unwrap();

        let ServerMessage::Snapshot { snapshot } = decoded else {
            panic!("expected snapshot");
        };
        let sidebar = snapshot.sidebar.expect("sidebar frame");
        assert_eq!(sidebar.counts, crate::sidebar::tree::BadgeCounts::default());
    }
}

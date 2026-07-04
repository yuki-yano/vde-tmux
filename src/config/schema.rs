use serde_json::{Value, json};

pub fn config_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "vde-tmux config",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "ghq_root": { "type": ["string", "null"] },
            "categories": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "display_names": { "type": "object", "additionalProperties": { "type": "string" } },
                    "order": { "type": "object", "additionalProperties": { "type": "integer" } },
                    "default_category": { "type": ["string", "null"] },
                    "rules": { "type": "array" },
                    "session_name_rules": { "type": "array" }
                }
            },
            "statusline": { "type": "object", "additionalProperties": true },
            "sidebar": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "width": { "type": "integer", "minimum": 1 }
                }
            },
            "daemon": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "poll_ms": { "type": "integer", "minimum": 1 },
                    "git": { "type": "object", "additionalProperties": true }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_contains_top_level_sections() {
        let schema = config_schema();
        let properties = schema
            .get("properties")
            .and_then(|value| value.as_object())
            .unwrap();

        for key in ["ghq_root", "categories", "statusline", "sidebar", "daemon"] {
            assert!(
                properties.contains_key(key),
                "missing schema property {key}"
            );
        }
    }
}

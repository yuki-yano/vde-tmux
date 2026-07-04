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
            "statusline": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "session_badge": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "enabled": { "type": "boolean" },
                            "suffix": { "type": "string" },
                            "glyphs": {
                                "type": "object",
                                "additionalProperties": true,
                                "properties": {
                                    "blocked": { "type": "string" },
                                    "working": { "type": "string" },
                                    "done": { "type": "string" },
                                    "idle": { "type": "string" }
                                }
                            }
                        }
                    }
                }
            },
            "sidebar": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "width": {
                        "oneOf": [
                            { "type": "integer", "minimum": 1 },
                            { "type": "string", "pattern": "^(100|[1-9][0-9]?)%$" }
                        ]
                    },
                    "min_width": { "type": "integer", "minimum": 1 }
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

    #[test]
    fn schema_sidebar_width_accepts_integer_or_percent_string() {
        let schema = config_schema();
        let sidebar = &schema["properties"]["sidebar"]["properties"];
        assert!(sidebar["width"]["oneOf"].is_array());
        assert_eq!(sidebar["min_width"]["type"], "integer");
    }
}

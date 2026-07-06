use serde_json::{Value, json};

pub fn config_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "vde-tmux config",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "categories": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "display_names": { "type": "object", "additionalProperties": { "type": "string" } },
                    "order": { "type": "object", "additionalProperties": { "type": "integer" } },
                    "default_category": { "type": ["string", "null"] },
                    "rules": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "category": { "type": "string" },
                                "path_patterns": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                }
                            }
                        }
                    },
                    "session_name_rules": { "type": "array" }
                }
            },
            "badge": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "glyphs": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "blocked": { "type": "string" },
                            "working": { "type": "string" },
                            "done": { "type": "string" },
                            "idle": { "type": "string" }
                        }
                    },
                    "colors": {
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
            },
            "popup": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "width": { "type": "string" },
                    "height": { "type": "string" }
                }
            },
            "statusline": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "summary": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "enabled": { "type": "boolean" }
                        }
                    },
                    "sessions": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "badge_style": {
                                "type": "string",
                                "enum": ["inline", "plain", "outer"]
                            },
                            "separator": { "type": "string" }
                        }
                    },
                    "category": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "mode": { "type": "string" },
                            "format": { "type": "string" },
                            "inactive_format": { "type": "string" },
                            "show_badge": { "type": "boolean" }
                        }
                    },
                    "session_badge": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "enabled": { "type": "boolean" },
                            "suffix": { "type": "string" },
                            "hide_idle": { "type": "boolean" }
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
                    "min_width": { "type": "integer", "minimum": 1 },
                    "colors": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "error": { "type": "string" },
                            "running": { "type": "string" },
                            "permission": { "type": "string" },
                            "background": { "type": "string" },
                            "waiting": { "type": "string" },
                            "idle": { "type": "string" },
                            "selection_bg": { "type": "string" },
                            "header_active_bg": { "type": "string" },
                            "header_active_fg": { "type": "string" }
                        }
                    },
                    "header": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "format": { "type": "string" },
                            "prefix": { "type": "string" },
                            "suffix": { "type": "string" },
                            "separator": { "type": "string" },
                            "bold": { "type": "boolean" },
                            "colors": {
                                "type": "object",
                                "additionalProperties": true,
                                "properties": {
                                    "fg": { "type": "string" },
                                    "bg": { "type": "string" },
                                    "outer_bg": { "type": "string" }
                                }
                            }
                        }
                    },
                    "preview": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "history_lines": { "type": "integer", "minimum": 0 }
                        }
                    },
                    "live": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "enabled": { "type": "boolean" },
                            "lines": { "type": "integer", "minimum": 0 },
                            "interval_ms": { "type": "integer", "minimum": 1 }
                        }
                    }
                }
            },
            "daemon": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "poll_ms": { "type": "integer", "minimum": 1 },
                    "git": { "type": "object", "additionalProperties": true }
                }
            },
            "notify": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "enabled": { "type": "boolean" },
                    "command": { "type": "string" }
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

        for key in [
            "categories",
            "statusline",
            "sidebar",
            "daemon",
            "badge",
            "notify",
            "popup",
        ] {
            assert!(
                properties.contains_key(key),
                "missing schema property {key}"
            );
        }
        assert!(!properties.contains_key("ghq_root"));
    }

    #[test]
    fn schema_contains_popup_size() {
        let schema = config_schema();
        let popup = &schema["properties"]["popup"]["properties"];

        assert_eq!(popup["width"]["type"], "string");
        assert_eq!(popup["height"]["type"], "string");
    }

    #[test]
    fn schema_sidebar_width_accepts_integer_or_percent_string() {
        let schema = config_schema();
        let sidebar = &schema["properties"]["sidebar"]["properties"];
        assert!(sidebar["width"]["oneOf"].is_array());
        assert_eq!(sidebar["min_width"]["type"], "integer");
    }

    #[test]
    fn schema_contains_sidebar_colors() {
        let schema = config_schema();
        let colors = &schema["properties"]["sidebar"]["properties"]["colors"]["properties"];

        assert_eq!(colors["header_active_bg"]["type"], "string");
        assert_eq!(colors["selection_bg"]["type"], "string");
        assert!(colors.get("attention").is_none());
        assert!(colors.get("selection_active_bg").is_none());
    }

    #[test]
    fn schema_contains_sidebar_header_style() {
        let schema = config_schema();
        let header = &schema["properties"]["sidebar"]["properties"]["header"]["properties"];

        assert_eq!(header["format"]["type"], "string");
        assert_eq!(header["prefix"]["type"], "string");
        assert_eq!(header["suffix"]["type"], "string");
        assert_eq!(header["separator"]["type"], "string");
        assert_eq!(header["bold"]["type"], "boolean");
        assert_eq!(header["colors"]["type"], "object");
    }

    #[test]
    fn schema_contains_sidebar_preview_history_lines() {
        let schema = config_schema();
        let preview = &schema["properties"]["sidebar"]["properties"]["preview"]["properties"];

        assert_eq!(preview["history_lines"]["type"], "integer");
        assert_eq!(preview["history_lines"]["minimum"], 0);
    }

    #[test]
    fn schema_contains_sidebar_live_and_notify() {
        let schema = config_schema();
        let live = &schema["properties"]["sidebar"]["properties"]["live"]["properties"];
        let notify = &schema["properties"]["notify"]["properties"];

        assert_eq!(live["enabled"]["type"], "boolean");
        assert_eq!(live["lines"]["type"], "integer");
        assert_eq!(live["interval_ms"]["type"], "integer");
        assert_eq!(notify["enabled"]["type"], "boolean");
        assert_eq!(notify["command"]["type"], "string");
    }

    #[test]
    fn schema_contains_path_patterns_and_top_level_badge_glyphs() {
        let schema = config_schema();
        let rule = &schema["properties"]["categories"]["properties"]["rules"]["items"];
        assert_eq!(
            rule["properties"]["path_patterns"]["items"]["type"],
            "string"
        );
        assert!(rule["properties"].get("ghq_patterns").is_none());

        let badge = &schema["properties"]["badge"]["properties"]["glyphs"]["properties"];
        assert_eq!(badge["working"]["type"], "string");
        let badge_colors = &schema["properties"]["badge"]["properties"]["colors"]["properties"];
        assert_eq!(badge_colors["working"]["type"], "string");
        assert!(
            schema["properties"]["statusline"]["properties"]["session_badge"]["properties"]
                .get("glyphs")
                .is_none()
        );
        assert_eq!(
            schema["properties"]["statusline"]["properties"]["session_badge"]["properties"]["hide_idle"]
                ["type"],
            "boolean"
        );
        assert_eq!(
            schema["properties"]["statusline"]["properties"]["summary"]["properties"]["enabled"]["type"],
            "boolean"
        );
        assert_eq!(
            schema["properties"]["statusline"]["properties"]["category"]["properties"]["inactive_format"]
                ["type"],
            "string"
        );
        assert_eq!(
            schema["properties"]["statusline"]["properties"]["sessions"]["properties"]["badge_style"]
                ["enum"][0],
            "inline"
        );
        assert_eq!(
            schema["properties"]["statusline"]["properties"]["sessions"]["properties"]["badge_style"]
                ["enum"][2],
            "outer"
        );
    }
}

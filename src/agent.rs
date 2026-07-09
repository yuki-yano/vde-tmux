pub fn display_agent_name(agent: &str) -> String {
    match agent.trim() {
        "codex" | "Codex" => "Codex".to_string(),
        "claude" | "Claude" => "Claude".to_string(),
        other => other.to_string(),
    }
}

pub fn display_agent_label_prefix(label: &str) -> String {
    for agent in ["codex", "claude"] {
        if label == agent {
            return display_agent_name(agent);
        }
        if let Some(rest) = label.strip_prefix(agent)
            && rest
                .chars()
                .next()
                .is_some_and(|ch| matches!(ch, ':' | ' ' | '(' | '\u{00b7}'))
        {
            return format!("{}{rest}", display_agent_name(agent));
        }
    }
    label.to_string()
}

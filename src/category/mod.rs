use std::collections::BTreeSet;

use crate::config::Config;
use crate::session::{Direction, SessionInfo};

pub fn resolve_category_for_session(config: &Config, session: &SessionInfo) -> String {
    resolve_category_for_session_with_stored(config, session, true)
}

pub fn resolve_dynamic_category_for_session(config: &Config, session: &SessionInfo) -> String {
    resolve_category_for_session_with_stored(config, session, false)
}

fn resolve_category_for_session_with_stored(
    config: &Config,
    session: &SessionInfo,
    use_stored_category: bool,
) -> String {
    if !session.category_override.is_empty() {
        return session.category_override.clone();
    }
    for rule in &config.categories.rules {
        if rule
            .path_patterns
            .iter()
            .any(|pattern| matches_path_pattern(pattern, &session.project_path))
        {
            return rule.category.clone();
        }
    }
    for rule in &config.categories.session_name_rules {
        if rule
            .patterns
            .iter()
            .any(|pattern| wildcard_match(pattern, &session.name))
        {
            return rule.category.clone();
        }
    }
    if use_stored_category && !session.category.is_empty() {
        return session.category.clone();
    }
    config
        .categories
        .default_category
        .clone()
        .filter(|category| !category.is_empty())
        .unwrap_or_default()
}

pub fn sorted_categories(config: &Config, sessions: &[SessionInfo]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for session in sessions {
        let category = resolve_category_for_session(config, session);
        if !category.is_empty() {
            set.insert(category);
        }
    }
    set.extend(config.categories.display_names.keys().cloned());
    set.extend(config.categories.order.keys().cloned());
    if let Some(default_category) = &config.categories.default_category
        && !default_category.is_empty()
    {
        set.insert(default_category.clone());
    }
    let mut categories = set.into_iter().collect::<Vec<_>>();
    categories.sort_by(|left, right| {
        let left_order = config
            .categories
            .order
            .get(left)
            .copied()
            .unwrap_or(i64::MAX);
        let right_order = config
            .categories
            .order
            .get(right)
            .copied()
            .unwrap_or(i64::MAX);
        left_order.cmp(&right_order).then_with(|| left.cmp(right))
    });
    categories
}

pub fn sessions_in_category<'a>(
    config: &Config,
    sessions: &'a [SessionInfo],
    category: &str,
) -> Vec<&'a SessionInfo> {
    sessions
        .iter()
        .filter(|session| resolve_category_for_session(config, session) == category)
        .collect()
}

pub fn sorted_effective_categories(config: &Config, sessions: &[SessionInfo]) -> Vec<String> {
    let mut categories = sessions
        .iter()
        .map(|session| resolve_category_for_session(config, session))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    categories.sort_by(|left, right| {
        let left_order = config
            .categories
            .order
            .get(left)
            .copied()
            .unwrap_or(i64::MAX);
        let right_order = config
            .categories
            .order
            .get(right)
            .copied()
            .unwrap_or(i64::MAX);
        left_order.cmp(&right_order).then_with(|| left.cmp(right))
    });
    categories
}

pub fn adjacent_category(
    config: &Config,
    sessions: &[SessionInfo],
    current_category: &str,
    direction: Direction,
) -> Option<String> {
    let categories = sorted_effective_categories(config, sessions);
    if categories.is_empty() {
        return None;
    }
    let index = categories
        .iter()
        .position(|category| category == current_category)
        .unwrap_or(0);
    let next = match direction {
        Direction::Next => (index + 1) % categories.len(),
        Direction::Previous => (index + categories.len() - 1) % categories.len(),
    };
    categories.get(next).cloned()
}

fn matches_path_pattern(pattern: &str, path: &str) -> bool {
    if wildcard_match(pattern, path) {
        return true;
    }
    path.match_indices('/').any(|(index, _)| {
        let suffix = &path[index + 1..];
        wildcard_match(pattern, suffix)
    })
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut pattern_index, mut text_index) = (0, 0);
    let mut star_index = None;
    let mut match_index = 0;

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == text[text_index] || pattern[pattern_index] == b'?')
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            match_index = text_index;
            pattern_index += 1;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            match_index += 1;
            text_index = match_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }

    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CategoryRule, Config, SessionNameRule};
    use crate::session::{Direction, SessionInfo};

    fn session(
        name: &str,
        project_path: &str,
        category: &str,
        override_category: &str,
    ) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            project_path: project_path.to_string(),
            category: category.to_string(),
            category_override: override_category.to_string(),
            ..SessionInfo::default()
        }
    }

    #[test]
    fn override_wins_over_project_and_stored_category() {
        let mut config = Config::default();
        config.categories.rules.push(CategoryRule {
            category: "project".to_string(),
            path_patterns: vec!["github.com/acme/*".to_string()],
        });
        let actual = resolve_category_for_session(
            &config,
            &session("main", "/ghq/github.com/acme/app", "work", "private"),
        );
        assert_eq!(actual, "private");
    }

    #[test]
    fn project_rule_matches_suffix_with_star() {
        let mut config = Config::default();
        config.categories.rules.push(CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["github.com/acme/*".to_string()],
        });
        let actual = resolve_category_for_session(
            &config,
            &session("app", "/Users/me/ghq/github.com/acme/app", "", ""),
        );
        assert_eq!(actual, "work");
    }

    #[test]
    fn project_rule_matches_path_patterns_suffix_with_star() {
        let mut config = Config::default();
        config.categories.rules.push(CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["github.com/acme/*".to_string()],
        });
        let actual = resolve_category_for_session(
            &config,
            &session("app", "/Users/me/src/github.com/acme/app", "", ""),
        );
        assert_eq!(actual, "work");
    }

    #[test]
    fn session_name_rule_matches_wildcard() {
        let mut config = Config::default();
        config.categories.session_name_rules.push(SessionNameRule {
            category: "private".to_string(),
            patterns: vec!["secret-*".to_string()],
        });
        let actual =
            resolve_category_for_session(&config, &session("secret-repo", "/tmp/repo", "", ""));
        assert_eq!(actual, "private");
    }

    #[test]
    fn stored_category_wins_over_default() {
        let mut config = Config::default();
        config.categories.default_category = Some("default".to_string());
        let actual = resolve_category_for_session(&config, &session("repo", "", "work", ""));
        assert_eq!(actual, "work");
    }

    #[test]
    fn dynamic_category_uses_default_instead_of_stored_category() {
        let mut config = Config::default();
        config.categories.default_category = Some("public".to_string());
        let actual = resolve_dynamic_category_for_session(
            &config,
            &session("repo", "/Users/me", "work", ""),
        );
        assert_eq!(actual, "public");
    }

    #[test]
    fn dynamic_category_still_prefers_project_rule() {
        let mut config = Config::default();
        config.categories.default_category = Some("public".to_string());
        config.categories.rules.push(CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["github.com/acme/*".to_string()],
        });
        let actual = resolve_dynamic_category_for_session(
            &config,
            &session("repo", "/Users/me/src/github.com/acme/app", "public", ""),
        );
        assert_eq!(actual, "work");
    }

    #[test]
    fn sorted_categories_uses_order_then_name() {
        let mut config = Config::default();
        config
            .categories
            .display_names
            .insert("work".into(), "W".into());
        config
            .categories
            .display_names
            .insert("private".into(), "P".into());
        config.categories.order.insert("private".into(), 20);
        config.categories.order.insert("work".into(), 10);
        let categories = sorted_categories(&config, &[session("main", "", "misc", "")]);
        assert_eq!(categories, vec!["work", "private", "misc"]);
    }

    #[test]
    fn adjacent_category_wraps_by_direction() {
        let mut config = Config::default();
        config.categories.order.insert("a".into(), 10);
        config.categories.order.insert("b".into(), 20);
        let sessions = [session("one", "", "a", ""), session("two", "", "b", "")];
        assert_eq!(
            adjacent_category(&config, &sessions, "a", Direction::Next),
            Some("b".to_string())
        );
        assert_eq!(
            adjacent_category(&config, &sessions, "a", Direction::Previous),
            Some("b".to_string())
        );
        assert_eq!(
            adjacent_category(&config, &sessions, "b", Direction::Next),
            Some("a".to_string())
        );
        assert_eq!(
            adjacent_category(&config, &sessions, "b", Direction::Previous),
            Some("a".to_string())
        );
    }

    #[test]
    fn adjacent_category_skips_empty_configured_categories() {
        let mut config = Config::default();
        config.categories.order.insert("a".into(), 10);
        config.categories.order.insert("empty".into(), 20);
        config.categories.order.insert("b".into(), 30);
        let sessions = [session("one", "", "a", ""), session("two", "", "b", "")];

        assert_eq!(
            adjacent_category(&config, &sessions, "a", Direction::Next),
            Some("b".to_string())
        );
        assert_eq!(
            adjacent_category(&config, &sessions, "b", Direction::Next),
            Some("a".to_string())
        );
    }
}

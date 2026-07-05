//! vtm 相当の statusline sessions/category 描画。

use anyhow::{Result, anyhow};

use crate::category::{resolve_category_for_session, sessions_in_category, sorted_categories};
use crate::config::{BadgeStyle, Config, SegmentStyle, StatuslineCategoryConfig};
use crate::session::{
    SessionInfo, current_session_name, find_session, list_sessions, switch_client, use_category,
};
use crate::tmux::TmuxRunner;

pub fn statusline_sessions(runner: &dyn TmuxRunner, config: &Config) -> Result<String> {
    let sessions = list_sessions(runner)?;
    let current_session = current_session_name(runner)?;
    let current_category = current_category(config, &sessions, &current_session);
    Ok(render_statusline_sessions(
        config,
        &sessions,
        &current_session,
        &current_category,
    ))
}

pub fn statusline_category(runner: &dyn TmuxRunner, config: &Config) -> Result<String> {
    let sessions = list_sessions(runner)?;
    let current_session = current_session_name(runner)?;
    let current_category = current_category(config, &sessions, &current_session);
    Ok(render_statusline_category(
        config,
        &sessions,
        &current_category,
    ))
}

pub fn switch_statusline_session(
    runner: &dyn TmuxRunner,
    config: &Config,
    index: usize,
) -> Result<()> {
    let sessions = list_sessions(runner)?;
    let current_session = current_session_name(runner)?;
    let current_category = current_category(config, &sessions, &current_session);
    let category_sessions = sessions_in_category(config, &sessions, &current_category);
    let session = category_sessions
        .get(index)
        .ok_or_else(|| anyhow!("session index out of range: {index}"))?;
    switch_client(runner, &session.name)
}

pub fn switch_statusline_category(
    runner: &dyn TmuxRunner,
    config: &Config,
    index: usize,
) -> Result<()> {
    let sessions = list_sessions(runner)?;
    let categories = sorted_categories(config, &sessions);
    let category = categories
        .get(index)
        .ok_or_else(|| anyhow!("category index out of range: {index}"))?;
    use_category(runner, config, category)
}

pub fn render_statusline_sessions(
    config: &Config,
    sessions: &[SessionInfo],
    current_session: &str,
    current_category: &str,
) -> String {
    sessions_in_category(config, sessions, current_category)
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let style = if session.name == current_session {
                &config.statusline.sessions.current
            } else {
                &config.statusline.sessions.other
            };
            render_session_segment(
                style,
                &session.badge,
                &session.state,
                &session.name,
                index,
                config.statusline.sessions.show_index,
                config.statusline.sessions.badge_style,
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

pub fn render_statusline_category(
    config: &Config,
    sessions: &[SessionInfo],
    current_category: &str,
) -> String {
    let categories = if config.statusline.category.mode == "current" {
        if current_category.is_empty() {
            Vec::new()
        } else {
            vec![current_category.to_string()]
        }
    } else {
        sorted_categories(config, sessions)
    };
    categories
        .iter()
        .map(|category| {
            let label = config
                .categories
                .display_names
                .get(category)
                .map(String::as_str)
                .unwrap_or(category);
            let count = sessions_in_category(config, sessions, category).len();
            let body = config
                .statusline
                .category
                .format
                .replace("{category}", label)
                .replace("{count}", &count.to_string());
            tmux_style_category(
                &config.statusline.category,
                &body,
                category == current_category,
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

fn current_category(config: &Config, sessions: &[SessionInfo], current_session: &str) -> String {
    find_session(sessions, current_session)
        .map(|session| resolve_category_for_session(config, session))
        .unwrap_or_default()
}

fn render_session_segment(
    style: &SegmentStyle,
    badge: &str,
    state: &str,
    session_name: &str,
    index: usize,
    show_index: bool,
    badge_style: BadgeStyle,
) -> String {
    let label = if show_index {
        format!("{}:{session_name}", index + 1)
    } else {
        session_name.to_string()
    };
    let label = format!(
        "{}{label}",
        badge_fragment(badge, state, style, badge_style)
    );
    let body = style
        .format
        .replace("{session}", &label)
        .replace("{index}", &(index + 1).to_string());
    tmux_style_segment(style, &body)
}

fn badge_fragment(
    badge: &str,
    state: &str,
    style: &SegmentStyle,
    badge_style: BadgeStyle,
) -> String {
    if badge.is_empty() {
        return String::new();
    }
    if badge_style == BadgeStyle::Plain {
        return badge.to_string();
    }
    let color = match state {
        "blocked" => Some("red"),
        "working" => Some("green"),
        "done" => Some("cyan"),
        _ => None,
    };
    match color {
        Some(color) => {
            let restore = style.colors.fg.as_deref().unwrap_or("default");
            format!("#[fg={color}]{badge}#[fg={restore}]")
        }
        None => badge.to_string(),
    }
}

fn tmux_style_segment(style: &SegmentStyle, body: &str) -> String {
    let mut attrs = Vec::new();
    if style.bold {
        attrs.push("bold".to_string());
    }
    if let Some(fg) = &style.colors.fg {
        attrs.push(format!("fg={fg}"));
    }
    if let Some(bg) = &style.colors.bg {
        attrs.push(format!("bg={bg}"));
    }
    let styled = if attrs.is_empty() {
        body.to_string()
    } else {
        format!("#[{}]{}#[default]", attrs.join(","), body)
    };
    format!("{}{}{}", style.prefix, styled, style.suffix)
}

fn tmux_style_category(config: &StatuslineCategoryConfig, body: &str, active: bool) -> String {
    let colors = if active {
        &config.colors
    } else {
        &config.inactive_colors
    };
    let mut attrs = Vec::new();
    if config.bold && active {
        attrs.push("bold".to_string());
    }
    if let Some(fg) = &colors.fg {
        attrs.push(format!("fg={fg}"));
    }
    if let Some(bg) = &colors.bg {
        attrs.push(format!("bg={bg}"));
    }
    let styled = if attrs.is_empty() {
        body.to_string()
    } else {
        format!("#[{}]{}#[default]", attrs.join(","), body)
    };
    format!("{}{}{}", config.prefix, styled, config.suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BadgeStyle, Config};
    use crate::session::SessionInfo;

    fn session(name: &str, category: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            category: category.to_string(),
            ..SessionInfo::default()
        }
    }

    #[test]
    fn renders_sessions_with_current_marker() {
        let config = Config::default();
        let rendered = render_statusline_sessions(
            &config,
            &[session("main", "work"), session("sub", "work")],
            "main",
            "work",
        );
        assert!(rendered.contains("main"));
        assert!(rendered.contains("sub"));
    }

    #[test]
    fn current_session_is_bold_by_default() {
        let config = Config::default();
        let rendered = render_statusline_sessions(
            &config,
            &[session("main", "work"), session("sub", "work")],
            "main",
            "work",
        );
        assert!(rendered.contains("#[bold] main #[default]"), "{rendered}");
        assert!(rendered.contains(" sub "), "{rendered}");
        assert!(!rendered.contains("#[bold] sub"), "{rendered}");
    }

    #[test]
    fn render_statusline_sessions_prefixes_badge_to_label() {
        let config = Config::default();
        let mut main = session("main", "work");
        main.badge = "🔴 ".to_string();
        let rendered =
            render_statusline_sessions(&config, &[main, session("sub", "work")], "main", "work");

        assert!(rendered.contains("🔴 main"));
        assert!(rendered.contains("sub"));
    }

    #[test]
    fn inline_badge_uses_state_color_and_restores_segment_fg() {
        let config = Config::default();
        let mut main = session("main", "work");
        main.badge = "▲".to_string();
        main.state = "blocked".to_string();
        let rendered = render_statusline_sessions(&config, &[main], "main", "work");
        assert!(
            rendered.contains("#[fg=red]▲#[fg=default]main"),
            "{rendered}"
        );
    }

    #[test]
    fn inline_badge_restores_configured_segment_fg() {
        let mut config = Config::default();
        config.statusline.sessions.other.colors.fg = Some("white".to_string());
        let mut sub = session("sub", "work");
        sub.badge = "●".to_string();
        sub.state = "working".to_string();
        let rendered = render_statusline_sessions(&config, &[sub], "main", "work");
        assert!(
            rendered.contains("#[fg=green]●#[fg=white]sub"),
            "{rendered}"
        );
    }

    #[test]
    fn plain_badge_style_keeps_legacy_concatenation() {
        let mut config = Config::default();
        config.statusline.sessions.badge_style = BadgeStyle::Plain;
        let mut main = session("main", "work");
        main.badge = "▲".to_string();
        main.state = "blocked".to_string();
        let rendered = render_statusline_sessions(&config, &[main], "main", "work");
        assert!(rendered.contains("▲main"), "{rendered}");
        assert!(!rendered.contains("#[fg=red]"), "{rendered}");
    }

    #[test]
    fn renders_empty_category_when_no_categories_exist() {
        let config = Config::default();
        let rendered = render_statusline_category(&config, &[], "");
        assert_eq!(rendered, "");
    }

    #[test]
    fn renders_category_display_names_in_order() {
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
        let rendered = render_statusline_category(
            &config,
            &[session("a", "private"), session("b", "work")],
            "work",
        );
        assert!(rendered.find("W").unwrap() < rendered.find("P").unwrap());
    }

    #[test]
    fn current_category_mode_renders_only_current_category() {
        let mut config = Config::default();
        config.statusline.category.mode = "current".to_string();
        config
            .categories
            .display_names
            .insert("work".into(), "W".into());
        config
            .categories
            .display_names
            .insert("private".into(), "P".into());
        let rendered = render_statusline_category(
            &config,
            &[session("a", "private"), session("b", "work")],
            "work",
        );
        assert!(rendered.contains("W"));
        assert!(!rendered.contains("P"));
    }

    #[test]
    fn category_format_supports_count_placeholder() {
        let mut config = Config::default();
        config.statusline.category.format = "{category} {count} ".to_string();
        let rendered = render_statusline_category(
            &config,
            &[
                session("a", "work"),
                session("b", "work"),
                session("c", "private"),
            ],
            "work",
        );
        assert!(rendered.contains("work 2"), "{rendered}");
        assert!(rendered.contains("private 1"), "{rendered}");
    }
}

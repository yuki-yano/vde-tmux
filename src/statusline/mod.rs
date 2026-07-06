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
    let heartbeat = crate::options::show_global_option(runner, crate::options::KEY_HEARTBEAT)?
        .and_then(|value| value.parse::<i64>().ok());
    let stale = is_heartbeat_stale(
        heartbeat,
        crate::sidebar::tree::now_epoch_secs(),
        config.daemon.poll_ms,
    );
    Ok(render_statusline_sessions_with_stale(
        config,
        &sessions,
        &current_session,
        &current_category,
        stale,
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
    render_statusline_sessions_with_stale(
        config,
        sessions,
        current_session,
        current_category,
        false,
    )
}

pub fn render_statusline_sessions_with_stale(
    config: &Config,
    sessions: &[SessionInfo],
    current_session: &str,
    current_category: &str,
    stale: bool,
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
            let badge = if stale && !session.badge.is_empty() {
                "?"
            } else {
                &session.badge
            };
            let state = if stale { "" } else { &session.state };
            let label = if config.statusline.sessions.show_index {
                // `{num}: {session}` 形式。show_index=false で番号ごと省く。
                format!("{}: {}", index + 1, session.name)
            } else {
                session.name.clone()
            };
            let segment = render_session_segment(
                style,
                badge,
                state,
                &label,
                index,
                config.statusline.sessions.badge_style,
                &config.badge.colors,
            );
            // クリックで switch-client できるよう tmux の session range で包む
            // (.tmux.conf 側の MouseDown1Status バインドが `-t =` で拾う)
            if session.id.is_empty() {
                segment
            } else {
                format!("#[range=session|{}]{segment}#[norange]", session.id)
            }
        })
        .collect::<Vec<_>>()
        .join(&config.statusline.sessions.separator)
}

pub fn is_heartbeat_stale(heartbeat: Option<i64>, now: i64, poll_ms: u64) -> bool {
    let Some(heartbeat) = heartbeat else {
        return false;
    };
    let threshold = std::cmp::max(5_i64, (poll_ms.saturating_mul(3) / 1000) as i64);
    now.saturating_sub(heartbeat) > threshold
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
        .enumerate()
        .map(|(index, category)| {
            let active = category == current_category;
            let label = config
                .categories
                .display_names
                .get(category)
                .map(String::as_str)
                .unwrap_or(category);
            let category_sessions = sessions_in_category(config, sessions, category);
            let colors = if active {
                &config.statusline.category.colors
            } else {
                &config.statusline.category.inactive_colors
            };
            let badge = category_badge_fragment(config, &category_sessions, colors);
            let format = if active {
                &config.statusline.category.format
            } else {
                &config.statusline.category.inactive_format
            };
            let body = format
                .replace("{category}", label)
                .replace("{name}", category)
                .replace("{count}", &category_sessions.len().to_string())
                .replace("{badge}", &badge);
            let segment = tmux_style_category(&config.statusline.category, &body, active);
            // クリックで `vt statusline-category switch N` を発火させる user range
            format!("#[range=user|{}]{segment}#[norange]", index + 1)
        })
        .collect::<Vec<_>>()
        .join("")
}

/// category 内の最悪 agent 状態(`@vde_session_state` 由来)を色付きグリフにする。
/// `statusline.category.show_badge`(既定 false)が無効な場合と、
/// 状態を持つ session が無い場合は空文字列。
fn category_badge_fragment(
    config: &Config,
    category_sessions: &[&SessionInfo],
    colors: &crate::config::SegmentColors,
) -> String {
    if !config.statusline.category.show_badge {
        return String::new();
    }
    let rank = |state: &str| match state {
        "blocked" => Some(0_u8),
        "working" => Some(1),
        "done" => Some(2),
        "idle" => Some(3),
        _ => None,
    };
    let worst = category_sessions
        .iter()
        .filter_map(|session| rank(&session.state).map(|rank| (rank, session.state.as_str())))
        .min_by_key(|(rank, _)| *rank);
    let Some((_, state)) = worst else {
        return String::new();
    };
    let glyphs = &config.badge.glyphs;
    let glyph = match state {
        "blocked" => &glyphs.blocked,
        "working" => &glyphs.working,
        "done" => &glyphs.done,
        _ => &glyphs.idle,
    };
    match config.badge.colors.for_state(state) {
        Some(color) => {
            let restore = colors.fg.as_deref().unwrap_or("default");
            format!("#[fg={color}]{glyph}#[fg={restore}]")
        }
        None => glyph.to_string(),
    }
}

/// `vt statusline-attention` の CLI エントリ。daemon(または fallback)の
/// 素のテキストを config の装飾で包む。空なら装飾ごと出さない。
pub fn statusline_attention(
    runner: &dyn TmuxRunner,
    env: &std::collections::BTreeMap<String, String>,
    config: &Config,
) -> Result<String> {
    let inner = crate::daemon::statusline_attention(runner, env)?;
    Ok(render_attention_segment(
        &config.statusline.attention,
        &inner,
    ))
}

pub fn render_attention_segment(style: &crate::config::AttentionConfig, inner: &str) -> String {
    if inner.is_empty() {
        return String::new();
    }
    let body = style.format.replace("{attention}", inner);
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
        body
    } else {
        format!("#[{}]{}#[default]", attrs.join(","), body)
    };
    format!("{}{}{}", style.prefix, styled, style.suffix)
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
    label: &str,
    index: usize,
    badge_style: BadgeStyle,
    colors: &crate::config::BadgeColors,
) -> String {
    if badge_style == BadgeStyle::Outer {
        let body = style
            .format
            .replace("{badge}", "")
            .replace("{session}", label)
            .replace("{index}", &(index + 1).to_string());
        let segment = tmux_style_segment(style, &body);
        if badge.is_empty() {
            return segment;
        }
        let glyph = match colors.for_state(state) {
            Some(color) => format!("#[fg={color}]{badge}#[default]"),
            None => badge.to_string(),
        };
        return format!("{glyph} {segment}");
    }
    let fragment = badge_fragment(badge, state, style, badge_style, colors);
    // format に {badge} があればそこへ(存在時のみ末尾スペース付き)、
    // 無ければ従来どおりラベル直前へ密着連結する。
    let (badge_token, label) = if style.format.contains("{badge}") {
        let token = if fragment.is_empty() {
            String::new()
        } else {
            format!("{fragment} ")
        };
        (token, label.to_string())
    } else {
        (String::new(), format!("{fragment}{label}"))
    };
    let body = style
        .format
        .replace("{badge}", &badge_token)
        .replace("{session}", &label)
        .replace("{index}", &(index + 1).to_string());
    tmux_style_segment(style, &body)
}

fn badge_fragment(
    badge: &str,
    state: &str,
    style: &SegmentStyle,
    badge_style: BadgeStyle,
    colors: &crate::config::BadgeColors,
) -> String {
    if badge.is_empty() {
        return String::new();
    }
    if badge_style == BadgeStyle::Plain {
        return badge.to_string();
    }
    match colors.for_state(state) {
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
    let use_inactive =
        !active && (!config.inactive_prefix.is_empty() || !config.inactive_suffix.is_empty());
    let (prefix, suffix) = if use_inactive {
        (&config.inactive_prefix, &config.inactive_suffix)
    } else {
        (&config.prefix, &config.suffix)
    };
    format!("{prefix}{styled}{suffix}")
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

    fn session_with_badge(name: &str, category: &str, badge: &str, state: &str) -> SessionInfo {
        let mut session = session(name, category);
        session.badge = badge.to_string();
        session.state = state.to_string();
        session
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
            rendered.contains("#[fg=#ff6b6b]▲#[fg=default]main"),
            "{rendered}"
        );
    }

    #[test]
    fn inline_badge_restores_configured_segment_fg() {
        let mut config = Config::default();
        config.badge.colors.working = "#00ff00".to_string();
        config.statusline.sessions.other.colors.fg = Some("white".to_string());
        let mut sub = session("sub", "work");
        sub.badge = "●".to_string();
        sub.state = "working".to_string();
        let rendered = render_statusline_sessions(&config, &[sub], "main", "work");
        assert!(
            rendered.contains("#[fg=#00ff00]●#[fg=white]sub"),
            "{rendered}"
        );
    }

    #[test]
    fn inline_badge_segment_renders_exact_markup() {
        let config = Config::default();
        let mut main = session("main", "work");
        main.badge = "▲".to_string();
        main.state = "blocked".to_string();
        let rendered = render_statusline_sessions(&config, &[main], "main", "work");

        assert_eq!(
            rendered,
            "#[bold] #[fg=#ff6b6b]▲#[fg=default]main #[default]"
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
        assert!(!rendered.contains("#[fg=#ff6b6b]"), "{rendered}");
    }

    #[test]
    fn outer_badge_places_glyph_on_bar_before_segment() {
        let mut config = Config::default();
        config.statusline.sessions.badge_style = BadgeStyle::Outer;
        config.statusline.sessions.current.colors.fg = Some("#ecebff".to_string());
        config.statusline.sessions.current.colors.bg = Some("#453f9e".to_string());
        let sessions = vec![session_with_badge("main", "work", "●", "working")];
        let rendered = render_statusline_sessions(&config, &sessions, "main", "work");
        assert!(
            rendered
                .contains("#[fg=#4fd08a]●#[default] #[bold,fg=#ecebff,bg=#453f9e] main #[default]"),
            "{rendered}"
        );
    }

    #[test]
    fn outer_badge_without_badge_renders_segment_only() {
        let mut config = Config::default();
        config.statusline.sessions.badge_style = BadgeStyle::Outer;
        let sessions = vec![session("main", "work")];
        let rendered = render_statusline_sessions(&config, &sessions, "main", "work");
        assert!(!rendered.contains("#[default] #[bold]"), "{rendered}");
        assert!(rendered.contains("#[bold] main #[default]"), "{rendered}");
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
    fn active_category_can_expand_name_while_inactive_uses_inactive_format() {
        let mut config = Config::default();
        config.statusline.category.format = "{category} {name} ".to_string();
        config.statusline.category.inactive_format = "{category} ".to_string();
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
            &[session("a", "work"), session("b", "private")],
            "work",
        );
        assert!(rendered.contains("W work "), "{rendered}");
        assert!(rendered.contains("P "), "{rendered}");
        assert!(!rendered.contains("P private"), "{rendered}");
    }

    #[test]
    fn show_index_uses_colon_separator() {
        let mut config = Config::default();
        config.statusline.sessions.show_index = true;
        let rendered = render_statusline_sessions(
            &config,
            &[session("main", "work"), session("sub", "work")],
            "main",
            "work",
        );
        // `{num}: {session}` 形式
        assert!(rendered.contains("1: main"), "{rendered}");
        assert!(rendered.contains("2: sub"), "{rendered}");
        assert!(!rendered.contains("1 main"), "{rendered}");
    }

    #[test]
    fn badge_placeholder_positions_badge_with_trailing_space_only_when_present() {
        let mut config = Config::default();
        config.statusline.sessions.show_index = true;
        config.statusline.sessions.current.format = " {badge}{session} ".to_string();
        config.statusline.sessions.other.format = " {badge}{session} ".to_string();
        let mut main = session("main", "work");
        main.badge = "▲".to_string();
        main.state = "blocked".to_string();
        let rendered =
            render_statusline_sessions(&config, &[main, session("sub", "work")], "main", "work");
        // badge あり: グリフの後にスペース1つ → "▲ 1: main"
        assert!(
            rendered.contains("#[fg=#ff6b6b]▲#[fg=default] 1: main"),
            "{rendered}"
        );
        // badge なし: 余分なスペースが残らない → " 2: sub "
        assert!(rendered.contains(" 2: sub "), "{rendered}");
        assert!(!rendered.contains("  2: sub"), "{rendered}");
    }

    #[test]
    fn category_badge_is_hidden_by_default_even_with_badge_placeholder() {
        let mut config = Config::default();
        config.statusline.category.format = "{badge}{category} ".to_string();
        let mut blocked = session("a", "work");
        blocked.state = "blocked".to_string();
        let rendered = render_statusline_category(&config, &[blocked], "work");
        assert!(rendered.contains("work "), "{rendered}");
        assert!(!rendered.contains('▲'), "{rendered}");
    }

    #[test]
    fn category_badge_shows_worst_state_with_color_and_restore() {
        let mut config = Config::default();
        config.statusline.category.format = "{badge}{category} ".to_string();
        config.statusline.category.show_badge = true;
        config.badge.colors.blocked = "#aa0000".to_string();
        config.statusline.category.colors.fg = Some("#1C1C1C".to_string());
        let mut blocked = session("a", "work");
        blocked.state = "blocked".to_string();
        let mut working = session("b", "work");
        working.state = "working".to_string();
        let rendered = render_statusline_category(&config, &[blocked, working], "work");
        assert!(
            rendered.contains("#[fg=#aa0000]▲#[fg=#1C1C1C]work"),
            "{rendered}"
        );
    }

    #[test]
    fn category_badge_is_empty_without_agent_state_and_idle_is_colored() {
        let mut config = Config::default();
        config.statusline.category.format = "{badge}{category} ".to_string();
        config.statusline.category.show_badge = true;
        // 状態なし → バッジなし
        let rendered = render_statusline_category(&config, &[session("a", "work")], "work");
        assert!(rendered.contains("work "), "{rendered}");
        assert!(
            !rendered.contains("▲") && !rendered.contains("○"),
            "{rendered}"
        );
        // idle のみ → 色付きの ○
        let mut idle = session("a", "work");
        idle.state = "idle".to_string();
        let rendered = render_statusline_category(&config, &[idle], "work");
        assert!(
            rendered.contains("#[fg=#6f6b85]○#[fg=default]work"),
            "{rendered}"
        );
    }

    #[test]
    fn attention_segment_defaults_to_red_text() {
        let config = Config::default();
        let rendered = render_attention_segment(&config.statusline.attention, "▲ proxy · perm 2m");
        assert_eq!(rendered, "#[fg=#ff6b6b]▲ proxy · perm 2m#[default]");
    }

    #[test]
    fn attention_segment_supports_pill_styling_and_empty_input() {
        let mut config = Config::default();
        config.statusline.attention.format = " {attention} ".to_string();
        config.statusline.attention.prefix = "<".to_string();
        config.statusline.attention.suffix = ">".to_string();
        config.statusline.attention.colors.fg = Some("#FFD9D6".to_string());
        config.statusline.attention.colors.bg = Some("#6E2A28".to_string());
        let rendered = render_attention_segment(&config.statusline.attention, "▲ proxy · perm 2m");
        assert_eq!(
            rendered,
            "<#[fg=#FFD9D6,bg=#6E2A28] ▲ proxy · perm 2m #[default]>"
        );
        // 空入力は装飾ごと出さない(pill の殻を残さない)
        assert_eq!(
            render_attention_segment(&config.statusline.attention, ""),
            ""
        );
    }

    #[test]
    fn sessions_join_with_separator_between_segments_only() {
        let mut config = Config::default();
        config.statusline.sessions.separator = "#[fg=#4a4860]│#[default]".to_string();
        let mut a = session("a", "work");
        a.id = "$1".to_string();
        let mut b = session("b", "work");
        b.id = "$2".to_string();
        let mut c = session("c", "work");
        c.id = "$3".to_string();
        let rendered = render_statusline_sessions(&config, &[a, b, c], "a", "work");
        // 3 セグメント → 区切りは間の 2 箇所だけ
        assert_eq!(rendered.matches('│').count(), 2, "{rendered}");
        // 先頭・末尾には付かない
        assert!(!rendered.starts_with("#[fg=#4a4860]│"), "{rendered}");
        assert!(!rendered.ends_with("│#[default]"), "{rendered}");
        // 区切りは range の外(#[norange] と次の #[range= の間)に入る
        assert!(
            rendered.contains("#[norange]#[fg=#4a4860]│#[default]#[range=session|$2]"),
            "{rendered}"
        );
    }

    #[test]
    fn sessions_without_separator_keep_tight_join() {
        let config = Config::default();
        let rendered = render_statusline_sessions(
            &config,
            &[session("a", "work"), session("b", "work")],
            "a",
            "work",
        );
        // 既定は区切りなし → 従来どおり密着連結
        assert!(!rendered.contains('│'), "{rendered}");
    }

    #[test]
    fn session_segments_are_wrapped_in_session_ranges() {
        let config = Config::default();
        let mut main = session("main", "work");
        main.id = "$3".to_string();
        let rendered = render_statusline_sessions(&config, &[main], "main", "work");
        assert!(rendered.starts_with("#[range=session|$3]"), "{rendered}");
        assert!(rendered.ends_with("#[norange]"), "{rendered}");
        // id が空(テスト用フィクスチャ等)なら range を付けない
        let rendered =
            render_statusline_sessions(&config, &[session("sub", "work")], "main", "work");
        assert!(!rendered.contains("#[range="), "{rendered}");
    }

    #[test]
    fn category_segments_are_wrapped_in_user_ranges() {
        let config = Config::default();
        let rendered = render_statusline_category(
            &config,
            &[session("a", "private"), session("b", "work")],
            "work",
        );
        assert!(rendered.contains("#[range=user|1]"), "{rendered}");
        assert!(rendered.contains("#[range=user|2]"), "{rendered}");
        assert!(rendered.contains("#[norange]"), "{rendered}");
    }

    #[test]
    fn category_uses_inactive_prefix_suffix_when_configured() {
        let mut config = Config::default();
        config.statusline.category.prefix = "<A>".to_string();
        config.statusline.category.suffix = "</A>".to_string();
        config.statusline.category.inactive_prefix = "<I>".to_string();
        config.statusline.category.inactive_suffix = "</I>".to_string();
        let rendered = render_statusline_category(
            &config,
            &[session("a", "work"), session("b", "private")],
            "work",
        );
        assert!(rendered.contains("<A>work </A>"), "{rendered}");
        assert!(rendered.contains("<I>private </I>"), "{rendered}");
    }

    #[test]
    fn category_falls_back_to_shared_prefix_suffix_when_inactive_unset() {
        let mut config = Config::default();
        config.statusline.category.prefix = "<P>".to_string();
        config.statusline.category.suffix = "</P>".to_string();
        let rendered = render_statusline_category(
            &config,
            &[session("a", "work"), session("b", "private")],
            "work",
        );
        assert!(rendered.contains("<P>work </P>"), "{rendered}");
        assert!(rendered.contains("<P>private </P>"), "{rendered}");
    }

    #[test]
    fn category_format_supports_count_placeholder() {
        let mut config = Config::default();
        config.statusline.category.format = "{category} {count} ".to_string();
        config.statusline.category.inactive_format = "{category} {count} ".to_string();
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

    #[test]
    fn stale_heartbeat_replaces_badges_with_question_mark() {
        assert!(is_heartbeat_stale(Some(940), 1000, 1000));
        assert!(!is_heartbeat_stale(Some(998), 1000, 1000));
        assert!(!is_heartbeat_stale(None, 1000, 1000));

        let config = Config::default();
        let mut main = session("main", "work");
        main.badge = "▲".to_string();
        main.state = "blocked".to_string();
        let rendered =
            render_statusline_sessions_with_stale(&config, &[main], "main", "work", true);
        assert!(rendered.contains("?main"), "{rendered}");
        assert!(!rendered.contains("▲main"), "{rendered}");
    }

    #[test]
    fn heartbeat_stale_boundary_is_strictly_greater_than_threshold() {
        assert!(!is_heartbeat_stale(Some(995), 1000, 1000));
        assert!(is_heartbeat_stale(Some(994), 1000, 1000));
        assert!(!is_heartbeat_stale(Some(988), 1000, 4000));
        assert!(is_heartbeat_stale(Some(987), 1000, 4000));
    }
}

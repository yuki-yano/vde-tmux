use anyhow::{Result, anyhow};

use crate::category::{resolve_category_for_session, sessions_in_category, sorted_categories};
use crate::config::{
    BadgeConfig, BadgeGlyphs, BadgeStyle, Config, SegmentColors, SegmentStyle,
    SessionBadgeChipConfig, SessionBadgeMode, StatuslineCategoryConfig,
};
use crate::daemon::session_badge::{BadgeState, badge_state, glyph_for_state};
use crate::hook::{AgentStatus, pane_rollup_level};
use crate::options::snapshot::detect_agent_from_command;
use crate::session::{
    SessionInfo, current_session_name, exact_session_target, find_session, list_sessions,
    switch_client, use_category,
};
use crate::tmux::TmuxRunner;
use crate::window::{WindowInfo, list_windows_for_target, select_window};

const PANE_FIELD_SEP: char = '\u{1f}';

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

pub fn statusline_windows(runner: &dyn TmuxRunner, config: &Config) -> Result<String> {
    let current_session = current_session_name(runner)?;
    let target = exact_session_target(&current_session);
    let windows = list_windows_for_target(runner, &target)?;
    Ok(render_statusline_windows(config, &windows))
}

pub fn statusline_pane(
    runner: &dyn TmuxRunner,
    config: &Config,
    target: Option<&str>,
    text_fg: Option<&str>,
) -> Result<String> {
    let format = pane_status_format();
    let output = if let Some(target) = target.filter(|target| !target.trim().is_empty()) {
        runner.run(&["display-message", "-p", "-t", target, &format])?
    } else {
        runner.run(&["display-message", "-p", &format])?
    };
    let pane = parse_pane_status(output.trim()).unwrap_or_default();
    let now = crate::sidebar::tree::now_epoch_secs();
    Ok(render_statusline_pane(config, &pane, text_fg, now))
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

pub fn switch_statusline_window(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    select_window(runner, target)
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

pub fn handle_statusline_click(
    runner: &dyn TmuxRunner,
    config: &Config,
    range: Option<&str>,
) -> Result<()> {
    let Some(range) = range.map(str::trim).filter(|range| !range.is_empty()) else {
        return Ok(());
    };
    if let Some(target) = range.strip_prefix("window:") {
        if !target.trim().is_empty() {
            return select_window(runner, target);
        }
        return Ok(());
    }
    if let Some(target) = range.strip_prefix("session:") {
        if !target.trim().is_empty() {
            runner.run(&["switch-client", "-t", target])?;
        }
        return Ok(());
    }
    if range.starts_with('$') {
        runner.run(&["switch-client", "-t", range])?;
        return Ok(());
    }
    if let Ok(index) = range.parse::<usize>() {
        if index == 0 {
            return Ok(());
        }
        return switch_statusline_category(runner, config, index - 1);
    }
    Ok(())
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
            let counts_mode = config.statusline.session_badge.mode == SessionBadgeMode::Counts;
            let state = if stale || counts_mode {
                ""
            } else {
                &session.state
            };
            let label = if config.statusline.sessions.show_index {
                format!("{}: {}", index + 1, session.name)
            } else {
                session.name.clone()
            };
            let badge_options = SessionBadgeRenderOptions {
                badge_style: config.statusline.sessions.badge_style,
                separate_badge: counts_mode,
                badge_config: &config.badge,
                chip_config: &config.statusline.session_badge.chip,
            };
            let segment =
                render_session_segment(style, badge, state, &label, index, &badge_options);
            if session.id.is_empty() {
                segment
            } else {
                format!("#[range=user|session:{}]{segment}#[norange]", session.id)
            }
        })
        .collect::<Vec<_>>()
        .join(&config.statusline.sessions.separator)
}

pub fn render_statusline_windows(config: &Config, windows: &[WindowInfo]) -> String {
    windows
        .iter()
        .map(|window| {
            let style = window_segment_style(config, window);
            let body = style
                .format
                .replace("{index}", &window.index)
                .replace("{window}", &window.name)
                .replace("{name}", &window.name)
                .replace("{id}", &window.id)
                .replace("{panes}", &window.panes.to_string())
                .replace("{command}", &window.command);
            let segment = tmux_style_segment(&style, &body);
            if window.id.is_empty() {
                segment
            } else {
                format!("#[range=user|window:{}]{segment}#[norange]", window.id)
            }
        })
        .collect::<Vec<_>>()
        .join(&config.statusline.windows.separator)
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
            format!("#[range=user|{}]{segment}#[norange]", index + 1)
        })
        .collect::<Vec<_>>()
        .join("")
}

fn pane_status_format() -> String {
    [
        "#{pane_id}",
        "#{pane_active}",
        "#{pane_current_command}",
        "#{@vde_agent}",
        "#{@vde_status}",
        "#{@vde_wait_reason}",
        "#{@vde_attention}",
        "#{@vde_started_at}",
        "#{@vde_completed_at}",
    ]
    .join(&PANE_FIELD_SEP.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PaneStatusInfo {
    pane_id: String,
    active: bool,
    current_command: String,
    agent: String,
    status: String,
    wait_reason: String,
    attention: String,
    started_at: String,
    completed_at: String,
}

fn parse_pane_status(raw: &str) -> Option<PaneStatusInfo> {
    let fields = raw.split(PANE_FIELD_SEP).collect::<Vec<_>>();
    if fields.len() != 9 {
        return None;
    }
    Some(PaneStatusInfo {
        pane_id: fields[0].to_string(),
        active: fields[1] == "1",
        current_command: fields[2].to_string(),
        agent: fields[3].to_string(),
        status: fields[4].to_string(),
        wait_reason: fields[5].to_string(),
        attention: fields[6].to_string(),
        started_at: fields[7].to_string(),
        completed_at: fields[8].to_string(),
    })
}

fn render_statusline_pane(
    config: &Config,
    pane: &PaneStatusInfo,
    text_fg_override: Option<&str>,
    now: i64,
) -> String {
    let style = if pane.active {
        &config.statusline.panes.current
    } else {
        &config.statusline.panes.other
    };
    let text_fg = normalize_tmux_color(
        text_fg_override
            .or(style.colors.fg.as_deref())
            .unwrap_or("default"),
    );
    let detail = render_statusline_pane_detail(config, pane, &text_fg, now);
    let body = render_statusline_pane_body(config, pane, &style.format, &detail, &text_fg, now);
    tmux_style_segment(style, &body)
}

fn render_statusline_pane_body(
    config: &Config,
    pane: &PaneStatusInfo,
    format: &str,
    detail: &str,
    text_fg: &str,
    now: i64,
) -> String {
    let agent = pane_agent_label(pane);
    let process = tmux_plain_text(&pane.current_command);
    let name = agent.clone().unwrap_or_else(|| process.clone());
    let (badge, status, time) = if agent.is_some() {
        let state = pane_badge_state(pane);
        (
            pane_badge_fragment(config, state, text_fg),
            pane_status_fragment(config, pane, state, text_fg),
            pane_time_fragment(config, pane, state, text_fg, now),
        )
    } else {
        (String::new(), String::new(), String::new())
    };

    format
        .replace("{pane}", &tmux_plain_text(&pane.pane_id))
        .replace("{id}", &tmux_plain_text(&pane.pane_id))
        .replace("{process}", &process)
        .replace(
            "{agent}",
            &tmux_plain_text(agent.as_deref().unwrap_or_default()),
        )
        .replace("{name}", &tmux_plain_text(&name))
        .replace("{badge}", &badge)
        .replace("{status}", &status)
        .replace("{time}", &time)
        .replace("{detail}", detail)
}

fn render_statusline_pane_detail(
    config: &Config,
    pane: &PaneStatusInfo,
    text_fg: &str,
    now: i64,
) -> String {
    let Some(agent) = pane_agent_label(pane) else {
        return tmux_plain_text(&pane.current_command);
    };
    let badge_state = pane_badge_state(pane);
    let glyph = glyph_for_state(badge_state, &config.badge.glyphs);
    let badge_color = config
        .badge
        .colors
        .for_state(badge_state.as_str())
        .unwrap_or("default");
    let text_fg = normalize_tmux_color(text_fg);
    let status = pane_status_label(pane, badge_state);
    let elapsed = pane_time_label(pane, badge_state, now)
        .map(|label| format!(" {label}"))
        .unwrap_or_default();
    format!(
        "#[fg={badge_color}]{glyph} #[fg={text_fg}]{} #[fg={text_fg}] #[fg={badge_color}]{status}{elapsed}#[fg={text_fg}]",
        tmux_plain_text(&agent)
    )
}

fn pane_badge_fragment(config: &Config, state: BadgeState, text_fg: &str) -> String {
    let glyph = glyph_for_state(state, &config.badge.glyphs);
    let badge_color = config
        .badge
        .colors
        .for_state(state.as_str())
        .unwrap_or("default");
    format!("#[fg={badge_color}]{glyph}#[fg={text_fg}]")
}

fn pane_status_fragment(
    config: &Config,
    pane: &PaneStatusInfo,
    state: BadgeState,
    text_fg: &str,
) -> String {
    let color = config
        .badge
        .colors
        .for_state(state.as_str())
        .unwrap_or("default");
    format!(
        "#[fg={color}]{}#[fg={text_fg}]",
        pane_status_label(pane, state)
    )
}

fn pane_time_fragment(
    config: &Config,
    pane: &PaneStatusInfo,
    state: BadgeState,
    text_fg: &str,
    now: i64,
) -> String {
    let Some(label) = pane_time_label(pane, state, now) else {
        return String::new();
    };
    let color = config
        .badge
        .colors
        .for_state(state.as_str())
        .unwrap_or("default");
    format!("#[fg={color}]{label}#[fg={text_fg}]")
}

fn pane_agent_label(pane: &PaneStatusInfo) -> Option<String> {
    let agent = pane.agent.trim();
    if !agent.is_empty() {
        return Some(crate::agent::display_agent_name(agent));
    }
    detect_agent_from_command(&pane.current_command).map(crate::agent::display_agent_name)
}

fn pane_badge_state(pane: &PaneStatusInfo) -> BadgeState {
    let Some(status) = parse_agent_status(&pane.status) else {
        return BadgeState::Working;
    };
    badge_state(
        pane_rollup_level(Some(status), non_empty(pane.wait_reason.as_str())),
        false,
    )
}

fn pane_status_label(pane: &PaneStatusInfo, state: BadgeState) -> &'static str {
    if state == BadgeState::Done {
        return "done";
    }
    match parse_agent_status(&pane.status) {
        Some(AgentStatus::Running) | None => "running",
        Some(AgentStatus::Waiting) => "waiting",
        Some(AgentStatus::Idle) => "idle",
        Some(AgentStatus::Error) => "error",
    }
}

fn pane_time_label(pane: &PaneStatusInfo, state: BadgeState, now: i64) -> Option<String> {
    let (epoch, suffix) = match state {
        BadgeState::Done | BadgeState::Idle => (pane.completed_at.trim(), " ago"),
        BadgeState::Blocked | BadgeState::Working => (pane.started_at.trim(), ""),
    };
    let epoch = epoch.parse::<i64>().ok()?;
    Some(format!("{}{}", short_elapsed_label(now - epoch), suffix))
}

fn short_elapsed_label(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 10 {
        return format!("{minutes}m{:02}s", secs % 60);
    }
    crate::sidebar::tree::humanize_secs(secs)
}

fn parse_agent_status(raw: &str) -> Option<AgentStatus> {
    match raw {
        "running" => Some(AgentStatus::Running),
        "waiting" => Some(AgentStatus::Waiting),
        "idle" => Some(AgentStatus::Idle),
        "error" => Some(AgentStatus::Error),
        _ => None,
    }
}

fn non_empty(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    (!raw.is_empty()).then_some(raw)
}

fn normalize_tmux_color(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() == 6 && raw.chars().all(|ch| ch.is_ascii_hexdigit()) {
        format!("#{raw}")
    } else if raw.is_empty() {
        "default".to_string()
    } else {
        raw.to_string()
    }
}

fn tmux_plain_text(raw: &str) -> String {
    raw.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .replace("#[", "# [")
}

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

struct SessionBadgeRenderOptions<'a> {
    badge_style: BadgeStyle,
    separate_badge: bool,
    badge_config: &'a BadgeConfig,
    chip_config: &'a SessionBadgeChipConfig,
}

fn render_session_segment(
    style: &SegmentStyle,
    badge: &str,
    state: &str,
    label: &str,
    index: usize,
    options: &SessionBadgeRenderOptions<'_>,
) -> String {
    if options.badge_style == BadgeStyle::Chip {
        return render_chip_session_segment(style, badge, state, label, index, options);
    }
    if options.badge_style == BadgeStyle::Outer {
        let body = style
            .format
            .replace("{badge}", "")
            .replace("{session}", label)
            .replace("{index}", &(index + 1).to_string());
        let segment = tmux_style_segment(style, &body);
        if badge.is_empty() {
            return segment;
        }
        let glyph = if options.separate_badge {
            counts_badge_fragment(badge, "default", options.badge_config)
        } else {
            match options.badge_config.colors.for_state(state) {
                Some(color) => format!("#[fg={color}]{badge}#[default]"),
                None => badge.to_string(),
            }
        };
        return format!("{glyph} {segment}");
    }
    let fragment = if options.separate_badge && options.badge_style != BadgeStyle::Plain {
        let restore = style.colors.fg.as_deref().unwrap_or("default");
        counts_badge_fragment(badge, restore, options.badge_config)
    } else {
        badge_fragment(
            badge,
            state,
            style,
            options.badge_style,
            &options.badge_config.colors,
        )
    };
    let (badge_token, label) = if style.format.contains("{badge}") {
        let token = if fragment.is_empty() {
            String::new()
        } else {
            format!("{fragment} ")
        };
        (token, label.to_string())
    } else if options.separate_badge && !fragment.is_empty() {
        let separator = if fragment.chars().last().is_some_and(char::is_whitespace) {
            ""
        } else {
            " "
        };
        (String::new(), format!("{fragment}{separator}{label}"))
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

fn render_chip_session_segment(
    style: &SegmentStyle,
    badge: &str,
    state: &str,
    label: &str,
    index: usize,
    options: &SessionBadgeRenderOptions<'_>,
) -> String {
    let body = style
        .format
        .replace("{badge}", "")
        .replace("{session}", label)
        .replace("{index}", &(index + 1).to_string());
    if badge.is_empty() {
        return tmux_style_segment(style, &body);
    }

    let chip_body = chip_badge_body(badge, state, options.separate_badge, options.badge_config);
    let chip_start = format!(
        "#[fg={}]{}#[bg={}] {chip_body} ",
        options.chip_config.bg, options.chip_config.cap_left, options.chip_config.bg
    );
    if let Some(segment_bg) = &style.colors.bg {
        return format!(
            "{chip_start}#[bg={segment_bg}]{}#[default] ",
            tmux_style_segment_without_prefix(style, &body)
        );
    }

    let chip_end = format!(
        "#[fg={},bg=default]{}#[default]",
        options.chip_config.bg, options.chip_config.cap_right
    );
    format!(
        "{chip_start}{chip_end} {}",
        tmux_style_segment(style, &body)
    )
}

fn chip_badge_body(
    badge: &str,
    state: &str,
    separate_badge: bool,
    badge_config: &BadgeConfig,
) -> String {
    if separate_badge {
        return counts_badge_fragment(badge, "default", badge_config);
    }
    match badge_config.colors.for_state(state) {
        Some(color) => format!("#[fg={color}]{badge}#[fg=default]"),
        None => badge.to_string(),
    }
}

fn counts_badge_fragment(badge: &str, restore_fg: &str, badge_config: &BadgeConfig) -> String {
    let tokens = badge.split_whitespace().collect::<Vec<_>>();
    let mut parts = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if index + 1 < tokens.len()
            && let Some(state) = count_glyph_state(tokens[index], &badge_config.glyphs)
            && tokens[index + 1].chars().all(|c| c.is_ascii_digit())
        {
            let color = match state {
                BadgeState::Blocked => &badge_config.colors.blocked,
                BadgeState::Working => &badge_config.colors.working,
                BadgeState::Done => &badge_config.colors.done,
                BadgeState::Idle => &badge_config.colors.idle,
            };
            parts.push(format!(
                "#[fg={color}]{} {}#[fg={restore_fg}]",
                tokens[index],
                tokens[index + 1]
            ));
            index += 2;
            continue;
        }
        parts.push(tokens[index].to_string());
        index += 1;
    }
    parts.join(" ")
}

fn count_glyph_state(token: &str, glyphs: &BadgeGlyphs) -> Option<BadgeState> {
    [
        BadgeState::Blocked,
        BadgeState::Working,
        BadgeState::Done,
        BadgeState::Idle,
    ]
    .into_iter()
    .find(|state| token == glyph_for_state(*state, glyphs))
}

fn window_segment_style(config: &Config, window: &WindowInfo) -> SegmentStyle {
    let mut style = if window.active {
        config.statusline.windows.current.clone()
    } else {
        config.statusline.windows.other.clone()
    };
    if window.last {
        apply_color_overlay(&mut style.colors, &config.statusline.windows.last);
    }
    if window.bell {
        apply_color_overlay(&mut style.colors, &config.statusline.windows.bell);
    } else if window.activity || window.silence {
        apply_color_overlay(&mut style.colors, &config.statusline.windows.activity);
    }
    style
}

fn apply_color_overlay(target: &mut SegmentColors, overlay: &SegmentColors) {
    if let Some(fg) = &overlay.fg {
        target.fg = Some(fg.clone());
    }
    if let Some(bg) = &overlay.bg {
        target.bg = Some(bg.clone());
    }
    if let Some(outer_bg) = &overlay.outer_bg {
        target.outer_bg = Some(outer_bg.clone());
    }
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
    format!(
        "{}{}{}",
        style.prefix,
        tmux_style_segment_body(style, body),
        style.suffix
    )
}

fn tmux_style_segment_without_prefix(style: &SegmentStyle, body: &str) -> String {
    format!("{}{}", tmux_style_segment_body(style, body), style.suffix)
}

fn tmux_style_segment_body(style: &SegmentStyle, body: &str) -> String {
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
    if attrs.is_empty() {
        body.to_string()
    } else {
        format!("#[{}]{}#[default]", attrs.join(","), body)
    }
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
    use crate::config::{BadgeStyle, Config, SessionBadgeMode};
    use crate::session::SessionInfo;
    use crate::window::WindowInfo;

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

    fn tmux_visible_width(text: &str) -> usize {
        let mut plain = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '#' && chars.peek() == Some(&'[') {
                let _ = chars.next();
                for style_ch in chars.by_ref() {
                    if style_ch == ']' {
                        break;
                    }
                }
            } else {
                plain.push(ch);
            }
        }
        unicode_width::UnicodeWidthStr::width(plain.as_str())
    }

    fn window(index: &str, id: &str, name: &str, active: bool) -> WindowInfo {
        WindowInfo {
            session: "main".to_string(),
            index: index.to_string(),
            id: id.to_string(),
            name: name.to_string(),
            panes: 1,
            active,
            last: false,
            bell: false,
            activity: false,
            silence: false,
            command: "zsh".to_string(),
        }
    }

    #[test]
    fn render_statusline_pane_detail_renders_agent_state_badge_and_elapsed() {
        let pane = PaneStatusInfo {
            pane_id: "%1".to_string(),
            current_command: "node".to_string(),
            agent: "codex".to_string(),
            status: "running".to_string(),
            started_at: "917".to_string(),
            ..PaneStatusInfo::default()
        };

        let rendered = render_statusline_pane_detail(&Config::default(), &pane, "1E1E2E", 1000);

        assert_eq!(
            rendered,
            "#[fg=#4fd08a]● #[fg=#1E1E2E]Codex #[fg=#1E1E2E] #[fg=#4fd08a]running 1m23s#[fg=#1E1E2E]"
        );
    }

    #[test]
    fn render_statusline_pane_detail_renders_idle_age() {
        let pane = PaneStatusInfo {
            pane_id: "%1".to_string(),
            current_command: "node".to_string(),
            agent: "codex".to_string(),
            status: "idle".to_string(),
            attention: "1".to_string(),
            completed_at: "185".to_string(),
            ..PaneStatusInfo::default()
        };

        let rendered = render_statusline_pane_detail(&Config::default(), &pane, "#9696CE", 1000);

        assert_eq!(
            rendered,
            "#[fg=#a8a8b2]○ #[fg=#9696CE]Codex #[fg=#9696CE] #[fg=#a8a8b2]idle 13m ago#[fg=#9696CE]"
        );
    }

    #[test]
    fn render_statusline_pane_detail_uses_process_for_non_agent() {
        let pane = PaneStatusInfo {
            pane_id: "%1".to_string(),
            current_command: "zsh".to_string(),
            ..PaneStatusInfo::default()
        };

        let rendered = render_statusline_pane_detail(&Config::default(), &pane, "default", 1000);

        assert_eq!(rendered, "zsh");
    }

    #[test]
    fn render_statusline_pane_uses_configured_format_and_placeholders() {
        let mut config = Config::default();
        config.statusline.panes.current.format =
            "{pane}|{badge}|{agent}|{status}|{time}|{detail}|{name}|{process}".to_string();
        config.statusline.panes.current.prefix = "<".to_string();
        config.statusline.panes.current.suffix = ">".to_string();
        config.statusline.panes.current.colors.fg = Some("#eeeeee".to_string());
        config.statusline.panes.current.colors.bg = None;
        let pane = PaneStatusInfo {
            pane_id: "%1".to_string(),
            active: true,
            current_command: "node".to_string(),
            agent: "codex".to_string(),
            status: "running".to_string(),
            started_at: "970".to_string(),
            ..PaneStatusInfo::default()
        };

        let rendered = render_statusline_pane(&config, &pane, None, 1000);

        assert_eq!(
            rendered,
            "<#[fg=#eeeeee]%1|#[fg=#4fd08a]●#[fg=#eeeeee]|Codex|#[fg=#4fd08a]running#[fg=#eeeeee]|#[fg=#4fd08a]30s#[fg=#eeeeee]|#[fg=#4fd08a]● #[fg=#eeeeee]Codex #[fg=#eeeeee] #[fg=#4fd08a]running 30s#[fg=#eeeeee]|Codex|node#[default]>"
        );
    }

    #[test]
    fn render_statusline_windows_uses_current_and_other_styles_with_ranges() {
        let mut config = Config::default();
        config.statusline.windows.current.colors.fg = Some("#20233a".to_string());
        config.statusline.windows.current.colors.bg = Some("#9d8cf5".to_string());
        config.statusline.windows.current.prefix = "#[fg=#9d8cf5]".to_string();
        config.statusline.windows.current.suffix =
            "#[fg=#9d8cf5,bg=default]#[default]".to_string();
        config.statusline.windows.other.colors.fg = Some("#9591ad".to_string());
        config.statusline.windows.separator = "#[fg=#8f8ba8]│#[default]".to_string();
        let rendered = render_statusline_windows(
            &config,
            &[
                window("1", "@1", "zsh", false),
                window("2", "@2", "editor", true),
            ],
        );

        assert!(
            rendered.contains("#[range=user|window:@1]#[fg=#9591ad] 1:zsh #[default]#[norange]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("#[range=user|window:@2]#[fg=#9d8cf5]#[bold,fg=#20233a,bg=#9d8cf5] 2:editor #[default]#[fg=#9d8cf5,bg=default]#[default]#[norange]"),
            "{rendered}"
        );
        assert_eq!(rendered.matches('│').count(), 1, "{rendered}");
    }

    #[test]
    fn render_statusline_windows_replaces_all_placeholders() {
        let mut config = Config::default();
        config.statusline.windows.other.format =
            " {index}:{window}:{name}:{id}:{panes}:{command} ".to_string();
        let mut item = window("3", "@7", "logs", false);
        item.panes = 4;
        item.command = "tail".to_string();

        let rendered = render_statusline_windows(&config, &[item]);

        assert!(rendered.contains(" 3:logs:logs:@7:4:tail "), "{rendered}");
    }

    #[test]
    fn render_statusline_windows_applies_bell_before_activity_overlay() {
        let mut config = Config::default();
        config.statusline.windows.other.colors.fg = Some("#9591ad".to_string());
        config.statusline.windows.bell.fg = Some("#ff6b6b".to_string());
        config.statusline.windows.activity.fg = Some("#ffaa00".to_string());
        let mut item = window("1", "@1", "alert", false);
        item.bell = true;
        item.activity = true;

        let rendered = render_statusline_windows(&config, &[item]);

        assert!(rendered.contains("#[fg=#ff6b6b] 1:alert "), "{rendered}");
        assert!(!rendered.contains("#ffaa00"), "{rendered}");
    }

    #[test]
    fn render_statusline_windows_applies_activity_and_last_overlays() {
        let mut config = Config::default();
        config.statusline.windows.other.colors.fg = Some("#9591ad".to_string());
        config.statusline.windows.last.bg = Some("#333333".to_string());
        config.statusline.windows.activity.fg = Some("#ff6b6b".to_string());
        let mut item = window("1", "@1", "active", false);
        item.last = true;
        item.silence = true;

        let rendered = render_statusline_windows(&config, &[item]);

        assert!(
            rendered.contains("#[fg=#ff6b6b,bg=#333333] 1:active "),
            "{rendered}"
        );
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
    fn counts_session_badge_colors_each_count_and_separates_label() {
        let mut config = Config::default();
        config.statusline.session_badge.mode = SessionBadgeMode::Counts;
        let mut main = session("main", "work");
        main.badge = "▲ 2 ● 1 ○ 5".to_string();
        main.state = "blocked".to_string();
        let rendered = render_statusline_sessions(&config, &[main], "main", "work");

        assert_eq!(
            rendered,
            "#[bold] #[fg=#ff6b6b]▲ 2#[fg=default] #[fg=#4fd08a]● 1#[fg=default] #[fg=#a8a8b2]○ 5#[fg=default] main #[default]"
        );
    }

    #[test]
    fn outer_counts_session_badge_colors_counts_before_segment() {
        let mut config = Config::default();
        config.statusline.session_badge.mode = SessionBadgeMode::Counts;
        config.statusline.sessions.badge_style = BadgeStyle::Outer;
        config.statusline.sessions.current.colors.fg = Some("#ecebff".to_string());
        config.statusline.sessions.current.colors.bg = Some("#453f9e".to_string());
        let mut main = session("main", "work");
        main.badge = "● 1 ○ 3".to_string();
        main.state = "working".to_string();
        let rendered = render_statusline_sessions(&config, &[main], "main", "work");

        assert_eq!(
            rendered,
            "#[fg=#4fd08a]● 1#[fg=default] #[fg=#a8a8b2]○ 3#[fg=default] #[bold,fg=#ecebff,bg=#453f9e] main #[default]"
        );
    }

    #[test]
    fn chip_counts_badge_wraps_normal_session_before_segment() {
        let mut config = Config::default();
        config.statusline.session_badge.mode = SessionBadgeMode::Counts;
        config.statusline.sessions.badge_style = BadgeStyle::Chip;
        let mut sub = session("sub", "work");
        sub.badge = "● 1 ○ 3".to_string();
        sub.state = "working".to_string();

        let rendered = render_statusline_sessions(&config, &[sub], "main", "work");

        assert_eq!(
            rendered,
            "#[fg=#303047]#[bg=#303047] #[fg=#4fd08a]● 1#[fg=default] #[fg=#a8a8b2]○ 3#[fg=default] #[fg=#303047,bg=default]#[default]  sub "
        );
    }

    #[test]
    fn chip_counts_badge_connects_to_current_segment_without_prefix() {
        let mut config = Config::default();
        config.statusline.session_badge.mode = SessionBadgeMode::Counts;
        config.statusline.sessions.badge_style = BadgeStyle::Chip;
        config.statusline.sessions.current.colors.fg = Some("#ecebff".to_string());
        config.statusline.sessions.current.colors.bg = Some("#453f9e".to_string());
        config.statusline.sessions.current.prefix = "<prefix>".to_string();
        config.statusline.sessions.current.suffix = "<suffix>".to_string();
        let mut main = session("main", "work");
        main.badge = "● 1 ○ 3".to_string();
        main.state = "working".to_string();

        let rendered = render_statusline_sessions(&config, &[main], "main", "work");

        assert_eq!(
            rendered,
            "#[fg=#303047]#[bg=#303047] #[fg=#4fd08a]● 1#[fg=default] #[fg=#a8a8b2]○ 3#[fg=default] #[bg=#453f9e]#[bold,fg=#ecebff,bg=#453f9e] main #[default]<suffix>#[default] "
        );
        assert!(!rendered.contains("<prefix>"), "{rendered}");
    }

    #[test]
    fn chip_current_and_other_segments_keep_equal_visible_width() {
        let mut config = Config::default();
        config.statusline.session_badge.mode = SessionBadgeMode::Counts;
        config.statusline.sessions.badge_style = BadgeStyle::Chip;
        config.statusline.sessions.current.colors.fg = Some("#ecebff".to_string());
        config.statusline.sessions.current.colors.bg = Some("#453f9e".to_string());
        config.statusline.sessions.current.suffix = ">".to_string();
        let mut main = session("main", "work");
        main.badge = "● 1 ○ 3".to_string();
        main.state = "working".to_string();

        let current = render_statusline_sessions(&config, &[main.clone()], "main", "work");
        let other = render_statusline_sessions(&config, &[main], "other", "work");

        assert_eq!(tmux_visible_width(&current), tmux_visible_width(&other));
    }

    #[test]
    fn chip_without_badge_renders_regular_segment_with_prefix() {
        let mut config = Config::default();
        config.statusline.sessions.badge_style = BadgeStyle::Chip;
        config.statusline.sessions.current.prefix = "<prefix>".to_string();
        config.statusline.sessions.current.suffix = "<suffix>".to_string();

        let rendered =
            render_statusline_sessions(&config, &[session("main", "work")], "main", "work");

        assert_eq!(rendered, "<prefix>#[bold] main #[default]<suffix>");
    }

    #[test]
    fn chip_rollup_badge_uses_state_color_inside_chip() {
        let mut config = Config::default();
        config.statusline.sessions.badge_style = BadgeStyle::Chip;
        let mut sub = session("sub", "work");
        sub.badge = "▲".to_string();
        sub.state = "blocked".to_string();

        let rendered = render_statusline_sessions(&config, &[sub], "main", "work");

        assert_eq!(
            rendered,
            "#[fg=#303047]#[bg=#303047] #[fg=#ff6b6b]▲#[fg=default] #[fg=#303047,bg=default]#[default]  sub "
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
        assert!(
            rendered.contains("#[fg=#ff6b6b]▲#[fg=default] 1: main"),
            "{rendered}"
        );
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
        let rendered = render_statusline_category(&config, &[session("a", "work")], "work");
        assert!(rendered.contains("work "), "{rendered}");
        assert!(
            !rendered.contains("▲") && !rendered.contains("○"),
            "{rendered}"
        );
        let mut idle = session("a", "work");
        idle.state = "idle".to_string();
        let rendered = render_statusline_category(&config, &[idle], "work");
        assert!(
            rendered.contains("#[fg=#a8a8b2]○#[fg=default]work"),
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
        assert_eq!(rendered.matches('│').count(), 2, "{rendered}");
        assert!(!rendered.starts_with("#[fg=#4a4860]│"), "{rendered}");
        assert!(!rendered.ends_with("│#[default]"), "{rendered}");
        assert!(
            rendered.contains("#[norange]#[fg=#4a4860]│#[default]#[range=user|session:$2]"),
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
        assert!(!rendered.contains('│'), "{rendered}");
    }

    #[test]
    fn session_segments_are_wrapped_in_session_ranges() {
        let config = Config::default();
        let mut main = session("main", "work");
        main.id = "$3".to_string();
        let rendered = render_statusline_sessions(&config, &[main], "main", "work");
        assert!(
            rendered.starts_with("#[range=user|session:$3]"),
            "{rendered}"
        );
        assert!(rendered.ends_with("#[norange]"), "{rendered}");
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

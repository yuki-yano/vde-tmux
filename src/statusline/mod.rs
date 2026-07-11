use anyhow::{Result, anyhow};

use crate::category::{resolve_category_for_session, sessions_in_category, sorted_categories};
use crate::config::{
    AgentBadgeConfig, BadgeConfig, BadgeGlyphs, BadgeStyle, Config, SegmentColors, SegmentStyle,
    SessionBadgeChipConfig, SessionBadgeMode, StatuslineCategoryConfig,
};
use crate::daemon::protocol::v2::{
    CategoryStatusPresentation, PanePresentation, SessionStatusPresentation, StatusSnapshot,
    WindowStatusPresentation,
};
use crate::daemon::session_badge::{
    BadgeState, BadgeStateCounts, agent_badge_value_from_counts, badge_value_from_counts,
    glyph_for_state,
};
use crate::session::{
    SessionInfo, current_session_name, find_session, list_sessions, switch_client, use_category,
};
use crate::tmux::TmuxRunner;
use crate::window::select_window;

pub fn switch_statusline_session(
    runner: &dyn TmuxRunner,
    config: &Config,
    index: usize,
) -> Result<()> {
    let sessions = list_sessions(runner)?;
    let current_session = current_session_name(runner)?;
    let current_category = current_category(config, &sessions, &current_session);
    let category_sessions = sessions_in_category(config, &sessions, &current_category);
    let Some(session) = category_sessions.get(index) else {
        return Ok(());
    };
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredStatusSegments {
    pub snapshot_revision: u64,
    pub summary: String,
    pub category: String,
    pub sessions: String,
    pub windows: String,
    pub attention: String,
}

pub fn render_structured_status_snapshot(
    config: &Config,
    snapshot: &StatusSnapshot,
) -> StructuredStatusSegments {
    StructuredStatusSegments {
        snapshot_revision: snapshot.snapshot_revision,
        summary: render_structured_summary(config, snapshot.summary),
        category: render_structured_categories(config, &snapshot.categories),
        sessions: render_structured_sessions(config, &snapshot.sessions),
        windows: render_structured_windows(config, &snapshot.windows),
        attention: render_structured_attention(config, &snapshot.attention),
    }
}

pub fn render_structured_pane_status(
    config: &Config,
    pane: &PanePresentation,
    now_epoch: i64,
) -> String {
    let style = if pane.active {
        &config.statusline.panes.current
    } else {
        &config.statusline.panes.other
    };
    let text_fg = normalize_tmux_color(style.colors.fg.as_deref().unwrap_or("default"));
    let process = structured_external_text(&pane.current_command);
    let path = structured_external_text(&pane.current_path);
    let pane_id = structured_external_text(&pane.pane_instance.pane_id);
    let (agent, badge, status, time, detail) = match &pane.resolved {
        Some(resolved) => {
            let agent = structured_external_text(&crate::agent::display_agent_name(
                resolved.canonical.agent.as_str(),
            ));
            let badge_state = resolved.badge;
            let badge = structured_pane_badge(config, badge_state, &text_fg);
            let status_label = structured_pane_status_label(&resolved.canonical, badge_state);
            let status =
                structured_pane_status_fragment(config, status_label, badge_state, &text_fg);
            let time_label =
                structured_pane_time_label(&resolved.canonical, badge_state, now_epoch);
            let time =
                structured_pane_time_fragment(config, time_label.as_deref(), badge_state, &text_fg);
            let detail = structured_pane_detail(
                config,
                &agent,
                status_label,
                time_label.as_deref(),
                badge_state,
                &text_fg,
            );
            (agent, badge, status, time, detail)
        }
        None => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            process.clone(),
        ),
    };
    let name = if agent.is_empty() { &process } else { &agent };
    let body = render_structured_template(
        &style.format,
        &[
            ("{pane}", pane_id.as_str()),
            ("{id}", pane_id.as_str()),
            ("{process}", process.as_str()),
            ("{path}", path.as_str()),
            ("{agent}", agent.as_str()),
            ("{name}", name.as_str()),
            ("{badge}", badge.as_str()),
            ("{status}", status.as_str()),
            ("{time}", time.as_str()),
            ("{detail}", detail.as_str()),
        ],
    );
    tmux_style_segment(style, &body)
}

fn render_structured_summary(config: &Config, mut counts: BadgeStateCounts) -> String {
    if !config.statusline.summary.enabled {
        return String::new();
    }
    if config.statusline.summary.hide_idle {
        counts.idle = 0;
    }
    crate::daemon::render_summary(
        &[
            (BadgeState::Blocked, counts.blocked),
            (BadgeState::Working, counts.working),
            (BadgeState::Done, counts.done),
            (BadgeState::Idle, counts.idle),
        ],
        &config.badge,
    )
}

fn render_structured_sessions(config: &Config, sessions: &[SessionStatusPresentation]) -> String {
    let mut sessions = sessions.iter().collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        left.created_at
            .unwrap_or(i64::MAX)
            .cmp(&right.created_at.unwrap_or(i64::MAX))
            .then_with(|| left.session_name.cmp(&right.session_name))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    sessions
        .into_iter()
        .enumerate()
        .map(|(index, session)| {
            let style = if session.active {
                &config.statusline.sessions.current
            } else {
                &config.statusline.sessions.other
            };
            let badge = badge_value_from_counts(
                session.counts,
                &config.badge.glyphs,
                config.statusline.session_badge.mode,
                &config.statusline.session_badge.suffix,
                config.statusline.session_badge.hide_idle,
            )
            .unwrap_or_default();
            let state = session
                .counts
                .rollup_state()
                .unwrap_or(BadgeState::Idle)
                .as_str();
            let name = structured_external_text(&session.session_name);
            let label = if config.statusline.sessions.show_index {
                format!("{}: {name}", index + 1)
            } else {
                name
            };
            let options = SessionBadgeRenderOptions {
                badge_style: config.statusline.sessions.badge_style,
                separate_badge: config.statusline.session_badge.mode == SessionBadgeMode::Counts,
                badge_config: &config.badge,
                chip_config: &config.statusline.session_badge.chip,
            };
            let segment =
                render_structured_session_segment(style, &badge, state, &label, index, &options);
            format!(
                "#[range=user|session:{}]{segment}#[norange]",
                session.session_id
            )
        })
        .collect::<Vec<_>>()
        .join(&config.statusline.sessions.separator)
}

fn render_structured_session_segment(
    style: &SegmentStyle,
    badge: &str,
    state: &str,
    label: &str,
    index: usize,
    options: &SessionBadgeRenderOptions<'_>,
) -> String {
    let index_label = (index + 1).to_string();
    if options.badge_style == BadgeStyle::Chip {
        let body = render_structured_template(
            &style.format,
            &[
                ("{badge}", ""),
                ("{session}", label),
                ("{index}", index_label.as_str()),
            ],
        );
        return if badge.is_empty() {
            tmux_style_segment(style, &body)
        } else {
            render_chip_agent_segment(
                style,
                badge,
                state,
                &body,
                options.separate_badge,
                options.badge_config,
                options.chip_config,
            )
        };
    }
    if options.badge_style == BadgeStyle::Outer {
        let body = render_structured_template(
            &style.format,
            &[
                ("{badge}", ""),
                ("{session}", label),
                ("{index}", index_label.as_str()),
            ],
        );
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
        counts_badge_fragment(
            badge,
            style.colors.fg.as_deref().unwrap_or("default"),
            options.badge_config,
        )
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
        (
            if fragment.is_empty() {
                String::new()
            } else {
                format!("{fragment} ")
            },
            label.to_string(),
        )
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
    let body = render_structured_template(
        &style.format,
        &[
            ("{badge}", badge_token.as_str()),
            ("{session}", label.as_str()),
            ("{index}", index_label.as_str()),
        ],
    );
    tmux_style_segment(style, &body)
}

fn render_structured_categories(
    config: &Config,
    categories: &[CategoryStatusPresentation],
) -> String {
    let mut categories = categories.iter().collect::<Vec<_>>();
    categories.sort_by(|left, right| {
        config
            .categories
            .order
            .get(&left.category)
            .copied()
            .unwrap_or(i64::MAX)
            .cmp(
                &config
                    .categories
                    .order
                    .get(&right.category)
                    .copied()
                    .unwrap_or(i64::MAX),
            )
            .then_with(|| left.category.cmp(&right.category))
    });
    if config.statusline.category.mode == "current" {
        categories.retain(|category| category.active);
    }
    categories
        .into_iter()
        .enumerate()
        .map(|(index, category)| {
            let active = category.active;
            let label = structured_external_text(
                config
                    .categories
                    .display_names
                    .get(&category.category)
                    .map(String::as_str)
                    .unwrap_or(&category.category),
            );
            let name = structured_external_text(&category.category);
            let badge = structured_agent_badge(
                config,
                category.counts,
                &config.statusline.category.agent_badge,
            );
            let colors = category_colors(&config.statusline.category, active);
            let badge_fragment = agent_badge_fragment_for_config(
                config,
                &config.statusline.category.agent_badge,
                config.statusline.category.badge_style,
                badge.as_ref(),
                colors,
            );
            let format = if active {
                &config.statusline.category.format
            } else {
                &config.statusline.category.inactive_format
            };
            let count = category.session_ids.len().to_string();
            let body = render_structured_template(
                format,
                &[
                    ("{category}", label.as_str()),
                    ("{name}", name.as_str()),
                    ("{count}", count.as_str()),
                    ("{badge}", badge_fragment.as_str()),
                ],
            );
            let segment = if config.statusline.category.badge_style == BadgeStyle::Chip {
                match badge.as_ref() {
                    Some((value, state)) => {
                        render_chip_category_segment(config, value, state, &body, active)
                    }
                    None => tmux_style_category(&config.statusline.category, &body, active),
                }
            } else {
                tmux_style_category(&config.statusline.category, &body, active)
            };
            format!("#[range=user|{}]{segment}#[norange]", index + 1)
        })
        .collect::<Vec<_>>()
        .join("")
}

fn render_structured_windows(config: &Config, windows: &[WindowStatusPresentation]) -> String {
    let mut windows = windows.iter().collect::<Vec<_>>();
    windows.sort_by(|left, right| {
        left.window_index
            .unwrap_or(i64::MAX)
            .cmp(&right.window_index.unwrap_or(i64::MAX))
            .then_with(|| left.window_id.cmp(&right.window_id))
    });
    windows
        .into_iter()
        .map(|window| {
            let style = structured_window_segment_style(config, window);
            let badge = structured_agent_badge(
                config,
                window.counts,
                &config.statusline.windows.agent_badge,
            );
            let badge_fragment = agent_badge_fragment_for_config(
                config,
                &config.statusline.windows.agent_badge,
                config.statusline.windows.badge_style,
                badge.as_ref(),
                &style.colors,
            );
            let index = window
                .window_index
                .map(|value| value.to_string())
                .unwrap_or_default();
            let name = structured_external_text(&window.window_name);
            let command =
                structured_external_text(window.current_command.as_deref().unwrap_or_default());
            let pane_count = window.pane_count.to_string();
            let state = window
                .counts
                .rollup_state()
                .unwrap_or(BadgeState::Idle)
                .as_str();
            let body = render_structured_template(
                &style.format,
                &[
                    ("{badge}", badge_fragment.as_str()),
                    ("{index}", index.as_str()),
                    ("{window}", name.as_str()),
                    ("{name}", name.as_str()),
                    ("{id}", window.window_id.as_str()),
                    ("{panes}", pane_count.as_str()),
                    ("{command}", command.as_str()),
                    ("{state}", state),
                ],
            );
            let segment = if config.statusline.windows.badge_style == BadgeStyle::Chip {
                match badge.as_ref() {
                    Some((value, state)) => render_chip_agent_segment(
                        &style,
                        value,
                        state,
                        &body,
                        config.statusline.windows.agent_badge.mode == SessionBadgeMode::Counts,
                        &config.badge,
                        &config.statusline.session_badge.chip,
                    ),
                    None => tmux_style_segment(&style, &body),
                }
            } else {
                tmux_style_segment(&style, &body)
            };
            format!(
                "#[range=user|window:{}]{segment}#[norange]",
                window.window_id
            )
        })
        .collect::<Vec<_>>()
        .join(&config.statusline.windows.separator)
}

fn structured_window_segment_style(
    config: &Config,
    window: &WindowStatusPresentation,
) -> SegmentStyle {
    let mut style = if window.active {
        config.statusline.windows.current.clone()
    } else {
        config.statusline.windows.other.clone()
    };
    if window.last {
        apply_color_overlay(&mut style.colors, &config.statusline.windows.last);
    }
    if window.bell.unwrap_or(false) {
        apply_color_overlay(&mut style.colors, &config.statusline.windows.bell);
    } else if window.activity.unwrap_or(false) || window.silence.unwrap_or(false) {
        apply_color_overlay(&mut style.colors, &config.statusline.windows.activity);
    }
    style
}

fn structured_agent_badge(
    config: &Config,
    counts: BadgeStateCounts,
    badge_config: &AgentBadgeConfig,
) -> Option<(String, String)> {
    if !badge_config.enabled {
        return None;
    }
    let value = agent_badge_value_from_counts(counts, &config.badge.glyphs, badge_config)?;
    let state = counts
        .rollup_state()
        .unwrap_or(BadgeState::Idle)
        .as_str()
        .to_string();
    Some((value, state))
}

fn render_structured_attention(
    config: &Config,
    entries: &[crate::daemon::protocol::v2::AttentionEntry],
) -> String {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.elapsed_seconds));
    let Some(entry) = entries.first() else {
        return String::new();
    };
    let reason = match entry.reason.as_deref() {
        Some(reason) if reason.to_ascii_lowercase().contains("permission") => "perm",
        Some(reason) if reason.starts_with("Other(") => "wait",
        Some(_) => "err",
        None => "err",
    };
    let elapsed = format!("{}m", entry.elapsed_seconds.max(0) / 60);
    let more = entries.len().saturating_sub(1);
    let suffix = if more > 0 {
        format!(" +{more}")
    } else {
        String::new()
    };
    let inner = format!(
        "▲ {} · {reason} {elapsed}{suffix}",
        structured_external_text(&entry.session_name)
    );
    render_attention_segment(&config.statusline.attention, &inner)
}

fn structured_pane_badge(config: &Config, state: BadgeState, text_fg: &str) -> String {
    let glyph = glyph_for_state(state, &config.badge.glyphs);
    let color = config
        .badge
        .colors
        .for_state(state.as_str())
        .unwrap_or("default");
    format!("#[fg={color}]{glyph}#[fg={text_fg}]")
}

fn structured_pane_status_label(
    state: &crate::pane_state::PaneState,
    badge: BadgeState,
) -> &'static str {
    if badge == BadgeState::Done {
        return "done";
    }
    match state.lifecycle {
        crate::pane_state::LifecycleState::Idle => "idle",
        crate::pane_state::LifecycleState::Running => "running",
        crate::pane_state::LifecycleState::Waiting { .. } => "waiting",
        crate::pane_state::LifecycleState::Error { .. } => "error",
    }
}

fn structured_pane_status_fragment(
    config: &Config,
    label: &str,
    state: BadgeState,
    text_fg: &str,
) -> String {
    let color = config
        .badge
        .colors
        .for_state(state.as_str())
        .unwrap_or("default");
    format!("#[fg={color}]{label}#[fg={text_fg}]")
}

fn structured_pane_time_label(
    state: &crate::pane_state::PaneState,
    badge: BadgeState,
    now_epoch: i64,
) -> Option<String> {
    let (epoch, suffix) = match badge {
        BadgeState::Done | BadgeState::Idle => (state.completed_at?, " ago"),
        BadgeState::Blocked | BadgeState::Working => (state.started_at?, ""),
    };
    Some(format!(
        "{}m{suffix}",
        now_epoch.saturating_sub(epoch).max(0) / 60
    ))
}

fn structured_pane_time_fragment(
    config: &Config,
    label: Option<&str>,
    state: BadgeState,
    text_fg: &str,
) -> String {
    let Some(label) = label else {
        return String::new();
    };
    let color = config
        .badge
        .colors
        .for_state(state.as_str())
        .unwrap_or("default");
    format!("#[fg={color}]{label}#[fg={text_fg}]")
}

fn structured_pane_detail(
    config: &Config,
    agent: &str,
    status: &str,
    time: Option<&str>,
    state: BadgeState,
    text_fg: &str,
) -> String {
    let glyph = glyph_for_state(state, &config.badge.glyphs);
    let color = config
        .badge
        .colors
        .for_state(state.as_str())
        .unwrap_or("default");
    let elapsed = time.map(|value| format!(" {value}")).unwrap_or_default();
    format!(
        "#[fg={color}]{glyph} #[fg={text_fg}]{agent} #[fg={text_fg}] #[fg={color}]{status}{elapsed}#[fg={text_fg}]"
    )
}

fn render_structured_template(template: &str, values: &[(&str, &str)]) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    while !remaining.is_empty() {
        if let Some((token, value)) = values
            .iter()
            .find(|(token, _)| remaining.starts_with(*token))
        {
            rendered.push_str(value);
            remaining = &remaining[token.len()..];
            continue;
        }
        let character = remaining
            .chars()
            .next()
            .expect("non-empty template has a first character");
        rendered.push(character);
        remaining = &remaining[character.len_utf8()..];
    }
    rendered
}

fn structured_external_text(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_control() {
            escaped.push(' ');
        } else if character == '#' {
            escaped.push_str("##");
        } else {
            escaped.push(character);
        }
    }
    escaped
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

fn agent_badge_fragment_for_config(
    config: &Config,
    agent_config: &AgentBadgeConfig,
    badge_style: BadgeStyle,
    badge: Option<&(String, String)>,
    colors: &crate::config::SegmentColors,
) -> String {
    if badge_style == BadgeStyle::Chip {
        return String::new();
    }
    let Some((value, state)) = badge else {
        return String::new();
    };
    agent_badge_fragment(
        config,
        agent_config,
        badge_style,
        value,
        state,
        colors.fg.as_deref(),
        colors.bg.as_deref(),
    )
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

fn render_chip_agent_segment(
    style: &SegmentStyle,
    badge: &str,
    state: &str,
    body: &str,
    separate_badge: bool,
    badge_config: &BadgeConfig,
    chip_config: &SessionBadgeChipConfig,
) -> String {
    if badge.is_empty() {
        return tmux_style_segment(style, body);
    }

    let chip_body = chip_badge_body(badge, state, separate_badge, badge_config);
    let chip_start = format!(
        "#[fg={}]{}#[bg={}] {chip_body} ",
        chip_config.bg, chip_config.cap_left, chip_config.bg
    );
    if let Some(segment_bg) = &style.colors.bg {
        return format!(
            "{chip_start}#[bg={segment_bg}]{}#[default] ",
            tmux_style_segment_without_prefix(style, body)
        );
    }

    let chip_end = format!(
        "#[fg={},bg=default]{}#[default]",
        chip_config.bg, chip_config.cap_right
    );
    format!("{chip_start}{chip_end} {}", tmux_style_segment(style, body))
}

fn render_chip_category_segment(
    config: &Config,
    badge: &str,
    state: &str,
    body: &str,
    active: bool,
) -> String {
    if badge.is_empty() {
        return tmux_style_category(&config.statusline.category, body, active);
    }

    let chip_config = &config.statusline.session_badge.chip;
    let counts_mode = config.statusline.category.agent_badge.mode == SessionBadgeMode::Counts;
    let chip_body = chip_badge_body(badge, state, counts_mode, &config.badge);
    let chip_start = format!(
        "#[fg={}]{}#[bg={}] {chip_body} ",
        chip_config.bg, chip_config.cap_left, chip_config.bg
    );
    let colors = category_colors(&config.statusline.category, active);
    let segment_bg = colors.bg.as_deref().unwrap_or(chip_config.bg.as_str());
    let styled = tmux_style_category_body_with_bg(
        &config.statusline.category,
        body,
        active,
        Some(segment_bg),
    );
    let suffix = if colors.bg.is_some() {
        category_affixes(&config.statusline.category, active)
            .1
            .to_string()
    } else {
        format!(
            "#[fg={},bg=default]{}#[default] ",
            chip_config.bg, chip_config.cap_right
        )
    };
    format!("{chip_start}#[bg={segment_bg}]{styled}{suffix}")
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

fn agent_badge_fragment(
    config: &Config,
    agent_config: &AgentBadgeConfig,
    badge_style: BadgeStyle,
    badge: &str,
    state: &str,
    restore_fg: Option<&str>,
    restore_bg: Option<&str>,
) -> String {
    if badge.is_empty() {
        return String::new();
    }
    let restore_fg = restore_fg.unwrap_or("default");
    let restore_bg = restore_bg.unwrap_or("default");
    let counts_mode = agent_config.mode == SessionBadgeMode::Counts;
    match badge_style {
        BadgeStyle::Plain => badge.to_string(),
        BadgeStyle::Chip => {
            let chip_config = &config.statusline.session_badge.chip;
            let chip_body = chip_badge_body(badge, state, counts_mode, &config.badge);
            format!(
                "#[fg={},bg={restore_bg}]{}#[fg={restore_fg},bg={}] {chip_body} #[fg={},bg={restore_bg}]{}#[fg={restore_fg},bg={restore_bg}]",
                chip_config.bg,
                chip_config.cap_left,
                chip_config.bg,
                chip_config.bg,
                chip_config.cap_right
            )
        }
        BadgeStyle::Inline | BadgeStyle::Outer => {
            if counts_mode {
                counts_badge_fragment(badge, restore_fg, &config.badge)
            } else {
                match config.badge.colors.for_state(state) {
                    Some(color) => format!("#[fg={color}]{badge}#[fg={restore_fg}]"),
                    None => badge.to_string(),
                }
            }
        }
    }
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

fn category_colors(config: &StatuslineCategoryConfig, active: bool) -> &SegmentColors {
    if active {
        &config.colors
    } else {
        &config.inactive_colors
    }
}

fn category_affixes(config: &StatuslineCategoryConfig, active: bool) -> (&str, &str) {
    let use_inactive =
        !active && (!config.inactive_prefix.is_empty() || !config.inactive_suffix.is_empty());
    if use_inactive {
        (&config.inactive_prefix, &config.inactive_suffix)
    } else {
        (&config.prefix, &config.suffix)
    }
}

fn tmux_style_category(config: &StatuslineCategoryConfig, body: &str, active: bool) -> String {
    let styled = tmux_style_category_body(config, body, active);
    let (prefix, suffix) = category_affixes(config, active);
    format!("{prefix}{styled}{suffix}")
}

fn tmux_style_category_body(config: &StatuslineCategoryConfig, body: &str, active: bool) -> String {
    tmux_style_category_body_with_bg(config, body, active, None)
}

fn tmux_style_category_body_with_bg(
    config: &StatuslineCategoryConfig,
    body: &str,
    active: bool,
    bg_override: Option<&str>,
) -> String {
    let colors = category_colors(config, active);
    let mut attrs = Vec::new();
    if config.bold && active {
        attrs.push("bold".to_string());
    }
    if let Some(fg) = &colors.fg {
        attrs.push(format!("fg={fg}"));
    }
    if let Some(bg) = bg_override.or(colors.bg.as_deref()) {
        attrs.push(format!("bg={bg}"));
    }
    if attrs.is_empty() {
        body.to_string()
    } else {
        format!("#[{}]{}#[default]", attrs.join(","), body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn structured_pane(
        command: &str,
        path: &str,
        active: bool,
        resolved: Option<(crate::pane_state::LifecycleState, BadgeState)>,
    ) -> PanePresentation {
        let pane_instance = crate::pane_state::PaneInstance {
            pane_id: "%7".to_string(),
            pane_pid: 700,
        };
        let resolved = resolved.map(|(lifecycle, badge)| crate::pane_state::ResolvedPaneState {
            canonical: crate::pane_state::PaneState {
                schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
                state_id: crate::pane_state::StateId::parse("00000000000000000000000000000007")
                    .unwrap(),
                revision: 3,
                pane_instance: pane_instance.clone(),
                agent: crate::pane_state::AgentKind::parse("codex").unwrap(),
                agent_session_id: None,
                agent_epoch: 1,
                agent_present: true,
                scan_verified: true,
                synthetic_completion_armed: false,
                lifecycle,
                run_seq: 1,
                completed_seq: 0,
                acknowledged_seq: 0,
                started_at: Some(60),
                completed_at: None,
                prompt: None,
                tasks: crate::pane_state::TaskState::default(),
                subagents: Vec::new(),
                worktree_activity: None,
            },
            window_id: "@2".to_string(),
            pane_id: pane_instance.pane_id.clone(),
            current_path: path.to_string(),
            badge,
        });
        PanePresentation {
            pane_instance,
            session_links: Vec::new(),
            window_id: "@2".to_string(),
            window_name: "editor".to_string(),
            current_path: path.to_string(),
            current_command: command.to_string(),
            active,
            stored: None,
            resolved,
            diagnostic: None,
        }
    }

    #[test]
    fn structured_snapshot_renders_projection_counts_and_metadata() {
        let mut config = Config::default();
        config.statusline.sessions.current.format = "{index}|{session}|{badge}".to_string();
        config.statusline.windows.current.format =
            "{index}|{window}|{panes}|{command}|{state}|{badge}".to_string();
        config.statusline.windows.agent_badge.enabled = true;
        config.statusline.category.format = "{category}|{name}|{count}|{badge}".to_string();
        config.statusline.category.agent_badge.enabled = true;
        config
            .categories
            .display_names
            .insert("work".to_string(), "Work".to_string());
        let snapshot = StatusSnapshot {
            snapshot_revision: 41,
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            summary: BadgeStateCounts {
                blocked: 1,
                working: 2,
                done: 0,
                idle: 1,
            },
            sessions: vec![SessionStatusPresentation {
                session_id: "$1".to_string(),
                session_name: "main".to_string(),
                category: Some("work".to_string()),
                attached: Some(true),
                created_at: Some(100),
                active: true,
                counts: BadgeStateCounts {
                    blocked: 1,
                    ..BadgeStateCounts::default()
                },
            }],
            windows: vec![WindowStatusPresentation {
                window_id: "@2".to_string(),
                window_name: "editor".to_string(),
                pane_count: 3,
                session_ids: vec!["$1".to_string()],
                window_index: Some(2),
                active: true,
                last: false,
                bell: Some(false),
                activity: Some(false),
                silence: Some(false),
                current_command: Some("nvim".to_string()),
                counts: BadgeStateCounts {
                    working: 1,
                    ..BadgeStateCounts::default()
                },
            }],
            categories: vec![CategoryStatusPresentation {
                category: "work".to_string(),
                session_ids: vec!["$1".to_string()],
                active: true,
                counts: BadgeStateCounts {
                    done: 1,
                    ..BadgeStateCounts::default()
                },
            }],
            attention: vec![crate::daemon::protocol::v2::AttentionEntry {
                pane_instance: crate::pane_state::PaneInstance {
                    pane_id: "%7".to_string(),
                    pane_pid: 700,
                },
                session_name: "main".to_string(),
                badge: BadgeState::Blocked,
                reason: Some("PermissionPrompt".to_string()),
                elapsed_seconds: 125,
            }],
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot);

        assert_eq!(rendered.snapshot_revision, 41);
        assert!(rendered.summary.contains("▲1"), "{}", rendered.summary);
        assert!(rendered.summary.contains("●2"), "{}", rendered.summary);
        assert!(rendered.summary.contains("○1"), "{}", rendered.summary);
        assert!(
            rendered.sessions.contains("1|main|"),
            "{}",
            rendered.sessions
        );
        assert!(
            rendered.sessions.contains("range=user|session:$1"),
            "{}",
            rendered.sessions
        );
        assert!(
            rendered.windows.contains("2|editor|3|nvim|working|"),
            "{}",
            rendered.windows
        );
        assert!(
            rendered.windows.contains("range=user|window:@2"),
            "{}",
            rendered.windows
        );
        assert!(
            rendered.category.contains("Work|work|1|"),
            "{}",
            rendered.category
        );
        assert!(
            rendered.attention.contains("▲ main · perm 2m"),
            "{}",
            rendered.attention
        );
    }

    #[test]
    fn structured_snapshot_escapes_external_text_without_recursive_template_expansion() {
        let mut config = Config::default();
        config.statusline.sessions.current.format = "{session}|{index}".to_string();
        config.statusline.windows.current.format = "{window}|{command}|{index}".to_string();
        let snapshot = StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            summary: BadgeStateCounts::default(),
            sessions: vec![SessionStatusPresentation {
                session_id: "$1".to_string(),
                session_name: "dev#[fg=red]\n{index}".to_string(),
                category: None,
                attached: None,
                created_at: None,
                active: true,
                counts: BadgeStateCounts::default(),
            }],
            windows: vec![WindowStatusPresentation {
                window_id: "@1".to_string(),
                window_name: "win#{command}".to_string(),
                pane_count: 1,
                session_ids: vec!["$1".to_string()],
                window_index: Some(4),
                active: true,
                last: false,
                bell: None,
                activity: None,
                silence: None,
                current_command: Some("sh#[bg=red]\t{window}".to_string()),
                counts: BadgeStateCounts::default(),
            }],
            categories: Vec::new(),
            attention: Vec::new(),
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot);

        assert!(
            rendered.sessions.contains("dev##[fg=red] {index}|1"),
            "{}",
            rendered.sessions
        );
        assert!(
            rendered
                .windows
                .contains("win##{command}|sh##[bg=red] {window}|4"),
            "{}",
            rendered.windows
        );
    }

    #[test]
    fn structured_pane_uses_resolved_badge_and_minute_clock() {
        let mut config = Config::default();
        config.statusline.panes.current.format =
            "{pane}|{agent}|{badge}|{status}|{time}|{detail}".to_string();
        let pane = structured_pane(
            "codex",
            "/tmp",
            true,
            Some((
                crate::pane_state::LifecycleState::Waiting {
                    reason: crate::pane_state::WaitReason::PermissionPrompt,
                },
                BadgeState::Blocked,
            )),
        );

        let rendered = render_structured_pane_status(&config, &pane, 180);

        assert!(rendered.contains("%7|Codex|"), "{rendered}");
        assert!(rendered.contains("waiting"), "{rendered}");
        assert!(rendered.matches("2m").count() >= 2, "{rendered}");
        assert!(
            rendered.contains(&config.badge.colors.blocked),
            "{rendered}"
        );
    }

    #[test]
    fn structured_non_agent_pane_preserves_process_and_path_with_safe_text() {
        let mut config = Config::default();
        config.statusline.panes.other.format = "{process}|{path}|{detail}".to_string();
        let pane = structured_pane("zsh#[fg=red]\n{path}", "/tmp/#{process}\t", false, None);

        let rendered = render_structured_pane_status(&config, &pane, 180);

        assert!(
            rendered.contains("zsh##[fg=red] {path}|/tmp/##{process} |zsh##[fg=red] {path}"),
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
}

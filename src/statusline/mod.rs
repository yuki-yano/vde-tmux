use anyhow::{Result, anyhow};
use base64::Engine as _;

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
use crate::session::Direction;
use crate::tmux::TmuxRunner;
use crate::window::select_window;

pub(crate) const STATUS_OPTION_CELL_BUDGET: usize = 80;

#[derive(Debug)]
struct StatusToken {
    rendered: String,
    compact: String,
    current: bool,
}

pub fn switch_statusline_session(
    runner: &dyn TmuxRunner,
    client_name: &str,
    session_id: &str,
    index: usize,
) -> Result<()> {
    let targets = displayed_targets(
        runner,
        session_id,
        crate::options::KEY_STATUS_SESSIONS,
        "session:",
    )?;
    let target = targets.get(index).ok_or_else(|| {
        anyhow!(
            "displayed session index {} is no longer available; wait for the status line to redraw",
            index + 1
        )
    })?;
    validate_tmux_target(target, '$', "session")?;
    runner.run(&["switch-client", "-c", client_name, "-t", target])?;
    Ok(())
}

pub fn cycle_statusline_session(
    runner: &dyn TmuxRunner,
    client_name: &str,
    session_id: &str,
    direction: Direction,
) -> Result<()> {
    let targets = displayed_targets(
        runner,
        session_id,
        crate::options::KEY_STATUS_SESSIONS,
        "session:",
    )?;
    let current = targets
        .iter()
        .position(|target| target == session_id)
        .ok_or_else(|| {
            anyhow!(
                "current session {session_id} is not present in the displayed status model; wait for the status line to redraw"
            )
        })?;
    let next = match direction {
        Direction::Next => (current + 1) % targets.len(),
        Direction::Previous => (current + targets.len() - 1) % targets.len(),
    };
    let target = &targets[next];
    validate_tmux_target(target, '$', "session")?;
    runner.run(&["switch-client", "-c", client_name, "-t", target])?;
    Ok(())
}

pub fn switch_statusline_window(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    select_window(runner, target)
}

pub fn switch_statusline_category(
    runner: &dyn TmuxRunner,
    config: &Config,
    client_name: &str,
    session_id: &str,
    index: usize,
) -> Result<()> {
    let (targets, _) = displayed_category_targets(runner, session_id)?;
    let target = targets
        .get(index)
        .ok_or_else(|| {
            anyhow!(
                "displayed category index {} is no longer available; wait for the status line to redraw",
                index + 1
            )
        })?;
    let category = decode_category_key(target)?;
    crate::session::use_category_for_client(runner, config, &category, client_name)
}

pub fn cycle_statusline_category(
    runner: &dyn TmuxRunner,
    config: &Config,
    client_name: &str,
    session_id: &str,
    direction: Direction,
) -> Result<()> {
    let sessions = crate::session::list_sessions(runner)?;
    let current_session = sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| anyhow!("current session {session_id} is not present in tmux"))?;
    let current_category = crate::category::resolve_category_for_session(config, current_session);
    let targets = crate::category::sorted_effective_categories(config, &sessions);
    if targets.len() <= 1 {
        return Err(anyhow!(
            "category cycle requires at least two categories with sessions"
        ));
    }
    let current_index = targets
        .iter()
        .position(|category| category == &current_category)
        .ok_or_else(|| {
            anyhow!("current category {current_category} is not present in the category cycle")
        })?;
    let next = match direction {
        Direction::Next => (current_index + 1) % targets.len(),
        Direction::Previous => (current_index + targets.len() - 1) % targets.len(),
    };
    crate::session::use_category_for_client_from_sessions(
        runner,
        config,
        &sessions,
        &targets[next],
        client_name,
    )
}

pub fn handle_statusline_click(
    runner: &dyn TmuxRunner,
    config: &Config,
    client_name: Option<&str>,
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
        validate_tmux_target(target, '$', "session")?;
        let client_name = client_name
            .ok_or_else(|| anyhow!("session click is missing an invoking tmux client"))?;
        runner.run(&["switch-client", "-c", client_name, "-t", target])?;
        return Ok(());
    }
    if let Some(target) = range.strip_prefix("category:") {
        let category = decode_category_key(target)?;
        let client_name = client_name
            .ok_or_else(|| anyhow!("category click is missing an invoking tmux client"))?;
        return crate::session::use_category_for_client(runner, config, &category, client_name);
    }
    if let Some(target) = range.strip_prefix("category-current:") {
        let category = decode_category_key(target)?;
        let client_name = client_name
            .ok_or_else(|| anyhow!("category click is missing an invoking tmux client"))?;
        return crate::session::use_category_for_client(runner, config, &category, client_name);
    }
    if range.starts_with('$') {
        validate_tmux_target(range, '$', "session")?;
        let client_name = client_name
            .ok_or_else(|| anyhow!("session click is missing an invoking tmux client"))?;
        runner.run(&["switch-client", "-c", client_name, "-t", range])?;
        return Ok(());
    }
    Ok(())
}

pub fn encode_category_key(category: &str) -> Result<String> {
    if category.len() > 256 {
        return Err(anyhow!("category key exceeds 256 UTF-8 bytes"));
    }
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(category.as_bytes()))
}

pub fn decode_category_key(encoded: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| anyhow!("invalid category target encoding: {error}"))?;
    if bytes.len() > 256 {
        return Err(anyhow!("category key exceeds 256 UTF-8 bytes"));
    }
    if base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes) != encoded {
        return Err(anyhow!(
            "category target encoding is not canonical base64url"
        ));
    }
    String::from_utf8(bytes).map_err(|error| anyhow!("category target is not UTF-8: {error}"))
}

fn displayed_targets(
    runner: &dyn TmuxRunner,
    session_id: &str,
    option: &str,
    prefix: &str,
) -> Result<Vec<String>> {
    validate_tmux_target(session_id, '$', "session")?;
    let rendered =
        crate::options::show_session_option(runner, session_id, option)?.ok_or_else(|| {
            anyhow!("{option} is empty for {session_id}; wait for the status line to redraw")
        })?;
    let targets = top_level_user_ranges(&rendered)?
        .into_iter()
        .filter_map(|range| range.strip_prefix(prefix).map(str::to_string))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return Err(anyhow!(
            "{option} has no trusted {prefix} targets for {session_id}; wait for the status line to redraw"
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for target in &targets {
        validate_tmux_target(target, '$', "session")?;
        if !seen.insert(target) {
            return Err(anyhow!(
                "{option} contains duplicate session targets; wait for the status line to redraw"
            ));
        }
    }
    Ok(targets)
}

fn displayed_category_targets(
    runner: &dyn TmuxRunner,
    session_id: &str,
) -> Result<(Vec<String>, String)> {
    validate_tmux_target(session_id, '$', "session")?;
    let option = crate::options::KEY_STATUS_CATEGORY;
    let rendered = crate::options::show_session_option(runner, session_id, option)?
        .ok_or_else(|| anyhow!("{option} is empty for {session_id}; wait for redraw"))?;
    let mut targets = Vec::new();
    let mut current = None;
    for range in top_level_user_ranges(&rendered)? {
        if let Some(target) = range.strip_prefix("category-current:") {
            if targets.iter().any(|existing| existing == target) {
                return Err(anyhow!(
                    "{option} contains duplicate category targets; wait for redraw"
                ));
            }
            if current.replace(target.to_string()).is_some() {
                return Err(anyhow!(
                    "{option} contains multiple active categories; wait for redraw"
                ));
            }
            targets.push(target.to_string());
        } else if let Some(target) = range.strip_prefix("category:") {
            if targets.iter().any(|existing| existing == target) {
                return Err(anyhow!(
                    "{option} contains duplicate category targets; wait for redraw"
                ));
            }
            targets.push(target.to_string());
        }
    }
    let current = current.ok_or_else(|| {
        anyhow!("{option} has no active category for {session_id}; wait for redraw")
    })?;
    if targets.is_empty() {
        return Err(anyhow!("{option} has no category targets; wait for redraw"));
    }
    Ok((targets, current))
}

fn top_level_user_ranges(rendered: &str) -> Result<Vec<String>> {
    let bytes = rendered.as_bytes();
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"##") {
            index += 2;
            continue;
        }
        if !bytes[index..].starts_with(b"#[") {
            index += 1;
            continue;
        }
        let Some(relative_end) = bytes[index + 2..].iter().position(|byte| *byte == b']') else {
            return Err(anyhow!(
                "displayed status option contains an unterminated tmux directive"
            ));
        };
        let end = index + 2 + relative_end;
        let directive = &rendered[index + 2..end];
        if let Some(range) = directive.strip_prefix("range=user|") {
            if depth == 0 {
                ranges.push(range.to_string());
            }
            depth = depth
                .checked_add(1)
                .ok_or_else(|| anyhow!("displayed status range nesting overflow"))?;
        } else if directive == "norange" {
            if depth == 0 {
                return Err(anyhow!(
                    "displayed status option contains an unmatched #[norange]"
                ));
            }
            depth -= 1;
        }
        index = end + 1;
    }
    if depth != 0 {
        return Err(anyhow!(
            "displayed status option contains an unclosed user range"
        ));
    }
    Ok(ranges)
}

fn validate_tmux_target(target: &str, prefix: char, kind: &str) -> Result<()> {
    let valid = target.strip_prefix(prefix).is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    if valid {
        Ok(())
    } else {
        Err(anyhow!("invalid {kind} target: {target}"))
    }
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
) -> Result<StructuredStatusSegments> {
    render_bounded_status_snapshot(config, snapshot)
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
    let process = structured_external_text(if pane.current_command.is_empty() {
        "(empty)"
    } else {
        &pane.current_command
    });
    let path = structured_external_text(&pane.current_path);
    let pane_id = structured_external_text(&pane.pane_instance.pane_id);
    let window = structured_external_text(if pane.window_name.is_empty() {
        "(unnamed)"
    } else {
        &pane.window_name
    });
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
        None => {
            let (badge, status) = if pane.diagnostic.is_some()
                || matches!(
                    &pane.stored,
                    Some(crate::pane_state::StoredStateDescriptor::Quarantined { .. })
                ) {
                ("?".to_string(), "invalid state".to_string())
            } else {
                ("—".to_string(), "no state".to_string())
            };
            (
                "(no agent)".to_string(),
                badge,
                status,
                "(empty)".to_string(),
                process.clone(),
            )
        }
    };
    let name = if pane.resolved.is_none() {
        &process
    } else {
        &agent
    };
    let format = &style.format;
    let body = render_structured_template(
        format,
        &[
            ("{pane}", pane_id.as_str()),
            ("{id}", pane_id.as_str()),
            ("{process}", process.as_str()),
            ("{path}", path.as_str()),
            ("{window}", window.as_str()),
            ("{agent}", agent.as_str()),
            ("{name}", name.as_str()),
            ("{badge}", badge.as_str()),
            ("{status}", status.as_str()),
            ("{state}", status.as_str()),
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

#[cfg(test)]
fn render_structured_sessions(config: &Config, sessions: &[SessionStatusPresentation]) -> String {
    let tokens = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| render_session_token(config, session, index))
        .collect::<Vec<_>>();
    tokens
        .into_iter()
        .map(|token| token.rendered)
        .collect::<Vec<_>>()
        .join(&config.statusline.sessions.separator)
}

fn render_session_token(
    config: &Config,
    session: &SessionStatusPresentation,
    index: usize,
) -> StatusToken {
    let style = if session.active {
        &config.statusline.sessions.current
    } else {
        &config.statusline.sessions.other
    };
    let badge = config
        .statusline
        .session_badge
        .enabled
        .then(|| {
            badge_value_from_counts(
                session.counts,
                &config.badge.glyphs,
                config.statusline.session_badge.mode,
                &config.statusline.session_badge.suffix,
                config.statusline.session_badge.hide_idle,
            )
        })
        .flatten()
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
    let segment = render_structured_session_segment(style, &badge, state, &label, index, &options);
    StatusToken {
        compact: format!(
            "#[range=user|session:{}]{}#[norange]",
            session.session_id, session.session_id
        ),
        rendered: format!(
            "#[range=user|session:{}]{segment}#[norange]",
            session.session_id
        ),
        current: session.active,
    }
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

fn structured_category_tokens(
    config: &Config,
    categories: &[CategoryStatusPresentation],
) -> Result<Vec<StatusToken>> {
    let mut categories = categories.iter().collect::<Vec<_>>();
    if config.statusline.category.mode == "current" {
        categories.retain(|category| category.active);
    }
    categories
        .into_iter()
        .enumerate()
        .map(|(index, category)| -> Result<StatusToken> {
            let active = category.active;
            let label = structured_external_text(if category.category.is_empty() {
                "uncategorized"
            } else {
                config
                    .categories
                    .display_names
                    .get(&category.category)
                    .map(String::as_str)
                    .unwrap_or(&category.category)
            });
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
            let target = encode_category_key(&category.category)?;
            let range = if active {
                format!("category-current:{target}")
            } else {
                format!("category:{target}")
            };
            let compact_label = format!("cat:{}", index + 1);
            Ok(StatusToken {
                compact: format!("#[range=user|{range}]{compact_label}#[norange]"),
                rendered: format!("#[range=user|{range}]{segment}#[norange]"),
                current: active,
            })
        })
        .collect::<Result<Vec<_>>>()
}

fn structured_window_tokens(
    config: &Config,
    windows: &[WindowStatusPresentation],
) -> Vec<StatusToken> {
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
            let name = structured_external_text(if window.window_name.is_empty() {
                "(unnamed)"
            } else {
                &window.window_name
            });
            let command = structured_external_text(
                window
                    .current_command
                    .as_deref()
                    .filter(|command| !command.is_empty())
                    .unwrap_or("(empty)"),
            );
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
            let rendered = format!(
                "#[range=user|window:{}]{segment}#[norange]",
                window.window_id
            );
            StatusToken {
                compact: format!(
                    "#[range=user|window:{}]{}#[norange]",
                    window.window_id, window.window_id
                ),
                rendered,
                current: window.active,
            }
        })
        .collect::<Vec<_>>()
}

fn render_bounded_status_snapshot(
    config: &Config,
    snapshot: &StatusSnapshot,
) -> Result<StructuredStatusSegments> {
    let mut category_tokens = structured_category_tokens(config, &snapshot.categories)?;
    let session_tokens = snapshot
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| render_session_token(config, session, index))
        .collect::<Vec<_>>();
    let mut window_tokens = structured_window_tokens(config, &snapshot.windows);
    let mut category_included = category_tokens
        .iter()
        .map(|token| token.current)
        .collect::<Vec<_>>();
    // Session navigation uses the stable targets embedded in this exact rendered model. Keep
    // every ordered session visible so the status line and next/previous actions never collapse
    // to the current session plus a non-actionable `+N` summary.
    let session_included = vec![true; session_tokens.len()];
    let mut window_included = window_tokens
        .iter()
        .map(|token| token.current)
        .collect::<Vec<_>>();
    let (attention_full, attention_compact) =
        structured_attention_variants(config, &snapshot.attention);
    let mut attention = attention_full;
    let summary_candidate = render_structured_summary(config, snapshot.summary);
    let mut summary = summary_candidate.clone();

    // Keep the complete session action model independent from the bounded status content.
    // Within the remaining options, summary is useful context but must never displace attention
    // or the current category/window identities.
    if status_projection_width(
        &summary,
        &category_tokens,
        &category_included,
        &session_tokens,
        &session_included,
        &window_tokens,
        &window_included,
        &attention,
        config,
    ) > STATUS_OPTION_CELL_BUDGET
    {
        summary.clear();
    }

    if status_projection_width(
        &summary,
        &category_tokens,
        &category_included,
        &session_tokens,
        &session_included,
        &window_tokens,
        &window_included,
        &attention,
        config,
    ) > STATUS_OPTION_CELL_BUDGET
    {
        compact_current_tokens(&mut window_tokens);
    }
    if status_projection_width(
        &summary,
        &category_tokens,
        &category_included,
        &session_tokens,
        &session_included,
        &window_tokens,
        &window_included,
        &attention,
        config,
    ) > STATUS_OPTION_CELL_BUDGET
    {
        compact_current_tokens(&mut category_tokens);
    }
    if status_projection_width(
        &summary,
        &category_tokens,
        &category_included,
        &session_tokens,
        &session_included,
        &window_tokens,
        &window_included,
        &attention,
        config,
    ) > STATUS_OPTION_CELL_BUDGET
    {
        attention = attention_compact;
    }

    // Compaction can make room for summary again. Reconsider it before any inactive peer so the
    // published projection continues to follow the documented semantic priority.
    if summary.is_empty() && !summary_candidate.is_empty() {
        summary = summary_candidate;
        if status_projection_width(
            &summary,
            &category_tokens,
            &category_included,
            &session_tokens,
            &session_included,
            &window_tokens,
            &window_included,
            &attention,
            config,
        ) > STATUS_OPTION_CELL_BUDGET
        {
            summary.clear();
        }
    }

    for index in 0..category_tokens.len() {
        try_include_status_peer(
            index,
            &mut category_included,
            &category_tokens,
            &session_tokens,
            &session_included,
            &window_tokens,
            &window_included,
            &summary,
            &attention,
            config,
        );
    }
    for index in 0..window_tokens.len() {
        if window_included[index] {
            continue;
        }
        window_included[index] = true;
        if status_projection_width(
            &summary,
            &category_tokens,
            &category_included,
            &session_tokens,
            &session_included,
            &window_tokens,
            &window_included,
            &attention,
            config,
        ) > STATUS_OPTION_CELL_BUDGET
        {
            window_included[index] = false;
        }
    }

    let category = render_selected_status_tokens(&category_tokens, &category_included, "");
    let sessions = render_selected_sessions(
        config,
        &snapshot.sessions,
        &session_tokens,
        &session_included,
    );
    let windows = render_selected_status_tokens(
        &window_tokens,
        &window_included,
        &config.statusline.windows.separator,
    );
    Ok(StructuredStatusSegments {
        snapshot_revision: snapshot.snapshot_revision,
        summary,
        category,
        sessions,
        windows,
        attention,
    })
}

#[allow(clippy::too_many_arguments)]
fn try_include_status_peer(
    index: usize,
    included: &mut [bool],
    category_tokens: &[StatusToken],
    session_tokens: &[StatusToken],
    session_included: &[bool],
    window_tokens: &[StatusToken],
    window_included: &[bool],
    summary: &str,
    attention: &str,
    config: &Config,
) {
    if included[index] {
        return;
    }
    included[index] = true;
    if status_projection_width(
        summary,
        category_tokens,
        included,
        session_tokens,
        session_included,
        window_tokens,
        window_included,
        attention,
        config,
    ) > STATUS_OPTION_CELL_BUDGET
    {
        included[index] = false;
    }
}

#[allow(clippy::too_many_arguments)]
fn status_projection_width(
    summary: &str,
    category_tokens: &[StatusToken],
    category_included: &[bool],
    _session_tokens: &[StatusToken],
    _session_included: &[bool],
    window_tokens: &[StatusToken],
    window_included: &[bool],
    attention: &str,
    config: &Config,
) -> usize {
    tmux_display_width(summary)
        + selected_status_tokens_width(category_tokens, category_included, "")
        + selected_status_tokens_width(
            window_tokens,
            window_included,
            &config.statusline.windows.separator,
        )
        + tmux_display_width(attention)
}

fn selected_status_tokens_width(
    tokens: &[StatusToken],
    included: &[bool],
    separator: &str,
) -> usize {
    tmux_display_width(&render_selected_status_tokens(tokens, included, separator))
}

fn render_selected_status_tokens(
    tokens: &[StatusToken],
    included: &[bool],
    separator: &str,
) -> String {
    let rendered = tokens
        .iter()
        .zip(included)
        .filter(|(_, included)| **included)
        .map(|(token, _)| token.rendered.clone())
        .collect::<Vec<_>>();
    join_bounded_tokens(
        rendered,
        included.iter().filter(|included| !**included).count(),
        separator,
    )
}

fn render_selected_sessions(
    config: &Config,
    sessions: &[SessionStatusPresentation],
    selected_tokens: &[StatusToken],
    included: &[bool],
) -> String {
    let mut displayed_index = 0usize;
    let rendered = sessions
        .iter()
        .zip(selected_tokens)
        .zip(included)
        .filter_map(|((session, selected_token), included)| {
            if !*included {
                return None;
            }
            let token = render_session_token(config, session, displayed_index);
            displayed_index += 1;
            Some(
                if token.current && selected_token.rendered == selected_token.compact {
                    token.compact
                } else {
                    token.rendered
                },
            )
        })
        .collect::<Vec<_>>();
    join_bounded_tokens(
        rendered,
        sessions.len().saturating_sub(displayed_index),
        &config.statusline.sessions.separator,
    )
}

fn compact_current_tokens(tokens: &mut [StatusToken]) {
    for token in tokens.iter_mut().filter(|token| token.current) {
        token.rendered = token.compact.clone();
    }
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

#[cfg(test)]
fn render_structured_attention(
    config: &Config,
    entries: &[crate::daemon::protocol::v2::AttentionEntry],
) -> String {
    let (full, compact) = structured_attention_variants(config, entries);
    if tmux_display_width(&full) <= STATUS_OPTION_CELL_BUDGET {
        full
    } else {
        compact
    }
}

fn structured_attention_variants(
    config: &Config,
    entries: &[crate::daemon::protocol::v2::AttentionEntry],
) -> (String, String) {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.elapsed_seconds));
    let Some(entry) = entries.first() else {
        return (String::new(), String::new());
    };
    let reason = match entry.reason.as_deref() {
        Some(reason) if reason.to_ascii_lowercase().contains("permission") => "perm",
        Some(reason) if reason.starts_with("Other(") => "wait",
        Some(_) => "err",
        None => "err",
    };
    let elapsed = format_bounded_duration(entry.elapsed_seconds);
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
    (
        render_attention_segment(&config.statusline.attention, &inner),
        format!("▲ blocked{suffix}"),
    )
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
        "{}{suffix}",
        format_bounded_duration(now_epoch.saturating_sub(epoch))
    ))
}

/// Compact elapsed time shared by statusline attention and pane-border state.
pub(crate) fn format_bounded_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 10 {
        return format!("{minutes}m{:02}s", seconds % 60);
    }
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        let remaining_minutes = minutes % 60;
        return if remaining_minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{remaining_minutes}m")
        };
    }
    format!("{}d", hours / 24)
}

fn join_bounded_tokens(rendered: Vec<String>, omitted: usize, separator: &str) -> String {
    let mut rendered = rendered.join(separator);
    if omitted > 0 {
        if !rendered.is_empty() {
            rendered.push(' ');
        }
        rendered.push_str(&format!("+{omitted}"));
    }
    rendered
}

fn tmux_display_width(rendered: &str) -> usize {
    use unicode_width::UnicodeWidthChar;

    let mut width = 0usize;
    let mut remaining = rendered;
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix("##") {
            width += 1;
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix("#[")
            && let Some(end) = rest.find(']')
        {
            remaining = &rest[end + 1..];
            continue;
        }
        let character = remaining
            .chars()
            .next()
            .expect("non-empty status text has a first character");
        width += UnicodeWidthChar::width(character).unwrap_or(0);
        remaining = &remaining[character.len_utf8()..];
    }
    width
}

pub(crate) fn structured_status_display_width(rendered: &str) -> usize {
    tmux_display_width(rendered)
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
    use crate::config::{BadgeStyle, Config, SessionBadgeMode};
    use crate::tmux::mock::MockTmuxRunner;

    fn status_session(id: &str, name: &str, active: bool) -> SessionStatusPresentation {
        SessionStatusPresentation {
            session_id: id.to_string(),
            session_name: name.to_string(),
            category: Some("work".to_string()),
            attached: Some(active),
            created_at: Some(1),
            active,
            counts: BadgeStateCounts {
                blocked: 1,
                working: 1,
                done: 1,
                idle: 1,
            },
        }
    }

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
            pane_width: 80,
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

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

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
            rendered.category.contains(&format!(
                "range=user|category-current:{}",
                encode_category_key("work").unwrap()
            )),
            "{}",
            rendered.category
        );
        assert!(
            rendered.attention.contains("▲ main · perm 2m05s"),
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

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

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
    fn structured_pane_uses_resolved_badge_and_second_precision_clock() {
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
        assert!(rendered.matches("2m00s").count() >= 2, "{rendered}");
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
    fn bounded_duration_preserves_seconds_below_ten_minutes() {
        assert_eq!(format_bounded_duration(-1), "0s");
        assert_eq!(format_bounded_duration(30), "30s");
        assert_eq!(format_bounded_duration(90), "1m30s");
        assert_eq!(format_bounded_duration(599), "9m59s");
        assert_eq!(format_bounded_duration(600), "10m");
        assert_eq!(format_bounded_duration(720), "12m");
        assert_eq!(format_bounded_duration(5_400), "1h30m");
        assert_eq!(format_bounded_duration(172_800), "2d");
    }

    #[test]
    fn tmux_intrinsic_width_counts_ascii_cjk_and_emoji_but_not_styles() {
        assert_eq!(tmux_display_width("#[fg=red]abc日本🚀#[default]"), 9);
        assert_eq!(tmux_display_width("##[literal]"), 10);
    }

    #[test]
    fn pane_border_always_uses_configured_format_at_width_boundaries() {
        let mut config = Config::default();
        config.statusline.panes.current.format =
            "CUSTOM:{window}:{agent}:{status}:{time}:{process}".to_string();
        let mut pane = structured_pane("", "/tmp", true, None);
        pane.window_name.clear();

        for width in [31, 32, 63, 64] {
            pane.pane_width = width;
            let rendered = render_structured_pane_status(&config, &pane, 30);
            assert!(
                rendered.contains("CUSTOM:(unnamed):(no agent):no state:(empty):(empty)"),
                "width {width}: {rendered}"
            );
        }
    }

    #[test]
    fn pane_border_does_not_infer_an_invalid_unresolved_state_as_idle() {
        let mut config = Config::default();
        config.statusline.panes.current.format =
            "{agent}|{badge}|{state}|{time}|{detail}".to_string();
        let mut pane = structured_pane("zsh", "/tmp", true, None);
        pane.stored = Some(crate::pane_state::StoredStateDescriptor::Quarantined {
            quarantine_id: "q1".to_string(),
        });
        pane.diagnostic = Some(crate::pane_state::PaneStateLoadError {
            pane_instance: pane.pane_instance.clone(),
            quarantine_id: "q1".to_string(),
            message: "invalid custom state".to_string(),
        });

        let rendered = render_structured_pane_status(&config, &pane, 30);

        assert!(
            rendered.contains("(no agent)|?|invalid state|(empty)|zsh"),
            "{rendered}"
        );
        assert!(!rendered.contains("idle"), "{rendered}");
    }

    #[test]
    fn session_option_keeps_every_unicode_token_while_other_options_remain_bounded() {
        let mut config = Config::default();
        config.statusline.sessions.show_index = false;
        config.statusline.sessions.current.format = " {session} ".to_string();
        config.statusline.sessions.other.format = " {session} ".to_string();
        config.statusline.sessions.separator = "·".to_string();
        config.statusline.windows.current.format = " {window} ".to_string();
        config.statusline.windows.other.format = " {window} ".to_string();
        config.statusline.windows.separator = "·".to_string();
        config.statusline.category.format = " {category} ".to_string();
        config.statusline.category.inactive_format = " {category} ".to_string();

        let sessions = (0..12)
            .map(|index| SessionStatusPresentation {
                session_id: format!("${}", index + 1),
                session_name: if index == 5 {
                    "現在🚀".to_string()
                } else {
                    format!("日本語セッション{index}🚀")
                },
                category: Some("work".to_string()),
                attached: None,
                created_at: None,
                active: index == 5,
                counts: BadgeStateCounts::default(),
            })
            .collect::<Vec<_>>();
        let windows = (0..12)
            .map(|index| WindowStatusPresentation {
                window_id: format!("@{}", index + 1),
                window_name: if index == 7 {
                    "現在の窓🪟".to_string()
                } else {
                    format!("編集ウィンドウ{index}🪟")
                },
                pane_count: 1,
                session_ids: vec!["$1".to_string()],
                window_index: Some(index),
                active: index == 7,
                last: false,
                bell: None,
                activity: None,
                silence: None,
                current_command: None,
                counts: BadgeStateCounts::default(),
            })
            .collect::<Vec<_>>();
        let categories = (0..12)
            .map(|index| CategoryStatusPresentation {
                category: if index == 4 {
                    "現在カテゴリ🚀".to_string()
                } else {
                    format!("カテゴリ{index}🚀")
                },
                session_ids: vec![format!("${}", index + 1)],
                active: index == 4,
                counts: BadgeStateCounts::default(),
            })
            .collect::<Vec<_>>();
        let snapshot = StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$6".to_string(),
            },
            summary: BadgeStateCounts::default(),
            sessions,
            windows,
            categories,
            attention: vec![crate::daemon::protocol::v2::AttentionEntry {
                pane_instance: crate::pane_state::PaneInstance {
                    pane_id: "%9".to_string(),
                    pane_pid: 900,
                },
                session_name: "要確認".to_string(),
                badge: BadgeState::Blocked,
                reason: Some("permission_prompt".to_string()),
                elapsed_seconds: 5_400,
            }],
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();
        assert!(tmux_display_width(&rendered.sessions) > 80);
        assert_eq!(top_level_user_ranges(&rendered.sessions).unwrap().len(), 12);
        assert!(!rendered.sessions.contains("+"), "{}", rendered.sessions);
        for option in [&rendered.windows, &rendered.category] {
            assert!(
                tmux_display_width(option) <= 80,
                "{}: {option}",
                tmux_display_width(option)
            );
            assert!(option.contains("+"), "{option}");
        }
        let total = [
            &rendered.attention,
            &rendered.category,
            &rendered.sessions,
            &rendered.windows,
            &rendered.summary,
        ]
        .into_iter()
        .map(|segment| tmux_display_width(segment))
        .sum::<usize>();
        assert!(
            total > 80,
            "complete session projection should be allowed beyond the shared budget: {rendered:?}"
        );
        assert!(
            rendered.sessions.contains("現在🚀"),
            "{}",
            rendered.sessions
        );
        assert!(rendered.sessions.contains("range=user|session:$6"));
        assert!(rendered.windows.contains("range=user|window:@8"));
        assert!(
            rendered.windows.contains("現在の窓🪟") || rendered.windows.contains("@8"),
            "{}",
            rendered.windows
        );
        assert!(
            rendered.category.contains("現在カテゴリ🚀") || rendered.category.contains("cat:5"),
            "{}",
            rendered.category
        );
        assert!(rendered.category.contains("range=user|category-current:"));
        assert!(rendered.attention.contains('▲'), "{}", rendered.attention);
        for label in ["日本語セッション0🚀", "編集ウィンドウ0🪟", "カテゴリ0🚀"]
        {
            assert!(
                !rendered.sessions.contains(label)
                    || rendered.sessions.contains(&format!("{label} ")),
                "semantic tokens must be included whole: {}",
                rendered.sessions
            );
        }
    }

    #[test]
    fn oversized_summary_never_displaces_attention_or_current_identities() {
        let mut config = Config::default();
        config.badge.glyphs.blocked = "B".repeat(70);
        let snapshot = StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            summary: BadgeStateCounts {
                blocked: 1,
                ..BadgeStateCounts::default()
            },
            sessions: vec![SessionStatusPresentation {
                session_id: "$1".to_string(),
                session_name: "main".to_string(),
                category: Some("work".to_string()),
                attached: Some(true),
                created_at: Some(1),
                active: true,
                counts: BadgeStateCounts::default(),
            }],
            windows: vec![WindowStatusPresentation {
                window_id: "@1".to_string(),
                window_name: "editor".to_string(),
                pane_count: 1,
                session_ids: vec!["$1".to_string()],
                window_index: Some(0),
                active: true,
                last: false,
                bell: None,
                activity: None,
                silence: None,
                current_command: None,
                counts: BadgeStateCounts::default(),
            }],
            categories: vec![CategoryStatusPresentation {
                category: "work".to_string(),
                session_ids: vec!["$1".to_string()],
                active: true,
                counts: BadgeStateCounts::default(),
            }],
            attention: vec![crate::daemon::protocol::v2::AttentionEntry {
                pane_instance: crate::pane_state::PaneInstance {
                    pane_id: "%9".to_string(),
                    pane_pid: 900,
                },
                session_name: "review".to_string(),
                badge: BadgeState::Blocked,
                reason: Some("permission_prompt".to_string()),
                elapsed_seconds: 90,
            }],
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(rendered.summary.is_empty(), "{}", rendered.summary);
        assert!(
            rendered.attention.contains("review"),
            "{}",
            rendered.attention
        );
        assert!(rendered.category.contains("category-current:"));
        assert!(rendered.sessions.contains("range=user|session:$1"));
        assert!(rendered.windows.contains("range=user|window:@1"));
        let total = [
            &rendered.attention,
            &rendered.category,
            &rendered.sessions,
            &rendered.windows,
            &rendered.summary,
        ]
        .into_iter()
        .map(|segment| tmux_display_width(segment))
        .sum::<usize>();
        assert!(total <= STATUS_OPTION_CELL_BUDGET, "{total}: {rendered:?}");
    }

    #[test]
    fn oversized_current_tokens_keep_stable_action_targets_within_budget() {
        let mut config = Config::default();
        config.statusline.sessions.current.format = "{session}".to_string();
        config.statusline.windows.current.format = "{window}".to_string();
        config.statusline.category.format = "{category}".to_string();
        let snapshot = StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$42".to_string(),
            },
            summary: BadgeStateCounts::default(),
            sessions: vec![SessionStatusPresentation {
                session_id: "$42".to_string(),
                session_name: "界🚀".repeat(100),
                category: Some("work".to_string()),
                attached: None,
                created_at: None,
                active: true,
                counts: BadgeStateCounts::default(),
            }],
            windows: vec![WindowStatusPresentation {
                window_id: "@77".to_string(),
                window_name: "窓🪟".repeat(100),
                pane_count: 1,
                session_ids: vec!["$42".to_string()],
                window_index: Some(1),
                active: true,
                last: false,
                bell: None,
                activity: None,
                silence: None,
                current_command: None,
                counts: BadgeStateCounts::default(),
            }],
            categories: vec![CategoryStatusPresentation {
                category: "分類🚀".repeat(25),
                session_ids: vec!["$42".to_string()],
                active: true,
                counts: BadgeStateCounts::default(),
            }],
            attention: Vec::new(),
        };
        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(rendered.sessions.contains("range=user|session:$42"));
        assert!(rendered.sessions.contains(&"界🚀".repeat(100)));
        assert!(rendered.windows.contains("range=user|window:@77"));
        assert!(rendered.windows.contains("@77"));
        assert!(rendered.category.contains("range=user|category-current:"));
        assert!(rendered.category.contains("cat:"));
        assert!(tmux_display_width(&rendered.sessions) > 80);
        for option in [&rendered.windows, &rendered.category] {
            assert!(tmux_display_width(option) <= 80, "{option}");
        }
    }

    #[test]
    fn every_session_remains_visible_when_the_session_segment_exceeds_the_budget() {
        let mut config = Config::default();
        config.badge.glyphs.blocked = "S".repeat(50);
        config.statusline.sessions.current.format = "{session}".to_string();
        config.statusline.sessions.other.format = "{session}".to_string();
        config.statusline.windows.current.format = "{window}".to_string();
        config.statusline.category.format = "{category}".to_string();
        let snapshot = StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$42".to_string(),
            },
            summary: BadgeStateCounts {
                blocked: 1,
                ..BadgeStateCounts::default()
            },
            sessions: vec![
                SessionStatusPresentation {
                    session_id: "$42".to_string(),
                    session_name: "界🚀".repeat(100),
                    category: Some("work".to_string()),
                    attached: None,
                    created_at: None,
                    active: true,
                    counts: BadgeStateCounts::default(),
                },
                SessionStatusPresentation {
                    session_id: "$43".to_string(),
                    session_name: "inactive-peer-abcdefghijklmnop".to_string(),
                    category: Some("work".to_string()),
                    attached: None,
                    created_at: None,
                    active: false,
                    counts: BadgeStateCounts::default(),
                },
            ],
            windows: vec![WindowStatusPresentation {
                window_id: "@77".to_string(),
                window_name: "窓🪟".repeat(100),
                pane_count: 1,
                session_ids: vec!["$42".to_string()],
                window_index: Some(1),
                active: true,
                last: false,
                bell: None,
                activity: None,
                silence: None,
                current_command: None,
                counts: BadgeStateCounts::default(),
            }],
            categories: vec![CategoryStatusPresentation {
                category: "分類🚀".repeat(25),
                session_ids: vec!["$42".to_string(), "$43".to_string()],
                active: true,
                counts: BadgeStateCounts::default(),
            }],
            attention: Vec::new(),
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(!rendered.summary.is_empty(), "{rendered:?}");
        assert!(rendered.sessions.contains(&"界🚀".repeat(100)));
        assert!(rendered.sessions.contains("inactive-peer"));
        assert!(!rendered.sessions.contains("+1"), "{}", rendered.sessions);
        assert_eq!(
            top_level_user_ranges(&rendered.sessions).unwrap(),
            vec!["session:$42", "session:$43"]
        );
        let total = [
            &rendered.attention,
            &rendered.category,
            &rendered.sessions,
            &rendered.windows,
            &rendered.summary,
        ]
        .into_iter()
        .map(|segment| tmux_display_width(segment))
        .sum::<usize>();
        assert!(total > STATUS_OPTION_CELL_BUDGET, "{total}: {rendered:?}");
    }

    #[test]
    fn oversized_session_list_does_not_compact_independently_bounded_status_content() {
        let mut config = Config::default();
        config.statusline.sessions.current.format = "{session}".to_string();
        config.statusline.sessions.other.format = "{session}".to_string();
        config.statusline.category.format = "{category}".to_string();
        config.statusline.windows.current.format = "{window}".to_string();
        let snapshot = StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$1".to_string(),
            },
            summary: BadgeStateCounts {
                working: 1,
                ..BadgeStateCounts::default()
            },
            sessions: (1..=8)
                .map(|index| SessionStatusPresentation {
                    session_id: format!("${index}"),
                    session_name: format!("session-{index}-{}", "x".repeat(24)),
                    category: Some("work".to_string()),
                    attached: None,
                    created_at: None,
                    active: index == 1,
                    counts: BadgeStateCounts::default(),
                })
                .collect(),
            windows: vec![WindowStatusPresentation {
                window_id: "@1".to_string(),
                window_name: "editor".to_string(),
                pane_count: 1,
                session_ids: vec!["$1".to_string()],
                window_index: Some(1),
                active: true,
                last: false,
                bell: None,
                activity: None,
                silence: None,
                current_command: None,
                counts: BadgeStateCounts::default(),
            }],
            categories: vec![CategoryStatusPresentation {
                category: "work".to_string(),
                session_ids: vec!["$1".to_string()],
                active: true,
                counts: BadgeStateCounts::default(),
            }],
            attention: vec![crate::daemon::protocol::v2::AttentionEntry {
                pane_instance: crate::pane_state::PaneInstance {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                },
                session_name: "review".to_string(),
                badge: BadgeState::Blocked,
                reason: Some("permission_prompt".to_string()),
                elapsed_seconds: 90,
            }],
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(tmux_display_width(&rendered.sessions) > 80);
        assert!(!rendered.summary.is_empty(), "{rendered:?}");
        assert!(rendered.category.contains("work"), "{rendered:?}");
        assert!(rendered.windows.contains("editor"), "{rendered:?}");
        assert!(rendered.attention.contains("review · perm 1m30s"));
    }

    #[test]
    fn compact_category_visual_ids_are_unique_within_one_snapshot() {
        let config = Config::default();
        let categories = vec![
            CategoryStatusPresentation {
                category: "同じ見た目🚀".repeat(10),
                session_ids: vec!["$1".to_string()],
                active: true,
                counts: BadgeStateCounts::default(),
            },
            CategoryStatusPresentation {
                category: "同じ見た目🚀".repeat(9) + "別",
                session_ids: vec!["$2".to_string()],
                active: true,
                counts: BadgeStateCounts::default(),
            },
        ];

        let tokens = structured_category_tokens(&config, &categories).unwrap();

        assert!(tokens[0].compact.contains("cat:1"));
        assert!(tokens[1].compact.contains("cat:2"));
        assert_ne!(tokens[0].compact, tokens[1].compact);
        assert!(tokens[0].compact.contains("category-current:"));
        assert!(tokens[1].compact.contains("category-current:"));
    }

    #[test]
    fn session_indices_and_targets_cover_the_complete_ordered_model() {
        let mut config = Config::default();
        config.statusline.sessions.show_index = true;
        config.statusline.sessions.current.format = "{index}:{session}".to_string();
        config.statusline.sessions.other.format = "{index}:{session}".to_string();
        config.statusline.sessions.separator = " ".to_string();
        let sessions = (0..10)
            .map(|index| SessionStatusPresentation {
                session_id: format!("${}", index + 1),
                session_name: format!("session-{index}-{}", "x".repeat(20)),
                category: None,
                attached: None,
                created_at: None,
                active: index == 8,
                counts: BadgeStateCounts::default(),
            })
            .collect::<Vec<_>>();

        let rendered = render_structured_sessions(&config, &sessions);
        let targets = top_level_user_ranges(&rendered).unwrap();
        assert_eq!(targets.len(), 10);
        assert_eq!(targets.first().map(String::as_str), Some("session:$1"));
        assert_eq!(targets.last().map(String::as_str), Some("session:$10"));
        for index in 1..=10 {
            assert!(
                rendered.contains(&format!("{index}:{index}: session-{}-", index - 1)),
                "{rendered}"
            );
        }
        assert!(!rendered.contains("+1"), "{rendered}");
        assert!(tmux_display_width(&rendered) > 80, "{rendered}");
    }

    #[test]
    fn attention_budget_never_drops_the_blocked_identity() {
        let mut config = Config::default();
        config.statusline.attention.prefix = "x".repeat(100);
        let entries = vec![crate::daemon::protocol::v2::AttentionEntry {
            pane_instance: crate::pane_state::PaneInstance {
                pane_id: "%9".to_string(),
                pane_pid: 900,
            },
            session_name: "長いセッション🚀".repeat(50),
            badge: BadgeState::Blocked,
            reason: Some("permission_prompt".to_string()),
            elapsed_seconds: 5_400,
        }];

        let rendered = render_structured_attention(&config, &entries);

        assert_eq!(rendered, "▲ blocked");
        assert!(tmux_display_width(&rendered) <= 80);
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
    fn displayed_target_parser_ignores_escaped_and_nested_spoofed_ranges() {
        let rendered = concat!(
            "#[range=user|session:$1] one##[range=user|session:$9]",
            "#[range=user|session:$8]nested#[norange]#[norange]",
            "#[range=user|session:$2] two #[norange]"
        );

        assert_eq!(
            top_level_user_ranges(rendered).unwrap(),
            vec!["session:$1", "session:$2"]
        );
    }

    #[test]
    fn displayed_target_parser_rejects_partial_or_unbalanced_ranges() {
        for (rendered, expected) in [
            ("#[range=user|session:$1", "unterminated tmux directive"),
            ("#[norange]", "unmatched #[norange]"),
            ("#[range=user|session:$1] partial", "unclosed user range"),
        ] {
            let error = top_level_user_ranges(rendered).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "expected {expected:?} in {error:#}"
            );
        }
    }

    #[test]
    fn session_switch_recovers_only_after_partial_published_option_is_replaced() {
        for rendered in [
            "#[range=user|session:$2",
            "#[norange]",
            "#[range=user|session:$2] partial",
        ] {
            let mock = MockTmuxRunner::new();
            mock.stub(
                &[
                    "show-option",
                    "-qv",
                    "-t",
                    "$1",
                    crate::options::KEY_STATUS_SESSIONS,
                ],
                rendered,
            );

            assert!(switch_statusline_session(&mock, "client", "$1", 0).is_err());
            assert!(
                mock.calls()
                    .iter()
                    .all(|call| call.first().map(String::as_str) != Some("switch-client")),
                "partial option must fail closed: {rendered:?}"
            );
        }

        let recovered = MockTmuxRunner::new();
        recovered.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_SESSIONS,
            ],
            "#[range=user|session:$2] stable #[norange]\n",
        );
        recovered.stub(&["switch-client", "-c", "client", "-t", "$2"], "");

        switch_statusline_session(&recovered, "client", "$1", 0).unwrap();

        assert!(recovered.calls().iter().any(|call| {
            call == &vec![
                "switch-client".to_string(),
                "-c".to_string(),
                "client".to_string(),
                "-t".to_string(),
                "$2".to_string(),
            ]
        }));
    }

    #[test]
    fn session_switch_uses_target_from_current_session_option() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_SESSIONS,
            ],
            "#[range=user|session:$2] zeta #[norange]#[range=user|session:$1] alpha #[norange]\n",
        );
        mock.stub(&["switch-client", "-c", "client", "-t", "$1"], "");

        switch_statusline_session(&mock, "client", "$1", 1).unwrap();

        assert!(mock.calls().iter().any(|call| {
            call == &vec![
                "switch-client".to_string(),
                "-c".to_string(),
                "client".to_string(),
                "-t".to_string(),
                "$1".to_string(),
            ]
        }));
    }

    #[test]
    fn session_cycle_uses_every_ordered_stable_target_in_the_published_model() {
        let rendered = (1..=6)
            .map(|index| format!("#[range=user|session:${index}] session-{index} #[norange]"))
            .collect::<String>();
        for (direction, expected) in [(Direction::Next, "$4"), (Direction::Previous, "$2")] {
            let mock = MockTmuxRunner::new();
            mock.stub(
                &[
                    "show-option",
                    "-qv",
                    "-t",
                    "$3",
                    crate::options::KEY_STATUS_SESSIONS,
                ],
                &rendered,
            );
            mock.stub(&["switch-client", "-c", "client", "-t", expected], "");

            cycle_statusline_session(&mock, "client", "$3", direction).unwrap();

            assert!(mock.calls().iter().any(|call| {
                call == &vec![
                    "switch-client".to_string(),
                    "-c".to_string(),
                    "client".to_string(),
                    "-t".to_string(),
                    expected.to_string(),
                ]
            }));
        }
    }

    #[test]
    fn session_cycle_wraps_and_rejects_duplicate_or_missing_current_targets() {
        for (current, direction, expected) in [
            ("$1", Direction::Previous, "$3"),
            ("$3", Direction::Next, "$1"),
        ] {
            let mock = MockTmuxRunner::new();
            mock.stub(
                &[
                    "show-option",
                    "-qv",
                    "-t",
                    current,
                    crate::options::KEY_STATUS_SESSIONS,
                ],
                "#[range=user|session:$1] one #[norange]#[range=user|session:$2] two #[norange]#[range=user|session:$3] three #[norange]",
            );
            mock.stub(&["switch-client", "-c", "client", "-t", expected], "");

            cycle_statusline_session(&mock, "client", current, direction).unwrap();
        }

        for rendered in [
            "#[range=user|session:$1] one #[norange]#[range=user|session:$1] duplicate #[norange]",
            "#[range=user|session:$1] one #[norange]#[range=user|session:$2] two #[norange]",
        ] {
            let mock = MockTmuxRunner::new();
            mock.stub(
                &[
                    "show-option",
                    "-qv",
                    "-t",
                    "$3",
                    crate::options::KEY_STATUS_SESSIONS,
                ],
                rendered,
            );

            assert!(cycle_statusline_session(&mock, "client", "$3", Direction::Next).is_err());
            assert!(
                mock.calls()
                    .iter()
                    .all(|call| call.first().map(String::as_str) != Some("switch-client"))
            );
        }
    }

    #[test]
    fn stale_session_index_returns_error_without_switching() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_SESSIONS,
            ],
            "#[range=user|session:$1] alpha #[norange]\n",
        );

        let error = switch_statusline_session(&mock, "client", "$1", 1).unwrap_err();

        assert!(error.to_string().contains("no longer available"));
        assert!(
            mock.calls()
                .iter()
                .all(|call| call.first().map(String::as_str) != Some("switch-client"))
        );
    }

    #[test]
    fn category_cycle_origin_comes_only_from_the_published_display_model() {
        let mock = MockTmuxRunner::new();
        let work = encode_category_key("work").unwrap();
        let personal = encode_category_key("personal").unwrap();
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_CATEGORY,
            ],
            &format!(
                "#[range=user|category:{personal}] personal #[norange]#[range=user|category-current:{work}] work #[norange]\n"
            ),
        );

        let (targets, current) = displayed_category_targets(&mock, "$1").unwrap();

        assert_eq!(targets, vec![personal, work.clone()]);
        assert_eq!(current, work);
        assert!(
            mock.calls()
                .iter()
                .all(|call| call.first().map(String::as_str) != Some("list-sessions"))
        );
    }

    #[test]
    fn category_cycle_rejects_a_display_without_one_active_target() {
        let mock = MockTmuxRunner::new();
        let work = encode_category_key("work").unwrap();
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_CATEGORY,
            ],
            &format!("#[range=user|category:{work}] work #[norange]\n"),
        );

        let error = displayed_category_targets(&mock, "$1").unwrap_err();

        assert!(error.to_string().contains("no active category"));

        let mock = MockTmuxRunner::new();
        let personal = encode_category_key("personal").unwrap();
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_CATEGORY,
            ],
            &format!(
                "#[range=user|category-current:{work}] one #[norange]#[range=user|category-current:{personal}] two #[norange]\n"
            ),
        );
        let error = displayed_category_targets(&mock, "$1").unwrap_err();
        assert!(error.to_string().contains("multiple active categories"));
    }

    #[test]
    fn category_cycle_uses_all_effective_categories_in_current_mode() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        let sessions = "one\u{1f}1\u{1f}100\u{1f}a\u{1f}\u{1f}\u{1f}$1\ntwo\u{1f}0\u{1f}101\u{1f}b\u{1f}\u{1f}\u{1f}$2\nthree\u{1f}0\u{1f}102\u{1f}c\u{1f}\u{1f}\u{1f}$3\n";
        mock.stub(&["list-sessions", "-F", &format], sessions);
        let memory_key = crate::session::client_memory_key("client", "b");
        mock.stub(&["show-option", "-gqv", &memory_key], "");
        mock.stub(&["switch-client", "-c", "client", "-t", "=two:"], "");
        mock.stub(&["set-option", "-g", &memory_key, "two"], "");
        let mut config = Config::default();
        config.statusline.category.mode = "current".to_string();

        cycle_statusline_category(&mock, &config, "client", "$1", Direction::Next).unwrap();

        assert!(mock.calls().iter().all(|call| {
            call.first().map(String::as_str) != Some("show-option")
                || call.get(2).map(String::as_str) != Some("-t")
        }));
        assert!(
            mock.calls()
                .iter()
                .any(|call| { call == &["switch-client", "-c", "client", "-t", "=two:"] })
        );
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|call| call.first().map(String::as_str) == Some("list-sessions"))
                .count(),
            1,
            "one category cycle must use one authoritative session snapshot"
        );
        assert_eq!(
            mock.calls().last().unwrap(),
            &["switch-client", "-c", "client", "-t", "=two:"]
        );
    }

    #[test]
    fn consecutive_category_cycles_preserve_next_and_previous_order() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "one\u{1f}1\u{1f}100\u{1f}a\u{1f}\u{1f}\u{1f}$1\ntwo\u{1f}0\u{1f}101\u{1f}b\u{1f}\u{1f}\u{1f}$2\nthree\u{1f}0\u{1f}102\u{1f}c\u{1f}\u{1f}\u{1f}$3\n",
        );
        for category in ["a", "b", "c"] {
            let key = crate::session::client_memory_key("client", category);
            mock.stub(&["show-option", "-gqv", &key], "");
        }
        mock.stub(&["switch-client", "-c", "client", "-t", "=one:"], "");
        mock.stub(&["switch-client", "-c", "client", "-t", "=two:"], "");
        mock.stub(&["switch-client", "-c", "client", "-t", "=three:"], "");

        let config = Config::default();
        cycle_statusline_category(&mock, &config, "client", "$1", Direction::Next).unwrap();
        cycle_statusline_category(&mock, &config, "client", "$2", Direction::Next).unwrap();
        cycle_statusline_category(&mock, &config, "client", "$3", Direction::Previous).unwrap();

        let switches = mock
            .calls()
            .into_iter()
            .filter(|call| call.first().map(String::as_str) == Some("switch-client"))
            .collect::<Vec<_>>();
        assert_eq!(
            switches,
            vec![
                vec!["switch-client", "-c", "client", "-t", "=two:"],
                vec!["switch-client", "-c", "client", "-t", "=three:"],
                vec!["switch-client", "-c", "client", "-t", "=two:"],
            ]
        );
    }

    #[test]
    fn category_cycle_errors_when_only_one_effective_category_exists() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "one\u{1f}1\u{1f}100\u{1f}a\u{1f}\u{1f}\u{1f}$1\n",
        );

        let error =
            cycle_statusline_category(&mock, &Config::default(), "client", "$1", Direction::Next)
                .unwrap_err();

        assert!(error.to_string().contains("at least two categories"));
        assert!(
            mock.calls()
                .iter()
                .all(|call| call.first().map(String::as_str) != Some("switch-client"))
        );
    }

    #[test]
    fn category_target_round_trips_special_utf8_and_uncategorized() {
        for category in ["", "work space:|#日本語"] {
            let encoded = encode_category_key(category).unwrap();
            assert_eq!(decode_category_key(&encoded).unwrap(), category);
            assert!(
                encoded
                    .bytes()
                    .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') })
            );
        }
        assert!(encode_category_key(&"x".repeat(257)).is_err());
        let oversized = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("x".repeat(257));
        assert!(decode_category_key(&oversized).is_err());
        assert!(decode_category_key("QR").is_err());
    }

    #[test]
    fn structured_snapshot_rejects_unaddressable_category_key() {
        let snapshot = StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Global,
            summary: BadgeStateCounts::default(),
            sessions: Vec::new(),
            windows: Vec::new(),
            categories: vec![CategoryStatusPresentation {
                category: "x".repeat(257),
                session_ids: vec!["$1".to_string()],
                active: true,
                counts: BadgeStateCounts::default(),
            }],
            attention: Vec::new(),
        };

        let error = render_structured_status_snapshot(&Config::default(), &snapshot).unwrap_err();

        assert!(error.to_string().contains("exceeds 256"));
    }

    #[test]
    fn disabled_session_badge_is_empty_for_every_style_and_mode() {
        for style in [
            BadgeStyle::Inline,
            BadgeStyle::Plain,
            BadgeStyle::Outer,
            BadgeStyle::Chip,
        ] {
            for mode in [SessionBadgeMode::Rollup, SessionBadgeMode::Counts] {
                let mut config = Config::default();
                config.statusline.session_badge.enabled = false;
                config.statusline.session_badge.mode = mode;
                config.statusline.sessions.badge_style = style;
                config.statusline.sessions.current.format = "{badge}{session}".to_string();

                let rendered =
                    render_structured_sessions(&config, &[status_session("$1", "main", true)]);

                for glyph in ["▲", "●", "✓", "○"] {
                    assert!(!rendered.contains(glyph), "{style:?}/{mode:?}: {rendered}");
                }
                assert!(rendered.contains("main"), "{style:?}/{mode:?}: {rendered}");
            }
        }
    }

    #[test]
    fn session_badge_plain_outer_inline_and_chip_markup_is_exact() {
        let style = SegmentStyle {
            format: "{badge}{session}".to_string(),
            ..SegmentStyle::default()
        };
        let config = Config::default();
        let render = |badge_style| {
            render_structured_session_segment(
                &style,
                "▲",
                "blocked",
                "main",
                0,
                &SessionBadgeRenderOptions {
                    badge_style,
                    separate_badge: false,
                    badge_config: &config.badge,
                    chip_config: &config.statusline.session_badge.chip,
                },
            )
        };

        assert_eq!(render(BadgeStyle::Plain), "▲ main");
        assert_eq!(
            render(BadgeStyle::Inline),
            "#[fg=#ff6b6b]▲#[fg=default] main"
        );
        assert_eq!(render(BadgeStyle::Outer), "#[fg=#ff6b6b]▲#[default] main");
        assert_eq!(
            render(BadgeStyle::Chip),
            "#[fg=#303047]\u{e0b6}#[bg=#303047] #[fg=#ff6b6b]▲#[fg=default] #[fg=#303047,bg=default]\u{e0b4}#[default] main"
        );
    }
}

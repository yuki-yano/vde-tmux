use anyhow::{Result, anyhow};

use crate::config::{
    AgentBadgeConfig, BadgeConfig, BadgeGlyphs, BadgeStyle, Config, FixedWidthAlignment,
    SegmentColors, SegmentStyle, SessionBadgeChipConfig, SessionBadgeMode,
    StatuslineCategoryConfig,
};
use crate::daemon::protocol::v2::{
    CategoryStatusPresentation, PanePresentation, SessionStatusPresentation, StatusContext,
    StatusSnapshot, WindowStatusPresentation,
};
use crate::daemon::session_badge::{
    BadgeState, BadgeStateCounts, agent_badge_value_from_counts, badge_value_from_counts,
    glyph_for_state,
};

use super::targets::{
    CATEGORY_RANGE_PREFIX, CURRENT_CATEGORY_RANGE_PREFIX, TMUX_USER_RANGE_NAME_MAX_BYTES,
    attention_target_key, category_target_key, primary_attention_entry,
};

pub(crate) const STATUS_OPTION_CELL_BUDGET: usize = 80;
pub(super) const STATUS_NOW_FORMAT_OPTION: &str = "@vde_status_now_format";

#[derive(Debug)]
pub(super) struct StatusToken {
    pub(super) rendered: String,
    pub(super) compact: String,
    pub(super) current: bool,
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

pub fn render_structured_pane_status(config: &Config, pane: &PanePresentation) -> String {
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
            let awaiting_result = badge_state == BadgeState::Working
                && resolved.canonical.claude_background.is_awaiting_result();
            let badge = structured_pane_badge(config, badge_state, &text_fg, awaiting_result);
            let status_label = structured_pane_status_label(&resolved.canonical, badge_state);
            let status =
                structured_pane_status_fragment(config, status_label, badge_state, &text_fg);
            let time_label = structured_pane_time_label(&resolved.canonical, badge_state);
            let time =
                structured_pane_time_fragment(config, time_label.as_deref(), badge_state, &text_fg);
            let detail = structured_pane_detail(
                config,
                &agent,
                status_label,
                time_label.as_deref(),
                badge_state,
                &text_fg,
                awaiting_result,
            );
            (agent, badge, status, time, detail)
        }
        None => {
            let (badge, status) = ("—".to_string(), "No state".to_string());
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
    let mut rendered = tmux_style_segment(style, &body);
    if let Some(color) = pane_border_highlight_color(config, pane) {
        rendered.push_str(&format!(
            "#[fg={color}]{}#[default]",
            "─".repeat(usize::from(pane.pane_width))
        ));
    }
    escape_tmux_secondary_expansion_percent(&rendered)
}

pub(super) fn pane_border_highlight_color(
    config: &Config,
    pane: &PanePresentation,
) -> Option<String> {
    let badge = pane.resolved.as_ref()?.badge;
    let color = match badge {
        BadgeState::Blocked => &config.badge.colors.blocked,
        BadgeState::Limited => &config.badge.colors.limited,
        BadgeState::Working => &config.badge.colors.working,
        BadgeState::Done => &config.badge.colors.done,
        BadgeState::Unknown | BadgeState::Idle => return None,
    };
    Some(normalize_tmux_color(color))
}

pub(super) fn render_structured_summary(config: &Config, counts: BadgeStateCounts) -> String {
    if !config.statusline.summary.enabled {
        return String::new();
    }
    let state_counts = [
        (BadgeState::Blocked, counts.blocked),
        (BadgeState::Limited, counts.limited),
        (BadgeState::Working, counts.working),
        (BadgeState::Done, counts.done),
        (BadgeState::Unknown, counts.unknown),
        (BadgeState::Idle, counts.idle),
    ];
    let visible_counts = if config.statusline.summary.hide_idle {
        &state_counts[..5]
    } else {
        &state_counts[..]
    };
    let visible_counts = visible_counts
        .iter()
        .copied()
        .filter(|(badge, count)| *badge != BadgeState::Unknown || *count > 0)
        .collect::<Vec<_>>();
    crate::daemon::render_summary(
        &visible_counts,
        &config.badge,
        &config.statusline.summary.format,
    )
}

pub(super) fn render_structured_sessions(
    config: &Config,
    sessions: &[SessionStatusPresentation],
) -> String {
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

pub(crate) fn sessions_display_width(
    config: &Config,
    sessions: &[SessionStatusPresentation],
) -> usize {
    tmux_display_width(&render_structured_sessions(config, sessions))
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

pub(super) fn render_structured_session_segment(
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

pub(super) fn structured_category_tokens(
    config: &Config,
    categories: &[CategoryStatusPresentation],
) -> Result<Vec<StatusToken>> {
    let mut categories = categories.iter().collect::<Vec<_>>();
    if config.statusline.category.mode == "current" {
        categories.retain(|category| category.active);
    }
    let mut seen_targets = std::collections::BTreeSet::new();
    categories
        .into_iter()
        .map(|category| -> Result<StatusToken> {
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
            let target = category_target_key(&category.category)?;
            if !seen_targets.insert(target.clone()) {
                return Err(anyhow!(
                    "category target collision; rename one of the colliding categories"
                ));
            }
            let range = if active {
                format!("{CURRENT_CATEGORY_RANGE_PREFIX}{target}")
            } else {
                format!("{CATEGORY_RANGE_PREFIX}{target}")
            };
            debug_assert!(range.len() <= TMUX_USER_RANGE_NAME_MAX_BYTES);
            let rendered = format!("#[range=user|{range}]{segment}#[norange]");
            Ok(StatusToken {
                compact: rendered.clone(),
                rendered,
                current: active,
            })
        })
        .collect::<Result<Vec<_>>>()
}

fn structured_window_tokens(
    config: &Config,
    windows: &[&WindowStatusPresentation],
) -> Vec<StatusToken> {
    windows
        .iter()
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
            let raw_index = window
                .window_index
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".to_string());
            let index = if window.active {
                format!("#[reverse]{raw_index}#[noreverse]")
            } else {
                raw_index
            };
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
            let mut body = render_structured_template(
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
            if badge_fragment.is_empty() && style.format == " {index} {badge} " {
                body = format!(" {index} ");
            }
            let notification = window_notification(config, window, &style);
            body.push_str(&notification);
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
            // Width compaction preserves the public index and agent counts. Internal @IDs
            // identify click targets only; they are never the visible current-window label.
            let mut compact_style = style.clone();
            compact_style.prefix.clear();
            compact_style.suffix.clear();
            let compact_badge = badge
                .as_ref()
                .map(|(value, state)| {
                    agent_badge_fragment(
                        config,
                        &config.statusline.windows.agent_badge,
                        BadgeStyle::Inline,
                        value,
                        state,
                        style.colors.fg.as_deref(),
                        style.colors.bg.as_deref(),
                    )
                })
                .unwrap_or_default();
            let compact_body = if compact_badge.is_empty() {
                format!(" {index}{notification} ")
            } else {
                format!(" {index} {compact_badge}{notification} ")
            };
            let compact = tmux_style_segment(&compact_style, &compact_body);
            StatusToken {
                compact: format!(
                    "#[range=user|window:{}]{compact}#[norange]",
                    window.window_id
                ),
                rendered,
                current: window.active,
            }
        })
        .collect::<Vec<_>>()
}

fn window_notification(
    config: &Config,
    window: &WindowStatusPresentation,
    style: &SegmentStyle,
) -> String {
    let (glyph, colors) = if window.bell.unwrap_or(false) {
        ("♪", &config.statusline.windows.bell)
    } else if window.activity.unwrap_or(false) || window.silence.unwrap_or(false) {
        ("·", &config.statusline.windows.activity)
    } else {
        return String::new();
    };
    let color = colors.fg.as_deref().unwrap_or("#f9e2af");
    let restore = style.colors.fg.as_deref().unwrap_or("default");
    format!(" #[fg={color}]{glyph}#[fg={restore}]")
}

fn render_window_map(config: &Config, windows: &[WindowStatusPresentation]) -> String {
    if windows.is_empty() {
        return String::new();
    }
    let mut ordered = windows.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.window_index
            .unwrap_or(i64::MAX)
            .cmp(&right.window_index.unwrap_or(i64::MAX))
            .then_with(|| left.window_id.cmp(&right.window_id))
    });
    let tokens = structured_window_tokens(config, &ordered);
    let active = tokens.iter().position(|token| token.current);
    let current_name = active.map(|index| {
        let name = &ordered[index].window_name;
        let name = if name.is_empty() { "(unnamed)" } else { name };
        name.chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>()
    });
    let cell_widths = [false, true].map(|compact| {
        tokens
            .iter()
            .map(|token| {
                tmux_display_width(if compact {
                    &token.compact
                } else {
                    &token.rendered
                })
            })
            .max()
            .unwrap_or(0)
    });
    let render = |start: usize, end: usize, compact: bool, name_width: usize| {
        let selected = &tokens[start..end];
        let cells = selected
            .iter()
            .map(|token| {
                if compact {
                    &token.compact
                } else {
                    &token.rendered
                }
            })
            .collect::<Vec<_>>();
        // Equal-width cells keep every index in place when only the active Window changes.
        let cell_width = cell_widths[usize::from(compact)];
        let cells = cells
            .iter()
            .map(|cell| {
                let padding = " ".repeat(cell_width.saturating_sub(tmux_display_width(cell)));
                format!("{cell}{padding}")
            })
            .collect::<Vec<_>>()
            .join(&config.statusline.windows.separator);
        let left = window_map_omission(config, &ordered[..start], true);
        let right = window_map_omission(config, &ordered[end..], false);
        let name = current_name
            .as_ref()
            .filter(|_| name_width > 0)
            .map(|name| {
                format!(
                    " │ {}",
                    structured_external_text(&crate::sidebar::render::truncate_display(
                        name, name_width
                    ))
                )
            })
            .unwrap_or_default();
        format!("W {} {left}{cells}{right}{name}", ordered.len())
    };
    let full = render(0, tokens.len(), false, 16);
    if tmux_display_width(&full) <= STATUS_OPTION_CELL_BUDGET {
        return full;
    }
    let center = active.unwrap_or(0);
    let mut start = center;
    let mut end = center + 1;
    let name_width =
        if tmux_display_width(&render(start, end, true, 16)) <= STATUS_OPTION_CELL_BUDGET {
            16
        } else {
            STATUS_OPTION_CELL_BUDGET
                .saturating_sub(tmux_display_width(&render(start, end, true, 0)))
                .saturating_sub(3)
                .min(16)
        };
    // Grow a contiguous neighborhood, alternating left and right; hidden windows retain
    // their actual side and aggregate state rather than becoming a misleading trailing +N.
    loop {
        let mut grew = false;
        if start > 0
            && tmux_display_width(&render(start - 1, end, true, name_width))
                <= STATUS_OPTION_CELL_BUDGET
        {
            start -= 1;
            grew = true;
        }
        if end < tokens.len()
            && tmux_display_width(&render(start, end + 1, true, name_width))
                <= STATUS_OPTION_CELL_BUDGET
        {
            end += 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    render(start, end, true, name_width)
}

fn window_map_omission(
    config: &Config,
    windows: &[&WindowStatusPresentation],
    left: bool,
) -> String {
    if windows.is_empty() {
        return String::new();
    }
    let mut counts = BadgeStateCounts::default();
    for window in windows {
        counts.merge(window.counts);
    }
    let states = [
        (BadgeState::Blocked, counts.blocked),
        (BadgeState::Limited, counts.limited),
        (BadgeState::Done, counts.done),
    ];
    let badges = states
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(state, count)| {
            format!(
                "{} {count}",
                structured_pane_badge(config, state, "default", false)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let badges = if badges.is_empty() {
        String::new()
    } else {
        format!(" {badges}")
    };
    if left {
        format!("{}…{badges} ", windows.len())
    } else {
        format!(" …{}{badges}", windows.len())
    }
}

fn render_bounded_status_snapshot(
    config: &Config,
    snapshot: &StatusSnapshot,
) -> Result<StructuredStatusSegments> {
    let category_tokens = structured_category_tokens(config, &snapshot.categories)?;
    let session_tokens = snapshot
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| render_session_token(config, session, index))
        .collect::<Vec<_>>();
    let category_included = vec![true; category_tokens.len()];
    // Keep Session navigation and global Summary independent from Window compaction.
    let session_included = vec![true; session_tokens.len()];
    let windows = render_window_map(config, &snapshot.windows);
    let summary = render_structured_summary(config, snapshot.summary);
    let (attention_full, attention_compact) =
        structured_attention_variants(config, &snapshot.attention);
    let category_width = category_tokens
        .iter()
        .map(|token| tmux_display_width(&token.rendered))
        .sum::<usize>();
    let attention = if tmux_display_width(&summary)
        + category_width
        + tmux_display_width(&windows)
        + tmux_display_width(&attention_full)
        > STATUS_OPTION_CELL_BUDGET
    {
        attention_compact
    } else {
        attention_full
    };

    let category = render_selected_status_tokens(&category_tokens, &category_included, "");
    let mut sessions = render_selected_sessions(
        config,
        &snapshot.sessions,
        &session_tokens,
        &session_included,
    );
    if config.statusline.sessions.fixed_width
        && matches!(snapshot.context, StatusContext::Session { .. })
        && let Some(session_zone_width) = snapshot.session_zone_width
    {
        sessions = pad_session_zone(
            sessions,
            session_zone_width,
            config.statusline.sessions.fixed_width_alignment,
        );
    }
    Ok(StructuredStatusSegments {
        snapshot_revision: snapshot.snapshot_revision,
        summary,
        category,
        sessions,
        windows,
        attention,
    })
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

pub(super) fn pad_session_zone(
    rendered: String,
    target_width: usize,
    alignment: FixedWidthAlignment,
) -> String {
    let padding = target_width.saturating_sub(tmux_display_width(&rendered));
    if padding == 0 {
        return rendered;
    }
    match alignment {
        FixedWidthAlignment::Left => format!("{rendered}#[default]{}", " ".repeat(padding)),
        FixedWidthAlignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!(
                "#[default]{}{}#[default]{}",
                " ".repeat(left),
                rendered,
                " ".repeat(right)
            )
        }
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

fn structured_attention_variants(
    config: &Config,
    entries: &[crate::daemon::protocol::v2::AttentionEntry],
) -> (String, String) {
    let Some(entry) = primary_attention_entry(entries) else {
        return (String::new(), String::new());
    };
    let reason = match entry.reason.as_deref() {
        Some(reason) if reason.to_ascii_lowercase().contains("permission") => None,
        Some(reason) if reason.starts_with("Other(") => Some("wait"),
        Some(_) => Some("err"),
        None => Some("err"),
    };
    let more = entries.len().saturating_sub(1);
    let suffix = if more > 0 {
        format!(" +{more}")
    } else {
        String::new()
    };
    let session_name = structured_external_text(&entry.session_name);
    let inner = match reason {
        Some(reason) => format!("▲ {session_name} · {reason}{suffix}"),
        None => format!("▲ {session_name}{suffix}"),
    };
    let target = attention_target_key(&entry.pane_instance);
    (
        format!(
            "#[range=user|{target}]{}#[norange]",
            render_attention_segment(&config.statusline.attention, &inner)
        ),
        format!("#[range=user|{target}]▲ blocked{suffix}#[norange]"),
    )
}

fn structured_pane_badge(
    config: &Config,
    state: BadgeState,
    text_fg: &str,
    awaiting_result: bool,
) -> String {
    let glyph = if awaiting_result {
        &config.badge.glyphs.awaiting_result
    } else {
        glyph_for_state(state, &config.badge.glyphs)
    };
    let color = config
        .badge
        .colors
        .for_state(state.as_str())
        .unwrap_or("default");
    format!("#[fg={color}]{glyph}#[fg={text_fg}]")
}

pub(super) fn structured_pane_status_label(
    state: &crate::pane_state::PaneState,
    badge: BadgeState,
) -> &'static str {
    if badge == BadgeState::Done {
        return "Done";
    }
    if badge == BadgeState::Working && state.claude_background.is_awaiting_result() {
        return "結果待ち";
    }
    match state.lifecycle {
        crate::pane_state::LifecycleState::Idle => "Idle",
        crate::pane_state::LifecycleState::Running => "Running",
        crate::pane_state::LifecycleState::Waiting { ref reason } if reason.is_usage_limit() => {
            "Limited"
        }
        crate::pane_state::LifecycleState::Waiting { .. } => "Waiting",
        crate::pane_state::LifecycleState::Error { .. } => "Error",
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
) -> Option<String> {
    let (epoch, suffix) = match badge {
        BadgeState::Unknown => return None,
        BadgeState::Done | BadgeState::Idle => (state.completed_at?, " ago"),
        BadgeState::Blocked | BadgeState::Limited | BadgeState::Working => (state.started_at?, ""),
    };
    Some(format!("{}{suffix}", tmux_bounded_duration(epoch)))
}

/// Builds the former compact elapsed-time presentation as a tmux format.
///
/// The pane status option is written only when semantic state changes. tmux evaluates this
/// expression while drawing the visible pane border, so elapsed seconds advance without a
/// periodic `set-option -p` against every pane in the server.
pub(super) fn tmux_bounded_duration(epoch: i64) -> String {
    let epoch = epoch.max(0);
    let now = format!("#{{T:{STATUS_NOW_FORMAT_OPTION}}}");
    let elapsed = format!("#{{?#{{e|<:{now},{epoch}}},0,#{{e|-:{now},{epoch}}}}}");
    let minutes = format!("#{{e|/:{elapsed},60}}");
    let seconds = format!("#{{e|m:{elapsed},60}}");
    let padded_seconds = format!("#{{?#{{e|<:{seconds},10}},0,}}{seconds}");
    let hours = format!("#{{e|/:{elapsed},3600}}");
    let remaining_minutes = format!("#{{e|m:{minutes},60}}");
    let hour_label = format!("{hours}h#{{?#{{e|==:{remaining_minutes},0}},,{remaining_minutes}m}}");
    let days = format!("#{{e|/:{elapsed},86400}}");

    format!(
        "#{{?#{{e|<:{elapsed},60}},{elapsed}s,#{{?#{{e|<:{elapsed},600}},{minutes}m{padded_seconds}s,#{{?#{{e|<:{elapsed},3600}},{minutes}m,#{{?#{{e|<:{elapsed},86400}},{hour_label},{days}d}}}}}}}}"
    )
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

pub(super) fn tmux_display_width(rendered: &str) -> usize {
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
    awaiting_result: bool,
) -> String {
    let glyph = if awaiting_result {
        &config.badge.glyphs.awaiting_result
    } else {
        glyph_for_state(state, &config.badge.glyphs)
    };
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

/// Escapes literal percent signs for `#{E:@vde_status_pane}`.
///
/// tmux treats a single `%` as strftime syntax during the secondary expansion. Pane IDs are
/// native tmux values such as `%7`, so the option must contain `%%7` to display `%7` after `E:`.
fn escape_tmux_secondary_expansion_percent(raw: &str) -> String {
    raw.replace('%', "%%")
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

pub(super) struct SessionBadgeRenderOptions<'a> {
    pub(super) badge_style: BadgeStyle,
    pub(super) separate_badge: bool,
    pub(super) badge_config: &'a BadgeConfig,
    pub(super) chip_config: &'a SessionBadgeChipConfig,
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
    let chip_bg = style
        .colors
        .bg
        .as_deref()
        .unwrap_or(chip_config.bg.as_str());
    let chip_start = format!(
        "#[fg={chip_bg}]{}#[bg={chip_bg}] {chip_body} ",
        chip_config.cap_left
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
    let colors = category_colors(&config.statusline.category, active);
    let segment_bg = colors.bg.as_deref().unwrap_or(chip_config.bg.as_str());
    let chip_start = format!(
        "#[fg={segment_bg}]{}#[bg={segment_bg}] {chip_body} ",
        chip_config.cap_left
    );
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
            "#[fg={segment_bg},bg=default]{}#[default] ",
            chip_config.cap_right
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
                BadgeState::Limited => &badge_config.colors.limited,
                BadgeState::Working => &badge_config.colors.working,
                BadgeState::Done => &badge_config.colors.done,
                BadgeState::Unknown | BadgeState::Idle => &badge_config.colors.idle,
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
        BadgeState::Limited,
        BadgeState::Working,
        BadgeState::Done,
        BadgeState::Unknown,
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

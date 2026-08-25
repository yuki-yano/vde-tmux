use std::collections::BTreeSet;

use super::*;
use crate::sidebar::state::{PresentationMode, SidebarState};
use crate::sidebar::tree::{SidebarRow, SidebarRowKind};
use ratatui::style::{Color, Modifier};
use ratatui::text::Span;

fn row(
    id: &str,
    kind: SidebarRowKind,
    depth: usize,
    label: &str,
    rollup: RollupLevel,
) -> SidebarRow {
    SidebarRow {
        id: id.to_string(),
        kind,
        depth,
        label: label.to_string(),
        chat_count: 1,
        rollup,
        badge_state: None,
        expanded: true,
        pane_id: None,
        git: None,
        active: false,
        meta: None,
    }
}

fn chat_row(id: &str, label: &str, rollup: RollupLevel, badge: BadgeState) -> SidebarRow {
    let mut row = row(id, SidebarRowKind::Chat, 0, label, rollup);
    row.badge_state = Some(badge);
    row
}

fn repo_row(label: &str, rollup: RollupLevel) -> SidebarRow {
    row(
        &format!("repo::misc::{label}"),
        SidebarRowKind::Repo,
        0,
        label,
        rollup,
    )
}

fn category_row(label: &str, rollup: RollupLevel) -> SidebarRow {
    row(
        &format!("category::{label}"),
        SidebarRowKind::Category,
        0,
        label,
        rollup,
    )
}

fn detail_row(id: &str, label: &str, rollup: RollupLevel) -> SidebarRow {
    row(id, SidebarRowKind::Detail, 1, label, rollup)
}

fn assert_span_fg(spans: &[Span<'_>], content: &str, color: Color) {
    assert!(
        spans
            .iter()
            .any(|span| span.content.as_ref() == content && span.style.fg == Some(color)),
        "span {content:?} with fg {color:?} not found in {spans:?}"
    );
}

#[test]
fn zone_row_renders_as_colored_heading() {
    let mut zone = row(
        "zone::triage",
        SidebarRowKind::Zone,
        0,
        "TRIAGE",
        RollupLevel::Permission,
    );
    zone.chat_count = 2;

    let lines = render_lines(
        &[zone],
        &SidebarState::default(),
        30,
        &SidebarRenderTheme::default(),
    );
    let text = line_to_string(lines[0].clone());

    assert!(text.starts_with(" ▍TRIAGE 2"), "{text:?}");
    assert!(
        lines[0].spans.iter().any(|span| {
            span.style.fg == Some(Color::Red) && span.style.add_modifier.contains(Modifier::BOLD)
        }),
        "{lines:?}"
    );
}

#[test]
fn width_tier_boundaries() {
    assert_eq!(WidthTier::from_width(2), WidthTier::Rail);
    assert_eq!(WidthTier::from_width(3), WidthTier::Rail);
    assert_eq!(WidthTier::from_width(4), WidthTier::Micro);
    assert_eq!(WidthTier::from_width(23), WidthTier::Micro);
    assert_eq!(WidthTier::from_width(24), WidthTier::Dense);
    assert_eq!(WidthTier::from_width(35), WidthTier::Dense);
    assert_eq!(WidthTier::from_width(36), WidthTier::Standard);
}

#[test]
fn agent_identity_palette_is_fixed_and_distinct_from_status_palette() {
    let theme = SidebarRenderTheme::default();

    assert_eq!(agent_identity_color("codex", &theme), CODEX_AGENT_COLOR);
    assert_eq!(agent_identity_color("Codex", &theme), CODEX_AGENT_COLOR);
    assert_eq!(agent_identity_color("claude", &theme), CLAUDE_AGENT_COLOR);
    assert_eq!(agent_identity_color("Claude", &theme), CLAUDE_AGENT_COLOR);
    assert_eq!(agent_identity_color("opencode", &theme), theme.branch);
    assert_ne!(CODEX_AGENT_COLOR, CLAUDE_AGENT_COLOR);
    for status in [
        BadgeState::Blocked,
        BadgeState::Working,
        BadgeState::Done,
        BadgeState::Idle,
    ] {
        assert_ne!(CODEX_AGENT_COLOR, theme.badge_color(status));
        assert_ne!(CLAUDE_AGENT_COLOR, theme.badge_color(status));
    }
}

#[test]
fn dense_tier_renders_one_line_per_chat_with_origin_abbrev() {
    let mut chat = chat_row(
        "chat::%1",
        "claude: fix the bug",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("claude".to_string()),
        elapsed_secs: Some(780),
        origin: Some("misc/vde-tmux".to_string()),
        ..Default::default()
    });

    let rendered = render_rows(&[chat], &SidebarState::default(), 30);

    assert!(rendered.contains("● Claude  vde"), "{rendered:?}");
    assert!(rendered.ends_with("13m "), "{rendered:?}");
}

#[test]
fn dense_tier_renders_badge_glyph_in_status_color() {
    let mut chat = chat_row(
        "chat::%1",
        "claude: fix the bug",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("claude".to_string()),
        elapsed_secs: Some(780),
        origin: Some("misc/vde-tmux".to_string()),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();

    let lines = render_lines(&[chat], &SidebarState::default(), 30, &theme);

    let glyph = lines[0]
        .spans
        .iter()
        .find(|span| span.content == "●")
        .unwrap_or_else(|| panic!("badge glyph span not found: {:?}", lines[0]));
    assert_eq!(glyph.style.fg, Some(theme.badge_color(BadgeState::Working)));
    let agent = lines[0]
        .spans
        .iter()
        .find(|span| span.content.contains("Claude"))
        .unwrap_or_else(|| panic!("agent span not found: {:?}", lines[0]));
    assert_eq!(agent.style.fg, Some(CLAUDE_AGENT_COLOR));
    assert!(agent.style.add_modifier.contains(Modifier::BOLD));
    let origin = lines[0]
        .spans
        .iter()
        .find(|span| span.content.contains("vde"))
        .unwrap_or_else(|| panic!("origin span not found: {:?}", lines[0]));
    assert_eq!(origin.style.fg, Some(AGENT_ORIGIN_COLOR));
}

#[test]
fn micro_tier_renders_glyph_and_status_only() {
    let mut chat = chat_row(
        "chat::%1",
        "codex",
        RollupLevel::Permission,
        BadgeState::Blocked,
    );
    chat.expanded = false;

    let rendered = render_rows(&[chat], &SidebarState::default(), 8);

    assert_eq!(rendered, " ▲      ");
}

#[test]
fn rail_renders_counts_then_rows() {
    let blocked1 = chat_row(
        "chat::%1",
        "codex",
        RollupLevel::Permission,
        BadgeState::Blocked,
    );
    let blocked2 = chat_row(
        "chat::%2",
        "claude",
        RollupLevel::Permission,
        BadgeState::Blocked,
    );
    let working = chat_row(
        "chat::%3",
        "opencode",
        RollupLevel::Running,
        BadgeState::Working,
    );

    let rendered = render_rows(&[blocked1, blocked2, working], &SidebarState::default(), 3);

    assert_eq!(rendered, "▲2\n●1\n──\n ▲\n ▲\n ●");
}

#[test]
fn pane_pin_markers_fit_every_width_tier_and_view() {
    let mut pinned = chat_row(
        "chat::%1::100",
        "codex: pinned unread",
        RollupLevel::Permission,
        BadgeState::Blocked,
    );
    pinned.expanded = false;
    pinned.meta = Some(crate::sidebar::tree::RowMeta {
        is_unread: true,
        pinned: true,
        agent: Some("codex".to_string()),
        ..Default::default()
    });
    let mut priority = SidebarState {
        presentation_mode: PresentationMode::Priority,
        ..SidebarState::default()
    };
    let theme = SidebarRenderTheme::default();

    for width in [3, 8, 30, 40] {
        let rendered = render_rows(std::slice::from_ref(&pinned), &priority, width);
        assert!(rendered.contains('✦'), "width={width}: {rendered:?}");
        assert!(
            rendered.lines().all(|line| display_width(line) <= width),
            "width={width}: {rendered:?}"
        );
        let styled = render_lines(std::slice::from_ref(&pinned), &priority, width, &theme);
        assert!(
            styled.iter().flat_map(|line| &line.spans).any(|span| {
                span.content.as_ref() == "✦" && span.style.fg == Some(theme.toggle)
            }),
            "width={width}: {styled:?}"
        );
    }
    let styled = render_lines(std::slice::from_ref(&pinned), &priority, 40, &theme);
    assert_span_fg(&styled[0].spans, "✦", theme.toggle);

    priority.presentation_mode = PresentationMode::Flat;
    let flat = render_rows(std::slice::from_ref(&pinned), &priority, 40);
    assert!(flat.contains('✦'), "{flat:?}");

    pinned.meta.as_mut().unwrap().pinned = false;
    priority.presentation_mode = PresentationMode::Priority;
    let unpinned = render_rows(std::slice::from_ref(&pinned), &priority, 40);
    assert!(unpinned.contains('·'), "{unpinned:?}");
    assert!(!unpinned.contains('✦'), "{unpinned:?}");

    priority.presentation_mode = PresentationMode::Flat;
    let outside_priority = render_rows(&[pinned], &priority, 40);
    assert!(!outside_priority.contains('✦'), "{outside_priority:?}");
    assert!(!outside_priority.contains('·'), "{outside_priority:?}");
}

#[test]
fn rail_uses_explicit_overflow_marker_for_double_digit_counts() {
    let rows = (0..10)
        .map(|index| {
            chat_row(
                &format!("chat::%{index}"),
                "codex",
                RollupLevel::Running,
                BadgeState::Working,
            )
        })
        .collect::<Vec<_>>();

    let rendered = render_rows(&rows, &SidebarState::default(), 3);

    assert_eq!(rendered.lines().next(), Some("●9+"));
    assert!(rendered.lines().all(|line| display_width(line) <= 3));
}

#[test]
fn rail_counts_expanded_chat_once() {
    let mut chat = row(
        "chat::%1",
        SidebarRowKind::Chat,
        1,
        "codex",
        RollupLevel::Running,
    );
    chat.badge_state = Some(BadgeState::Working);
    chat.expanded = true;
    chat.pane_id = Some("%1".to_string());
    let text = render_rows(&[chat], &SidebarState::default(), 2);

    assert_eq!(text, "●1\n──\n ●");
}

#[test]
fn dense_micro_and_rail_modes_continue_to_omit_detail_rows() {
    let mut chat = row(
        "chat::%1",
        SidebarRowKind::Chat,
        0,
        "codex",
        RollupLevel::Running,
    );
    chat.badge_state = Some(BadgeState::Working);
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        elapsed_secs: Some(60),
        ..Default::default()
    });
    let detail = detail_row(
        "detail::%1::task::0::in_progress",
        "\u{2514} ● Build",
        RollupLevel::Running,
    );
    let rows = vec![chat, detail];

    for width in [2, 12, 30] {
        let rendered = render_rows(&rows, &SidebarState::default(), width);
        assert!(!rendered.contains("Build"), "{width}: {rendered:?}");
    }
}

#[test]
fn render_rows_includes_current_agent_indentation_and_rollup() {
    let rows = vec![
        row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Running,
        ),
        row(
            "chat::%1::10",
            SidebarRowKind::Chat,
            1,
            "codex %1",
            RollupLevel::Running,
        ),
    ];
    let state = SidebarState {
        selection: Some("chat::%1::10".to_string()),
        current_agents: BTreeSet::from([crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        }]),
        ..SidebarState::default()
    };

    let rendered = render_rows(&rows, &state, 40);

    assert!(rendered.contains(" ▾ app"));
    assert!(rendered.contains("▎   ▾ Codex %1"));
}

#[test]
fn render_rows_uses_rail_for_narrow_width() {
    let chat = chat_row(
        "chat::%1",
        "codex %1",
        RollupLevel::Permission,
        BadgeState::Blocked,
    );
    let rows = vec![chat];
    let rendered = render_rows(&rows, &SidebarState::default(), 2);
    assert_eq!(rendered, "▲1\n──\n ▲");
}

#[test]
fn render_repo_row_includes_git_badge() {
    let mut repo = repo_row("app", RollupLevel::Running);
    repo.git = Some(crate::git::GitBadge {
        branch: "main".to_string(),
        ahead: 2,
        behind: 1,
        insertions: 184,
        deletions: 37,
    });

    let rendered = render_rows(&[repo], &SidebarState::default(), 80);

    assert!(rendered.contains("main ↑2 ↓1 +184 -37"));
}

#[test]
fn render_repo_row_omits_zero_git_counts() {
    let mut repo = repo_row("app", RollupLevel::Idle);
    repo.git = Some(crate::git::GitBadge {
        branch: "main".to_string(),
        ahead: 0,
        behind: 0,
        insertions: 0,
        deletions: 0,
    });

    let rendered = render_rows(&[repo], &SidebarState::default(), 80);

    assert!(rendered.contains("▾ app main"));
    assert!(!rendered.contains("+0"));
    assert!(!rendered.contains("-0"));
    assert!(!rendered.contains("↑0"));
    assert!(!rendered.contains("↓0"));
}

#[test]
fn render_lines_color_rollup_category_selection_and_git_badges() {
    let mut repo = repo_row("app", RollupLevel::Running);
    repo.git = Some(crate::git::GitBadge {
        branch: "main".to_string(),
        ahead: 2,
        behind: 1,
        insertions: 12,
        deletions: 3,
    });
    let category = category_row("misc", RollupLevel::Idle);
    let state = SidebarState {
        selection: Some("repo::misc::app".to_string()),
        ..SidebarState::default()
    };

    let theme = SidebarRenderTheme::default();
    let lines = render_lines(&[category, repo], &state, 80, &theme);

    assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.content.as_ref() == "▾ "
                && span.style.fg == Some(Color::Indexed(147))
                && span.style.add_modifier.contains(Modifier::BOLD)),
        "{:?}",
        lines[0]
    );
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.content.trim() == "misc"
                && span.style.fg == Some(Color::Indexed(215))
                && span.style.add_modifier.contains(Modifier::BOLD)),
        "{:?}",
        lines[0]
    );
    assert_eq!(lines[1].style.bg, Some(Color::Rgb(0x30, 0x30, 0x34)));
    assert!(
        lines[1].spans.iter().any(|span| {
            span.content.trim() == "↑2" && span.style.fg == Some(theme.git_ahead)
        })
    );
    assert!(
        lines[1].spans.iter().any(|span| {
            span.content.trim() == "↓1" && span.style.fg == Some(theme.git_behind)
        })
    );
    assert!(lines[1].spans.iter().any(|span| {
        span.content.trim() == "+12" && span.style.fg == Some(theme.git_insertions)
    }));
    assert!(
        lines[1].spans.iter().any(|span| {
            span.content.trim() == "-3" && span.style.fg == Some(theme.git_deletions)
        })
    );
}

#[test]
fn category_and_repo_rows_use_distinct_colors() {
    let theme = SidebarRenderTheme::default();
    let category = category_row("misc", RollupLevel::Idle);
    let repo = row(
        "repo::misc::app",
        SidebarRowKind::Repo,
        0,
        "app",
        RollupLevel::Idle,
    );

    assert_eq!(row_style(&category, &theme).fg, Some(Color::Indexed(215)));
    assert_eq!(row_style(&repo, &theme).fg, Some(Color::LightCyan));
}

#[test]
fn category_row_label_omits_diamond_in_every_tier() {
    let category = category_row("dev", RollupLevel::Idle);

    let standard = render_rows(
        std::slice::from_ref(&category),
        &SidebarState::default(),
        40,
    );
    let dense = render_rows(&[category], &SidebarState::default(), 30);

    assert!(standard.contains("▾ dev"), "{standard:?}");
    assert!(!standard.contains("⋄"), "{standard:?}");
    assert!(!dense.contains("⋄"), "{dense:?}");
}

#[test]
fn category_row_fills_remaining_width_with_rule() {
    let mut category = category_row("dev", RollupLevel::Idle);
    category.meta = Some(crate::sidebar::tree::RowMeta {
        attention_count: Some(1),
        ..Default::default()
    });

    let rendered = render_rows(&[category], &SidebarState::default(), 40);

    assert!(rendered.contains("▾ dev ─"), "{rendered:?}");
    assert!(rendered.contains("─ ▲1 "), "{rendered:?}");
}

#[test]
fn repo_and_chat_rows_keep_space_filler() {
    let repo = row(
        "repo::misc::app",
        SidebarRowKind::Repo,
        0,
        "app",
        RollupLevel::Idle,
    );
    let chat = row(
        "chat::%1",
        SidebarRowKind::Chat,
        0,
        "codex",
        RollupLevel::Idle,
    );

    let rendered = render_rows(&[repo, chat], &SidebarState::default(), 40);

    assert!(!rendered.contains('─'), "{rendered:?}");
}

#[test]
fn active_rows_render_left_bar_without_chat_bg() {
    let mut category = category_row("dev", RollupLevel::Idle);
    category.active = true;
    let mut chat = row(
        "chat::%1::10",
        SidebarRowKind::Chat,
        1,
        "codex: active prompt",
        RollupLevel::Running,
    );
    chat.active = true;
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        task_summary: Some("active task".to_string()),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();

    let lines = render_lines(
        &[category.clone(), chat.clone()],
        &SidebarState::default(),
        40,
        &theme,
    );

    assert_eq!(line_to_string(lines[0].clone()).chars().next(), Some('▎'));
    assert_eq!(lines[0].spans[0].style.fg, Some(theme.active_bar));
    assert_eq!(lines[0].style.bg, None);
    assert_eq!(line_to_string(lines[1].clone()).chars().next(), Some('▎'));
    assert_eq!(lines[1].spans[0].style.fg, Some(theme.active_bar));
    assert_eq!(lines[1].style.bg, None);
    assert_eq!(line_to_string(lines[2].clone()).chars().next(), Some('▎'));
    assert_eq!(lines[2].spans[0].style.fg, Some(theme.active_bar));
    assert_eq!(lines[2].style.bg, None);

    let selected = SidebarState {
        selection: Some("chat::%1::10".to_string()),
        current_agents: BTreeSet::from([crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        }]),
        ..SidebarState::default()
    };
    let selected_lines = render_lines(&[chat], &selected, 40, &theme);
    assert_eq!(selected_lines[0].style.bg, Some(theme.selection_bg));
    assert_eq!(selected_lines[1].style.bg, Some(theme.selection_bg));
    assert_eq!(
        selected_lines[0].spans[0].style.fg,
        Some(theme.selection_bar)
    );
    assert_ne!(theme.selection_bar, theme.active_bar);
    assert_eq!(
        line_to_string(selected_lines[0].clone()).chars().next(),
        Some('▎')
    );
    assert_eq!(
        line_to_string(selected_lines[1].clone()).chars().next(),
        Some('▎')
    );
}

#[test]
fn expanded_chat_selection_styles_chat_and_detail_rows() {
    let mut chat = row(
        "chat::%1::101",
        SidebarRowKind::Chat,
        0,
        "codex",
        RollupLevel::Running,
    );
    chat.expanded = true;
    chat.pane_id = Some("%1".to_string());
    let mut detail = row(
        "detail::%1::101::prompt",
        SidebarRowKind::Detail,
        1,
        "review PR",
        RollupLevel::Running,
    );
    detail.pane_id = Some("%1".to_string());
    let other = row(
        "chat::%2::202",
        SidebarRowKind::Chat,
        0,
        "claude",
        RollupLevel::Running,
    );
    let state = SidebarState {
        selection: Some("chat::%1::101".to_string()),
        ..SidebarState::default()
    };
    let theme = SidebarRenderTheme::default();

    let lines = render_lines(&[chat, detail, other], &state, 60, &theme);

    assert_eq!(lines[0].style.bg, Some(theme.selection_bg));
    assert_eq!(lines[1].style.bg, Some(theme.selection_bg));
    assert_eq!(lines[2].style.bg, None);
    assert_eq!(line_to_string(lines[0].clone()).chars().next(), Some(' '));
    assert_eq!(line_to_string(lines[1].clone()).chars().next(), Some(' '));
}

#[test]
fn category_row_never_renders_badge_glyph() {
    let mut category = category_row("dev", RollupLevel::Permission);
    category.badge_state = Some(BadgeState::Blocked);

    let lines = render_lines(
        std::slice::from_ref(&category),
        &SidebarState::default(),
        40,
        &SidebarRenderTheme::default(),
    );
    let text = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(!text.contains('▲'), "{text:?}");
}

#[test]
fn chat_rows_render_badge_glyph_and_omit_trailing_status_text() {
    let chat = chat_row(
        "chat::%1",
        "codex (%1)",
        RollupLevel::Running,
        BadgeState::Working,
    );

    let rendered = render_rows(&[chat], &SidebarState::default(), 80);

    assert!(rendered.contains("● Codex (%1)"), "{rendered}");
    assert!(!rendered.contains("[Running]"), "{rendered}");
}

#[test]
fn colorize_follows_ideal_multi_tone_scheme() {
    let mut chat = chat_row(
        "chat::%1",
        "claude: fix flicker",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.active = true;
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("claude".to_string()),
        task_summary: Some("fix flicker".to_string()),
        elapsed_secs: Some(780),
        ..Default::default()
    });
    let detail = detail_row("detail::%1::note", "plain detail", RollupLevel::Running);
    let lines = render_lines(
        &[chat, detail],
        &SidebarState::default(),
        40,
        &SidebarRenderTheme::default(),
    );

    let chat_spans = &lines[0].spans;
    assert!(
        chat_spans
            .iter()
            .any(|span| span.content.as_ref() == "Claude"
                && span.style.fg == Some(CLAUDE_AGENT_COLOR)
                && span.style.add_modifier.contains(Modifier::BOLD)),
        "{chat_spans:?}"
    );
    assert!(
        chat_spans
            .iter()
            .any(|span| span.content.as_ref() == "Claude"
                && span.style.fg == Some(CLAUDE_AGENT_COLOR)
                && span.style.add_modifier.contains(Modifier::BOLD)),
        "{chat_spans:?}"
    );
    let prompt_spans = &lines[1].spans;
    assert!(
        prompt_spans
            .iter()
            .any(|span| span.content.as_ref().contains("fix flicker")
                && span.style.fg == Some(Color::Reset)
                && !span.style.add_modifier.contains(Modifier::BOLD)),
        "{prompt_spans:?}"
    );
    assert_eq!(chat_spans[0].content.as_ref(), "▎");
    assert_eq!(chat_spans[0].style.fg, Some(Color::Indexed(147)));
    assert!(
        chat_spans.iter().any(|span| span.content.as_ref() == "▸ "
            && span.style.fg == Some(Color::Indexed(147))
            && span.style.add_modifier.contains(Modifier::BOLD)),
        "{chat_spans:?}"
    );
    assert!(
        chat_spans.iter().any(|span| span.content.as_ref() == "13m"
            && span.style.fg == Some(Color::Green)
            && !span.style.add_modifier.contains(Modifier::DIM)),
        "{chat_spans:?}"
    );
    let detail_spans = &lines[2].spans;
    assert!(
        detail_spans
            .iter()
            .any(|span| span.content.as_ref().contains("plain detail")
                && span.style.fg == Some(Color::Indexed(246))
                && !span.style.add_modifier.contains(Modifier::DIM)),
        "{detail_spans:?}"
    );
}

#[test]
fn task_detail_rows_colorize_status_icons() {
    let theme = SidebarRenderTheme::default();
    let rows = vec![
        detail_row(
            "detail::%1::task::0::completed",
            "\u{251c} ✓ Explore",
            RollupLevel::Running,
        ),
        detail_row(
            "detail::%1::task::1::in_progress",
            "\u{251c} ● Build",
            RollupLevel::Running,
        ),
        detail_row(
            "detail::%1::task::2::pending",
            "\u{2514} ○ Verify",
            RollupLevel::Running,
        ),
    ];

    let lines = render_lines(&rows, &SidebarState::default(), 60, &theme);

    assert_span_fg(&lines[0].spans, "✓", theme.task_done);
    assert_span_fg(&lines[1].spans, "●", theme.task_working);
    assert_span_fg(&lines[2].spans, "○", theme.task_pending);
    assert_span_fg(&lines[0].spans, " Explore", theme.task_label);
    assert_span_fg(&lines[0].spans, "\u{251c} ", theme.marker);
}

#[test]
fn subagent_detail_rows_colorize_label_and_id() {
    let theme = SidebarRenderTheme::default();
    let detail = detail_row(
        "detail::%1::subagent::0",
        "\u{2514} Agent - Explore #sub1",
        RollupLevel::Running,
    );

    let lines = render_lines(&[detail], &SidebarState::default(), 60, &theme);

    assert_span_fg(&lines[0].spans, "\u{2514} ", theme.marker);
    assert_span_fg(&lines[0].spans, "Agent - Explore", theme.subagent_label);
    assert_span_fg(&lines[0].spans, " #sub1", theme.subagent_id);
}

#[test]
fn worktree_detail_row_uses_worktree_color() {
    let theme = SidebarRenderTheme::default();
    let detail = detail_row("detail::%1::worktree", "+ feature", RollupLevel::Running);

    let lines = render_lines(&[detail], &SidebarState::default(), 60, &theme);

    assert_span_fg(&lines[0].spans, "+ feature", theme.worktree);
}

#[test]
fn worktree_activity_detail_row_uses_worktree_activity_color() {
    let theme = SidebarRenderTheme::default();
    let detail = detail_row(
        "detail::%1::worktree-activity",
        "vw exec feature",
        RollupLevel::Running,
    );

    let lines = render_lines(&[detail], &SidebarState::default(), 60, &theme);

    assert_span_fg(&lines[0].spans, "vw exec feature", theme.worktree_activity);
}

#[test]
fn summary_is_rendered_as_primary_detail_text() {
    let theme = SidebarRenderTheme::default();
    let summary = detail_row(
        "detail::%1::summary",
        "sidebar task summary",
        RollupLevel::Running,
    );
    let lines = render_lines(&[summary], &SidebarState::default(), 60, &theme);

    assert_span_fg(&lines[0].spans, "sidebar task summary", Color::Reset);
}

#[test]
fn expanded_agent_header_separates_identity_origin_and_summary_colors() {
    let theme = SidebarRenderTheme::default();
    let mut chat = row(
        "chat::%1::101",
        SidebarRowKind::Chat,
        0,
        "Codex · public/vde-tmux",
        RollupLevel::Running,
    );
    chat.expanded = true;
    chat.badge_state = Some(BadgeState::Working);
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        ..Default::default()
    });
    let summary = detail_row(
        "detail::%1::summary",
        "サイドバー表示を改善",
        RollupLevel::Running,
    );

    let lines = render_lines(&[chat, summary], &SidebarState::default(), 60, &theme);

    assert_span_fg(&lines[0].spans, "Codex", CODEX_AGENT_COLOR);
    assert!(
        lines[0].spans.iter().any(|span| {
            span.content.as_ref() == "Codex" && span.style.add_modifier.contains(Modifier::BOLD)
        }),
        "{:?}",
        lines[0].spans
    );
    assert_span_fg(&lines[0].spans, " · public/vde-tmux", AGENT_ORIGIN_COLOR);
    assert_span_fg(&lines[1].spans, "サイドバー表示を改善", Color::Reset);
}

#[test]
fn closed_agent_header_uses_the_same_identity_origin_and_summary_colors() {
    let theme = SidebarRenderTheme::default();
    let mut chat = row(
        "chat::%1::101",
        SidebarRowKind::Chat,
        0,
        "Codex · public/vde-tmux",
        RollupLevel::Running,
    );
    chat.expanded = false;
    chat.badge_state = Some(BadgeState::Working);
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        origin: Some("public/vde-tmux".to_string()),
        task_summary: Some("サイドバー表示を改善".to_string()),
        elapsed_secs: Some(60),
        ..Default::default()
    });

    let lines = render_lines(&[chat], &SidebarState::default(), 64, &theme);

    assert_span_fg(&lines[0].spans, "Codex", CODEX_AGENT_COLOR);
    assert!(
        lines[0].spans.iter().any(|span| {
            span.content.as_ref() == "Codex" && span.style.add_modifier.contains(Modifier::BOLD)
        }),
        "{:?}",
        lines[0].spans
    );
    assert_span_fg(&lines[0].spans, " · public/vde-tmux", AGENT_ORIGIN_COLOR);
    assert_span_fg(&lines[1].spans, "サイドバー表示を改善", Color::Reset);
}

#[test]
fn signal_background_and_response_use_distinct_semantic_colors() {
    let theme = SidebarRenderTheme::default();
    let mut signal = detail_row(
        "detail::%1::signal",
        "feature  ↑ 2  ↓ 1  +184  -37  ☑ 1/3  :3000",
        RollupLevel::Running,
    );
    signal.git = Some(crate::git::GitBadge {
        branch: "feature".to_string(),
        ahead: 2,
        behind: 1,
        insertions: 184,
        deletions: 37,
    });
    signal.meta = Some(crate::sidebar::tree::RowMeta {
        tasks_done: Some(1),
        tasks_total: Some(3),
        ..Default::default()
    });
    let rows = vec![
        signal,
        detail_row(
            "detail::%1::background",
            "◎ $ pnpm dev",
            RollupLevel::Running,
        ),
        detail_row(
            "detail::%1::response",
            "▷ server is ready",
            RollupLevel::Idle,
        ),
    ];

    let lines = render_lines(&rows, &SidebarState::default(), 60, &theme);

    assert_span_fg(&lines[0].spans, "feature", theme.branch);
    assert_span_fg(&lines[0].spans, "↑ 2", theme.git_ahead);
    assert_span_fg(&lines[0].spans, "↓ 1", theme.git_behind);
    assert_span_fg(&lines[0].spans, "+184", theme.git_insertions);
    assert_span_fg(&lines[0].spans, "-37", theme.git_deletions);
    assert_span_fg(&lines[0].spans, "☑ 1/3", theme.task_working);
    assert_span_fg(&lines[0].spans, ":3000", theme.branch);
    assert_span_fg(&lines[1].spans, "◎ $ pnpm dev", theme.badge_working);
    assert_span_fg(&lines[2].spans, "▷ ", theme.branch);
    assert_span_fg(&lines[2].spans, "server is ready", RESPONSE_PREVIEW_COLOR);
}

#[test]
fn triage_origin_uses_the_muted_teal_identity_context_color() {
    let origin = detail_row(
        "detail::%1::origin",
        "origin: public/vde-tmux",
        RollupLevel::Idle,
    );

    let lines = render_lines(
        &[origin],
        &SidebarState::default(),
        60,
        &SidebarRenderTheme::default(),
    );

    assert_span_fg(
        &lines[0].spans,
        "origin: public/vde-tmux",
        AGENT_ORIGIN_COLOR,
    );
}

#[test]
fn narrow_width_truncates_task_detail_without_panicking() {
    let detail = detail_row(
        "detail::%1::task::0::in_progress",
        "\u{2514} ● Implement an extremely long task label",
        RollupLevel::Running,
    );

    let rendered = render_rows(&[detail], &SidebarState::default(), 36);

    assert!(rendered.contains('●'), "{rendered:?}");
}

#[test]
fn expanded_chat_row_right_aligns_time_without_state_text() {
    let mut chat = chat_row(
        "chat::%1",
        "codex",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = true;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        elapsed_secs: Some(720),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();

    assert_eq!(right_label(&chat).as_deref(), Some("12m 00s"));
    assert_eq!(
        right_style(&chat, &theme).fg,
        Some(theme.badge_color(BadgeState::Working))
    );
    let lines = render_lines(&[chat], &SidebarState::default(), 40, &theme);
    let chat_spans = &lines[0].spans;

    assert!(
        chat_spans
            .iter()
            .any(|span| span.content.as_ref() == "Codex"
                && span.style.add_modifier.contains(Modifier::BOLD)
                && span.style.fg == Some(CODEX_AGENT_COLOR)),
        "{chat_spans:?}"
    );
    assert!(
        !chat_spans
            .iter()
            .any(|span| span.content.as_ref() == "Running"),
        "{chat_spans:?}"
    );
    assert_span_fg(
        chat_spans,
        "12m 00s",
        theme.badge_color(BadgeState::Working),
    );
    assert!(line_to_string(lines[0].clone()).ends_with("12m 00s "));
}

#[test]
fn expanded_chat_rows_omit_state_text_for_every_badge() {
    let cases = [
        (BadgeState::Blocked, RollupLevel::Error, "2m 00s"),
        (BadgeState::Working, RollupLevel::Running, "2m 00s"),
        (BadgeState::Done, RollupLevel::Idle, "2m00s ago"),
        (BadgeState::Idle, RollupLevel::Idle, "2m00s ago"),
    ];

    for (badge, rollup, expected) in cases {
        let mut chat = chat_row("chat::%1", "codex", rollup, badge);
        chat.expanded = true;
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            elapsed_secs: Some(120),
            completed_age_secs: Some(120),
            ..Default::default()
        });
        assert_eq!(right_label(&chat).as_deref(), Some(expected));
    }
}

#[test]
fn expanded_blocked_chat_row_shows_only_elapsed_time() {
    let mut chat = chat_row(
        "chat::%1",
        "codex",
        RollupLevel::Permission,
        BadgeState::Blocked,
    );
    chat.expanded = true;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        wait_reason: Some("permission_prompt".to_string()),
        elapsed_secs: Some(120),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();

    assert_eq!(right_label(&chat).as_deref(), Some("2m 00s"));
    let lines = render_lines(&[chat], &SidebarState::default(), 60, &theme);
    let chat_spans = &lines[0].spans;

    assert_span_fg(chat_spans, "2m 00s", theme.badge_color(BadgeState::Blocked));
    assert!(line_to_string(lines[0].clone()).ends_with("2m 00s "));
    assert!(!line_to_string(lines[0].clone()).contains("permission_prompt"));
}

#[test]
fn usage_limit_wait_reason_has_a_distinct_compact_label() {
    assert_eq!(short_wait_reason("usage_limit"), "usage-limit");
    assert_ne!(short_wait_reason("usage_limit"), "rate-limit");
}

#[test]
fn expanded_idle_chat_row_right_aligns_completed_age() {
    let mut chat = chat_row("chat::%1", "codex", RollupLevel::Idle, BadgeState::Idle);
    chat.expanded = true;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        completed_age_secs: Some(815),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();

    assert_eq!(right_label(&chat).as_deref(), Some("13m ago"));
    assert_eq!(
        right_style(&chat, &theme).fg,
        Some(theme.badge_color(BadgeState::Idle))
    );

    let rendered = render_rows(&[chat], &SidebarState::default(), 32);
    assert!(rendered.ends_with("13m ago "), "{rendered:?}");
}

#[test]
fn expanded_done_chat_row_right_aligns_done_age_with_done_color() {
    let mut chat = chat_row("chat::%1", "codex", RollupLevel::Idle, BadgeState::Done);
    chat.expanded = true;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        completed_age_secs: Some(815),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();

    assert_eq!(right_label(&chat).as_deref(), Some("13m ago"));
    assert_eq!(
        right_style(&chat, &theme).fg,
        Some(theme.badge_color(BadgeState::Done))
    );

    let lines = render_lines(&[chat], &SidebarState::default(), 32, &theme);
    assert_span_fg(
        &lines[0].spans,
        "13m ago",
        theme.badge_color(BadgeState::Done),
    );
}

#[test]
fn repo_branch_is_rendered_in_branch_color() {
    let mut repo = repo_row("app", RollupLevel::Running);
    repo.git = Some(crate::git::GitBadge {
        branch: "main".to_string(),
        ahead: 0,
        behind: 0,
        insertions: 0,
        deletions: 0,
    });
    let lines = render_lines(
        &[repo],
        &SidebarState::default(),
        40,
        &SidebarRenderTheme::default(),
    );
    let spans = &lines[0].spans;
    assert!(
        spans.iter().any(|span| span.content.as_ref() == "app"
            && span.style.fg == Some(Color::LightCyan)
            && span.style.add_modifier.contains(Modifier::BOLD)),
        "{spans:?}"
    );
    assert!(
        spans.iter().any(|span| span.content.trim() == "main"
            && span.style.fg == Some(Color::Indexed(73))
            && !span.style.add_modifier.contains(Modifier::BOLD)),
        "{spans:?}"
    );
}

#[test]
fn rail_uses_badge_glyphs() {
    let chat = chat_row("chat::%1", "codex", RollupLevel::Idle, BadgeState::Done);

    let rendered = render_rows(&[chat], &SidebarState::default(), 2);

    assert_eq!(rendered, "✓1\n──\n ✓");
}

#[test]
fn selected_group_uses_background_without_a_current_agent_marker() {
    let rows = vec![row(
        "repo::misc::app",
        SidebarRowKind::Repo,
        0,
        "app",
        RollupLevel::Running,
    )];
    let state = SidebarState {
        selection: Some("repo::misc::app".to_string()),
        ..SidebarState::default()
    };
    let rendered = render_rows(&rows, &state, 40);
    assert!(rendered.starts_with(" ▾ app"), "{rendered:?}");
    assert_eq!(display_width(&rendered), 40, "{rendered:?}");
}

#[test]
fn current_agent_has_a_yellow_marker_in_every_width_tier() {
    let chat = chat_row(
        "chat::%1::10",
        "codex",
        RollupLevel::Running,
        BadgeState::Working,
    );
    let state = SidebarState {
        current_agents: BTreeSet::from([crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        }]),
        ..SidebarState::default()
    };

    for width in [2, 8, 30, 40] {
        let rendered = render_rows(std::slice::from_ref(&chat), &state, width);
        assert!(rendered.contains('▎'), "{width}: {rendered:?}");
        let lines = render_lines(
            std::slice::from_ref(&chat),
            &state,
            width,
            &SidebarRenderTheme::default(),
        );
        assert!(
            lines.iter().flat_map(|line| &line.spans).any(|span| {
                span.content == "▎"
                    && span.style.fg == Some(SidebarRenderTheme::default().selection_bar)
            }),
            "{width}: {lines:?}"
        );
    }
}

#[test]
fn selected_chat_without_a_current_agent_has_no_yellow_marker() {
    let chat = chat_row(
        "chat::%1::10",
        "codex",
        RollupLevel::Running,
        BadgeState::Working,
    );
    let state = SidebarState {
        selection: Some("chat::%1::10".to_string()),
        ..SidebarState::default()
    };
    let theme = SidebarRenderTheme::default();

    let lines = render_lines(&[chat], &state, 40, &theme);

    assert_eq!(lines[0].style.bg, Some(theme.selection_bg));
    assert!(
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| { span.content != "▎" || span.style.fg != Some(theme.selection_bar) })
    );
}

#[test]
fn boundary_width_ascii_cjk_emoji_golden() {
    let state = SidebarState {
        selection: Some("chat::%1::10".to_string()),
        current_agents: BTreeSet::from([crate::pane_state::PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 10,
        }]),
        ..SidebarState::default()
    };
    for label in ["Codex: fix sidebar", "Codex: 修正確認", "Codex: fix 🧭✨"] {
        let mut chat = chat_row(
            "chat::%1::10",
            label,
            RollupLevel::Running,
            BadgeState::Working,
        );
        chat.expanded = false;
        chat.pane_id = Some("%1".to_string());
        chat.meta = Some(crate::sidebar::tree::RowMeta {
            agent: Some("codex".to_string()),
            prompt: label
                .split_once(':')
                .map(|(_, prompt)| prompt.trim().to_string()),
            task_summary: label
                .split_once(':')
                .map(|(_, prompt)| prompt.trim().to_string()),
            elapsed_secs: Some(90),
            ..Default::default()
        });
        for width in [16, 24, 35, 36] {
            let lines = render_lines(
                std::slice::from_ref(&chat),
                &state,
                width,
                &SidebarRenderTheme::default(),
            );
            let rendered = lines.into_iter().map(line_to_string).collect::<Vec<_>>();
            assert!(
                rendered.iter().all(|line| display_width(line) <= width),
                "{label:?} width={width}: {rendered:?}"
            );
            assert!(rendered.iter().any(|line| line.contains('▎')));
            let expected = match (label, width) {
                ("Codex: fix sidebar", 16) => vec!["▎● 1m30s        "],
                ("Codex: fix sidebar", 24) => vec!["▎● Codex  fix si… 1m30s "],
                ("Codex: fix sidebar", 35) => {
                    vec!["▎● Codex  fix sidebar        1m30s "]
                }
                ("Codex: fix sidebar", 36) => vec![
                    "▎ ▸ ● Codex                   1m30s ",
                    "     fix sidebar                    ",
                ],
                ("Codex: 修正確認", 16) => vec!["▎● 1m30s        "],
                ("Codex: 修正確認", 24) => vec!["▎● Codex  修正確… 1m30s "],
                ("Codex: 修正確認", 35) => {
                    vec!["▎● Codex  修正確認           1m30s "]
                }
                ("Codex: 修正確認", 36) => vec![
                    "▎ ▸ ● Codex                   1m30s ",
                    "     修正確認                       ",
                ],
                ("Codex: fix 🧭✨", 16) => vec!["▎● 1m30s        "],
                ("Codex: fix 🧭✨", 24) => vec!["▎● Codex  fix 🧭… 1m30s "],
                ("Codex: fix 🧭✨", 35) => {
                    vec!["▎● Codex  fix 🧭✨           1m30s "]
                }
                ("Codex: fix 🧭✨", 36) => vec![
                    "▎ ▸ ● Codex                   1m30s ",
                    "     fix 🧭✨                       ",
                ],
                _ => unreachable!(),
            };
            assert_eq!(rendered, expected, "{label:?} width={width}");
        }
    }
}

#[test]
fn repo_row_right_aligns_attention_count() {
    let mut repo = row(
        "repo::misc::app",
        SidebarRowKind::Repo,
        0,
        "app",
        RollupLevel::Permission,
    );
    repo.meta = Some(crate::sidebar::tree::RowMeta {
        attention_count: Some(2),
        ..Default::default()
    });
    let rendered = render_rows(&[repo], &SidebarState::default(), 40);
    assert!(rendered.ends_with("▲2 "), "{rendered:?}");
    assert!(!rendered.contains("[permission:"), "{rendered:?}");
}

#[test]
fn closed_unoperated_agent_omits_empty_second_line() {
    let mut chat = row(
        "chat::%1::101",
        SidebarRowKind::Chat,
        0,
        "codex",
        RollupLevel::Idle,
    );
    chat.badge_state = Some(BadgeState::Idle);
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        prompt: None,
        wait_reason: None,
        ..Default::default()
    });
    let rendered = render_lines_with_indices(
        &[chat],
        &SidebarState::default(),
        64,
        &SidebarRenderTheme::default(),
    );

    assert_eq!(rendered.lines.len(), 1);
    assert_eq!(rendered.row_indices, vec![Some(0)]);
}

#[test]
fn closed_chat_standard_renders_two_line_digest_with_signals() {
    let mut chat = row(
        "chat::%1",
        SidebarRowKind::Chat,
        0,
        "codex: review sidebar state shape",
        RollupLevel::Permission,
    );
    chat.badge_state = Some(BadgeState::Blocked);
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        prompt: Some("review sidebar state shape".to_string()),
        task_summary: Some("review sidebar state shape".to_string()),
        wait_reason: Some("permission_prompt".to_string()),
        elapsed_secs: Some(127),
        tasks_done: Some(2),
        tasks_total: Some(5),
        subagent_count: Some(2),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();
    let rendered = render_lines_with_indices(&[chat], &SidebarState::default(), 64, &theme);
    let text = rendered
        .lines
        .iter()
        .cloned()
        .map(line_to_string)
        .collect::<Vec<_>>();

    assert_eq!(rendered.row_indices, vec![Some(0), Some(0)]);
    assert_eq!(text.len(), 2);
    assert!(text[0].contains("▸ ▲ Codex"), "{text:?}");
    assert!(text[0].ends_with("☑ 2/5 · ↳ 2 · 2m07s "), "{text:?}");
    assert!(
        text[1].starts_with("     review sidebar state shape"),
        "{text:?}"
    );
    assert!(text[1].ends_with("↩ permission "), "{text:?}");
    assert_span_fg(&rendered.lines[0].spans, "☑ 2/5", theme.task_working);
    assert_span_fg(&rendered.lines[0].spans, "↳ 2", theme.subagent_label);
    assert_span_fg(
        &rendered.lines[0].spans,
        "2m07s",
        theme.badge_color(BadgeState::Blocked),
    );
    assert!(
        text.iter().all(|line| display_width(line) == 64),
        "{text:?}"
    );
}

#[test]
fn closed_chat_places_task_before_colored_time_without_state_words() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: implement sidebar",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        elapsed_secs: Some(127),
        tasks_done: Some(1),
        tasks_total: Some(3),
        subagent_count: Some(2),
        ..Default::default()
    });

    let parts = closed_chat_right_parts(&chat);

    assert_eq!(
        parts
            .iter()
            .map(|part| part.text.as_str())
            .collect::<Vec<_>>(),
        vec!["☑ 1/3", "↳ 2", "2m07s"]
    );
    assert!(parts.iter().all(|part| {
        !["Running", "Idle", "Done", "Waiting"]
            .iter()
            .any(|state| part.text.contains(state))
    }));
}

#[test]
fn closed_and_expanded_summary_content_start_at_the_same_column() {
    let summary = "align this task";
    for depth in [0, 2] {
        for (active, selected) in [(false, false), (true, false), (false, true)] {
            let mut closed = row(
                "chat::%1::10",
                SidebarRowKind::Chat,
                depth,
                "codex: align this task",
                RollupLevel::Running,
            );
            closed.badge_state = Some(BadgeState::Working);
            closed.expanded = false;
            closed.active = active;
            closed.meta = Some(crate::sidebar::tree::RowMeta {
                agent: Some("codex".to_string()),
                prompt: Some("raw latest prompt".to_string()),
                task_summary: Some(summary.to_string()),
                ..Default::default()
            });
            let mut expanded = row(
                "detail::%1::10::summary",
                SidebarRowKind::Detail,
                depth + 1,
                summary,
                RollupLevel::Running,
            );
            expanded.active = active;
            let state = SidebarState {
                selection: selected.then(|| "chat::%1::10".to_string()),
                ..SidebarState::default()
            };

            let closed_line = line_to_string(
                render_lines(&[closed], &state, 64, &SidebarRenderTheme::default())[1].clone(),
            );
            let expanded_line = line_to_string(
                render_lines(&[expanded], &state, 64, &SidebarRenderTheme::default())[0].clone(),
            );

            let summary_column = |line: &str| {
                let byte_index = line.find(summary).expect("summary must be rendered");
                display_width(&line[..byte_index])
            };
            assert_eq!(
                summary_column(&closed_line),
                summary_column(&expanded_line),
                "depth={depth} active={active} selected={selected}"
            );
        }
    }
}

#[test]
fn closed_chat_task_progress_with_zero_done_uses_working_color() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: implement sidebar task colors",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        prompt: Some("implement sidebar task colors".to_string()),
        elapsed_secs: Some(42),
        tasks_done: Some(0),
        tasks_total: Some(3),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();

    let lines = render_lines(&[chat], &SidebarState::default(), 64, &theme);

    assert_span_fg(&lines[0].spans, "☑ 0/3", theme.task_working);
}

#[test]
fn closed_chat_selection_styles_both_digest_lines() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: review PR",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        prompt: Some("review PR".to_string()),
        task_summary: Some("review PR".to_string()),
        elapsed_secs: Some(522),
        ..Default::default()
    });
    let state = SidebarState {
        selection: Some("chat::%1".to_string()),
        ..SidebarState::default()
    };

    let lines = render_lines(&[chat], &state, 40, &SidebarRenderTheme::default());

    assert_eq!(lines.len(), 2);
    assert!(
        lines
            .iter()
            .all(|line| { line.style.bg == Some(SidebarRenderTheme::default().selection_bg) })
    );
    assert_eq!(line_to_string(lines[0].clone()).chars().next(), Some(' '));
    assert_eq!(line_to_string(lines[1].clone()).chars().next(), Some(' '));
    assert!(line_to_string(lines[0].clone()).ends_with("8m42s "));
    assert_span_fg(
        &lines[0].spans,
        "8m42s",
        SidebarRenderTheme::default().badge_color(BadgeState::Working),
    );
}

#[test]
fn closed_chat_completed_state_matches_expanded_state_appearance() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: review PR",
        RollupLevel::Idle,
        BadgeState::Done,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        prompt: Some("review PR".to_string()),
        completed_age_secs: Some(815),
        ..Default::default()
    });
    let theme = SidebarRenderTheme::default();

    let lines = render_lines(&[chat], &SidebarState::default(), 40, &theme);

    assert_span_fg(
        &lines[0].spans,
        "13m ago",
        theme.badge_color(BadgeState::Done),
    );
}

#[test]
fn standard_boundary_switches_closed_chat_from_dense_to_digest() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: review PR",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        prompt: Some("review PR".to_string()),
        task_summary: Some("review PR".to_string()),
        elapsed_secs: Some(720),
        ..Default::default()
    });

    assert_eq!(
        render_rows(&[chat.clone()], &SidebarState::default(), 35)
            .lines()
            .count(),
        1
    );
    assert_eq!(
        render_rows(&[chat], &SidebarState::default(), 36)
            .lines()
            .count(),
        2
    );
}

#[test]
fn closed_chat_digest_truncates_long_right_tokens_to_width() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: review very long sidebar prompt",
        RollupLevel::Permission,
        BadgeState::Blocked,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        prompt: Some("review very long sidebar prompt".to_string()),
        task_summary: Some("review very long sidebar prompt".to_string()),
        wait_reason: Some("very_long_custom_wait_reason".to_string()),
        elapsed_secs: Some(8 * 60 + 42),
        tasks_done: Some(123),
        tasks_total: Some(999),
        subagent_count: Some(42),
        ..Default::default()
    });

    let rendered = render_rows(&[chat], &SidebarState::default(), 36);

    assert_eq!(rendered.lines().count(), 2, "{rendered:?}");
    assert!(
        rendered.lines().all(|line| display_width(line) == 36),
        "{rendered:?}"
    );
    assert!(
        rendered.lines().nth(1).unwrap().contains('…'),
        "{rendered:?}"
    );
}

#[test]
fn chat_row_shows_elapsed_when_running() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: fix",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        elapsed_secs: Some(815),
        ..Default::default()
    });
    let rendered = render_rows(&[chat], &SidebarState::default(), 30);
    assert!(rendered.ends_with("13m "), "{rendered:?}");
}

#[test]
fn chat_row_shows_completed_age_when_done() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: fix",
        RollupLevel::Idle,
        BadgeState::Done,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        completed_age_secs: Some(815),
        ..Default::default()
    });

    assert_eq!(right_label(&chat).as_deref(), Some("13m ago"));
    assert_eq!(
        right_style(&chat, &SidebarRenderTheme::default()).fg,
        Some(SidebarRenderTheme::default().badge_color(BadgeState::Idle))
    );

    let rendered = render_rows(&[chat], &SidebarState::default(), 30);
    assert!(rendered.ends_with("13m ago "), "{rendered:?}");
    assert_eq!(
        display_width(rendered.lines().next().unwrap()),
        30,
        "{rendered:?}"
    );
}

#[test]
fn chat_row_shows_completed_age_when_idle() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: fix",
        RollupLevel::Idle,
        BadgeState::Idle,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        completed_age_secs: Some(815),
        ..Default::default()
    });

    assert_eq!(right_label(&chat).as_deref(), Some("13m ago"));
    assert_eq!(
        right_style(&chat, &SidebarRenderTheme::default()).fg,
        Some(SidebarRenderTheme::default().badge_color(BadgeState::Idle))
    );

    let rendered = render_rows(&[chat], &SidebarState::default(), 30);
    assert!(rendered.ends_with("13m ago "), "{rendered:?}");
}

#[test]
fn expanded_chat_row_uses_full_elapsed_right_label() {
    let mut chat = chat_row(
        "chat::%1",
        "codex: fix",
        RollupLevel::Running,
        BadgeState::Working,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        elapsed_secs: Some(780),
        ..Default::default()
    });

    assert_eq!(right_label(&chat).as_deref(), Some("13m"));

    chat.expanded = true;

    assert_eq!(right_label(&chat).as_deref(), Some("13m 00s"));
    assert_eq!(
        right_style(&chat, &SidebarRenderTheme::default()).fg,
        Some(SidebarRenderTheme::default().badge_color(BadgeState::Working))
    );
}

#[test]
fn long_cjk_summary_is_truncated_with_ellipsis_keeping_right_column() {
    let mut chat = chat_row(
        "chat::%1",
        "codex",
        RollupLevel::Permission,
        BadgeState::Blocked,
    );
    chat.expanded = false;
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        agent: Some("codex".to_string()),
        task_summary: Some("日本語のとても長いタスク要約を表示する".to_string()),
        elapsed_secs: Some(13 * 60),
        ..Default::default()
    });
    let rendered = render_rows(&[chat], &SidebarState::default(), 24);
    assert!(rendered.contains('…'), "{rendered:?}");
    assert!(rendered.ends_with("13m "), "{rendered:?}");
    assert_eq!(display_width(&rendered), 24, "{rendered:?}");
}

#[test]
fn badge_glyph_is_rendered_in_badge_color_span() {
    let chat = chat_row(
        "chat::%1",
        "codex",
        RollupLevel::Running,
        BadgeState::Working,
    );
    let lines = render_lines(
        &[chat],
        &SidebarState::default(),
        40,
        &SidebarRenderTheme::default(),
    );
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.content.contains('●') && span.style.fg == Some(Color::Green)),
        "{lines:?}"
    );
}

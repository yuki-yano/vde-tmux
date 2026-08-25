use super::*;
use crate::sidebar::render::text::line_to_string;

#[test]
fn axis_segments_use_header_mode_color_and_glyphs() {
    let theme = SidebarRenderTheme::default();
    let state = SidebarState {
        category_scope: CategoryScope::Current,
        presentation_mode: PresentationMode::Tree,
        ..SidebarState::default()
    };

    let mode_style = mode_segment_style(&theme);
    assert_eq!(mode_style.fg, Some(Color::Indexed(16)));
    assert_eq!(mode_style.bg, Some(Color::Indexed(147)));
    assert!(mode_style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(
        build_header_layout(&state, 80).lines[1].text,
        " ◉ Current · ≣ Tree     ▾ \u{e0b0}".to_string()
    );
}

#[test]
fn reset_header_background_keeps_mode_and_section_foreground_only() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
sidebar:
  header:
    prefix: ""
    suffix: ""
    bold: true
    colors:
      fg: "#b4befe"
      bg: reset
  colors:
    category: "#cba6f7"
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
    let header =
        build_header_layout_with_counts(&SidebarState::default(), 80, &theme, rich_header_counts());

    let section = style_for_segment(&header, 0, "SIDEBAR");
    assert_eq!(section.fg, Some(Color::Rgb(0xcb, 0xa6, 0xf7)));
    assert_eq!(section.bg, None);
    assert!(section.add_modifier.contains(Modifier::BOLD));

    let mode = style_for_segment(&header, 1, "≣ Tree");
    assert_eq!(mode.fg, Some(Color::Rgb(0xb4, 0xbe, 0xfe)));
    assert_eq!(mode.bg, None);
    assert!(mode.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn header_filter_positions_are_stable_across_view_axes() {
    let text_for = |presentation_mode: PresentationMode| {
        let state = SidebarState {
            presentation_mode,
            ..SidebarState::default()
        };
        build_header_layout(&state, 80).lines[2].text.clone()
    };
    let tree = text_for(PresentationMode::Tree);
    let flat = text_for(PresentationMode::Flat);
    let priority = text_for(PresentationMode::Priority);

    assert_eq!(flat.find('≡'), tree.find('≡'), "{flat:?} vs {tree:?}");
    assert_eq!(display_width(&flat), display_width(&tree));
    assert_eq!(display_width(&tree), display_width(&priority));
    assert_eq!(tree.find('≡'), priority.find('≡'));
}

fn rich_header_counts() -> BadgeCounts {
    BadgeCounts {
        total: 7,
        blocked: 1,
        limited: 0,
        working: 1,
        done: 0,
        idle: 5,
    }
}

fn segment_text(line: &HeaderLine, segment: &HeaderSegment) -> String {
    slice_display(&line.text, segment.range.start, segment.range.end)
}

fn style_for_segment(layout: &HeaderLayout, row: usize, needle: &str) -> Style {
    let line = &layout.lines[row];
    line.segments
        .iter()
        .find(|segment| segment_text(line, segment).contains(needle))
        .unwrap_or_else(|| panic!("segment {needle:?} not found in {:?}", line.segments))
        .style
        .expect("segment style")
}

fn style_after_segment(layout: &HeaderLayout, row: usize, needle: &str) -> Style {
    let line = &layout.lines[row];
    let index = line
        .segments
        .iter()
        .position(|segment| segment_text(line, segment) == needle)
        .unwrap_or_else(|| panic!("segment {needle:?} not found in {:?}", line.segments));
    line.segments[index + 1]
        .style
        .expect("following segment style")
}

#[test]
fn header_layout_uses_powerline_title_and_filter_chip_rows() {
    let state = SidebarState {
        category_scope: CategoryScope::All,
        presentation_mode: PresentationMode::Tree,
        filter: StatusFilter::All,
        ..SidebarState::default()
    };

    let header = build_header_layout_with_counts(
        &state,
        80,
        &SidebarRenderTheme::default(),
        rich_header_counts(),
    );

    assert_eq!(header.row_count(), 3);
    assert_eq!(header.lines[0].text, " SIDEBAR");
    assert_eq!(
        header.lines[1].text,
        " ◉ All     · ≣ Tree     ▾ \u{e0b0}".to_string()
    );
    assert_eq!(header.lines[2].text, " ≡ 7  ▲ 1  ⋄ 0  ● 1  ✓ 0  ○ 5 ");
    let section = style_for_segment(&header, 0, "SIDEBAR");
    assert_eq!(section.fg, Some(SidebarRenderTheme::default().category));
    assert!(section.add_modifier.contains(Modifier::BOLD));
    assert!(!header.lines[1].text.contains("tasks"));
}

#[test]
fn header_title_leaves_counts_to_the_filter_chip_row() {
    let counts = BadgeCounts {
        total: 1,
        idle: 1,
        ..BadgeCounts::default()
    };

    let header = build_header_layout_with_counts(
        &SidebarState::default(),
        80,
        &SidebarRenderTheme::default(),
        counts,
    );

    assert!(!header.lines[1].text.contains('1'));
    assert!(header.lines[2].text.contains("≡ 1"));
}

#[test]
fn header_hit_test_targets_modes_and_available_filter_chips() {
    let state = SidebarState {
        category_scope: CategoryScope::All,
        presentation_mode: PresentationMode::Tree,
        filter: StatusFilter::All,
        ..SidebarState::default()
    };

    let header = build_header_layout_with_counts(
        &state,
        80,
        &SidebarRenderTheme::default(),
        rich_header_counts(),
    );

    assert_eq!(header_hit_test(&header, 0, 2), None);
    assert_eq!(
        header_hit_test(&header, 1, 2),
        Some(HeaderAction::ToggleCategoryScope)
    );
    assert_eq!(
        header_hit_test(&header, 1, 18),
        Some(HeaderAction::CyclePresentationMode)
    );
    assert_eq!(
        header_hit_test(&header, 2, 1),
        Some(HeaderAction::SetFilter(StatusFilter::All))
    );
    assert_eq!(
        header_hit_test(&header, 2, 6),
        Some(HeaderAction::SetFilter(StatusFilter::AttentionOnly))
    );
    assert_eq!(
        header_hit_test(&header, 2, 16),
        Some(HeaderAction::SetFilter(StatusFilter::WorkingOnly))
    );
    assert_eq!(header_hit_test(&header, 2, 11), None);
    assert_eq!(header_hit_test(&header, 2, 21), None);
    assert_eq!(
        header_hit_test(&header, 2, 26),
        Some(HeaderAction::SetFilter(StatusFilter::IdleOnly))
    );
}

#[test]
fn unread_done_does_not_render_the_red_attention_chip() {
    let state = SidebarState {
        category_scope: CategoryScope::All,
        presentation_mode: PresentationMode::Tree,
        filter: StatusFilter::All,
        ..SidebarState::default()
    };
    let counts = BadgeCounts {
        total: 1,
        blocked: 0,
        done: 1,
        ..BadgeCounts::default()
    };

    let header =
        build_header_layout_with_counts(&state, 80, &SidebarRenderTheme::default(), counts);

    assert!(
        header.lines[2].text.contains("▲ 0"),
        "{:?}",
        header.lines[2].text
    );
    assert!(
        header.lines[2].text.contains("✓ 1"),
        "{:?}",
        header.lines[2].text
    );
}

#[test]
fn active_chip_fg_follows_configured_header_fg() {
    let theme = SidebarRenderTheme {
        header_active_fg: Some(Color::Rgb(0x19, 0x16, 0x27)),
        ..SidebarRenderTheme::default()
    };
    let counts = BadgeCounts {
        total: 3,
        blocked: 1,
        limited: 0,
        working: 1,
        done: 0,
        idle: 1,
    };
    let state = SidebarState {
        filter: StatusFilter::AttentionOnly,
        ..SidebarState::default()
    };

    let header = build_header_layout_with_counts(&state, 80, &theme, counts);

    let badge = style_for_segment(&header, 2, "▲");
    assert_eq!(badge.fg, Some(theme.badge_blocked));
    assert_eq!(badge.bg, Some(theme.header_mode));
    let active_text = style_after_segment(&header, 2, "▲");
    assert_eq!(active_text.fg, Some(Color::Rgb(0x19, 0x16, 0x27)));
    assert_eq!(active_text.bg, Some(theme.header_mode));
    assert!(!active_text.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn header_chip_fg_overrides_active_chip_fg_but_not_mode_fg() {
    let theme = SidebarRenderTheme {
        header_active_fg: Some(Color::Rgb(0x98, 0xb2, 0xf6)),
        header_chip_fg: Some(Color::Rgb(0x23, 0x23, 0x32)),
        ..SidebarRenderTheme::default()
    };
    let counts = BadgeCounts {
        total: 3,
        blocked: 1,
        limited: 0,
        working: 1,
        done: 0,
        idle: 1,
    };
    let state = SidebarState {
        filter: StatusFilter::AttentionOnly,
        ..SidebarState::default()
    };

    let header = build_header_layout_with_counts(&state, 80, &theme, counts);

    let badge = style_for_segment(&header, 2, "▲");
    assert_eq!(badge.fg, Some(theme.badge_blocked));
    assert_eq!(badge.bg, Some(theme.header_mode));
    let active_text = style_after_segment(&header, 2, "▲");
    assert_eq!(active_text.fg, Some(Color::Rgb(0x23, 0x23, 0x32)));
    assert_eq!(active_text.bg, Some(theme.header_mode));
    let mode = style_for_segment(&header, 1, "≣");
    assert_eq!(mode.fg, Some(Color::Rgb(0x98, 0xb2, 0xf6)));
}

#[test]
fn active_all_chip_bg_follows_configured_filter_bg() {
    let theme = SidebarRenderTheme {
        header_active_bg: Some(Color::Rgb(0x45, 0x3f, 0x9e)),
        header_filter_bg: Some(Color::Rgb(0xee, 0xee, 0xf4)),
        ..SidebarRenderTheme::default()
    };
    let state = SidebarState {
        filter: StatusFilter::All,
        ..SidebarState::default()
    };

    let header = build_header_layout_with_counts(&state, 80, &theme, rich_header_counts());

    let active = style_for_segment(&header, 2, "≡ 7");
    assert_eq!(active.bg, Some(Color::Rgb(0xee, 0xee, 0xf4)));
}

#[test]
fn header_chips_use_configured_badge_glyphs() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
badge:
  glyphs:
    working: "W"
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_app_config(&config);
    let state = SidebarState::default();

    let header = build_header_layout_with_counts(&state, 80, &theme, rich_header_counts());

    assert!(
        header.lines[2].text.contains("W 1"),
        "{:?}",
        header.lines[2].text
    );
    assert!(
        !header.lines[2].text.contains("● 1"),
        "{:?}",
        header.lines[2].text
    );
}

#[test]
fn custom_header_suffix_is_rendered_after_mode_segment() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
sidebar:
  header:
    suffix: ""
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);

    let header =
        build_header_layout_with_counts(&SidebarState::default(), 80, &theme, rich_header_counts());

    assert!(
        header.lines[1].text.ends_with("▾ "),
        "{:?}",
        header.lines[1].text
    );
}

#[test]
fn chip_caps_render_as_pill_and_skip_zero_chips() {
    let theme = SidebarRenderTheme {
        header_chip_prefix: "\u{e0b6}".to_string(),
        header_chip_suffix: "\u{e0b4}".to_string(),
        ..SidebarRenderTheme::default()
    };
    let counts = BadgeCounts {
        total: 3,
        blocked: 1,
        limited: 0,
        working: 0,
        done: 0,
        idle: 2,
    };
    let state = SidebarState::default();

    let header = build_header_layout_with_counts(&state, 80, &theme, counts);
    let line = &header.lines[2];

    assert_eq!(
        line.text,
        "\u{e0b6} ≡ 3 \u{e0b4} \u{e0b6} ▲ 1 \u{e0b4}  ⋄ 0   ● 0   ✓ 0  \u{e0b6} ○ 2 \u{e0b4}"
    );
    let cap = style_for_segment(&header, 2, "\u{e0b6}");
    assert_eq!(cap.fg, Some(theme.header_mode));
    assert_eq!(
        header_hit_test(&header, 2, 0),
        Some(HeaderAction::SetFilter(StatusFilter::All))
    );
    let zero_badge = style_for_segment(&header, 2, "●");
    assert_eq!(zero_badge.fg, Some(theme.badge_working));
    assert_eq!(zero_badge.bg, None);
    assert!(zero_badge.add_modifier.contains(Modifier::DIM));
    let zero_count = style_after_segment(&header, 2, "●");
    assert_eq!(zero_count.fg, Some(theme.detail));
    assert_eq!(zero_count.bg, None);
    assert!(zero_count.add_modifier.contains(Modifier::DIM));
}

#[test]
fn header_chip_styles_distinguish_active_nonzero_and_zero_states() {
    let theme = SidebarRenderTheme::default();
    let counts = BadgeCounts {
        total: 7,
        blocked: 0,
        limited: 0,
        working: 2,
        done: 0,
        idle: 5,
    };
    let state = SidebarState {
        category_scope: CategoryScope::All,
        presentation_mode: PresentationMode::Tree,
        filter: StatusFilter::AttentionOnly,
        ..SidebarState::default()
    };

    let header = build_header_layout_with_counts(&state, 80, &theme, counts);

    let mode = style_for_segment(&header, 1, "≣ Tree");
    assert_eq!(mode.fg, Some(Color::Indexed(16)));
    assert_eq!(mode.bg, Some(theme.header_mode));
    assert!(mode.add_modifier.contains(Modifier::BOLD));

    let active_badge = style_for_segment(&header, 2, "▲");
    assert_eq!(active_badge.fg, Some(theme.badge_blocked));
    assert_eq!(active_badge.bg, Some(theme.header_mode));
    assert!(active_badge.add_modifier.contains(Modifier::BOLD));
    let active_count = style_after_segment(&header, 2, "▲");
    assert_eq!(active_count.fg, Some(Color::Indexed(16)));
    assert_eq!(active_count.bg, Some(theme.header_mode));
    assert!(active_count.add_modifier.contains(Modifier::BOLD));

    let working = style_for_segment(&header, 2, "●");
    assert_eq!(working.fg, Some(theme.badge_working));
    assert_eq!(working.bg, Some(theme.active_bg));

    let done_badge = style_for_segment(&header, 2, "✓");
    assert_eq!(done_badge.fg, Some(theme.badge_done));
    assert_eq!(done_badge.bg, None);
    assert!(done_badge.add_modifier.contains(Modifier::DIM));
    let done_count = style_after_segment(&header, 2, "✓");
    assert_eq!(done_count.fg, Some(theme.detail));
    assert_eq!(done_count.bg, None);
    assert!(done_count.add_modifier.contains(Modifier::DIM));
    assert!(!done_count.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn header_suffix_can_remove_powerline_arrow() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
sidebar:
  header:
    suffix: ""
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
    let header =
        build_header_layout_with_counts(&SidebarState::default(), 80, &theme, rich_header_counts());

    assert_eq!(theme.header_suffix, "");
    assert!(!header.lines[1].text.contains('\u{e0b0}'));
    assert_eq!(header.lines[1].text, " ◉ Current · ≣ Tree     ▾ ");
}

#[test]
fn header_width_truncates_mode_without_rendering_duplicate_total() {
    let state = SidebarState {
        category_scope: CategoryScope::All,
        presentation_mode: PresentationMode::Tree,
        ..SidebarState::default()
    };

    let compact = build_header_layout_with_counts(
        &state,
        12,
        &SidebarRenderTheme::default(),
        rich_header_counts(),
    );
    assert_eq!(compact.lines[1].text, " ◉ All     …");
    assert!(!compact.lines[1].text.contains("tasks"));

    let narrow = build_header_layout_with_counts(
        &state,
        6,
        &SidebarRenderTheme::default(),
        rich_header_counts(),
    );
    assert!(display_width(&narrow.lines[1].text) <= 6);
    assert!(
        narrow.lines[1].text.ends_with('…'),
        "{:?}",
        narrow.lines[1].text
    );
}

#[test]
fn footer_documents_vim_navigation_keys() {
    let footer = line_to_string(build_footer_line(64));

    assert!(footer.contains("gg/G ends"), "{footer:?}");
    assert!(footer.contains("C-d/u half"), "{footer:?}");
    assert!(footer.contains("C-f/b page"), "{footer:?}");
}

#[test]
fn header_mode_badge_style_can_be_configured() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
sidebar:
  header:
    format: " {label} "
    prefix: "["
    suffix: "]"
    bold: true
    colors:
      fg: white
      bg: "24"
      outer_bg: "235"
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
    let state = SidebarState::default();

    let header = build_header_layout_with_counts(&state, 80, &theme, rich_header_counts());
    let lines = render_header_lines(&header, &theme);
    let mode = style_for_segment(&header, 1, "≣ Tree");
    let suffix = style_for_segment(&header, 1, "]");

    assert_eq!(header.lines[1].text, "[ ◉ Current · ≣ Tree     ]");
    assert_eq!(mode.fg, Some(Color::White));
    assert_eq!(mode.bg, Some(Color::Indexed(24)));
    assert!(mode.add_modifier.contains(Modifier::BOLD));
    assert_eq!(suffix.fg, Some(Color::Indexed(24)));
    assert_eq!(suffix.bg, Some(Color::Indexed(235)));
    assert_eq!(
        lines[1].spans[0].style,
        Style::default().fg(Color::Indexed(24))
    );
    assert_eq!(lines[1].spans[1].style, mode);
}

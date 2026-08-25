use super::*;

#[test]
fn parse_color_accepts_valid_hex() {
    assert_eq!(parse_color(Some("#ff8800")), Some(Color::Rgb(255, 136, 0)));
}

#[test]
fn parse_color_rejects_non_ascii_six_byte_hex_without_panicking() {
    // "#あXYZ" is 6 bytes but only 4 chars; byte-index slicing would panic.
    assert_eq!(parse_color(Some("#\u{3042}XYZ")), None);
    // Valid byte length but non-hex ASCII stays None as before.
    assert_eq!(parse_color(Some("#gggggg")), None);
}

#[test]
fn branch_defaults_to_muted_cyan() {
    assert_eq!(SidebarRenderTheme::default().branch, Color::Indexed(73));
}

#[test]
fn selection_and_active_colors_are_configurable() {
    let config = crate::config::SidebarColorsConfig {
        selection_bar: Some("#f2d98f".to_string()),
        active_bg: Some("235".to_string()),
        active_bar: Some("magenta".to_string()),
        ..Default::default()
    };
    let theme = SidebarRenderTheme::from_config(&config);

    assert_eq!(theme.selection_bar, Color::Rgb(0xf2, 0xd9, 0x8f));
    assert_eq!(theme.active_bg, Color::Indexed(235));
    assert_eq!(theme.active_bar, Color::Magenta);
    assert_eq!(SidebarRenderTheme::default().active_bg, Color::Indexed(235));
    assert_eq!(
        SidebarRenderTheme::default().active_bar,
        Color::Indexed(147)
    );
    assert_eq!(
        SidebarRenderTheme::default().selection_bar,
        Color::Indexed(229)
    );
}

#[test]
fn sidebar_render_theme_reads_git_task_subagent_and_worktree_detail_colors() {
    let config = crate::config::SidebarColorsConfig {
        git_ahead: Some("109".to_string()),
        git_behind: Some("180".to_string()),
        git_insertions: Some("79".to_string()),
        git_deletions: Some("#d98b8b".to_string()),
        task_done: Some("220".to_string()),
        task_working: Some("221".to_string()),
        task_pending: Some("darkgray".to_string()),
        task_label: Some("246".to_string()),
        subagent_label: Some("73".to_string()),
        subagent_id: Some("74".to_string()),
        worktree: Some("cyan".to_string()),
        worktree_activity: Some("#4fd08a".to_string()),
        ..Default::default()
    };
    let theme = SidebarRenderTheme::from_config(&config);

    assert_eq!(theme.git_ahead, Color::Indexed(109));
    assert_eq!(theme.git_behind, Color::Indexed(180));
    assert_eq!(theme.git_insertions, Color::Indexed(79));
    assert_eq!(theme.git_deletions, Color::Rgb(217, 139, 139));
    assert_eq!(theme.task_done, Color::Indexed(220));
    assert_eq!(theme.task_working, Color::Indexed(221));
    assert_eq!(theme.task_pending, Color::DarkGray);
    assert_eq!(theme.task_label, Color::Indexed(246));
    assert_eq!(theme.subagent_label, Color::Indexed(73));
    assert_eq!(theme.subagent_id, Color::Indexed(74));
    assert_eq!(theme.worktree, Color::Cyan);
    assert_eq!(theme.worktree_activity, Color::Rgb(79, 208, 138));

    let default = SidebarRenderTheme::default();
    assert_eq!(default.git_ahead, Color::Indexed(108));
    assert_eq!(default.git_behind, Color::Indexed(179));
    assert_eq!(default.git_insertions, Color::Indexed(78));
    assert_eq!(default.git_deletions, Color::Indexed(174));
    assert_eq!(default.task_done, Color::Indexed(220));
    assert_eq!(default.task_working, Color::Indexed(220));
    assert_eq!(default.task_pending, Color::DarkGray);
    assert_eq!(default.task_label, Color::Indexed(246));
    assert_eq!(default.subagent_label, Color::Indexed(73));
    assert_eq!(default.subagent_id, Color::Indexed(73));
    assert_eq!(default.worktree, Color::Indexed(73));
    assert_eq!(default.worktree_activity, Color::Indexed(73));
}

#[test]
fn theme_maps_badge_states_to_default_colors() {
    let theme = SidebarRenderTheme::default();
    assert_eq!(theme.badge_color(BadgeState::Blocked), Color::Red);
    assert_eq!(
        theme.badge_color(BadgeState::Limited),
        Color::Rgb(0xf5, 0xa7, 0x42)
    );
    assert_eq!(theme.badge_color(BadgeState::Working), Color::Green);
    assert_eq!(theme.badge_color(BadgeState::Done), Color::Cyan);
    assert_eq!(theme.badge_color(BadgeState::Idle), Color::Indexed(248));
}

#[test]
fn sidebar_badge_colors_use_shared_badge_colors() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
badge:
  colors:
    working: "#57d98a"
    done: "#5aa6ff"
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_app_config(&config);
    assert_eq!(
        theme.badge_color(BadgeState::Working),
        Color::Rgb(0x57, 0xd9, 0x8a)
    );
    assert_eq!(
        theme.badge_color(BadgeState::Done),
        Color::Rgb(0x5a, 0xa6, 0xff)
    );
    assert_eq!(
        theme.badge_color(BadgeState::Blocked),
        Color::Rgb(0xff, 0x6b, 0x6b)
    );
    assert_eq!(
        theme.badge_color(BadgeState::Idle),
        Color::Rgb(0xa8, 0xa8, 0xb2)
    );
}

#[test]
fn sidebar_colors_badge_overrides_take_precedence_over_badge_colors() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
badge:
  colors:
    working: "#57d98a"
    idle: "#c6c3d8"
sidebar:
  colors:
    badge_idle: "#8b88a0"
    badge_done: "#4d7fc4"
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_app_config(&config);
    assert_eq!(
        theme.badge_color(BadgeState::Idle),
        Color::Rgb(0x8b, 0x88, 0xa0)
    );
    assert_eq!(
        theme.badge_color(BadgeState::Done),
        Color::Rgb(0x4d, 0x7f, 0xc4)
    );
    assert_eq!(
        theme.badge_color(BadgeState::Working),
        Color::Rgb(0x57, 0xd9, 0x8a)
    );
    assert_eq!(
        theme.badge_color(BadgeState::Blocked),
        Color::Rgb(0xff, 0x6b, 0x6b)
    );
}

#[test]
fn sidebar_colors_badge_overrides_apply_without_app_config() {
    let config = serde_yaml_ng::from_str::<crate::config::SidebarColorsConfig>(
        r##"
badge_working: "#3fae7a"
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_config(&config);
    assert_eq!(
        theme.badge_color(BadgeState::Working),
        Color::Rgb(0x3f, 0xae, 0x7a)
    );
    assert_eq!(theme.badge_color(BadgeState::Idle), Color::Indexed(248));
}

#[test]
fn sidebar_rollup_colors_use_shared_badge_colors() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
badge:
  colors:
    limited: "#f5a742"
    blocked: "#ff1111"
    working: "#22ff22"
    idle: "#999999"
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_app_config(&config);

    assert_eq!(
        theme.rollup_color(RollupLevel::Limited),
        Color::Rgb(0xf5, 0xa7, 0x42)
    );
    assert_eq!(
        theme.rollup_color(RollupLevel::Running),
        Color::Rgb(0x22, 0xff, 0x22)
    );
    assert_eq!(
        theme.rollup_color(RollupLevel::Permission),
        Color::Rgb(0xff, 0x11, 0x11)
    );
    assert_eq!(
        theme.rollup_color(RollupLevel::Waiting),
        Color::Rgb(0xff, 0x11, 0x11)
    );
    assert_eq!(
        theme.rollup_color(RollupLevel::Error),
        Color::Rgb(0xff, 0x11, 0x11)
    );
    assert_eq!(
        theme.rollup_color(RollupLevel::Background),
        Color::Rgb(0x99, 0x99, 0x99)
    );
    assert_eq!(
        theme.rollup_color(RollupLevel::Idle),
        Color::Rgb(0x99, 0x99, 0x99)
    );
}

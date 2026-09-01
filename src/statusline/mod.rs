mod navigation;
mod render;
mod targets;

pub use navigation::{
    cycle_statusline_category, cycle_statusline_session, handle_statusline_click,
    switch_statusline_category, switch_statusline_session, switch_statusline_window,
};
pub use render::{
    StructuredStatusSegments, render_attention_segment, render_structured_pane_status,
    render_structured_status_snapshot,
};
pub(crate) use render::{sessions_display_width, structured_status_display_width};
// STATUS_OPTION_CELL_BUDGET and the target key builders are consumed only through
// `crate::statusline::` paths inside cfg(test) code, so the re-exports look unused
// to a production build.
#[allow(unused_imports)]
pub(crate) use render::STATUS_OPTION_CELL_BUDGET;
pub(crate) use targets::{ATTENTION_RANGE_PREFIX, resolve_attention_target};
#[allow(unused_imports)]
pub(crate) use targets::{attention_target_key, category_target_key};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BadgeStyle, Config, SessionBadgeMode};
    use crate::daemon::protocol::v2::AttentionEntry;
    use crate::pane_state::PaneInstance;
    use crate::tmux::mock::MockTmuxRunner;

    use crate::config::{FixedWidthAlignment, SegmentStyle};
    use crate::daemon::protocol::v2::{
        CategoryStatusPresentation, PanePresentation, SessionStatusPresentation, StatusContext,
        StatusSnapshot, WindowStatusPresentation,
    };
    use crate::daemon::session_badge::{BadgeState, BadgeStateCounts};
    use crate::session::Direction;

    use super::navigation::{
        cycle_statusline_category_with_snapshot, displayed_category_targets, top_level_user_ranges,
    };
    use super::render::{
        STATUS_NOW_FORMAT_OPTION, SessionBadgeRenderOptions, StatusToken, pad_session_zone,
        pane_border_highlight_color, render_structured_attention,
        render_structured_session_segment, render_structured_sessions, render_structured_summary,
        status_projection_width, structured_category_tokens, structured_pane_status_label,
        tmux_bounded_duration, tmux_display_width,
    };
    use super::targets::{
        CATEGORY_RANGE_PREFIX, CURRENT_CATEGORY_RANGE_PREFIX, TMUX_USER_RANGE_NAME_MAX_BYTES,
        validate_category_target,
    };

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
                limited: 0,
                working: 1,
                done: 1,
                idle: 1,
            },
        }
    }

    fn plain_status_session(id: &str, name: &str, active: bool) -> SessionStatusPresentation {
        SessionStatusPresentation {
            session_id: id.to_string(),
            session_name: name.to_string(),
            category: Some("work".to_string()),
            attached: None,
            created_at: None,
            active,
            counts: BadgeStateCounts::default(),
        }
    }

    fn status_snapshot() -> StatusSnapshot {
        StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Global,
            summary: BadgeStateCounts::default(),
            session_zone_width: None,
            sessions: Vec::new(),
            windows: Vec::new(),
            categories: Vec::new(),
            attention: Vec::new(),
        }
    }

    fn category_navigation_snapshot(assignments: &[(&str, &str, &str)]) -> StatusSnapshot {
        let mut snapshot = status_snapshot();
        for (session_id, session_name, category) in assignments {
            snapshot.sessions.push(SessionStatusPresentation {
                session_id: (*session_id).to_string(),
                session_name: (*session_name).to_string(),
                category: Some((*category).to_string()),
                attached: None,
                created_at: None,
                active: false,
                counts: BadgeStateCounts::default(),
            });
            if let Some(existing) = snapshot
                .categories
                .iter_mut()
                .find(|existing| existing.category == *category)
            {
                existing.session_ids.push((*session_id).to_string());
            } else {
                snapshot.categories.push(CategoryStatusPresentation {
                    category: (*category).to_string(),
                    session_ids: vec![(*session_id).to_string()],
                    active: false,
                    counts: BadgeStateCounts::default(),
                });
            }
        }
        snapshot
    }

    fn status_window(id: &str, name: &str, index: i64, active: bool) -> WindowStatusPresentation {
        WindowStatusPresentation {
            window_id: id.to_string(),
            window_name: name.to_string(),
            pane_count: 1,
            session_ids: vec!["$1".to_string()],
            window_index: Some(index),
            active,
            last: false,
            bell: None,
            activity: None,
            silence: None,
            current_command: None,
            counts: BadgeStateCounts::default(),
        }
    }

    fn status_category(name: &str, active: bool) -> CategoryStatusPresentation {
        CategoryStatusPresentation {
            category: name.to_string(),
            session_ids: vec!["$1".to_string()],
            active,
            counts: BadgeStateCounts {
                idle: 1,
                ..BadgeStateCounts::default()
            },
        }
    }

    fn blocked_attention(
        pane_id: &str,
        pane_pid: u32,
        session_name: &str,
        elapsed_seconds: i64,
    ) -> AttentionEntry {
        AttentionEntry {
            pane_instance: PaneInstance {
                pane_id: pane_id.to_string(),
                pane_pid,
            },
            session_name: session_name.to_string(),
            badge: BadgeState::Blocked,
            reason: Some("permission_prompt".to_string()),
            elapsed_seconds,
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
                agent_process: None,
                agent_epoch: 1,
                agent_present: true,
                scan_verified: true,
                synthetic_completion_armed: false,
                lifecycle,
                run_seq: 1,
                current_run: None,
                completed_seq: 0,
                unread: crate::pane_state::UnreadState::default(),
                started_at: Some(60),
                completed_at: None,
                prompt: None,
                latest_response: None,
                task_context: crate::pane_state::TaskContextState::default(),
                tasks: crate::pane_state::TaskState::default(),
                subagents: Vec::new(),
                worktree_activity: None,
                background_process: None,
                listening_ports: Vec::new(),
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
            focused: active,
            agent_process: None,
            stored: None,
            resolved,
            retained_state: None,
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
                limited: 0,
                working: 2,
                done: 0,
                idle: 1,
            },
            session_zone_width: None,
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
                pane_count: 3,
                bell: Some(false),
                activity: Some(false),
                silence: Some(false),
                current_command: Some("nvim".to_string()),
                counts: BadgeStateCounts {
                    working: 1,
                    ..BadgeStateCounts::default()
                },
                ..status_window("@2", "editor", 2, true)
            }],
            categories: vec![CategoryStatusPresentation {
                counts: BadgeStateCounts {
                    done: 1,
                    ..BadgeStateCounts::default()
                },
                ..status_category("work", true)
            }],
            attention: vec![{
                let mut entry = blocked_attention("%7", 700, "main", 125);
                entry.reason = Some("PermissionPrompt".to_string());
                entry
            }],
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert_eq!(rendered.snapshot_revision, 41);
        assert!(rendered.summary.contains("▲ 1"), "{}", rendered.summary);
        assert!(rendered.summary.contains("● 2"), "{}", rendered.summary);
        assert!(rendered.summary.contains("○ 1"), "{}", rendered.summary);
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
                "range=user|C:{}",
                category_target_key("work").unwrap()
            )),
            "{}",
            rendered.category
        );
        assert!(
            rendered.attention.contains("▲ main"),
            "{}",
            rendered.attention
        );
        assert!(
            !rendered.attention.contains("perm"),
            "{}",
            rendered.attention
        );
        assert_eq!(
            top_level_user_ranges(&rendered.attention).unwrap(),
            vec![attention_target_key(&crate::pane_state::PaneInstance {
                pane_id: "%7".to_string(),
                pane_pid: 700,
            })]
        );
    }

    #[test]
    fn category_status_keeps_categories_without_agents() {
        let mut config = Config::default();
        config.statusline.category.format = "{category}".to_string();
        config.statusline.category.inactive_format = "{category}".to_string();
        let snapshot = StatusSnapshot {
            categories: vec![
                CategoryStatusPresentation {
                    category: crate::category::UNCATEGORIZED.to_string(),
                    session_ids: vec!["$1".to_string()],
                    active: false,
                    counts: BadgeStateCounts::default(),
                },
                status_category("work", true),
            ],
            ..status_snapshot()
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(
            rendered.category.contains(crate::category::UNCATEGORIZED),
            "{}",
            rendered.category
        );
        assert!(rendered.category.contains("work"), "{}", rendered.category);
        assert_eq!(top_level_user_ranges(&rendered.category).unwrap().len(), 2);
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
                current_command: Some("sh#[bg=red]\t{window}".to_string()),
                ..status_window("@1", "win#{command}", 4, true)
            }],
            ..status_snapshot()
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
    fn structured_pane_uses_resolved_badge_with_dynamic_elapsed_time() {
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

        let rendered = render_structured_pane_status(&config, &pane);

        assert!(rendered.contains("%%7|Codex|"), "{rendered}");
        assert!(rendered.contains("Waiting"), "{rendered}");
        assert!(rendered.matches(STATUS_NOW_FORMAT_OPTION).count() >= 2);
        assert!(rendered.matches(",60}").count() >= 2, "{rendered}");
        assert!(!rendered.contains("2m00s"), "{rendered}");
        assert!(
            rendered.contains(&config.badge.colors.blocked),
            "{rendered}"
        );
    }

    #[test]
    fn pane_border_highlight_uses_non_idle_badge_colors_only() {
        let mut config = Config::default();
        config.badge.colors.blocked = "ff0000".to_string();
        config.badge.colors.limited = "#f5a742".to_string();
        config.badge.colors.working = "#00ff00".to_string();
        config.badge.colors.done = "cyan".to_string();

        for (badge, expected) in [
            (BadgeState::Blocked, Some("#ff0000")),
            (BadgeState::Limited, Some("#f5a742")),
            (BadgeState::Working, Some("#00ff00")),
            (BadgeState::Done, Some("cyan")),
            (BadgeState::Idle, None),
        ] {
            let pane = structured_pane(
                "codex",
                "/tmp",
                false,
                Some((crate::pane_state::LifecycleState::Idle, badge)),
            );
            assert_eq!(
                pane_border_highlight_color(&config, &pane).as_deref(),
                expected
            );
            let rendered = render_structured_pane_status(&config, &pane);
            if let Some(color) = expected {
                assert!(rendered.contains(&format!("#[fg={color}]─")));
            } else {
                assert!(!rendered.contains('─'));
            }
        }

        let non_agent = structured_pane("zsh", "/tmp", false, None);
        assert_eq!(pane_border_highlight_color(&config, &non_agent), None);
    }

    #[test]
    fn dynamic_duration_embeds_epoch_and_old_display_thresholds() {
        let rendered = tmux_bounded_duration(123);

        assert!(rendered.contains(STATUS_NOW_FORMAT_OPTION), "{rendered}");
        assert!(rendered.contains(",123}"), "{rendered}");
        for threshold in [60, 600, 3_600, 86_400] {
            assert!(rendered.contains(&format!(",{threshold}}}")), "{rendered}");
        }
        assert!(rendered.contains("m"), "{rendered}");
        assert!(rendered.contains("s"), "{rendered}");
        assert!(rendered.contains("h"), "{rendered}");
        assert!(rendered.contains("d"), "{rendered}");
        assert!(!rendered.chars().any(char::is_control));
    }

    #[test]
    fn pane_state_labels_start_with_uppercase_letters() {
        let cases = [
            (
                crate::pane_state::LifecycleState::Running,
                BadgeState::Working,
                "Running",
            ),
            (
                crate::pane_state::LifecycleState::Waiting {
                    reason: crate::pane_state::WaitReason::PermissionPrompt,
                },
                BadgeState::Blocked,
                "Waiting",
            ),
            (
                crate::pane_state::LifecycleState::Waiting {
                    reason: crate::pane_state::WaitReason::usage_limit(),
                },
                BadgeState::Limited,
                "Limited",
            ),
            (
                crate::pane_state::LifecycleState::Error { reason: None },
                BadgeState::Blocked,
                "Error",
            ),
            (
                crate::pane_state::LifecycleState::Running,
                BadgeState::Done,
                "Done",
            ),
            (
                crate::pane_state::LifecycleState::Idle,
                BadgeState::Idle,
                "Idle",
            ),
        ];

        for (lifecycle, badge, expected) in cases {
            let pane = structured_pane("codex", "/tmp", true, Some((lifecycle, badge)));
            let resolved = pane.resolved.expect("resolved pane");
            assert_eq!(
                structured_pane_status_label(&resolved.canonical, badge),
                expected
            );
        }
    }

    #[test]
    fn structured_non_agent_pane_preserves_process_and_path_with_safe_text() {
        let mut config = Config::default();
        config.statusline.panes.other.format = "{process}|{path}|{detail}".to_string();
        let pane = structured_pane("zsh#[fg=red]\n{path}", "/tmp/#{process}\t", false, None);

        let rendered = render_structured_pane_status(&config, &pane);

        assert!(
            rendered.contains("zsh##[fg=red] {path}|/tmp/##{process} |zsh##[fg=red] {path}"),
            "{rendered}"
        );
    }

    #[test]
    fn tmux_intrinsic_width_counts_ascii_cjk_and_emoji_but_not_styles() {
        assert_eq!(tmux_display_width("#[fg=red]abc日本🚀#[default]"), 9);
        assert_eq!(tmux_display_width("##[literal]"), 10);
    }

    #[test]
    fn session_zone_padding_defaults_to_left_alignment() {
        assert_eq!(
            pad_session_zone("abc".to_string(), 7, FixedWidthAlignment::Left),
            "abc#[default]    "
        );
    }

    #[test]
    fn centered_session_zone_padding_is_balanced_and_puts_an_odd_cell_on_the_right() {
        assert_eq!(
            pad_session_zone("abc".to_string(), 7, FixedWidthAlignment::Center),
            "#[default]  abc#[default]  "
        );
        assert_eq!(
            pad_session_zone("abc".to_string(), 8, FixedWidthAlignment::Center),
            "#[default]  abc#[default]   "
        );
    }

    #[test]
    fn fixed_width_alignment_controls_rendered_session_position() {
        let sessions = vec![status_session("$1", "main", true)];
        let mut config = Config::default();
        config.statusline.sessions.fixed_width = true;
        let unpadded = render_structured_sessions(&config, &sessions);
        let snapshot = StatusSnapshot {
            context: StatusContext::Session {
                session_id: "$1".to_string(),
            },
            session_zone_width: Some(tmux_display_width(&unpadded) + 4),
            sessions,
            ..status_snapshot()
        };

        let left = render_structured_status_snapshot(&config, &snapshot).unwrap();
        assert_eq!(left.sessions, format!("{unpadded}#[default]    "));

        config.statusline.sessions.fixed_width_alignment = FixedWidthAlignment::Center;
        let centered = render_structured_status_snapshot(&config, &snapshot).unwrap();
        assert_eq!(
            centered.sessions,
            format!("#[default]  {unpadded}#[default]  ")
        );
    }

    #[test]
    fn global_status_context_does_not_pad_the_session_zone() {
        let mut config = Config::default();
        config.statusline.sessions.fixed_width = true;
        let sessions = vec![status_session("$1", "main", false)];
        let snapshot = StatusSnapshot {
            session_zone_width: Some(40),
            sessions: sessions.clone(),
            ..status_snapshot()
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert_eq!(
            rendered.sessions,
            render_structured_sessions(&config, &sessions)
        );
    }

    #[test]
    fn disabled_fixed_width_ignores_the_snapshot_session_zone_width() {
        let config = Config::default();
        let sessions = vec![status_session("$1", "main", true)];
        let snapshot = StatusSnapshot {
            context: StatusContext::Session {
                session_id: "$1".to_string(),
            },
            session_zone_width: Some(40),
            sessions: sessions.clone(),
            ..status_snapshot()
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert_eq!(
            rendered.sessions,
            render_structured_sessions(&config, &sessions)
        );
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
            let rendered = render_structured_pane_status(&config, &pane);
            assert!(
                rendered.contains("CUSTOM:(unnamed):(no agent):No state:(empty):(empty)"),
                "width {width}: {rendered}"
            );
        }
    }

    #[test]
    fn pane_border_does_not_infer_an_unresolved_state_as_idle() {
        let mut config = Config::default();
        config.statusline.panes.current.format =
            "{agent}|{badge}|{state}|{time}|{detail}".to_string();
        let pane = structured_pane("zsh", "/tmp", true, None);
        let rendered = render_structured_pane_status(&config, &pane);

        assert!(
            rendered.contains("(no agent)|—|No state|(empty)|zsh"),
            "{rendered}"
        );
        assert!(!rendered.contains("Idle"), "{rendered}");
    }

    #[test]
    fn session_and_category_options_keep_every_unicode_token() {
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
            .map(|index| {
                plain_status_session(
                    &format!("${}", index + 1),
                    &if index == 5 {
                        "現在🚀".to_string()
                    } else {
                        format!("日本語セッション{index}🚀")
                    },
                    index == 5,
                )
            })
            .collect::<Vec<_>>();
        let windows = (0..12)
            .map(|index| {
                status_window(
                    &format!("@{}", index + 1),
                    &if index == 7 {
                        "現在の窓🪟".to_string()
                    } else {
                        format!("編集ウィンドウ{index}🪟")
                    },
                    index,
                    index == 7,
                )
            })
            .collect::<Vec<_>>();
        let categories = (0..12)
            .map(|index| CategoryStatusPresentation {
                session_ids: vec![format!("${}", index + 1)],
                ..status_category(
                    &if index == 4 {
                        "現在カテゴリ🚀".to_string()
                    } else {
                        format!("カテゴリ{index}🚀")
                    },
                    index == 4,
                )
            })
            .collect::<Vec<_>>();
        let snapshot = StatusSnapshot {
            snapshot_revision: 1,
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$6".to_string(),
            },
            summary: BadgeStateCounts::default(),
            session_zone_width: None,
            sessions,
            windows,
            categories,
            attention: vec![blocked_attention("%9", 900, "要確認", 5_400)],
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();
        assert!(tmux_display_width(&rendered.sessions) > 80);
        assert_eq!(top_level_user_ranges(&rendered.sessions).unwrap().len(), 12);
        assert!(!rendered.sessions.contains("+"), "{}", rendered.sessions);
        assert!(tmux_display_width(&rendered.category) > 80);
        assert_eq!(top_level_user_ranges(&rendered.category).unwrap().len(), 12);
        assert!(!rendered.category.contains("+"), "{}", rendered.category);
        assert!(!rendered.summary.is_empty(), "{rendered:?}");
        assert!(tmux_display_width(&rendered.windows) <= 80);
        assert!(rendered.windows.contains("+"), "{}", rendered.windows);
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
            rendered.category.contains("現在カテゴリ🚀"),
            "{}",
            rendered.category
        );
        assert!(rendered.category.contains("range=user|C:"));
        assert!(rendered.attention.contains('▲'), "{}", rendered.attention);
        assert!(rendered.sessions.contains("日本語セッション0🚀"));
        assert!(rendered.category.contains("カテゴリ0🚀"));
    }

    #[test]
    fn summary_default_format_and_custom_format_preserve_token_separator() {
        let mut config = Config::default();
        let counts = BadgeStateCounts {
            blocked: 1,
            limited: 1,
            working: 1,
            done: 1,
            idle: 1,
        };

        let default_summary = render_structured_summary(&config, counts);
        assert_eq!(
            default_summary,
            "#[fg=#ff6b6b]▲ 1#[default] #[fg=#f5a742]⋄ 1#[default] #[fg=#4fd08a]● 1#[default] #[fg=#45cbe6]✓ 1#[default] #[fg=#a8a8b2]○ 1#[default]"
        );
        assert_eq!(tmux_display_width(&default_summary), 19);

        config.statusline.summary.format = "{count}{badge}".to_string();
        let custom_summary = render_structured_summary(&config, counts);
        assert_eq!(
            custom_summary,
            "#[fg=#ff6b6b]1▲#[default] #[fg=#f5a742]1⋄#[default] #[fg=#4fd08a]1●#[default] #[fg=#45cbe6]1✓#[default] #[fg=#a8a8b2]1○#[default]"
        );
        assert_eq!(tmux_display_width(&custom_summary), 14);
    }

    #[test]
    fn summary_includes_zero_states_and_keeps_single_digit_width_stable() {
        let mut config = Config::default();
        let empty_summary = render_structured_summary(&config, BadgeStateCounts::default());
        assert_eq!(
            empty_summary,
            "#[fg=#ff6b6b,dim]▲ 0#[default] #[fg=#f5a742,dim]⋄ 0#[default] #[fg=#4fd08a,dim]● 0#[default] #[fg=#45cbe6,dim]✓ 0#[default] #[fg=#a8a8b2,dim]○ 0#[default]"
        );

        let mixed_summary = render_structured_summary(
            &config,
            BadgeStateCounts {
                blocked: 0,
                limited: 0,
                working: 1,
                done: 0,
                idle: 9,
            },
        );
        assert_eq!(
            mixed_summary,
            "#[fg=#ff6b6b,dim]▲ 0#[default] #[fg=#f5a742,dim]⋄ 0#[default] #[fg=#4fd08a]● 1#[default] #[fg=#45cbe6,dim]✓ 0#[default] #[fg=#a8a8b2]○ 9#[default]"
        );
        assert_eq!(tmux_display_width(&empty_summary), 19);
        assert_eq!(tmux_display_width(&mixed_summary), 19);

        config.statusline.summary.hide_idle = true;
        let without_idle = render_structured_summary(&config, BadgeStateCounts::default());
        assert_eq!(
            without_idle,
            "#[fg=#ff6b6b,dim]▲ 0#[default] #[fg=#f5a742,dim]⋄ 0#[default] #[fg=#4fd08a,dim]● 0#[default] #[fg=#45cbe6,dim]✓ 0#[default]"
        );
        assert_eq!(tmux_display_width(&without_idle), 15);
    }

    #[test]
    fn default_summary_width_is_counted_at_the_eighty_cell_budget_boundary() {
        let config = Config::default();
        let summary = render_structured_summary(
            &config,
            BadgeStateCounts {
                blocked: 1,
                limited: 0,
                working: 1,
                done: 1,
                idle: 1,
            },
        );
        let mut category_tokens = [StatusToken {
            rendered: "c".repeat(61),
            compact: String::new(),
            current: true,
        }];
        let category_included = [true];

        assert_eq!(
            status_projection_width(
                &summary,
                &category_tokens,
                &category_included,
                &[],
                &[],
                &[],
                &[],
                "",
                &config,
            ),
            STATUS_OPTION_CELL_BUDGET
        );

        category_tokens[0].rendered.push('c');
        assert_eq!(
            status_projection_width(
                &summary,
                &category_tokens,
                &category_included,
                &[],
                &[],
                &[],
                &[],
                "",
                &config,
            ),
            STATUS_OPTION_CELL_BUDGET + 1
        );
    }

    #[test]
    fn oversized_summary_remains_visible_with_current_action_targets() {
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
            session_zone_width: None,
            sessions: vec![SessionStatusPresentation {
                attached: Some(true),
                created_at: Some(1),
                ..plain_status_session("$1", "main", true)
            }],
            windows: vec![status_window("@1", "editor", 0, true)],
            categories: vec![status_category("work", true)],
            attention: vec![blocked_attention("%9", 900, "review", 90)],
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(
            rendered.summary.contains(&"B".repeat(70)),
            "{}",
            rendered.summary
        );
        assert!(
            rendered.attention.contains("▲ blocked"),
            "{}",
            rendered.attention
        );
        assert!(rendered.category.contains("range=user|C:"));
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
        assert!(total > STATUS_OPTION_CELL_BUDGET, "{total}: {rendered:?}");
    }

    #[test]
    fn oversized_current_session_and_category_tokens_keep_full_action_targets() {
        let mut config = Config::default();
        config.statusline.sessions.current.format = "{session}".to_string();
        config.statusline.windows.current.format = "{window}".to_string();
        config.statusline.category.format = "{category}".to_string();
        let snapshot = StatusSnapshot {
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$42".to_string(),
            },
            sessions: vec![plain_status_session("$42", &"界🚀".repeat(100), true)],
            windows: vec![WindowStatusPresentation {
                session_ids: vec!["$42".to_string()],
                ..status_window("@77", &"窓🪟".repeat(100), 1, true)
            }],
            categories: vec![CategoryStatusPresentation {
                session_ids: vec!["$42".to_string()],
                ..status_category(&"分類🚀".repeat(25), true)
            }],
            ..status_snapshot()
        };
        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(rendered.sessions.contains("range=user|session:$42"));
        assert!(rendered.sessions.contains(&"界🚀".repeat(100)));
        assert!(rendered.windows.contains("range=user|window:@77"));
        assert!(rendered.windows.contains("@77"));
        assert!(rendered.category.contains("range=user|C:"));
        assert!(rendered.category.contains(&"分類🚀".repeat(25)));
        assert!(tmux_display_width(&rendered.sessions) > 80);
        assert!(tmux_display_width(&rendered.category) > 80);
        assert!(tmux_display_width(&rendered.windows) <= 80);
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
            context: crate::daemon::protocol::v2::StatusContext::Session {
                session_id: "$42".to_string(),
            },
            summary: BadgeStateCounts {
                blocked: 1,
                ..BadgeStateCounts::default()
            },
            sessions: vec![
                plain_status_session("$42", &"界🚀".repeat(100), true),
                plain_status_session("$43", "inactive-peer-abcdefghijklmnop", false),
            ],
            windows: vec![WindowStatusPresentation {
                session_ids: vec!["$42".to_string()],
                ..status_window("@77", &"窓🪟".repeat(100), 1, true)
            }],
            categories: vec![CategoryStatusPresentation {
                session_ids: vec!["$42".to_string(), "$43".to_string()],
                ..status_category(&"分類🚀".repeat(25), true)
            }],
            ..status_snapshot()
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(!rendered.summary.is_empty(), "{rendered:?}");
        assert!(rendered.category.contains(&"分類🚀".repeat(25)));
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
            session_zone_width: None,
            sessions: (1..=8)
                .map(|index| {
                    plain_status_session(
                        &format!("${index}"),
                        &format!("session-{index}-{}", "x".repeat(24)),
                        index == 1,
                    )
                })
                .collect(),
            windows: vec![status_window("@1", "editor", 1, true)],
            categories: vec![status_category("work", true)],
            attention: vec![blocked_attention("%1", 101, "review", 90)],
        };

        let rendered = render_structured_status_snapshot(&config, &snapshot).unwrap();

        assert!(tmux_display_width(&rendered.sessions) > 80);
        assert!(!rendered.summary.is_empty(), "{rendered:?}");
        assert!(rendered.category.contains("work"), "{rendered:?}");
        assert!(rendered.windows.contains("editor"), "{rendered:?}");
        assert!(rendered.attention.contains("▲ review"));
        assert!(!rendered.attention.contains("perm"));
        assert!(!rendered.attention.contains("1m30s"));
    }

    #[test]
    fn category_tokens_never_replace_names_with_compact_visual_ids() {
        let config = Config::default();
        let categories = vec![
            status_category(&"同じ見た目🚀".repeat(10), true),
            CategoryStatusPresentation {
                session_ids: vec!["$2".to_string()],
                ..status_category(&("同じ見た目🚀".repeat(9) + "別"), true)
            },
        ];

        let tokens = structured_category_tokens(&config, &categories).unwrap();

        assert_eq!(tokens[0].compact, tokens[0].rendered);
        assert_eq!(tokens[1].compact, tokens[1].rendered);
        assert_ne!(tokens[0].compact, tokens[1].compact);
        assert!(tokens[0].compact.contains(&"同じ見た目🚀".repeat(10)));
        assert!(
            tokens[1]
                .compact
                .contains(&("同じ見た目🚀".repeat(9) + "別"))
        );
        assert!(tokens[0].compact.contains("range=user|C:"));
        assert!(tokens[1].compact.contains("range=user|C:"));
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
                category: None,
                ..plain_status_session(
                    &format!("${}", index + 1),
                    &format!("session-{index}-{}", "x".repeat(20)),
                    index == 8,
                )
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
        let entries = vec![blocked_attention(
            "%9",
            900,
            &"長いセッション🚀".repeat(50),
            5_400,
        )];

        let rendered = render_structured_attention(&config, &entries);

        assert_eq!(
            rendered,
            format!(
                "#[range=user|{}]▲ blocked#[norange]",
                attention_target_key(&entries[0].pane_instance)
            )
        );
        assert!(tmux_display_width(&rendered) <= 80);
    }

    #[test]
    fn attention_target_tracks_the_oldest_pane_and_fails_closed_when_stale() {
        let newer = blocked_attention("%1", 101, "newer", 10);
        let mut older = blocked_attention("%2", 202, "older", 20);
        older.reason = Some("Other(wait)".to_string());
        let entries = vec![newer, older.clone()];
        let target = attention_target_key(&older.pane_instance);

        assert!(target.starts_with(ATTENTION_RANGE_PREFIX));
        assert!(target.len() <= TMUX_USER_RANGE_NAME_MAX_BYTES);
        assert_eq!(
            resolve_attention_target(&entries, &target).unwrap(),
            older.pane_instance
        );
        assert!(resolve_attention_target(&entries, "p:stale").is_err());
    }

    #[test]
    fn attention_hides_permission_reason_but_keeps_other_wait_and_error_reasons() {
        let config = Config::default();
        let mut entry = blocked_attention("%1", 101, "main", 10);

        let permission = render_structured_attention(&config, std::slice::from_ref(&entry));
        assert!(permission.contains("▲ main"), "{permission}");
        assert!(!permission.contains("perm"), "{permission}");

        entry.reason = Some("Other(wait)".to_string());
        let waiting = render_structured_attention(&config, std::slice::from_ref(&entry));
        assert!(waiting.contains("▲ main · wait"), "{waiting}");

        entry.reason = Some("error".to_string());
        let error = render_structured_attention(&config, std::slice::from_ref(&entry));
        assert!(error.contains("▲ main · err"), "{error}");
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
    fn stale_session_index_succeeds_without_switching() {
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

        switch_statusline_session(&mock, "client", "$1", 1).unwrap();

        assert!(
            mock.calls()
                .iter()
                .all(|call| call.first().map(String::as_str) != Some("switch-client"))
        );
    }

    #[test]
    fn category_targets_come_only_from_the_published_display_model() {
        let mock = MockTmuxRunner::new();
        let work = category_target_key("work").unwrap();
        let personal = category_target_key("personal").unwrap();
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_CATEGORY,
            ],
            &format!(
                "#[range=user|c:{personal}] personal #[norange]#[range=user|C:{work}] work #[norange]\n"
            ),
        );

        let targets = displayed_category_targets(&mock, "$1").unwrap();

        assert_eq!(targets, vec![personal, work]);
        assert!(
            mock.calls()
                .iter()
                .all(|call| call.first().map(String::as_str) != Some("list-sessions"))
        );
    }

    #[test]
    fn category_targets_allow_a_hidden_current_and_reject_multiple_active_targets() {
        let mock = MockTmuxRunner::new();
        let work = category_target_key("work").unwrap();
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_CATEGORY,
            ],
            &format!("#[range=user|c:{work}] work #[norange]\n"),
        );

        assert_eq!(
            displayed_category_targets(&mock, "$1").unwrap(),
            vec![work.clone()]
        );

        let mock = MockTmuxRunner::new();
        let personal = category_target_key("personal").unwrap();
        mock.stub(
            &[
                "show-option",
                "-qv",
                "-t",
                "$1",
                crate::options::KEY_STATUS_CATEGORY,
            ],
            &format!(
                "#[range=user|C:{work}] one #[norange]#[range=user|C:{personal}] two #[norange]\n"
            ),
        );
        let error = displayed_category_targets(&mock, "$1").unwrap_err();
        assert!(error.to_string().contains("multiple active categories"));
    }

    #[test]
    fn category_cycle_uses_all_effective_categories_in_current_mode() {
        let mock = MockTmuxRunner::new();
        let memory_key = crate::session::client_memory_key("client", "b");
        mock.stub(&["show-option", "-gqv", &memory_key], "");
        mock.stub(&["switch-client", "-c", "client", "-t", "=two:"], "");
        let snapshot = category_navigation_snapshot(&[
            ("$1", "one", "a"),
            ("$2", "two", "b"),
            ("$3", "three", "c"),
        ]);

        cycle_statusline_category_with_snapshot(&mock, &snapshot, "client", "$1", Direction::Next)
            .unwrap();

        assert!(mock.calls().iter().all(|call| {
            call.first().map(String::as_str) != Some("show-option")
                || call.get(2).map(String::as_str) != Some("-t")
        }));
        assert!(
            mock.calls()
                .iter()
                .any(|call| { call == &["switch-client", "-c", "client", "-t", "=two:"] })
        );
        assert!(
            mock.calls()
                .iter()
                .all(|call| call.first().map(String::as_str) != Some("list-sessions")),
            "resolved category cycle must not query a second session snapshot"
        );
        assert_eq!(
            mock.calls().last().unwrap(),
            &["switch-client", "-c", "client", "-t", "=two:"]
        );
    }

    #[test]
    fn consecutive_category_cycles_preserve_next_and_previous_order() {
        let mock = MockTmuxRunner::new();
        for category in ["a", "b", "c"] {
            let key = crate::session::client_memory_key("client", category);
            mock.stub(&["show-option", "-gqv", &key], "");
        }
        mock.stub(&["switch-client", "-c", "client", "-t", "=one:"], "");
        mock.stub(&["switch-client", "-c", "client", "-t", "=two:"], "");
        mock.stub(&["switch-client", "-c", "client", "-t", "=three:"], "");

        let snapshot = category_navigation_snapshot(&[
            ("$1", "one", "a"),
            ("$2", "two", "b"),
            ("$3", "three", "c"),
        ]);
        cycle_statusline_category_with_snapshot(&mock, &snapshot, "client", "$1", Direction::Next)
            .unwrap();
        cycle_statusline_category_with_snapshot(&mock, &snapshot, "client", "$2", Direction::Next)
            .unwrap();
        cycle_statusline_category_with_snapshot(
            &mock,
            &snapshot,
            "client",
            "$3",
            Direction::Previous,
        )
        .unwrap();

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
        let snapshot = category_navigation_snapshot(&[("$1", "one", "a")]);
        let error = cycle_statusline_category_with_snapshot(
            &mock,
            &snapshot,
            "client",
            "$1",
            Direction::Next,
        )
        .unwrap_err();

        assert!(error.to_string().contains("at least two categories"));
        assert!(
            mock.calls()
                .iter()
                .all(|call| call.first().map(String::as_str) != Some("switch-client"))
        );
    }

    #[test]
    fn category_target_is_fixed_length_for_special_utf8_and_uncategorized() {
        for category in ["", "public", "work space:|#日本語"] {
            let encoded = category_target_key(category).unwrap();
            assert_eq!(encoded.len(), 12);
            validate_category_target(&encoded).unwrap();
            assert!(
                encoded
                    .bytes()
                    .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') })
            );
            assert!(
                format!("{CATEGORY_RANGE_PREFIX}{encoded}").len() <= TMUX_USER_RANGE_NAME_MAX_BYTES
            );
            assert!(
                format!("{CURRENT_CATEGORY_RANGE_PREFIX}{encoded}").len()
                    <= TMUX_USER_RANGE_NAME_MAX_BYTES
            );
        }
        assert!(category_target_key(&"x".repeat(257)).is_err());
        assert!(validate_category_target("QR").is_err());
    }

    #[test]
    fn structured_snapshot_rejects_unaddressable_category_key() {
        let snapshot = StatusSnapshot {
            categories: vec![CategoryStatusPresentation {
                category: "x".repeat(257),
                ..status_category("placeholder", true)
            }],
            ..status_snapshot()
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

use anyhow::Result;

use crate::tmux::TmuxRunner;

pub const KEY_PANE_STATE: &str = "@vde_pane_state";
pub const KEY_STATUS_SUMMARY: &str = "@vde_status_summary";
pub const KEY_STATUS_CATEGORY: &str = "@vde_status_category";
pub const KEY_STATUS_SESSIONS: &str = "@vde_status_sessions";
pub const KEY_STATUS_WINDOWS: &str = "@vde_status_windows";
pub const KEY_STATUS_ATTENTION: &str = "@vde_status_attention";
pub const KEY_STATUS_PANE: &str = "@vde_status_pane";

pub const KEY_SIDEBAR_MARKER: &str = "@vde_sidebar";

pub const KEY_CATEGORY: &str = "@vde_category";
pub const KEY_CATEGORY_OVERRIDE: &str = "@vde_category_override";
pub const KEY_PROJECT_PATH: &str = "@vde_project_path";

/// Fixed pane-option list retained for `pane-state cleanup-legacy --all`.
pub const LEGACY_PANE_OPTION_KEYS: &[&str] = &[
    "@vde_agent",
    "@vde_status",
    "@vde_prompt",
    "@vde_prompt_source",
    "@vde_wait_reason",
    "@vde_attention",
    "@vde_started_at",
    "@vde_completed_at",
    "@vde_tasks",
    "@vde_task_items",
    "@vde_task_item_ids",
    "@vde_subagents",
    "@vde_worktree_activity",
];

/// Fixed session-option list retained for `pane-state cleanup-legacy --all`.
pub const LEGACY_SESSION_OPTION_KEYS: &[&str] = &[
    "@vde_session_status",
    "@vde_session_state",
    "@vde_session_agent_counts",
];

/// Fixed window-option list retained for `pane-state cleanup-legacy --all`.
pub const LEGACY_WINDOW_OPTION_KEYS: &[&str] = &[
    "@vde_window_status",
    "@vde_window_state",
    "@vde_window_agent_counts",
];

pub fn set_session_option(
    runner: &dyn TmuxRunner,
    session: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    runner.run(&["set-option", "-t", session, key, value])?;
    Ok(())
}

pub fn unset_session_option(runner: &dyn TmuxRunner, session: &str, key: &str) -> Result<()> {
    runner.run(&["set-option", "-u", "-t", session, key])?;
    Ok(())
}

pub fn set_global_option(runner: &dyn TmuxRunner, key: &str, value: &str) -> Result<()> {
    runner.run(&["set-option", "-g", key, value])?;
    Ok(())
}

pub fn unset_global_option(runner: &dyn TmuxRunner, key: &str) -> Result<()> {
    runner.run(&["set-option", "-gu", key])?;
    Ok(())
}

pub fn show_global_option(runner: &dyn TmuxRunner, key: &str) -> Result<Option<String>> {
    let value = runner
        .run(&["show-option", "-gqv", key])?
        .trim()
        .to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

pub fn show_session_option(
    runner: &dyn TmuxRunner,
    session_id: &str,
    key: &str,
) -> Result<Option<String>> {
    let value = runner
        .run(&["show-option", "-qv", "-t", session_id, key])?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    #[test]
    fn all_keys_use_vde_namespace() {
        let mut keys: Vec<&str> = LEGACY_PANE_OPTION_KEYS.to_vec();
        keys.extend(LEGACY_SESSION_OPTION_KEYS);
        keys.extend(LEGACY_WINDOW_OPTION_KEYS);
        keys.extend([
            KEY_SIDEBAR_MARKER,
            KEY_CATEGORY,
            KEY_CATEGORY_OVERRIDE,
            KEY_PROJECT_PATH,
            KEY_PANE_STATE,
            KEY_STATUS_SUMMARY,
            KEY_STATUS_CATEGORY,
            KEY_STATUS_SESSIONS,
            KEY_STATUS_WINDOWS,
            KEY_STATUS_ATTENTION,
            KEY_STATUS_PANE,
        ]);
        for key in keys {
            assert!(key.starts_with("@vde_"), "{key} が @vde_ 名前空間でない");
        }
    }

    #[test]
    fn legacy_cleanup_lists_are_fixed_and_exclude_canonical_and_display_options() {
        assert_eq!(
            LEGACY_PANE_OPTION_KEYS,
            [
                "@vde_agent",
                "@vde_status",
                "@vde_prompt",
                "@vde_prompt_source",
                "@vde_wait_reason",
                "@vde_attention",
                "@vde_started_at",
                "@vde_completed_at",
                "@vde_tasks",
                "@vde_task_items",
                "@vde_task_item_ids",
                "@vde_subagents",
                "@vde_worktree_activity",
            ]
        );
        assert_eq!(
            LEGACY_SESSION_OPTION_KEYS,
            [
                "@vde_session_status",
                "@vde_session_state",
                "@vde_session_agent_counts",
            ]
        );
        assert_eq!(
            LEGACY_WINDOW_OPTION_KEYS,
            [
                "@vde_window_status",
                "@vde_window_state",
                "@vde_window_agent_counts",
            ]
        );
        let legacy = LEGACY_PANE_OPTION_KEYS
            .iter()
            .chain(LEGACY_SESSION_OPTION_KEYS)
            .chain(LEGACY_WINDOW_OPTION_KEYS)
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(legacy.len(), 19);
        assert!(!legacy.contains(&KEY_PANE_STATE));
        for key in [
            KEY_STATUS_SUMMARY,
            KEY_STATUS_CATEGORY,
            KEY_STATUS_SESSIONS,
            KEY_STATUS_WINDOWS,
            KEY_STATUS_ATTENTION,
            KEY_STATUS_PANE,
        ] {
            assert!(!legacy.contains(&key));
        }
    }

    #[test]
    fn unset_session_option_issues_unset() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["set-option", "-u", "-t", "main", KEY_CATEGORY_OVERRIDE],
            "",
        );
        unset_session_option(&mock, "main", KEY_CATEGORY_OVERRIDE).unwrap();
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn set_global_option_issues_global_set() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
        set_global_option(&mock, "@vde_client_616263_work", "main").unwrap();
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn show_global_option_reads_quiet_value() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-option", "-gqv", "@vde_client_616263_work"],
            "main\n",
        );
        let value = show_global_option(&mock, "@vde_client_616263_work").unwrap();
        assert_eq!(value, Some("main".to_string()));
    }

    #[test]
    fn show_global_option_maps_empty_to_none() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "\n");
        let value = show_global_option(&mock, "@vde_client_616263_work").unwrap();
        assert_eq!(value, None);
    }

    #[test]
    fn show_session_option_preserves_rendered_leading_space() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["show-option", "-qv", "-t", "$1", KEY_STATUS_SESSIONS],
            " #[range=user|session:$1] main #[norange]\n",
        );

        let value = show_session_option(&mock, "$1", KEY_STATUS_SESSIONS).unwrap();

        assert_eq!(
            value.as_deref(),
            Some(" #[range=user|session:$1] main #[norange]")
        );
    }
}

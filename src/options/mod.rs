pub mod snapshot;

use anyhow::Result;

use crate::tmux::TmuxRunner;

pub const KEY_AGENT: &str = "@vde_agent";
pub const KEY_STATUS: &str = "@vde_status";
pub const KEY_PROMPT: &str = "@vde_prompt";
pub const KEY_PROMPT_SOURCE: &str = "@vde_prompt_source";
pub const KEY_WAIT_REASON: &str = "@vde_wait_reason";
pub const KEY_ATTENTION: &str = "@vde_attention";
pub const KEY_STARTED_AT: &str = "@vde_started_at";
pub const KEY_COMPLETED_AT: &str = "@vde_completed_at";
pub const KEY_TASKS: &str = "@vde_tasks";
pub const KEY_TASK_ITEMS: &str = "@vde_task_items";
pub const KEY_TASK_ITEM_IDS: &str = "@vde_task_item_ids";
pub const KEY_SUBAGENTS: &str = "@vde_subagents";
pub const KEY_WORKTREE_ACTIVITY: &str = "@vde_worktree_activity";

pub const KEY_SIDEBAR_MARKER: &str = "@vde_sidebar";

pub const KEY_LAYOUT_BASELINE: &str = "@vde_layout_baseline";
pub const KEY_LAYOUT_PANES: &str = "@vde_layout_panes";

pub const KEY_CATEGORY: &str = "@vde_category";
pub const KEY_CATEGORY_OVERRIDE: &str = "@vde_category_override";
pub const KEY_PROJECT_PATH: &str = "@vde_project_path";

pub const KEY_SESSION_STATUS: &str = "@vde_session_status";
pub const KEY_SESSION_STATE: &str = "@vde_session_state";

pub const KEY_HEARTBEAT: &str = "@vde_heartbeat";

pub const PANE_STATE_KEYS: &[&str] = &[
    KEY_AGENT,
    KEY_STATUS,
    KEY_PROMPT,
    KEY_PROMPT_SOURCE,
    KEY_WAIT_REASON,
    KEY_ATTENTION,
    KEY_STARTED_AT,
    KEY_COMPLETED_AT,
    KEY_TASKS,
    KEY_TASK_ITEMS,
    KEY_SUBAGENTS,
    KEY_WORKTREE_ACTIVITY,
];

pub fn set_pane_option(
    runner: &dyn TmuxRunner,
    pane_id: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    runner.run(&["set-option", "-p", "-t", pane_id, key, value])?;
    Ok(())
}

pub fn unset_pane_option(runner: &dyn TmuxRunner, pane_id: &str, key: &str) -> Result<()> {
    runner.run(&["set-option", "-p", "-u", "-t", pane_id, key])?;
    Ok(())
}

pub fn set_window_option(
    runner: &dyn TmuxRunner,
    window: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    runner.run(&["set-option", "-w", "-t", window, key, value])?;
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    #[test]
    fn all_keys_use_vde_namespace() {
        let mut keys: Vec<&str> = PANE_STATE_KEYS.to_vec();
        keys.extend([
            KEY_SIDEBAR_MARKER,
            KEY_LAYOUT_BASELINE,
            KEY_LAYOUT_PANES,
            KEY_CATEGORY,
            KEY_CATEGORY_OVERRIDE,
            KEY_PROJECT_PATH,
            KEY_SESSION_STATUS,
            KEY_SESSION_STATE,
            KEY_HEARTBEAT,
        ]);
        for key in keys {
            assert!(key.starts_with("@vde_"), "{key} が @vde_ 名前空間でない");
        }
    }

    #[test]
    fn set_pane_option_issues_scoped_set() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["set-option", "-p", "-t", "%3", KEY_STATUS, "running"], "");
        set_pane_option(&mock, "%3", KEY_STATUS, "running").unwrap();
        assert_eq!(
            mock.calls(),
            vec![vec![
                "set-option".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                "%3".to_string(),
                KEY_STATUS.to_string(),
                "running".to_string(),
            ]]
        );
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
}

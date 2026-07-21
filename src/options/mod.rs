use anyhow::Result;

use crate::tmux::TmuxRunner;

pub const KEY_STATUS_SUMMARY: &str = "@vde_status_summary";
pub const KEY_STATUS_CATEGORY: &str = "@vde_status_category";
pub const KEY_STATUS_SESSIONS: &str = "@vde_status_sessions";
pub const KEY_STATUS_WINDOWS: &str = "@vde_status_windows";
pub const KEY_STATUS_ATTENTION: &str = "@vde_status_attention";
pub const KEY_STATUS_PANE: &str = "@vde_status_pane";

pub const KEY_SIDEBAR_MARKER: &str = "@vde_sidebar";

pub const KEY_CATEGORY: &str = "@vde_category";
pub const KEY_PROJECT_PATH: &str = "@vde_project_path";

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
    fn external_tmux_option_contract_is_exact() {
        let keys = [
            KEY_SIDEBAR_MARKER,
            KEY_CATEGORY,
            KEY_PROJECT_PATH,
            KEY_STATUS_SUMMARY,
            KEY_STATUS_CATEGORY,
            KEY_STATUS_SESSIONS,
            KEY_STATUS_WINDOWS,
            KEY_STATUS_ATTENTION,
            KEY_STATUS_PANE,
        ];
        assert_eq!(
            keys,
            [
                "@vde_sidebar",
                "@vde_category",
                "@vde_project_path",
                "@vde_status_summary",
                "@vde_status_category",
                "@vde_status_sessions",
                "@vde_status_windows",
                "@vde_status_attention",
                "@vde_status_pane",
            ]
        );
    }

    #[test]
    fn unset_session_option_issues_unset() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["set-option", "-u", "-t", "main", KEY_CATEGORY], "");
        unset_session_option(&mock, "main", KEY_CATEGORY).unwrap();
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

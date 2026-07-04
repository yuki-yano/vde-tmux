//! project path から tmux session を作成または切替する。

use std::path::Path;

use anyhow::{Result, bail};

use crate::category::resolve_category_for_session;
use crate::config::Config;
use crate::options::{KEY_CATEGORY, KEY_PROJECT_PATH, set_session_option};
use crate::session::{SessionInfo, find_session, list_sessions, switch_client};
use crate::tmux::TmuxRunner;

pub fn session_name_for_path(path: &str) -> String {
    let base = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".into());
    let name = base
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if name.is_empty() {
        "project".to_string()
    } else {
        name
    }
}

pub fn switch_project(runner: &dyn TmuxRunner, config: &Config, path: &str) -> Result<()> {
    if path.trim().is_empty() {
        bail!("project path is empty");
    }
    let session_name = session_name_for_path(path);
    let sessions = list_sessions(runner)?;
    if find_session(&sessions, &session_name).is_none() {
        runner.run(&["new-session", "-d", "-s", &session_name, "-c", path])?;
        set_session_option(runner, &session_name, KEY_PROJECT_PATH, path)?;
        let session = SessionInfo {
            name: session_name.clone(),
            project_path: path.to_string(),
            ..SessionInfo::default()
        };
        let category = resolve_category_for_session(config, &session);
        set_session_option(runner, &session_name, KEY_CATEGORY, &category)?;
    }
    switch_client(runner, &session_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    #[test]
    fn session_name_replaces_unsafe_chars() {
        assert_eq!(session_name_for_path("/tmp/my repo"), "my_repo");
    }

    #[test]
    fn switch_project_creates_missing_session_and_sets_options() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        mock.stub(&["list-sessions", "-F", &format], "");
        mock.stub(&["new-session", "-d", "-s", "repo", "-c", "/tmp/repo"], "");
        mock.stub(
            &[
                "set-option",
                "-t",
                "repo",
                crate::options::KEY_PROJECT_PATH,
                "/tmp/repo",
            ],
            "",
        );
        mock.stub(
            &["set-option", "-t", "repo", crate::options::KEY_CATEGORY, ""],
            "",
        );
        mock.stub(&["switch-client", "-t", "repo"], "");
        switch_project(&mock, &crate::config::Config::default(), "/tmp/repo").unwrap();
        assert_eq!(mock.calls().len(), 5);
    }
}

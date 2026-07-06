//! project path から tmux session を作成または切替する。

use std::path::Path;

use anyhow::{Result, bail};

use crate::category::resolve_category_for_session;
use crate::config::Config;
use crate::options::{KEY_CATEGORY, KEY_PROJECT_PATH, set_session_option};
use crate::session::{
    SessionInfo, current_client_name, find_session, list_sessions, remember_session_for_client,
    switch_client_for_client,
};
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
    let client = current_client_name(runner)?;
    if client.trim().is_empty() {
        bail!("no tmux client available for project switch");
    }
    let session_name = session_name_for_path(path);
    let sessions = list_sessions(runner)?;
    let category = if let Some(session) = find_session(&sessions, &session_name) {
        resolve_category_for_session(config, session)
    } else {
        runner.run(&["new-session", "-d", "-s", &session_name, "-c", path])?;
        set_session_option(runner, &session_name, KEY_PROJECT_PATH, path)?;
        let session = SessionInfo {
            name: session_name.clone(),
            project_path: path.to_string(),
            ..SessionInfo::default()
        };
        let category = resolve_category_for_session(config, &session);
        set_session_option(runner, &session_name, KEY_CATEGORY, &category)?;
        category
    };
    switch_client_for_client(runner, &client, &session_name)?;
    remember_session_for_client(runner, &client, &category, &session_name)
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
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "\t/dev/ttys001\n",
        );
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
            &[
                "set-option",
                "-t",
                "repo",
                crate::options::KEY_CATEGORY,
                "public",
            ],
            "",
        );
        mock.stub(&["switch-client", "-c", "/dev/ttys001", "-t", "=repo:"], "");
        mock.stub(
            &[
                "set-option",
                "-g",
                "@vde_client_2f6465762f74747973303031_public",
                "repo",
            ],
            "",
        );
        switch_project(&mock, &config, "/tmp/repo").unwrap();
        assert_eq!(mock.calls().len(), 7);
    }

    #[test]
    fn switch_project_does_not_create_session_without_client() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "\t\n",
        );
        mock.stub(&["list-clients", "-F", "#{client_name}\t#{client_tty}"], "");

        let err = switch_project(&mock, &crate::config::Config::default(), "/tmp/repo")
            .unwrap_err()
            .to_string();

        assert!(err.contains("no tmux client"), "{err}");
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn switch_project_uses_exact_target_for_dotted_session_name() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "/dev/ttys001\t/dev/ttys001\n",
        );
        mock.stub(&["list-sessions", "-F", &format], "");
        mock.stub(
            &["new-session", "-d", "-s", "ni.zsh", "-c", "/tmp/ni.zsh"],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "ni.zsh",
                crate::options::KEY_PROJECT_PATH,
                "/tmp/ni.zsh",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "ni.zsh",
                crate::options::KEY_CATEGORY,
                "public",
            ],
            "",
        );
        mock.stub(
            &["switch-client", "-c", "/dev/ttys001", "-t", "=ni.zsh:"],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-g",
                "@vde_client_2f6465762f74747973303031_public",
                "ni.zsh",
            ],
            "",
        );

        switch_project(&mock, &config, "/tmp/ni.zsh").unwrap();
    }
}

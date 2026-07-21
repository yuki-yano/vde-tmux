use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::category::resolve_category_for_session;
use crate::config::{Config, PopupConfig};
use crate::options::{KEY_CATEGORY, KEY_PROJECT_PATH, set_session_option};
use crate::session::{
    SessionInfo, current_client_name, find_session, list_sessions, remember_session_for_client,
    switch_client_for_client,
};
use crate::tmux::TmuxRunner;

pub trait ProjectSelectorIo {
    fn list_projects(&mut self) -> Result<Vec<String>>;
    fn choose_project(&mut self, choices: &[String]) -> Result<Option<String>>;
}

pub struct SystemProjectSelectorIo;

impl ProjectSelectorIo for SystemProjectSelectorIo {
    fn list_projects(&mut self) -> Result<Vec<String>> {
        let output =
            crate::tmux::run_command("ghq", &["list", "-p"], Some(Duration::from_secs(3)))?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect())
    }

    fn choose_project(&mut self, choices: &[String]) -> Result<Option<String>> {
        if choices.is_empty() {
            return Ok(None);
        }
        let preview = r#"path="$(eval echo {})"; if command -v bat >/dev/null 2>&1; then bat --color=always --paging=never --style=plain --theme="Catppuccin Mocha" "$path/README.md"; else cat "$path/README.md"; fi"#;
        let mut child = Command::new("fzf")
            .args([
                "--prompt=Project> ",
                "--preview",
                preview,
                "--bind",
                "ctrl-d:preview-page-down,ctrl-u:preview-page-up",
                "--no-separator",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn fzf")?;
        if let Some(mut stdin) = child.stdin.take() {
            for choice in choices {
                writeln!(stdin, "{choice}")?;
            }
        }
        let output = child.wait_with_output().context("failed to wait fzf")?;
        if !output.status.success() {
            return Ok(None);
        }
        let selected = String::from_utf8(output.stdout)
            .context("fzf output was not utf-8")?
            .trim()
            .to_string();
        Ok((!selected.is_empty()).then_some(selected))
    }
}

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

pub fn display_project_path(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_string();
    };
    if path == home {
        return "~".to_string();
    }
    path.strip_prefix(&format!("{home}/"))
        .map(|rest| format!("~/{rest}"))
        .unwrap_or_else(|| path.to_string())
}

pub fn restore_project_selection(selection: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return selection.to_string();
    };
    if selection == "~" {
        return home.to_string();
    }
    selection
        .strip_prefix("~/")
        .map(|rest| format!("{home}/{rest}"))
        .unwrap_or_else(|| selection.to_string())
}

pub fn project_selector_popup_command(exe: &str) -> String {
    format!("{} project selector", shell_quote(exe))
}

pub fn open_project_selector_popup(
    runner: &dyn TmuxRunner,
    popup: &PopupConfig,
    exe: &str,
) -> Result<()> {
    let command = project_selector_popup_command(exe);
    runner.run(&[
        "display-popup",
        "-E",
        "-w",
        &popup.width,
        "-h",
        &popup.height,
        "-d",
        "#{pane_current_path}",
        &command,
    ])?;
    Ok(())
}

pub fn run_project_selector(
    runner: &dyn TmuxRunner,
    config: &Config,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let mut io = SystemProjectSelectorIo;
    run_project_selector_with_io(runner, config, env.get("HOME").map(String::as_str), &mut io)
}

pub fn run_project_selector_with_io(
    runner: &dyn TmuxRunner,
    config: &Config,
    home: Option<&str>,
    io: &mut dyn ProjectSelectorIo,
) -> Result<()> {
    let projects = io.list_projects()?;
    let choices = projects
        .iter()
        .map(|project| display_project_path(project, home))
        .collect::<Vec<_>>();
    let Some(selection) = io.choose_project(&choices)? else {
        return Ok(());
    };
    let path = restore_project_selection(&selection, home);
    switch_project(runner, config, &path)
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
    let (category, created_window) = if let Some(session) = find_session(&sessions, &session_name) {
        let category = resolve_category_for_session(config, session);
        set_session_option(runner, &session_name, KEY_CATEGORY, &category)?;
        (category, None)
    } else {
        let created_window = runner
            .run(&[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{window_id}",
                "-s",
                &session_name,
                "-c",
                path,
            ])?
            .trim()
            .to_string();
        set_session_option(runner, &session_name, KEY_PROJECT_PATH, path)?;
        let session = SessionInfo {
            name: session_name.clone(),
            project_path: path.to_string(),
            ..SessionInfo::default()
        };
        let category = resolve_category_for_session(config, &session);
        set_session_option(runner, &session_name, KEY_CATEGORY, &category)?;
        (category, Some(created_window))
    };
    switch_client_for_client(runner, &client, &session_name)?;
    if let Some(window) = created_window
        .as_deref()
        .filter(|window| !window.is_empty())
    {
        crate::sidebar::layout::open_if_auto_all_enabled(
            runner,
            window,
            &std::env::current_exe()?,
            config.sidebar.width,
            config.sidebar.min_width,
        )?;
    }
    remember_session_for_client(runner, &client, &category, &session_name)
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    struct MockProjectSelectorIo {
        projects: Vec<String>,
        selection: Option<String>,
        seen_choices: Vec<String>,
    }

    impl ProjectSelectorIo for MockProjectSelectorIo {
        fn list_projects(&mut self) -> Result<Vec<String>> {
            Ok(self.projects.clone())
        }

        fn choose_project(&mut self, choices: &[String]) -> Result<Option<String>> {
            self.seen_choices = choices.to_vec();
            Ok(self.selection.clone())
        }
    }

    #[test]
    fn session_name_replaces_unsafe_chars() {
        assert_eq!(session_name_for_path("/tmp/my repo"), "my_repo");
    }

    #[test]
    fn selector_displays_home_relative_path_and_restores_selection() {
        let home = "/Users/me";

        assert_eq!(
            display_project_path("/Users/me/repos/github.com/acme/app", Some(home)),
            "~/repos/github.com/acme/app"
        );
        assert_eq!(
            restore_project_selection("~/repos/github.com/acme/app", Some(home)),
            "/Users/me/repos/github.com/acme/app"
        );
    }

    #[test]
    fn selector_popup_uses_current_exe_command() {
        let mock = MockTmuxRunner::new();
        let command = project_selector_popup_command("/tmp/my vt");
        mock.stub(
            &[
                "display-popup",
                "-E",
                "-w",
                "50%",
                "-h",
                "50%",
                "-d",
                "#{pane_current_path}",
                &command,
            ],
            "",
        );

        open_project_selector_popup(&mock, &crate::config::PopupConfig::default(), "/tmp/my vt")
            .unwrap();

        assert_eq!(command, "'/tmp/my vt' project selector");
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn selector_switches_selected_project() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        let mut selector = MockProjectSelectorIo {
            projects: vec!["/Users/me/repos/ni.zsh".to_string()],
            selection: Some("~/repos/ni.zsh".to_string()),
            seen_choices: Vec::new(),
        };
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "/dev/ttys001\t/dev/ttys001\n",
        );
        mock.stub(&["list-sessions", "-F", &format], "");
        mock.stub(
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{window_id}",
                "-s",
                "ni.zsh",
                "-c",
                "/Users/me/repos/ni.zsh",
            ],
            "@1\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "ni.zsh",
                crate::options::KEY_PROJECT_PATH,
                "/Users/me/repos/ni.zsh",
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
            &["show-hooks", "-g", "after-new-window[90]"],
            "after-new-window[90] \n",
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

        run_project_selector_with_io(&mock, &config, Some("/Users/me"), &mut selector).unwrap();

        assert_eq!(selector.seen_choices, vec!["~/repos/ni.zsh"]);
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
        mock.stub(
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{window_id}",
                "-s",
                "repo",
                "-c",
                "/tmp/repo",
            ],
            "@1\n",
        );
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
        mock.stub(
            &["show-hooks", "-g", "after-new-window[90]"],
            "after-new-window[90] \n",
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
        assert_eq!(mock.calls().len(), 8);
    }

    #[test]
    fn switch_project_updates_category_mirror_for_existing_session() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        let mut config = crate::config::Config::default();
        config.categories.rules.push(crate::config::CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["/tmp/repo".to_string()],
        });
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "client\t/dev/ttys001\n",
        );
        mock.stub(
            &["list-sessions", "-F", &format],
            "repo\u{1f}1\u{1f}100\u{1f}\u{1f}/tmp/repo\u{1f}\u{1f}$1\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "repo",
                crate::options::KEY_CATEGORY,
                "work",
            ],
            "",
        );
        mock.stub(&["switch-client", "-c", "client", "-t", "=repo:"], "");
        mock.stub(
            &["set-option", "-g", "@vde_client_636c69656e74_work", "repo"],
            "",
        );

        switch_project(&mock, &config, "/tmp/repo").unwrap();

        assert!(mock.calls().iter().all(|call| {
            !matches!(
                call.first().map(String::as_str),
                Some("new-session" | "set-option")
            ) || call.get(3).map(String::as_str) != Some(crate::options::KEY_PROJECT_PATH)
        }));
    }

    #[test]
    fn switch_project_opens_sidebar_for_created_session_when_all_sidebar_is_enabled() {
        let mock = MockTmuxRunner::new();
        let format = crate::session::session_list_format();
        let exe = std::env::current_exe().unwrap();
        let attach_command = format!("{} sidebar attach", shell_quote(&exe.display().to_string()));
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        config.sidebar.width = crate::config::SidebarWidth::Columns(40);
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "\t/dev/ttys001\n",
        );
        mock.stub(&["list-sessions", "-F", &format], "");
        mock.stub(
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{window_id}",
                "-s",
                "repo",
                "-c",
                "/tmp/repo",
            ],
            "@9\n",
        );
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
        mock.stub(
            &["show-hooks", "-g", "after-new-window[90]"],
            "after-new-window[90] run-shell 'vt sidebar layout-applied'\n",
        );
        mock.stub(
            &[
                "list-panes",
                "-t",
                "@9",
                "-F",
                crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
            ],
            "%1\t\t80\n",
        );
        mock.stub(
            &[
                "display-message",
                "-p",
                "-t",
                "@9",
                "-F",
                "#{window_layout}",
            ],
            "layout-before\n",
        );
        mock.stub(
            &[
                "split-window",
                "-d",
                "-t",
                "@9",
                "-hbf",
                "-l",
                "40",
                &attach_command,
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

        assert!(mock.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("split-window")
                && call.get(2).map(String::as_str) == Some("-t")
                && call.get(3).map(String::as_str) == Some("@9")
        }));
        let calls = mock.calls();
        let switch_index = calls
            .iter()
            .position(|call| call.first().map(String::as_str) == Some("switch-client"))
            .unwrap();
        let split_index = calls
            .iter()
            .position(|call| call.first().map(String::as_str) == Some("split-window"))
            .unwrap();
        assert!(
            switch_index < split_index,
            "sidebar should open after the created session is attached"
        );
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
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{window_id}",
                "-s",
                "ni.zsh",
                "-c",
                "/tmp/ni.zsh",
            ],
            "@1\n",
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
            &["show-hooks", "-g", "after-new-window[90]"],
            "after-new-window[90] \n",
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

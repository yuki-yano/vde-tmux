use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};

use crate::category::{
    adjacent_category, resolve_category_for_session, resolve_dynamic_category_for_session,
    sessions_in_category,
};
use crate::config::Config;
use crate::options::{
    KEY_CATEGORY, KEY_CATEGORY_OVERRIDE, KEY_PROJECT_PATH, set_global_option, set_session_option,
    show_global_option,
};
use crate::tmux::TmuxRunner;

const FIELD_SEP: char = '\u{1f}';

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionInfo {
    pub name: String,
    pub attached: bool,
    pub created_at: i64,
    pub category: String,
    pub project_path: String,
    pub category_override: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Next,
    Previous,
}

pub fn session_list_format() -> String {
    [
        "#{session_name}",
        "#{session_attached}",
        "#{session_created}",
        "#{@vde_category}",
        "#{@vde_project_path}",
        "#{@vde_category_override}",
        "#{session_id}",
    ]
    .join(&FIELD_SEP.to_string())
}

pub fn parse_sessions(output: &str) -> Vec<SessionInfo> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEP).collect::<Vec<_>>();
            if fields.len() != 7 {
                return None;
            }
            Some(SessionInfo {
                name: fields[0].to_string(),
                attached: fields[1] == "1",
                created_at: fields[2].parse().unwrap_or_default(),
                category: fields[3].to_string(),
                project_path: fields[4].to_string(),
                category_override: fields[5].to_string(),
                id: fields[6].to_string(),
            })
        })
        .collect()
}

pub fn list_sessions(runner: &dyn TmuxRunner) -> Result<Vec<SessionInfo>> {
    let format = session_list_format();
    let output = runner.run(&["list-sessions", "-F", &format])?;
    Ok(parse_sessions(&output))
}

pub fn current_client_name(runner: &dyn TmuxRunner) -> Result<String> {
    if let Ok(client) = runner.run(&["display-message", "-p", "#{client_name}\t#{client_tty}"]) {
        let client = parse_client_target_line(client.trim());
        if !client.is_empty() {
            return Ok(client.to_string());
        }
    }
    Ok(runner
        .run(&["list-clients", "-F", "#{client_name}\t#{client_tty}"])?
        .lines()
        .map(parse_client_target_line)
        .find(|client| !client.is_empty())
        .unwrap_or_default()
        .to_string())
}

fn parse_client_target_line(line: &str) -> &str {
    let (name, tty) = line.split_once('\t').unwrap_or((line, ""));
    let name = name.trim();
    if !name.is_empty() {
        return name;
    }
    tty.trim()
}

pub fn current_session_name(runner: &dyn TmuxRunner) -> Result<String> {
    Ok(runner
        .run(&["display-message", "-p", "#{session_name}"])?
        .trim()
        .to_string())
}

pub fn switch_client(runner: &dyn TmuxRunner, session: &str) -> Result<()> {
    let target = exact_session_target(session);
    runner.run(&["switch-client", "-t", &target])?;
    Ok(())
}

pub fn exact_session_target(session: &str) -> String {
    format!("={session}:")
}

pub fn switch_client_for_client(
    runner: &dyn TmuxRunner,
    client_name: &str,
    session: &str,
) -> Result<()> {
    if client_name.trim().is_empty() {
        return switch_client(runner, session);
    }
    let target = exact_session_target(session);
    runner.run(&["switch-client", "-c", client_name, "-t", &target])?;
    Ok(())
}

pub fn client_memory_key(client_name: &str, category: &str) -> String {
    let hex = client_name
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let safe_category = category
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("@vde_client_{hex}_{safe_category}")
}

pub fn remember_session_for_client(
    runner: &dyn TmuxRunner,
    client_name: &str,
    category: &str,
    session_name: &str,
) -> Result<()> {
    set_global_option(
        runner,
        &client_memory_key(client_name, category),
        session_name,
    )
}

pub fn remembered_session_for_client(
    runner: &dyn TmuxRunner,
    client_name: &str,
    category: &str,
) -> Result<Option<String>> {
    show_global_option(runner, &client_memory_key(client_name, category))
}

pub fn find_session<'a>(sessions: &'a [SessionInfo], name: &str) -> Option<&'a SessionInfo> {
    sessions.iter().find(|session| session.name == name)
}

pub fn remember_client_session_for_session(
    runner: &dyn TmuxRunner,
    config: &Config,
    client_name: &str,
    session_name: &str,
) -> Result<()> {
    let sessions = list_sessions(runner)?;
    let session = find_session(&sessions, session_name)
        .ok_or_else(|| anyhow!("session not found: {session_name}"))?;
    let category = resolve_category_for_session(config, session);
    remember_session_for_client(runner, client_name, &category, session_name)
}

pub fn remember_current_client_session(runner: &dyn TmuxRunner, config: &Config) -> Result<()> {
    let client = current_client_name(runner)?;
    let current = current_session_name(runner)?;
    remember_client_session_for_session(runner, config, &client, &current)
}

pub fn set_session_category_override(
    runner: &dyn TmuxRunner,
    session_name: &str,
    category: &str,
) -> Result<()> {
    set_session_option(runner, session_name, KEY_CATEGORY_OVERRIDE, category)?;
    set_session_option(runner, session_name, KEY_CATEGORY, category)
}

pub fn create_session(
    runner: &dyn TmuxRunner,
    config: &Config,
    env: &BTreeMap<String, String>,
    cwd: Option<&str>,
) -> Result<String> {
    let client = current_client_name(runner)?;
    if client.trim().is_empty() {
        bail!("no tmux client available for session creation");
    }
    let cwd = resolve_session_cwd(runner, env, cwd)?;
    let created_format = format!("#{{session_name}}{FIELD_SEP}#{{window_id}}");
    let created = runner.run(&["new-session", "-d", "-P", "-F", &created_format, "-c", &cwd])?;
    let (session_name, window_id) = parse_created_session(&created)?;
    set_session_option(runner, &session_name, KEY_PROJECT_PATH, &cwd)?;
    let session = SessionInfo {
        name: session_name.clone(),
        project_path: cwd,
        ..SessionInfo::default()
    };
    let category = resolve_dynamic_category_for_session(config, &session);
    set_session_option(runner, &session_name, KEY_CATEGORY, &category)?;
    switch_client_for_client(runner, &client, &session_name)?;
    if !window_id.is_empty() {
        crate::sidebar::layout::open_if_auto_all_enabled(
            runner,
            &window_id,
            &std::env::current_exe()?,
            config.sidebar.width,
            config.sidebar.min_width,
        )?;
    }
    if !category.is_empty() {
        remember_session_for_client(runner, &client, &category, &session_name)?;
    }
    Ok(session_name)
}

fn parse_created_session(output: &str) -> Result<(String, String)> {
    let line = output.trim();
    let Some((session_name, window_id)) = line.split_once(FIELD_SEP) else {
        bail!("tmux did not return session/window ids for new session");
    };
    let session_name = session_name.trim().to_string();
    if session_name.is_empty() {
        bail!("tmux did not return a new session name");
    }
    Ok((session_name, window_id.trim().to_string()))
}

fn resolve_session_cwd(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    cwd: Option<&str>,
) -> Result<String> {
    let cwd = match cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => cwd.to_string(),
        None => runner
            .run(&["display-message", "-p", "#{pane_current_path}"])?
            .trim()
            .to_string(),
    };
    let cwd = expand_tilde_path(&cwd, env.get("HOME").map(String::as_str));
    if cwd.trim().is_empty() {
        bail!("session cwd is empty");
    }
    Ok(cwd)
}

fn expand_tilde_path(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_string();
    };
    if path == "~" {
        return home.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        }
    } else {
        path.to_string()
    }
}

pub fn refresh_session_categories(runner: &dyn TmuxRunner, config: &Config) -> Result<()> {
    for session in list_sessions(runner)? {
        let category = resolve_dynamic_category_for_session(config, &session);
        set_session_option(runner, &session.name, KEY_CATEGORY, &category)?;
    }
    Ok(())
}

pub fn use_category(runner: &dyn TmuxRunner, config: &Config, category: &str) -> Result<()> {
    let client = current_client_name(runner)?;
    let sessions = list_sessions(runner)?;
    if let Some(remembered) = remembered_session_for_client(runner, &client, category)?
        && find_session(&sessions, &remembered).is_some()
    {
        switch_client_for_client(runner, &client, &remembered)?;
        return remember_session_for_client(runner, &client, category, &remembered);
    }

    let Some(session) = sessions_in_category(config, &sessions, category)
        .first()
        .copied()
    else {
        bail!("no session in category: {category}");
    };
    switch_client_for_client(runner, &client, &session.name)?;
    remember_session_for_client(runner, &client, category, &session.name)
}

pub fn use_adjacent_category(
    runner: &dyn TmuxRunner,
    config: &Config,
    direction: Direction,
) -> Result<()> {
    let current = current_session_name(runner)?;
    let sessions = list_sessions(runner)?;
    let session = find_session(&sessions, &current)
        .ok_or_else(|| anyhow!("current session not found: {current}"))?;
    let current_category = resolve_category_for_session(config, session);
    let next_category = adjacent_category(config, &sessions, &current_category, direction)
        .ok_or_else(|| anyhow!("no categories available"))?;
    use_category(runner, config, &next_category)
}

pub fn cycle_session(runner: &dyn TmuxRunner, config: &Config, direction: Direction) -> Result<()> {
    let client = current_client_name(runner)?;
    let current = current_session_name(runner)?;
    let sessions = list_sessions(runner)?;
    let session = find_session(&sessions, &current)
        .ok_or_else(|| anyhow!("current session not found: {current}"))?;
    let category = resolve_category_for_session(config, session);
    let category_sessions = sessions_in_category(config, &sessions, &category);
    if category_sessions.is_empty() {
        bail!("no session in current category: {category}");
    }
    let index = category_sessions
        .iter()
        .position(|session| session.name == current)
        .unwrap_or(0);
    let next = match direction {
        Direction::Next => (index + 1) % category_sessions.len(),
        Direction::Previous => (index + category_sessions.len() - 1) % category_sessions.len(),
    };
    let next_name = category_sessions[next].name.clone();
    switch_client_for_client(runner, &client, &next_name)?;
    remember_session_for_client(runner, &client, &category, &next_name)
}

pub fn on_client_session_changed(
    runner: &dyn TmuxRunner,
    config: &Config,
    client_name: Option<&str>,
    session_name: Option<&str>,
) -> Result<()> {
    match (client_name, session_name) {
        (Some(client_name), Some(session_name)) => {
            remember_client_session_for_session(runner, config, client_name, session_name)
        }
        _ => remember_current_client_session(runner, config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    #[test]
    fn parse_sessions_reads_vde_options() {
        let sep = '\u{1f}'.to_string();
        let raw = format!(
            "{}\n{}\n",
            ["main", "1", "100", "work", "/repo", "", "$1"].join(&sep),
            ["sub", "0", "90", "", "", "private", "$2"].join(&sep)
        );
        let sessions = parse_sessions(&raw);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "main");
        assert!(sessions[0].attached);
        assert_eq!(sessions[0].category, "work");
        assert_eq!(sessions[0].project_path, "/repo");
        assert_eq!(sessions[1].category_override, "private");
    }

    #[test]
    fn list_sessions_uses_single_tmux_call() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
        );
        let sessions = list_sessions(&mock).unwrap();
        assert_eq!(sessions[0].name, "main");
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn current_context_reads_client_and_session() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "client-1\t/dev/ttys001\n",
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        assert_eq!(current_client_name(&mock).unwrap(), "client-1");
        assert_eq!(current_session_name(&mock).unwrap(), "main");
    }

    #[test]
    fn current_client_name_falls_back_to_list_clients_when_tmux_has_no_current_client() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "\t\n",
        );
        mock.stub(
            &["list-clients", "-F", "#{client_name}\t#{client_tty}"],
            "\t/dev/ttys001\n",
        );

        assert_eq!(current_client_name(&mock).unwrap(), "/dev/ttys001");
    }

    #[test]
    fn current_client_name_falls_back_to_list_clients_when_display_message_fails() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-clients", "-F", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );

        assert_eq!(current_client_name(&mock).unwrap(), "abc");
    }

    #[test]
    fn client_memory_key_hex_encodes_client_name() {
        assert_eq!(client_memory_key("abc", "work"), "@vde_client_616263_work");
        assert_eq!(
            client_memory_key("a/b", "work/private"),
            "@vde_client_612f62_work_private"
        );
    }

    #[test]
    fn remember_current_client_session_uses_effective_category() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\n",
        );
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
        remember_current_client_session(&mock, &crate::config::Config::default()).unwrap();
        assert_eq!(mock.calls().len(), 4);
    }

    #[test]
    fn set_session_category_override_sets_override_and_category() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &[
                "set-option",
                "-t",
                "main",
                crate::options::KEY_CATEGORY_OVERRIDE,
                "private",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "main",
                crate::options::KEY_CATEGORY,
                "private",
            ],
            "",
        );
        set_session_category_override(&mock, "main", "private").unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn create_session_sets_project_category_switches_and_remembers() {
        let mock = MockTmuxRunner::new();
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "client\t/dev/ttys001\n",
        );
        mock.stub(
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_name}\u{1f}#{window_id}",
                "-c",
                "/Users/me",
            ],
            "zsh\u{1f}@9\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "zsh",
                crate::options::KEY_PROJECT_PATH,
                "/Users/me",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "zsh",
                crate::options::KEY_CATEGORY,
                "public",
            ],
            "",
        );
        mock.stub(&["switch-client", "-c", "client", "-t", "=zsh:"], "");
        mock.stub(
            &["show-hooks", "-g", "after-new-window[90]"],
            "after-new-window[90] \n",
        );
        mock.stub(
            &["set-option", "-g", "@vde_client_636c69656e74_public", "zsh"],
            "",
        );

        let session = create_session(
            &mock,
            &config,
            &BTreeMap::from([("HOME".to_string(), "/Users/me".to_string())]),
            Some("~/"),
        )
        .unwrap();

        assert_eq!(session, "zsh");
        assert_eq!(mock.calls().len(), 7);
    }

    #[test]
    fn create_session_opens_sidebar_for_created_window_when_all_sidebar_is_enabled() {
        let mock = MockTmuxRunner::new();
        let exe = std::env::current_exe().unwrap();
        let attach_command = format!(
            "{} sidebar attach",
            shell_quote_for_test(&exe.display().to_string())
        );
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        config.sidebar.width = crate::config::SidebarWidth::Columns(40);
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "client\t/dev/ttys001\n",
        );
        mock.stub(
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_name}\u{1f}#{window_id}",
                "-c",
                "/Users/me",
            ],
            "zsh\u{1f}@9\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "zsh",
                crate::options::KEY_PROJECT_PATH,
                "/Users/me",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "zsh",
                crate::options::KEY_CATEGORY,
                "public",
            ],
            "",
        );
        mock.stub(&["switch-client", "-c", "client", "-t", "=zsh:"], "");
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
        mock.stub(
            &["set-option", "-g", "@vde_client_636c69656e74_public", "zsh"],
            "",
        );

        create_session(
            &mock,
            &config,
            &BTreeMap::from([("HOME".to_string(), "/Users/me".to_string())]),
            Some("~/"),
        )
        .unwrap();

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
    fn cycle_session_switches_next_in_current_category() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\nsub\u{1f}0\u{1f}101\u{1f}work\u{1f}\u{1f}\u{1f}$2\nother\u{1f}0\u{1f}102\u{1f}private\u{1f}\u{1f}\u{1f}$3\n",
        );
        mock.stub(&["switch-client", "-c", "abc", "-t", "=sub:"], "");
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "sub"], "");
        cycle_session(&mock, &crate::config::Config::default(), Direction::Next).unwrap();
        assert_eq!(mock.calls().len(), 5);
    }

    #[test]
    fn use_adjacent_category_switches_with_explicit_client() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}a\u{1f}\u{1f}\u{1f}$1\none\u{1f}0\u{1f}101\u{1f}b\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["show-option", "-gqv", "@vde_client_616263_b"], "");
        mock.stub(&["switch-client", "-c", "abc", "-t", "=one:"], "");
        mock.stub(&["set-option", "-g", "@vde_client_616263_b", "one"], "");

        use_adjacent_category(&mock, &crate::config::Config::default(), Direction::Next).unwrap();

        assert_eq!(mock.calls().len(), 7);
    }

    #[test]
    fn use_category_prefers_remembered_session() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\nsub\u{1f}0\u{1f}101\u{1f}work\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "sub\n");
        mock.stub(&["switch-client", "-c", "abc", "-t", "=sub:"], "");
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "sub"], "");
        use_category(&mock, &crate::config::Config::default(), "work").unwrap();
        assert_eq!(mock.calls().len(), 5);
    }

    #[test]
    fn hook_with_args_remembers_given_client_session() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\n",
        );
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
        on_client_session_changed(
            &mock,
            &crate::config::Config::default(),
            Some("abc"),
            Some("main"),
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn refresh_session_categories_sets_effective_categories() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}private\u{1f}$1\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "main",
                crate::options::KEY_CATEGORY,
                "private",
            ],
            "",
        );
        refresh_session_categories(&mock, &crate::config::Config::default()).unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn refresh_session_categories_uses_default_over_stale_stored_category() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}/Users/me\u{1f}\u{1f}$1\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "main",
                crate::options::KEY_CATEGORY,
                "public",
            ],
            "",
        );
        refresh_session_categories(&mock, &config).unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    fn shell_quote_for_test(value: &str) -> String {
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
}

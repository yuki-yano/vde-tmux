//! tmux session と client の読み取り、切替、per-client 記憶を扱う。

use anyhow::{Result, anyhow, bail};

use crate::category::{adjacent_category, resolve_category_for_session, sessions_in_category};
use crate::config::Config;
use crate::options::{
    KEY_CATEGORY, KEY_CATEGORY_OVERRIDE, set_global_option, set_session_option, show_global_option,
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
    pub badge: String,
    pub state: String,
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
        "#{@vde_session_status}",
        "#{@vde_session_state}",
        "#{session_id}",
    ]
    .join(&FIELD_SEP.to_string())
}

pub fn parse_sessions(output: &str) -> Vec<SessionInfo> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEP).collect::<Vec<_>>();
            if fields.len() != 9 {
                return None;
            }
            Some(SessionInfo {
                name: fields[0].to_string(),
                attached: fields[1] == "1",
                created_at: fields[2].parse().unwrap_or_default(),
                category: fields[3].to_string(),
                project_path: fields[4].to_string(),
                category_override: fields[5].to_string(),
                badge: fields[6].to_string(),
                state: fields[7].to_string(),
                id: fields[8].to_string(),
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
    Ok(runner
        .run(&["display-message", "-p", "#{client_name}"])?
        .trim()
        .to_string())
}

pub fn current_session_name(runner: &dyn TmuxRunner) -> Result<String> {
    Ok(runner
        .run(&["display-message", "-p", "#{session_name}"])?
        .trim()
        .to_string())
}

pub fn switch_client(runner: &dyn TmuxRunner, session: &str) -> Result<()> {
    runner.run(&["switch-client", "-t", session])?;
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

pub fn refresh_session_categories(runner: &dyn TmuxRunner, config: &Config) -> Result<()> {
    for session in list_sessions(runner)? {
        let category = resolve_category_for_session(config, &session);
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
        switch_client(runner, &remembered)?;
        return remember_session_for_client(runner, &client, category, &remembered);
    }

    let Some(session) = sessions_in_category(config, &sessions, category)
        .first()
        .copied()
    else {
        bail!("no session in category: {category}");
    };
    switch_client(runner, &session.name)?;
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
    switch_client(runner, &next_name)?;
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
        let raw = "main\u{1f}1\u{1f}100\u{1f}work\u{1f}/repo\u{1f}\u{1f}\u{1f}\u{1f}\nsub\u{1f}0\u{1f}90\u{1f}\u{1f}\u{1f}private\u{1f}\u{1f}\u{1f}\n";
        let sessions = parse_sessions(raw);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "main");
        assert!(sessions[0].attached);
        assert_eq!(sessions[0].category, "work");
        assert_eq!(sessions[0].project_path, "/repo");
        assert_eq!(sessions[1].category_override, "private");
    }

    #[test]
    fn session_list_format_includes_session_status() {
        assert!(session_list_format().contains("#{@vde_session_status}"));
    }

    #[test]
    fn session_list_format_includes_session_state() {
        assert!(session_list_format().contains("#{@vde_session_state}"));
    }

    #[test]
    fn parse_sessions_reads_badge_field() {
        let sep = '\u{1f}';
        let line = [
            "main",
            "1",
            "1700000000",
            "misc",
            "/tmp",
            "",
            "▲",
            "blocked",
            "$1",
        ]
        .join(&sep.to_string());
        let sessions = parse_sessions(&line);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].badge, "▲");
    }

    #[test]
    fn parse_sessions_reads_state_field() {
        let sep = '\u{1f}';
        let line = [
            "main",
            "1",
            "1700000000",
            "misc",
            "/tmp",
            "",
            "●",
            "working",
            "$1",
        ]
        .join(&sep.to_string());
        let sessions = parse_sessions(&line);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, "working");
    }

    #[test]
    fn list_sessions_uses_single_tmux_call() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
        );
        let sessions = list_sessions(&mock).unwrap();
        assert_eq!(sessions[0].name, "main");
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn current_context_reads_client_and_session() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["display-message", "-p", "#{client_name}"], "client-1\n");
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        assert_eq!(current_client_name(&mock).unwrap(), "client-1");
        assert_eq!(current_session_name(&mock).unwrap(), "main");
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
        mock.stub(&["display-message", "-p", "#{client_name}"], "abc\n");
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
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
    fn cycle_session_switches_next_in_current_category() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(&["display-message", "-p", "#{client_name}"], "abc\n");
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\nsub\u{1f}0\u{1f}101\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\nother\u{1f}0\u{1f}102\u{1f}private\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
        );
        mock.stub(&["switch-client", "-t", "sub"], "");
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "sub"], "");
        cycle_session(&mock, &crate::config::Config::default(), Direction::Next).unwrap();
        assert_eq!(mock.calls().len(), 5);
    }

    #[test]
    fn use_category_prefers_remembered_session() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(&["display-message", "-p", "#{client_name}"], "abc\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\nsub\u{1f}0\u{1f}101\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
        );
        mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "sub\n");
        mock.stub(&["switch-client", "-t", "sub"], "");
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
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\n",
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
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}private\u{1f}\u{1f}\u{1f}\n",
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
}

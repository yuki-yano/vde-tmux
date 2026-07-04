//! tmux session と client の読み取り、切替、per-client 記憶を扱う。

use anyhow::Result;

use crate::options::{set_global_option, show_global_option};
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
}

pub fn session_list_format() -> String {
    [
        "#{session_name}",
        "#{session_attached}",
        "#{session_created}",
        "#{@vde_category}",
        "#{@vde_project_path}",
        "#{@vde_category_override}",
    ]
    .join(&FIELD_SEP.to_string())
}

pub fn parse_sessions(output: &str) -> Vec<SessionInfo> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEP).collect::<Vec<_>>();
            if fields.len() != 6 {
                return None;
            }
            Some(SessionInfo {
                name: fields[0].to_string(),
                attached: fields[1] == "1",
                created_at: fields[2].parse().unwrap_or_default(),
                category: fields[3].to_string(),
                project_path: fields[4].to_string(),
                category_override: fields[5].to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    #[test]
    fn parse_sessions_reads_vde_options() {
        let raw = "main\u{1f}1\u{1f}100\u{1f}work\u{1f}/repo\u{1f}\nsub\u{1f}0\u{1f}90\u{1f}\u{1f}\u{1f}private\n";
        let sessions = parse_sessions(raw);
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
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\n",
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
}

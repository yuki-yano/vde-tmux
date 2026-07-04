//! 全 pane の @vde_* 状態を list-panes 1 コールで取得する一括 reader。
//! daemon の tmux worker と statusline フォールバックの両方がこれを使う。

use anyhow::Result;

use super::PANE_STATE_KEYS;
use crate::tmux::TmuxRunner;

const FIELD_SEP: char = '\u{1f}';

/// 1 pane 分の観測値。@vde_* が未設定のフィールドは空文字列。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneSnapshot {
    pub session: String,
    pub window_id: String,
    pub pane_id: String,
    pub current_path: String,
    pub current_command: String,
    /// この pane の window がセッションのカレント window か(#{window_active})。
    pub window_active: bool,
    /// セッションにクライアントがアタッチされているか(#{session_attached} > 0)。
    pub session_attached: bool,
    pub is_sidebar: bool,
    pub agent: String,
    pub status: String,
    pub prompt: String,
    pub prompt_source: String,
    pub wait_reason: String,
    pub attention: String,
    pub started_at: String,
    pub completed_at: String,
    pub tasks: String,
    pub subagents: String,
}

/// list-panes -a に渡す -F フォーマット文字列を組み立てる。
/// 固定 7 フィールド + @vde_sidebar + PANE_STATE_KEYS の順。
pub fn snapshot_format() -> String {
    let mut fields: Vec<String> = vec![
        "#{session_name}".into(),
        "#{window_id}".into(),
        "#{pane_id}".into(),
        "#{pane_current_path}".into(),
        "#{pane_current_command}".into(),
        "#{window_active}".into(),
        "#{session_attached}".into(),
        format!("#{{{key}}}", key = super::KEY_SIDEBAR_MARKER),
    ];
    fields.extend(PANE_STATE_KEYS.iter().map(|key| format!("#{{{key}}}")));
    fields.join(&FIELD_SEP.to_string())
}

/// list-panes -a の出力(snapshot_format 準拠)をパースする。
/// フィールド数が合わない行はスキップして残りを返す(壊れた 1 行で全体を落とさない)。
pub fn parse_snapshot_lines(output: &str) -> Vec<PaneSnapshot> {
    let expected = 8 + PANE_STATE_KEYS.len();
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(FIELD_SEP).collect();
            if fields.len() != expected {
                return None;
            }
            Some(PaneSnapshot {
                session: fields[0].to_string(),
                window_id: fields[1].to_string(),
                pane_id: fields[2].to_string(),
                current_path: fields[3].to_string(),
                current_command: fields[4].to_string(),
                window_active: fields[5] == "1",
                session_attached: !fields[6].is_empty() && fields[6] != "0",
                is_sidebar: fields[7] == "1",
                agent: fields[8].to_string(),
                status: fields[9].to_string(),
                prompt: fields[10].to_string(),
                prompt_source: fields[11].to_string(),
                wait_reason: fields[12].to_string(),
                attention: fields[13].to_string(),
                started_at: fields[14].to_string(),
                completed_at: fields[15].to_string(),
                tasks: fields[16].to_string(),
                subagents: fields[17].to_string(),
            })
        })
        .collect()
}

/// 全セッションの pane snapshot を 1 コールで取得する。
pub fn read_all_panes(runner: &dyn TmuxRunner) -> Result<Vec<PaneSnapshot>> {
    let format = snapshot_format();
    let output = runner.run(&["list-panes", "-a", "-F", &format])?;
    Ok(parse_snapshot_lines(&output))
}

pub fn detect_agent_from_command(command: &str) -> Option<&'static str> {
    let leaf = command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match leaf.as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

pub fn is_live_agent_pane(pane: &PaneSnapshot) -> bool {
    !pane.is_sidebar
        && !pane.agent.trim().is_empty()
        && detect_agent_from_command(&pane.current_command).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::PANE_STATE_KEYS;
    use crate::tmux::mock::MockTmuxRunner;

    fn line(fields: &[&str]) -> String {
        fields.join("\u{1f}")
    }

    #[test]
    fn format_field_count_matches_parser_expectation() {
        assert_eq!(
            snapshot_format().matches('\u{1f}').count(),
            8 + PANE_STATE_KEYS.len() - 1
        );
    }

    #[test]
    fn snapshot_format_includes_window_active_and_session_attached() {
        let format = snapshot_format();
        assert!(format.contains("#{window_active}"));
        assert!(format.contains("#{session_attached}"));
    }

    #[test]
    fn parse_snapshot_lines_reads_activity_fields() {
        let raw = line(&[
            "main", "@1", "%1", "/tmp", "zsh", "1", "2", "", "codex", "running", "", "", "", "",
            "", "", "", "",
        ]);
        let panes = parse_snapshot_lines(&raw);
        assert_eq!(panes.len(), 1);
        assert!(panes[0].window_active);
        assert!(panes[0].session_attached);

        let detached = line(&[
            "main", "@1", "%1", "/tmp", "zsh", "0", "0", "", "codex", "running", "", "", "", "",
            "", "", "", "",
        ]);
        let panes = parse_snapshot_lines(&detached);
        assert!(!panes[0].window_active);
        assert!(!panes[0].session_attached);
    }

    #[test]
    fn parses_pane_with_agent_state() {
        let raw = line(&[
            "main",
            "@1",
            "%3",
            "/Users/me/repo",
            "node",
            "0",
            "0",
            "",
            "claude",
            "running",
            "fix bug",
            "hook",
            "",
            "",
            "1720000000",
            "",
            "2/5",
            "",
        ]);
        let panes = parse_snapshot_lines(&raw);
        assert_eq!(panes.len(), 1);
        let pane = &panes[0];
        assert_eq!(pane.session, "main");
        assert_eq!(pane.pane_id, "%3");
        assert!(!pane.is_sidebar);
        assert_eq!(pane.agent, "claude");
        assert_eq!(pane.status, "running");
        assert_eq!(pane.tasks, "2/5");
    }

    #[test]
    fn sidebar_marker_and_empty_options_parse() {
        let raw = line(&[
            "main",
            "@1",
            "%9",
            "/Users/me",
            "vt",
            "0",
            "0",
            "1",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]);
        let panes = parse_snapshot_lines(&raw);
        assert_eq!(panes.len(), 1);
        assert!(panes[0].is_sidebar);
        assert_eq!(panes[0].agent, "");
    }

    #[test]
    fn malformed_line_is_skipped_not_fatal() {
        let good = line(&[
            "main", "@1", "%3", "/p", "zsh", "0", "0", "", "", "", "", "", "", "", "", "", "", "",
        ]);
        let raw = format!("broken-line\n{good}\n");
        let panes = parse_snapshot_lines(&raw);
        assert_eq!(panes.len(), 1);
    }

    #[test]
    fn read_all_panes_issues_single_list_panes_call() {
        let mock = MockTmuxRunner::new();
        let format = snapshot_format();
        mock.stub(&["list-panes", "-a", "-F", &format], "");
        let panes = read_all_panes(&mock).unwrap();
        assert!(panes.is_empty());
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn detects_agent_process_from_current_command_only_for_real_agent_binary() {
        assert_eq!(detect_agent_from_command("codex"), Some("codex"));
        assert_eq!(
            detect_agent_from_command("/opt/homebrew/bin/claude --danger"),
            Some("claude")
        );
        assert_eq!(detect_agent_from_command("opencode"), Some("opencode"));
        assert_eq!(detect_agent_from_command("node"), None);
        assert_eq!(detect_agent_from_command("zsh"), None);
    }
}

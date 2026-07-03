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
/// 固定 5 フィールド + @vde_sidebar + PANE_STATE_KEYS の順。
pub fn snapshot_format() -> String {
    let mut fields: Vec<String> = vec![
        "#{session_name}".into(),
        "#{window_id}".into(),
        "#{pane_id}".into(),
        "#{pane_current_path}".into(),
        "#{pane_current_command}".into(),
        format!("#{{{key}}}", key = super::KEY_SIDEBAR_MARKER),
    ];
    fields.extend(PANE_STATE_KEYS.iter().map(|key| format!("#{{{key}}}")));
    fields.join(&FIELD_SEP.to_string())
}

/// list-panes -a の出力(snapshot_format 準拠)をパースする。
/// フィールド数が合わない行はスキップして残りを返す(壊れた 1 行で全体を落とさない)。
pub fn parse_snapshot_lines(output: &str) -> Vec<PaneSnapshot> {
    let expected = 6 + PANE_STATE_KEYS.len();
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
                is_sidebar: fields[5] == "1",
                agent: fields[6].to_string(),
                status: fields[7].to_string(),
                prompt: fields[8].to_string(),
                prompt_source: fields[9].to_string(),
                wait_reason: fields[10].to_string(),
                attention: fields[11].to_string(),
                started_at: fields[12].to_string(),
                completed_at: fields[13].to_string(),
                tasks: fields[14].to_string(),
                subagents: fields[15].to_string(),
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
            6 + PANE_STATE_KEYS.len() - 1
        );
    }

    #[test]
    fn parses_pane_with_agent_state() {
        let raw = line(&[
            "main",
            "@1",
            "%3",
            "/Users/me/repo",
            "node",
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
            "main", "@1", "%3", "/p", "zsh", "", "", "", "", "", "", "", "", "", "", "",
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
}

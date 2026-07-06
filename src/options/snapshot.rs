//! 全 pane の @vde_* 状態を list-panes 1 コールで取得する一括 reader。
//! daemon の tmux worker と statusline フォールバックの両方がこれを使う。

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

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
    pub pane_tty: String,
    pub pane_pid: String,
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
    /// `pane_current_command`、process tree、TTY のいずれかで現在 agent を観測できたか。
    pub agent_observed: bool,
}

/// list-panes -a に渡す -F フォーマット文字列を組み立てる。
/// 固定 9 フィールド + @vde_sidebar + PANE_STATE_KEYS の順。
pub fn snapshot_format() -> String {
    let mut fields: Vec<String> = vec![
        "#{session_name}".into(),
        "#{window_id}".into(),
        "#{pane_id}".into(),
        "#{pane_current_path}".into(),
        "#{pane_current_command}".into(),
        "#{pane_tty}".into(),
        "#{pane_pid}".into(),
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
    let expected = 10 + PANE_STATE_KEYS.len();
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(FIELD_SEP).collect();
            if fields.len() != expected {
                return None;
            }
            let current_command = fields[4].to_string();
            let agent_observed = detect_agent_from_command(&current_command).is_some();
            Some(PaneSnapshot {
                session: fields[0].to_string(),
                window_id: fields[1].to_string(),
                pane_id: fields[2].to_string(),
                current_path: fields[3].to_string(),
                current_command,
                pane_tty: fields[5].to_string(),
                pane_pid: fields[6].to_string(),
                window_active: fields[7] == "1",
                session_attached: !fields[8].is_empty() && fields[8] != "0",
                is_sidebar: fields[9] == "1",
                agent: fields[10].to_string(),
                status: fields[11].to_string(),
                prompt: fields[12].to_string(),
                prompt_source: fields[13].to_string(),
                wait_reason: fields[14].to_string(),
                attention: fields[15].to_string(),
                started_at: fields[16].to_string(),
                completed_at: fields[17].to_string(),
                tasks: fields[18].to_string(),
                subagents: fields[19].to_string(),
                agent_observed,
            })
        })
        .collect()
}

/// 全セッションの pane snapshot を 1 コールで取得する。
pub fn read_all_panes(runner: &dyn TmuxRunner) -> Result<Vec<PaneSnapshot>> {
    let format = snapshot_format();
    let output = runner.run(&["list-panes", "-a", "-F", &format])?;
    let mut panes = parse_snapshot_lines(&output);
    enrich_agents_from_processes(&mut panes);
    Ok(panes)
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

fn detect_agent_from_hint(hint: &str) -> Option<&'static str> {
    let lower = hint.to_ascii_lowercase();
    if lower.contains("codex") {
        Some("codex")
    } else if lower.contains("claude") {
        Some("claude")
    } else if lower.contains("opencode") {
        Some("opencode")
    } else {
        None
    }
}

fn detect_agent_from_tty_commands(commands: &str) -> Option<&'static str> {
    detect_agent_from_hint(commands)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessEntry {
    command: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ProcessSnapshot {
    by_pid: BTreeMap<i64, ProcessEntry>,
    children: BTreeMap<i64, Vec<i64>>,
}

impl ProcessSnapshot {
    fn parse(output: &str) -> Self {
        let mut snapshot = Self::default();
        for line in output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let mut fields = line.splitn(3, char::is_whitespace);
            let Some(pid) = fields.next().and_then(|value| value.parse::<i64>().ok()) else {
                continue;
            };
            let Some(ppid) = fields
                .next()
                .and_then(|value| value.trim().parse::<i64>().ok())
            else {
                continue;
            };
            let command = fields.next().unwrap_or("").trim().to_string();
            snapshot.by_pid.insert(pid, ProcessEntry { command });
            snapshot.children.entry(ppid).or_default().push(pid);
        }
        snapshot
    }

    fn find_agent_from_pid_tree(&self, root_pid: i64) -> Option<&'static str> {
        let mut stack = vec![root_pid];
        let mut visited = BTreeSet::new();
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            if let Some(entry) = self.by_pid.get(&pid)
                && let Some(agent) = detect_agent_from_hint(&entry.command)
            {
                return Some(agent);
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        None
    }
}

fn enrich_agents_from_processes(panes: &mut [PaneSnapshot]) {
    if !panes.iter().any(needs_runtime_agent_detection) {
        return;
    }
    let Some(processes) = read_process_snapshot() else {
        enrich_agents_from_ttys(panes);
        return;
    };
    enrich_agents_from_process_snapshot(panes, &processes);
    enrich_agents_from_ttys(panes);
}

fn enrich_agents_from_process_snapshot(panes: &mut [PaneSnapshot], processes: &ProcessSnapshot) {
    for pane in panes
        .iter_mut()
        .filter(|pane| needs_runtime_agent_detection(pane))
    {
        let Some(pid) = pane.pane_pid.trim().parse::<i64>().ok() else {
            continue;
        };
        if let Some(agent) = processes.find_agent_from_pid_tree(pid) {
            pane.agent = agent.to_string();
            pane.agent_observed = true;
        }
    }
}

fn needs_runtime_agent_detection(pane: &PaneSnapshot) -> bool {
    !pane.agent_observed && detect_agent_from_command(&pane.current_command).is_none()
}

fn read_process_snapshot() -> Option<ProcessSnapshot> {
    let output = Command::new("ps")
        .args(["-ax", "-o", "pid=,ppid=,command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(ProcessSnapshot::parse(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn enrich_agents_from_ttys(panes: &mut [PaneSnapshot]) {
    let mut tty_agents = BTreeMap::new();
    let ttys = panes
        .iter()
        .filter(|pane| needs_runtime_agent_detection(pane))
        .filter_map(|pane| normalize_tty(&pane.pane_tty))
        .collect::<BTreeSet<_>>();

    for tty in ttys {
        if let Some(commands) = read_tty_commands(&tty)
            && let Some(agent) = detect_agent_from_tty_commands(&commands)
        {
            tty_agents.insert(tty, agent);
        }
    }

    for pane in panes
        .iter_mut()
        .filter(|pane| needs_runtime_agent_detection(pane))
    {
        let Some(tty) = normalize_tty(&pane.pane_tty) else {
            continue;
        };
        if let Some(agent) = tty_agents.get(&tty) {
            pane.agent = (*agent).to_string();
            pane.agent_observed = true;
        }
    }
}

fn normalize_tty(tty: &str) -> Option<String> {
    let tty = tty.trim();
    if tty.is_empty() || tty == "?" {
        return None;
    }
    Some(tty.strip_prefix("/dev/").unwrap_or(tty).to_string())
}

fn read_tty_commands(tty: &str) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-t", tty])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn effective_agent(pane: &PaneSnapshot) -> Option<&str> {
    if let Some(agent) = detect_agent_from_command(&pane.current_command) {
        return Some(agent);
    }
    let agent = pane.agent.trim();
    if pane.agent_observed && !agent.is_empty() {
        return Some(agent);
    }
    None
}

pub fn is_live_agent_pane(pane: &PaneSnapshot) -> bool {
    !pane.is_sidebar && effective_agent(pane).is_some()
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
            10 + PANE_STATE_KEYS.len() - 1
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
            "main",
            "@1",
            "%1",
            "/tmp",
            "zsh",
            "/dev/ttys001",
            "123",
            "1",
            "2",
            "",
            "codex",
            "running",
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
        assert!(panes[0].window_active);
        assert!(panes[0].session_attached);

        let detached = line(&[
            "main",
            "@1",
            "%1",
            "/tmp",
            "zsh",
            "/dev/ttys001",
            "123",
            "0",
            "0",
            "",
            "codex",
            "running",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
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
            "/dev/ttys001",
            "123",
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
            "/dev/ttys001",
            "123",
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
            "main",
            "@1",
            "%3",
            "/p",
            "zsh",
            "/dev/ttys001",
            "123",
            "0",
            "0",
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
            "",
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

    #[test]
    fn stale_hook_marked_agent_pane_is_not_live_when_command_is_shell() {
        let pane = PaneSnapshot {
            current_command: "zsh".to_string(),
            agent: "codex".to_string(),
            status: "running".to_string(),
            ..PaneSnapshot::default()
        };

        assert!(!is_live_agent_pane(&pane));
    }

    #[test]
    fn command_marked_agent_pane_is_live_without_hook_options() {
        let pane = PaneSnapshot {
            current_command: "claude".to_string(),
            ..PaneSnapshot::default()
        };

        assert!(is_live_agent_pane(&pane));
        assert_eq!(effective_agent(&pane), Some("claude"));
    }

    #[test]
    fn parse_snapshot_lines_reads_pid_and_tty_fields() {
        let raw = line(&[
            "main",
            "@1",
            "%3",
            "/Users/me/repo",
            "node",
            "/dev/ttys061",
            "74605",
            "0",
            "0",
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
            "",
        ]);

        let panes = parse_snapshot_lines(&raw);

        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_tty, "/dev/ttys061");
        assert_eq!(panes[0].pane_pid, "74605");
    }

    #[test]
    fn detects_agent_from_process_tree_under_pane_pid() {
        let processes = ProcessSnapshot::parse(
            r#"
74605 1 -zsh
89779 74605 node /Users/me/.npm/bin/codex --yolo
89780 89779 /opt/homebrew/bin/codex --yolo
"#,
        );

        assert_eq!(processes.find_agent_from_pid_tree(74605), Some("codex"));
    }

    #[test]
    fn process_detected_agent_pane_is_live_without_hook_options() {
        let processes = ProcessSnapshot::parse(
            r#"
74605 1 -zsh
89779 74605 node /Users/me/.npm/bin/codex --yolo
"#,
        );
        let mut panes = vec![PaneSnapshot {
            current_command: "node".to_string(),
            pane_pid: "74605".to_string(),
            ..PaneSnapshot::default()
        }];

        enrich_agents_from_process_snapshot(&mut panes, &processes);

        assert_eq!(effective_agent(&panes[0]), Some("codex"));
        assert!(is_live_agent_pane(&panes[0]));
    }

    #[test]
    fn detects_agent_from_tty_command_output() {
        let commands = "-zsh\nnode /Users/me/.npm/bin/claude --dangerously-skip-permissions\n";

        assert_eq!(detect_agent_from_tty_commands(commands), Some("claude"));
    }
}

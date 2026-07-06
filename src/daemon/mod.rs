//! daemon の snapshot 集約と statusline badge。

pub mod lifecycle;
pub mod protocol;
pub mod runtime;
pub mod server;
pub mod session_badge;
pub mod workers;

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::protocol::{ClientMessage, ServerMessage};
use crate::daemon::session_badge::{BadgeState, badge_state, glyph_for_state};
use crate::hook::{AgentStatus, RollupLevel, pane_rollup_level};
use crate::options::snapshot::{PaneSnapshot, effective_agent, is_live_agent_pane, read_all_panes};
use crate::sidebar::state::SidebarState;
use crate::sidebar::tree::{SidebarRow, now_epoch_secs};
use crate::tmux::TmuxRunner;

const ENV_DAEMON_SOCKET: &str = "VDE_DAEMON_SOCKET";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPaneSummary {
    pub pane_id: String,
    pub agent: String,
    pub status: Option<AgentStatus>,
    pub wait_reason: Option<String>,
    pub rollup: RollupLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSnapshot {
    pub agent_count: usize,
    pub rollup: RollupLevel,
    pub panes: Vec<AgentPaneSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar: Option<SidebarFrame>,
    #[serde(default)]
    pub events: Vec<TransitionEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionEvent {
    pub pane_id: String,
    pub agent: String,
    pub from: Option<BadgeState>,
    pub to: BadgeState,
    pub at_epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarFrame {
    pub state: SidebarState,
    pub rows: Vec<SidebarRow>,
}

pub fn build_snapshot(panes: &[PaneSnapshot]) -> DaemonSnapshot {
    build_snapshot_with_sidebar(panes, None)
}

pub fn build_snapshot_with_sidebar(
    panes: &[PaneSnapshot],
    sidebar: Option<SidebarFrame>,
) -> DaemonSnapshot {
    let panes = panes
        .iter()
        .filter(|pane| is_live_agent_pane(pane))
        .map(|pane| {
            let status = parse_agent_status_for_pane(pane);
            let wait_reason = (!pane.wait_reason.is_empty()).then(|| pane.wait_reason.clone());
            let rollup = pane_rollup_level(status, wait_reason.as_deref());
            AgentPaneSummary {
                pane_id: pane.pane_id.clone(),
                agent: effective_agent(pane).unwrap_or_default().to_string(),
                status,
                wait_reason,
                rollup,
            }
        })
        .collect::<Vec<_>>();
    let rollup = panes
        .iter()
        .map(|pane| pane.rollup)
        .min()
        .unwrap_or(RollupLevel::Idle);
    DaemonSnapshot {
        agent_count: panes.len(),
        rollup,
        panes,
        sidebar,
        events: Vec::new(),
    }
}

pub fn render_summary(
    counts: &[(BadgeState, usize)],
    badge: &crate::config::BadgeConfig,
) -> String {
    counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(state, count)| {
            let glyph = glyph_for_state(*state, &badge.glyphs);
            let color = match state {
                BadgeState::Blocked => &badge.colors.blocked,
                BadgeState::Working => &badge.colors.working,
                BadgeState::Done => &badge.colors.done,
                BadgeState::Idle => &badge.colors.idle,
            };
            format!("#[fg={color}]{glyph}{count}#[default]")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn format_attention(entries: &[(String, RollupLevel, i64)]) -> String {
    let mut entries = entries.to_vec();
    entries.sort_by_key(|entry| Reverse(entry.2));
    let Some((session, level, elapsed)) = entries.first() else {
        return String::new();
    };
    let reason = match level {
        RollupLevel::Error => "err",
        RollupLevel::Permission => "perm",
        _ => "wait",
    };
    let elapsed = if *elapsed < 60 {
        format!("{elapsed}s")
    } else {
        format!("{}m", elapsed / 60)
    };
    let more = entries.len().saturating_sub(1);
    let suffix = if more > 0 {
        format!(" +{more}")
    } else {
        String::new()
    };
    // 装飾(色・pill)は statusline::render_attention_segment 側で付ける
    format!("▲ {session} · {reason} {elapsed}{suffix}")
}

pub fn statusline_summary_fallback(
    runner: &dyn TmuxRunner,
    config: &crate::config::Config,
) -> Result<String> {
    if !config.statusline.summary.enabled {
        return Ok(String::new());
    }
    let panes = read_all_panes(runner)?;
    let mut counts = summary_counts_for_panes(&panes);
    if config.statusline.summary.hide_idle {
        counts[3].1 = 0;
    }
    Ok(render_summary(&counts, &config.badge))
}

pub fn statusline_summary(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<String> {
    if !config.statusline.summary.enabled {
        return Ok(String::new());
    }
    let socket_path = daemon_socket_path(env, None);
    if socket_path.exists()
        && let Ok(value) = query_statusline_summary(&socket_path)
    {
        return Ok(value);
    }
    statusline_summary_fallback(runner, config)
}

pub fn statusline_attention_fallback(runner: &dyn TmuxRunner) -> Result<String> {
    let panes = read_all_panes(runner)?;
    let now = now_epoch_secs();
    let entries = panes
        .iter()
        .filter(|pane| is_live_agent_pane(pane))
        .filter(|pane| !(pane.window_active && pane.session_attached))
        .filter_map(|pane| {
            let status = parse_agent_status_for_pane(pane);
            let wait_reason = (!pane.wait_reason.is_empty()).then_some(pane.wait_reason.as_str());
            let rollup = pane_rollup_level(status, wait_reason);
            if badge_state(rollup, false) != BadgeState::Blocked {
                return None;
            }
            let started = pane.started_at.parse::<i64>().unwrap_or(now);
            Some((pane.session.clone(), rollup, (now - started).max(0)))
        })
        .collect::<Vec<_>>();
    Ok(format_attention(&entries))
}

pub fn statusline_attention(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let socket_path = daemon_socket_path(env, None);
    if socket_path.exists()
        && let Ok(value) = query_statusline_attention(&socket_path)
    {
        return Ok(value);
    }
    statusline_attention_fallback(runner)
}

pub fn daemon_socket_path(env: &BTreeMap<String, String>, explicit: Option<&str>) -> PathBuf {
    if let Some(path) = explicit.filter(|path| !path.trim().is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = env
        .get(ENV_DAEMON_SOCKET)
        .filter(|path| !path.trim().is_empty())
    {
        return PathBuf::from(path);
    }
    if let Some(runtime_dir) = env
        .get("XDG_RUNTIME_DIR")
        .filter(|path| !path.trim().is_empty())
    {
        return PathBuf::from(runtime_dir).join("vde-tmux/daemon.sock");
    }
    PathBuf::from(format!("/tmp/vde-tmux-{}/daemon.sock", unsafe {
        libc::geteuid()
    }))
}

pub fn query_statusline_summary(socket_path: &Path) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect {}", socket_path.display()))?;
    serde_json::to_writer(
        &mut stream,
        &ClientMessage::Query {
            proto: 1,
            what: crate::daemon::protocol::QueryTarget::Summary,
        },
    )?;
    stream.write_all(b"\n")?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line)?;
    match serde_json::from_str::<ServerMessage>(line.trim())? {
        ServerMessage::Summary { text } => Ok(text),
        ServerMessage::Attention { .. } => bail!("unexpected daemon attention response"),
        ServerMessage::Ack => bail!("unexpected daemon ack response"),
        ServerMessage::Error { message } => bail!(message),
        ServerMessage::Snapshot { .. } => bail!("unexpected daemon snapshot response"),
    }
}

pub fn query_statusline_attention(socket_path: &Path) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect {}", socket_path.display()))?;
    serde_json::to_writer(
        &mut stream,
        &ClientMessage::Query {
            proto: 1,
            what: crate::daemon::protocol::QueryTarget::Attention,
        },
    )?;
    stream.write_all(b"\n")?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line)?;
    match serde_json::from_str::<ServerMessage>(line.trim())? {
        ServerMessage::Attention { text } => Ok(text),
        ServerMessage::Summary { .. } => bail!("unexpected daemon summary response"),
        ServerMessage::Ack => bail!("unexpected daemon ack response"),
        ServerMessage::Error { message } => bail!(message),
        ServerMessage::Snapshot { .. } => bail!("unexpected daemon snapshot response"),
    }
}

fn summary_counts_for_panes(panes: &[PaneSnapshot]) -> [(BadgeState, usize); 4] {
    let mut blocked = 0usize;
    let mut working = 0usize;
    let mut done = 0usize;
    let mut idle = 0usize;
    for pane in panes.iter().filter(|pane| is_live_agent_pane(pane)) {
        let status = parse_agent_status_for_pane(pane);
        let wait_reason = (!pane.wait_reason.is_empty()).then_some(pane.wait_reason.as_str());
        match badge_state(pane_rollup_level(status, wait_reason), false) {
            BadgeState::Blocked => blocked += 1,
            BadgeState::Working => working += 1,
            BadgeState::Done => done += 1,
            BadgeState::Idle => idle += 1,
        }
    }
    [
        (BadgeState::Blocked, blocked),
        (BadgeState::Working, working),
        (BadgeState::Done, done),
        (BadgeState::Idle, idle),
    ]
}

fn parse_agent_status_for_pane(pane: &PaneSnapshot) -> Option<AgentStatus> {
    let status = parse_agent_status(&pane.status);
    if status == Some(AgentStatus::Running) && pane.started_at.trim().parse::<i64>().is_err() {
        None
    } else {
        status
    }
}

fn parse_agent_status(raw: &str) -> Option<AgentStatus> {
    match raw {
        "running" => Some(AgentStatus::Running),
        "waiting" => Some(AgentStatus::Waiting),
        "idle" => Some(AgentStatus::Idle),
        "error" => Some(AgentStatus::Error),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{ClientMessage, ServerMessage};
    use crate::options::snapshot::{PaneSnapshot, snapshot_format};
    use crate::tmux::mock::MockTmuxRunner;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn pane(agent: &str, status: &str, wait_reason: &str) -> PaneSnapshot {
        PaneSnapshot {
            pane_id: "%1".to_string(),
            current_command: agent.to_string(),
            agent: agent.to_string(),
            status: status.to_string(),
            wait_reason: wait_reason.to_string(),
            started_at: if status == "running" { "100" } else { "" }.to_string(),
            ..PaneSnapshot::default()
        }
    }

    fn pane_in_session(
        session: &str,
        pane_id: &str,
        status: &str,
        wait_reason: &str,
        started_at: &str,
        window_active: bool,
        session_attached: bool,
    ) -> PaneSnapshot {
        PaneSnapshot {
            session: session.to_string(),
            window_id: "@1".to_string(),
            pane_id: pane_id.to_string(),
            current_path: "/tmp".to_string(),
            current_command: "codex".to_string(),
            window_active,
            session_attached,
            is_sidebar: false,
            agent: "codex".to_string(),
            status: status.to_string(),
            wait_reason: wait_reason.to_string(),
            started_at: started_at.to_string(),
            ..PaneSnapshot::default()
        }
    }

    fn snapshot_line(pane: &PaneSnapshot) -> String {
        [
            pane.session.as_str(),
            pane.window_id.as_str(),
            pane.pane_id.as_str(),
            pane.current_path.as_str(),
            pane.current_command.as_str(),
            pane.pane_tty.as_str(),
            pane.pane_pid.as_str(),
            if pane.window_active { "1" } else { "0" },
            if pane.session_attached { "1" } else { "0" },
            if pane.is_sidebar { "1" } else { "0" },
            pane.agent.as_str(),
            pane.status.as_str(),
            pane.prompt.as_str(),
            pane.prompt_source.as_str(),
            pane.wait_reason.as_str(),
            pane.attention.as_str(),
            pane.started_at.as_str(),
            pane.completed_at.as_str(),
            pane.tasks.as_str(),
            pane.subagents.as_str(),
        ]
        .join("\u{1f}")
    }

    #[test]
    fn build_snapshot_ignores_non_agent_panes() {
        let snapshot = build_snapshot(&[
            pane("codex", "running", ""),
            pane("", "", ""),
            pane("claude", "idle", ""),
        ]);
        assert_eq!(snapshot.agent_count, 2);
        assert_eq!(snapshot.rollup, crate::hook::RollupLevel::Running);
    }

    #[test]
    fn build_snapshot_ignores_stale_hook_agent_when_command_is_shell() {
        let mut hook_marked = pane("codex", "running", "");
        hook_marked.current_command = "zsh".to_string();

        let snapshot = build_snapshot(&[hook_marked]);

        assert_eq!(snapshot.agent_count, 0);
        assert!(snapshot.panes.is_empty());
    }

    #[test]
    fn build_snapshot_uses_command_agent_when_hook_options_are_missing() {
        let mut pane = pane("", "", "");
        pane.current_command = "claude".to_string();

        let snapshot = build_snapshot(&[pane]);

        assert_eq!(snapshot.agent_count, 1);
        assert_eq!(snapshot.panes[0].agent, "claude");
    }

    #[test]
    fn permission_waiting_wins_over_idle() {
        let snapshot = build_snapshot(&[
            pane("claude", "idle", ""),
            pane("codex", "waiting", "permission_prompt"),
        ]);
        assert_eq!(snapshot.rollup, crate::hook::RollupLevel::Permission);
    }

    #[test]
    fn render_summary_counts_states_with_markup_and_omits_zero() {
        use crate::daemon::session_badge::BadgeState;
        let badge = crate::config::BadgeConfig::default();
        let counts = [
            (BadgeState::Blocked, 2),
            (BadgeState::Working, 1),
            (BadgeState::Done, 0),
            (BadgeState::Idle, 3),
        ];
        assert_eq!(
            render_summary(&counts, &badge),
            "#[fg=#ff6b6b]▲2#[default] #[fg=#4fd08a]●1#[default] #[fg=#6f6b85]○3#[default]"
        );
    }

    #[test]
    fn render_summary_is_empty_without_agents() {
        let badge = crate::config::BadgeConfig::default();
        let counts = [];
        assert_eq!(render_summary(&counts, &badge), "");
    }

    #[test]
    fn fallback_summary_counts_idle_as_idle_not_done() {
        let counts = summary_counts_for_panes(&[pane_in_session(
            "main", "%1", "idle", "", "", false, false,
        )]);

        assert_eq!(
            render_summary(&counts, &crate::config::BadgeConfig::default()),
            "#[fg=#6f6b85]○1#[default]"
        );
    }

    #[test]
    fn fallback_summary_treats_running_without_started_at_as_idle() {
        let counts = summary_counts_for_panes(&[pane_in_session(
            "main", "%1", "running", "", "", false, false,
        )]);

        assert_eq!(
            render_summary(&counts, &crate::config::BadgeConfig::default()),
            "#[fg=#6f6b85]○1#[default]"
        );
    }

    #[test]
    fn format_attention_abbreviates_wait_and_error_without_more_suffix() {
        assert_eq!(
            format_attention(&[("etl".to_string(), crate::hook::RollupLevel::Waiting, 59)]),
            "▲ etl · wait 59s"
        );
        assert_eq!(
            format_attention(&[("proxy".to_string(), crate::hook::RollupLevel::Error, 60)]),
            "▲ proxy · err 1m"
        );
    }

    #[test]
    fn attention_fallback_excludes_visible_session_and_adds_more_count() {
        let mock = MockTmuxRunner::new();
        let hidden_old = pane_in_session(
            "proxy",
            "%1",
            "waiting",
            "permission_prompt",
            "100",
            false,
            false,
        );
        let hidden_new = pane_in_session(
            "etl",
            "%2",
            "waiting",
            "permission_prompt",
            "200",
            false,
            false,
        );
        let visible = pane_in_session(
            "main",
            "%3",
            "waiting",
            "permission_prompt",
            "50",
            true,
            true,
        );
        let output = [
            snapshot_line(&hidden_old),
            snapshot_line(&hidden_new),
            snapshot_line(&visible),
        ]
        .join("\n");
        let format = snapshot_format();
        mock.stub(&["list-panes", "-a", "-F", &format], &output);

        let text = statusline_attention_fallback(&mock).unwrap();

        assert!(text.contains("▲ proxy · perm"), "{text}");
        assert!(text.contains("+1"), "{text}");
        assert!(!text.contains("main"), "{text}");
    }

    #[test]
    fn daemon_socket_path_prefers_explicit_then_env() {
        let env = BTreeMap::from([(ENV_DAEMON_SOCKET.to_string(), "/tmp/env.sock".to_string())]);
        assert_eq!(
            daemon_socket_path(&env, Some("/tmp/explicit.sock")),
            PathBuf::from("/tmp/explicit.sock")
        );
        assert_eq!(
            daemon_socket_path(&env, None),
            PathBuf::from("/tmp/env.sock")
        );
    }

    #[test]
    fn daemon_socket_path_uses_xdg_runtime_dir_before_tmp_fallback() {
        let env = BTreeMap::from([("XDG_RUNTIME_DIR".to_string(), "/run/user/501".to_string())]);

        assert_eq!(
            daemon_socket_path(&env, None),
            PathBuf::from("/run/user/501/vde-tmux/daemon.sock")
        );
    }

    #[test]
    fn query_statusline_summary_reads_server_response() {
        let socket_path = unique_socket_path();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            let message: ClientMessage = serde_json::from_str(request.trim()).unwrap();
            assert_eq!(
                message,
                ClientMessage::Query {
                    proto: 1,
                    what: crate::daemon::protocol::QueryTarget::Summary
                }
            );
            serde_json::to_writer(
                &mut stream,
                &ServerMessage::Summary {
                    text: "#[fg=green]●1#[default]".to_string(),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let value = query_statusline_summary(&socket_path).unwrap();
        handle.join().unwrap();
        fs::remove_file(socket_path).unwrap();
        assert_eq!(value, "#[fg=green]●1#[default]");
    }

    fn unique_socket_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        PathBuf::from(format!("/tmp/vt-test-{nanos}.sock"))
    }
}

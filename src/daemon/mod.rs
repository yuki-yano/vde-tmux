//! daemon の snapshot 集約と statusline badge。

pub mod lifecycle;
pub mod protocol;
pub mod runtime;
pub mod server;
pub mod session_badge;
pub mod workers;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::protocol::{ClientMessage, ServerMessage};
use crate::hook::{AgentStatus, RollupLevel, pane_rollup_level};
use crate::options::snapshot::{PaneSnapshot, is_live_agent_pane, read_all_panes};
use crate::sidebar::state::SidebarState;
use crate::sidebar::tree::SidebarRow;
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
            let status = parse_agent_status(&pane.status);
            let wait_reason = (!pane.wait_reason.is_empty()).then(|| pane.wait_reason.clone());
            let rollup = pane_rollup_level(status, wait_reason.as_deref());
            AgentPaneSummary {
                pane_id: pane.pane_id.clone(),
                agent: pane.agent.clone(),
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
    }
}

pub fn render_agent_badge(snapshot: &DaemonSnapshot) -> String {
    if snapshot.agent_count == 0 {
        return String::new();
    }
    format!("{}:{}", rollup_label(snapshot.rollup), snapshot.agent_count)
}

pub fn statusline_agent_badge_fallback(runner: &dyn TmuxRunner) -> Result<String> {
    let panes = read_all_panes(runner)?;
    Ok(render_agent_badge(&build_snapshot(&panes)))
}

pub fn statusline_agent_badge(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let socket_path = daemon_socket_path(env, None);
    if socket_path.exists()
        && let Ok(value) = query_statusline_agent_badge(&socket_path)
    {
        return Ok(value);
    }
    statusline_agent_badge_fallback(runner)
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

pub fn query_statusline_agent_badge(socket_path: &Path) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect {}", socket_path.display()))?;
    serde_json::to_writer(
        &mut stream,
        &ClientMessage::Query {
            proto: 1,
            what: crate::daemon::protocol::QueryTarget::Statusline,
        },
    )?;
    stream.write_all(b"\n")?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line)?;
    match serde_json::from_str::<ServerMessage>(line.trim())? {
        ServerMessage::Statusline { agent_badge } => Ok(agent_badge),
        ServerMessage::Ack => bail!("unexpected daemon ack response"),
        ServerMessage::Error { message } => bail!(message),
        ServerMessage::Snapshot { .. } => bail!("unexpected daemon snapshot response"),
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

fn rollup_label(level: RollupLevel) -> &'static str {
    match level {
        RollupLevel::Error => "error",
        RollupLevel::Running => "running",
        RollupLevel::Permission => "permission",
        RollupLevel::Background => "background",
        RollupLevel::Waiting => "waiting",
        RollupLevel::Idle => "idle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{ClientMessage, ServerMessage};
    use crate::options::snapshot::PaneSnapshot;
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
            ..PaneSnapshot::default()
        }
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
    fn build_snapshot_ignores_stale_agent_option_when_command_is_not_agent() {
        let mut stale = pane("codex", "running", "");
        stale.current_command = "zsh".to_string();

        let snapshot = build_snapshot(&[stale]);

        assert_eq!(snapshot.agent_count, 0);
        assert_eq!(render_agent_badge(&snapshot), "");
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
    fn render_agent_badge_is_empty_without_agents() {
        let snapshot = build_snapshot(&[pane("", "", "")]);
        assert_eq!(render_agent_badge(&snapshot), "");
    }

    #[test]
    fn render_agent_badge_includes_rollup_and_count() {
        let snapshot = build_snapshot(&[pane("codex", "running", "")]);
        assert_eq!(render_agent_badge(&snapshot), "running:1");
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
    fn query_statusline_agent_badge_reads_server_response() {
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
                    what: crate::daemon::protocol::QueryTarget::Statusline
                }
            );
            serde_json::to_writer(
                &mut stream,
                &ServerMessage::Statusline {
                    agent_badge: "running:1".to_string(),
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let value = query_statusline_agent_badge(&socket_path).unwrap();
        handle.join().unwrap();
        fs::remove_file(socket_path).unwrap();
        assert_eq!(value, "running:1");
    }

    fn unique_socket_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        PathBuf::from(format!("/tmp/vt-test-{nanos}.sock"))
    }
}

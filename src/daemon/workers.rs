use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::daemon::runtime::DaemonEvent;
use crate::detect::{demote_stale_running, detect_codex_wait_reason};
use crate::git::{GitRunner, SystemGitRunner, collect_git_badges};
use crate::hook::AgentStatus;
use crate::options::snapshot::{PaneSnapshot, effective_agent, is_live_agent_pane, read_all_panes};
use crate::sidebar::layout::jump_to_pane;
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

pub trait WorkerIo: Send + Sync + 'static {
    fn read_panes(&self) -> Result<Vec<PaneSnapshot>>;
    fn capture_tail(&self, pane_id: &str) -> Result<String>;
    fn jump_to_pane(&self, pane_id: &str) -> Result<()>;
    fn preview_pane(&self, pane_id: &str, history_lines: u32) -> Result<()>;
    fn unset_pane_option(&self, pane_id: &str, key: &str) -> Result<()>;
    fn set_session_option(&self, session: &str, key: &str, value: &str) -> Result<()>;
    fn unset_session_option(&self, session: &str, key: &str) -> Result<()>;
    fn set_global_option(&self, key: &str, value: &str) -> Result<()>;
    fn unset_global_option(&self, key: &str) -> Result<()>;
    fn run_notify(&self, command: &str, pane_id: &str, agent: &str, state: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct SystemWorkerIo {
    runner: SystemTmuxRunner,
}

impl SystemWorkerIo {
    pub fn new(runner: SystemTmuxRunner) -> Self {
        Self { runner }
    }
}

impl WorkerIo for SystemWorkerIo {
    fn read_panes(&self) -> Result<Vec<PaneSnapshot>> {
        read_all_panes(&self.runner)
    }

    fn capture_tail(&self, pane_id: &str) -> Result<String> {
        self.runner
            .run(&["capture-pane", "-p", "-S", "-80", "-t", pane_id])
    }

    fn jump_to_pane(&self, pane_id: &str) -> Result<()> {
        jump_to_pane(&self.runner, pane_id)
    }

    fn preview_pane(&self, pane_id: &str, history_lines: u32) -> Result<()> {
        let env = std::env::vars().collect();
        crate::sidebar::preview::open_preview_floating_pane(
            &self.runner,
            &env,
            pane_id,
            history_lines,
        )
    }

    fn unset_pane_option(&self, pane_id: &str, key: &str) -> Result<()> {
        crate::options::unset_pane_option(&self.runner, pane_id, key)
    }

    fn set_session_option(&self, session: &str, key: &str, value: &str) -> Result<()> {
        crate::options::set_session_option(&self.runner, session, key, value)
    }

    fn unset_session_option(&self, session: &str, key: &str) -> Result<()> {
        crate::options::unset_session_option(&self.runner, session, key)
    }

    fn set_global_option(&self, key: &str, value: &str) -> Result<()> {
        crate::options::set_global_option(&self.runner, key, value)
    }

    fn unset_global_option(&self, key: &str) -> Result<()> {
        crate::options::unset_global_option(&self.runner, key)
    }

    fn run_notify(&self, command: &str, pane_id: &str, agent: &str, state: &str) -> Result<()> {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env("VDE_PANE_ID", pane_id)
            .env("VDE_AGENT", agent)
            .env("VDE_BADGE_STATE", state)
            .spawn()?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct LatestPanes {
    panes: Mutex<Vec<PaneSnapshot>>,
}

impl LatestPanes {
    pub fn store(&self, panes: Vec<PaneSnapshot>) {
        *self.panes.lock().expect("latest panes poisoned") = panes;
    }

    pub fn load(&self) -> Vec<PaneSnapshot> {
        self.panes.lock().expect("latest panes poisoned").clone()
    }
}

pub fn start_tmux_worker(
    io: Arc<dyn WorkerIo>,
    latest_panes: Arc<LatestPanes>,
    tx: Sender<DaemonEvent>,
    poll: Duration,
    stale_threshold_seconds: i64,
) {
    thread::spawn(move || {
        loop {
            if let Err(error) = poll_tmux_once_with_latest(
                io.clone(),
                latest_panes.clone(),
                tx.clone(),
                stale_threshold_seconds,
            ) {
                eprintln!("[vde-tmux] daemon tmux worker error: {error:#}");
            }
            thread::sleep(poll);
        }
    });
}

pub fn poll_tmux_once(
    io: Arc<dyn WorkerIo>,
    tx: Sender<DaemonEvent>,
    stale_threshold_seconds: i64,
) -> Result<()> {
    let latest = Arc::new(LatestPanes::default());
    poll_tmux_once_with_latest(io, latest, tx, stale_threshold_seconds)
}

fn poll_tmux_once_with_latest(
    io: Arc<dyn WorkerIo>,
    latest_panes: Arc<LatestPanes>,
    tx: Sender<DaemonEvent>,
    stale_threshold_seconds: i64,
) -> Result<()> {
    let panes = read_panes_with_detection(io.as_ref(), stale_threshold_seconds)?;
    latest_panes.store(panes.clone());
    tx.send(DaemonEvent::PanesUpdated(panes))?;
    Ok(())
}

pub fn read_panes_with_detection(
    io: &dyn WorkerIo,
    stale_threshold_seconds: i64,
) -> Result<Vec<PaneSnapshot>> {
    let now = now_epoch();
    Ok(io
        .read_panes()?
        .into_iter()
        .map(|pane| apply_capture_detection(io, pane, now, stale_threshold_seconds))
        .collect())
}

pub fn start_git_worker(
    git: Arc<dyn GitRunner>,
    latest_panes: Arc<LatestPanes>,
    tx: Sender<DaemonEvent>,
    poll: Duration,
) {
    thread::spawn(move || {
        loop {
            if let Err(error) = poll_git_once(git.clone(), latest_panes.clone(), tx.clone()) {
                eprintln!("[vde-tmux] daemon git worker error: {error:#}");
            }
            thread::sleep(poll);
        }
    });
}

pub fn poll_git_once(
    git: Arc<dyn GitRunner>,
    latest_panes: Arc<LatestPanes>,
    tx: Sender<DaemonEvent>,
) -> Result<()> {
    let badges = collect_git_badges(git.as_ref(), &latest_panes.load());
    tx.send(DaemonEvent::GitStatusUpdated(badges))?;
    Ok(())
}

pub fn system_git_runner(timeout: Duration) -> SystemGitRunner {
    SystemGitRunner::new(timeout)
}

pub fn apply_capture_detection(
    io: &dyn WorkerIo,
    mut pane: PaneSnapshot,
    now_epoch: i64,
    stale_threshold_seconds: i64,
) -> PaneSnapshot {
    if !is_live_agent_pane(&pane) {
        return pane;
    }
    if pane.agent.trim().is_empty()
        && let Some(agent) = effective_agent(&pane)
    {
        pane.agent = agent.to_string();
    }
    let mut observed_activity_epoch = None;
    let running_has_started_at =
        pane.status == "running" && pane.started_at.trim().parse::<i64>().is_ok();
    let has_hook_wait_reason = !pane.wait_reason.trim().is_empty();
    let status_allows_capture_detection = pane.status.trim().is_empty() || pane.status == "running";
    let should_detect_wait_reason = !has_hook_wait_reason && status_allows_capture_detection;
    let should_capture = should_detect_wait_reason || pane.status == "running";
    if should_capture && let Ok(tail) = io.capture_tail(&pane.pane_id) {
        if should_detect_wait_reason && let Some(wait_reason) = detect_codex_wait_reason(&tail) {
            pane.status = "waiting".to_string();
            pane.wait_reason = wait_reason.to_string();
        } else if running_has_started_at && !tail.trim().is_empty() {
            observed_activity_epoch = Some(now_epoch);
        }
    }
    if pane.status == "running" && !running_has_started_at {
        pane.status = "idle".to_string();
        pane.wait_reason.clear();
    }
    let last_activity = observed_activity_epoch
        .or_else(|| pane.completed_at.parse::<i64>().ok())
        .or_else(|| pane.started_at.parse::<i64>().ok())
        .unwrap_or(now_epoch);
    let status = parse_status(&pane.status);
    if demote_stale_running(status, last_activity, now_epoch, stale_threshold_seconds)
        == Some(AgentStatus::Idle)
    {
        pane.status = "idle".to_string();
        pane.wait_reason.clear();
    }
    pane
}

fn parse_status(raw: &str) -> Option<AgentStatus> {
    match raw {
        "running" => Some(AgentStatus::Running),
        "waiting" => Some(AgentStatus::Waiting),
        "idle" => Some(AgentStatus::Idle),
        "error" => Some(AgentStatus::Error),
        _ => None,
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::runtime::DaemonEvent;
    use crate::git::GitRunner;
    use crate::options::snapshot::PaneSnapshot;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, mpsc};

    #[derive(Default)]
    struct MockWorkerIo {
        panes: Mutex<Vec<PaneSnapshot>>,
        captures: Mutex<BTreeMap<String, String>>,
        jumps: Mutex<Vec<String>>,
    }

    impl WorkerIo for MockWorkerIo {
        fn read_panes(&self) -> anyhow::Result<Vec<PaneSnapshot>> {
            Ok(self.panes.lock().unwrap().clone())
        }

        fn capture_tail(&self, pane_id: &str) -> anyhow::Result<String> {
            Ok(self
                .captures
                .lock()
                .unwrap()
                .get(pane_id)
                .cloned()
                .unwrap_or_default())
        }

        fn jump_to_pane(&self, pane_id: &str) -> anyhow::Result<()> {
            self.jumps.lock().unwrap().push(pane_id.to_string());
            Ok(())
        }

        fn preview_pane(&self, _pane_id: &str, _history_lines: u32) -> anyhow::Result<()> {
            Ok(())
        }

        fn unset_pane_option(&self, _pane_id: &str, _key: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_session_option(
            &self,
            _session: &str,
            _key: &str,
            _value: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn unset_session_option(&self, _session: &str, _key: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_global_option(&self, _key: &str, _value: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn unset_global_option(&self, _key: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn run_notify(
            &self,
            _command: &str,
            _pane_id: &str,
            _agent: &str,
            _state: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct MockGitRunner {
        branch: String,
        counts: String,
    }

    impl GitRunner for MockGitRunner {
        fn run(&self, _cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            match args {
                ["branch", "--show-current"] => Ok(self.branch.clone()),
                ["rev-list", "--left-right", "--count", "@{upstream}...HEAD"] => {
                    Ok(self.counts.clone())
                }
                _ => anyhow::bail!("unexpected git args: {args:?}"),
            }
        }
    }

    fn pane(pane_id: &str, agent: &str, status: &str) -> PaneSnapshot {
        PaneSnapshot {
            session: "main".to_string(),
            window_id: "@1".to_string(),
            pane_id: pane_id.to_string(),
            current_path: "/tmp/app".to_string(),
            current_command: agent.to_string(),
            agent: agent.to_string(),
            status: status.to_string(),
            ..PaneSnapshot::default()
        }
    }

    #[test]
    fn tmux_worker_sends_panes_updated() {
        let io = Arc::new(MockWorkerIo::default());
        io.panes
            .lock()
            .unwrap()
            .push(pane("%1", "codex", "running"));
        let (tx, rx) = mpsc::channel();

        poll_tmux_once(io, tx, 100).unwrap();

        let DaemonEvent::PanesUpdated(panes) = rx.recv().unwrap() else {
            panic!("expected panes updated");
        };
        assert_eq!(panes[0].pane_id, "%1");
    }

    #[test]
    fn git_worker_merges_badges_without_blocking_tmux_poll() {
        let panes = Arc::new(LatestPanes::default());
        panes.store(vec![pane("%1", "codex", "running")]);
        let (tx, rx) = mpsc::channel();
        let git = Arc::new(MockGitRunner {
            branch: "main\n".to_string(),
            counts: "0\t1\n".to_string(),
        });

        poll_git_once(git, panes, tx).unwrap();

        let DaemonEvent::GitStatusUpdated(badges) = rx.recv().unwrap() else {
            panic!("expected git status updated");
        };
        assert_eq!(badges["/tmp/app"].branch, "main");
    }

    #[test]
    fn tmux_worker_applies_capture_pane_detection() {
        let io = Arc::new(MockWorkerIo::default());
        let mut pane = pane("%1", "", "");
        pane.current_command = "codex".to_string();
        io.panes.lock().unwrap().push(pane);
        io.captures.lock().unwrap().insert(
            "%1".to_string(),
            "? Allow command to run?\n  y) yes\n  n) no\n".to_string(),
        );
        let (tx, rx) = mpsc::channel();

        poll_tmux_once(io, tx, 100).unwrap();

        let DaemonEvent::PanesUpdated(panes) = rx.recv().unwrap() else {
            panic!("expected panes updated");
        };
        assert_eq!(panes[0].status, "waiting");
        assert_eq!(panes[0].wait_reason, "permission_prompt");
    }

    #[test]
    fn tmux_worker_does_not_infer_running_from_non_empty_tail_without_hook_status() {
        let io = Arc::new(MockWorkerIo::default());
        let mut pane = pane("%1", "", "");
        pane.current_command = "claude".to_string();
        io.panes.lock().unwrap().push(pane);
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Claude is working\n".to_string());
        let (tx, rx) = mpsc::channel();

        poll_tmux_once(io, tx, 100).unwrap();

        let DaemonEvent::PanesUpdated(panes) = rx.recv().unwrap() else {
            panic!("expected panes updated");
        };
        assert_eq!(panes[0].agent, "claude");
        assert_eq!(panes[0].status, "");
    }

    #[test]
    fn tmux_worker_detects_claude_permission_prompt_without_hook_options() {
        let io = Arc::new(MockWorkerIo::default());
        let mut pane = pane("%1", "", "");
        pane.current_command = "claude".to_string();
        io.panes.lock().unwrap().push(pane);
        io.captures.lock().unwrap().insert(
            "%1".to_string(),
            "Claude needs your permission to use Bash\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n"
                .to_string(),
        );
        let (tx, rx) = mpsc::channel();

        poll_tmux_once(io, tx, 100).unwrap();

        let DaemonEvent::PanesUpdated(panes) = rx.recv().unwrap() else {
            panic!("expected panes updated");
        };
        assert_eq!(panes[0].agent, "claude");
        assert_eq!(panes[0].status, "waiting");
        assert_eq!(panes[0].wait_reason, "permission_prompt");
    }

    #[test]
    fn running_status_without_wait_reason_uses_capture_prompt_detection() {
        let io = MockWorkerIo::default();
        let mut active = pane("%1", "codex", "running");
        active.started_at = "990".to_string();
        io.captures.lock().unwrap().insert(
            "%1".to_string(),
            "Question 1/1 (1 unanswered)\n今の気分に一番近いものはどれですか？\n› 1. 集中したい\n"
                .to_string(),
        );

        let pane = apply_capture_detection(&io, active, 1_000, 30);

        assert_eq!(pane.status, "waiting");
        assert_eq!(pane.wait_reason, "codex_question_prompt");
    }

    #[test]
    fn stale_running_is_demoted_in_snapshot_only() {
        let io = Arc::new(MockWorkerIo::default());
        let mut stale = pane("%1", "codex", "running");
        stale.started_at = "100".to_string();
        io.panes.lock().unwrap().push(stale);
        let (tx, rx) = mpsc::channel();

        poll_tmux_once(io, tx, 30).unwrap();

        let DaemonEvent::PanesUpdated(panes) = rx.recv().unwrap() else {
            panic!("expected panes updated");
        };
        assert_eq!(panes[0].status, "idle");
    }

    #[test]
    fn running_pane_with_captured_activity_is_not_demoted_to_idle() {
        let io = MockWorkerIo::default();
        let mut active = pane("%1", "claude", "running");
        active.started_at = "100".to_string();
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Claude is still working\n".to_string());

        let pane = apply_capture_detection(&io, active, 1_000, 30);

        assert_eq!(pane.status, "running");
    }

    #[test]
    fn running_without_started_at_is_demoted_even_with_non_empty_tail() {
        let io = MockWorkerIo::default();
        let active = pane("%1", "codex", "running");
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Codex is ready for input\n".to_string());

        let pane = apply_capture_detection(&io, active, 1_000, 30);

        assert_eq!(pane.status, "idle");
    }
}

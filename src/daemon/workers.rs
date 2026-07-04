use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::daemon::runtime::DaemonEvent;
use crate::detect::{demote_stale_running, detect_codex_wait_reason};
use crate::git::{GitRunner, SystemGitRunner, collect_git_badges};
use crate::hook::AgentStatus;
use crate::options::snapshot::{PaneSnapshot, read_all_panes};
use crate::sidebar::layout::jump_to_pane;
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

pub trait WorkerIo: Send + Sync + 'static {
    fn read_panes(&self) -> Result<Vec<PaneSnapshot>>;
    fn capture_tail(&self, pane_id: &str) -> Result<String>;
    fn jump_to_pane(&self, pane_id: &str) -> Result<()>;
    fn set_session_option(&self, session: &str, key: &str, value: &str) -> Result<()>;
    fn unset_session_option(&self, session: &str, key: &str) -> Result<()>;
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

    fn set_session_option(&self, session: &str, key: &str, value: &str) -> Result<()> {
        crate::options::set_session_option(&self.runner, session, key, value)
    }

    fn unset_session_option(&self, session: &str, key: &str) -> Result<()> {
        crate::options::unset_session_option(&self.runner, session, key)
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
    let now = now_epoch();
    let panes = io
        .read_panes()?
        .into_iter()
        .map(|pane| apply_capture_detection(io.as_ref(), pane, now, stale_threshold_seconds))
        .collect::<Vec<_>>();
    latest_panes.store(panes.clone());
    tx.send(DaemonEvent::PanesUpdated(panes))?;
    Ok(())
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
    if pane.agent.trim().is_empty() || pane.is_sidebar {
        return pane;
    }
    let should_capture = pane.wait_reason.trim().is_empty() || pane.status == "running";
    if should_capture
        && let Ok(tail) = io.capture_tail(&pane.pane_id)
        && let Some(wait_reason) = detect_codex_wait_reason(&tail)
    {
        pane.status = "waiting".to_string();
        pane.wait_reason = wait_reason.to_string();
    }
    let last_activity = pane
        .completed_at
        .parse::<i64>()
        .ok()
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
        io.panes
            .lock()
            .unwrap()
            .push(pane("%1", "codex", "running"));
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
}

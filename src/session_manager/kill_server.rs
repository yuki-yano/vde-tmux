use std::collections::BTreeSet;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::SessionManagerKillConfig;
use crate::daemon::lifecycle::TmuxServerIncarnation;
use crate::tmux::TmuxRunner;

const PANE_PROCESS_FORMAT: &str =
    "#{pane_id}\u{1f}#{pane_pid}\u{1f}#{pane_tty}\u{1f}#{pane_current_command}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneProcess {
    pane_id: String,
    pane_pid: i32,
    pane_tty: String,
    pane_current_command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessIdentity {
    pid: i32,
    pgid: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerPresence {
    Present,
    Absent,
}

pub trait KillServerOps {
    fn resolve_server(&mut self) -> Result<TmuxServerIncarnation>;
    fn list_panes(&mut self) -> Result<Vec<PaneProcess>>;
    fn process_groups(&mut self, panes: &[PaneProcess]) -> Result<BTreeSet<i32>>;
    fn self_process(&mut self) -> Result<ProcessIdentity>;
    fn process_group_for_pid(&mut self, pid: i32) -> Result<Option<i32>>;
    fn disable_daemon(&mut self, server: &TmuxServerIncarnation) -> Result<()>;
    fn send_ctrl_c(&mut self, pane_id: &str) -> Result<()>;
    fn send_term(&mut self, pgid: i32) -> Result<()>;
    fn send_kill(&mut self, pgid: i32) -> Result<()>;
    fn process_group_alive(&mut self, pgid: i32) -> Result<bool>;
    fn wait(&mut self, duration: Duration);
    fn server_presence(&mut self, server: &TmuxServerIncarnation) -> Result<ServerPresence>;
    fn kill_server(&mut self) -> Result<()>;
}

type DisableDaemonFn<'a> = dyn FnMut(&TmuxServerIncarnation) -> Result<()> + 'a;

pub struct SystemKillServerOps<'a> {
    runner: &'a dyn TmuxRunner,
    disable_daemon: Box<DisableDaemonFn<'a>>,
}

impl<'a> SystemKillServerOps<'a> {
    pub fn new(
        runner: &'a dyn TmuxRunner,
        disable_daemon: impl FnMut(&TmuxServerIncarnation) -> Result<()> + 'a,
    ) -> Self {
        Self {
            runner,
            disable_daemon: Box::new(disable_daemon),
        }
    }
}

impl KillServerOps for SystemKillServerOps<'_> {
    fn resolve_server(&mut self) -> Result<TmuxServerIncarnation> {
        TmuxServerIncarnation::resolve_from_runner(self.runner)
    }

    fn list_panes(&mut self) -> Result<Vec<PaneProcess>> {
        parse_pane_processes(
            &self
                .runner
                .run(&["list-panes", "-a", "-F", PANE_PROCESS_FORMAT])?,
        )
    }

    fn process_groups(&mut self, panes: &[PaneProcess]) -> Result<BTreeSet<i32>> {
        let output = Command::new("ps")
            .args(["-axo", "pid=,pgid=,tty="])
            .output()
            .context("failed to query the process table")?;
        if !output.status.success() {
            bail!("process table query failed with exit {}", output.status);
        }
        let output = String::from_utf8(output.stdout).context("process table was not utf-8")?;
        collect_process_groups_from_table(&output, panes)
    }

    fn self_process(&mut self) -> Result<ProcessIdentity> {
        let pid = i32::try_from(std::process::id()).context("self PID does not fit in i32")?;
        let pgid =
            get_process_group(pid)?.ok_or_else(|| anyhow::anyhow!("self process is gone"))?;
        validate_process_identity(pid, pgid, "session-manager")?;
        Ok(ProcessIdentity { pid, pgid })
    }

    fn process_group_for_pid(&mut self, pid: i32) -> Result<Option<i32>> {
        get_process_group(pid)
    }

    fn disable_daemon(&mut self, server: &TmuxServerIncarnation) -> Result<()> {
        (self.disable_daemon)(server)
    }

    fn send_ctrl_c(&mut self, pane_id: &str) -> Result<()> {
        self.runner.run(&["send-keys", "-t", pane_id, "C-c"])?;
        Ok(())
    }

    fn send_term(&mut self, pgid: i32) -> Result<()> {
        signal_process_group(pgid, libc::SIGTERM, "SIGTERM")
    }

    fn send_kill(&mut self, pgid: i32) -> Result<()> {
        signal_process_group(pgid, libc::SIGKILL, "SIGKILL")
    }

    fn process_group_alive(&mut self, pgid: i32) -> Result<bool> {
        validate_pgid(pgid)?;
        let result = unsafe { libc::kill(-pgid, 0) };
        if result == 0 {
            return Ok(true);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => bail!("permission denied while checking process group {pgid}"),
            Some(errno) => bail!("failed to check process group {pgid}: errno {errno}"),
            None => bail!("failed to check process group {pgid}"),
        }
    }

    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn server_presence(&mut self, server: &TmuxServerIncarnation) -> Result<ServerPresence> {
        server_presence_for_runner(self.runner, server)
    }

    fn kill_server(&mut self) -> Result<()> {
        self.runner.run(&["kill-server"])?;
        Ok(())
    }
}

pub(crate) fn server_presence_for_runner(
    runner: &dyn TmuxRunner,
    server: &TmuxServerIncarnation,
) -> Result<ServerPresence> {
    let original_metadata = match std::fs::symlink_metadata(&server.socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ServerPresence::Absent);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect tmux socket {}",
                    server.socket_path.display()
                )
            });
        }
    };
    let actual = match TmuxServerIncarnation::resolve_from_runner(runner) {
        Ok(actual) => actual,
        Err(error) => {
            let pid = i32::try_from(server.identity.pid)
                .context("tmux server PID does not fit in i32")?;
            if !process_alive(pid)? {
                remove_verified_stale_tmux_socket(&server.socket_path, &original_metadata)?;
                return Ok(ServerPresence::Absent);
            }
            return Err(error).context("failed to verify the running tmux server identity");
        }
    };
    if actual != *server {
        bail!(
            "tmux server incarnation changed: expected {}, received {}",
            server.hash,
            actual.hash
        );
    }
    Ok(ServerPresence::Present)
}

fn remove_verified_stale_tmux_socket(
    path: &std::path::Path,
    expected: &std::fs::Metadata,
) -> Result<()> {
    let actual = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to recheck stale tmux socket"),
    };
    if !actual.file_type().is_socket() {
        bail!("refusing to remove non-socket tmux path {}", path.display());
    }
    if actual.uid() != unsafe { libc::geteuid() } {
        bail!("refusing to remove tmux socket owned by another user");
    }
    if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        bail!("refusing to remove replaced tmux socket {}", path.display());
    }
    std::fs::remove_file(path)
        .with_context(|| format!("failed to remove stale tmux socket {}", path.display()))
}

#[derive(Debug, Default)]
struct CleanupProgress {
    daemon_disabled: bool,
    ctrl_c_sent: usize,
    term_sent: usize,
    kill_sent: usize,
}

impl CleanupProgress {
    fn failure(&self, stage: &str, error: anyhow::Error) -> anyhow::Error {
        anyhow::anyhow!(
            "Kill Server cleanup failed during {stage}; tmux server was not killed; partial state: daemon_disabled={}, ctrl_c_sent={}, term_sent={}, kill_sent={}: {error:#}",
            self.daemon_disabled,
            self.ctrl_c_sent,
            self.term_sent,
            self.kill_sent,
        )
    }
}

pub fn clean_kill_server(
    ops: &mut dyn KillServerOps,
    config: &SessionManagerKillConfig,
) -> Result<()> {
    let mut progress = CleanupProgress::default();
    let server = ops
        .resolve_server()
        .context("failed to identify tmux server")?;
    let panes = ops
        .list_panes()
        .context("failed to query all tmux panes before cleanup")?;
    validate_panes(&panes)?;
    let self_process = ops
        .self_process()
        .context("failed to identify session-manager process")?;
    let server_pid = i32::try_from(server.identity.pid).context("tmux server PID is invalid")?;
    let server_pgid = ops
        .process_group_for_pid(server_pid)
        .context("failed to identify tmux server process group")?;
    if let Some(pgid) = server_pgid {
        validate_process_identity(server_pid, pgid, "tmux server")?;
    }
    let mut initial_groups = ops
        .process_groups(&panes)
        .context("failed to collect initial pane process groups")?;
    initial_groups.remove(&self_process.pgid);
    if let Some(pgid) = server_pgid {
        initial_groups.remove(&pgid);
    }
    validate_target_groups(&initial_groups, self_process.pgid, server_pgid)?;

    if let Err(error) = ops.disable_daemon(&server) {
        return Err(progress.failure("daemon disable", error));
    }
    progress.daemon_disabled = true;

    if config.send_ctrl_c {
        for pane in &panes {
            if let Err(error) = ops.send_ctrl_c(&pane.pane_id) {
                return Err(progress.failure("pane Ctrl-C", error));
            }
            progress.ctrl_c_sent += 1;
        }
        ops.wait(Duration::from_millis(config.term_wait_ms));
    }

    let mut groups = ops
        .process_groups(&panes)
        .map_err(|error| progress.failure("pane process-group refresh", error))?;
    groups.remove(&self_process.pgid);
    if let Some(pgid) = server_pgid {
        groups.remove(&pgid);
    }
    validate_target_groups(&groups, self_process.pgid, server_pgid)
        .map_err(|error| progress.failure("pane process-group validation", error))?;

    for pgid in &groups {
        if let Err(error) = ops.send_term(*pgid) {
            return Err(progress.failure("SIGTERM", error));
        }
        progress.term_sent += 1;
    }
    if !groups.is_empty() {
        ops.wait(Duration::from_millis(config.term_wait_ms));
    }

    let mut survivors = BTreeSet::new();
    for pgid in &groups {
        match ops.process_group_alive(*pgid) {
            Ok(true) => {
                survivors.insert(*pgid);
            }
            Ok(false) => {}
            Err(error) => return Err(progress.failure("post-SIGTERM liveness check", error)),
        }
    }
    for pgid in &survivors {
        if let Err(error) = ops.send_kill(*pgid) {
            return Err(progress.failure("SIGKILL", error));
        }
        progress.kill_sent += 1;
    }
    if !survivors.is_empty() {
        ops.wait(Duration::from_millis(config.kill_wait_ms));
    }
    for pgid in &survivors {
        match ops.process_group_alive(*pgid) {
            Ok(false) => {}
            Ok(true) => {
                return Err(progress.failure(
                    "post-SIGKILL liveness check",
                    anyhow::anyhow!("process group {pgid} is still alive"),
                ));
            }
            Err(error) => return Err(progress.failure("post-SIGKILL liveness check", error)),
        }
    }

    match ops.server_presence(&server) {
        Ok(ServerPresence::Absent) => return Ok(()),
        Ok(ServerPresence::Present) => {}
        Err(error) => return Err(progress.failure("tmux server identity verification", error)),
    }
    if let Err(error) = ops.kill_server() {
        return match ops.server_presence(&server) {
            Ok(ServerPresence::Absent) => Ok(()),
            Ok(ServerPresence::Present) => Err(progress.failure("tmux kill-server", error)),
            Err(verify_error) => Err(progress.failure(
                "tmux kill-server absence verification",
                anyhow::anyhow!("{error:#}; verification failed: {verify_error:#}"),
            )),
        };
    }
    Ok(())
}

fn parse_pane_processes(output: &str) -> Result<Vec<PaneProcess>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = line.split('\u{1f}').collect::<Vec<_>>();
            if fields.len() != 4 {
                bail!("tmux returned invalid pane process data");
            }
            let pane_pid = fields[1]
                .parse::<i32>()
                .context("tmux returned an invalid pane PID")?;
            Ok(PaneProcess {
                pane_id: fields[0].to_string(),
                pane_pid,
                pane_tty: normalize_tty(fields[2]),
                pane_current_command: fields[3].to_string(),
            })
        })
        .collect()
}

fn validate_panes(panes: &[PaneProcess]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for pane in panes {
        if !pane.pane_id.starts_with('%') || pane.pane_id[1..].parse::<u64>().is_err() {
            bail!("invalid pane id {:?}", pane.pane_id);
        }
        if !ids.insert(&pane.pane_id) {
            bail!("duplicate pane id {:?}", pane.pane_id);
        }
        if pane.pane_pid <= 1 {
            bail!("invalid pane PID {} for {}", pane.pane_pid, pane.pane_id);
        }
        if pane.pane_tty.contains(char::is_whitespace) {
            bail!("invalid pane TTY {:?} for {}", pane.pane_tty, pane.pane_id);
        }
        if pane.pane_current_command.contains(['\n', '\r', '\u{1f}']) {
            bail!("invalid pane command for {}", pane.pane_id);
        }
    }
    Ok(())
}

fn normalize_tty(tty: &str) -> String {
    let tty = tty.trim();
    if matches!(tty, "" | "?" | "-") {
        return String::new();
    }
    tty.strip_prefix("/dev/").unwrap_or(tty).to_string()
}

fn collect_process_groups_from_table(output: &str, panes: &[PaneProcess]) -> Result<BTreeSet<i32>> {
    let pane_pids = panes
        .iter()
        .map(|pane| pane.pane_pid)
        .collect::<BTreeSet<_>>();
    let pane_ttys = panes
        .iter()
        .filter_map(|pane| (!pane.pane_tty.is_empty()).then_some(pane.pane_tty.as_str()))
        .collect::<BTreeSet<_>>();
    let mut groups = BTreeSet::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 {
            bail!("invalid process table row {line:?}");
        }
        let pid = fields[0]
            .parse::<i32>()
            .with_context(|| format!("invalid process table PID in {line:?}"))?;
        let pgid = fields[1]
            .parse::<i32>()
            .with_context(|| format!("invalid process table PGID in {line:?}"))?;
        let tty = normalize_tty(fields[2]);
        if pane_pids.contains(&pid) || (!tty.is_empty() && pane_ttys.contains(tty.as_str())) {
            validate_process_identity(pid, pgid, "pane workload")?;
            groups.insert(pgid);
        }
    }
    Ok(groups)
}

fn validate_process_identity(pid: i32, pgid: i32, label: &str) -> Result<()> {
    if pid <= 1 {
        bail!("refusing invalid {label} PID {pid}");
    }
    validate_pgid(pgid).with_context(|| format!("invalid {label} process group"))
}

fn validate_pgid(pgid: i32) -> Result<()> {
    if pgid <= 1 {
        bail!("refusing unsafe process group {pgid}");
    }
    Ok(())
}

fn validate_target_groups(
    groups: &BTreeSet<i32>,
    self_pgid: i32,
    server_pgid: Option<i32>,
) -> Result<()> {
    for pgid in groups {
        validate_pgid(*pgid)?;
        if *pgid == self_pgid {
            bail!("pane workload includes the session-manager process group {pgid}");
        }
        if server_pgid == Some(*pgid) {
            bail!("pane workload includes the tmux server process group {pgid}");
        }
    }
    Ok(())
}

fn get_process_group(pid: i32) -> Result<Option<i32>> {
    if pid <= 1 {
        bail!("refusing invalid PID {pid}");
    }
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid > 1 {
        return Ok(Some(pgid));
    }
    if pgid == 0 || pgid == 1 {
        bail!("refusing unsafe process group {pgid} for PID {pid}");
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(None),
        Some(errno) => bail!("failed to resolve process group for PID {pid}: errno {errno}"),
        None => bail!("failed to resolve process group for PID {pid}"),
    }
}

fn process_alive(pid: i32) -> Result<bool> {
    if pid <= 1 {
        bail!("refusing invalid PID {pid}");
    }
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return Ok(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        Some(errno) => bail!("failed to check PID {pid}: errno {errno}"),
        None => bail!("failed to check PID {pid}"),
    }
}

fn signal_process_group(pgid: i32, signal: i32, label: &str) -> Result<()> {
    validate_pgid(pgid)?;
    let result = unsafe { libc::kill(-pgid, signal) };
    if result == 0 {
        return Ok(());
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        Some(libc::EPERM) => bail!("permission denied sending {label} to process group {pgid}"),
        Some(errno) => bail!("failed sending {label} to process group {pgid}: errno {errno}"),
        None => bail!("failed sending {label} to process group {pgid}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use super::*;
    use crate::daemon::topology::ServerIdentity;

    struct MockOps {
        events: Vec<String>,
        panes: Vec<PaneProcess>,
        groups: VecDeque<Result<BTreeSet<i32>>>,
        alive: BTreeMap<i32, VecDeque<Result<bool>>>,
        self_process: ProcessIdentity,
        server_pgid: Option<i32>,
        disable_error: Option<anyhow::Error>,
        term_error: Option<anyhow::Error>,
        presence: VecDeque<Result<ServerPresence>>,
        kill_server_error: Option<anyhow::Error>,
    }

    impl Default for MockOps {
        fn default() -> Self {
            Self {
                events: Vec::new(),
                panes: vec![PaneProcess {
                    pane_id: "%1".to_string(),
                    pane_pid: 101,
                    pane_tty: "ttys001".to_string(),
                    pane_current_command: "zsh".to_string(),
                }],
                groups: VecDeque::from([
                    Ok(BTreeSet::from([201, 202])),
                    Ok(BTreeSet::from([201, 202])),
                ]),
                alive: BTreeMap::from([
                    (201, VecDeque::from([Ok(false)])),
                    (202, VecDeque::from([Ok(true), Ok(false)])),
                ]),
                self_process: ProcessIdentity {
                    pid: 900,
                    pgid: 900,
                },
                server_pgid: Some(800),
                disable_error: None,
                term_error: None,
                presence: VecDeque::from([Ok(ServerPresence::Present)]),
                kill_server_error: None,
            }
        }
    }

    impl KillServerOps for MockOps {
        fn resolve_server(&mut self) -> Result<TmuxServerIncarnation> {
            self.events.push("resolve-server".to_string());
            Ok(TmuxServerIncarnation {
                socket_path: "/tmp/test-tmux".into(),
                identity: ServerIdentity {
                    pid: 800,
                    start_time: 1,
                },
                hash: "server-hash".to_string(),
            })
        }

        fn list_panes(&mut self) -> Result<Vec<PaneProcess>> {
            self.events.push("list-panes".to_string());
            Ok(self.panes.clone())
        }

        fn process_groups(&mut self, _panes: &[PaneProcess]) -> Result<BTreeSet<i32>> {
            self.events.push("process-groups".to_string());
            self.groups.pop_front().expect("process group result")
        }

        fn self_process(&mut self) -> Result<ProcessIdentity> {
            self.events.push("self-process".to_string());
            Ok(self.self_process)
        }

        fn process_group_for_pid(&mut self, _pid: i32) -> Result<Option<i32>> {
            self.events.push("server-pgid".to_string());
            Ok(self.server_pgid)
        }

        fn disable_daemon(&mut self, _server: &TmuxServerIncarnation) -> Result<()> {
            self.events.push("disable-daemon".to_string());
            self.disable_error.take().map_or(Ok(()), Err)
        }

        fn send_ctrl_c(&mut self, pane_id: &str) -> Result<()> {
            self.events.push(format!("ctrl-c:{pane_id}"));
            Ok(())
        }

        fn send_term(&mut self, pgid: i32) -> Result<()> {
            self.events.push(format!("term:{pgid}"));
            self.term_error.take().map_or(Ok(()), Err)
        }

        fn send_kill(&mut self, pgid: i32) -> Result<()> {
            self.events.push(format!("kill:{pgid}"));
            Ok(())
        }

        fn process_group_alive(&mut self, pgid: i32) -> Result<bool> {
            self.events.push(format!("alive:{pgid}"));
            self.alive
                .get_mut(&pgid)
                .and_then(VecDeque::pop_front)
                .expect("liveness result")
        }

        fn wait(&mut self, duration: Duration) {
            self.events.push(format!("wait:{}", duration.as_millis()));
        }

        fn server_presence(&mut self, _server: &TmuxServerIncarnation) -> Result<ServerPresence> {
            self.events.push("server-presence".to_string());
            self.presence.pop_front().expect("presence result")
        }

        fn kill_server(&mut self) -> Result<()> {
            self.events.push("kill-server".to_string());
            self.kill_server_error.take().map_or(Ok(()), Err)
        }
    }

    fn config() -> SessionManagerKillConfig {
        SessionManagerKillConfig {
            send_ctrl_c: true,
            term_wait_ms: 300,
            kill_wait_ms: 400,
        }
    }

    #[test]
    fn cleanup_orders_ctrl_c_term_live_kill_and_kill_server() {
        let mut ops = MockOps::default();
        clean_kill_server(&mut ops, &config()).unwrap();
        assert_eq!(
            ops.events,
            [
                "resolve-server",
                "list-panes",
                "self-process",
                "server-pgid",
                "process-groups",
                "disable-daemon",
                "ctrl-c:%1",
                "wait:300",
                "process-groups",
                "term:201",
                "term:202",
                "wait:300",
                "alive:201",
                "alive:202",
                "kill:202",
                "wait:400",
                "alive:202",
                "server-presence",
                "kill-server",
            ]
        );
    }

    #[test]
    fn daemon_disable_failure_stops_before_signals_and_kill_server() {
        let mut ops = MockOps {
            disable_error: Some(anyhow::anyhow!("disable failed")),
            ..MockOps::default()
        };
        let error = clean_kill_server(&mut ops, &config()).unwrap_err();
        assert!(error.to_string().contains("daemon_disabled=false"));
        assert!(error.to_string().contains("tmux server was not killed"));
        assert!(ops.events.iter().all(|event| {
            !event.starts_with("ctrl-c") && !event.starts_with("term") && event != "kill-server"
        }));
    }

    #[test]
    fn duplicate_process_groups_are_signaled_once_and_self_group_is_rejected() {
        let panes = vec![
            PaneProcess {
                pane_id: "%1".to_string(),
                pane_pid: 101,
                pane_tty: "ttys001".to_string(),
                pane_current_command: "zsh".to_string(),
            },
            PaneProcess {
                pane_id: "%2".to_string(),
                pane_pid: 102,
                pane_tty: "ttys001".to_string(),
                pane_current_command: "bash".to_string(),
            },
        ];
        let groups = collect_process_groups_from_table(
            "101 201 ttys001\n102 201 ttys001\n103 202 ttys001\n",
            &panes,
        )
        .unwrap();
        assert_eq!(groups, BTreeSet::from([201, 202]));
        assert!(validate_target_groups(&groups, 201, Some(800)).is_err());
    }

    #[test]
    fn term_failure_does_not_kill_server() {
        let mut ops = MockOps {
            term_error: Some(anyhow::anyhow!("term denied")),
            ..MockOps::default()
        };
        let error = clean_kill_server(&mut ops, &config()).unwrap_err();
        assert!(error.to_string().contains("partial state"));
        assert!(!ops.events.contains(&"kill-server".to_string()));
    }

    #[test]
    fn self_process_group_is_excluded_from_every_signal() {
        let mut ops = MockOps {
            groups: VecDeque::from([
                Ok(BTreeSet::from([201, 900])),
                Ok(BTreeSet::from([201, 900])),
            ]),
            alive: BTreeMap::from([(201, VecDeque::from([Ok(false)]))]),
            ..MockOps::default()
        };
        clean_kill_server(&mut ops, &config()).unwrap();
        assert!(ops.events.contains(&"term:201".to_string()));
        assert!(
            ops.events
                .iter()
                .all(|event| event != "term:900" && event != "kill:900")
        );
    }

    #[test]
    fn liveness_check_failure_leaves_tmux_server_running() {
        let mut ops = MockOps {
            alive: BTreeMap::from([
                (
                    201,
                    VecDeque::from([Err(anyhow::anyhow!("liveness denied"))]),
                ),
                (202, VecDeque::from([Ok(false)])),
            ]),
            ..MockOps::default()
        };
        let error = clean_kill_server(&mut ops, &config()).unwrap_err();
        assert!(error.to_string().contains("liveness denied"));
        assert!(error.to_string().contains("tmux server was not killed"));
        assert!(!ops.events.contains(&"kill-server".to_string()));
    }

    #[test]
    fn kill_server_error_is_propagated_when_server_remains() {
        let mut ops = MockOps {
            presence: VecDeque::from([Ok(ServerPresence::Present), Ok(ServerPresence::Present)]),
            kill_server_error: Some(anyhow::anyhow!("kill-server failed")),
            ..MockOps::default()
        };
        let error = clean_kill_server(&mut ops, &config()).unwrap_err();
        assert!(error.to_string().contains("kill-server failed"));
    }

    #[test]
    fn kill_server_error_is_success_when_target_server_is_verified_absent() {
        let mut ops = MockOps {
            presence: VecDeque::from([Ok(ServerPresence::Present), Ok(ServerPresence::Absent)]),
            kill_server_error: Some(anyhow::anyhow!("no server running")),
            ..MockOps::default()
        };
        clean_kill_server(&mut ops, &config()).unwrap();
    }

    #[test]
    fn invalid_pane_data_is_rejected_before_daemon_disable() {
        assert!(parse_pane_processes("%1\u{1f}0\u{1f}ttys001\u{1f}zsh").is_ok());
        let mut ops = MockOps {
            panes: vec![PaneProcess {
                pane_id: "%1".to_string(),
                pane_pid: 0,
                pane_tty: "ttys001".to_string(),
                pane_current_command: "zsh".to_string(),
            }],
            ..MockOps::default()
        };
        assert!(clean_kill_server(&mut ops, &config()).is_err());
        assert!(!ops.events.contains(&"disable-daemon".to_string()));
    }
}

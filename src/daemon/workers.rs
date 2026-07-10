use std::collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::daemon::runtime::DaemonEvent;
use crate::daemon::topology::ServerIdentity;
use crate::detect::{demote_stale_running, detect_codex_wait_reason};
use crate::git::{GitRunner, SystemGitRunner, collect_git_badges, collect_worktree_infos};
use crate::hook::AgentStatus;
use crate::options::snapshot::{PaneSnapshot, effective_agent, is_live_agent_pane, read_all_panes};
use crate::pane_state::ObservationDispatchSnapshot;
use crate::sidebar::layout::jump_to_pane;
use crate::tmux::{SystemTmuxRunner, TmuxRunner};
use crate::{
    pane_state::{
        AgentKind, AgentPresenceObservation, CaptureInference, CaptureObservation,
        CaptureTrackerSnapshot, DaemonInstanceId, EventId, LifecycleState, PaneEvent,
        PaneEventEnvelope, PaneInstance, PaneState, StoredStateDescriptor, WaitReason,
    },
    tmux::{run_command, tmux_args},
};

pub const CAPTURE_HISTORY_LINES: &str = "-80";
pub const STALE_CAPTURE_SECONDS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessDetection {
    pub agents: BTreeSet<AgentKind>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentProcessSnapshot {
    commands: BTreeMap<u32, String>,
    children: BTreeMap<u32, Vec<u32>>,
    complete: bool,
}

impl AgentProcessSnapshot {
    pub fn parse(output: &str, command_succeeded: bool) -> Self {
        let mut snapshot = Self {
            complete: command_succeeded,
            ..Self::default()
        };
        if !command_succeeded {
            return snapshot;
        }
        for line in output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let mut fields = line.split_whitespace();
            let Some(pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
                snapshot.complete = false;
                continue;
            };
            let Some(ppid) = fields
                .next()
                .and_then(|value| value.trim().parse::<u32>().ok())
            else {
                snapshot.complete = false;
                continue;
            };
            let command = fields.collect::<Vec<_>>().join(" ");
            if command.is_empty() {
                snapshot.complete = false;
                continue;
            }
            if snapshot.commands.insert(pid, command).is_some() {
                snapshot.complete = false;
            }
            snapshot.children.entry(ppid).or_default().push(pid);
        }
        snapshot
    }

    pub fn detect_from_pid_tree(&self, root_pid: u32) -> ProcessDetection {
        if !self.complete || !self.commands.contains_key(&root_pid) {
            return ProcessDetection {
                agents: BTreeSet::new(),
                complete: false,
            };
        }
        let mut agents = BTreeSet::new();
        let mut stack = vec![root_pid];
        let mut visited = BTreeSet::new();
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            if let Some(command) = self.commands.get(&pid)
                && let Some(agent) = detect_process_agent(command)
            {
                agents.insert(agent);
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        ProcessDetection {
            agents,
            complete: true,
        }
    }
}

fn detect_process_agent(command: &str) -> Option<AgentKind> {
    let mut fields = command.split_whitespace();
    let executable = fields.next()?.rsplit('/').next()?.to_ascii_lowercase();
    let direct = matches!(executable.as_str(), "claude" | "codex" | "opencode")
        .then_some(executable.as_str());
    let interpreted = matches!(
        executable.as_str(),
        "node" | "bun" | "deno" | "python" | "python3"
    )
    .then(|| fields.next())
    .flatten()
    .and_then(|script| {
        script
            .split(['/', '\\'])
            .map(str::to_ascii_lowercase)
            .find_map(|component| match component.as_str() {
                "claude" | "claude-code" => Some("claude"),
                "codex" | "codex-cli" => Some("codex"),
                "opencode" => Some("opencode"),
                _ => None,
            })
    });
    direct
        .or(interpreted)
        .and_then(|agent| AgentKind::parse(agent).ok())
}

pub fn read_agent_process_snapshot(timeout: Duration) -> AgentProcessSnapshot {
    match run_command("ps", &["-ax", "-o", "pid=,ppid=,command="], Some(timeout)) {
        Ok(output) => AgentProcessSnapshot::parse(&output, true),
        Err(_) => AgentProcessSnapshot::parse("", false),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureBatchOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub trait ObservationWorkerIo: Send + Sync + 'static {
    fn capture_batch(&self, args: &[String]) -> Result<CaptureBatchOutput>;
}

#[derive(Debug, Clone)]
pub struct SystemObservationWorkerIo {
    socket_name: Option<String>,
    timeout: Duration,
}

impl SystemObservationWorkerIo {
    pub fn new(socket_name: Option<String>) -> Self {
        Self {
            socket_name,
            timeout: Duration::from_secs(1),
        }
    }

    pub fn with_timeout(socket_name: Option<String>, timeout: Duration) -> Self {
        Self {
            socket_name,
            timeout,
        }
    }
}

impl ObservationWorkerIo for SystemObservationWorkerIo {
    fn capture_batch(&self, args: &[String]) -> Result<CaptureBatchOutput> {
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let tmux_args = tmux_args(self.socket_name.as_deref(), &refs);
        let mut child = std::process::Command::new("tmux")
            .args(tmux_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().map(|mut stdout| {
            thread::spawn(move || {
                let mut bytes = Vec::new();
                stdout.read_to_end(&mut bytes).map(|_| bytes)
            })
        });
        let stderr = child.stderr.take().map(|mut stderr| {
            thread::spawn(move || {
                let mut bytes = Vec::new();
                stderr.read_to_end(&mut bytes).map(|_| bytes)
            })
        });
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("tmux capture batch timed out after {:?}", self.timeout);
            }
            thread::sleep(Duration::from_millis(5));
        };
        let stdout = stdout
            .ok_or_else(|| anyhow::anyhow!("capture stdout was not piped"))?
            .join()
            .map_err(|_| anyhow::anyhow!("capture stdout reader panicked"))??;
        let stderr = stderr
            .ok_or_else(|| anyhow::anyhow!("capture stderr was not piped"))?
            .join()
            .map_err(|_| anyhow::anyhow!("capture stderr reader panicked"))??;
        Ok(CaptureBatchOutput {
            exit_code: status.code(),
            stdout: String::from_utf8(stdout)?,
            stderr: String::from_utf8(stderr)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureBatchError {
    Random(String),
    Io(String),
    ProcessFailed(Option<i32>),
    Stderr(String),
    DelimiterMismatch {
        expected: usize,
        actual: usize,
    },
    InvalidIdentityHeader,
    IdentityMismatch {
        expected: ServerIdentity,
        actual: ServerIdentity,
    },
}

impl std::fmt::Display for CaptureBatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Random(message) | Self::Io(message) => formatter.write_str(message),
            Self::ProcessFailed(code) => write!(formatter, "capture batch failed with {code:?}"),
            Self::Stderr(stderr) => write!(formatter, "capture batch wrote stderr: {stderr}"),
            Self::DelimiterMismatch { expected, actual } => write!(
                formatter,
                "capture delimiter count mismatch: expected {expected}, received {actual}"
            ),
            Self::InvalidIdentityHeader => formatter.write_str("invalid capture identity header"),
            Self::IdentityMismatch { expected, actual } => write!(
                formatter,
                "tmux server identity mismatch: expected {}:{}, received {}:{}",
                expected.pid, expected.start_time, actual.pid, actual.start_time
            ),
        }
    }
}

impl std::error::Error for CaptureBatchError {}

pub fn generate_capture_delimiter() -> Result<String, CaptureBatchError> {
    EventId::generate()
        .map(|event_id| event_id.as_str().to_string())
        .map_err(|error| CaptureBatchError::Random(error.to_string()))
}

pub fn capture_batch_args(panes: &[PaneInstance], delimiter: &str) -> Vec<String> {
    let mut args = vec![
        "display-message".to_string(),
        "-p".to_string(),
        capture_identity_format(delimiter),
        ";".to_string(),
    ];
    for (index, pane) in panes.iter().enumerate() {
        if index > 0 {
            args.push(";".to_string());
            args.extend([
                "display-message".to_string(),
                "-p".to_string(),
                delimiter.to_string(),
                ";".to_string(),
            ]);
        }
        args.extend([
            "capture-pane".to_string(),
            "-p".to_string(),
            "-S".to_string(),
            CAPTURE_HISTORY_LINES.to_string(),
            "-t".to_string(),
            pane.pane_id.clone(),
        ]);
    }
    args
}

fn capture_identity_format(delimiter: &str) -> String {
    format!("__vde_capture_identity_{delimiter}__#{{pid}}:#{{start_time}}")
}

pub fn collect_capture_batch(
    io: &dyn ObservationWorkerIo,
    panes: &[PaneInstance],
    expected_identity: &ServerIdentity,
) -> std::result::Result<Vec<String>, CaptureBatchError> {
    if panes.is_empty() {
        return Ok(Vec::new());
    }
    let delimiter = generate_capture_delimiter()?;
    let output = io
        .capture_batch(&capture_batch_args(panes, &delimiter))
        .map_err(|error| CaptureBatchError::Io(error.to_string()))?;
    parse_capture_batch(output, panes.len(), &delimiter, expected_identity)
}

pub fn parse_capture_batch(
    output: CaptureBatchOutput,
    pane_count: usize,
    delimiter: &str,
    expected_identity: &ServerIdentity,
) -> Result<Vec<String>, CaptureBatchError> {
    let (identity_line, stdout) = output
        .stdout
        .split_once('\n')
        .ok_or(CaptureBatchError::InvalidIdentityHeader)?;
    let prefix = format!("__vde_capture_identity_{delimiter}__");
    let identity = identity_line
        .strip_suffix('\r')
        .unwrap_or(identity_line)
        .strip_prefix(&prefix)
        .and_then(|value| value.split_once(':'))
        .and_then(|(pid, start_time)| {
            Some(ServerIdentity {
                pid: pid.parse().ok()?,
                start_time: start_time.parse().ok()?,
            })
        })
        .ok_or(CaptureBatchError::InvalidIdentityHeader)?;
    if &identity != expected_identity {
        return Err(CaptureBatchError::IdentityMismatch {
            expected: expected_identity.clone(),
            actual: identity,
        });
    }
    if output.exit_code != Some(0) {
        return Err(CaptureBatchError::ProcessFailed(output.exit_code));
    }
    if !output.stderr.is_empty() {
        return Err(CaptureBatchError::Stderr(output.stderr));
    }
    let mut tails = vec![String::new()];
    let mut delimiter_count = 0;
    for line in stdout.split_inclusive('\n') {
        let value = line.strip_suffix('\n').unwrap_or(line);
        let value = value.strip_suffix('\r').unwrap_or(value);
        if value == delimiter {
            delimiter_count += 1;
            tails.push(String::new());
        } else {
            tails
                .last_mut()
                .expect("capture tails always has one entry")
                .push_str(line);
        }
    }
    let expected = pane_count.saturating_sub(1);
    if delimiter_count != expected || tails.len() != pane_count {
        return Err(CaptureBatchError::DelimiterMismatch {
            expected,
            actual: delimiter_count,
        });
    }
    Ok(tails)
}

pub fn classify_presence(
    current: Option<&PaneState>,
    detected_agents: &BTreeSet<AgentKind>,
    scan_complete: bool,
) -> AgentPresenceObservation {
    if !scan_complete {
        return AgentPresenceObservation::Unknown;
    }
    if let Some(current) = current
        && detected_agents.contains(&current.agent)
    {
        return AgentPresenceObservation::Present(current.agent.clone());
    }
    match detected_agents.len() {
        0 => match current {
            Some(state) if !supports_process_detection(&state.agent) || !state.scan_verified => {
                AgentPresenceObservation::Unknown
            }
            _ => AgentPresenceObservation::Absent,
        },
        1 => AgentPresenceObservation::Present(
            detected_agents
                .iter()
                .next()
                .expect("one detected agent")
                .clone(),
        ),
        _ => AgentPresenceObservation::Unknown,
    }
}

pub fn infer_capture(
    state: Option<&PaneState>,
    tracker: &CaptureTrackerSnapshot,
    tail: &str,
    observed_at: i64,
) -> CaptureObservation {
    let observed_fingerprint = capture_sha256(tail);
    let inference = if observed_fingerprint.is_none()
        || tracker.rebaseline_pending
        || tracker.fingerprint.is_none()
    {
        CaptureInference::NoChange
    } else if let Some(reason) = detect_codex_wait_reason(tail) {
        CaptureInference::PermissionWait {
            reason: if reason == "permission_prompt" {
                WaitReason::PermissionPrompt
            } else {
                WaitReason::Other(reason.to_string())
            },
        }
    } else if observed_fingerprint != tracker.fingerprint {
        CaptureInference::ActivityObserved
    } else if state.is_some_and(|state| {
        matches!(state.lifecycle, LifecycleState::Running)
            && observed_at.saturating_sub(
                state
                    .started_at
                    .into_iter()
                    .chain(tracker.last_change_at)
                    .max()
                    .unwrap_or(observed_at),
            ) >= STALE_CAPTURE_SECONDS
    }) {
        CaptureInference::StaleRunCompleted
    } else {
        CaptureInference::NoChange
    };
    CaptureObservation {
        inference,
        observed_fingerprint,
    }
}

pub fn capture_sha256(tail: &str) -> Option<[u8; 32]> {
    if tail.trim().is_empty() {
        return None;
    }
    Some(Sha256::digest(tail.as_bytes()).into())
}

pub fn observation_envelope(
    daemon_instance_id: DaemonInstanceId,
    pane_instance: PaneInstance,
    base: Option<StoredStateDescriptor>,
    tracker: &CaptureTrackerSnapshot,
    observed_at: i64,
    presence: AgentPresenceObservation,
    capture: Option<CaptureObservation>,
) -> Result<PaneEventEnvelope> {
    let capture = (!matches!(presence, AgentPresenceObservation::Unknown))
        .then_some(capture)
        .flatten();
    Ok(PaneEventEnvelope {
        daemon_instance_id,
        event_id: EventId::generate()?,
        pane_instance,
        agent: None,
        agent_session_id: None,
        event: PaneEvent::ObservationBatch {
            base,
            tracker_generation: tracker.generation,
            observed_at,
            presence,
            capture,
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationPollResult {
    pub envelopes: Vec<PaneEventEnvelope>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationPollError {
    UnverifiedServerIdentity(CaptureBatchError),
    Event(String),
}

impl std::fmt::Display for ObservationPollError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnverifiedServerIdentity(error) => write!(formatter, "{error}"),
            Self::Event(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ObservationPollError {}

impl ObservationPollError {
    pub fn requires_daemon_exit(&self) -> bool {
        matches!(self, Self::UnverifiedServerIdentity(_))
    }
}

pub fn run_observation_poll(
    io: &dyn ObservationWorkerIo,
    dispatch: &[ObservationDispatchSnapshot],
    processes: &AgentProcessSnapshot,
    daemon_instance_id: &DaemonInstanceId,
    expected_identity: &ServerIdentity,
    observed_at: i64,
) -> std::result::Result<ObservationPollResult, ObservationPollError> {
    let panes = dispatch
        .iter()
        .map(|snapshot| snapshot.pane_instance.clone())
        .collect::<Vec<_>>();
    let (tails, mut diagnostics) = match collect_capture_batch(io, &panes, expected_identity) {
        Ok(tails) => (Some(tails), Vec::new()),
        Err(error @ CaptureBatchError::InvalidIdentityHeader)
        | Err(error @ CaptureBatchError::IdentityMismatch { .. }) => {
            return Err(ObservationPollError::UnverifiedServerIdentity(error));
        }
        Err(error) => (None, vec![format!("capture_batch_discarded: {error}")]),
    };
    let mut envelopes = Vec::new();
    for (index, snapshot) in dispatch.iter().enumerate() {
        if matches!(
            snapshot.base,
            Some(StoredStateDescriptor::Quarantined { .. })
        ) {
            diagnostics.push(format!(
                "quarantined_observation_skipped: {}",
                snapshot.pane_instance.pane_id
            ));
            continue;
        }
        let detection = processes.detect_from_pid_tree(snapshot.pane_instance.pane_pid);
        let presence = classify_presence(
            snapshot.state.as_ref(),
            &detection.agents,
            detection.complete,
        );
        if detection.complete && detection.agents.len() > 1 {
            diagnostics.push(format!(
                "ambiguous_agent_processes: {}",
                snapshot.pane_instance.pane_id
            ));
        }
        let capture = tails
            .as_ref()
            .and_then(|tails| tails.get(index))
            .map(|tail| {
                infer_capture(
                    snapshot.state.as_ref(),
                    &snapshot.tracker,
                    tail,
                    observed_at,
                )
            });
        envelopes.push(
            observation_envelope(
                daemon_instance_id.clone(),
                snapshot.pane_instance.clone(),
                snapshot.base.clone(),
                &snapshot.tracker,
                observed_at,
                presence,
                capture,
            )
            .map_err(|error| ObservationPollError::Event(error.to_string()))?,
        );
    }
    Ok(ObservationPollResult {
        envelopes,
        diagnostics,
    })
}

pub fn pane_removal_envelopes(
    daemon_instance_id: &DaemonInstanceId,
    previous: &[ObservationDispatchSnapshot],
    current: &BTreeSet<PaneInstance>,
    topology_complete: bool,
) -> Result<Vec<PaneEventEnvelope>> {
    if !topology_complete {
        return Ok(Vec::new());
    }
    previous
        .iter()
        .filter(|snapshot| !current.contains(&snapshot.pane_instance))
        .map(|snapshot| {
            Ok(PaneEventEnvelope {
                daemon_instance_id: daemon_instance_id.clone(),
                event_id: EventId::generate()?,
                pane_instance: snapshot.pane_instance.clone(),
                agent: None,
                agent_session_id: None,
                event: PaneEvent::PaneRemoved {
                    expected: snapshot.base.clone(),
                },
            })
        })
        .collect()
}

fn supports_process_detection(agent: &AgentKind) -> bool {
    matches!(agent.as_str(), "claude" | "codex" | "opencode")
}

pub trait WorkerIo: Send + Sync + 'static {
    fn read_panes(&self) -> Result<Vec<PaneSnapshot>>;
    fn capture_tail(&self, pane_id: &str) -> Result<String>;
    fn jump_to_pane(&self, pane_id: &str) -> Result<()>;
    fn preview_pane(&self, pane_id: &str, history_lines: u32) -> Result<()>;
    fn set_pane_option(&self, pane_id: &str, key: &str, value: &str) -> Result<()>;
    fn unset_pane_option(&self, pane_id: &str, key: &str) -> Result<()>;
    fn set_session_option(&self, session: &str, key: &str, value: &str) -> Result<()>;
    fn unset_session_option(&self, session: &str, key: &str) -> Result<()>;
    fn set_window_option(&self, window: &str, key: &str, value: &str) -> Result<()>;
    fn unset_window_option(&self, window: &str, key: &str) -> Result<()>;
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

    fn set_pane_option(&self, pane_id: &str, key: &str, value: &str) -> Result<()> {
        crate::options::set_pane_option(&self.runner, pane_id, key, value)
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

    fn set_window_option(&self, window: &str, key: &str, value: &str) -> Result<()> {
        crate::options::set_window_option(&self.runner, window, key, value)
    }

    fn unset_window_option(&self, window: &str, key: &str) -> Result<()> {
        crate::options::unset_window_option(&self.runner, window, key)
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

#[derive(Debug, Default)]
struct CaptureActivityTracker {
    panes: BTreeMap<String, CaptureActivityState>,
}

#[derive(Debug, Default)]
pub struct SharedCaptureActivity {
    tracker: Mutex<CaptureActivityTracker>,
}

impl SharedCaptureActivity {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune(&self, pane_ids: &BTreeSet<String>) {
        self.tracker
            .lock()
            .expect("capture activity tracker poisoned")
            .prune(pane_ids);
    }

    fn record_tail(
        &self,
        pane_id: &str,
        started_at: Option<i64>,
        now_epoch: i64,
        tail: &str,
    ) -> Option<i64> {
        self.tracker
            .lock()
            .expect("capture activity tracker poisoned")
            .record_tail(pane_id, started_at, now_epoch, tail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureActivityState {
    started_at: Option<i64>,
    fingerprint: u64,
    last_changed_at: i64,
}

impl CaptureActivityTracker {
    fn record_tail(
        &mut self,
        pane_id: &str,
        started_at: Option<i64>,
        now_epoch: i64,
        tail: &str,
    ) -> Option<i64> {
        if tail.trim().is_empty() {
            return None;
        }
        let fingerprint = capture_fingerprint(tail);
        let baseline = started_at.unwrap_or(now_epoch);
        match self.panes.get_mut(pane_id) {
            Some(state) if state.started_at == started_at => {
                if state.fingerprint != fingerprint {
                    state.fingerprint = fingerprint;
                    state.last_changed_at = now_epoch;
                }
                Some(state.last_changed_at)
            }
            _ => {
                self.panes.insert(
                    pane_id.to_string(),
                    CaptureActivityState {
                        started_at,
                        fingerprint,
                        last_changed_at: baseline,
                    },
                );
                Some(baseline)
            }
        }
    }

    fn prune(&mut self, pane_ids: &BTreeSet<String>) {
        self.panes.retain(|pane_id, _| pane_ids.contains(pane_id));
    }
}

fn capture_fingerprint(tail: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    tail.hash(&mut hasher);
    hasher.finish()
}

pub fn start_tmux_worker(
    io: Arc<dyn WorkerIo>,
    latest_panes: Arc<LatestPanes>,
    capture_activity: Arc<SharedCaptureActivity>,
    tx: Sender<DaemonEvent>,
    poll: Duration,
    stale_threshold_seconds: i64,
) {
    thread::spawn(move || {
        loop {
            if let Err(error) = poll_tmux_once_with_latest(
                io.clone(),
                latest_panes.clone(),
                capture_activity.clone(),
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
    let capture_activity = Arc::new(SharedCaptureActivity::new());
    poll_tmux_once_with_latest(io, latest, capture_activity, tx, stale_threshold_seconds)
}

fn poll_tmux_once_with_latest(
    io: Arc<dyn WorkerIo>,
    latest_panes: Arc<LatestPanes>,
    capture_activity: Arc<SharedCaptureActivity>,
    tx: Sender<DaemonEvent>,
    stale_threshold_seconds: i64,
) -> Result<()> {
    let panes = read_panes_with_shared_capture_activity(
        io.as_ref(),
        stale_threshold_seconds,
        capture_activity.as_ref(),
    )?;
    latest_panes.store(panes.clone());
    tx.send(DaemonEvent::PanesUpdated(panes))?;
    Ok(())
}

pub fn read_panes_with_detection(
    io: &dyn WorkerIo,
    stale_threshold_seconds: i64,
) -> Result<Vec<PaneSnapshot>> {
    let mut capture_activity = CaptureActivityTracker::default();
    read_panes_with_detection_tracked(io, stale_threshold_seconds, &mut capture_activity)
}

pub fn read_panes_with_shared_capture_activity(
    io: &dyn WorkerIo,
    stale_threshold_seconds: i64,
    capture_activity: &SharedCaptureActivity,
) -> Result<Vec<PaneSnapshot>> {
    let now = now_epoch();
    let panes = io.read_panes()?;
    capture_activity.prune(
        &panes
            .iter()
            .map(|pane| pane.pane_id.clone())
            .collect::<BTreeSet<_>>(),
    );
    Ok(panes
        .into_iter()
        .map(|pane| {
            apply_capture_detection_with_recorder(
                io,
                pane,
                now,
                stale_threshold_seconds,
                |pane_id, started_at, now_epoch, tail| {
                    capture_activity.record_tail(pane_id, started_at, now_epoch, tail)
                },
            )
        })
        .collect())
}

fn read_panes_with_detection_tracked(
    io: &dyn WorkerIo,
    stale_threshold_seconds: i64,
    capture_activity: &mut CaptureActivityTracker,
) -> Result<Vec<PaneSnapshot>> {
    let now = now_epoch();
    let panes = io.read_panes()?;
    capture_activity.prune(
        &panes
            .iter()
            .map(|pane| pane.pane_id.clone())
            .collect::<BTreeSet<_>>(),
    );
    Ok(panes
        .into_iter()
        .map(|pane| {
            apply_capture_detection_with_tracker(
                io,
                pane,
                now,
                stale_threshold_seconds,
                capture_activity,
            )
        })
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
    let panes = latest_panes.load();
    let badges = collect_git_badges(git.as_ref(), &panes);
    let worktrees = collect_worktree_infos(git.as_ref(), &panes);
    tx.send(DaemonEvent::GitStatusUpdated { badges, worktrees })?;
    Ok(())
}

pub fn system_git_runner(timeout: Duration) -> SystemGitRunner {
    SystemGitRunner::new(timeout)
}

pub fn apply_capture_detection(
    io: &dyn WorkerIo,
    pane: PaneSnapshot,
    now_epoch: i64,
    stale_threshold_seconds: i64,
) -> PaneSnapshot {
    let mut capture_activity = CaptureActivityTracker::default();
    apply_capture_detection_with_tracker(
        io,
        pane,
        now_epoch,
        stale_threshold_seconds,
        &mut capture_activity,
    )
}

fn apply_capture_detection_with_tracker(
    io: &dyn WorkerIo,
    pane: PaneSnapshot,
    now_epoch: i64,
    stale_threshold_seconds: i64,
    capture_activity: &mut CaptureActivityTracker,
) -> PaneSnapshot {
    apply_capture_detection_with_recorder(
        io,
        pane,
        now_epoch,
        stale_threshold_seconds,
        |pane_id, started_at, now_epoch, tail| {
            capture_activity.record_tail(pane_id, started_at, now_epoch, tail)
        },
    )
}

fn apply_capture_detection_with_recorder(
    io: &dyn WorkerIo,
    mut pane: PaneSnapshot,
    now_epoch: i64,
    stale_threshold_seconds: i64,
    mut record_tail: impl FnMut(&str, Option<i64>, i64, &str) -> Option<i64>,
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
    let started_at = pane.started_at.trim().parse::<i64>().ok();
    let running_has_started_at = pane.status == "running" && started_at.is_some();
    let has_hook_wait_reason = !pane.wait_reason.trim().is_empty();
    let status_allows_capture_detection = pane.status.trim().is_empty() || pane.status == "running";
    let should_detect_wait_reason = !has_hook_wait_reason && status_allows_capture_detection;
    let should_capture = should_detect_wait_reason || pane.status == "running";
    if should_capture && let Ok(tail) = io.capture_tail(&pane.pane_id) {
        if should_detect_wait_reason && let Some(wait_reason) = detect_codex_wait_reason(&tail) {
            pane.status = "waiting".to_string();
            pane.wait_reason = wait_reason.to_string();
        } else if running_has_started_at {
            observed_activity_epoch = record_tail(&pane.pane_id, started_at, now_epoch, &tail);
        }
    }
    if pane.status == "running" && !running_has_started_at {
        pane.status = "idle".to_string();
        pane.wait_reason.clear();
    }
    let last_activity = observed_activity_epoch.or(started_at).unwrap_or(now_epoch);
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

    struct MockObservationIo {
        calls: Mutex<usize>,
        tails: Vec<String>,
    }

    impl ObservationWorkerIo for MockObservationIo {
        fn capture_batch(&self, args: &[String]) -> anyhow::Result<CaptureBatchOutput> {
            *self.calls.lock().unwrap() += 1;
            let delimiter = args
                .iter()
                .find_map(|value| {
                    value
                        .strip_prefix("__vde_capture_identity_")
                        .and_then(|value| value.split_once("__"))
                        .map(|(delimiter, _)| delimiter.to_string())
                })
                .unwrap_or_default();
            Ok(CaptureBatchOutput {
                exit_code: Some(0),
                stdout: format!(
                    "__vde_capture_identity_{delimiter}__{}:{}\n{}",
                    server_identity().pid,
                    server_identity().start_time,
                    self.tails.join(&format!("{delimiter}\n"))
                ),
                stderr: String::new(),
            })
        }
    }

    fn server_identity() -> ServerIdentity {
        ServerIdentity {
            pid: 4242,
            start_time: 99,
        }
    }

    fn pane_instance(id: &str, pid: u32) -> PaneInstance {
        PaneInstance {
            pane_id: id.to_string(),
            pane_pid: pid,
        }
    }

    fn canonical_state(agent: &str) -> PaneState {
        PaneState {
            schema_version: crate::pane_state::PANE_STATE_SCHEMA_VERSION,
            state_id: crate::pane_state::StateId::parse("00112233445566778899aabbccddeeff")
                .unwrap(),
            revision: 1,
            pane_instance: pane_instance("%1", 11),
            agent: AgentKind::parse(agent).unwrap(),
            agent_session_id: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Running,
            run_seq: 1,
            completed_seq: 0,
            acknowledged_seq: 0,
            started_at: Some(100),
            completed_at: None,
            prompt: None,
            tasks: crate::pane_state::TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
        }
    }

    #[test]
    fn capture_batch_uses_one_worker_call_for_all_panes() {
        let io = MockObservationIo {
            calls: Mutex::new(0),
            tails: vec![
                "one\n".to_string(),
                "two\n".to_string(),
                "three\n".to_string(),
            ],
        };
        let panes = vec![
            pane_instance("%1", 11),
            pane_instance("%2", 22),
            pane_instance("%3", 33),
        ];
        assert_eq!(
            collect_capture_batch(&io, &panes, &server_identity()).unwrap(),
            io.tails
        );
        assert_eq!(*io.calls.lock().unwrap(), 1);
    }

    #[test]
    fn capture_batch_rejects_nonzero_stderr_and_delimiter_races() {
        let delimiter = "00112233445566778899aabbccddeeff";
        for output in [
            CaptureBatchOutput {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: String::new(),
            },
            CaptureBatchOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: "pane vanished".to_string(),
            },
            CaptureBatchOutput {
                exit_code: Some(0),
                stdout: "first only\n".to_string(),
                stderr: String::new(),
            },
            CaptureBatchOutput {
                exit_code: Some(0),
                stdout: format!("first\n{delimiter}\ncollision\n{delimiter}\nsecond\n"),
                stderr: String::new(),
            },
        ] {
            assert!(parse_capture_batch(output, 2, delimiter, &server_identity()).is_err());
        }
    }

    #[test]
    fn capture_batch_rejects_first_middle_and_last_pane_disappearance() {
        let delimiter = "00112233445566778899aabbccddeeff";
        let first_missing = CaptureBatchOutput {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "first pane missing".to_string(),
        };
        let middle_missing = CaptureBatchOutput {
            exit_code: Some(1),
            stdout: format!("first\n{delimiter}\n"),
            stderr: "middle pane missing".to_string(),
        };
        let last_missing = CaptureBatchOutput {
            exit_code: Some(1),
            stdout: format!("first\n{delimiter}\nsecond\n{delimiter}\n"),
            stderr: "last pane missing".to_string(),
        };
        assert!(parse_capture_batch(first_missing, 3, delimiter, &server_identity()).is_err());
        assert!(parse_capture_batch(middle_missing, 3, delimiter, &server_identity()).is_err());
        assert!(parse_capture_batch(last_missing, 3, delimiter, &server_identity()).is_err());
    }

    #[test]
    fn capture_batch_rejects_server_identity_mismatch() {
        let delimiter = "00112233445566778899aabbccddeeff";
        let output = CaptureBatchOutput {
            exit_code: Some(1),
            stdout: format!("__vde_capture_identity_{delimiter}__43:99\ntail\n"),
            stderr: "pane disappeared".to_string(),
        };
        assert!(matches!(
            parse_capture_batch(output, 1, delimiter, &server_identity()),
            Err(CaptureBatchError::IdentityMismatch { .. })
        ));
    }

    #[test]
    fn presence_is_three_state_and_prefers_current_kind() {
        let state = canonical_state("codex");
        let agents = BTreeSet::from([
            AgentKind::parse("claude").unwrap(),
            AgentKind::parse("codex").unwrap(),
        ]);
        assert_eq!(
            classify_presence(Some(&state), &agents, true),
            AgentPresenceObservation::Present(AgentKind::parse("codex").unwrap())
        );
        assert_eq!(
            classify_presence(None, &agents, true),
            AgentPresenceObservation::Unknown
        );
        assert_eq!(
            classify_presence(Some(&state), &BTreeSet::new(), false),
            AgentPresenceObservation::Unknown
        );
        let mut generic = canonical_state("generic");
        generic.scan_verified = false;
        assert_eq!(
            classify_presence(Some(&generic), &BTreeSet::new(), true),
            AgentPresenceObservation::Unknown
        );
    }

    #[test]
    fn process_snapshot_collects_all_agent_kinds_and_marks_malformed_input_incomplete() {
        let snapshot = AgentProcessSnapshot::parse(
            "   10     1 zsh\n   11    10 codex\n   12    10 /usr/bin/claude --resume\n   13    12 opencode\n   14    10 rg codex\n",
            true,
        );
        let detection = snapshot.detect_from_pid_tree(10);
        assert!(detection.complete);
        assert_eq!(
            detection
                .agents
                .iter()
                .map(AgentKind::as_str)
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "opencode"]
        );

        let malformed = AgentProcessSnapshot::parse("10 1 zsh\nbroken\n", true);
        assert!(!malformed.detect_from_pid_tree(10).complete);
        assert!(
            !AgentProcessSnapshot::parse("", false)
                .detect_from_pid_tree(10)
                .complete
        );
    }

    #[test]
    fn capture_inference_handles_baseline_change_rebaseline_and_stale() {
        let state = canonical_state("opencode");
        let baseline = infer_capture(
            Some(&state),
            &CaptureTrackerSnapshot::default(),
            "first\n",
            100,
        );
        assert_eq!(baseline.inference, CaptureInference::NoChange);
        let permission_baseline = infer_capture(
            Some(&state),
            &CaptureTrackerSnapshot::default(),
            "Allow command execution?\n1. Yes\n2. No\n",
            100,
        );
        assert_eq!(permission_baseline.inference, CaptureInference::NoChange);
        let mut tracker = CaptureTrackerSnapshot {
            fingerprint: baseline.observed_fingerprint,
            last_change_at: Some(100),
            ..CaptureTrackerSnapshot::default()
        };
        assert_eq!(
            infer_capture(Some(&state), &tracker, "changed\n", 101).inference,
            CaptureInference::ActivityObserved
        );
        tracker.rebaseline_pending = true;
        assert_eq!(
            infer_capture(Some(&state), &tracker, "changed\n", 500).inference,
            CaptureInference::NoChange
        );
        tracker.rebaseline_pending = false;
        assert_eq!(
            infer_capture(Some(&state), &tracker, "first\n", 500).inference,
            CaptureInference::StaleRunCompleted
        );
        assert!(
            infer_capture(Some(&state), &tracker, "   \n", 500)
                .observed_fingerprint
                .is_none()
        );
    }

    #[test]
    fn unknown_presence_drops_capture_from_observation_envelope() {
        let tracker = CaptureTrackerSnapshot::default();
        let envelope = observation_envelope(
            DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            pane_instance("%1", 11),
            None,
            &tracker,
            100,
            AgentPresenceObservation::Unknown,
            Some(CaptureObservation {
                inference: CaptureInference::ActivityObserved,
                observed_fingerprint: Some([1; 32]),
            }),
        )
        .unwrap();
        let PaneEvent::ObservationBatch { capture, .. } = envelope.event else {
            panic!("expected observation batch");
        };
        assert!(capture.is_none());
    }

    #[test]
    fn observation_poll_connects_frozen_dispatch_process_scan_and_single_capture() {
        let state = canonical_state("opencode");
        let tracker = CaptureTrackerSnapshot {
            epoch: Some((state.state_id.clone(), state.agent_epoch)),
            fingerprint: capture_sha256("before\n"),
            last_change_at: Some(100),
            ..CaptureTrackerSnapshot::default()
        };
        let dispatch = vec![ObservationDispatchSnapshot {
            pane_instance: state.pane_instance.clone(),
            base: Some(StoredStateDescriptor::Canonical {
                version: state.version(),
            }),
            tracker,
            state: Some(state),
        }];
        let io = MockObservationIo {
            calls: Mutex::new(0),
            tails: vec!["after\n".to_string()],
        };
        let processes = AgentProcessSnapshot::parse("11 1 opencode\n", true);
        let result = run_observation_poll(
            &io,
            &dispatch,
            &processes,
            &DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            &server_identity(),
            200,
        )
        .unwrap();
        assert_eq!(*io.calls.lock().unwrap(), 1);
        let PaneEvent::ObservationBatch {
            presence, capture, ..
        } = &result.envelopes[0].event
        else {
            panic!("expected observation batch");
        };
        assert_eq!(
            *presence,
            AgentPresenceObservation::Present(AgentKind::parse("opencode").unwrap())
        );
        assert_eq!(
            capture.as_ref().unwrap().inference,
            CaptureInference::ActivityObserved
        );
    }

    #[test]
    fn incomplete_topology_never_emits_pane_removal() {
        let state = canonical_state("codex");
        let previous = vec![ObservationDispatchSnapshot {
            pane_instance: state.pane_instance.clone(),
            base: Some(StoredStateDescriptor::Canonical {
                version: state.version(),
            }),
            tracker: CaptureTrackerSnapshot::default(),
            state: Some(state),
        }];
        let daemon = DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap();
        assert!(
            pane_removal_envelopes(&daemon, &previous, &BTreeSet::new(), false)
                .unwrap()
                .is_empty()
        );
        let removed = pane_removal_envelopes(&daemon, &previous, &BTreeSet::new(), true).unwrap();
        assert!(matches!(removed[0].event, PaneEvent::PaneRemoved { .. }));
    }

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

        fn set_pane_option(&self, _pane_id: &str, _key: &str, _value: &str) -> anyhow::Result<()> {
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

        fn set_window_option(&self, _window: &str, _key: &str, _value: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn unset_window_option(&self, _window: &str, _key: &str) -> anyhow::Result<()> {
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
        top_level: Option<String>,
        git_dir: Option<String>,
        common_dir: Option<String>,
        superproject: Option<String>,
    }

    impl GitRunner for MockGitRunner {
        fn run(&self, _cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            match args {
                ["branch", "--show-current"] => Ok(self.branch.clone()),
                ["rev-list", "--left-right", "--count", "@{upstream}...HEAD"] => {
                    Ok(self.counts.clone())
                }
                ["rev-parse", "--show-toplevel"] => self
                    .top_level
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("not a repo")),
                ["rev-parse", "--git-dir"] => self
                    .git_dir
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("not a repo")),
                ["rev-parse", "--git-common-dir"] => self
                    .common_dir
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("not a repo")),
                ["rev-parse", "--show-superproject-working-tree"] => {
                    Ok(self.superproject.clone().unwrap_or_default())
                }
                _ => anyhow::bail!("unexpected git args: {args:?}"),
            }
        }

        fn run_vw(&self, _cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            anyhow::bail!("unexpected vw args: {args:?}")
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
            top_level: Some("/tmp/app\n".to_string()),
            git_dir: Some("/tmp/repo/.git/worktrees/app\n".to_string()),
            common_dir: Some("/tmp/repo/.git\n".to_string()),
            superproject: Some("\n".to_string()),
        });

        poll_git_once(git, panes, tx).unwrap();

        let DaemonEvent::GitStatusUpdated { badges, worktrees } = rx.recv().unwrap() else {
            panic!("expected git status updated");
        };
        assert_eq!(badges["/tmp/app"].branch, "main");
        assert_eq!(worktrees["/tmp/app"].name, "app");
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
    fn stale_running_with_stable_non_empty_tail_is_demoted_to_idle() {
        let io = MockWorkerIo::default();
        let mut stale = pane("%1", "claude", "running");
        stale.started_at = "100".to_string();
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Claude is still working\n".to_string());

        let pane = apply_capture_detection(&io, stale, 1_000, 30);

        assert_eq!(pane.status, "idle");
    }

    #[test]
    fn running_pane_with_changed_capture_tail_is_not_demoted_to_idle() {
        let io = MockWorkerIo::default();
        let mut active = pane("%1", "claude", "running");
        active.started_at = "970".to_string();
        let mut tracker = CaptureActivityTracker::default();
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Working (1s)\n".to_string());
        let pane = apply_capture_detection_with_tracker(&io, active.clone(), 990, 30, &mut tracker);
        assert_eq!(pane.status, "running");

        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Working (40s)\n".to_string());
        let pane = apply_capture_detection_with_tracker(&io, active, 1_000, 30, &mut tracker);

        assert_eq!(pane.status, "running");
    }

    #[test]
    fn running_pane_uses_started_at_over_stale_completed_at() {
        let io = MockWorkerIo::default();
        let mut active = pane("%1", "codex", "running");
        active.started_at = "990".to_string();
        active.completed_at = "100".to_string();
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "Codex is ready for input\n".to_string());

        let pane = apply_capture_detection(&io, active, 1_000, 30);

        assert_eq!(pane.status, "running");
    }

    #[test]
    fn capture_activity_tracker_resets_when_started_at_changes() {
        let io = MockWorkerIo::default();
        let mut first = pane("%1", "codex", "running");
        first.started_at = "100".to_string();
        let mut tracker = CaptureActivityTracker::default();
        io.captures
            .lock()
            .unwrap()
            .insert("%1".to_string(), "same tail\n".to_string());
        let first_result = apply_capture_detection_with_tracker(&io, first, 200, 30, &mut tracker);
        assert_eq!(first_result.status, "idle");

        let mut second = pane("%1", "codex", "running");
        second.started_at = "990".to_string();
        let second_result =
            apply_capture_detection_with_tracker(&io, second, 1_000, 30, &mut tracker);

        assert_eq!(second_result.status, "running");
    }

    #[test]
    fn capture_activity_tracker_prunes_disappeared_panes() {
        let io = MockWorkerIo::default();
        let mut tracker = CaptureActivityTracker::default();
        tracker.record_tail("%1", Some(100), 100, "tail\n");
        tracker.record_tail("%2", Some(100), 100, "tail\n");

        io.panes.lock().unwrap().push(pane("%2", "codex", "idle"));
        read_panes_with_detection_tracked(&io, 30, &mut tracker).unwrap();

        assert!(!tracker.panes.contains_key("%1"));
        assert!(tracker.panes.contains_key("%2"));
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

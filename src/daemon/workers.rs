use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::process::Child;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::daemon::topology::ServerIdentity;
use crate::detect::detect_codex_wait_reason;
use crate::git::SystemGitRunner;
use crate::pane_state::ObservationDispatchSnapshot;
use crate::sidebar::layout::jump_to_pane;
use crate::tmux::SystemTmuxRunner;
use crate::tmux::TmuxRunner;
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
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    terminate_child_bounded(child, Duration::from_millis(100));
                    return Err(error.into());
                }
            }
            if Instant::now() >= deadline {
                terminate_child_bounded(child, Duration::from_millis(100));
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

fn terminate_child_bounded(mut child: Child, timeout: Duration) {
    let _ = child.kill();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(child.wait());
    });
    let _ = receiver.recv_timeout(timeout);
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

const SIDEBAR_SERVER_MISMATCH_SENTINEL: &str = "__vde_sidebar_server_mismatch__";
const SIDEBAR_PANE_MISMATCH_SENTINEL: &str = "__vde_sidebar_pane_mismatch__";
const SIDEBAR_JOB_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum SidebarTmuxError {
    ServerIncarnationMismatch,
    PaneInstanceMismatch(String),
    Command(anyhow::Error),
}

impl std::fmt::Display for SidebarTmuxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerIncarnationMismatch => write!(formatter, "tmux server incarnation changed"),
            Self::PaneInstanceMismatch(pane_id) => {
                write!(formatter, "pane instance changed: {pane_id}")
            }
            Self::Command(error) => write!(formatter, "tmux command failed: {error:#}"),
        }
    }
}

impl std::error::Error for SidebarTmuxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Command(error) => Some(error.as_ref()),
            Self::ServerIncarnationMismatch | Self::PaneInstanceMismatch(_) => None,
        }
    }
}

#[derive(Debug)]
enum SidebarGuardError {
    ServerIncarnationMismatch,
    PaneInstanceMismatch,
}

impl std::fmt::Display for SidebarGuardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerIncarnationMismatch => write!(formatter, "tmux server incarnation changed"),
            Self::PaneInstanceMismatch => write!(formatter, "pane instance changed"),
        }
    }
}

impl std::error::Error for SidebarGuardError {}

/// Applies the daemon's server-incarnation and selected-pane fences to every tmux operation made
/// by the sidebar FIFO worker. Public, direct sidebar commands intentionally keep using their
/// unguarded runner; only daemon-owned execution is wrapped here.
struct GuardedSidebarTmuxRunner<'a> {
    runner: &'a dyn TmuxRunner,
    expected_server: &'a ServerIdentity,
    expected_pane: &'a PaneInstance,
}

impl GuardedSidebarTmuxRunner<'_> {
    fn is_read(args: &[&str]) -> bool {
        matches!(
            args.first().copied(),
            Some("display-message" | "list-panes")
        )
    }

    fn guarded_mutation_args(&self, args: &[&str]) -> Vec<String> {
        let command = crate::pane_state::store::tmux_command_string(
            &args
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>(),
        );
        let pane_guard = format!("#{{==:#{{pane_pid}},{}}}", self.expected_pane.pane_pid);
        let pane_command = crate::pane_state::store::tmux_command_string(&[
            "if-shell".to_string(),
            "-F".to_string(),
            "-t".to_string(),
            self.expected_pane.pane_id.clone(),
            pane_guard,
            command,
            format!("display-message -p '{SIDEBAR_PANE_MISMATCH_SENTINEL}'"),
        ]);
        crate::pane_state::store::server_guarded_command_args(
            self.expected_server.pid,
            self.expected_server.start_time,
            pane_command,
            SIDEBAR_SERVER_MISMATCH_SENTINEL,
        )
    }

    fn guarded_read_args(&self, args: &[&str], token: &str) -> Vec<String> {
        let identity = format!("__vde_sidebar_identity_{token}__#{{pid}}:#{{start_time}}");
        let mut guarded = vec![
            "display-message".to_string(),
            "-p".to_string(),
            identity,
            ";".to_string(),
        ];
        guarded.extend(args.iter().map(|arg| (*arg).to_string()));
        guarded
    }
}

impl TmuxRunner for GuardedSidebarTmuxRunner<'_> {
    fn run(&self, args: &[&str]) -> Result<String> {
        if !Self::is_read(args) {
            let guarded = self.guarded_mutation_args(args);
            let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
            let output = self.runner.run(&refs).map_err(|error| {
                if is_missing_pane_error(&error) {
                    anyhow::Error::new(SidebarGuardError::PaneInstanceMismatch)
                } else {
                    error
                }
            })?;
            if output
                .lines()
                .any(|line| line.trim() == SIDEBAR_SERVER_MISMATCH_SENTINEL)
            {
                return Err(SidebarGuardError::ServerIncarnationMismatch.into());
            }
            if output
                .lines()
                .any(|line| line.trim() == SIDEBAR_PANE_MISMATCH_SENTINEL)
            {
                return Err(SidebarGuardError::PaneInstanceMismatch.into());
            }
            return Ok(output);
        }

        let token = EventId::generate()?.as_str().to_string();
        let guarded = self.guarded_read_args(args, &token);
        let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
        let output = self.runner.run(&refs)?;
        let (identity, body) = output.split_once('\n').ok_or_else(|| {
            anyhow::anyhow!("sidebar tmux read did not return an identity envelope")
        })?;
        let expected = format!(
            "__vde_sidebar_identity_{token}__{}:{}",
            self.expected_server.pid, self.expected_server.start_time
        );
        if identity != expected {
            return Err(SidebarGuardError::ServerIncarnationMismatch.into());
        }
        Ok(body.to_string())
    }
}

fn classify_sidebar_error(error: anyhow::Error, pane: &PaneInstance) -> SidebarTmuxError {
    match error.downcast_ref::<SidebarGuardError>() {
        Some(SidebarGuardError::ServerIncarnationMismatch) => {
            SidebarTmuxError::ServerIncarnationMismatch
        }
        Some(SidebarGuardError::PaneInstanceMismatch) => {
            SidebarTmuxError::PaneInstanceMismatch(pane.pane_id.clone())
        }
        None if is_missing_pane_error(&error) => {
            SidebarTmuxError::PaneInstanceMismatch(pane.pane_id.clone())
        }
        None => SidebarTmuxError::Command(error),
    }
}

fn is_missing_pane_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("can't find pane") || message.contains("no such pane")
}

pub trait WorkerIo: Send + Sync + 'static {
    fn jump_to_pane(&self, pane: &PaneInstance) -> std::result::Result<(), SidebarTmuxError>;
    fn preview_pane(
        &self,
        pane: &PaneInstance,
        history_lines: u32,
    ) -> std::result::Result<(), SidebarTmuxError>;
}

trait TimedTmuxIo: Send + Sync {
    fn run_with_timeout(&self, args: &[&str], timeout: Duration) -> Result<String>;
}

#[derive(Debug, Clone)]
struct SystemTimedTmuxIo {
    socket_name: Option<String>,
}

impl TimedTmuxIo for SystemTimedTmuxIo {
    fn run_with_timeout(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let runner = self
            .socket_name
            .as_ref()
            .map(|name| SystemTmuxRunner::with_socket_name(name, Some(timeout)))
            .unwrap_or_else(|| SystemTmuxRunner::with_timeout(timeout));
        runner.run(args)
    }
}

struct JobBudgetTmuxRunner<'a> {
    io: &'a dyn TimedTmuxIo,
    deadline: Instant,
}

impl TmuxRunner for JobBudgetTmuxRunner<'_> {
    fn run(&self, args: &[&str]) -> Result<String> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| anyhow::anyhow!("sidebar tmux command exceeded its 2 second budget"))?;
        self.io.run_with_timeout(args, remaining)
    }
}

#[derive(Debug, Clone)]
pub struct SystemWorkerIo {
    io: SystemTimedTmuxIo,
    expected_server: ServerIdentity,
}

impl SystemWorkerIo {
    pub fn new(socket_name: Option<String>, expected_server: ServerIdentity) -> Self {
        Self {
            io: SystemTimedTmuxIo { socket_name },
            expected_server,
        }
    }
}

impl WorkerIo for SystemWorkerIo {
    fn jump_to_pane(&self, pane: &PaneInstance) -> std::result::Result<(), SidebarTmuxError> {
        let budgeted = JobBudgetTmuxRunner {
            io: &self.io,
            deadline: Instant::now() + SIDEBAR_JOB_TIMEOUT,
        };
        let guarded = GuardedSidebarTmuxRunner {
            runner: &budgeted,
            expected_server: &self.expected_server,
            expected_pane: pane,
        };
        jump_to_pane(&guarded, &pane.pane_id).map_err(|error| classify_sidebar_error(error, pane))
    }

    fn preview_pane(
        &self,
        pane: &PaneInstance,
        history_lines: u32,
    ) -> std::result::Result<(), SidebarTmuxError> {
        let env = std::env::vars().collect();
        let budgeted = JobBudgetTmuxRunner {
            io: &self.io,
            deadline: Instant::now() + SIDEBAR_JOB_TIMEOUT,
        };
        let guarded = GuardedSidebarTmuxRunner {
            runner: &budgeted,
            expected_server: &self.expected_server,
            expected_pane: pane,
        };
        crate::sidebar::preview::open_preview_floating_pane(
            &guarded,
            &env,
            &pane.pane_id,
            history_lines,
        )
        .map_err(|error| classify_sidebar_error(error, pane))
    }
}

pub fn system_git_runner(timeout: Duration) -> SystemGitRunner {
    SystemGitRunner::new(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;

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

    #[cfg(unix)]
    #[test]
    fn bounded_child_termination_does_not_block_the_poll_worker() {
        let child = Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();

        terminate_child_bounded(child, Duration::from_millis(100));

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn pane_instance(id: &str, pid: u32) -> PaneInstance {
        PaneInstance {
            pane_id: id.to_string(),
            pane_pid: pid,
        }
    }

    struct SidebarGuardRunner {
        actual_server: ServerIdentity,
        read_body: String,
        mutation_output: String,
        mutation_error: Option<String>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl SidebarGuardRunner {
        fn new(actual_server: ServerIdentity, read_body: impl Into<String>) -> Self {
            Self {
                actual_server,
                read_body: read_body.into(),
                mutation_output: String::new(),
                mutation_error: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_mutation_output(mut self, output: impl Into<String>) -> Self {
            self.mutation_output = output.into();
            self
        }

        fn with_mutation_error(mut self, error: impl Into<String>) -> Self {
            self.mutation_error = Some(error.into());
            self
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TmuxRunner for SidebarGuardRunner {
        fn run(&self, args: &[&str]) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            if args.first() == Some(&"display-message") && args.get(3) == Some(&";") {
                let identity = args[2]
                    .replace("#{pid}", &self.actual_server.pid.to_string())
                    .replace("#{start_time}", &self.actual_server.start_time.to_string());
                return Ok(format!("{identity}\n{}", self.read_body));
            }
            if let Some(error) = &self.mutation_error {
                anyhow::bail!(error.clone());
            }
            Ok(self.mutation_output.clone())
        }
    }

    #[derive(Default)]
    struct TimedTmuxRecorder {
        timeouts: Mutex<Vec<Duration>>,
    }

    impl TimedTmuxIo for TimedTmuxRecorder {
        fn run_with_timeout(&self, _args: &[&str], timeout: Duration) -> Result<String> {
            self.timeouts.lock().unwrap().push(timeout);
            Ok(String::new())
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
    fn sidebar_worker_wraps_reads_and_each_jump_mutation_in_server_and_pane_guards() {
        let runner = SidebarGuardRunner::new(server_identity(), "main\u{1f}@1\u{1f}%1\n");
        let pane = pane_instance("%1", 11);
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &server_identity(),
            expected_pane: &pane,
        };

        jump_to_pane(&guarded, "%1").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0][0], "display-message");
        assert_eq!(calls[0][3], ";");
        for call in &calls[1..] {
            assert_eq!(call[0], "if-shell");
            assert!(call[2].contains("#{pid},4242"), "{call:?}");
            assert!(call[2].contains("#{start_time},99"), "{call:?}");
            assert!(call[3].contains("#{pane_pid},11"), "{call:?}");
        }
        assert!(calls[1][3].contains("switch-client"));
        assert!(calls[2][3].contains("select-window"));
        assert!(calls[3][3].contains("select-pane"));
    }

    #[test]
    fn sidebar_worker_rejects_read_identity_mismatch_before_any_mutation() {
        let runner = SidebarGuardRunner::new(
            ServerIdentity {
                pid: 4243,
                start_time: 100,
            },
            "main\u{1f}@1\u{1f}%1\n",
        );
        let pane = pane_instance("%1", 11);
        let expected_server = server_identity();
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &expected_server,
            expected_pane: &pane,
        };

        let error = jump_to_pane(&guarded, "%1").unwrap_err();

        assert!(matches!(
            error.downcast_ref::<SidebarGuardError>(),
            Some(SidebarGuardError::ServerIncarnationMismatch)
        ));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "display-message");
    }

    #[test]
    fn sidebar_worker_reports_server_and_pane_guard_mismatches_without_direct_mutation() {
        let pane = pane_instance("%1", 11);
        let expected_server = server_identity();
        for (output, expected_server_mismatch) in [
            (SIDEBAR_SERVER_MISMATCH_SENTINEL, true),
            (SIDEBAR_PANE_MISMATCH_SENTINEL, false),
        ] {
            let runner = SidebarGuardRunner::new(expected_server.clone(), "")
                .with_mutation_output(format!("{output}\n"));
            let guarded = GuardedSidebarTmuxRunner {
                runner: &runner,
                expected_server: &expected_server,
                expected_pane: &pane,
            };

            let error = guarded.run(&["select-pane", "-t", "%1"]).unwrap_err();

            assert_eq!(
                matches!(
                    error.downcast_ref::<SidebarGuardError>(),
                    Some(SidebarGuardError::ServerIncarnationMismatch)
                ),
                expected_server_mismatch
            );
            let calls = runner.calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0][0], "if-shell");
            assert_ne!(calls[0][0], "select-pane");
            assert!(calls[0][3].contains("select-pane"));
        }
    }

    #[test]
    fn sidebar_worker_treats_target_disappearance_as_pane_mismatch_without_retrying_raw_command() {
        let pane = pane_instance("%1", 11);
        let expected_server = server_identity();
        let runner = SidebarGuardRunner::new(expected_server.clone(), "")
            .with_mutation_error("tmux failed: can't find pane: %1");
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &expected_server,
            expected_pane: &pane,
        };

        let error = guarded.run(&["select-pane", "-t", "%1"]).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<SidebarGuardError>(),
            Some(SidebarGuardError::PaneInstanceMismatch)
        ));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "if-shell");
        assert_ne!(calls[0][0], "select-pane");
    }

    #[test]
    fn sidebar_job_uses_one_shared_deadline_across_multiple_tmux_calls() {
        let io = TimedTmuxRecorder::default();
        let runner = JobBudgetTmuxRunner {
            io: &io,
            deadline: Instant::now() + Duration::from_millis(200),
        };

        runner.run(&["display-message", "-p", "one"]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        runner.run(&["display-message", "-p", "two"]).unwrap();

        let timeouts = io.timeouts.lock().unwrap();
        assert_eq!(timeouts.len(), 2);
        assert!(timeouts[0] <= Duration::from_millis(200));
        assert!(timeouts[1] < timeouts[0]);
        drop(timeouts);
        let expired = JobBudgetTmuxRunner {
            io: &io,
            deadline: Instant::now() - Duration::from_millis(1),
        };
        assert!(expired.run(&["display-message", "-p", "late"]).is_err());
        assert_eq!(io.timeouts.lock().unwrap().len(), 2);
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
}

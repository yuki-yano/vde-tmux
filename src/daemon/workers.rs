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

fn capture_identity_format(delimiter: &str) -> String {
    format!("__vde_capture_identity_{delimiter}__#{{pid}}:#{{start_time}}")
}

fn live_ok_marker(delimiter: &str) -> String {
    format!("__vde_live_ok_{delimiter}__")
}

fn live_mismatch_marker(delimiter: &str) -> String {
    format!("__vde_live_mismatch_{delimiter}__")
}

fn obs_ok_marker(delimiter: &str) -> String {
    format!("__vde_obs_ok_{delimiter}__")
}

fn job_boundary_marker(delimiter: &str) -> String {
    format!("__vde_job_{delimiter}__")
}

/// Per-target outcome of one live ANSI capture invocation. Section-level
/// failures never poison the other targets in the same tmux command group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveCaptureSection {
    Body(String),
    PaneInstanceMismatch,
    TargetMissing,
    Malformed,
}

/// One job inside a combined capture invocation. Observation jobs capture
/// plain tails with an all-or-nothing contract; live jobs capture ANSI bodies
/// with per-target isolation and a pane-PID guard inside the tmux command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureJobSpec {
    ObservationPlain { panes: Vec<PaneInstance> },
    LiveAnsi { targets: Vec<PaneInstance> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureJobOutcome {
    Observation(std::result::Result<Vec<String>, CaptureBatchError>),
    Live(Vec<LiveCaptureSection>),
}

/// Builds one framed tmux command group for every job. The server identity
/// header, job boundary markers, per-section separators, and per-section
/// success markers make the output self-describing, so job failures are
/// isolated without relying on the process exit code.
pub fn combined_capture_args(jobs: &[CaptureJobSpec], delimiter: &str) -> Vec<String> {
    let mut args = vec![
        "display-message".to_string(),
        "-p".to_string(),
        capture_identity_format(delimiter),
    ];
    for job in jobs {
        args.push(";".to_string());
        args.extend([
            "display-message".to_string(),
            "-p".to_string(),
            job_boundary_marker(delimiter),
        ]);
        match job {
            CaptureJobSpec::ObservationPlain { panes } => {
                for (index, pane) in panes.iter().enumerate() {
                    args.push(";".to_string());
                    if index > 0 {
                        args.extend([
                            "display-message".to_string(),
                            "-p".to_string(),
                            delimiter.to_string(),
                            ";".to_string(),
                        ]);
                    }
                    let capture = vec![
                        "capture-pane".to_string(),
                        "-p".to_string(),
                        "-S".to_string(),
                        CAPTURE_HISTORY_LINES.to_string(),
                        "-t".to_string(),
                        pane.pane_id.clone(),
                    ];
                    let ok_marker = vec![
                        "display-message".to_string(),
                        "-p".to_string(),
                        obs_ok_marker(delimiter),
                    ];
                    // The guard makes a vanished pane observable: when `-t`
                    // fails to resolve, the whole if-shell errors out and the
                    // confirmation marker never appears, which discards the
                    // observation job. A bare display-message would run even
                    // after a failed capture and hide the loss.
                    args.extend([
                        "if-shell".to_string(),
                        "-F".to_string(),
                        "-t".to_string(),
                        pane.pane_id.clone(),
                        "1".to_string(),
                        format!(
                            "{} ; {}",
                            crate::pane_state::store::tmux_command_string(&capture),
                            crate::pane_state::store::tmux_command_string(&ok_marker),
                        ),
                    ]);
                }
            }
            CaptureJobSpec::LiveAnsi { targets } => {
                for (index, target) in targets.iter().enumerate() {
                    args.push(";".to_string());
                    if index > 0 {
                        args.extend([
                            "display-message".to_string(),
                            "-p".to_string(),
                            delimiter.to_string(),
                            ";".to_string(),
                        ]);
                    }
                    let capture = vec![
                        "capture-pane".to_string(),
                        "-p".to_string(),
                        "-e".to_string(),
                        "-t".to_string(),
                        target.pane_id.clone(),
                    ];
                    let ok_marker = vec![
                        "display-message".to_string(),
                        "-p".to_string(),
                        live_ok_marker(delimiter),
                    ];
                    let mismatch_marker = vec![
                        "display-message".to_string(),
                        "-p".to_string(),
                        live_mismatch_marker(delimiter),
                    ];
                    args.extend([
                        "if-shell".to_string(),
                        "-F".to_string(),
                        "-t".to_string(),
                        target.pane_id.clone(),
                        format!("#{{==:#{{pane_pid}},{}}}", target.pane_pid),
                        format!(
                            "{} ; {}",
                            crate::pane_state::store::tmux_command_string(&capture),
                            crate::pane_state::store::tmux_command_string(&ok_marker),
                        ),
                        crate::pane_state::store::tmux_command_string(&mismatch_marker),
                    ]);
                }
            }
        }
    }
    args
}

/// Parses one combined capture invocation. The exit code and stderr are not
/// used for validation because a missing live target fails its own tmux
/// sub-command; correctness is judged from the self-describing stdout
/// structure instead. Only a bad identity header, an identity mismatch, or a
/// broken job frame fails every job in the invocation.
pub fn parse_combined_capture(
    output: CaptureBatchOutput,
    jobs: &[CaptureJobSpec],
    delimiter: &str,
    expected_identity: &ServerIdentity,
) -> std::result::Result<Vec<CaptureJobOutcome>, CaptureBatchError> {
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
    let boundary = job_boundary_marker(delimiter);
    let mut bodies: Vec<String> = Vec::new();
    for line in stdout.split_inclusive('\n') {
        let value = line.strip_suffix('\n').unwrap_or(line);
        let value = value.strip_suffix('\r').unwrap_or(value);
        if value == boundary {
            bodies.push(String::new());
        } else if let Some(body) = bodies.last_mut() {
            body.push_str(line);
        }
    }
    if bodies.len() != jobs.len() {
        return Err(CaptureBatchError::DelimiterMismatch {
            expected: jobs.len(),
            actual: bodies.len(),
        });
    }
    Ok(jobs
        .iter()
        .zip(bodies)
        .map(|(job, body)| match job {
            CaptureJobSpec::ObservationPlain { panes } => {
                CaptureJobOutcome::Observation(parse_observation_job(&body, panes.len(), delimiter))
            }
            CaptureJobSpec::LiveAnsi { targets } => {
                CaptureJobOutcome::Live(parse_live_job(&body, targets.len(), delimiter))
            }
        })
        .collect())
}

fn split_sections(body: &str, delimiter: &str) -> Vec<String> {
    let mut sections = vec![String::new()];
    for line in body.split_inclusive('\n') {
        let value = line.strip_suffix('\n').unwrap_or(line);
        let value = value.strip_suffix('\r').unwrap_or(value);
        if value == delimiter {
            sections.push(String::new());
        } else {
            sections
                .last_mut()
                .expect("sections always has one entry")
                .push_str(line);
        }
    }
    sections
}

/// All-or-nothing: any pane section without its success marker discards the
/// whole observation job, matching the standalone observation contract.
fn parse_observation_job(
    body: &str,
    pane_count: usize,
    delimiter: &str,
) -> std::result::Result<Vec<String>, CaptureBatchError> {
    if pane_count == 0 {
        return Ok(Vec::new());
    }
    let sections = split_sections(body, delimiter);
    if sections.len() != pane_count {
        return Err(CaptureBatchError::DelimiterMismatch {
            expected: pane_count.saturating_sub(1),
            actual: sections.len().saturating_sub(1),
        });
    }
    let ok_marker = obs_ok_marker(delimiter);
    let mut tails = Vec::with_capacity(pane_count);
    for section in sections {
        let mut lines = section.split_inclusive('\n').collect::<Vec<_>>();
        let confirmed = lines.last().is_some_and(|last| {
            let value = last.strip_suffix('\n').unwrap_or(last);
            value.strip_suffix('\r').unwrap_or(value) == ok_marker
        });
        if !confirmed {
            return Err(CaptureBatchError::ProcessFailed(None));
        }
        lines.pop();
        tails.push(lines.concat());
    }
    Ok(tails)
}

/// Per-target isolation: a broken frame downgrades every target of this job
/// to `Malformed` without touching the other jobs in the invocation.
fn parse_live_job(body: &str, target_count: usize, delimiter: &str) -> Vec<LiveCaptureSection> {
    if target_count == 0 {
        return Vec::new();
    }
    let sections = split_sections(body, delimiter);
    if sections.len() != target_count {
        return vec![LiveCaptureSection::Malformed; target_count];
    }
    let ok_marker = live_ok_marker(delimiter);
    let mismatch_marker = live_mismatch_marker(delimiter);
    sections
        .into_iter()
        .map(|section| classify_live_section(&section, &ok_marker, &mismatch_marker))
        .collect()
}

fn classify_live_section(
    section: &str,
    ok_marker: &str,
    mismatch_marker: &str,
) -> LiveCaptureSection {
    let mut lines = section.split_inclusive('\n').collect::<Vec<_>>();
    let Some(last) = lines.last() else {
        return LiveCaptureSection::TargetMissing;
    };
    let value = last.strip_suffix('\n').unwrap_or(last);
    let value = value.strip_suffix('\r').unwrap_or(value);
    if value == ok_marker {
        lines.pop();
        return LiveCaptureSection::Body(lines.concat());
    }
    if value == mismatch_marker {
        return LiveCaptureSection::PaneInstanceMismatch;
    }
    LiveCaptureSection::Malformed
}

/// The only production entry point for tmux capture subprocesses. Observation
/// polls and live previews request captures here; requests that arrive inside
/// the same coalesce window share a single tmux invocation.
pub trait CaptureSource: Send + Sync {
    fn capture_plain_tails(
        &self,
        panes: &[PaneInstance],
    ) -> std::result::Result<Vec<String>, CaptureBatchError>;
    fn capture_live_sections(
        &self,
        targets: &[PaneInstance],
    ) -> std::result::Result<Vec<LiveCaptureSection>, CaptureBatchError>;
}

pub const CAPTURE_COALESCE_WINDOW: Duration = Duration::from_millis(25);

enum CaptureRequest {
    ObservationPlain {
        panes: Vec<PaneInstance>,
        reply: mpsc::SyncSender<std::result::Result<Vec<String>, CaptureBatchError>>,
    },
    LiveAnsi {
        targets: Vec<PaneInstance>,
        reply: mpsc::SyncSender<std::result::Result<Vec<LiveCaptureSection>, CaptureBatchError>>,
    },
}

#[derive(Clone)]
pub struct CaptureCoordinatorHandle {
    tx: mpsc::Sender<CaptureRequest>,
}

impl CaptureCoordinatorHandle {
    /// Queues a live capture without blocking; the reply arrives on the
    /// returned channel once the coalesced invocation completes. Used by the
    /// observation worker to piggyback due live targets on its own capture.
    pub fn request_live_sections(
        &self,
        targets: Vec<PaneInstance>,
    ) -> mpsc::Receiver<std::result::Result<Vec<LiveCaptureSection>, CaptureBatchError>> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let _ = self.tx.send(CaptureRequest::LiveAnsi {
            targets,
            reply: reply_tx,
        });
        reply_rx
    }
}

impl CaptureSource for CaptureCoordinatorHandle {
    fn capture_plain_tails(
        &self,
        panes: &[PaneInstance],
    ) -> std::result::Result<Vec<String>, CaptureBatchError> {
        if panes.is_empty() {
            return Ok(Vec::new());
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.tx
            .send(CaptureRequest::ObservationPlain {
                panes: panes.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| CaptureBatchError::Io("capture coordinator is stopped".to_string()))?;
        reply_rx.recv().map_err(|_| {
            CaptureBatchError::Io("capture coordinator dropped the reply".to_string())
        })?
    }

    fn capture_live_sections(
        &self,
        targets: &[PaneInstance],
    ) -> std::result::Result<Vec<LiveCaptureSection>, CaptureBatchError> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        self.request_live_sections(targets.to_vec())
            .recv()
            .map_err(|_| {
                CaptureBatchError::Io("capture coordinator dropped the reply".to_string())
            })?
    }
}

pub fn start_capture_coordinator(
    io: std::sync::Arc<dyn ObservationWorkerIo>,
    expected_identity: ServerIdentity,
) -> CaptureCoordinatorHandle {
    let (tx, rx) = mpsc::channel::<CaptureRequest>();
    thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut requests = vec![first];
            let deadline = Instant::now() + CAPTURE_COALESCE_WINDOW;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match rx.recv_timeout(remaining) {
                    Ok(request) => requests.push(request),
                    Err(_) => break,
                }
            }
            execute_capture_group(io.as_ref(), &expected_identity, requests);
        }
    });
    CaptureCoordinatorHandle { tx }
}

/// tmux clients reject command sequences beyond roughly 1000 arguments
/// (measured on tmux 3.7: 993 accepted, 1008 rejected), so capture
/// invocations are planned against an argument budget with a safety margin
/// and large jobs are split across several invocations. The default
/// nine-sidebar / ~62-pane configuration fits in a single invocation.
const MAX_ARGS_PER_CAPTURE_INVOCATION: usize = 850;
/// Worst-case arguments one guarded observation capture adds: the command
/// separator, a section separator, and six if-shell arguments.
const ARGS_PER_OBSERVATION_ITEM: usize = 11;
/// Worst-case arguments one guarded live capture adds: the command separator,
/// a section separator, and seven if-shell arguments.
const ARGS_PER_LIVE_ITEM: usize = 12;
const ARGS_PER_JOB_HEADER: usize = 4;
const ARGS_PER_INVOCATION_HEADER: usize = 3;

/// Splits the coalesced requests into invocations that fit the tmux argument
/// budget. Each planned entry keeps the index of the request it came from so
/// partial results can be re-assembled per request.
fn plan_capture_invocations(requests: &[CaptureRequest]) -> Vec<Vec<(usize, CaptureJobSpec)>> {
    let mut invocations: Vec<Vec<(usize, CaptureJobSpec)>> = Vec::new();
    let mut current: Vec<(usize, CaptureJobSpec)> = Vec::new();
    let mut current_args = ARGS_PER_INVOCATION_HEADER;
    for (request_index, request) in requests.iter().enumerate() {
        let (items, live) = match request {
            CaptureRequest::ObservationPlain { panes, .. } => (panes, false),
            CaptureRequest::LiveAnsi { targets, .. } => (targets, true),
        };
        let item_args = if live {
            ARGS_PER_LIVE_ITEM
        } else {
            ARGS_PER_OBSERVATION_ITEM
        };
        let mut offset = 0;
        while offset < items.len() {
            let budget =
                MAX_ARGS_PER_CAPTURE_INVOCATION.saturating_sub(current_args + ARGS_PER_JOB_HEADER);
            let fits = budget / item_args;
            if fits == 0 {
                invocations.push(std::mem::take(&mut current));
                current_args = ARGS_PER_INVOCATION_HEADER;
                continue;
            }
            let take = fits.min(items.len() - offset);
            let slice = items[offset..offset + take].to_vec();
            current.push((
                request_index,
                if live {
                    CaptureJobSpec::LiveAnsi { targets: slice }
                } else {
                    CaptureJobSpec::ObservationPlain { panes: slice }
                },
            ));
            current_args += ARGS_PER_JOB_HEADER + take * item_args;
            offset += take;
        }
    }
    if !current.is_empty() {
        invocations.push(current);
    }
    invocations
}

fn execute_capture_group(
    io: &dyn ObservationWorkerIo,
    expected_identity: &ServerIdentity,
    requests: Vec<CaptureRequest>,
) {
    let mut observation_acc: BTreeMap<usize, std::result::Result<Vec<String>, CaptureBatchError>> =
        BTreeMap::new();
    let mut live_acc: BTreeMap<
        usize,
        std::result::Result<Vec<LiveCaptureSection>, CaptureBatchError>,
    > = BTreeMap::new();
    for (request_index, request) in requests.iter().enumerate() {
        match request {
            CaptureRequest::ObservationPlain { .. } => {
                observation_acc.insert(request_index, Ok(Vec::new()));
            }
            CaptureRequest::LiveAnsi { .. } => {
                live_acc.insert(request_index, Ok(Vec::new()));
            }
        }
    }

    let mut fatal: Option<CaptureBatchError> = None;
    for invocation in plan_capture_invocations(&requests) {
        let jobs = invocation
            .iter()
            .map(|(_, job)| job.clone())
            .collect::<Vec<_>>();
        let outcome = generate_capture_delimiter().and_then(|delimiter| {
            let output = io
                .capture_batch(&combined_capture_args(&jobs, &delimiter))
                .map_err(|error| CaptureBatchError::Io(error.to_string()))?;
            parse_combined_capture(output, &jobs, &delimiter, expected_identity)
        });
        match outcome {
            Ok(outcomes) => {
                for ((request_index, _), outcome) in invocation.iter().zip(outcomes) {
                    match outcome {
                        CaptureJobOutcome::Observation(result) => {
                            let accumulator = observation_acc
                                .get_mut(request_index)
                                .expect("observation slice maps to an observation request");
                            match (accumulator, result) {
                                (Ok(tails), Ok(more)) => tails.extend(more),
                                (accumulator @ Ok(_), Err(error)) => *accumulator = Err(error),
                                (Err(_), _) => {}
                            }
                        }
                        CaptureJobOutcome::Live(sections) => {
                            if let Some(Ok(accumulated)) = live_acc.get_mut(request_index) {
                                accumulated.extend(sections);
                            }
                        }
                    }
                }
            }
            Err(
                error @ (CaptureBatchError::IdentityMismatch { .. }
                | CaptureBatchError::InvalidIdentityHeader),
            ) => {
                // The tmux server is no longer the one this daemon owns:
                // stop capturing and fail every requester.
                fatal = Some(error);
                break;
            }
            Err(error) => {
                for (request_index, _) in &invocation {
                    if let Some(accumulator) = observation_acc.get_mut(request_index) {
                        *accumulator = Err(error.clone());
                    }
                    if let Some(accumulator) = live_acc.get_mut(request_index) {
                        *accumulator = Err(error.clone());
                    }
                }
            }
        }
    }

    for (request_index, request) in requests.into_iter().enumerate() {
        match request {
            CaptureRequest::ObservationPlain { reply, .. } => {
                let result = match &fatal {
                    Some(error) => Err(error.clone()),
                    None => observation_acc
                        .remove(&request_index)
                        .expect("every observation request has an accumulator"),
                };
                let _ = reply.send(result);
            }
            CaptureRequest::LiveAnsi { reply, .. } => {
                let result = match &fatal {
                    Some(error) => Err(error.clone()),
                    None => live_acc
                        .remove(&request_index)
                        .expect("every live request has an accumulator"),
                };
                let _ = reply.send(result);
            }
        }
    }
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
    source: &dyn CaptureSource,
    dispatch: &[ObservationDispatchSnapshot],
    processes: &AgentProcessSnapshot,
    daemon_instance_id: &DaemonInstanceId,
    observed_at: i64,
) -> std::result::Result<ObservationPollResult, ObservationPollError> {
    let panes = dispatch
        .iter()
        .map(|snapshot| snapshot.pane_instance.clone())
        .collect::<Vec<_>>();
    let (tails, mut diagnostics) = match source.capture_plain_tails(&panes) {
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
    SourceClientMismatch,
    Command(anyhow::Error),
}

impl std::fmt::Display for SidebarTmuxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerIncarnationMismatch => write!(formatter, "tmux server incarnation changed"),
            Self::PaneInstanceMismatch(pane_id) => {
                write!(formatter, "pane instance changed: {pane_id}")
            }
            Self::SourceClientMismatch => {
                write!(
                    formatter,
                    "source sidebar is no longer focused by the tmux client"
                )
            }
            Self::Command(error) => write!(formatter, "tmux command failed: {error:#}"),
        }
    }
}

impl std::error::Error for SidebarTmuxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Command(error) => Some(error.as_ref()),
            Self::ServerIncarnationMismatch
            | Self::PaneInstanceMismatch(_)
            | Self::SourceClientMismatch => None,
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
            Some("display-message" | "list-panes" | "list-clients")
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
    message.contains("can't find pane")
        || message.contains("no such pane")
        || message.contains("pane not found")
}

pub trait WorkerIo: Send + Sync + 'static {
    fn jump_to_pane(
        &self,
        pane: &PaneInstance,
        client_pid: u32,
        source_pane: &PaneInstance,
    ) -> std::result::Result<(), SidebarTmuxError>;
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
    fn jump_to_pane(
        &self,
        pane: &PaneInstance,
        client_pid: u32,
        source_pane: &PaneInstance,
    ) -> std::result::Result<(), SidebarTmuxError> {
        let budgeted = JobBudgetTmuxRunner {
            io: &self.io,
            deadline: Instant::now() + SIDEBAR_JOB_TIMEOUT,
        };
        let guarded = GuardedSidebarTmuxRunner {
            runner: &budgeted,
            expected_server: &self.expected_server,
            expected_pane: pane,
        };
        crate::sidebar::layout::jump_to_pane_for_client(&guarded, pane, client_pid, source_pane)
            .map_err(|error| {
                if error
                    .to_string()
                    .contains(crate::sidebar::layout::SOURCE_CLIENT_MISMATCH_SENTINEL)
                {
                    SidebarTmuxError::SourceClientMismatch
                } else {
                    classify_sidebar_error(error, pane)
                }
            })
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
        crate::sidebar::preview::open_preview_floating_pane(&guarded, &env, pane, history_lines)
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

    #[test]
    fn git_worker_runner_receives_configured_timeout() {
        let runner = system_git_runner(Duration::from_millis(1234));
        assert_eq!(runner.timeout(), Duration::from_millis(1234));
    }

    fn combined_stdout(
        delimiter: &str,
        identity: &ServerIdentity,
        job_bodies: &[String],
    ) -> String {
        let mut stdout = format!(
            "__vde_capture_identity_{delimiter}__{}:{}\n",
            identity.pid, identity.start_time
        );
        for body in job_bodies {
            stdout.push_str(&format!("__vde_job_{delimiter}__\n"));
            stdout.push_str(body);
        }
        stdout
    }

    #[test]
    fn combined_live_job_guards_every_pane_pid_in_one_invocation() {
        let targets = vec![
            PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 10,
            },
            PaneInstance {
                pane_id: "%2".to_string(),
                pane_pid: 20,
            },
        ];

        let args = combined_capture_args(&[CaptureJobSpec::LiveAnsi { targets }], "d1");

        let joined = args.join(" ");
        assert!(joined.contains("#{==:#{pane_pid},10}"));
        assert!(joined.contains("#{==:#{pane_pid},20}"));
        let guarded_captures = args
            .iter()
            .filter(|arg| arg.contains("capture-pane") && arg.contains("-e"))
            .collect::<Vec<_>>();
        assert_eq!(guarded_captures.len(), 2);
        assert!(guarded_captures[0].contains("%1"));
        assert!(guarded_captures[1].contains("%2"));
        assert_eq!(args.iter().filter(|arg| *arg == "if-shell").count(), 2);
    }

    #[test]
    fn combined_live_sections_isolate_per_target_failures() {
        let delimiter = "d1";
        let identity = ServerIdentity {
            pid: 1,
            start_time: 2,
        };
        let targets = vec![
            pane_instance("%1", 10),
            pane_instance("%2", 20),
            pane_instance("%3", 30),
            pane_instance("%4", 40),
        ];
        let body = format!(
            "\u{1b}[31mred\u{1b}[0m\nline2\n__vde_live_ok_{delimiter}__\n\
             {delimiter}\n\
             __vde_live_mismatch_{delimiter}__\n\
             {delimiter}\n\
             {delimiter}\n\
             partial output without marker\n"
        );

        let outcomes = parse_combined_capture(
            CaptureBatchOutput {
                exit_code: Some(1),
                stdout: combined_stdout(delimiter, &identity, &[body]),
                stderr: "can't find pane: %3".to_string(),
            },
            &[CaptureJobSpec::LiveAnsi { targets }],
            delimiter,
            &identity,
        )
        .unwrap();

        let CaptureJobOutcome::Live(sections) = &outcomes[0] else {
            panic!("expected a live outcome, found {outcomes:?}");
        };
        assert_eq!(
            sections[0],
            LiveCaptureSection::Body("\u{1b}[31mred\u{1b}[0m\nline2\n".to_string())
        );
        assert_eq!(sections[1], LiveCaptureSection::PaneInstanceMismatch);
        assert_eq!(sections[2], LiveCaptureSection::TargetMissing);
        assert_eq!(sections[3], LiveCaptureSection::Malformed);
    }

    #[test]
    fn combined_parse_isolates_observation_and_live_job_failures() {
        let delimiter = "d5";
        let identity = ServerIdentity {
            pid: 1,
            start_time: 2,
        };
        let jobs = [
            CaptureJobSpec::ObservationPlain {
                panes: vec![pane_instance("%1", 10), pane_instance("%2", 20)],
            },
            CaptureJobSpec::LiveAnsi {
                targets: vec![pane_instance("%3", 30)],
            },
        ];

        // The second observation pane never confirmed its capture, while the
        // live job in the same invocation is perfectly healthy.
        let broken_observation =
            format!("one\n__vde_obs_ok_{delimiter}__\n{delimiter}\ntwo without marker\n");
        let healthy_live = format!("live-body\n__vde_live_ok_{delimiter}__\n");
        let outcomes = parse_combined_capture(
            CaptureBatchOutput {
                exit_code: Some(1),
                stdout: combined_stdout(
                    delimiter,
                    &identity,
                    &[broken_observation, healthy_live.clone()],
                ),
                stderr: "can't find pane: %2".to_string(),
            },
            &jobs,
            delimiter,
            &identity,
        )
        .unwrap();
        assert!(matches!(
            &outcomes[0],
            CaptureJobOutcome::Observation(Err(_))
        ));
        assert_eq!(
            outcomes[1],
            CaptureJobOutcome::Live(vec![LiveCaptureSection::Body("live-body\n".to_string())])
        );

        // The reverse: a malformed live section leaves the observation intact.
        let healthy_observation = format!(
            "one\n__vde_obs_ok_{delimiter}__\n{delimiter}\ntwo\n__vde_obs_ok_{delimiter}__\n"
        );
        let broken_live = "garbage without any marker\n".to_string();
        let outcomes = parse_combined_capture(
            CaptureBatchOutput {
                exit_code: Some(0),
                stdout: combined_stdout(delimiter, &identity, &[healthy_observation, broken_live]),
                stderr: String::new(),
            },
            &jobs,
            delimiter,
            &identity,
        )
        .unwrap();
        assert_eq!(
            outcomes[0],
            CaptureJobOutcome::Observation(Ok(vec!["one\n".to_string(), "two\n".to_string()]))
        );
        assert_eq!(
            outcomes[1],
            CaptureJobOutcome::Live(vec![LiveCaptureSection::Malformed])
        );
    }

    #[test]
    fn combined_live_job_keeps_empty_body_as_success() {
        let delimiter = "d2";
        let identity = ServerIdentity {
            pid: 1,
            start_time: 2,
        };
        let body = format!("__vde_live_ok_{delimiter}__\n");

        let outcomes = parse_combined_capture(
            CaptureBatchOutput {
                exit_code: Some(0),
                stdout: combined_stdout(delimiter, &identity, &[body]),
                stderr: String::new(),
            },
            &[CaptureJobSpec::LiveAnsi {
                targets: vec![pane_instance("%1", 10)],
            }],
            delimiter,
            &identity,
        )
        .unwrap();

        assert_eq!(
            outcomes[0],
            CaptureJobOutcome::Live(vec![LiveCaptureSection::Body(String::new())])
        );
    }

    #[test]
    fn combined_parse_identity_mismatch_is_fatal_for_every_job() {
        let delimiter = "d3";
        let wrong_identity = ServerIdentity {
            pid: 9,
            start_time: 9,
        };
        let stdout = combined_stdout(
            delimiter,
            &wrong_identity,
            &[format!("__vde_live_ok_{delimiter}__\n")],
        );

        let error = parse_combined_capture(
            CaptureBatchOutput {
                exit_code: Some(0),
                stdout,
                stderr: String::new(),
            },
            &[CaptureJobSpec::LiveAnsi {
                targets: vec![pane_instance("%1", 10)],
            }],
            delimiter,
            &ServerIdentity {
                pid: 1,
                start_time: 2,
            },
        )
        .unwrap_err();

        assert!(matches!(error, CaptureBatchError::IdentityMismatch { .. }));
    }

    struct MockCaptureSource {
        plain_calls: Mutex<usize>,
        tails: Vec<String>,
    }

    impl CaptureSource for MockCaptureSource {
        fn capture_plain_tails(
            &self,
            _panes: &[PaneInstance],
        ) -> std::result::Result<Vec<String>, CaptureBatchError> {
            *self.plain_calls.lock().unwrap() += 1;
            Ok(self.tails.clone())
        }

        fn capture_live_sections(
            &self,
            _targets: &[PaneInstance],
        ) -> std::result::Result<Vec<LiveCaptureSection>, CaptureBatchError> {
            unreachable!("observation poll never captures live sections")
        }
    }

    struct ScriptedCombinedIo {
        calls: Mutex<usize>,
        script: Box<dyn Fn(&str) -> String + Send + Sync>,
    }

    impl ObservationWorkerIo for ScriptedCombinedIo {
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
                .expect("combined capture args carry an identity format");
            Ok(CaptureBatchOutput {
                exit_code: Some(0),
                stdout: (self.script)(&delimiter),
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
        client_read_body: Option<String>,
        mutation_output: String,
        mutation_error: Option<String>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl SidebarGuardRunner {
        fn new(actual_server: ServerIdentity, read_body: impl Into<String>) -> Self {
            Self {
                actual_server,
                read_body: read_body.into(),
                client_read_body: None,
                mutation_output: String::new(),
                mutation_error: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_mutation_output(mut self, output: impl Into<String>) -> Self {
            self.mutation_output = output.into();
            self
        }

        fn with_client_read_body(mut self, output: impl Into<String>) -> Self {
            self.client_read_body = Some(output.into());
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
                let body = if args.contains(&"list-clients") {
                    self.client_read_body.as_ref().unwrap_or(&self.read_body)
                } else {
                    &self.read_body
                };
                return Ok(format!("{identity}\n{body}"));
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
    fn capture_coordinator_coalesces_live_and_observation_into_one_invocation() {
        let io = std::sync::Arc::new(ScriptedCombinedIo {
            calls: Mutex::new(0),
            script: Box::new(|delimiter| {
                let identity = server_identity();
                let live_body = format!("live-body\n__vde_live_ok_{delimiter}__\n");
                let observation_body = format!(
                    "one\n__vde_obs_ok_{delimiter}__\n{delimiter}\ntwo\n__vde_obs_ok_{delimiter}__\n"
                );
                format!(
                    "__vde_capture_identity_{delimiter}__{}:{}\n__vde_job_{delimiter}__\n{live_body}__vde_job_{delimiter}__\n{observation_body}",
                    identity.pid, identity.start_time
                )
            }),
        });
        let handle = start_capture_coordinator(io.clone(), server_identity());

        // The live request is queued first without blocking; the observation
        // request arrives within the coalesce window and shares the invocation.
        let live_rx = handle.request_live_sections(vec![pane_instance("%9", 90)]);
        let tails = handle
            .capture_plain_tails(&[pane_instance("%1", 11), pane_instance("%2", 22)])
            .unwrap();

        assert_eq!(tails, vec!["one\n".to_string(), "two\n".to_string()]);
        assert_eq!(
            live_rx.recv().unwrap().unwrap(),
            vec![LiveCaptureSection::Body("live-body\n".to_string())]
        );
        assert_eq!(*io.calls.lock().unwrap(), 1);
    }

    #[derive(Default)]
    struct SynthesizingCombinedIo {
        calls: Mutex<Vec<usize>>,
    }

    impl ObservationWorkerIo for SynthesizingCombinedIo {
        fn capture_batch(&self, args: &[String]) -> anyhow::Result<CaptureBatchOutput> {
            self.calls.lock().unwrap().push(args.len());
            let delimiter = args
                .iter()
                .find_map(|value| {
                    value
                        .strip_prefix("__vde_capture_identity_")
                        .and_then(|value| value.split_once("__"))
                        .map(|(delimiter, _)| delimiter.to_string())
                })
                .expect("combined capture args carry an identity format");
            let identity = server_identity();
            let boundary = format!("__vde_job_{delimiter}__");
            let mut stdout = format!(
                "__vde_capture_identity_{delimiter}__{}:{}\n",
                identity.pid, identity.start_time
            );
            let mut index = 3;
            while index < args.len() {
                match args[index].as_str() {
                    "display-message" => {
                        let payload = &args[index + 2];
                        if *payload == boundary {
                            stdout.push_str(&format!("{boundary}\n"));
                        } else if *payload == delimiter {
                            stdout.push_str(&format!("{delimiter}\n"));
                        }
                        index += 3;
                    }
                    "if-shell" => {
                        let pane = args[index + 3].clone();
                        let guard = args[index + 4].clone();
                        if guard == "1" {
                            stdout.push_str(&format!("tail-{pane}\n__vde_obs_ok_{delimiter}__\n"));
                            index += 6;
                        } else {
                            stdout.push_str(&format!("live-{pane}\n__vde_live_ok_{delimiter}__\n"));
                            index += 7;
                        }
                    }
                    _ => index += 1,
                }
            }
            Ok(CaptureBatchOutput {
                exit_code: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn capture_coordinator_splits_oversized_jobs_and_reassembles_results() {
        let io = std::sync::Arc::new(SynthesizingCombinedIo::default());
        let handle = start_capture_coordinator(io.clone(), server_identity());
        let panes = (0..100)
            .map(|index| pane_instance(&format!("%{index}"), 1000 + index as u32))
            .collect::<Vec<_>>();

        let live_rx = handle.request_live_sections(vec![pane_instance("%900", 9000)]);
        let tails = handle.capture_plain_tails(&panes).unwrap();

        assert_eq!(tails.len(), 100);
        assert_eq!(tails[0], "tail-%0\n");
        assert_eq!(tails[99], "tail-%99\n");
        assert_eq!(
            live_rx.recv().unwrap().unwrap(),
            vec![LiveCaptureSection::Body("live-%900\n".to_string())]
        );
        let calls = io.calls.lock().unwrap();
        assert!(
            calls.len() >= 2,
            "100 panes must span more than one invocation, saw {calls:?}"
        );
        for count in calls.iter() {
            assert!(
                *count <= MAX_ARGS_PER_CAPTURE_INVOCATION,
                "invocation argument count {count} exceeds the tmux budget"
            );
        }
    }

    #[test]
    fn default_scale_poll_plans_a_single_invocation() {
        let (observation_reply, _observation_rx) = mpsc::sync_channel(1);
        let (live_reply, _live_rx) = mpsc::sync_channel(1);
        let requests = vec![
            CaptureRequest::ObservationPlain {
                panes: (0..62)
                    .map(|index| pane_instance(&format!("%{index}"), 100 + index as u32))
                    .collect(),
                reply: observation_reply,
            },
            CaptureRequest::LiveAnsi {
                targets: (0..9)
                    .map(|index| pane_instance(&format!("%{}", 900 + index), 900 + index as u32))
                    .collect(),
                reply: live_reply,
            },
        ];

        let plan = plan_capture_invocations(&requests);

        // The baseline 62-pane, nine-sidebar configuration must not need an
        // extra tmux process for live capture.
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn combined_observation_job_guards_pane_resolution_per_section() {
        let args = combined_capture_args(
            &[CaptureJobSpec::ObservationPlain {
                panes: vec![pane_instance("%1", 10), pane_instance("%2", 20)],
            }],
            "d1",
        );

        // Each pane capture sits behind an if-shell so a vanished pane leaves
        // no confirmation marker instead of silently producing an empty tail.
        assert_eq!(args.iter().filter(|arg| *arg == "if-shell").count(), 2);
        let guarded = args
            .iter()
            .filter(|arg| arg.contains("capture-pane") && arg.contains("__vde_obs_ok_"))
            .collect::<Vec<_>>();
        assert_eq!(guarded.len(), 2);
        assert!(guarded[0].contains("%1"));
        assert!(guarded[1].contains("%2"));
    }

    #[test]
    fn capture_coordinator_runs_unaligned_requests_as_separate_invocations() {
        let io = std::sync::Arc::new(ScriptedCombinedIo {
            calls: Mutex::new(0),
            script: Box::new(|delimiter| {
                let identity = server_identity();
                format!(
                    "__vde_capture_identity_{delimiter}__{}:{}\n__vde_job_{delimiter}__\nlive-body\n__vde_live_ok_{delimiter}__\n",
                    identity.pid, identity.start_time
                )
            }),
        });
        let handle = start_capture_coordinator(io.clone(), server_identity());

        let first = handle
            .capture_live_sections(&[pane_instance("%9", 90)])
            .unwrap();
        // The second request arrives long after the first invocation's window.
        let second = handle
            .capture_live_sections(&[pane_instance("%9", 90)])
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(*io.calls.lock().unwrap(), 2);
    }

    #[test]
    fn sidebar_worker_wraps_atomic_jump_in_server_and_target_pane_guards() {
        let runner = SidebarGuardRunner::new(server_identity(), "$1\u{1f}@1\u{1f}%1\u{1f}11\n");
        let pane = pane_instance("%1", 11);
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &server_identity(),
            expected_pane: &pane,
        };

        crate::sidebar::layout::jump_to_pane(&guarded, "%1").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][0], "display-message");
        assert_eq!(calls[0][3], ";");
        assert_eq!(calls[1][0], "if-shell");
        assert!(calls[1][2].contains("#{pid},4242"), "{:?}", calls[1]);
        assert!(calls[1][2].contains("#{start_time},99"), "{:?}", calls[1]);
        assert!(calls[1][3].contains("#{pane_pid},11"), "{:?}", calls[1]);
        assert!(calls[1][3].contains("switch-client"));
        assert!(calls[1][3].contains("$1:@1.%1"));
        assert!(!calls[1][3].contains("select-window"));
        assert!(!calls[1][3].contains("select-pane"));
    }

    #[test]
    fn sidebar_worker_checks_target_and_source_instances_in_one_atomic_jump_mutation() {
        let runner = SidebarGuardRunner::new(server_identity(), "$1\u{1f}@1\u{1f}%1\u{1f}11\n")
            .with_client_read_body("20\u{1f}/dev/ttys002\n");
        let target = pane_instance("%1", 11);
        let source = pane_instance("%9", 909);
        let expected_server = server_identity();
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &expected_server,
            expected_pane: &target,
        };

        crate::sidebar::layout::jump_to_pane_for_client(&guarded, &target, 20, &source).unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0][0], "display-message");
        assert_eq!(calls[1][0], "display-message");
        assert_eq!(calls[2][0], "if-shell");
        let guarded_command = &calls[2][3];
        assert!(guarded_command.contains("#{pane_pid},11"), "{calls:?}");
        assert!(guarded_command.contains("#{pane_id},%9"), "{calls:?}");
        assert!(guarded_command.contains("#{pane_pid},909"), "{calls:?}");
        assert!(guarded_command.contains("switch-client"), "{calls:?}");
        assert!(guarded_command.contains("$1:@1.%1"), "{calls:?}");
        assert!(!guarded_command.contains("select-window"), "{calls:?}");
        assert!(!guarded_command.contains("select-pane"), "{calls:?}");
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

        let error = crate::sidebar::layout::jump_to_pane(&guarded, "%1").unwrap_err();

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

    fn observation_outcome(
        stdout_body: &str,
        pane_count: usize,
        delimiter: &str,
    ) -> std::result::Result<Vec<String>, CaptureBatchError> {
        let panes = (0..pane_count)
            .map(|index| pane_instance(&format!("%{index}"), 10 + index as u32))
            .collect::<Vec<_>>();
        let outcomes = parse_combined_capture(
            CaptureBatchOutput {
                exit_code: Some(1),
                stdout: combined_stdout(delimiter, &server_identity(), &[stdout_body.to_string()]),
                stderr: "pane vanished".to_string(),
            },
            &[CaptureJobSpec::ObservationPlain { panes }],
            delimiter,
            &server_identity(),
        )
        .unwrap();
        let CaptureJobOutcome::Observation(result) = outcomes.into_iter().next().unwrap() else {
            panic!("expected an observation outcome");
        };
        result
    }

    #[test]
    fn observation_job_rejects_missing_confirmations_and_delimiter_races() {
        let delimiter = "00112233445566778899aabbccddeeff";
        // Sections without a confirmation marker or with a delimiter collision
        // discard the whole observation job.
        assert!(observation_outcome("", 2, delimiter).is_err());
        assert!(observation_outcome("first only\n", 2, delimiter).is_err());
        assert!(
            observation_outcome(
                &format!("first\n{delimiter}\ncollision\n{delimiter}\nsecond\n"),
                2,
                delimiter
            )
            .is_err()
        );
    }

    #[test]
    fn observation_job_discards_all_when_first_middle_or_last_pane_disappears() {
        let delimiter = "00112233445566778899aabbccddeeff";
        let ok = format!("__vde_obs_ok_{delimiter}__");
        let first_missing = format!("{delimiter}\nsecond\n{ok}\n{delimiter}\nthird\n{ok}\n");
        let middle_missing = format!("first\n{ok}\n{delimiter}\n{delimiter}\nthird\n{ok}\n");
        let last_missing = format!("first\n{ok}\n{delimiter}\nsecond\n{ok}\n{delimiter}\n");
        assert!(observation_outcome(&first_missing, 3, delimiter).is_err());
        assert!(observation_outcome(&middle_missing, 3, delimiter).is_err());
        assert!(observation_outcome(&last_missing, 3, delimiter).is_err());

        let all_present =
            format!("first\n{ok}\n{delimiter}\nsecond\n{ok}\n{delimiter}\nthird\n{ok}\n");
        assert_eq!(
            observation_outcome(&all_present, 3, delimiter).unwrap(),
            vec![
                "first\n".to_string(),
                "second\n".to_string(),
                "third\n".to_string()
            ]
        );
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
        let source = MockCaptureSource {
            plain_calls: Mutex::new(0),
            tails: vec!["after\n".to_string()],
        };
        let processes = AgentProcessSnapshot::parse("11 1 opencode\n", true);
        let result = run_observation_poll(
            &source,
            &dispatch,
            &processes,
            &DaemonInstanceId::parse("ffeeddccbbaa99887766554433221100").unwrap(),
            200,
        )
        .unwrap();
        assert_eq!(*source.plain_calls.lock().unwrap(), 1);
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

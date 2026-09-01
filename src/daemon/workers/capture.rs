use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::daemon::topology::ServerIdentity;
use crate::pane_state::{EventId, PaneInstance};
use crate::tmux::tmux_args;

pub const CAPTURE_HISTORY_LINES: &str = "-80";
pub const OBSERVATION_CAPTURE_STDOUT_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const OBSERVATION_CAPTURE_STDERR_MAX_BYTES: usize = 64 * 1024;
pub const OBSERVATION_CAPTURE_GROUP_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureBatchOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub trait ObservationWorkerIo: Send + Sync + 'static {
    fn capture_batch(
        &self,
        args: &[String],
    ) -> std::result::Result<CaptureBatchOutput, CaptureBatchError>;
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
    fn capture_batch(
        &self,
        args: &[String],
    ) -> std::result::Result<CaptureBatchOutput, CaptureBatchError> {
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let tmux_args = tmux_args(self.socket_name.as_deref(), &refs);
        let mut child = std::process::Command::new("tmux")
            .args(tmux_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Own process group so a descendant that inherited the capture pipes
            // is killed with the child, letting the reader threads reach EOF.
            .process_group(0)
            .spawn()
            .map_err(|error| CaptureBatchError::Io(error.to_string()))?;
        let stdout = child.stdout.take().map(|stdout| {
            thread::spawn(move || {
                read_capture_pipe_bounded(stdout, OBSERVATION_CAPTURE_STDOUT_MAX_BYTES)
            })
        });
        let stderr = child.stderr.take().map(|stderr| {
            thread::spawn(move || {
                read_capture_pipe_bounded(stderr, OBSERVATION_CAPTURE_STDERR_MAX_BYTES)
            })
        });
        // On every path, kill the whole process group before reaping the child
        // so a descendant holding the capture pipes dies and the readers reach
        // EOF; the reads are then always joined, never detached.
        let status = crate::proc::await_exit_then_kill_group(&mut child, self.timeout);
        // Join both readers before propagating any error so no thread is left
        // detached on the error path.
        let stdout = collect_capture_reader("stdout", stdout);
        let stderr = collect_capture_reader("stderr", stderr);
        let status = status
            .map_err(|error| CaptureBatchError::Io(error.to_string()))?
            .ok_or_else(|| {
                CaptureBatchError::Io(format!(
                    "tmux capture batch timed out after {:?}",
                    self.timeout
                ))
            })?;
        let stdout = stdout?;
        let stderr = stderr?;
        Ok(CaptureBatchOutput {
            exit_code: status.code(),
            stdout: String::from_utf8(stdout.bytes)
                .map_err(|error| CaptureBatchError::Io(error.to_string()))?,
            stderr: String::from_utf8(stderr.bytes)
                .map_err(|error| CaptureBatchError::Io(error.to_string()))?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CaptureReaderOutput {
    bytes: Vec<u8>,
    total_bytes: usize,
    exceeded: bool,
}

fn read_capture_pipe_bounded(
    mut reader: impl Read,
    limit: usize,
) -> std::io::Result<CaptureReaderOutput> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut total_bytes = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(count);
        let retained = limit.saturating_sub(bytes.len()).min(count);
        bytes.extend_from_slice(&buffer[..retained]);
    }
    Ok(CaptureReaderOutput {
        bytes,
        total_bytes,
        exceeded: total_bytes > limit,
    })
}

fn collect_capture_reader(
    label: &str,
    reader: Option<thread::JoinHandle<std::io::Result<CaptureReaderOutput>>>,
) -> std::result::Result<CaptureReaderOutput, CaptureBatchError> {
    let output = reader
        .ok_or_else(|| CaptureBatchError::Io(format!("capture {label} was not piped")))?
        .join()
        .map_err(|_| CaptureBatchError::Io(format!("capture {label} reader panicked")))?
        .map_err(|error| CaptureBatchError::Io(error.to_string()))?;
    if output.exceeded {
        return Err(CaptureBatchError::OutputLimit {
            scope: format!("capture {label}"),
            actual: output.total_bytes,
            limit: if label == "stdout" {
                OBSERVATION_CAPTURE_STDOUT_MAX_BYTES
            } else {
                OBSERVATION_CAPTURE_STDERR_MAX_BYTES
            },
        });
    }
    Ok(output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureBatchError {
    Random(String),
    Io(String),
    ObservationQueueFull,
    OutputLimit {
        scope: String,
        actual: usize,
        limit: usize,
    },
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
            Self::ObservationQueueFull => {
                formatter.write_str("daemon observation capture queue is full")
            }
            Self::OutputLimit {
                scope,
                actual,
                limit,
            } => write!(
                formatter,
                "{scope} exceeded byte limit: {actual} bytes > {limit} bytes"
            ),
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

fn obs_ok_marker(delimiter: &str) -> String {
    format!("__vde_obs_ok_{delimiter}__")
}

fn job_boundary_marker(delimiter: &str) -> String {
    format!("__vde_job_{delimiter}__")
}

/// One job inside a combined capture invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureJobSpec {
    ObservationPlain { panes: Vec<PaneInstance> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureJobOutcome {
    Observation(std::result::Result<Vec<String>, CaptureBatchError>),
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
        }
    }
    args
}

/// Parses one combined capture invocation. The exit code and stderr are not
/// used for validation; correctness is judged from the self-describing stdout
/// structure instead.
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

/// The only production entry point for tmux capture subprocesses. Observation
/// polls that arrive inside the same coalesce window share one invocation.
pub trait CaptureSource: Send + Sync {
    fn capture_plain_tails(
        &self,
        panes: &[PaneInstance],
    ) -> std::result::Result<Vec<String>, CaptureBatchError>;
}

pub const CAPTURE_COALESCE_WINDOW: Duration = Duration::from_millis(25);
/// Bounds daemon observation capture requests only. Other tmux command paths
/// use their own queues and resource limits.
const DAEMON_OBSERVATION_CAPTURE_QUEUE_CAPACITY: usize = 8;

enum CaptureRequest {
    ObservationPlain {
        panes: Vec<PaneInstance>,
        reply: mpsc::SyncSender<std::result::Result<Vec<String>, CaptureBatchError>>,
    },
}

#[derive(Clone)]
pub struct CaptureCoordinatorHandle {
    tx: mpsc::SyncSender<CaptureRequest>,
}

impl CaptureCoordinatorHandle {
    fn try_enqueue(&self, request: CaptureRequest) -> Result<(), CaptureBatchError> {
        self.tx.try_send(request).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => CaptureBatchError::ObservationQueueFull,
            mpsc::TrySendError::Disconnected(_) => {
                CaptureBatchError::Io("capture coordinator is stopped".to_string())
            }
        })
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
        self.try_enqueue(CaptureRequest::ObservationPlain {
            panes: panes.to_vec(),
            reply: reply_tx,
        })?;
        reply_rx.recv().map_err(|_| {
            CaptureBatchError::Io("capture coordinator dropped the reply".to_string())
        })?
    }
}

pub fn start_capture_coordinator(
    io: std::sync::Arc<dyn ObservationWorkerIo>,
    expected_identity: ServerIdentity,
) -> CaptureCoordinatorHandle {
    let (tx, rx) = mpsc::sync_channel::<CaptureRequest>(DAEMON_OBSERVATION_CAPTURE_QUEUE_CAPACITY);
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
        let CaptureRequest::ObservationPlain { panes: items, .. } = request;
        let item_args = ARGS_PER_OBSERVATION_ITEM;
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
                CaptureJobSpec::ObservationPlain { panes: slice },
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
    for (request_index, request) in requests.iter().enumerate() {
        match request {
            CaptureRequest::ObservationPlain { .. } => {
                observation_acc.insert(request_index, Ok(Vec::new()));
            }
        }
    }

    let mut fatal: Option<CaptureBatchError> = None;
    let mut retained_group_bytes = 0usize;
    'invocations: for invocation in plan_capture_invocations(&requests) {
        let jobs = invocation
            .iter()
            .map(|(_, job)| job.clone())
            .collect::<Vec<_>>();
        let outcome = generate_capture_delimiter().and_then(|delimiter| {
            let output = io.capture_batch(&combined_capture_args(&jobs, &delimiter))?;
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
                                (Ok(tails), Ok(more)) => {
                                    let added = more.iter().map(String::len).sum::<usize>();
                                    if let Err(error) =
                                        add_retained_capture_bytes(&mut retained_group_bytes, added)
                                    {
                                        fatal = Some(error);
                                        break 'invocations;
                                    }
                                    tails.extend(more);
                                }
                                (accumulator @ Ok(_), Err(error)) => *accumulator = Err(error),
                                (Err(_), _) => {}
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
        }
    }
}

fn add_retained_capture_bytes(
    retained: &mut usize,
    added: usize,
) -> std::result::Result<(), CaptureBatchError> {
    *retained = retained.saturating_add(added);
    if *retained > OBSERVATION_CAPTURE_GROUP_MAX_BYTES {
        return Err(CaptureBatchError::OutputLimit {
            scope: "coalesced observation group".to_string(),
            actual: *retained,
            limit: OBSERVATION_CAPTURE_GROUP_MAX_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::daemon::workers::tests::{pane_instance, server_identity};

    #[test]
    fn observation_capture_reader_drains_but_retains_only_the_limit() {
        let input = vec![b'x'; 4096];
        let output = read_capture_pipe_bounded(input.as_slice(), 1024).unwrap();

        assert_eq!(output.bytes.len(), 1024);
        assert_eq!(output.total_bytes, 4096);
        assert!(output.exceeded);
    }

    #[test]
    fn observation_capture_reader_reports_a_typed_stream_limit() {
        let reader = thread::spawn(|| {
            Ok(CaptureReaderOutput {
                bytes: vec![b'x'; OBSERVATION_CAPTURE_STDOUT_MAX_BYTES],
                total_bytes: OBSERVATION_CAPTURE_STDOUT_MAX_BYTES + 1,
                exceeded: true,
            })
        });

        let error = collect_capture_reader("stdout", Some(reader)).unwrap_err();
        assert_eq!(
            error,
            CaptureBatchError::OutputLimit {
                scope: "capture stdout".to_string(),
                actual: OBSERVATION_CAPTURE_STDOUT_MAX_BYTES + 1,
                limit: OBSERVATION_CAPTURE_STDOUT_MAX_BYTES,
            }
        );
    }

    #[test]
    fn observation_capture_group_has_a_typed_total_byte_limit() {
        let mut retained = OBSERVATION_CAPTURE_GROUP_MAX_BYTES - 1;
        add_retained_capture_bytes(&mut retained, 1).unwrap();
        let error = add_retained_capture_bytes(&mut retained, 1).unwrap_err();

        assert_eq!(
            error,
            CaptureBatchError::OutputLimit {
                scope: "coalesced observation group".to_string(),
                actual: OBSERVATION_CAPTURE_GROUP_MAX_BYTES + 1,
                limit: OBSERVATION_CAPTURE_GROUP_MAX_BYTES,
            }
        );
    }

    #[test]
    fn observation_capture_queue_rejects_full_without_blocking() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let handle = CaptureCoordinatorHandle { tx };
        let (first_reply, _first_rx) = mpsc::sync_channel(1);
        handle
            .try_enqueue(CaptureRequest::ObservationPlain {
                panes: vec![pane_instance("%1", 10)],
                reply: first_reply,
            })
            .unwrap();

        let error = handle
            .capture_plain_tails(&[pane_instance("%2", 20)])
            .unwrap_err();

        assert_eq!(error, CaptureBatchError::ObservationQueueFull);
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
        let CaptureJobOutcome::Observation(result) = outcomes.into_iter().next().unwrap();
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
}

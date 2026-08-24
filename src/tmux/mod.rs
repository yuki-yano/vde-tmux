use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const INPUT_COMMAND_MAX_STDOUT_BYTES: usize = 16 * 1024;
pub const INPUT_COMMAND_MAX_STDERR_BYTES: usize = 16 * 1024;
const AGENT_PROCESS_RESOLVE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputWriteStage {
    BeforeSpawn,
    AfterSpawnBeforeWrite,
    AfterPartialWrite,
    AfterFullWrite,
}

#[derive(Debug)]
pub struct InputCommandError {
    pub stage: InputWriteStage,
    source: anyhow::Error,
}

impl InputCommandError {
    pub fn new(stage: InputWriteStage, source: impl Into<anyhow::Error>) -> Self {
        Self {
            stage,
            source: source.into(),
        }
    }
}

impl std::fmt::Display for InputCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for InputCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[cfg(test)]
pub mod mock;

pub trait TmuxRunner {
    fn run(&self, args: &[&str]) -> Result<String>;

    fn run_with_input(
        &self,
        _args: &[&str],
        _input: &[u8],
    ) -> std::result::Result<String, InputCommandError> {
        Err(InputCommandError::new(
            InputWriteStage::BeforeSpawn,
            anyhow::anyhow!("tmux runner does not support piped input"),
        ))
    }

    fn verify_agent_input_owner(&self, root_pid: u32, agent_pid: u32) -> Result<()> {
        let processes =
            crate::daemon::workers::read_agent_process_snapshot(Duration::from_secs(1), false);
        match processes.is_foreground_process_owner(root_pid, agent_pid) {
            Some(true) => Ok(()),
            Some(false) => bail!(
                "agent process {agent_pid} is not the foreground input owner for pane root {root_pid}"
            ),
            None => bail!("agent input owner process scan was incomplete"),
        }
    }

    fn resolve_agent_process(
        &self,
        root_pid: u32,
        agent: &crate::pane_state::AgentKind,
    ) -> Result<Option<crate::pane_state::AgentProcessIdentity>> {
        let processes = crate::daemon::workers::read_agent_process_snapshot(
            AGENT_PROCESS_RESOLVE_TIMEOUT,
            false,
        );
        let detection = processes.detect_from_pid_tree(root_pid);
        if !detection.complete || !detection.process_identities_complete {
            bail!("agent process scan was incomplete");
        }
        Ok(detection.exact_agent_process(agent))
    }

    fn run_bounded(&self, args: &[&str], max_stdout_bytes: usize) -> Result<BoundedOutput> {
        let output = self.run(args)?;
        Ok(bound_string(output, max_stdout_bytes))
    }

    fn run_tail_bounded(&self, args: &[&str], max_stdout_bytes: usize) -> Result<BoundedOutput> {
        let output = self.run(args)?;
        Ok(bound_string_tail(output, max_stdout_bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    pub text: String,
    pub total_bytes: usize,
    pub truncated: bool,
}

pub fn run_command(program: &str, args: &[&str], timeout: Option<Duration>) -> Result<String> {
    run_command_with_output_limit(program, args, timeout, None)
}

pub fn run_command_with_output_limit(
    program: &str,
    args: &[&str],
    timeout: Option<Duration>,
    max_stdout_bytes: Option<usize>,
) -> Result<String> {
    run_command_with_optional_input(program, args, None, timeout, max_stdout_bytes)
}

pub fn run_command_with_input_and_output_limits(
    program: &str,
    args: &[&str],
    input: &[u8],
    timeout: Option<Duration>,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> std::result::Result<String, InputCommandError> {
    run_command_with_input_limits(
        program,
        args,
        input,
        timeout,
        max_stdout_bytes,
        max_stderr_bytes,
    )
}

fn run_command_with_optional_input(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: Option<Duration>,
    max_stdout_bytes: Option<usize>,
) -> Result<String> {
    let mut command = command_with_timeout_group(program, timeout);
    let mut child = command
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

    let stdout = child
        .stdout
        .take()
        .map(|stdout| read_pipe_in_background(stdout, max_stdout_bytes, Retention::Prefix));
    let stderr = child
        .stderr
        .take()
        .map(|stderr| read_pipe_in_background(stderr, None, Retention::Prefix));
    let input_cancelled = Arc::new(AtomicBool::new(false));
    let stdin = input.and_then(|input| {
        child
            .stdin
            .take()
            .map(|stdin| write_pipe_in_background(stdin, input.to_vec(), input_cancelled.clone()))
    });

    let status = wait_for_child(program, &mut child, timeout);
    input_cancelled.store(true, Ordering::Release);
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }

    let stdin = collect_pipe_input(stdin);
    let stdout = collect_pipe_output(stdout);
    let stderr = collect_pipe_output(stderr);
    let status = status?;
    if stdout.exceeded {
        bail!(
            "{program} stdout exceeded byte limit: {actual} bytes > {limit} bytes",
            actual = stdout.total_bytes,
            limit = max_stdout_bytes.unwrap_or(usize::MAX),
        );
    }
    let stdout = String::from_utf8_lossy(&stdout.bytes).into_owned();
    if status.success() {
        stdin.with_context(|| format!("failed to pipe input to {program}"))?;
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&stderr.bytes);
    bail!(
        "{program} {args:?} failed (exit: {code:?}): {stderr}",
        code = status.code()
    )
}

fn run_command_with_input_limits(
    program: &str,
    args: &[&str],
    input: &[u8],
    timeout: Option<Duration>,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> std::result::Result<String, InputCommandError> {
    let mut command = command_with_timeout_group(program, timeout);
    let mut child = command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            InputCommandError::new(
                InputWriteStage::BeforeSpawn,
                anyhow::Error::new(error).context(format!("failed to spawn {program}")),
            )
        })?;

    let io_cancelled = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().map(|stdout| {
        read_pipe_in_background_cancellable(
            stdout,
            Some(max_stdout_bytes),
            Retention::Prefix,
            io_cancelled.clone(),
        )
    });
    let stderr = child.stderr.take().map(|stderr| {
        read_pipe_in_background_cancellable(
            stderr,
            Some(max_stderr_bytes),
            Retention::Prefix,
            io_cancelled.clone(),
        )
    });
    let stdin = child
        .stdin
        .take()
        .map(|stdin| write_pipe_in_background(stdin, input.to_vec(), io_cancelled.clone()));

    let status = wait_for_child(program, &mut child, timeout);
    io_cancelled.store(true, Ordering::Release);
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }

    let stdin = collect_pipe_input(stdin);
    let mut stdout = collect_pipe_output(stdout);
    let mut stderr = collect_pipe_output(stderr);
    let stage = classify_input_write_stage(&stdin);

    let status = status.map_err(|error| InputCommandError::new(stage, error))?;
    stdin.map_err(|error| {
        InputCommandError::new(
            stage,
            anyhow::Error::new(error).context(format!("failed to pipe input to {program}")),
        )
    })?;
    if let Some(error) = stdout.error.take() {
        return Err(InputCommandError::new(
            stage,
            anyhow::Error::new(error).context(format!("failed to read {program} stdout")),
        ));
    }
    if let Some(error) = stderr.error.take() {
        return Err(InputCommandError::new(
            stage,
            anyhow::Error::new(error).context(format!("failed to read {program} stderr")),
        ));
    }
    if stdout.exceeded {
        return Err(InputCommandError::new(
            stage,
            anyhow::anyhow!(
                "{program} stdout exceeded byte limit: {actual} bytes > {limit} bytes",
                actual = stdout.total_bytes,
                limit = max_stdout_bytes,
            ),
        ));
    }
    if stderr.exceeded {
        return Err(InputCommandError::new(
            stage,
            anyhow::anyhow!(
                "{program} stderr exceeded byte limit: {actual} bytes > {limit} bytes",
                actual = stderr.total_bytes,
                limit = max_stderr_bytes,
            ),
        ));
    }
    if !status.success() {
        return Err(InputCommandError::new(
            stage,
            anyhow::anyhow!(
                "{program} {args:?} failed (exit: {code:?}; stderr: {stderr_bytes} bytes)",
                code = status.code(),
                stderr_bytes = stderr.total_bytes,
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
}

fn classify_input_write_stage(stdin: &std::result::Result<(), PipeInputError>) -> InputWriteStage {
    match stdin {
        Ok(()) => InputWriteStage::AfterFullWrite,
        Err(error) if error.written == 0 => InputWriteStage::AfterSpawnBeforeWrite,
        Err(_) => InputWriteStage::AfterPartialWrite,
    }
}

fn write_pipe_in_background(
    mut pipe: ChildStdin,
    input: Vec<u8>,
    cancelled: Arc<AtomicBool>,
) -> thread::JoinHandle<std::result::Result<(), PipeInputError>> {
    thread::spawn(move || {
        let descriptor = pipe.as_raw_fd();
        let mut written = 0;
        // SAFETY: `descriptor` belongs to `pipe` for the duration of this
        // thread. Nonblocking writes let timeout cancellation interrupt a full
        // stdin pipe even if a descendant inherited the read end.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(PipeInputError::new(
                written,
                std::io::Error::last_os_error(),
            ));
        }
        while written < input.len() {
            if cancelled.load(Ordering::Acquire) {
                return Err(PipeInputError::new(
                    written,
                    std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "input write cancelled after child exit",
                    ),
                ));
            }
            match pipe.write(&input[written..]) {
                Ok(0) => {
                    return Err(PipeInputError::new(
                        written,
                        std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "failed to write complete input",
                        ),
                    ));
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(PipeInputError::new(written, error)),
            }
        }
        Ok(())
    })
}

#[derive(Debug)]
struct PipeInputError {
    written: usize,
    source: std::io::Error,
}

impl PipeInputError {
    fn new(written: usize, source: std::io::Error) -> Self {
        Self { written, source }
    }
}

impl std::fmt::Display for PipeInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for PipeInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn collect_pipe_input(
    handle: Option<thread::JoinHandle<std::result::Result<(), PipeInputError>>>,
) -> std::result::Result<(), PipeInputError> {
    handle
        .map(|handle| {
            handle.join().unwrap_or_else(|_| {
                Err(PipeInputError::new(
                    0,
                    std::io::Error::other("input writer thread panicked"),
                ))
            })
        })
        .unwrap_or(Ok(()))
}

pub fn run_command_bounded(
    program: &str,
    args: &[&str],
    timeout: Option<Duration>,
    max_stdout_bytes: usize,
) -> Result<BoundedOutput> {
    run_command_bounded_with_retention(program, args, timeout, max_stdout_bytes, Retention::Prefix)
}

pub fn run_command_tail_bounded(
    program: &str,
    args: &[&str],
    timeout: Option<Duration>,
    max_stdout_bytes: usize,
) -> Result<BoundedOutput> {
    run_command_bounded_with_retention(program, args, timeout, max_stdout_bytes, Retention::Tail)
}

fn run_command_bounded_with_retention(
    program: &str,
    args: &[&str],
    timeout: Option<Duration>,
    max_stdout_bytes: usize,
    retention: Retention,
) -> Result<BoundedOutput> {
    let mut command = command_with_timeout_group(program, timeout);
    let mut child = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

    let stdout = child
        .stdout
        .take()
        .map(|stdout| read_pipe_in_background(stdout, Some(max_stdout_bytes), retention));
    let stderr = child
        .stderr
        .take()
        .map(|stderr| read_pipe_in_background(stderr, None, Retention::Prefix));

    let status = wait_for_child(program, &mut child, timeout)?;
    let stdout = collect_pipe_output(stdout);
    if !status.success() {
        let stderr = collect_pipe_output(stderr);
        let stderr = String::from_utf8_lossy(&stderr.bytes);
        bail!(
            "{program} {args:?} failed (exit: {code:?}): {stderr}",
            code = status.code()
        );
    }
    let text = retained_utf8(stdout.bytes, retention, max_stdout_bytes);
    Ok(BoundedOutput {
        text,
        total_bytes: stdout.total_bytes,
        truncated: stdout.exceeded,
    })
}

fn wait_for_child(
    program: &str,
    child: &mut std::process::Child,
    timeout: Option<Duration>,
) -> Result<std::process::ExitStatus> {
    match timeout {
        None => child
            .wait()
            .with_context(|| format!("failed to wait {program}")),
        Some(limit) => match crate::proc::await_exit_then_kill_group(child, limit)
            .with_context(|| format!("failed to wait {program}"))?
        {
            Some(status) => Ok(status),
            None => bail!("{program} timed out after {limit:?}"),
        },
    }
}

fn command_with_timeout_group(program: &str, timeout: Option<Duration>) -> Command {
    let mut command = Command::new(program);
    if timeout.is_some() {
        command.process_group(0);
    }
    command
}

fn bound_string(mut text: String, max_bytes: usize) -> BoundedOutput {
    let total_bytes = text.len();
    if total_bytes <= max_bytes {
        return BoundedOutput {
            text,
            total_bytes,
            truncated: false,
        };
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    BoundedOutput {
        text,
        total_bytes,
        truncated: true,
    }
}

fn bound_string_tail(text: String, max_bytes: usize) -> BoundedOutput {
    let total_bytes = text.len();
    if total_bytes <= max_bytes {
        return BoundedOutput {
            text,
            total_bytes,
            truncated: false,
        };
    }
    let mut start = total_bytes.saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    BoundedOutput {
        text: text[start..].to_string(),
        total_bytes,
        truncated: true,
    }
}

fn retained_utf8(mut bytes: Vec<u8>, retention: Retention, max_bytes: usize) -> String {
    match retention {
        Retention::Prefix => {
            if let Err(error) = std::str::from_utf8(&bytes)
                && error.error_len().is_none()
            {
                bytes.truncate(error.valid_up_to());
            }
        }
        Retention::Tail => {
            let leading_continuations = bytes
                .iter()
                .take_while(|byte| **byte & 0b1100_0000 == 0b1000_0000)
                .count();
            bytes.drain(..leading_continuations);
        }
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    match retention {
        Retention::Prefix => bound_string(text, max_bytes).text,
        Retention::Tail => bound_string_tail(text, max_bytes).text,
    }
}

#[derive(Debug, Clone, Copy)]
enum Retention {
    Prefix,
    Tail,
}

#[derive(Debug, Default)]
struct CapturedPipe {
    bytes: Vec<u8>,
    total_bytes: usize,
    exceeded: bool,
    error: Option<std::io::Error>,
}

fn read_pipe_in_background<R>(
    mut pipe: R,
    limit: Option<usize>,
    retention: Retention,
) -> thread::JoinHandle<CapturedPipe>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = CapturedPipe::default();
        let mut tail = VecDeque::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let read = match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            retain_pipe_chunk(&mut output, &mut tail, &chunk[..read], limit, retention);
        }
        finish_pipe_capture(output, tail, limit, retention)
    })
}

fn retain_pipe_chunk(
    output: &mut CapturedPipe,
    tail: &mut VecDeque<u8>,
    chunk: &[u8],
    limit: Option<usize>,
    retention: Retention,
) {
    output.total_bytes = output.total_bytes.saturating_add(chunk.len());
    match (limit, retention) {
        (Some(limit), Retention::Prefix) => {
            let keep = limit.saturating_sub(output.bytes.len()).min(chunk.len());
            output.bytes.extend_from_slice(&chunk[..keep]);
        }
        (Some(0), Retention::Tail) => tail.clear(),
        (Some(limit), Retention::Tail) if chunk.len() >= limit => {
            tail.clear();
            tail.extend(chunk[chunk.len() - limit..].iter().copied());
        }
        (Some(limit), Retention::Tail) => {
            let overflow = tail.len().saturating_add(chunk.len()).saturating_sub(limit);
            for _ in 0..overflow {
                tail.pop_front();
            }
            tail.extend(chunk.iter().copied());
        }
        (None, _) => output.bytes.extend_from_slice(chunk),
    }
    output.exceeded |= limit.is_some_and(|limit| output.total_bytes > limit);
}

fn finish_pipe_capture(
    mut output: CapturedPipe,
    tail: VecDeque<u8>,
    limit: Option<usize>,
    retention: Retention,
) -> CapturedPipe {
    if limit.is_some() && matches!(retention, Retention::Tail) {
        output.bytes = tail.into_iter().collect();
    }
    output
}

fn read_pipe_in_background_cancellable<R>(
    mut pipe: R,
    limit: Option<usize>,
    retention: Retention,
    cancelled: Arc<AtomicBool>,
) -> thread::JoinHandle<CapturedPipe>
where
    R: Read + AsRawFd + Send + 'static,
{
    thread::spawn(move || {
        let descriptor = pipe.as_raw_fd();
        // SAFETY: `descriptor` belongs to `pipe` for this thread. Nonblocking
        // reads allow cancellation once the direct child exits even if a
        // descendant inherited a pipe writer.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return CapturedPipe {
                error: Some(std::io::Error::last_os_error()),
                ..CapturedPipe::default()
            };
        }

        let mut output = CapturedPipe::default();
        let mut tail = VecDeque::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let read = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => {
                    output.error = Some(error);
                    break;
                }
            };
            retain_pipe_chunk(&mut output, &mut tail, &chunk[..read], limit, retention);
        }
        finish_pipe_capture(output, tail, limit, retention)
    })
}

fn collect_pipe_output(handle: Option<thread::JoinHandle<CapturedPipe>>) -> CapturedPipe {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct SystemTmuxRunner {
    timeout: Option<Duration>,
    socket_name: Option<String>,
    max_output_bytes: Option<usize>,
}

impl SystemTmuxRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
            socket_name: None,
            max_output_bytes: None,
        }
    }

    pub fn with_socket_name(socket_name: impl Into<String>, timeout: Option<Duration>) -> Self {
        Self {
            timeout,
            socket_name: Some(socket_name.into()),
            max_output_bytes: None,
        }
    }

    pub fn from_env(timeout: Duration) -> Self {
        match std::env::var("VDE_TMUX_SOCKET_NAME") {
            Ok(socket_name) if !socket_name.trim().is_empty() => {
                Self::with_socket_name(socket_name, Some(timeout))
            }
            _ => Self::with_timeout(timeout),
        }
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = Some(max_output_bytes);
        self
    }
}

impl TmuxRunner for SystemTmuxRunner {
    fn run(&self, args: &[&str]) -> Result<String> {
        let owned_args = tmux_args(self.socket_name.as_deref(), args);
        let refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        run_command_with_output_limit("tmux", &refs, self.timeout, self.max_output_bytes)
    }

    fn run_with_input(
        &self,
        args: &[&str],
        input: &[u8],
    ) -> std::result::Result<String, InputCommandError> {
        let owned_args = tmux_args(self.socket_name.as_deref(), args);
        let refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        run_command_with_input_and_output_limits(
            "tmux",
            &refs,
            input,
            self.timeout,
            self.max_output_bytes
                .map_or(INPUT_COMMAND_MAX_STDOUT_BYTES, |limit| {
                    limit.min(INPUT_COMMAND_MAX_STDOUT_BYTES)
                }),
            INPUT_COMMAND_MAX_STDERR_BYTES,
        )
    }

    fn run_bounded(&self, args: &[&str], max_stdout_bytes: usize) -> Result<BoundedOutput> {
        let owned_args = tmux_args(self.socket_name.as_deref(), args);
        let refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        run_command_bounded("tmux", &refs, self.timeout, max_stdout_bytes)
    }

    fn run_tail_bounded(&self, args: &[&str], max_stdout_bytes: usize) -> Result<BoundedOutput> {
        let owned_args = tmux_args(self.socket_name.as_deref(), args);
        let refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        run_command_tail_bounded("tmux", &refs, self.timeout, max_stdout_bytes)
    }
}

pub fn tmux_args(socket_name: Option<&str>, args: &[&str]) -> Vec<String> {
    let mut tmux_args = Vec::new();
    if let Some(socket_name) = socket_name.filter(|name| !name.trim().is_empty()) {
        tmux_args.push("-L".to_string());
        tmux_args.push(socket_name.to_string());
    }
    tmux_args.extend(args.iter().map(|arg| (*arg).to_string()));
    tmux_args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn run_command_captures_stdout() {
        let out = run_command("/bin/sh", &["-c", "printf hello"], None).unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn run_command_drains_large_stdout_while_waiting() {
        let out = run_command(
            "/bin/sh",
            &[
                "-c",
                "i=0; while [ $i -lt 2048 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=$((i + 1)); done",
            ],
            Some(Duration::from_secs(2)),
        )
        .unwrap();

        assert_eq!(out.len(), 2048 * 64);
    }

    #[test]
    fn bounded_capture_drains_but_does_not_retain_oversized_stdout() {
        let started = std::time::Instant::now();
        let error = run_command_with_output_limit(
            "/bin/sh",
            &[
                "-c",
                "i=0; while [ $i -lt 4096 ]; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; i=$((i + 1)); done",
            ],
            Some(Duration::from_secs(2)),
            Some(1024),
        )
        .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("stdout exceeded byte limit"));
        assert!(error.to_string().contains("262144 bytes > 1024 bytes"));
    }

    #[test]
    fn piped_input_and_output_are_drained_concurrently_with_existing_bound() {
        let input = vec![b'x'; 256 * 1024];
        let started = Instant::now();
        let error = run_command_with_input_and_output_limits(
            "/bin/sh",
            &["-c", "cat"],
            &input,
            Some(Duration::from_secs(2)),
            1024,
            1024,
        )
        .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("stdout exceeded byte limit"));
        assert!(error.to_string().contains("262144 bytes > 1024 bytes"));
    }

    #[test]
    fn piped_input_timeout_does_not_wait_for_a_full_stdin_pipe_or_expose_input() {
        let secret = b"private-prompt-value".repeat(16 * 1024);
        let started = Instant::now();
        let error = run_command_with_input_and_output_limits(
            "/bin/sh",
            &["-c", "sleep 5"],
            &secret,
            Some(Duration::from_millis(100)),
            1024,
            1024,
        )
        .unwrap_err();
        let message = error.to_string();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(message.contains("timed out"));
        assert!(!message.contains("private-prompt-value"));
    }

    #[test]
    fn piped_input_is_not_in_nonzero_exit_error() {
        let error = run_command_with_input_and_output_limits(
            "/bin/sh",
            &["-c", "cat >&2; exit 3"],
            b"private-prompt-value",
            Some(Duration::from_secs(2)),
            1024,
            1024,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("stderr: 20 bytes"));
        assert!(!error.contains("private-prompt-value"));
    }

    #[test]
    fn piped_input_stderr_is_drained_with_a_fixed_bound() {
        let error = run_command_with_input_and_output_limits(
            "/bin/sh",
            &[
                "-c",
                "cat >/dev/null; i=0; while [ $i -lt 4096 ]; do printf x >&2; i=$((i + 1)); done; exit 3",
            ],
            b"short input",
            Some(Duration::from_secs(2)),
            1024,
            1024,
        )
        .unwrap_err();

        assert_eq!(error.stage, InputWriteStage::AfterFullWrite);
        assert!(error.to_string().contains("stderr exceeded byte limit"));
        assert!(error.to_string().contains("4096 bytes > 1024 bytes"));
    }

    #[test]
    fn input_error_distinguishes_partial_from_full_write() {
        let partial = run_command_with_input_and_output_limits(
            "/bin/sh",
            &["-c", "dd bs=1 count=1 of=/dev/null 2>/dev/null; exit 4"],
            &vec![b'x'; 1024 * 1024],
            Some(Duration::from_secs(2)),
            1024,
            1024,
        )
        .unwrap_err();
        let after = run_command_with_input_and_output_limits(
            "/bin/sh",
            &["-c", "cat >/dev/null; exit 4"],
            b"short input",
            Some(Duration::from_secs(2)),
            1024,
            1024,
        )
        .unwrap_err();

        assert_eq!(partial.stage, InputWriteStage::AfterPartialWrite);
        assert_eq!(after.stage, InputWriteStage::AfterFullWrite);
    }

    #[test]
    fn input_error_distinguishes_pre_spawn_from_zero_byte_post_spawn_failure() {
        let before_spawn = run_command_with_input_and_output_limits(
            "/definitely/not/a/vde-tmux-test-program",
            &[],
            b"input",
            Some(Duration::from_secs(2)),
            1024,
            1024,
        )
        .unwrap_err();
        let zero_write = Err(PipeInputError::new(
            0,
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed before first byte"),
        ));

        assert_eq!(before_spawn.stage, InputWriteStage::BeforeSpawn);
        assert_eq!(
            classify_input_write_stage(&zero_write),
            InputWriteStage::AfterSpawnBeforeWrite
        );
    }

    #[test]
    fn bounded_command_returns_a_truncated_prefix_and_total_size() {
        let output = run_command_bounded(
            "/bin/sh",
            &[
                "-c",
                "i=0; while [ $i -lt 4096 ]; do printf x; i=$((i + 1)); done",
            ],
            Some(Duration::from_secs(2)),
            1024,
        )
        .unwrap();

        assert_eq!(output.text.len(), 1024);
        assert_eq!(output.total_bytes, 4096);
        assert!(output.truncated);
    }

    #[test]
    fn tail_bounded_command_returns_latest_bytes_and_total_size() {
        let output = run_command_tail_bounded(
            "/bin/sh",
            &["-c", "printf 'old-newest'"],
            Some(Duration::from_secs(2)),
            6,
        )
        .unwrap();

        assert_eq!(output.text, "newest");
        assert_eq!(output.total_bytes, 10);
        assert!(output.truncated);
    }

    #[test]
    fn production_bounded_commands_do_not_return_partial_utf8_codepoints() {
        let prefix = run_command_bounded(
            "/bin/sh",
            &["-c", "printf 'あいう'"],
            Some(Duration::from_secs(2)),
            4,
        )
        .unwrap();
        let tail = run_command_tail_bounded(
            "/bin/sh",
            &["-c", "printf 'あいう'"],
            Some(Duration::from_secs(2)),
            4,
        )
        .unwrap();

        assert_eq!(prefix.text, "あ");
        assert_eq!(tail.text, "う");
        assert!(prefix.text.len() <= 4);
        assert!(tail.text.len() <= 4);
    }

    #[test]
    fn default_bounded_runner_truncates_at_a_utf8_boundary() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(&["display-message"], "あいう");

        let output = runner.run_bounded(&["display-message"], 4).unwrap();

        assert_eq!(output.text, "あ");
        assert_eq!(output.total_bytes, 9);
        assert!(output.truncated);
    }

    #[test]
    fn default_tail_bounded_runner_keeps_the_latest_utf8_suffix() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub(&["display-message"], "あいう");

        let output = runner.run_tail_bounded(&["display-message"], 4).unwrap();

        assert_eq!(output.text, "う");
        assert_eq!(output.total_bytes, 9);
        assert!(output.truncated);
    }

    #[test]
    fn run_command_nonzero_exit_returns_stderr_error() {
        let err = run_command("/bin/sh", &["-c", "echo boom >&2; exit 3"], None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("boom"), "stderr を含むこと: {msg}");
        assert!(msg.contains("exit"), "終了コード情報を含むこと: {msg}");
    }

    #[test]
    fn run_command_timeout_kills_descendants_and_unblocks_pipe_readers() {
        let started = std::time::Instant::now();
        let err = run_command(
            "/bin/sh",
            &["-c", "sleep 5 & wait"],
            Some(Duration::from_millis(100)),
        )
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "kill されずに待ち続けていないこと"
        );
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[test]
    fn tmux_args_prefixes_socket_name_when_present() {
        assert_eq!(
            tmux_args(Some("scratch"), &["list-sessions"]),
            vec![
                "-L".to_string(),
                "scratch".to_string(),
                "list-sessions".to_string()
            ]
        );
    }

    #[test]
    fn tmux_args_without_socket_name_is_plain() {
        assert_eq!(
            tmux_args(None, &["list-sessions"]),
            vec!["list-sessions".to_string()]
        );
    }
}

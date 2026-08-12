use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use crate::daemon::protocol::v2::ControlHealth;

const QUEUE_CAPACITY: usize = 32;
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 128 * 1024;
const SYNC_TIMEOUT: Duration = Duration::from_millis(500);
const CLIENT_WAIT_TIMEOUT: Duration = Duration::from_millis(650);
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlError {
    Starting,
    Degraded,
    QueueFull,
    InvalidCommand,
    Unavailable(String),
    CommandFailed(String),
    Deadline,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => formatter.write_str("tmux control client is starting"),
            Self::Degraded => formatter.write_str("tmux control client is reconnecting"),
            Self::QueueFull => formatter.write_str("tmux control command queue is full"),
            Self::InvalidCommand => formatter.write_str("tmux control command must be one line"),
            Self::Unavailable(message) => {
                write!(formatter, "tmux control client unavailable: {message}")
            }
            Self::CommandFailed(message) => {
                write!(formatter, "tmux control command failed: {message}")
            }
            Self::Deadline => formatter.write_str("tmux control command deadline exceeded"),
        }
    }
}

impl std::error::Error for ControlError {}

struct ControlJob {
    command: String,
    deadline: Instant,
    response: mpsc::Sender<Result<String, ControlError>>,
}

enum WorkerEvent {
    Job(ControlJob),
    Shutdown(mpsc::Sender<()>),
    Line {
        generation: u64,
        line: Result<String, String>,
    },
}

#[derive(Clone)]
pub(crate) struct TmuxControlHandle {
    events: SyncSender<WorkerEvent>,
    health: Arc<AtomicU8>,
    stopping: Arc<AtomicBool>,
}

impl TmuxControlHandle {
    pub(crate) fn unavailable() -> Self {
        let (events, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        Self {
            events,
            health: Arc::new(AtomicU8::new(health_code(ControlHealth::Degraded))),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn start(socket_path: PathBuf) -> Self {
        let (events, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let health = Arc::new(AtomicU8::new(health_code(ControlHealth::Starting)));
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_events = events.clone();
        let worker_health = health.clone();
        let worker_stopping = stopping.clone();
        thread::spawn(move || {
            supervisor(
                socket_path,
                worker_events,
                receiver,
                worker_health,
                worker_stopping,
            )
        });
        Self {
            events,
            health,
            stopping,
        }
    }

    pub(crate) fn health(&self) -> ControlHealth {
        decode_health(self.health.load(Ordering::SeqCst))
    }

    pub(crate) fn execute_until(
        &self,
        command: String,
        deadline: Instant,
    ) -> Result<String, ControlError> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(ControlError::Unavailable("worker is stopping".to_string()));
        }
        if command.is_empty() || command.contains(['\n', '\r']) {
            return Err(ControlError::InvalidCommand);
        }
        match self.health() {
            ControlHealth::Starting => return Err(ControlError::Starting),
            ControlHealth::Degraded => return Err(ControlError::Degraded),
            ControlHealth::Ready => {}
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(ControlError::Deadline);
        };
        let (response, receiver) = mpsc::channel();
        match self.events.try_send(WorkerEvent::Job(ControlJob {
            command,
            deadline,
            response,
        })) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(ControlError::QueueFull),
            Err(TrySendError::Disconnected(_)) => {
                return Err(ControlError::Unavailable("worker stopped".to_string()));
            }
        }
        receiver
            .recv_timeout(remaining)
            .unwrap_or(Err(ControlError::Deadline))
    }

    pub(crate) fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        let (acknowledge, acknowledged) = mpsc::channel();
        let mut event = WorkerEvent::Shutdown(acknowledge);
        let deadline = Instant::now() + CLIENT_WAIT_TIMEOUT;
        loop {
            match self.events.try_send(event) {
                Ok(()) => {
                    let _ = acknowledged.recv_timeout(CLIENT_WAIT_TIMEOUT);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(returned)) => {
                    event = returned;
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

fn health_code(health: ControlHealth) -> u8 {
    match health {
        ControlHealth::Starting => 0,
        ControlHealth::Ready => 1,
        ControlHealth::Degraded => 2,
    }
}

fn decode_health(code: u8) -> ControlHealth {
    match code {
        1 => ControlHealth::Ready,
        2 => ControlHealth::Degraded,
        _ => ControlHealth::Starting,
    }
}

struct Connection {
    child: Child,
    stdin: ChildStdin,
    generation: u64,
}

fn supervisor(
    socket_path: PathBuf,
    events: SyncSender<WorkerEvent>,
    receiver: Receiver<WorkerEvent>,
    health: Arc<AtomicU8>,
    stopping: Arc<AtomicBool>,
) {
    let mut generation = 0_u64;
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if stopping.load(Ordering::SeqCst) {
            health.store(health_code(ControlHealth::Degraded), Ordering::SeqCst);
            return;
        }
        generation = generation.wrapping_add(1);
        let connection = spawn_connection(&socket_path, generation, events.clone());
        let mut connection = match connection {
            Ok(connection) => connection,
            Err(_) => {
                health.store(health_code(ControlHealth::Degraded), Ordering::SeqCst);
                fail_queued(&receiver, ControlError::Degraded);
                if wait_to_retry(&receiver, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        health.store(health_code(ControlHealth::Starting), Ordering::SeqCst);
        if let Err(disposition) = sync_connection(&receiver, &mut connection) {
            health.store(health_code(ControlHealth::Degraded), Ordering::SeqCst);
            if disposition == WorkerDisposition::Shutdown {
                return;
            }
            poison(&mut connection);
            fail_queued(&receiver, ControlError::Degraded);
            if wait_to_retry(&receiver, backoff) {
                return;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
            continue;
        }
        health.store(health_code(ControlHealth::Ready), Ordering::SeqCst);
        backoff = INITIAL_BACKOFF;
        if run_ready(&receiver, &mut connection) == WorkerDisposition::Shutdown {
            health.store(health_code(ControlHealth::Degraded), Ordering::SeqCst);
            return;
        }
        poison(&mut connection);
        health.store(health_code(ControlHealth::Degraded), Ordering::SeqCst);
        fail_queued(&receiver, ControlError::Degraded);
        if wait_to_retry(&receiver, backoff) {
            return;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerDisposition {
    Reconnect,
    Shutdown,
}

fn wait_to_retry(receiver: &Receiver<WorkerEvent>, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        match receiver.recv_timeout(remaining) {
            Ok(WorkerEvent::Job(job)) => {
                let _ = job.response.send(Err(ControlError::Degraded));
            }
            Ok(WorkerEvent::Shutdown(acknowledge)) => {
                let _ = acknowledge.send(());
                return true;
            }
            Ok(WorkerEvent::Line { .. }) => {}
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => return true,
        }
    }
}

fn spawn_connection(
    socket_path: &Path,
    generation: u64,
    events: SyncSender<WorkerEvent>,
) -> Result<Connection, ControlError> {
    let mut child = Command::new("tmux")
        .arg("-S")
        .arg(socket_path)
        .arg("-N")
        .arg("-C")
        .arg("attach-session")
        .arg("-f")
        .arg("ignore-size,no-output,no-detach-on-destroy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ControlError::Unavailable(error.to_string()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ControlError::Unavailable("control stdin missing".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ControlError::Unavailable("control stdout missing".to_string()))?;
    thread::spawn(move || read_lines(stdout, generation, events));
    Ok(Connection {
        child,
        stdin,
        generation,
    })
}

fn read_lines(stdout: impl std::io::Read, generation: u64, events: SyncSender<WorkerEvent>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_bounded_line(&mut reader) {
            Ok(None) => {
                let _ = events.send(WorkerEvent::Line {
                    generation,
                    line: Err("control stdout closed".to_string()),
                });
                return;
            }
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(line) => {
                    if events
                        .send(WorkerEvent::Line {
                            generation,
                            line: Ok(line),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = events.send(WorkerEvent::Line {
                        generation,
                        line: Err(format!("control output is not UTF-8: {error}")),
                    });
                    return;
                }
            },
            Err(error) => {
                let _ = events.send(WorkerEvent::Line {
                    generation,
                    line: Err(error),
                });
                return;
            }
        }
    }
}

fn read_bounded_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|error| error.to_string())?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err("control stdout closed mid-line".to_string())
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > MAX_LINE_BYTES {
            return Err("control line exceeds 64 KiB".to_string());
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

#[derive(Default)]
struct BlockParser {
    block: Option<String>,
    output: String,
}

enum ParsedBlock {
    Pending,
    Complete(String),
    Error(String),
}

impl BlockParser {
    fn push(&mut self, raw: &str) -> Result<ParsedBlock, String> {
        let line = raw.trim_end_matches(['\r', '\n']);
        if line == "%exit" || line.starts_with("%exit ") {
            return Err("control client exited".to_string());
        }
        if let Some(block) = &self.block {
            if line == format!("%end {block}") {
                self.block = None;
                return Ok(ParsedBlock::Complete(std::mem::take(&mut self.output)));
            }
            if line == format!("%error {block}") {
                self.block = None;
                return Ok(ParsedBlock::Error(std::mem::take(&mut self.output)));
            }
            if line.starts_with("%begin ") {
                return Err("nested control response block".to_string());
            }
            if self.output.len().saturating_add(line.len() + 1) > MAX_RESPONSE_BYTES {
                return Err("control response exceeds 128 KiB".to_string());
            }
            self.output.push_str(line);
            self.output.push('\n');
            return Ok(ParsedBlock::Pending);
        }
        if let Some(block) = line.strip_prefix("%begin ") {
            if block.split_ascii_whitespace().count() != 3 {
                return Err("invalid control response header".to_string());
            }
            self.block = Some(block.to_string());
        }
        Ok(ParsedBlock::Pending)
    }
}

fn sync_connection(
    receiver: &Receiver<WorkerEvent>,
    connection: &mut Connection,
) -> Result<(), WorkerDisposition> {
    let deadline = Instant::now() + SYNC_TIMEOUT;
    let mut parser = BlockParser::default();
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(WorkerDisposition::Reconnect);
        };
        match receiver.recv_timeout(remaining) {
            Ok(WorkerEvent::Line { generation, line }) if generation == connection.generation => {
                let line = line.map_err(|_| WorkerDisposition::Reconnect)?;
                match parser
                    .push(&line)
                    .map_err(|_| WorkerDisposition::Reconnect)?
                {
                    ParsedBlock::Complete(_) => return Ok(()),
                    ParsedBlock::Error(_) => return Err(WorkerDisposition::Reconnect),
                    ParsedBlock::Pending => {}
                }
            }
            Ok(WorkerEvent::Job(job)) => {
                let _ = job.response.send(Err(ControlError::Starting));
            }
            Ok(WorkerEvent::Shutdown(acknowledge)) => {
                poison(connection);
                let _ = acknowledge.send(());
                return Err(WorkerDisposition::Shutdown);
            }
            Ok(WorkerEvent::Line { .. }) => {}
            Err(_) => return Err(WorkerDisposition::Reconnect),
        }
    }
}

fn run_ready(receiver: &Receiver<WorkerEvent>, connection: &mut Connection) -> WorkerDisposition {
    let mut parser = BlockParser::default();
    let mut current: Option<(ControlJob, Instant)> = None;
    loop {
        if current.is_none() {
            match receiver.recv() {
                Ok(WorkerEvent::Job(job)) => {
                    if dispatch(connection, &job).is_err() {
                        let _ = job.response.send(Err(ControlError::Degraded));
                        return WorkerDisposition::Reconnect;
                    }
                    let deadline = job.deadline;
                    current = Some((job, deadline));
                }
                Ok(WorkerEvent::Shutdown(acknowledge)) => {
                    poison(connection);
                    let _ = acknowledge.send(());
                    return WorkerDisposition::Shutdown;
                }
                Ok(WorkerEvent::Line { generation, line })
                    if generation == connection.generation =>
                {
                    if line.is_err() {
                        return WorkerDisposition::Reconnect;
                    }
                }
                Ok(WorkerEvent::Line { .. }) => {}
                Err(_) => return WorkerDisposition::Shutdown,
            }
            continue;
        }
        let deadline = current.as_ref().expect("current job exists").1;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            fail_current(&mut current, ControlError::Deadline);
            return WorkerDisposition::Reconnect;
        };
        match receiver.recv_timeout(remaining) {
            Ok(WorkerEvent::Job(job)) => {
                let _ = job.response.send(Err(ControlError::QueueFull));
            }
            Ok(WorkerEvent::Shutdown(acknowledge)) => {
                fail_current(
                    &mut current,
                    ControlError::Unavailable("worker is stopping".to_string()),
                );
                poison(connection);
                let _ = acknowledge.send(());
                return WorkerDisposition::Shutdown;
            }
            Ok(WorkerEvent::Line { generation, line }) if generation == connection.generation => {
                let line = match line {
                    Ok(line) => line,
                    Err(message) => {
                        fail_current(&mut current, ControlError::Unavailable(message));
                        return WorkerDisposition::Reconnect;
                    }
                };
                match parser.push(&line) {
                    Ok(ParsedBlock::Pending) => {}
                    Ok(ParsedBlock::Complete(output)) => {
                        if let Some((job, _)) = current.take() {
                            let _ = job.response.send(Ok(output));
                        }
                    }
                    Ok(ParsedBlock::Error(output)) => {
                        if let Some((job, _)) = current.take() {
                            let _ = job.response.send(Err(ControlError::CommandFailed(output)));
                        }
                    }
                    Err(message) => {
                        fail_current(&mut current, ControlError::Unavailable(message));
                        return WorkerDisposition::Reconnect;
                    }
                }
            }
            Ok(WorkerEvent::Line { .. }) => {}
            Err(RecvTimeoutError::Timeout) => {
                fail_current(&mut current, ControlError::Deadline);
                return WorkerDisposition::Reconnect;
            }
            Err(RecvTimeoutError::Disconnected) => return WorkerDisposition::Shutdown,
        }
    }
}

fn dispatch(connection: &mut Connection, job: &ControlJob) -> std::io::Result<()> {
    connection.stdin.write_all(job.command.as_bytes())?;
    connection.stdin.write_all(b"\n")?;
    connection.stdin.flush()
}

fn fail_current(current: &mut Option<(ControlJob, Instant)>, error: ControlError) {
    if let Some((job, _)) = current.take() {
        let _ = job.response.send(Err(error.clone()));
    }
}

fn fail_queued(receiver: &Receiver<WorkerEvent>, error: ControlError) {
    while let Ok(event) = receiver.try_recv() {
        if let WorkerEvent::Job(job) = event {
            let _ = job.response.send(Err(error.clone()));
        }
    }
}

fn poison(connection: &mut Connection) {
    let _ = connection.child.kill();
    let _ = connection.child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_ignores_notifications_and_correlates_one_block() {
        let mut parser = BlockParser::default();
        assert!(matches!(
            parser.push("%session-changed $1 main\n"),
            Ok(ParsedBlock::Pending)
        ));
        assert!(matches!(
            parser.push("%begin 1 2 3\n"),
            Ok(ParsedBlock::Pending)
        ));
        assert!(matches!(parser.push("output\n"), Ok(ParsedBlock::Pending)));
        assert_eq!(
            match parser.push("%end 1 2 3\n").unwrap() {
                ParsedBlock::Complete(output) => output,
                _ => panic!("expected complete block"),
            },
            "output\n"
        );
    }

    #[test]
    fn parser_rejects_nested_blocks_and_exit() {
        let mut parser = BlockParser::default();
        parser.push("%begin 1 2 3\n").unwrap();
        assert!(parser.push("%begin 2 3 4\n").is_err());
        assert!(BlockParser::default().push("%exit\n").is_err());
    }

    #[test]
    fn bounded_line_reader_rejects_oversized_and_truncated_lines() {
        let mut oversized = BufReader::new(std::io::Cursor::new(vec![b'x'; MAX_LINE_BYTES + 1]));
        assert!(
            read_bounded_line(&mut oversized)
                .unwrap_err()
                .contains("64 KiB")
        );

        let mut truncated = BufReader::new(std::io::Cursor::new(b"partial"));
        assert!(
            read_bounded_line(&mut truncated)
                .unwrap_err()
                .contains("mid-line")
        );
    }
}

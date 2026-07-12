use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

#[cfg(test)]
pub mod mock;

pub trait TmuxRunner {
    fn run(&self, args: &[&str]) -> Result<String>;
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
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

    let mut stdout = child
        .stdout
        .take()
        .map(|stdout| read_pipe_in_background(stdout, max_stdout_bytes));
    let mut stderr = child
        .stderr
        .take()
        .map(|stderr| read_pipe_in_background(stderr, None));

    let status = match timeout {
        None => child
            .wait()
            .with_context(|| format!("failed to wait {program}"))?,
        Some(limit) => {
            let deadline = Instant::now() + limit;
            loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("{program} timed out after {limit:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };

    let stdout = collect_pipe_output(stdout.take());
    if stdout.exceeded {
        bail!(
            "{program} stdout exceeded byte limit: {actual} bytes > {limit} bytes",
            actual = stdout.total_bytes,
            limit = max_stdout_bytes.unwrap_or(usize::MAX),
        );
    }
    let stdout = String::from_utf8_lossy(&stdout.bytes).into_owned();
    if status.success() {
        return Ok(stdout);
    }
    let stderr = collect_pipe_output(stderr.take());
    let stderr = String::from_utf8_lossy(&stderr.bytes);
    bail!(
        "{program} {args:?} failed (exit: {code:?}): {stderr}",
        code = status.code()
    )
}

#[derive(Debug, Default)]
struct CapturedPipe {
    bytes: Vec<u8>,
    total_bytes: usize,
    exceeded: bool,
}

fn read_pipe_in_background<R>(mut pipe: R, limit: Option<usize>) -> thread::JoinHandle<CapturedPipe>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = CapturedPipe::default();
        let mut chunk = [0_u8; 8192];
        loop {
            let read = match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            output.total_bytes = output.total_bytes.saturating_add(read);
            let keep = limit
                .map(|limit| limit.saturating_sub(output.bytes.len()).min(read))
                .unwrap_or(read);
            output.bytes.extend_from_slice(&chunk[..keep]);
            output.exceeded |= limit.is_some_and(|limit| output.total_bytes > limit);
        }
        output
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
    use std::time::Duration;

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
    fn run_command_nonzero_exit_returns_stderr_error() {
        let err = run_command("/bin/sh", &["-c", "echo boom >&2; exit 3"], None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("boom"), "stderr を含むこと: {msg}");
        assert!(msg.contains("exit"), "終了コード情報を含むこと: {msg}");
    }

    #[test]
    fn run_command_times_out_and_kills() {
        let started = std::time::Instant::now();
        let err = run_command(
            "/bin/sh",
            &["-c", "sleep 5"],
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

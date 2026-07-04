//! tmux コマンド発行層。全機能はこの trait 経由で tmux を呼ぶ
//! (具象 Command 直呼びを禁止し、MockTmuxRunner で全経路をテスト可能にする)。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

#[cfg(test)]
pub mod mock;

pub trait TmuxRunner {
    /// `tmux <args>` を実行し、成功した場合は stdout をそのまま返す。
    fn run(&self, args: &[&str]) -> Result<String>;
}

/// 外部コマンドを実行し stdout を返す。timeout 指定時は超過で kill してエラーを返す。
///
/// 注意: 出力の読み取りはプロセス終了後に行うため、pipe バッファ(64KB 程度)を大きく
/// 超える出力を出すコマンドには使わない(tmux / git の想定出力は十分小さい)。
pub fn run_command(program: &str, args: &[&str], timeout: Option<Duration>) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

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

    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if status.success() {
        return Ok(stdout);
    }
    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    bail!(
        "{program} {args:?} failed (exit: {code:?}): {stderr}",
        code = status.code()
    )
}

/// 実 tmux を呼ぶ Runner。timeout は経路ごとに選ぶ:
/// hook 経路はエージェントをブロックしないため必ず Some を指定する。
#[derive(Debug, Clone, Default)]
pub struct SystemTmuxRunner {
    timeout: Option<Duration>,
    socket_name: Option<String>,
}

impl SystemTmuxRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
            socket_name: None,
        }
    }

    pub fn with_socket_name(socket_name: impl Into<String>, timeout: Option<Duration>) -> Self {
        Self {
            timeout,
            socket_name: Some(socket_name.into()),
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
}

impl TmuxRunner for SystemTmuxRunner {
    fn run(&self, args: &[&str]) -> Result<String> {
        let owned_args = tmux_args(self.socket_name.as_deref(), args);
        let refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        run_command("tmux", &refs, self.timeout)
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

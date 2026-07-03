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
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTmuxRunner {
    timeout: Option<Duration>,
}

impl SystemTmuxRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
        }
    }
}

impl TmuxRunner for SystemTmuxRunner {
    fn run(&self, args: &[&str]) -> Result<String> {
        run_command("tmux", args, self.timeout)
    }
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
}

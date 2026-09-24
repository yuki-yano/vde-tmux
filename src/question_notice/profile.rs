//! Process-bound, finite Codex rendering profiles. Never retain version command output.
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::pane_state::AgentProcessIdentity;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexProfile {
    V01551,
    V01561,
    #[default]
    Unknown,
}

impl CodexProfile {
    pub fn from_version_output(stdout: &[u8], stderr: &[u8], success: bool) -> Self {
        if !success || !stderr.is_empty() || stdout.len() > 4096 {
            return Self::Unknown;
        }
        match stdout {
            b"codex-cli 0.155.1\n" => Self::V01551,
            b"codex-cli 0.156.1\n" => Self::V01561,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableFingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub ctime_sec: i64,
    pub ctime_nsec: i64,
}

impl ExecutableFingerprint {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime_sec: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime_sec: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

/// The lookup request contains no executable path or argv. The daemon resolves it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRequest {
    pub process: AgentProcessIdentity,
    pub executable: ExecutableFingerprint,
}

impl ProfileRequest {
    pub fn capture(process: AgentProcessIdentity) -> Option<Self> {
        process.validate().ok()?;
        let path = executable_path(process.pid)?;
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return None;
        }
        let started = process_start_epoch_nanos(&process)?;
        let changed =
            i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
        if changed > started
            || crate::daemon::lifecycle::agent_process_start_token(process.pid).ok()?
                != process.start_token
        {
            return None;
        }
        Some(Self {
            process,
            executable: ExecutableFingerprint::from_metadata(&metadata),
        })
    }

    pub fn matches_current_process(&self) -> bool {
        Self::capture(self.process.clone()).as_ref() == Some(self)
    }

    pub fn matches_embedded_process(&self) -> bool {
        self.matches_current_process()
            && process_arguments(self.process.pid)
                .is_some_and(|args| argument_mode(&args) == ArgumentMode::Embedded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgumentMode {
    Embedded,
    NonEmbedded,
    Unknown,
}

fn argument_mode(args: &[String]) -> ArgumentMode {
    let mut values = args.iter().skip(1).peekable();
    let mut positional_known = true;
    while let Some(arg) = values.next() {
        if arg == "--remote"
            || arg.starts_with("--remote=")
            || arg == "--connect"
            || arg.starts_with("--connect=")
        {
            return ArgumentMode::NonEmbedded;
        }
        // Clap ends value collection for attached values; the following token
        // can still be a subcommand. Bare image options consume multiple values.
        if arg.starts_with("--image=")
            || (arg.starts_with("-i") && arg.len() > 2 && !arg.starts_with("--"))
        {
            continue;
        }
        if arg == "-i" || arg == "--image" {
            let mut count = 0;
            while values.peek().is_some_and(|value| !value.starts_with('-')) {
                values.next();
                count += 1;
            }
            if count == 0 {
                return ArgumentMode::Unknown;
            }
            continue;
        }
        if matches!(
            arg.as_str(),
            "-c" | "--config"
                | "-m"
                | "--model"
                | "-p"
                | "--profile"
                | "-C"
                | "--cd"
                | "--add-dir"
                | "-i"
                | "--image"
                | "-s"
                | "--sandbox"
                | "-a"
                | "--ask-for-approval"
                | "--enable"
                | "--disable"
                | "--local-provider"
                | "--remote-auth-token-env"
        ) {
            if values.next().is_none() {
                return ArgumentMode::Unknown;
            }
            continue;
        }
        if arg == "--" {
            return if positional_known {
                ArgumentMode::Embedded
            } else {
                ArgumentMode::Unknown
            };
        }
        if arg.starts_with('-') {
            // An unknown option might consume its next token. Never interpret that
            // value as positive evidence of an out-of-scope subcommand.
            if ![
                "--config=",
                "--model=",
                "--profile=",
                "--cd=",
                "--add-dir=",
                "--image=",
                "--sandbox=",
                "--ask-for-approval=",
                "--enable=",
                "--disable=",
                "--local-provider=",
                "--remote-auth-token-env=",
            ]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
                && !(arg.len() > 2
                    && ["-c", "-m", "-p", "-s", "-a", "-C"]
                        .iter()
                        .any(|prefix| arg.starts_with(prefix)))
                && !matches!(
                    arg.as_str(),
                    "--strict-config"
                        | "--yolo"
                        | "--approve-for-me"
                        | "--not-so-yolo"
                        | "--worktree"
                        | "--no-daemon"
                        | "--no-alt-screen"
                        | "--dangerously-bypass-hook-trust"
                        | "--dangerously-bypass-approvals-and-sandbox"
                        | "--full-auto"
                        | "--oss"
                        | "--search"
                        | "--help"
                        | "-h"
                        | "--version"
                        | "-V"
                )
            {
                positional_known = false;
            }
            continue;
        }
        if !positional_known {
            return ArgumentMode::Unknown;
        }
        return if matches!(
            arg.as_str(),
            "exec"
                | "e"
                | "review"
                | "app-server"
                | "mcp-server"
                | "remote-control"
                | "exec-server"
                | "agents"
                | "tcp-tunnel"
                | "login"
                | "logout"
                | "mcp"
                | "plugin"
                | "app"
                | "completion"
                | "update"
                | "doctor"
                | "sandbox"
                | "debug"
                | "execpolicy"
                | "apply"
                | "a"
                | "queue"
                | "archive"
                | "delete"
                | "migrate-rollouts"
                | "unarchive"
                | "cloud"
                | "cloud-tasks"
                | "responses-api-proxy"
                | "stdio-to-uds"
                | "features"
        ) {
            ArgumentMode::NonEmbedded
        } else {
            ArgumentMode::Embedded
        };
    }
    if !args.is_empty() && positional_known {
        ArgumentMode::Embedded
    } else {
        ArgumentMode::Unknown
    }
}

/// Positive mode evidence only; unavailable argv/identity is not an exclusion.
pub fn positively_non_embedded(process: &AgentProcessIdentity) -> bool {
    executable_path(process.pid)
        .is_some_and(|path| path.file_name().is_some_and(|name| name == "codex"))
        && crate::daemon::lifecycle::agent_process_start_token(process.pid)
            .ok()
            .as_deref()
            == Some(&process.start_token)
        && process_arguments(process.pid)
            .is_some_and(|args| argument_mode(&args) == ArgumentMode::NonEmbedded)
        && crate::daemon::lifecycle::agent_process_start_token(process.pid)
            .ok()
            .as_deref()
            == Some(&process.start_token)
}

/// Only a transient local inspection. Neither argv nor environment is returned to the daemon
/// protocol or recorded in the profile cache.
fn process_arguments(pid: u32) -> Option<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        let mut mib = [
            libc::CTL_KERN,
            libc::KERN_PROCARGS2,
            i32::try_from(pid).ok()?,
        ];
        let mut bytes = vec![0_u8; 64 * 1024];
        let mut length = bytes.len();
        // SAFETY: mib and buffer are valid for their supplied lengths; this sysctl is read-only.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        bytes.truncate(length);
        let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
        if !(1..=256).contains(&argc) {
            return None;
        }
        let mut pos = 4 + bytes.get(4..)?.iter().position(|b| *b == 0)?;
        while bytes.get(pos) == Some(&0) {
            pos += 1;
        }
        let mut args = Vec::new();
        for _ in 0..argc {
            let end = pos + bytes.get(pos..)?.iter().position(|b| *b == 0)?;
            args.push(std::str::from_utf8(bytes.get(pos..end)?).ok()?.to_owned());
            pos = end + 1;
        }
        Some(args)
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(format!("/proc/{pid}/cmdline"))
            .ok()?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 64 * 1024 || bytes.last() != Some(&0) {
            return None;
        }
        bytes[..bytes.len() - 1]
            .split(|b| *b == 0)
            .map(|arg| std::str::from_utf8(arg).ok().map(str::to_owned))
            .collect()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

pub fn executable_path(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut path = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is writable and its exact capacity is passed to proc_pidpath.
        let count = unsafe {
            libc::proc_pidpath(
                i32::try_from(pid).ok()?,
                path.as_mut_ptr().cast(),
                path.len() as u32,
            )
        };
        if count <= 0 {
            return None;
        }
        path.truncate(path.iter().position(|byte| *byte == 0)?);
        Some(PathBuf::from(std::ffi::OsString::from_vec(path)))
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

fn process_start_epoch_nanos(process: &AgentProcessIdentity) -> Option<i128> {
    #[cfg(target_os = "macos")]
    {
        let (seconds, micros) = process.start_token.split_once(':')?;
        Some(seconds.parse::<i128>().ok()? * 1_000_000_000 + micros.parse::<i128>().ok()? * 1000)
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let boot = stat.lines().find_map(|line| line.strip_prefix("btime "))?;
        // SAFETY: sysconf has no pointer arguments or side effects.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks <= 0 {
            return None;
        }
        Some(
            boot.parse::<i128>().ok()? * 1_000_000_000
                + process.start_token.parse::<i128>().ok()? * 1_000_000_000 / i128::from(ticks),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = process;
        None
    }
}

#[derive(Debug, Default)]
struct CacheState {
    unknown_modes: u64,
    ready: std::collections::BTreeMap<ExecutableFingerprint, CodexProfile>,
    pending: std::collections::BTreeSet<ExecutableFingerprint>,
}

/// Query workers wait here, independently of the serial mutation coordinator.
#[derive(Debug, Default)]
pub struct ProfileCache {
    state: std::sync::Mutex<CacheState>,
    changed: std::sync::Condvar,
}

impl ProfileCache {
    pub fn unknown_modes(&self) -> u64 {
        self.state
            .lock()
            .expect("profile cache lock poisoned")
            .unknown_modes
    }
    pub fn cached(&self, request: &ProfileRequest) -> Option<CodexProfile> {
        self.state
            .lock()
            .expect("profile cache lock poisoned")
            .ready
            .get(&request.executable)
            .copied()
    }
    pub fn lookup(
        self: &std::sync::Arc<Self>,
        request: &ProfileRequest,
        deadline: std::time::Instant,
    ) -> CodexProfile {
        self.lookup_with(request, deadline, |request, deadline| {
            version_from_process(request, deadline)
        })
    }

    fn lookup_with(
        self: &std::sync::Arc<Self>,
        request: &ProfileRequest,
        deadline: std::time::Instant,
        lookup: impl FnOnce(&ProfileRequest, std::time::Instant) -> Option<CodexProfile>
        + Send
        + 'static,
    ) -> CodexProfile {
        use std::time::{Duration, Instant};
        let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        if !request.matches_embedded_process() {
            if request.matches_current_process()
                && process_arguments(request.process.pid)
                    .is_none_or(|args| argument_mode(&args) == ArgumentMode::Unknown)
            {
                let mut state = self.state.lock().expect("profile cache lock poisoned");
                state.unknown_modes = state.unknown_modes.saturating_add(1);
            }
            return CodexProfile::Unknown;
        }
        let mut state = self.state.lock().expect("profile cache lock poisoned");
        if !state.ready.contains_key(&request.executable)
            && !state.pending.contains(&request.executable)
        {
            if state.pending.len() >= 4 {
                return CodexProfile::Unknown;
            }
            state.pending.insert(request.executable.clone());
            let cache = self.clone();
            let owned = request.clone();
            std::thread::spawn(move || {
                let result = lookup(&owned, deadline);
                let mut state = cache.state.lock().expect("profile cache lock poisoned");
                state.pending.remove(&owned.executable);
                if let Some(result) = result {
                    if state.ready.len() >= 64 {
                        state.ready.pop_first();
                    }
                    state.ready.insert(owned.executable, result);
                }
                cache.changed.notify_all();
            });
        }
        loop {
            if let Some(profile) = state.ready.get(&request.executable).copied() {
                drop(state);
                return if request.matches_embedded_process() {
                    profile
                } else {
                    CodexProfile::Unknown
                };
            }
            if !state.pending.contains(&request.executable) {
                return CodexProfile::Unknown;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return CodexProfile::Unknown;
            };
            state = self
                .changed
                .wait_timeout(state, remaining)
                .expect("profile cache lock poisoned")
                .0;
        }
    }
}

fn version_from_process(
    request: &ProfileRequest,
    deadline: std::time::Instant,
) -> Option<CodexProfile> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    if !request.matches_current_process() {
        return None;
    }
    let path = executable_path(request.process.pid)
        .filter(|path| path.file_name().is_some_and(|name| name == "codex"))?;
    if Instant::now() >= deadline {
        return None;
    }
    let Ok(mut child) = Command::new(path)
        .arg("--version")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    else {
        return None;
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || read_version_pipe(stdout));
    let err = std::thread::spawn(move || read_version_pipe(stderr));
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Err(_) => break false,
            Ok(None) if Instant::now() >= deadline => break false,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    // SAFETY: this subprocess owns the newly created group. Kill remaining pipe holders too.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    let output = out.join().ok().flatten();
    let errors = err.join().ok().flatten();
    if !request.matches_current_process() {
        return None;
    }
    if !success {
        return None;
    }
    let (stdout, stderr) = output.zip(errors)?;
    if !stderr.is_empty() {
        return None;
    }
    Some(CodexProfile::from_version_output(&stdout, &stderr, true))
}

fn read_version_pipe(mut reader: impl std::io::Read) -> Option<Vec<u8>> {
    let mut buffer = [0_u8; 1024];
    let mut output = Vec::new();
    let mut overflow = false;
    loop {
        let count = reader.read(&mut buffer).ok()?;
        if count == 0 {
            return (!overflow).then_some(output);
        }
        if output.len() + count > 4096 {
            overflow = true;
        }
        if !overflow {
            output.extend_from_slice(&buffer[..count]);
        }
    }
}

#[cfg(test)]
mod tests {
    fn embedded_arguments(args: &[String]) -> bool {
        super::argument_mode(args) == super::ArgumentMode::Embedded
    }

    use super::*;

    #[test]
    fn profile_is_an_exact_finite_contract() {
        assert_eq!(
            CodexProfile::from_version_output(b"codex-cli 0.155.1\n", b"", true),
            CodexProfile::V01551
        );
        assert_eq!(
            CodexProfile::from_version_output(b"codex-cli 0.156.1\n", b"", true),
            CodexProfile::V01561
        );
        for output in [
            b"codex-cli 0.157.0\n".as_slice(),
            b"shim codex-cli 0.156.1\n",
            b"codex-cli 0.156.1\nsecret",
        ] {
            assert_eq!(
                CodexProfile::from_version_output(output, b"", true),
                CodexProfile::Unknown
            );
        }
        assert_eq!(
            CodexProfile::from_version_output(b"codex-cli 0.156.1\n", b"warning", true),
            CodexProfile::Unknown
        );
        assert_eq!(
            CodexProfile::from_version_output(b"codex-cli 0.156.1\n", b"", false),
            CodexProfile::Unknown
        );
    }

    #[test]
    fn request_binds_exact_process_and_never_serializes_path() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let process = AgentProcessIdentity {
            pid,
            start_token: crate::daemon::lifecycle::agent_process_start_token(pid).unwrap(),
        };
        let request = ProfileRequest::capture(process).unwrap();
        assert!(request.matches_current_process());
        assert!(!serde_json::to_string(&request).unwrap().contains("path"));
        let mut stale = request;
        stale.executable.ino += 1;
        assert!(!stale.matches_current_process());
        stale.process.start_token.push('1');
        assert!(!stale.matches_current_process());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn native_lookup_timeout_overflow_unknown_replacement_and_mode_exclusion() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!(
            "vt-profile-{}",
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("fixture.c");
        let mode = root.join("mode");
        let binary = root.join("codex");
        std::fs::write(
            &source,
            format!(
                r#"
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {{
    if (argc == 2 && strcmp(argv[1], "--version") == 0) {{
        FILE *f = fopen({}, "r"); int mode = 0; if (f) {{ fscanf(f, "%d", &mode); fclose(f); }}
        if (mode == 1) sleep(3);
        if (mode == 2) {{ puts("codex-cli 9.0.0"); return 0; }}
        if (mode == 3) {{ for (int i=0; i<5000; i++) putchar('x'); return 0; }}
        puts("codex-cli 0.156.1"); return 0;
    }}
    sleep(30); return 0;
}}
"#,
                serde_json::to_string(&mode.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
        assert!(
            std::process::Command::new("cc")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .unwrap()
                .success()
        );
        // Linux btime is whole seconds and starttime is whole clock ticks. A
        // freshly linked fixture can be newer than that conservative start
        // bound even though it predates exec. Age only the test fixture; keep
        // the production ctime guard fail-closed.
        #[cfg(target_os = "linux")]
        std::thread::sleep(Duration::from_millis(1100));
        let mut child = std::process::Command::new(&binary).spawn().unwrap();
        let process = AgentProcessIdentity {
            pid: child.id(),
            start_token: crate::daemon::lifecycle::agent_process_start_token(child.id()).unwrap(),
        };
        let request = ProfileRequest::capture(process.clone()).unwrap();
        assert!(!positively_non_embedded(&process));
        assert_eq!(
            version_from_process(&request, Instant::now() + Duration::from_secs(1)),
            Some(CodexProfile::V01561)
        );
        for (mode_value, expected) in [(1, None), (2, Some(CodexProfile::Unknown)), (3, None)] {
            std::fs::write(&mode, mode_value.to_string()).unwrap();
            assert_eq!(
                version_from_process(&request, Instant::now() + Duration::from_millis(100)),
                expected
            );
        }
        let mut remote = std::process::Command::new(&binary)
            .arg("app-server")
            .spawn()
            .unwrap();
        let process = AgentProcessIdentity {
            pid: remote.id(),
            start_token: crate::daemon::lifecycle::agent_process_start_token(remote.id()).unwrap(),
        };
        assert!(positively_non_embedded(&process));
        let _ = remote.kill();
        let _ = remote.wait();
        let mut unknown = std::process::Command::new(&binary)
            .arg("--future-mode")
            .spawn()
            .unwrap();
        let request_unknown = ProfileRequest::capture(AgentProcessIdentity {
            pid: unknown.id(),
            start_token: crate::daemon::lifecycle::agent_process_start_token(unknown.id()).unwrap(),
        })
        .unwrap();
        let cache = std::sync::Arc::new(ProfileCache::default());
        assert_eq!(
            cache.lookup(&request_unknown, Instant::now() + Duration::from_secs(1)),
            CodexProfile::Unknown
        );
        assert_eq!(cache.unknown_modes(), 1);
        let _ = unknown.kill();
        let _ = unknown.wait();
        std::fs::copy("/bin/sleep", root.join("replacement")).unwrap();
        std::fs::rename(root.join("replacement"), &binary).unwrap();
        assert!(!request.matches_current_process());
        assert_eq!(
            version_from_process(&request, Instant::now() + Duration::from_secs(1)),
            None
        );
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn positive_mode_exclusion_does_not_mistake_option_values_for_subcommands() {
        for args in [
            vec!["codex", "--no-daemon"],
            vec!["codex", "--no-daemon", "synthetic prompt"],
            vec!["codex", "--no-daemon", "--", "app-server"],
        ] {
            assert_eq!(
                argument_mode(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
                ArgumentMode::Embedded
            );
        }
        for args in [
            vec!["codex", "--yolo"],
            vec!["codex", "--yolo", "prompt"],
            vec!["codex", "--worktree"],
            vec!["codex", "--approve-for-me"],
            vec!["codex", "--not-so-yolo"],
            vec!["codex", "-i", "a", "app-server", "prompt"],
            vec!["codex", "--image=a,b", "--strict-config"],
            vec!["codex", "--remote-auth-token-env", "SYNTHETIC_ENV"],
            vec!["codex", "-C/tmp", "-mtest"],
        ] {
            assert_eq!(
                argument_mode(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
                ArgumentMode::Embedded
            );
        }
        let unknown = ["codex", "--new-option", "app-server"].map(str::to_owned);
        assert_eq!(argument_mode(&unknown), ArgumentMode::Unknown);
        assert!(!embedded_arguments(&unknown));
        for args in [
            vec!["codex", "app-server"],
            vec!["codex", "--image=fixture.png", "app-server"],
            vec!["codex", "-ifixture.png", "exec"],
            vec!["codex", "--strict-config", "--remote=example"],
            vec!["codex", "--connect", "example"],
        ] {
            assert!(!embedded_arguments(
                &args.into_iter().map(str::to_owned).collect::<Vec<_>>()
            ));
        }
        for args in [
            vec!["codex", "-C", "app"],
            vec!["codex", "--model", "review"],
            vec!["codex", "--strict-config"],
        ] {
            assert!(embedded_arguments(
                &args.into_iter().map(str::to_owned).collect::<Vec<_>>()
            ));
        }
    }

    #[test]
    fn transient_lookup_failure_does_not_poison_new_session_using_same_binary() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let request = ProfileRequest::capture(AgentProcessIdentity {
            pid: child.id(),
            start_token: crate::daemon::lifecycle::agent_process_start_token(child.id()).unwrap(),
        })
        .unwrap();
        assert!(request.matches_current_process());
        assert_eq!(
            process_arguments(child.id()).as_deref().map(argument_mode),
            Some(ArgumentMode::Embedded),
            "sleep fixture argv: {:?}",
            process_arguments(child.id())
        );
        let cache = Arc::new(ProfileCache::default());
        assert_eq!(
            cache.lookup_with(&request, Instant::now() + Duration::from_secs(1), |_, _| {
                None
            }),
            CodexProfile::Unknown
        );
        assert_eq!(cache.cached(&request), None);
        assert_eq!(
            cache.lookup_with(&request, Instant::now() + Duration::from_secs(1), |_, _| {
                Some(CodexProfile::V01561)
            }),
            CodexProfile::V01561
        );
        let unknown = Arc::new(ProfileCache::default());
        assert_eq!(
            unknown.lookup_with(&request, Instant::now() + Duration::from_secs(1), |_, _| {
                Some(CodexProfile::Unknown)
            }),
            CodexProfile::Unknown
        );
        assert_eq!(unknown.cached(&request), Some(CodexProfile::Unknown));
        assert_eq!(
            unknown.lookup_with(
                &request,
                Instant::now() + Duration::from_secs(1),
                |_, _| panic!("definitive unknown was retried")
            ),
            CodexProfile::Unknown
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn cache_coalesces_exact_binary_and_rebinds_each_reply_to_process() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::time::{Duration, Instant};
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let request = ProfileRequest::capture(AgentProcessIdentity {
            pid: child.id(),
            start_token: crate::daemon::lifecycle::agent_process_start_token(child.id()).unwrap(),
        })
        .unwrap();
        assert!(request.matches_current_process());
        assert_eq!(
            process_arguments(child.id()).as_deref().map(argument_mode),
            Some(ArgumentMode::Embedded),
            "sleep fixture argv: {:?}",
            process_arguments(child.id())
        );
        let cache = Arc::new(ProfileCache::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let workers = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let request = request.clone();
                let calls = calls.clone();
                std::thread::spawn(move || {
                    cache.lookup_with(
                        &request,
                        Instant::now() + Duration::from_secs(2),
                        move |_, _| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(50));
                            Some(CodexProfile::V01561)
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), CodexProfile::V01561);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            cache.lookup(&request, Instant::now() + Duration::from_secs(1)),
            CodexProfile::Unknown
        );
        for mode in [
            "exec",
            "app-server",
            "mcp-server",
            "--remote=example",
            "--connect",
        ] {
            assert!(!embedded_arguments(&["codex".into(), mode.into()]));
        }
        assert!(embedded_arguments(&[
            "codex".into(),
            "--strict-config".into()
        ]));
    }
}

//! Bounded lifecycle resynchronization from the exact live rollout writer.
//! This never synthesizes Run completion or startup-hook trust.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::pane_state::{AgentProcessIdentity, LifecycleState, PaneState};
use crate::question_notice::{
    ingress::TranscriptLocator,
    turn_order::{Cursor, identifier_digest},
};
use anyhow::{Result, ensure};

#[derive(Default)]
pub(crate) struct Resynchronizer {
    readers: BTreeMap<u32, Cursor>,
    next: usize,
}
impl Resynchronizer {
    pub fn retain(&mut self, pids: &std::collections::BTreeSet<u32>) {
        self.readers.retain(|pid, _| pids.contains(pid));
    }
    pub fn sample(&mut self, state: &PaneState) -> bool {
        self.verify(state).unwrap_or(false)
    }
    pub fn poll(&mut self, states: &[&PaneState]) -> BTreeMap<u32, bool> {
        if states.is_empty() {
            return BTreeMap::new();
        }
        let states: Vec<_> = (0..states.len())
            .map(|offset| states[(self.next + offset) % states.len()])
            .collect();
        let pids: Vec<_> = states
            .iter()
            .filter_map(|state| state.agent_process.as_ref().map(|p| p.pid))
            .collect();
        let paths = rollout_batch(&pids).unwrap_or_default();
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut results = BTreeMap::new();
        for state in &states {
            let Some(process) = state.agent_process.as_ref() else {
                continue;
            };
            if Instant::now() >= deadline {
                break;
            }
            #[cfg(not(target_os = "macos"))]
            if !paths.contains_key(&process.pid) {
                break;
            }
            let verified = paths
                .get(&process.pid)
                .and_then(|paths| {
                    locator_from_paths(
                        process,
                        state.agent_session_id.as_ref()?.as_str(),
                        paths,
                        deadline,
                    )
                    .ok()
                })
                .is_some_and(|locator| self.read_idle(state, locator, deadline).unwrap_or(false));
            results.insert(process.pid, verified);
            self.next = (self.next + 1) % states.len();
        }
        results
    }
    fn verify(&mut self, state: &PaneState) -> Result<bool> {
        ensure!(
            state.agent.as_str() == "codex"
                && state.agent_present
                && state.scan_verified
                && matches!(state.lifecycle, LifecycleState::Idle)
                && state.run_seq == state.completed_seq,
            "not an existing idle Codex session"
        );
        let process = state
            .agent_process
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing process"))?;
        let session = state
            .agent_session_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;
        let locator = active_locator(process, session.as_str())?;
        self.read_idle(state, locator, Instant::now() + Duration::from_millis(250))
    }
    fn read_idle(
        &mut self,
        state: &PaneState,
        locator: TranscriptLocator,
        round_deadline: Instant,
    ) -> Result<bool> {
        let process = state
            .agent_process
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing process"))?;
        let session = state
            .agent_session_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing session"))?;
        let reader = self
            .readers
            .entry(process.pid)
            .or_insert_with(|| Cursor::new(locator.clone(), identifier_digest(session.as_str())));
        if reader.locator() != &locator {
            *reader = Cursor::new(locator, identifier_digest(session.as_str()));
        }
        let deadline = round_deadline.min(Instant::now() + Duration::from_millis(100));
        let mut remaining = crate::question_notice::turn_order::REQUEST_BYTES;
        loop {
            let slice = reader
                .read_slice(remaining, deadline)
                .map_err(|_| anyhow::anyhow!("rollout history unavailable"))?;
            remaining = remaining.saturating_sub(slice.bytes);
            if !slice.more {
                return Ok(!slice.partial && reader.idle());
            }
            if remaining == 0 || Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }
}

pub(crate) fn active_locator(
    process: &AgentProcessIdentity,
    session: &str,
) -> Result<TranscriptLocator> {
    let deadline = Instant::now() + Duration::from_millis(500);
    let before = crate::daemon::lifecycle::agent_process_start_token(process.pid)?;
    ensure!(before == process.start_token, "process replaced");
    let args = crate::question_notice::profile::process_arguments(process.pid)
        .ok_or_else(|| anyhow::anyhow!("process arguments unavailable"))?;
    ensure!(
        crate::question_notice::profile::independent_arguments(&args),
        "shared or queued Codex invocation"
    );
    let paths = writable_rollouts(process.pid)?;
    locator_from_paths(process, session, &paths, deadline)
}
fn locator_from_paths(
    process: &AgentProcessIdentity,
    session: &str,
    paths: &[PathBuf],
    deadline: Instant,
) -> Result<TranscriptLocator> {
    ensure!(
        crate::daemon::lifecycle::agent_process_start_token(process.pid)? == process.start_token,
        "process replaced"
    );
    let args = crate::question_notice::profile::process_arguments(process.pid)
        .ok_or_else(|| anyhow::anyhow!("arguments unavailable"))?;
    ensure!(
        crate::question_notice::profile::independent_arguments(&args),
        "shared or queued invocation"
    );
    let locator = session_locator(session, paths, deadline)?;
    ensure!(
        crate::daemon::lifecycle::agent_process_start_token(process.pid)? == process.start_token,
        "process replaced"
    );
    Ok(locator)
}

fn session_locator(
    session: &str,
    paths: &[PathBuf],
    deadline: Instant,
) -> Result<TranscriptLocator> {
    use crate::question_notice::turn_order::{SessionHeader, session_header};

    ensure!(
        paths.len() <= 128,
        "session writer candidate limit exceeded"
    );
    ensure!(!paths.is_empty(), "current session writer is unavailable");
    // Subagent writers do not own the parent input. A second root writer still
    // makes the current session ambiguous, even if canonical hooks are stale.
    let expected_suffix = format!("-{session}.jsonl");
    let expected_digest = identifier_digest(session);
    let mut root = None;
    for path in paths {
        ensure!(
            Instant::now() < deadline,
            "session verification deadline exceeded"
        );
        // The exact writer, not a filesystem search, supplies home and session.
        let home = path
            .ancestors()
            .find(|path| path.file_name().is_some_and(|name| name == "sessions"))
            .and_then(Path::parent)
            .ok_or_else(|| anyhow::anyhow!("invalid rollout path"))?;
        let locator = TranscriptLocator::capture(home, path)
            .ok_or_else(|| anyhow::anyhow!("rollout identity unavailable"))?;
        let header = session_header(&locator, deadline);
        ensure!(
            Instant::now() < deadline,
            "session verification deadline exceeded"
        );
        match header
            .ok_or_else(|| anyhow::anyhow!("session header is invalid or exceeds its byte limit"))?
        {
            SessionHeader::NonRoot => {}
            SessionHeader::Root { session_digest } => {
                ensure!(root.is_none(), "multiple root session writers are open");
                let name_matches = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(&expected_suffix));
                root = Some((locator, name_matches && session_digest == expected_digest));
            }
        }
    }
    let (locator, matches) =
        root.ok_or_else(|| anyhow::anyhow!("current root session writer is unavailable"))?;
    ensure!(matches, "current session root metadata does not match");
    Ok(locator)
}

#[cfg(target_os = "macos")]
fn rollout_batch(pids: &[u32]) -> Result<BTreeMap<u32, Vec<PathBuf>>> {
    if pids.is_empty() {
        return Ok(BTreeMap::new());
    }
    ensure!(pids.len() <= 512, "too many Codex writers");
    let pids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // lsof exit 1 also means an individual requested PID had no matches.
    // Keep other PIDs' records; each record still requires exact owner/session proof.
    let output = crate::tmux::run_command_with_accepted_exit_codes(
        "lsof",
        &["-a", "-p", &pids, "-Ffan"],
        Duration::from_millis(500),
        512 * 1024,
        &[0, 1],
    )?;
    let mut results = BTreeMap::new();
    for group in output.split("\np") {
        let group = group.strip_prefix('p').unwrap_or(group);
        let Some((pid, records)) = group.split_once('\n') else {
            continue;
        };
        if let Ok(pid) = pid.parse() {
            results.insert(pid, parse_writable_rollouts(records));
        }
    }
    Ok(results)
}
#[cfg(not(target_os = "macos"))]
fn rollout_batch(pids: &[u32]) -> Result<BTreeMap<u32, Vec<PathBuf>>> {
    ensure!(pids.len() <= 512, "too many Codex writers");
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut results = BTreeMap::new();
    for pid in pids {
        if Instant::now() >= deadline {
            break;
        }
        results.insert(*pid, writable_rollouts(*pid).unwrap_or_default());
    }
    Ok(results)
}

#[cfg(target_os = "macos")]
fn writable_rollouts(pid: u32) -> Result<Vec<PathBuf>> {
    let output = crate::tmux::run_command_with_accepted_exit_codes(
        "lsof",
        &["-a", "-p", &pid.to_string(), "-Ffan"],
        Duration::from_millis(500),
        128 * 1024,
        &[0, 1],
    )?;
    Ok(parse_writable_rollouts(&output))
}
#[cfg(target_os = "macos")]
fn parse_writable_rollouts(output: &str) -> Vec<PathBuf> {
    let mut writable = false;
    let mut paths = Vec::new();
    for line in output.lines() {
        if line.starts_with('f') {
            writable = false;
        }
        if line == "aw" || line == "au" {
            writable = true;
        }
        if writable && let Some(path) = line.strip_prefix('n') {
            let path = PathBuf::from(path);
            if rollout_path(&path) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}
#[cfg(target_os = "linux")]
fn writable_rollouts(pid: u32) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(100);
    for (count, entry) in std::fs::read_dir(format!("/proc/{pid}/fd"))?.enumerate() {
        ensure!(
            count < 4096 && Instant::now() < deadline,
            "process descriptor budget exceeded"
        );
        let entry = entry?;
        let info = std::fs::read_to_string(format!(
            "/proc/{pid}/fdinfo/{}",
            entry.file_name().to_string_lossy()
        ))?;
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:\t"))
            .and_then(|value| u32::from_str_radix(value, 8).ok());
        if flags.is_some_and(|flags| flags & libc::O_ACCMODE as u32 != libc::O_RDONLY as u32) {
            let path = std::fs::read_link(entry.path())?;
            if rollout_path(&path) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn writable_rollouts(_pid: u32) -> Result<Vec<PathBuf>> {
    anyhow::bail!("active rollout observation is unsupported")
}
fn rollout_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .any(|component| component.as_os_str() == "sessions")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }

    struct Rollouts(PathBuf);

    impl Rollouts {
        fn new() -> Self {
            let home = std::env::temp_dir().join(format!(
                "vt-codex-rollouts-{}",
                crate::pane_state::EventId::generate().unwrap().as_str()
            ));
            std::fs::create_dir_all(home.join("sessions")).unwrap();
            Self(home)
        }

        fn write(&self, relative: &str, body: &str) -> PathBuf {
            let path = self.0.join("sessions").join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Rollouts {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exact_root_session_is_selected_among_active_and_completed_subagent_writers() {
        let fixture = Rollouts::new();
        let root = fixture.write(
            "rollout-root.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"root\",\"thread_source\":\"user\"}}\n",
        );
        let mut paths = vec![fixture.write(
            "rollout-subagent-active.jsonl",
            &format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"subagent-active\",\"session_id\":\"root\",\"forked_from_id\":\"root\",\"thread_source\":\"subagent\",\"source\":{{\"subagent\":{{\"thread_spawn\":{{\"parent_thread_id\":\"root\"}}}}}},\"base_instructions\":{{\"text\":\"{}\"}}}}}}\n", "x".repeat(23 * 1024)),
        )];
        paths.push(root.clone());
        paths.push(fixture.write(
            "rollout-subagent-completed.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"subagent-completed\",\"thread_source\":\"subagent\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"child-turn\"}}\n",
        ));
        paths.push(fixture.write(
            "rollout-voice.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"voice\",\"thread_source\":\"realtime_voice\"}}\n",
        ));
        let selected = session_locator("root", &paths, deadline()).unwrap();
        assert_eq!(selected.transcript, root.canonicalize().unwrap());
        assert_eq!(
            selected,
            TranscriptLocator::capture(&fixture.0, &root).unwrap()
        );
        let mut body = std::fs::read_to_string(&root).unwrap();
        body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"capacity-turn\",\"error\":{\"codex_error_info\":\"server_overloaded\"}}}\n");
        std::fs::write(&root, body).unwrap();
        assert!(crate::codex_capacity::read_failure(&selected, "capacity-turn", &mut 0).unwrap());
        assert!(session_locator("subagent-active", &paths, deadline()).is_err());
        assert!(session_locator("subagent-completed", &paths, deadline()).is_err());
    }

    #[test]
    fn root_session_selection_rejects_missing_ambiguous_and_untrusted_writers() {
        let fixture = Rollouts::new();
        let valid = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"root\",\"thread_source\":\"user\"}}\n";
        let root = fixture.write("rollout-root.jsonl", valid);
        assert!(session_locator("root", &[], deadline()).is_err());
        assert!(session_locator("missing", std::slice::from_ref(&root), deadline()).is_err());
        let duplicate = fixture.write("duplicate/rollout-root.jsonl", valid);
        assert!(session_locator("root", &[root.clone(), duplicate], deadline()).is_err());
        let other_root = fixture.write("rollout-other.jsonl", &valid.replace("root", "other"));
        let two_roots = [root.clone(), other_root];
        assert!(session_locator("root", &two_roots, deadline()).is_err());
        assert!(session_locator("other", &two_roots, deadline()).is_err());
        let unverified_child = fixture.write(
            "rollout-child.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\"}}\n",
        );
        assert!(session_locator("root", &[root.clone(), unverified_child], deadline()).is_err());
        assert!(session_locator("root", std::slice::from_ref(&root), Instant::now()).is_err());
        assert!(session_locator("root", &vec![root.clone(); 129], deadline()).is_err());
        for body in [
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"other\",\"thread_source\":\"user\"}}\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"root\",\"thread_source\":\"subagent\"}}\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"root\",\"thread_source\":\"user\",\"parent_thread_id\":\"parent\"}}\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"root\"}}\n",
            "{\"type\":\"session_meta\",\"payload\":",
        ] {
            std::fs::write(&root, body).unwrap();
            assert!(session_locator("root", std::slice::from_ref(&root), deadline()).is_err());
        }
        std::fs::write(&root, format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"root\",\"thread_source\":\"user\",\"base_instructions\":{{\"text\":\"{}\"}}}}}}\n", "x".repeat(256 * 1024))).unwrap();
        assert!(session_locator("root", std::slice::from_ref(&root), deadline()).is_err());
        std::fs::write(&root, valid).unwrap();
        let symlink = fixture.0.join("sessions/symlink/rollout-root.jsonl");
        std::fs::create_dir_all(symlink.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&root, &symlink).unwrap();
        assert!(session_locator("root", &[symlink], deadline()).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn partial_descriptor_output_with_diagnostics_is_not_trusted() {
        let read = |script| {
            crate::tmux::run_command_with_accepted_exit_codes(
                "/bin/sh",
                &["-c", script],
                std::time::Duration::from_millis(500),
                1024,
                &[0, 1],
            )
        };
        assert_eq!(read("printf partial; exit 1").unwrap(), "partial");
        assert!(read("printf partial; printf failure >&2; exit 1").is_err());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn exited_process_does_not_discard_other_process_descriptors() {
        let pid = std::process::id();
        let records = super::rollout_batch(&[pid, 99_999_999]).unwrap();
        assert!(records.contains_key(&pid));
        assert!(!records.contains_key(&99_999_999));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn only_writable_unique_rollout_descriptors_identify_the_active_session() {
        let output = "p1\nf4\nar\nn/home/.codex/sessions/rollout-old.jsonl\nf5\nau\nn/home/.codex/sessions/rollout-new.jsonl\nf6\naw\nn/home/.codex/sessions/rollout-new.jsonl\nf7\naw\nn/home/.codex/log.sqlite\n";
        assert_eq!(
            super::parse_writable_rollouts(output),
            vec![std::path::PathBuf::from(
                "/home/.codex/sessions/rollout-new.jsonl"
            )]
        );
    }
}

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use super::{MAX_ANCESTORS, NoticeReason, QuestionNoticeInput};
use crate::pane_state::{AgentProcessIdentity, IDENTIFIER_MAX_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputClass {
    OrdinaryPrompt,
    NonAuthoritativeInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSource {
    Startup,
    Resume,
    Clear,
    Fork,
    Compact,
    Unknown,
}

/// Private protocol metadata. It is never part of Pane State or durable Run storage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverInput {
    pub daemon_generation: Option<crate::pane_state::DaemonInstanceId>,
    pub startup_header_verified: bool,
    pub startup_journal: Option<StartupJournal>,
    pub parent_origin_verified: bool,
    pub ancestors: Vec<AgentProcessIdentity>,
    pub profile: super::profile::CodexProfile,
    pub process: Option<super::profile::ProfileRequest>,
    pub input_class: InputClass,
    pub source: SessionSource,
    pub home_digest: Option<String>,
    pub journal_root_digest: Option<String>,
    pub locator: Option<TranscriptLocator>,
    pub journal_failure: Option<super::journal::JournalFailure>,
    pub journal_failure_reported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupJournal {
    pub epoch: u64,
    pub session_dirty: bool,
}

impl ResolverInput {
    pub fn validate(&self) -> bool {
        self.ancestors.len() <= MAX_ANCESTORS
            && self
                .ancestors
                .iter()
                .all(|ancestor| ancestor.validate().is_ok())
            && self.home_digest.as_ref().is_none_or(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            && self.journal_root_digest.as_ref().is_none_or(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            && self
                .process
                .as_ref()
                .is_none_or(|request| request.process.validate().is_ok())
            && self.locator.as_ref().is_none_or(|locator| {
                locator.home.is_absolute()
                    && locator.transcript.is_absolute()
                    && locator.home.as_os_str().len() <= 4096
                    && locator.transcript.as_os_str().len() <= 4096
                    && self.home_digest.as_ref() == Some(&locator.home_digest())
            })
    }
}

/// Paths exist solely for IO by the bounded structural reader. Debug omits them.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptLocator {
    pub home: std::path::PathBuf,
    pub transcript: std::path::PathBuf,
    pub dev: u64,
    pub ino: u64,
}

impl std::fmt::Debug for TranscriptLocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptLocator")
            .field("dev", &self.dev)
            .field("ino", &self.ino)
            .finish_non_exhaustive()
    }
}

impl TranscriptLocator {
    pub fn capture(home: &Path, transcript: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let declared_home = home;
        let home = home.canonicalize().ok()?;
        if home.as_os_str().len() > 4096 || transcript.as_os_str().len() > 4096 {
            return None;
        }
        // The home itself may be an OS alias (macOS /var -> /private/var).
        // Only its descendant components are required to be symlink-free.
        let relative = transcript
            .strip_prefix(&home)
            .or_else(|_| transcript.strip_prefix(declared_home))
            .ok()?;
        if !relative.starts_with("sessions") {
            return None;
        }
        let mut path = home.clone();
        for component in relative.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                return None;
            }
            path.push(component);
            let metadata = std::fs::symlink_metadata(&path).ok()?;
            if metadata.file_type().is_symlink() || metadata.uid() != unsafe { libc::geteuid() } {
                return None;
            }
        }
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(Self {
            home,
            transcript: path,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    pub fn matches_current_file(&self) -> bool {
        Self::capture(&self.home, &self.transcript).as_ref() == Some(self)
    }

    pub fn home_digest(&self) -> String {
        super::digest(&self.home)
    }
}

impl SessionSource {
    pub fn from_payload(value: Option<&str>) -> Self {
        match value {
            Some("startup") => Self::Startup,
            Some("resume") => Self::Resume,
            Some("clear") => Self::Clear,
            Some("fork") => Self::Fork,
            Some("compact") => Self::Compact,
            _ => Self::Unknown,
        }
    }
}

/// This deliberately accepts the same payload for direct and queued submission.
/// Framing is used only as a veto, never as a source of identifiers or digests.
pub fn classify_prompt(prompt: Option<&str>, profile: super::profile::CodexProfile) -> InputClass {
    use super::profile::CodexProfile;
    let Some(prompt) = prompt.filter(|value| !value.trim().is_empty() && value.len() <= 64 * 1024)
    else {
        return InputClass::NonAuthoritativeInput;
    };
    if profile == CodexProfile::Unknown
        || prompt
            .lines()
            .any(|line| line.trim_start().starts_with('>'))
        || ["send_user_message", "question_reply", "questionItemId"]
            .into_iter()
            .any(|framing| prompt.contains(framing))
        || prompt.split('<').skip(1).any(|tail| {
            let tag = tail
                .trim_start_matches('/')
                .split(['>', ' ', '\n'])
                .next()
                .unwrap_or("");
            tag.len() >= 4 && "send_user_message_question_reply".starts_with(tag)
        })
    {
        InputClass::NonAuthoritativeInput
    } else {
        InputClass::OrdinaryPrompt
    }
}

/// A malformed PostToolUse may have issued a question. A known other tool did not.
pub fn needs_provisional_journal(event: &str, raw: &str) -> bool {
    if event != "PostToolUse" {
        return false;
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_none_or(|name| name.trim().is_empty() || name == "request_user_input_async")
}

#[derive(Debug)]
pub struct PreparedHook {
    pub metadata: ResolverInput,
    pub journal: Option<(super::journal::JournalLocation, String)>,
    pub excluded: bool,
    pub mode_excluded: bool,
    pub relevant: bool,
}

impl PreparedHook {
    /// Performs no daemon IO. The provisional entry must precede daemon startup/delivery.
    pub fn prepare(
        event: &str,
        raw: &str,
        home: Option<&Path>,
        env: &std::collections::BTreeMap<String, String>,
        runner: &dyn crate::tmux::TmuxRunner,
        now: i64,
        deadline: std::time::Instant,
    ) -> Self {
        use super::journal::{DirtyEntry, DirtyReason, JournalFailure, JournalLocation};
        let relevant = matches!(event, "SessionStart" | "UserPromptSubmit" | "Stop")
            || needs_provisional_journal(event, raw);
        if !relevant {
            return Self {
                metadata: ResolverInput {
                    daemon_generation: None,
                    startup_header_verified: false,
                    startup_journal: None,
                    parent_origin_verified: false,
                    ancestors: Vec::new(),
                    profile: super::profile::CodexProfile::Unknown,
                    process: None,
                    input_class: InputClass::NonAuthoritativeInput,
                    source: SessionSource::Unknown,
                    home_digest: None,
                    journal_root_digest: None,
                    locator: None,
                    journal_failure: None,
                    journal_failure_reported: false,
                },
                journal: None,
                excluded: false,
                mode_excluded: false,
                relevant: false,
            };
        }
        let payload = serde_json::from_str::<Value>(raw).ok();
        let text = |key| {
            payload
                .as_ref()
                .and_then(|value| value.get(key))
                .and_then(Value::as_str)
        };
        let origin = crate::hook::origin::codex_hook_origin_from_payload(
            text("session_id"),
            text("agent_id"),
            text("transcript_path"),
            home,
        );
        let outside_tmux = !env.get("TMUX").is_some_and(|value| !value.is_empty())
            || !env.get("TMUX_PANE").is_some_and(|value| !value.is_empty());
        let excluded = outside_tmux
            || origin == crate::hook::origin::HookOrigin::NonParent
            || crate::daemon::lifecycle::tmux_desired_mode(runner, env)
                .is_ok_and(|mode| mode == crate::daemon::lifecycle::DesiredMode::Disabled);
        let canonical_home = home.and_then(|path| path.canonicalize().ok());
        let home_digest = canonical_home.as_ref().map(super::digest);
        let ancestors = if excluded {
            Vec::new()
        } else {
            capture_ancestors().unwrap_or_default()
        };
        let mode_excluded = !excluded
            && ancestors
                .iter()
                .any(super::profile::positively_non_embedded);
        let excluded = excluded || mode_excluded;
        let process = if excluded {
            None
        } else {
            crate::hook::writer::resolve_pane_instance(runner, env)
                .ok()
                .flatten()
                .and_then(|pane| {
                    runner
                        .resolve_agent_process(
                            pane.pane_pid,
                            &crate::pane_state::AgentKind::parse("codex").expect("constant kind"),
                        )
                        .ok()
                        .flatten()
                })
                .filter(|process| ancestors.contains(process))
                .and_then(super::profile::ProfileRequest::capture)
        };
        let locator = home
            .zip(text("transcript_path"))
            .and_then(|(home, transcript)| TranscriptLocator::capture(home, Path::new(transcript)));
        let source = SessionSource::from_payload(text("source"));
        let startup_header_verified = !excluded
            && source == SessionSource::Startup
            && locator
                .as_ref()
                .zip(text("session_id"))
                .is_some_and(|(locator, session)| {
                    super::turn_order::verify_startup_header(
                        locator,
                        &super::turn_order::identifier_digest(session),
                        deadline.min(
                            std::time::Instant::now() + std::time::Duration::from_millis(1500),
                        ),
                    )
                });
        let mut prepared = Self {
            metadata: ResolverInput {
                daemon_generation: None,
                startup_header_verified,
                startup_journal: None,
                parent_origin_verified: origin.is_parent(),
                ancestors,
                profile: super::profile::CodexProfile::Unknown,
                process,
                input_class: InputClass::NonAuthoritativeInput,
                source,
                home_digest: home_digest.clone(),
                journal_root_digest: if excluded {
                    None
                } else {
                    home_digest
                        .clone()
                        .and_then(|home| JournalLocation::new(env, home).ok())
                        .and_then(|location| location.root_digest().ok())
                },
                locator,
                journal_failure: None,
                journal_failure_reported: false,
            },
            journal: None,
            excluded,
            mode_excluded,
            relevant,
        };
        if !excluded && needs_provisional_journal(event, raw) {
            let result = (|| {
                let home = home_digest.ok_or(JournalFailure::Unavailable)?;
                let location = JournalLocation::new(env, home.clone())?;
                let identifiers = (
                    text("session_id").filter(|id| valid_identifier(id)),
                    text("turn_id").filter(|id| valid_identifier(id)),
                    text("tool_use_id").filter(|id| valid_identifier(id)),
                );
                let reason = if text("tool_name") == Some("request_user_input_async") {
                    DirtyReason::QuestionHook
                } else {
                    DirtyReason::UnclassifiedHook
                };
                let entry = DirtyEntry::new(home, identifiers, reason, now)?;
                let mut guard = location.lock_hook(deadline)?;
                let key = guard.insert(entry)?;
                // Drop before daemon roundtrip; do not make the daemon wait on its sender.
                drop(guard);
                Ok((location, key))
            })();
            match result {
                Ok(ticket) => prepared.journal = Some(ticket),
                Err(reason) => prepared.metadata.journal_failure = Some(reason),
            }
        }
        if !excluded && event == "SessionStart" && source == SessionSource::Startup {
            let result = (|| {
                let home = prepared
                    .metadata
                    .home_digest
                    .clone()
                    .ok_or(JournalFailure::Unavailable)?;
                let session = text("session_id")
                    .filter(|id| valid_identifier(id))
                    .map(super::turn_order::identifier_digest)
                    .ok_or(JournalFailure::Invalid)?;
                let location = JournalLocation::new(env, home)?;
                let mut guard = location.lock_hook(deadline)?;
                let observation = guard.evaluate(Some(&session), super::journal::writer_state)?;
                Ok(StartupJournal {
                    epoch: observation.epoch,
                    session_dirty: observation.session_dirty,
                })
            })();
            match result {
                Ok(journal) => prepared.metadata.startup_journal = Some(journal),
                Err(reason) => prepared.metadata.journal_failure = Some(reason),
            }
        }
        prepared
    }

    pub fn excluded_notice(
        &self,
        event: &str,
        raw: &str,
        codex_home: Option<&Path>,
    ) -> Option<QuestionNoticeInput> {
        if !self.mode_excluded {
            return None;
        }
        from_payload(event, raw, codex_home).map(|notice| match notice {
            QuestionNoticeInput::Issued { .. } => QuestionNoticeInput::Rejected {
                reason: NoticeReason::AncestorNotInPane,
            },
            other => other,
        })
    }

    pub fn classify(&mut self, raw: &str, profile: super::profile::CodexProfile) {
        self.metadata.profile = profile;
        self.metadata.input_class = serde_json::from_str::<Value>(raw)
            .ok()
            .map(|value| classify_prompt(value.get("prompt").and_then(Value::as_str), profile))
            .unwrap_or(InputClass::NonAuthoritativeInput);
    }

    pub fn persisted(&self, result: &super::NoticeResult) {
        if result.durability != Some(super::NoticeDurability::Persisted) {
            return;
        }
        if let Some((location, key)) = &self.journal
            && let Ok(mut guard) =
                location.lock_hook(std::time::Instant::now() + std::time::Duration::from_secs(2))
        {
            let _ = guard.clear_persisted(key);
        }
    }
}

pub fn from_payload(
    event: &str,
    raw: &str,
    codex_home: Option<&Path>,
) -> Option<QuestionNoticeInput> {
    let payload: Value = serde_json::from_str(raw).ok()?;
    if payload.get("tool_name").and_then(Value::as_str) != Some("request_user_input_async") {
        return None;
    }
    let rejected = |reason| Some(QuestionNoticeInput::Rejected { reason });
    if event != "PostToolUse" {
        return None;
    }
    let text = |key| payload.get(key).and_then(Value::as_str);
    if text("hook_event_name") != Some("PostToolUse")
        || text("tool_response")
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .is_none_or(|value| value.get("accepted") != Some(&Value::Bool(true)))
    {
        return rejected(NoticeReason::InvalidPayload);
    }
    let Some((session, turn, tool)) = text("session_id")
        .zip(text("turn_id"))
        .zip(text("tool_use_id"))
        .map(|((session, turn), tool)| (session, turn, tool))
        .filter(|(session, turn, tool)| [*session, *turn, *tool].into_iter().all(valid_identifier))
    else {
        return rejected(NoticeReason::InvalidPayload);
    };
    if !crate::hook::origin::codex_hook_origin_from_payload(
        Some(session),
        text("agent_id"),
        text("transcript_path"),
        codex_home,
    )
    .is_parent()
    {
        return rejected(NoticeReason::OriginUnverified);
    }
    Some(QuestionNoticeInput::Issued {
        session_id: session.to_string(),
        turn_id: turn.to_string(),
        tool_use_id: tool.to_string(),
        ancestors: Vec::new(),
    })
}

pub fn valid_identifier(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= IDENTIFIER_MAX_BYTES && !value.contains(['\r', '\n'])
}

/// Capture once at hook entry. Missing/reused ancestors invalidate the whole chain.
pub fn capture_ancestors() -> anyhow::Result<Vec<AgentProcessIdentity>> {
    let mut pid = std::process::id();
    let mut seen = BTreeSet::new();
    let mut chain = Vec::new();
    for _ in 0..MAX_ANCESTORS {
        if pid <= 1 {
            return Ok(chain);
        }
        anyhow::ensure!(seen.insert(pid), "cyclic hook ancestry");
        let start_token = crate::daemon::lifecycle::agent_process_start_token(pid)?;
        let parent = parent_pid(pid)?;
        anyhow::ensure!(
            crate::daemon::lifecycle::agent_process_start_token(pid)? == start_token,
            "hook ancestor replaced"
        );
        chain.push(AgentProcessIdentity { pid, start_token });
        pid = parent;
    }
    anyhow::bail!("hook ancestor depth exceeded")
}

pub(super) fn parent_pid(pid: u32) -> anyhow::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_pidinfo writes a checked, fixed-size C record.
        let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let count = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        anyhow::ensure!(
            count == size && info.pbi_pid == pid,
            "hook ancestor unavailable"
        );
        Ok(info.pbi_ppid)
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields = stat
            .rsplit_once(") ")
            .ok_or_else(|| anyhow::anyhow!("invalid process stat"))?
            .1;
        Ok(fields
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("missing parent PID"))?
            .parse()?)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        anyhow::bail!("hook ancestor verification is unsupported on this OS")
    }
}

/// Recheck every link supplied by a still-running hook, including process reuse.
pub fn verify_ancestors(chain: &[AgentProcessIdentity], owner: &AgentProcessIdentity) -> bool {
    !chain.is_empty()
        && chain.len() <= MAX_ANCESTORS
        && chain.contains(owner)
        && chain.iter().all(|process| {
            process.validate().is_ok()
                && crate::daemon::lifecycle::agent_process_start_token(process.pid)
                    .is_ok_and(|token| token == process.start_token)
        })
        && chain
            .windows(2)
            .all(|pair| parent_pid(pair[0].pid).is_ok_and(|parent| parent == pair[1].pid))
}

#[cfg(test)]
mod classification_tests {
    use super::*;
    use crate::question_notice::profile::CodexProfile;

    #[test]
    fn answer_framing_and_partial_envelopes_are_vetoes_for_both_versions_and_routes() {
        for profile in [CodexProfile::V01551, CodexProfile::V01561] {
            for _route in ["direct", "queue"] {
                for input in [
                    "",
                    "  ",
                    "> synthetic question\n\nsynthetic answer",
                    "</send_user_message_question_reply>",
                    "<send_user_message_question_reply>{\"questionItemId\":\"synthetic\"}</send_user_message_question_reply>",
                    "<send_user_",
                    "{\"questionItemId\": broken",
                    "text\n> quoted question\n\nanswer",
                ] {
                    assert_eq!(
                        classify_prompt(Some(input), profile),
                        InputClass::NonAuthoritativeInput
                    );
                }
                assert_eq!(
                    classify_prompt(Some("continue with the next task"), profile),
                    InputClass::OrdinaryPrompt
                );
                assert_eq!(
                    classify_prompt(Some(&"x".repeat(65537)), profile),
                    InputClass::NonAuthoritativeInput
                );
            }
        }
        assert_eq!(
            classify_prompt(Some("continue"), CodexProfile::Unknown),
            InputClass::NonAuthoritativeInput
        );
    }

    #[test]
    fn positive_mode_exclusion_preserves_rejection_diagnostic_without_journal() {
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        let mut prepared = PreparedHook::prepare(
            "PreToolUse",
            "{}",
            None,
            &Default::default(),
            &runner,
            0,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        );
        prepared.excluded = true;
        prepared.mode_excluded = true;
        assert_eq!(
            prepared.excluded_notice(
                "PostToolUse",
                r#"{"tool_name":"request_user_input_async"}"#,
                None
            ),
            Some(QuestionNoticeInput::Rejected {
                reason: NoticeReason::InvalidPayload
            })
        );
        assert!(prepared.journal.is_none());
        assert!(prepared.excluded_notice("Stop", "{}", None).is_none());
        prepared.mode_excluded = false;
        assert!(
            prepared
                .excluded_notice(
                    "PostToolUse",
                    r#"{"tool_name":"request_user_input_async"}"#,
                    None
                )
                .is_none()
        );
    }

    #[test]
    fn only_positive_other_tool_excludes_provisional_dirty() {
        assert!(!needs_provisional_journal("PreToolUse", "invalid"));
        assert!(!needs_provisional_journal(
            "PostToolUse",
            r#"{"tool_name":"exec_command"}"#
        ));
        for raw in [
            "broken",
            "{}",
            r#"{"tool_name":null}"#,
            r#"{"tool_name":"request_user_input_async"}"#,
        ] {
            assert!(needs_provisional_journal("PostToolUse", raw));
        }
    }
}

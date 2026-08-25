use std::collections::BTreeMap;

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::super::connection::ApiConnection;
use super::super::contract::{
    AgentSummary, AgentWaitTarget, ApiAgentStartCapability, ApiAgentSteerCapability,
    ApiPromptConfirmationCapability, ApiPromptDispatchCapability, ApiProviderCapabilities,
    ApiResponseCapability, MAX_PROMPT_BYTES,
};
use super::super::pane::{pane_ref, require_same_pane, resolve_pane, verify_live_pane};
use super::projection::{agent_summary, canonical_state};
use crate::daemon::protocol::v2::{PanePresentation, ResolvedSnapshot};
use crate::pane_state::PaneInstance;
use crate::tmux::TmuxRunner;

pub(in crate::api) fn validate_prompt(prompt: &str) -> Result<()> {
    if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err(api_error!(
            "invalid_arguments",
            format!("prompt must contain between 1 and {MAX_PROMPT_BYTES} UTF-8 bytes"),
        )
        .into());
    }
    if prompt.chars().any(|character| {
        matches!(character, '\r' | '\t' | '\u{7f}')
            || (character.is_control() && character != '\n')
            || ('\u{80}'..='\u{9f}').contains(&character)
    }) {
        return Err(api_error!(
            "invalid_arguments",
            "prompt may contain LF newlines but must not contain CR, TAB, C0, DEL, or C1 controls",
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn provider_capabilities(agent: &str) -> Option<ApiProviderCapabilities> {
    match agent {
        "codex" => Some(ApiProviderCapabilities {
            prompt_dispatch: ApiPromptDispatchCapability::Durable,
            steer: ApiAgentSteerCapability::GuardedTerminalBestEffort,
            prompt_confirmation: ApiPromptConfirmationCapability::ProviderDigest,
            response: ApiResponseCapability::Artifact,
            interactive_keys: true,
            start: ApiAgentStartCapability::DurableInitialPrompt,
        }),
        "claude" => Some(ApiProviderCapabilities {
            prompt_dispatch: ApiPromptDispatchCapability::GuardedTerminal,
            steer: ApiAgentSteerCapability::GuardedTerminalBestEffort,
            prompt_confirmation: ApiPromptConfirmationCapability::LifecycleCursor,
            response: ApiResponseCapability::TerminalRead,
            interactive_keys: true,
            start: ApiAgentStartCapability::ProviderSession,
        }),
        "opencode" => Some(ApiProviderCapabilities {
            prompt_dispatch: ApiPromptDispatchCapability::GuardedTerminal,
            steer: ApiAgentSteerCapability::Disabled,
            prompt_confirmation: ApiPromptConfirmationCapability::None,
            response: ApiResponseCapability::TerminalRead,
            interactive_keys: true,
            start: ApiAgentStartCapability::InputOwnerOnly,
        }),
        _ => None,
    }
}

pub(in crate::api) fn agent_start_readiness_matches(
    readiness: ApiAgentStartCapability,
    state: &crate::pane_state::PaneState,
) -> bool {
    match readiness {
        ApiAgentStartCapability::DurableInitialPrompt | ApiAgentStartCapability::InputOwnerOnly => {
            true
        }
        ApiAgentStartCapability::ProviderSession => {
            state.agent_session_id.is_some() && state.scan_verified && state.agent_process.is_some()
        }
        ApiAgentStartCapability::Disabled => false,
    }
}

pub(in crate::api) fn validate_terminal_keys(keys: &[String]) -> Result<()> {
    const NAMED_KEYS: [&str; 23] = [
        "Enter", "Escape", "Tab", "BSpace", "Up", "Down", "Left", "Right", "Home", "End", "PPage",
        "NPage", "IC", "DC", "C-c", "C-d", "C-g", "C-z", "Space", "F1", "F2", "F3", "F4",
    ];
    if keys.is_empty() || keys.len() > 16 {
        return Err(api_error!(
            "invalid_arguments",
            "agent send-keys requires between 1 and 16 keys",
        )
        .into());
    }
    for key in keys {
        let single_literal = {
            let mut chars = key.chars();
            chars
                .next()
                .is_some_and(|character| !character.is_control())
                && chars.next().is_none()
        };
        if !single_literal && !NAMED_KEYS.contains(&key.as_str()) {
            return Err(api_error!(
                "invalid_arguments",
                format!("unsupported logical key: {key}"),
            )
            .into());
        }
    }
    Ok(())
}

pub(in crate::api) fn validate_start_args(args: &[String]) -> Result<()> {
    if args.len() > 64 {
        return Err(api_error!(
            "invalid_arguments",
            "agent start accepts at most 64 arguments",
        )
        .into());
    }
    let mut total = 0_usize;
    for arg in args {
        if arg.len() > 4_096
            || arg.chars().any(|character| {
                character == '\0'
                    || character == '\r'
                    || character == '\n'
                    || character.is_control()
            })
        {
            return Err(api_error!(
                "invalid_arguments",
                "agent start arguments must be at most 4,096 bytes and contain no controls",
            )
            .into());
        }
        total = total.saturating_add(arg.len());
    }
    if total > MAX_PROMPT_BYTES {
        return Err(api_error!(
            "invalid_arguments",
            format!("agent start arguments exceed {MAX_PROMPT_BYTES} bytes"),
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn provider_program(agent: &str) -> Option<&'static str> {
    match agent {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

pub(in crate::api) fn supported_shell(command: &str) -> bool {
    matches!(command, "bash" | "dash" | "fish" | "sh" | "zsh")
}

pub(in crate::api) fn render_shell_command(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(in crate::api) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(in crate::api) fn verify_agent_input_target(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    connection: &ApiConnection,
    pane: &PanePresentation,
    identity: &AgentIdentity,
) -> Result<()> {
    verify_live_pane(runner, env, connection, &identity.pane_instance)?;
    verify_live_agent_process(runner, identity, pane)?;
    runner
        .verify_agent_input_owner(identity.pane_instance.pane_pid, identity.agent_process.pid)
        .map_err(|error| {
            api_error!(
                "agent_not_input_owner",
                format!(
                    "agent in pane {} is not the foreground input owner: {error:#}",
                    identity.pane_instance.pane_id
                ),
            )
        })?;
    Ok(())
}

pub(in crate::api) fn resolve_agent<'a>(
    snapshot: &'a ResolvedSnapshot,
    target: &str,
    server_identity: &str,
) -> Result<&'a PanePresentation> {
    if target.starts_with("vta1:") {
        let identity = parse_agent_ref(target, server_identity)?;
        return require_same_agent(snapshot, &identity);
    }
    let pane = resolve_pane(snapshot, target, server_identity)?;
    let Some(resolved) = pane.resolved.as_ref() else {
        return Err(api_error!(
            "agent_not_found",
            format!("pane {} has no present agent", pane.pane_instance.pane_id),
        )
        .into());
    };
    if !resolved.canonical.agent_present {
        return Err(api_error!(
            "agent_not_found",
            format!("pane {} has no present agent", pane.pane_instance.pane_id),
        )
        .into());
    }
    Ok(pane)
}

pub(in crate::api) fn resolve_wait_resume_agent<'a>(
    snapshot: &'a ResolvedSnapshot,
    target: &str,
    server_identity: &str,
) -> Result<(&'a PanePresentation, AgentIdentity)> {
    let mut identity = parse_agent_ref(target, server_identity)?;
    let pane = require_same_pane(snapshot, &identity.pane_instance)?;
    let state = canonical_state(pane).ok_or_else(|| {
        api_error!(
            "stale_reference",
            format!(
                "agent state in pane {} is no longer retained",
                identity.pane_instance.pane_id
            ),
        )
    })?;
    if state.state_id.as_str() != identity.state_id || state.agent_epoch != identity.agent_epoch {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} was replaced",
                identity.pane_instance.pane_id
            ),
        )
        .into());
    }
    let persisted_process = state.agent_process.map(agent_process_ref);
    if persisted_process.as_ref() != Some(&identity.agent_process) {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent process baseline in pane {} does not match the reference",
                identity.pane_instance.pane_id
            ),
        )
        .into());
    }
    identity.agent = state.agent.as_str().to_string();
    Ok((pane, identity))
}

#[derive(Debug, Clone)]
pub(in crate::api) struct AgentIdentity {
    pub(in crate::api) pane_instance: PaneInstance,
    pub(in crate::api) state_id: String,
    pub(in crate::api) agent_epoch: u64,
    pub(in crate::api) agent: String,
    pub(in crate::api) agent_process: AgentProcessRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::api) struct AgentProcessRef {
    pub(in crate::api) pid: u32,
    pub(in crate::api) start_token_hash: String,
}

impl AgentIdentity {
    pub(in crate::api) fn from_pane(pane: &PanePresentation) -> Result<Self> {
        let resolved = pane.resolved.as_ref().ok_or_else(|| {
            api_error!(
                "agent_not_found",
                format!("pane {} has no resolved agent", pane.pane_instance.pane_id),
            )
        })?;
        if !resolved.canonical.agent_present {
            return Err(api_error!(
                "stale_reference",
                format!(
                    "agent in pane {} is no longer present",
                    pane.pane_instance.pane_id
                ),
            )
            .into());
        }
        let agent_process = pane
            .agent_process
            .as_ref()
            .map(agent_process_ref)
            .ok_or_else(|| {
                api_error!(
                    "exact_identity_unavailable",
                    format!(
                        "agent in pane {} has no unique live process identity; exact read/wait is unavailable",
                        pane.pane_instance.pane_id
                    ),
                )
            })?;
        Ok(Self {
            pane_instance: pane.pane_instance.clone(),
            state_id: resolved.canonical.state_id.as_str().to_string(),
            agent_epoch: resolved.canonical.agent_epoch,
            agent: resolved.canonical.agent.as_str().to_string(),
            agent_process,
        })
    }
}

pub(in crate::api) fn wait_target(
    pane: &PanePresentation,
    server_identity: &str,
    identity: &AgentIdentity,
    agent_ref_override: Option<&str>,
) -> AgentWaitTarget {
    AgentWaitTarget {
        agent_ref: agent_ref_override
            .map(str::to_string)
            .unwrap_or_else(|| agent_ref(server_identity, pane)),
        pane_ref: pane_ref(server_identity, &identity.pane_instance),
        pane_id: identity.pane_instance.pane_id.clone(),
        pane_pid: identity.pane_instance.pane_pid,
        agent: identity.agent.clone(),
        state_id: identity.state_id.clone(),
        agent_epoch: identity.agent_epoch,
        process_pid: identity.agent_process.pid,
    }
}

pub(in crate::api) fn require_same_agent<'a>(
    snapshot: &'a ResolvedSnapshot,
    expected: &AgentIdentity,
) -> Result<&'a PanePresentation> {
    let pane = require_same_pane(snapshot, &expected.pane_instance)?;
    let Some(resolved) = pane.resolved.as_ref() else {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} is no longer resolved",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    };
    if resolved.canonical.state_id.as_str() != expected.state_id
        || resolved.canonical.agent_epoch != expected.agent_epoch
        || !resolved.canonical.agent_present
    {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    match pane.agent_process.as_ref().map(agent_process_ref) {
        None => Err(api_error!(
            "exact_identity_unavailable",
            format!(
                "agent process in pane {} is not currently uniquely verifiable",
                expected.pane_instance.pane_id
            ),
        )
        .into()),
        Some(actual) if actual != expected.agent_process => Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into()),
        Some(_) => Ok(pane),
    }
}

pub(in crate::api) fn require_same_agent_state<'a>(
    snapshot: &'a ResolvedSnapshot,
    expected: &AgentIdentity,
) -> Result<&'a PanePresentation> {
    let pane = require_same_pane(snapshot, &expected.pane_instance)?;
    let Some(state) = canonical_state(pane) else {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} is no longer resolved",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    };
    if state.state_id.as_str() != expected.state_id
        || state.agent_epoch != expected.agent_epoch
        || state.agent.as_str() != expected.agent
    {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    Ok(pane)
}

pub(in crate::api) fn require_same_agent_process(
    pane: &PanePresentation,
    expected: &AgentIdentity,
) -> Result<()> {
    let actual = pane.agent_process.as_ref().map(agent_process_ref);
    match actual.as_ref() {
        Some(actual) if actual == &expected.agent_process => Ok(()),
        None => Err(api_error!(
            "identity_verification_failed",
            format!(
                "agent process in pane {} is no longer uniquely verifiable",
                expected.pane_instance.pane_id
            ),
        )
        .into()),
        Some(_) => Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into()),
    }
}

pub(in crate::api) fn reject_replaced_agent_process(
    pane: &PanePresentation,
    expected: &AgentIdentity,
) -> Result<()> {
    let Some(actual) = pane.agent_process.as_ref().map(agent_process_ref) else {
        return Ok(());
    };
    if actual != expected.agent_process {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn current_agent_after_event_match(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    connection: &ApiConnection,
    snapshot: &ResolvedSnapshot,
    expected: &AgentIdentity,
) -> Option<AgentSummary> {
    let pane = require_same_agent_state(snapshot, expected).ok()?;
    let state = canonical_state(pane)?;
    if !state.agent_present {
        return None;
    }
    require_same_agent_process(pane, expected).ok()?;
    verify_live_pane(runner, env, connection, &expected.pane_instance).ok()?;
    verify_live_agent_process(runner, expected, pane).ok()?;
    agent_summary(pane, snapshot, &connection.server_identity)
}

pub(in crate::api) fn verify_live_agent_process(
    runner: &dyn TmuxRunner,
    expected: &AgentIdentity,
    pane: &PanePresentation,
) -> Result<()> {
    let actual = resolve_live_agent_process(runner, expected, pane)?;
    if actual.as_ref() != Some(&expected.agent_process) {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn reject_live_agent_process_replacement(
    runner: &dyn TmuxRunner,
    expected: &AgentIdentity,
    pane: &PanePresentation,
) -> Result<()> {
    if resolve_live_agent_process(runner, expected, pane)?
        .is_some_and(|actual| actual != expected.agent_process)
    {
        return Err(api_error!(
            "stale_reference",
            format!(
                "agent process in pane {} was replaced",
                expected.pane_instance.pane_id
            ),
        )
        .into());
    }
    Ok(())
}

pub(in crate::api) fn resolve_live_agent_process(
    runner: &dyn TmuxRunner,
    expected: &AgentIdentity,
    pane: &PanePresentation,
) -> Result<Option<AgentProcessRef>> {
    let state = &pane
        .resolved
        .as_ref()
        .ok_or_else(|| api_error!("stale_reference", "agent state disappeared"))?
        .canonical;
    let actual = runner
        .resolve_agent_process(expected.pane_instance.pane_pid, &state.agent)
        .map_err(|error| {
            api_error!(
                "identity_verification_failed",
                format!(
                    "could not verify the live agent process in pane {}: {error}",
                    expected.pane_instance.pane_id
                ),
            )
        })?
        .as_ref()
        .map(agent_process_ref);
    Ok(actual)
}

pub(in crate::api) fn agent_ref(server_identity: &str, pane: &PanePresentation) -> String {
    let state = &pane
        .resolved
        .as_ref()
        .expect("agent_ref requires resolved agent")
        .canonical;
    let process = pane
        .agent_process
        .as_ref()
        .map(agent_process_ref)
        .expect("agent_ref requires exact agent process identity");
    format!(
        "vta1:{server_identity}:{}:{}:{}:{}:{}:{}",
        pane.pane_instance.pane_id.trim_start_matches('%'),
        pane.pane_instance.pane_pid,
        state.state_id.as_str(),
        state.agent_epoch,
        process.pid,
        process.start_token_hash,
    )
}

pub(in crate::api) fn agent_process_ref(
    identity: &crate::pane_state::AgentProcessIdentity,
) -> AgentProcessRef {
    AgentProcessRef {
        pid: identity.pid,
        start_token_hash: format!("{:x}", Sha256::digest(identity.start_token.as_bytes())),
    }
}

pub(in crate::api) fn parse_agent_ref(value: &str, server_identity: &str) -> Result<AgentIdentity> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 8 || parts[0] != "vta1" {
        return Err(api_error!("invalid_reference", "invalid agent_ref").into());
    }
    if parts[1] != server_identity {
        return Err(api_error!(
            "stale_reference",
            "agent_ref belongs to another tmux server",
        )
        .into());
    }
    let pane_instance = PaneInstance {
        pane_id: format!("%{}", parts[2]),
        pane_pid: parts[3]
            .parse()
            .map_err(|_| api_error!("invalid_reference", "invalid agent_ref pane PID"))?,
    };
    pane_instance
        .validate()
        .map_err(|error| api_error!("invalid_reference", error.to_string()))?;
    crate::pane_state::StateId::parse(parts[4])
        .map_err(|error| api_error!("invalid_reference", error.to_string()))?;
    let agent_epoch = parts[5]
        .parse::<u64>()
        .ok()
        .filter(|epoch| *epoch > 0)
        .ok_or_else(|| api_error!("invalid_reference", "invalid agent_ref epoch"))?;
    let agent_pid = parts[6]
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| api_error!("invalid_reference", "invalid agent_ref process PID"))?;
    if parts[7].len() != 64
        || !parts[7]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(
            api_error!("invalid_reference", "invalid agent_ref process start token").into(),
        );
    }
    Ok(AgentIdentity {
        pane_instance,
        state_id: parts[4].to_string(),
        agent_epoch,
        agent: String::new(),
        agent_process: AgentProcessRef {
            pid: agent_pid,
            start_token_hash: parts[7].to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::contract::ApiError;
    use crate::api::pane::parse_pane_ref;
    use crate::api::test_support::*;

    #[test]
    fn prompt_input_contract_accepts_lf_and_rejects_unsafe_controls() {
        validate_prompt("review\nthis").unwrap();
        validate_prompt(&"x".repeat(MAX_PROMPT_BYTES)).unwrap();

        for invalid in [
            "",
            "tab\there",
            "cr\rhere",
            "nul\0here",
            "esc\u{1b}here",
            "c1\u{85}here",
        ] {
            assert!(validate_prompt(invalid).is_err(), "{invalid:?}");
        }
        assert!(validate_prompt(&"x".repeat(MAX_PROMPT_BYTES + 1)).is_err());
    }

    #[test]
    fn references_pin_server_pane_process_and_agent_epoch() {
        let pane = PaneInstance {
            pane_id: "%456".to_string(),
            pane_pid: 1234,
        };
        let encoded = pane_ref("server", &pane);
        assert_eq!(encoded, "vtp1:server:456:1234");
        assert_eq!(parse_pane_ref(&encoded, "server").unwrap(), pane);
        assert_eq!(
            parse_pane_ref(&encoded, "other").unwrap_err().to_string(),
            "pane_ref belongs to another tmux server"
        );

        let agent_pane = test_agent_pane();
        let encoded = agent_ref("server", &agent_pane);
        assert!(!encoded.contains("test-process-start"));
        let identity = parse_agent_ref(&encoded, "server").unwrap();
        assert_eq!(identity.pane_instance, agent_pane.pane_instance);
        assert_eq!(identity.state_id, "00112233445566778899aabbccddeeff");
        assert_eq!(identity.agent_epoch, 1);
        assert_eq!(identity.agent_process.pid, 9001);
        assert_eq!(identity.agent_process.start_token_hash.len(), 64);
    }

    #[test]
    fn exact_agent_operations_reject_missing_process_identity() {
        let mut pane = test_agent_pane();
        pane.agent_process = None;

        let error = AgentIdentity::from_pane(&pane).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "exact_identity_unavailable"
        );
    }

    #[test]
    fn ongoing_wait_tolerates_unverifiable_identity_but_rejects_replacement() {
        let mut pane = test_agent_pane();
        let identity = AgentIdentity::from_pane(&pane).unwrap();
        pane.agent_process = None;
        reject_replaced_agent_process(&pane, &identity).unwrap();
        let snapshot = test_snapshot(pane.clone());
        let error = require_same_agent(&snapshot, &identity).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "exact_identity_unavailable"
        );

        pane.agent_process = Some(crate::pane_state::AgentProcessIdentity {
            pid: 9002,
            start_token: "replacement-process-start".to_string(),
        });
        let error = reject_replaced_agent_process(&pane, &identity).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "stale_reference"
        );
    }

    #[test]
    fn wait_start_allows_process_exit_but_rejects_a_live_replacement() {
        let pane = test_agent_pane();
        let identity = AgentIdentity::from_pane(&pane).unwrap();
        let runner = crate::tmux::mock::MockTmuxRunner::new();
        runner.stub_agent_process(101, "codex", None);
        reject_live_agent_process_replacement(&runner, &identity, &pane).unwrap();

        runner.stub_agent_process(
            101,
            "codex",
            Some(crate::pane_state::AgentProcessIdentity {
                pid: 9002,
                start_token: "replacement-process-start".to_string(),
            }),
        );
        let error = reject_live_agent_process_replacement(&runner, &identity, &pane).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().code(),
            "stale_reference"
        );
    }
}

//! Local process ownership proof shared by Codex lifecycle and Question ingress.
//! An inherited TMUX_PANE in a shared app-server is not a TUI routing contract.
use std::collections::BTreeMap;

use crate::pane_state::{AgentProcessIdentity, PaneInstance};
use crate::tmux::TmuxRunner;

#[derive(Debug, Default)]
pub struct CodexHookOwner {
    pub ancestors: Vec<AgentProcessIdentity>,
    pub non_embedded: bool,
    pub pane: Option<PaneInstance>,
    pub process: Option<AgentProcessIdentity>,
}

impl CodexHookOwner {
    pub fn capture(runner: &dyn TmuxRunner, env: &BTreeMap<String, String>) -> Self {
        if !env.get("TMUX").is_some_and(|value| !value.is_empty())
            || !env.get("TMUX_PANE").is_some_and(|value| !value.is_empty())
        {
            return Self::default();
        }
        let ancestors = crate::question_notice::ingress::capture_ancestors().unwrap_or_default();
        let pane = crate::hook::writer::resolve_pane_instance(runner, env)
            .ok()
            .flatten();
        // Stop at the pane root. Unrelated sibling/child Codex processes cannot
        // invalidate the hook's own process lineage. A detached server has no
        // such root in its chain and cannot borrow its original TMUX_PANE.
        let root = pane
            .as_ref()
            .and_then(|pane| ancestors.iter().position(|p| p.pid == pane.pane_pid));
        let local = &ancestors[..root.map_or(ancestors.len(), |index| index + 1)];
        let arguments: Option<Vec<_>> = local
            .iter()
            .map(|p| {
                let args = crate::question_notice::profile::process_arguments(p.pid)?;
                (crate::daemon::lifecycle::agent_process_start_token(p.pid)
                    .ok()
                    .as_deref()
                    == Some(p.start_token.as_str()))
                .then_some(args)
            })
            .collect();
        let non_embedded = arguments
            .as_ref()
            .is_some_and(|commands| commands.iter().any(|args| non_embedded_ancestor(args)));
        let process = arguments
            .as_ref()
            .filter(|_| !non_embedded && root.is_some())
            .and_then(|commands| {
                commands
                    .iter()
                    .position(|args| crate::daemon::workers::is_codex_command(args))
            })
            .map(|index| local[index].clone());
        Self {
            ancestors,
            non_embedded,
            pane,
            process,
        }
    }

    pub fn verified(&self) -> bool {
        !self.non_embedded
            && self.pane.as_ref().is_some_and(|pane| {
                let Some(root) = self.ancestors.iter().position(|p| p.pid == pane.pane_pid) else {
                    return false;
                };
                self.process
                    .as_ref()
                    .is_some_and(|process| self.ancestors[..=root].contains(process))
            })
    }

    /// Revalidate the bound lineage immediately before each delivery attempt.
    /// No uniqueness scan: a concurrent --version or MCP subprocess is unrelated.
    pub fn still_owned(&self, pane: &PaneInstance) -> bool {
        self.verified()
            && self.pane.as_ref() == Some(pane)
            && self
                .ancestors
                .iter()
                .take_while(|p| p.pid != pane.pane_pid)
                .chain(self.ancestors.iter().find(|p| p.pid == pane.pane_pid))
                .all(|process| {
                    crate::daemon::lifecycle::agent_process_start_token(process.pid)
                        .ok()
                        .as_deref()
                        == Some(process.start_token.as_str())
                })
    }
}

fn non_embedded_ancestor(args: &[String]) -> bool {
    // Parse the first positional argument after options, never words in a
    // prompt or option value. Exe names may be changed by package managers.
    // Interpreted launchers put the provider script before its arguments.
    let program = args
        .first()
        .and_then(|arg| std::path::Path::new(arg).file_name())
        .and_then(|name| name.to_str());
    let interpreted = program
        .is_some_and(|program| crate::daemon::workers::INTERPRETER_PROGRAMS.contains(&program));
    let prefix_len = if interpreted { 2 } else { 1 };
    let codex_program = args
        .get(..prefix_len)
        .is_some_and(crate::daemon::workers::is_codex_command);
    let args = if interpreted { &args[1..] } else { args };
    if codex_program {
        crate::question_notice::profile::non_embedded_arguments(args)
    } else {
        crate::question_notice::profile::app_server_arguments(args)
    }
}

pub fn record_rejection(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    reason: &'static str,
) {
    if env.get("TMUX").is_some_and(|value| !value.is_empty())
        && let Ok(server) = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)
    {
        let _ = crate::daemon::lifecycle::append_daemon_log(env, &server.hash, reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_mode_is_not_inferred_from_prompt_or_config_values() {
        for args in [
            vec!["make", "review"],
            vec!["pnpm", "exec", "codex"],
            vec!["codex", "fix", "the", "app-server", "startup"],
            vec!["codex", "--model", "app-server"],
            vec!["codex", "--", "app-server"],
            vec!["node", "/provider/codex.js", "fix", "app-server"],
        ] {
            assert!(!non_embedded_ancestor(
                &args.into_iter().map(String::from).collect::<Vec<_>>()
            ));
        }
        for args in [
            vec!["codex-managed", "--model", "synthetic", "app-server"],
            vec!["node", "/provider/codex.js", "app-server"],
            vec!["codex", "mcp-server"],
            vec!["codex", "--remote=synthetic"],
        ] {
            assert!(non_embedded_ancestor(
                &args.into_iter().map(String::from).collect::<Vec<_>>()
            ));
        }
    }

    #[test]
    fn final_delivery_guard_rechecks_pane_and_native_start_token() {
        let process = AgentProcessIdentity {
            pid: std::process::id(),
            start_token: crate::daemon::lifecycle::agent_process_start_token(std::process::id())
                .unwrap(),
        };
        let pane = PaneInstance {
            pane_id: "%1".into(),
            pane_pid: process.pid,
        };
        let mut owner = CodexHookOwner {
            ancestors: vec![process.clone()],
            non_embedded: false,
            pane: Some(pane.clone()),
            process: Some(process),
        };
        assert!(owner.still_owned(&pane));
        assert!(!owner.still_owned(&PaneInstance {
            pane_id: "%1".into(),
            pane_pid: 1
        }));
        owner.ancestors[0].start_token = "replaced".into();
        owner.process = Some(owner.ancestors[0].clone());
        assert!(!owner.still_owned(&pane));
    }

    #[test]
    fn shared_or_detached_server_cannot_borrow_its_launching_pane() {
        let tui = AgentProcessIdentity {
            pid: 10,
            start_token: "tui".into(),
        };
        let server = AgentProcessIdentity {
            pid: 20,
            start_token: "server".into(),
        };
        let pane = PaneInstance {
            pane_id: "%1".into(),
            pane_pid: 10,
        };
        let mut owner = CodexHookOwner {
            ancestors: vec![server.clone(), tui.clone()],
            non_embedded: true,
            pane: Some(pane),
            process: Some(tui.clone()),
        };
        assert!(!owner.verified());
        owner.non_embedded = false;
        owner.ancestors = vec![server];
        assert!(!owner.verified());
        owner.ancestors = vec![tui];
        assert!(owner.verified());
        owner.process.as_mut().unwrap().start_token = "replacement".into();
        assert!(!owner.verified());
        assert!(!CodexHookOwner::default().verified());
    }
}

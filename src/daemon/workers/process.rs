use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::pane_state::AgentKind;
use crate::tmux::run_command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessDetection {
    pub agents: BTreeSet<AgentKind>,
    pub agent_processes: BTreeMap<AgentKind, BTreeSet<crate::pane_state::AgentProcessIdentity>>,
    pub complete: bool,
    pub process_identities_complete: bool,
}

impl ProcessDetection {
    pub fn exact_agent_process(
        &self,
        agent: &AgentKind,
    ) -> Option<crate::pane_state::AgentProcessIdentity> {
        if !self.complete || !self.process_identities_complete {
            return None;
        }
        let processes = self.agent_processes.get(agent)?;
        (processes.len() == 1)
            .then(|| processes.iter().next().cloned())
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentProcessSnapshot {
    commands: BTreeMap<u32, String>,
    children: BTreeMap<u32, Vec<u32>>,
    process_groups: BTreeMap<u32, (i32, i32)>,
    listening_ports: BTreeMap<u32, BTreeSet<u16>>,
    complete: bool,
    ports_complete: bool,
}

impl AgentProcessSnapshot {
    pub fn parse(output: &str, command_succeeded: bool) -> Self {
        let mut snapshot = Self {
            complete: command_succeeded,
            ..Self::default()
        };
        if !command_succeeded {
            return snapshot;
        }
        for line in output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let mut fields = line.split_whitespace();
            let Some(pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
                snapshot.complete = false;
                continue;
            };
            let Some(ppid) = fields
                .next()
                .and_then(|value| value.trim().parse::<u32>().ok())
            else {
                snapshot.complete = false;
                continue;
            };
            let Some(process_group) = fields.next().and_then(|value| value.parse::<i32>().ok())
            else {
                snapshot.complete = false;
                continue;
            };
            let Some(terminal_process_group) =
                fields.next().and_then(|value| value.parse::<i32>().ok())
            else {
                snapshot.complete = false;
                continue;
            };
            let command = fields.collect::<Vec<_>>().join(" ");
            if command.is_empty() {
                snapshot.complete = false;
                continue;
            }
            if snapshot.commands.insert(pid, command).is_some() {
                snapshot.complete = false;
            }
            if snapshot
                .process_groups
                .insert(pid, (process_group, terminal_process_group))
                .is_some()
            {
                snapshot.complete = false;
            }
            snapshot.children.entry(ppid).or_default().push(pid);
        }
        snapshot
    }

    pub fn detect_from_pid_tree(&self, root_pid: u32) -> ProcessDetection {
        if !self.complete || !self.commands.contains_key(&root_pid) {
            return ProcessDetection {
                agents: BTreeSet::new(),
                agent_processes: BTreeMap::new(),
                complete: false,
                process_identities_complete: false,
            };
        }
        let mut agents = BTreeSet::new();
        let mut direct_agent_processes =
            BTreeMap::<AgentKind, BTreeSet<crate::pane_state::AgentProcessIdentity>>::new();
        let mut interpreted_agent_processes =
            BTreeMap::<AgentKind, BTreeSet<crate::pane_state::AgentProcessIdentity>>::new();
        let mut direct_agents = BTreeSet::new();
        let mut process_identities_complete = true;
        let mut stack = vec![root_pid];
        let mut visited = BTreeSet::new();
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            if let Some(command) = self.commands.get(&pid)
                && let Some(detected) = detect_process_agent(command)
            {
                let agent = detected.agent;
                agents.insert(agent.clone());
                if detected.source == AgentProcessSource::Direct {
                    direct_agents.insert(agent.clone());
                }
                match crate::daemon::lifecycle::agent_process_start_token(pid) {
                    Ok(start_token) => {
                        let processes = match detected.source {
                            AgentProcessSource::Direct => &mut direct_agent_processes,
                            AgentProcessSource::Interpreted => &mut interpreted_agent_processes,
                        };
                        processes
                            .entry(agent)
                            .or_default()
                            .insert(crate::pane_state::AgentProcessIdentity { pid, start_token });
                    }
                    Err(_) => process_identities_complete = false,
                }
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        let agent_processes = prefer_direct_agent_processes(
            &agents,
            &direct_agents,
            direct_agent_processes,
            interpreted_agent_processes,
        );
        ProcessDetection {
            agents,
            agent_processes,
            complete: true,
            process_identities_complete,
        }
    }

    pub fn contains_nvim_process(&self, root_pid: u32, process_pid: u32) -> Option<bool> {
        let (_, root_terminal_process_group) = self.process_groups.get(&root_pid).copied()?;
        if !self.complete || root_terminal_process_group <= 0 {
            return None;
        }
        let mut stack = vec![root_pid];
        let mut visited = BTreeSet::new();
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            if pid == process_pid {
                let is_foreground = self.process_groups.get(&pid).is_some_and(
                    |(process_group, terminal_process_group)| {
                        *process_group == root_terminal_process_group
                            && *terminal_process_group == root_terminal_process_group
                    },
                );
                return Some(
                    is_foreground
                        && self
                            .commands
                            .get(&pid)
                            .is_some_and(|command| is_nvim_process(command)),
                );
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        Some(false)
    }

    pub fn is_foreground_process_owner(&self, root_pid: u32, process_pid: u32) -> Option<bool> {
        let (_, root_terminal_process_group) = self.process_groups.get(&root_pid).copied()?;
        if !self.complete || root_terminal_process_group <= 1 {
            return None;
        }
        let descendants = self.descendants(root_pid)?;
        if !descendants.contains(&process_pid) {
            return Some(false);
        }
        Some(self.process_groups.get(&process_pid).is_some_and(
            |(process_group, terminal_process_group)| {
                *process_group == root_terminal_process_group
                    && *terminal_process_group == root_terminal_process_group
            },
        ))
    }

    pub fn process_observation(
        &self,
        root_pid: u32,
        background_command: Option<&str>,
        agent_process_checked: bool,
        agent_process: Option<crate::pane_state::AgentProcessIdentity>,
    ) -> Option<crate::pane_state::ProcessObservation> {
        let descendants = self.descendants(root_pid)?;
        let background_process_alive = background_command.map(|command| {
            descendants.iter().any(|pid| {
                self.commands
                    .get(pid)
                    .is_some_and(|line| process_line_matches_command(line, command))
            })
        });
        let listening_ports = self.ports_complete.then(|| {
            descendants
                .iter()
                .filter_map(|pid| self.listening_ports.get(pid))
                .flatten()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .take(crate::pane_state::MAX_LISTENING_PORTS)
                .collect::<Vec<_>>()
        });
        (agent_process_checked
            || agent_process.is_some()
            || background_process_alive.is_some()
            || listening_ports.is_some())
        .then_some(crate::pane_state::ProcessObservation {
            agent_process_checked,
            agent_process,
            background_process_alive,
            listening_ports,
        })
    }

    fn descendants(&self, root_pid: u32) -> Option<BTreeSet<u32>> {
        if !self.complete || !self.commands.contains_key(&root_pid) {
            return None;
        }
        let mut descendants = BTreeSet::new();
        let mut stack = vec![root_pid];
        while let Some(pid) = stack.pop() {
            if !descendants.insert(pid) {
                continue;
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        Some(descendants)
    }

    fn observe_listening_ports(&mut self, output: &str) {
        self.listening_ports.clear();
        let mut current_pid = None;
        for line in output.lines() {
            if let Some(pid) = line.strip_prefix('p') {
                current_pid = pid.parse::<u32>().ok();
                continue;
            }
            if let Some(name) = line.strip_prefix('n')
                && let Some(pid) = current_pid
                && let Some(port) = listening_port_from_name(name)
            {
                self.listening_ports.entry(pid).or_default().insert(port);
            }
        }
        self.ports_complete = true;
    }
}

fn prefer_direct_agent_processes(
    agents: &BTreeSet<AgentKind>,
    direct_agents: &BTreeSet<AgentKind>,
    mut direct: BTreeMap<AgentKind, BTreeSet<crate::pane_state::AgentProcessIdentity>>,
    mut interpreted: BTreeMap<AgentKind, BTreeSet<crate::pane_state::AgentProcessIdentity>>,
) -> BTreeMap<AgentKind, BTreeSet<crate::pane_state::AgentProcessIdentity>> {
    agents
        .iter()
        .filter_map(|agent| {
            let candidates = if direct_agents.contains(agent) {
                direct.remove(agent)
            } else {
                interpreted.remove(agent)
            };
            candidates.map(|candidates| (agent.clone(), candidates))
        })
        .collect()
}

fn listening_port_from_name(name: &str) -> Option<u16> {
    let (_, tail) = name.trim().rsplit_once(':')?;
    let digits = tail
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn process_line_matches_command(line: &str, command: &str) -> bool {
    let line = normalize_process_text(line);
    let command = normalize_process_text(command);
    if command.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    line.match_indices(&command).any(|(index, _)| {
        let end = index + command.len();
        let before = index == 0 || !is_process_token_byte(bytes[index - 1]);
        let after = end == bytes.len() || !is_process_token_byte(bytes[end]);
        before && after
    })
}

fn normalize_process_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_process_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn is_nvim_process(command: &str) -> bool {
    let Some(executable) = command.split_whitespace().next() else {
        return false;
    };
    matches!(
        executable.rsplit('/').next().unwrap_or(executable),
        "gview"
            | "gvim"
            | "view"
            | "vim"
            | "vimdiff"
            | "vi"
            | "nvi"
            | "nvim"
            | "nvimdiff"
            | "vimx"
            | "nvimx"
            | "nvimxdiff"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentProcessSource {
    Direct,
    Interpreted,
}

struct DetectedProcessAgent {
    agent: AgentKind,
    source: AgentProcessSource,
}

fn detect_process_agent(command: &str) -> Option<DetectedProcessAgent> {
    let mut fields = command.split_whitespace();
    let executable = fields.next()?.rsplit('/').next()?;
    if matches!(executable, "claude" | "codex" | "opencode") {
        // Codex may spawn ChatGPT's non-interactive app server as a descendant.
        // It is not a pane occupant and must not make the interactive Codex
        // process identity ambiguous.
        if executable == "codex" && fields.next() == Some("app-server") {
            return None;
        }
        return Some(DetectedProcessAgent {
            agent: AgentKind::parse(executable).ok()?,
            source: AgentProcessSource::Direct,
        });
    }
    let executable = executable.to_ascii_lowercase();
    let interpreted = if matches!(
        executable.as_str(),
        "node" | "bun" | "deno" | "python" | "python3"
    ) {
        let script = fields.next()?;
        let agent = script
            .split(['/', '\\'])
            .map(str::to_ascii_lowercase)
            .find_map(|component| match component.as_str() {
                "claude" | "claude-code" => Some("claude"),
                "codex" | "codex-cli" => Some("codex"),
                "opencode" => Some("opencode"),
                _ => None,
            })?;
        if agent == "codex" && fields.next() == Some("app-server") {
            return None;
        }
        Some(agent)
    } else {
        None
    };
    Some(DetectedProcessAgent {
        agent: AgentKind::parse(interpreted?).ok()?,
        source: AgentProcessSource::Interpreted,
    })
}

pub fn read_agent_process_snapshot(timeout: Duration, scan_ports: bool) -> AgentProcessSnapshot {
    let mut snapshot = match run_command(
        "ps",
        &["-ax", "-o", "pid=,ppid=,pgid=,tpgid=,command="],
        Some(timeout),
    ) {
        Ok(output) => AgentProcessSnapshot::parse(&output, true),
        Err(_) => AgentProcessSnapshot::parse("", false),
    };
    if scan_ports
        && let Ok(output) = run_command(
            "sh",
            &[
                "-c",
                "lsof -iTCP -sTCP:LISTEN -nP -F pn; code=$?; [ \"$code\" -eq 0 ] || [ \"$code\" -eq 1 ]",
            ],
            Some(timeout),
        )
    {
        snapshot.observe_listening_ports(&output);
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvim_marker_requires_the_exact_process_inside_the_pane_tree() {
        let snapshot = AgentProcessSnapshot::parse(
            "100 1 100 200 -zsh\n200 100 200 200 node editprompt\n300 200 200 200 /opt/bin/nvim prompt.md\n400 100 400 400 node codex\n500 400 500 0 nvim +Man!\n",
            true,
        );
        assert_eq!(snapshot.contains_nvim_process(100, 300), Some(true));
        assert_eq!(snapshot.contains_nvim_process(100, 400), Some(false));
        assert_eq!(snapshot.contains_nvim_process(100, 500), Some(false));
        assert_eq!(snapshot.contains_nvim_process(100, 999), Some(false));
        assert_eq!(snapshot.contains_nvim_process(999, 300), None);
        assert_eq!(
            AgentProcessSnapshot::parse("", false).contains_nvim_process(100, 300),
            None
        );
    }

    #[test]
    fn foreground_process_owner_requires_the_pane_tpgid_on_both_agent_fields() {
        let snapshot = AgentProcessSnapshot::parse(
            "100 1 100 200 -zsh\n\
             200 100 200 200 codex\n\
             201 100 201 200 claude\n\
             202 100 200 202 opencode\n\
             203 1 200 200 codex\n",
            true,
        );

        assert_eq!(snapshot.is_foreground_process_owner(100, 200), Some(true));
        assert_eq!(snapshot.is_foreground_process_owner(100, 201), Some(false));
        assert_eq!(snapshot.is_foreground_process_owner(100, 202), Some(false));
        assert_eq!(snapshot.is_foreground_process_owner(100, 203), Some(false));
        assert_eq!(snapshot.is_foreground_process_owner(100, 999), Some(false));

        let no_foreground = AgentProcessSnapshot::parse("100 1 100 1 -zsh\n", true);
        assert_eq!(no_foreground.is_foreground_process_owner(100, 100), None);
        let no_terminal = AgentProcessSnapshot::parse("100 1 100 0 -zsh\n", true);
        assert_eq!(no_terminal.is_foreground_process_owner(100, 100), None);
        assert_eq!(snapshot.is_foreground_process_owner(999, 200), None);
        assert_eq!(
            AgentProcessSnapshot::parse("", false).is_foreground_process_owner(100, 200),
            None
        );
    }

    #[test]
    fn process_snapshot_collects_all_agent_kinds_and_marks_malformed_input_incomplete() {
        let snapshot = AgentProcessSnapshot::parse(
            "   10     1 10 10 zsh\n   11    10 11 11 codex\n   12    10 12 12 /usr/bin/claude --resume\n   13    12 13 13 opencode\n   14    10 14 14 rg codex\n",
            true,
        );
        let detection = snapshot.detect_from_pid_tree(10);
        assert!(detection.complete);
        assert_eq!(
            detection
                .agents
                .iter()
                .map(AgentKind::as_str)
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "opencode"]
        );

        let malformed = AgentProcessSnapshot::parse("10 1 10 10 zsh\nbroken\n", true);
        assert!(!malformed.detect_from_pid_tree(10).complete);
        assert!(
            !AgentProcessSnapshot::parse("", false)
                .detect_from_pid_tree(10)
                .complete
        );
    }

    #[test]
    fn exact_agent_process_requires_one_unique_identity() {
        let codex = AgentKind::parse("codex").unwrap();
        let first = crate::pane_state::AgentProcessIdentity {
            pid: 10,
            start_token: "first".to_string(),
        };
        let second = crate::pane_state::AgentProcessIdentity {
            pid: 11,
            start_token: "second".to_string(),
        };
        let mut detection = ProcessDetection {
            agents: BTreeSet::from([codex.clone()]),
            agent_processes: BTreeMap::from([(codex.clone(), BTreeSet::from([first.clone()]))]),
            complete: true,
            process_identities_complete: true,
        };

        assert_eq!(detection.exact_agent_process(&codex), Some(first));
        detection
            .agent_processes
            .get_mut(&codex)
            .unwrap()
            .insert(second);
        assert_eq!(detection.exact_agent_process(&codex), None);
    }

    #[test]
    fn direct_agent_binary_wins_over_its_interpreted_launcher() {
        let codex = AgentKind::parse("codex").unwrap();
        let launcher = crate::pane_state::AgentProcessIdentity {
            pid: 10,
            start_token: "launcher".to_string(),
        };
        let native = crate::pane_state::AgentProcessIdentity {
            pid: 11,
            start_token: "native".to_string(),
        };
        let processes = prefer_direct_agent_processes(
            &BTreeSet::from([codex.clone()]),
            &BTreeSet::from([codex.clone()]),
            BTreeMap::from([(codex.clone(), BTreeSet::from([native.clone()]))]),
            BTreeMap::from([(codex.clone(), BTreeSet::from([launcher]))]),
        );

        assert_eq!(processes[&codex], BTreeSet::from([native]));
    }

    #[test]
    fn process_agent_detection_distinguishes_native_binary_from_launcher() {
        let launcher =
            detect_process_agent("node /opt/node_modules/@openai/codex/bin/codex.js --yolo")
                .unwrap();
        let native =
            detect_process_agent("/opt/vendor/aarch64-apple-darwin/bin/codex --yolo").unwrap();

        assert_eq!(launcher.agent.as_str(), "codex");
        assert_eq!(launcher.source, AgentProcessSource::Interpreted);
        assert_eq!(native.agent.as_str(), "codex");
        assert_eq!(native.source, AgentProcessSource::Direct);
        assert!(
            detect_process_agent(
                "/Applications/ChatGPT.app/Contents/Resources/codex app-server --listen stdio://"
            )
            .is_none()
        );
        assert!(
            detect_process_agent(
                "node /opt/node_modules/@openai/codex/bin/codex.js app-server --listen stdio://"
            )
            .is_none()
        );
        assert!(
            detect_process_agent(
                "/Users/example/.codex/computer-use/Codex Computer Use.app/Contents/MacOS/client"
            )
            .is_none()
        );
    }

    #[test]
    fn process_snapshot_maps_ports_and_background_liveness_to_pane_tree() {
        let mut snapshot = AgentProcessSnapshot::parse(
            "100 1 100 100 zsh\n200 100 200 100 bash -c pnpm dev\n300 200 300 100 node server.js\n",
            true,
        );
        snapshot.observe_listening_ports("p300\nn127.0.0.1:3000\nn*:5173\n");

        let observation = snapshot
            .process_observation(100, Some("pnpm dev"), false, None)
            .unwrap();
        assert_eq!(observation.background_process_alive, Some(true));
        assert_eq!(observation.listening_ports, Some(vec![3000, 5173]));
        assert_eq!(
            snapshot
                .process_observation(100, Some("cargo watch"), false, None)
                .unwrap()
                .background_process_alive,
            Some(false)
        );
    }
}

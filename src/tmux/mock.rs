use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use anyhow::{Result, bail};

use super::{InputCommandError, InputWriteStage, TmuxRunner};

type AgentProcessKey = (u32, String);
type StubAgentProcessResult =
    std::result::Result<Option<crate::pane_state::AgentProcessIdentity>, String>;

#[derive(Debug, Default)]
pub struct MockTmuxRunner {
    responses: RefCell<HashMap<Vec<String>, String>>,
    agent_processes:
        RefCell<HashMap<AgentProcessKey, Option<crate::pane_state::AgentProcessIdentity>>>,
    agent_process_sequences: RefCell<HashMap<AgentProcessKey, VecDeque<StubAgentProcessResult>>>,
    calls: RefCell<Vec<Vec<String>>>,
    input_calls: RefCell<Vec<(Vec<String>, Vec<u8>)>>,
    agent_input_owners: RefCell<HashMap<(u32, u32), std::result::Result<bool, String>>>,
}

impl MockTmuxRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stub(&self, args: &[&str], output: &str) {
        self.responses.borrow_mut().insert(
            args.iter().map(|s| s.to_string()).collect(),
            output.to_string(),
        );
    }

    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.borrow().clone()
    }

    pub fn input_calls(&self) -> Vec<(Vec<String>, Vec<u8>)> {
        self.input_calls.borrow().clone()
    }

    pub fn stub_agent_input_owner(&self, root_pid: u32, agent_pid: u32, is_owner: bool) {
        self.agent_input_owners
            .borrow_mut()
            .insert((root_pid, agent_pid), Ok(is_owner));
    }

    pub fn stub_agent_process(
        &self,
        root_pid: u32,
        agent: &str,
        identity: Option<crate::pane_state::AgentProcessIdentity>,
    ) {
        self.agent_processes
            .borrow_mut()
            .insert((root_pid, agent.to_string()), identity);
    }

    pub fn stub_agent_process_sequence(
        &self,
        root_pid: u32,
        agent: &str,
        identities: impl IntoIterator<Item = StubAgentProcessResult>,
    ) {
        self.agent_process_sequences.borrow_mut().insert(
            (root_pid, agent.to_string()),
            identities.into_iter().collect(),
        );
    }
}

impl TmuxRunner for MockTmuxRunner {
    fn run(&self, args: &[&str]) -> Result<String> {
        let key: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.calls.borrow_mut().push(key.clone());
        match self.responses.borrow().get(&key) {
            Some(output) => Ok(output.clone()),
            None => bail!("no stub registered for tmux {key:?}"),
        }
    }

    fn run_with_input(
        &self,
        args: &[&str],
        input: &[u8],
    ) -> std::result::Result<String, InputCommandError> {
        let key: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.calls.borrow_mut().push(key.clone());
        self.input_calls
            .borrow_mut()
            .push((key.clone(), input.to_vec()));
        match self.responses.borrow().get(&key) {
            Some(output) => Ok(output.clone()),
            None => Err(InputCommandError::new(
                InputWriteStage::BeforeSpawn,
                anyhow::anyhow!("no stub registered for tmux {key:?}"),
            )),
        }
    }

    fn verify_agent_input_owner(&self, root_pid: u32, agent_pid: u32) -> Result<()> {
        match self.agent_input_owners.borrow().get(&(root_pid, agent_pid)) {
            Some(Ok(true)) => Ok(()),
            Some(Ok(false)) => bail!(
                "agent process {agent_pid} is not the foreground input owner for pane root {root_pid}"
            ),
            Some(Err(error)) => bail!(error.clone()),
            None => bail!("no agent input owner stub registered for {root_pid}/{agent_pid}"),
        }
    }

    fn resolve_agent_process(
        &self,
        root_pid: u32,
        agent: &crate::pane_state::AgentKind,
    ) -> Result<Option<crate::pane_state::AgentProcessIdentity>> {
        let key = (root_pid, agent.as_str().to_string());
        if let Some(result) = self
            .agent_process_sequences
            .borrow_mut()
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
        {
            return result.map_err(anyhow::Error::msg);
        }
        self.agent_processes
            .borrow()
            .get(&key)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no agent process stub registered for {root_pid}/{}",
                    agent.as_str()
                )
            })
    }
}

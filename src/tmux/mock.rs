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
    input_errors: RefCell<HashMap<Vec<String>, (InputWriteStage, String)>>,
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

    pub fn stub_agent_input_owner_error(&self, root_pid: u32, agent_pid: u32, error: &str) {
        self.agent_input_owners
            .borrow_mut()
            .insert((root_pid, agent_pid), Err(error.to_string()));
    }

    pub fn stub_input_error(&self, args: &[&str], stage: InputWriteStage, error: &str) {
        self.input_errors.borrow_mut().insert(
            args.iter().map(|arg| (*arg).to_string()).collect(),
            (stage, error.to_string()),
        );
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
        if let Some((stage, error)) = self.input_errors.borrow().get(&key) {
            return Err(InputCommandError::new(
                *stage,
                anyhow::anyhow!(error.clone()),
            ));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubbed_args_return_output_and_record_calls() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["list-sessions"], "main\n");
        let out = mock.run(&["list-sessions"]).unwrap();
        assert_eq!(out, "main\n");
        assert_eq!(mock.calls(), vec![vec!["list-sessions".to_string()]]);
    }

    #[test]
    fn unstubbed_args_error() {
        let mock = MockTmuxRunner::new();
        let err = mock.run(&["kill-server"]).unwrap_err();
        assert!(err.to_string().contains("no stub registered"));
    }

    #[test]
    fn input_calls_record_payload_and_shared_call_order() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["first"], "one");
        mock.stub(&["load-buffer", "-"], "two");

        mock.run(&["first"]).unwrap();
        mock.run_with_input(&["load-buffer", "-"], b"private prompt")
            .unwrap();

        assert_eq!(
            mock.calls(),
            vec![
                vec!["first".to_string()],
                vec!["load-buffer".to_string(), "-".to_string()]
            ]
        );
        assert_eq!(
            mock.input_calls(),
            vec![(
                vec!["load-buffer".to_string(), "-".to_string()],
                b"private prompt".to_vec()
            )]
        );
    }

    #[test]
    fn agent_input_owner_can_be_stubbed_as_allowed_denied_or_error() {
        let mock = MockTmuxRunner::new();
        mock.stub_agent_input_owner(100, 200, true);
        mock.stub_agent_input_owner(100, 201, false);
        mock.stub_agent_input_owner_error(100, 202, "scan failed");

        mock.verify_agent_input_owner(100, 200).unwrap();
        assert!(mock.verify_agent_input_owner(100, 201).is_err());
        assert_eq!(
            mock.verify_agent_input_owner(100, 202)
                .unwrap_err()
                .to_string(),
            "scan failed"
        );
    }

    #[test]
    fn input_error_stage_can_be_stubbed() {
        let mock = MockTmuxRunner::new();
        mock.stub_input_error(
            &["load-buffer", "-"],
            InputWriteStage::AfterFullWrite,
            "tmux exited",
        );

        let error = mock
            .run_with_input(&["load-buffer", "-"], b"prompt")
            .unwrap_err();
        assert_eq!(error.stage, InputWriteStage::AfterFullWrite);
        assert_eq!(error.to_string(), "tmux exited");
    }
}

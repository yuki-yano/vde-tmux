use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Result, bail};

use super::TmuxRunner;

#[derive(Debug, Default)]
pub struct MockTmuxRunner {
    responses: RefCell<HashMap<Vec<String>, String>>,
    calls: RefCell<Vec<Vec<String>>>,
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
}

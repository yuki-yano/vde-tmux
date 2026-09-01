use std::time::{Duration, Instant};

use anyhow::Result;

use crate::daemon::topology::ServerIdentity;
use crate::pane_state::{EventId, PaneInstance};
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

const SIDEBAR_SERVER_MISMATCH_SENTINEL: &str = "__vde_sidebar_server_mismatch__";
const SIDEBAR_PANE_MISMATCH_SENTINEL: &str = "__vde_sidebar_pane_mismatch__";
const SIDEBAR_JOB_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum SidebarTmuxError {
    ServerIncarnationMismatch,
    PaneInstanceMismatch(String),
    NoAvailablePane,
    SourceClientMismatch,
    Command(anyhow::Error),
}

impl std::fmt::Display for SidebarTmuxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerIncarnationMismatch => write!(formatter, "tmux server incarnation changed"),
            Self::PaneInstanceMismatch(pane_id) => {
                write!(formatter, "pane instance changed: {pane_id}")
            }
            Self::NoAvailablePane => write!(formatter, "no unread pane is still available"),
            Self::SourceClientMismatch => {
                write!(
                    formatter,
                    "source sidebar is no longer focused by the tmux client"
                )
            }
            Self::Command(error) => write!(formatter, "tmux command failed: {error:#}"),
        }
    }
}

impl std::error::Error for SidebarTmuxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Command(error) => Some(error.as_ref()),
            Self::ServerIncarnationMismatch
            | Self::PaneInstanceMismatch(_)
            | Self::NoAvailablePane
            | Self::SourceClientMismatch => None,
        }
    }
}

#[derive(Debug)]
enum SidebarGuardError {
    ServerIncarnationMismatch,
    PaneInstanceMismatch,
}

impl std::fmt::Display for SidebarGuardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerIncarnationMismatch => write!(formatter, "tmux server incarnation changed"),
            Self::PaneInstanceMismatch => write!(formatter, "pane instance changed"),
        }
    }
}

impl std::error::Error for SidebarGuardError {}

/// Applies the daemon's server-incarnation and selected-pane fences to every tmux operation made
/// by the sidebar FIFO worker. Public, direct sidebar commands intentionally keep using their
/// unguarded runner; only daemon-owned execution is wrapped here.
struct GuardedSidebarTmuxRunner<'a> {
    runner: &'a dyn TmuxRunner,
    expected_server: &'a ServerIdentity,
    expected_pane: &'a PaneInstance,
}

impl GuardedSidebarTmuxRunner<'_> {
    fn is_read(args: &[&str]) -> bool {
        matches!(
            args.first().copied(),
            Some("display-message" | "list-panes" | "list-clients")
        )
    }

    fn guarded_mutation_args(&self, args: &[&str]) -> Vec<String> {
        let command = crate::pane_state::store::tmux_command_string(
            &args
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>(),
        );
        let pane_guard = format!("#{{==:#{{pane_pid}},{}}}", self.expected_pane.pane_pid);
        let pane_command = crate::pane_state::store::tmux_command_string(&[
            "if-shell".to_string(),
            "-F".to_string(),
            "-t".to_string(),
            self.expected_pane.pane_id.clone(),
            pane_guard,
            command,
            format!("display-message -p '{SIDEBAR_PANE_MISMATCH_SENTINEL}'"),
        ]);
        crate::pane_state::store::server_guarded_command_args(
            self.expected_server.pid,
            self.expected_server.start_time,
            pane_command,
            SIDEBAR_SERVER_MISMATCH_SENTINEL,
        )
    }

    fn guarded_read_args(&self, args: &[&str], token: &str) -> Vec<String> {
        let identity = format!("__vde_sidebar_identity_{token}__#{{pid}}:#{{start_time}}");
        let mut guarded = vec![
            "display-message".to_string(),
            "-p".to_string(),
            identity,
            ";".to_string(),
        ];
        guarded.extend(args.iter().map(|arg| (*arg).to_string()));
        guarded
    }
}

impl TmuxRunner for GuardedSidebarTmuxRunner<'_> {
    fn run(&self, args: &[&str]) -> Result<String> {
        if !Self::is_read(args) {
            let guarded = self.guarded_mutation_args(args);
            let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
            let output = self.runner.run(&refs).map_err(|error| {
                if is_missing_pane_error(&error) {
                    anyhow::Error::new(SidebarGuardError::PaneInstanceMismatch)
                } else {
                    error
                }
            })?;
            if output
                .lines()
                .any(|line| line.trim() == SIDEBAR_SERVER_MISMATCH_SENTINEL)
            {
                return Err(SidebarGuardError::ServerIncarnationMismatch.into());
            }
            if output
                .lines()
                .any(|line| line.trim() == SIDEBAR_PANE_MISMATCH_SENTINEL)
            {
                return Err(SidebarGuardError::PaneInstanceMismatch.into());
            }
            return Ok(output);
        }

        let token = EventId::generate()?.as_str().to_string();
        let guarded = self.guarded_read_args(args, &token);
        let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
        let output = self.runner.run(&refs)?;
        let (identity, body) = output.split_once('\n').ok_or_else(|| {
            anyhow::anyhow!("sidebar tmux read did not return an identity envelope")
        })?;
        let expected = format!(
            "__vde_sidebar_identity_{token}__{}:{}",
            self.expected_server.pid, self.expected_server.start_time
        );
        if identity != expected {
            return Err(SidebarGuardError::ServerIncarnationMismatch.into());
        }
        Ok(body.to_string())
    }
}

fn classify_sidebar_error(error: anyhow::Error, pane: &PaneInstance) -> SidebarTmuxError {
    match error.downcast_ref::<SidebarGuardError>() {
        Some(SidebarGuardError::ServerIncarnationMismatch) => {
            SidebarTmuxError::ServerIncarnationMismatch
        }
        Some(SidebarGuardError::PaneInstanceMismatch) => {
            SidebarTmuxError::PaneInstanceMismatch(pane.pane_id.clone())
        }
        None if is_missing_pane_error(&error) => {
            SidebarTmuxError::PaneInstanceMismatch(pane.pane_id.clone())
        }
        None => SidebarTmuxError::Command(error),
    }
}

fn is_missing_pane_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("can't find pane")
        || message.contains("no such pane")
        || message.contains("pane not found")
}

pub trait WorkerIo: Send + Sync + 'static {
    fn jump_to_first_available_pane(
        &self,
        panes: &[PaneInstance],
        client_pid: u32,
        source_pane: &PaneInstance,
    ) -> std::result::Result<PaneInstance, SidebarTmuxError>;
}

trait TimedTmuxIo: Send + Sync {
    fn run_with_timeout(&self, args: &[&str], timeout: Duration) -> Result<String>;
}

#[derive(Debug, Clone)]
struct SystemTimedTmuxIo {
    socket_name: Option<String>,
}

impl TimedTmuxIo for SystemTimedTmuxIo {
    fn run_with_timeout(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let runner = self
            .socket_name
            .as_ref()
            .map(|name| SystemTmuxRunner::with_socket_name(name, Some(timeout)))
            .unwrap_or_else(|| SystemTmuxRunner::with_timeout(timeout));
        runner.run(args)
    }
}

struct JobBudgetTmuxRunner<'a> {
    io: &'a dyn TimedTmuxIo,
    deadline: Instant,
}

impl TmuxRunner for JobBudgetTmuxRunner<'_> {
    fn run(&self, args: &[&str]) -> Result<String> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| anyhow::anyhow!("sidebar tmux command exceeded its 2 second budget"))?;
        self.io.run_with_timeout(args, remaining)
    }
}

#[derive(Debug, Clone)]
pub struct SystemWorkerIo {
    io: SystemTimedTmuxIo,
    expected_server: ServerIdentity,
}

impl SystemWorkerIo {
    pub fn new(socket_name: Option<String>, expected_server: ServerIdentity) -> Self {
        Self {
            io: SystemTimedTmuxIo { socket_name },
            expected_server,
        }
    }
}

fn jump_to_first_available_pane_with_runner(
    runner: &dyn TmuxRunner,
    expected_server: &ServerIdentity,
    panes: &[PaneInstance],
    client_pid: u32,
    source_pane: &PaneInstance,
) -> std::result::Result<PaneInstance, SidebarTmuxError> {
    for pane in panes {
        let guarded = GuardedSidebarTmuxRunner {
            runner,
            expected_server,
            expected_pane: pane,
        };
        let result = crate::sidebar::layout::jump_to_pane_for_client(
            &guarded,
            pane,
            client_pid,
            source_pane,
        )
        .map_err(|error| {
            if error
                .to_string()
                .contains(crate::sidebar::layout::SOURCE_CLIENT_MISMATCH_SENTINEL)
            {
                SidebarTmuxError::SourceClientMismatch
            } else {
                classify_sidebar_error(error, pane)
            }
        });
        match result {
            Ok(()) => return Ok(pane.clone()),
            Err(SidebarTmuxError::PaneInstanceMismatch(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(SidebarTmuxError::NoAvailablePane)
}

impl WorkerIo for SystemWorkerIo {
    fn jump_to_first_available_pane(
        &self,
        panes: &[PaneInstance],
        client_pid: u32,
        source_pane: &PaneInstance,
    ) -> std::result::Result<PaneInstance, SidebarTmuxError> {
        let budgeted = JobBudgetTmuxRunner {
            io: &self.io,
            deadline: Instant::now() + SIDEBAR_JOB_TIMEOUT,
        };
        jump_to_first_available_pane_with_runner(
            &budgeted,
            &self.expected_server,
            panes,
            client_pid,
            source_pane,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use crate::daemon::workers::tests::{pane_instance, server_identity};

    struct SidebarGuardRunner {
        actual_server: ServerIdentity,
        read_body: String,
        client_read_body: Option<String>,
        mutation_output: String,
        mutation_error: Option<String>,
        stale_pane_pid: Option<u32>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl SidebarGuardRunner {
        fn new(actual_server: ServerIdentity, read_body: impl Into<String>) -> Self {
            Self {
                actual_server,
                read_body: read_body.into(),
                client_read_body: None,
                mutation_output: String::new(),
                mutation_error: None,
                stale_pane_pid: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_mutation_output(mut self, output: impl Into<String>) -> Self {
            self.mutation_output = output.into();
            self
        }

        fn with_client_read_body(mut self, output: impl Into<String>) -> Self {
            self.client_read_body = Some(output.into());
            self
        }

        fn with_mutation_error(mut self, error: impl Into<String>) -> Self {
            self.mutation_error = Some(error.into());
            self
        }

        fn with_stale_pane_pid(mut self, pane_pid: u32) -> Self {
            self.stale_pane_pid = Some(pane_pid);
            self
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TmuxRunner for SidebarGuardRunner {
        fn run(&self, args: &[&str]) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            if args.first() == Some(&"display-message") && args.get(3) == Some(&";") {
                let identity = args[2]
                    .replace("#{pid}", &self.actual_server.pid.to_string())
                    .replace("#{start_time}", &self.actual_server.start_time.to_string());
                let body = if args.contains(&"list-clients") {
                    self.client_read_body.as_ref().unwrap_or(&self.read_body)
                } else {
                    &self.read_body
                };
                return Ok(format!("{identity}\n{body}"));
            }
            if let Some(error) = &self.mutation_error {
                anyhow::bail!(error.clone());
            }
            if self.stale_pane_pid.is_some_and(|pane_pid| {
                args.iter()
                    .any(|arg| arg.contains(&format!("#{{pane_pid}},{pane_pid}")))
            }) {
                return Ok(format!("{SIDEBAR_PANE_MISMATCH_SENTINEL}\n"));
            }
            Ok(self.mutation_output.clone())
        }
    }

    #[derive(Default)]
    struct TimedTmuxRecorder {
        timeouts: Mutex<Vec<Duration>>,
    }

    impl TimedTmuxIo for TimedTmuxRecorder {
        fn run_with_timeout(&self, _args: &[&str], timeout: Duration) -> Result<String> {
            self.timeouts.lock().unwrap().push(timeout);
            Ok(String::new())
        }
    }

    #[test]
    fn sidebar_worker_wraps_atomic_jump_in_server_and_target_pane_guards() {
        let runner = SidebarGuardRunner::new(server_identity(), "$1\u{1f}@1\u{1f}%1\u{1f}11\n");
        let pane = pane_instance("%1", 11);
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &server_identity(),
            expected_pane: &pane,
        };

        crate::sidebar::layout::jump_to_pane(&guarded, "%1").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][0], "display-message");
        assert_eq!(calls[0][3], ";");
        assert_eq!(calls[1][0], "if-shell");
        assert!(calls[1][2].contains("#{pid},4242"), "{:?}", calls[1]);
        assert!(calls[1][2].contains("#{start_time},99"), "{:?}", calls[1]);
        assert!(calls[1][3].contains("#{pane_pid},11"), "{:?}", calls[1]);
        assert!(calls[1][3].contains("switch-client"));
        assert!(calls[1][3].contains("$1:@1.%1"));
        assert!(!calls[1][3].contains("select-window"));
        assert!(!calls[1][3].contains("select-pane"));
    }

    #[test]
    fn sidebar_worker_checks_target_and_source_instances_in_one_atomic_jump_mutation() {
        let runner = SidebarGuardRunner::new(server_identity(), "$1\u{1f}@1\u{1f}%1\u{1f}11\n")
            .with_client_read_body("20\u{1f}/dev/ttys002\n");
        let target = pane_instance("%1", 11);
        let source = pane_instance("%9", 909);
        let expected_server = server_identity();
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &expected_server,
            expected_pane: &target,
        };

        crate::sidebar::layout::jump_to_pane_for_client(&guarded, &target, 20, &source).unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0][0], "display-message");
        assert_eq!(calls[1][0], "display-message");
        assert_eq!(calls[2][0], "if-shell");
        let guarded_command = &calls[2][3];
        assert!(guarded_command.contains("#{pane_pid},11"), "{calls:?}");
        assert!(guarded_command.contains("#{pane_id},%9"), "{calls:?}");
        assert!(guarded_command.contains("#{pane_pid},909"), "{calls:?}");
        assert!(guarded_command.contains("switch-client"), "{calls:?}");
        assert!(guarded_command.contains("$1:@1.%1"), "{calls:?}");
        assert!(!guarded_command.contains("select-window"), "{calls:?}");
        assert!(!guarded_command.contains("select-pane"), "{calls:?}");
    }

    #[test]
    fn sidebar_worker_rejects_read_identity_mismatch_before_any_mutation() {
        let runner = SidebarGuardRunner::new(
            ServerIdentity {
                pid: 4243,
                start_time: 100,
            },
            "main\u{1f}@1\u{1f}%1\n",
        );
        let pane = pane_instance("%1", 11);
        let expected_server = server_identity();
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &expected_server,
            expected_pane: &pane,
        };

        let error = crate::sidebar::layout::jump_to_pane(&guarded, "%1").unwrap_err();

        assert!(matches!(
            error.downcast_ref::<SidebarGuardError>(),
            Some(SidebarGuardError::ServerIncarnationMismatch)
        ));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "display-message");
    }

    #[test]
    fn sidebar_worker_reports_server_and_pane_guard_mismatches_without_direct_mutation() {
        let pane = pane_instance("%1", 11);
        let expected_server = server_identity();
        for (output, expected_server_mismatch) in [
            (SIDEBAR_SERVER_MISMATCH_SENTINEL, true),
            (SIDEBAR_PANE_MISMATCH_SENTINEL, false),
        ] {
            let runner = SidebarGuardRunner::new(expected_server.clone(), "")
                .with_mutation_output(format!("{output}\n"));
            let guarded = GuardedSidebarTmuxRunner {
                runner: &runner,
                expected_server: &expected_server,
                expected_pane: &pane,
            };

            let error = guarded.run(&["select-pane", "-t", "%1"]).unwrap_err();

            assert_eq!(
                matches!(
                    error.downcast_ref::<SidebarGuardError>(),
                    Some(SidebarGuardError::ServerIncarnationMismatch)
                ),
                expected_server_mismatch
            );
            let calls = runner.calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0][0], "if-shell");
            assert_ne!(calls[0][0], "select-pane");
            assert!(calls[0][3].contains("select-pane"));
        }
    }

    #[test]
    fn sidebar_worker_treats_target_disappearance_as_pane_mismatch_without_retrying_raw_command() {
        let pane = pane_instance("%1", 11);
        let expected_server = server_identity();
        let runner = SidebarGuardRunner::new(expected_server.clone(), "")
            .with_mutation_error("tmux failed: can't find pane: %1");
        let guarded = GuardedSidebarTmuxRunner {
            runner: &runner,
            expected_server: &expected_server,
            expected_pane: &pane,
        };

        let error = guarded.run(&["select-pane", "-t", "%1"]).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<SidebarGuardError>(),
            Some(SidebarGuardError::PaneInstanceMismatch)
        ));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "if-shell");
        assert_ne!(calls[0][0], "select-pane");
    }

    #[test]
    fn unread_jump_skips_a_stale_target_and_uses_the_next_candidate() {
        let runner = SidebarGuardRunner::new(
            server_identity(),
            "$1\u{1f}@1\u{1f}%1\u{1f}11\n$1\u{1f}@2\u{1f}%2\u{1f}22\n",
        )
        .with_client_read_body("20\u{1f}/dev/ttys002\n")
        .with_stale_pane_pid(11);
        let source = pane_instance("%9", 909);
        let candidates = [pane_instance("%1", 11), pane_instance("%2", 22)];

        let selected = jump_to_first_available_pane_with_runner(
            &runner,
            &server_identity(),
            &candidates,
            20,
            &source,
        )
        .unwrap();

        assert_eq!(selected, pane_instance("%2", 22));
        let calls = runner.calls();
        assert_eq!(calls.len(), 6);
        assert!(calls[2][3].contains("#{pane_pid},11"));
        assert!(calls[5][3].contains("#{pane_pid},22"));
    }

    #[test]
    fn sidebar_job_uses_one_shared_deadline_across_multiple_tmux_calls() {
        let io = TimedTmuxRecorder::default();
        let runner = JobBudgetTmuxRunner {
            io: &io,
            deadline: Instant::now() + Duration::from_millis(200),
        };

        runner.run(&["display-message", "-p", "one"]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        runner.run(&["display-message", "-p", "two"]).unwrap();

        let timeouts = io.timeouts.lock().unwrap();
        assert_eq!(timeouts.len(), 2);
        assert!(timeouts[0] <= Duration::from_millis(200));
        assert!(timeouts[1] < timeouts[0]);
        drop(timeouts);
        let expired = JobBudgetTmuxRunner {
            io: &io,
            deadline: Instant::now() - Duration::from_millis(1),
        };
        assert!(expired.run(&["display-message", "-p", "late"]).is_err());
        assert_eq!(io.timeouts.lock().unwrap().len(), 2);
    }
}

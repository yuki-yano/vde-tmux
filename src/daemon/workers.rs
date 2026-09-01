use std::time::Duration;

use crate::git::SystemGitRunner;

mod capture;
mod observation;
mod process;
mod sidebar_tmux;

pub use capture::{
    CAPTURE_COALESCE_WINDOW, CAPTURE_HISTORY_LINES, CaptureBatchError, CaptureBatchOutput,
    CaptureCoordinatorHandle, CaptureJobOutcome, CaptureJobSpec, CaptureSource,
    OBSERVATION_CAPTURE_GROUP_MAX_BYTES, OBSERVATION_CAPTURE_STDERR_MAX_BYTES,
    OBSERVATION_CAPTURE_STDOUT_MAX_BYTES, ObservationWorkerIo, SystemObservationWorkerIo,
    combined_capture_args, generate_capture_delimiter, parse_combined_capture,
    start_capture_coordinator,
};
pub use observation::{
    ObservationPollError, ObservationPollResult, ObservationSample, STALE_CAPTURE_SECONDS,
    USAGE_LIMIT_CAPTURE_INTERVAL_SECONDS, capture_sha256, classify_presence, infer_capture,
    observation_envelope, pane_removal_envelopes, run_observation_poll,
};
pub use process::{AgentProcessSnapshot, ProcessDetection, read_agent_process_snapshot};
pub use sidebar_tmux::{SidebarTmuxError, SystemWorkerIo, WorkerIo};

pub fn system_git_runner(timeout: Duration) -> SystemGitRunner {
    SystemGitRunner::new(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::daemon::topology::ServerIdentity;
    use crate::pane_state::PaneInstance;

    pub(super) fn server_identity() -> ServerIdentity {
        ServerIdentity {
            pid: 4242,
            start_time: 99,
        }
    }

    pub(super) fn pane_instance(id: &str, pid: u32) -> PaneInstance {
        PaneInstance {
            pane_id: id.to_string(),
            pane_pid: pid,
        }
    }

    #[test]
    fn git_worker_runner_receives_configured_timeout() {
        let runner = system_git_runner(Duration::from_millis(1234));
        assert_eq!(runner.timeout(), Duration::from_millis(1234));
    }
}

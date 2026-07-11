use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::daemon::protocol::v2::{PanePresentation, StatusContext, StatusSnapshot};
use crate::daemon::topology::ServerIdentity;
use crate::pane_state::PaneInstance;

pub const STATUS_PUSH_MIN_INTERVAL: Duration = Duration::from_secs(1);
pub const RENDER_CLOCK_INTERVAL: Duration = Duration::from_secs(30);
pub const STATUS_PUSH_SERVER_MISMATCH_SENTINEL: &str = "__vde_status_push_server_mismatch__";
const STATUS_PUSH_PANE_MISMATCH_PREFIX: &str = "__vde_status_push_pane_mismatch__";
const STATUS_PUSH_BATCH_FILE_PREFIX: &str = ".status-push-batch-";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DisplayOptionKey {
    GlobalSummary,
    SessionCategory { session_id: String },
    SessionSessions { session_id: String },
    SessionWindows { session_id: String },
    SessionAttention { session_id: String },
    PaneStatus(PaneInstance),
}

impl DisplayOptionKey {
    pub fn option_name(&self) -> &'static str {
        match self {
            Self::GlobalSummary => crate::options::KEY_STATUS_SUMMARY,
            Self::SessionCategory { .. } => crate::options::KEY_STATUS_CATEGORY,
            Self::SessionSessions { .. } => crate::options::KEY_STATUS_SESSIONS,
            Self::SessionWindows { .. } => crate::options::KEY_STATUS_WINDOWS,
            Self::SessionAttention { .. } => crate::options::KEY_STATUS_ATTENTION,
            Self::PaneStatus(_) => crate::options::KEY_STATUS_PANE,
        }
    }

    pub fn scope(&self) -> DisplayOptionScope<'_> {
        match self {
            Self::GlobalSummary => DisplayOptionScope::Global,
            Self::SessionCategory { session_id }
            | Self::SessionSessions { session_id }
            | Self::SessionWindows { session_id }
            | Self::SessionAttention { session_id } => DisplayOptionScope::Session { session_id },
            Self::PaneStatus(pane) => DisplayOptionScope::Pane(pane),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayOptionScope<'a> {
    Global,
    Session { session_id: &'a str },
    Pane(&'a PaneInstance),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayOptionValue {
    Set(String),
    Unset,
}

impl DisplayOptionValue {
    fn validate(&self, key: &DisplayOptionKey) -> Result<(), StatusPushError> {
        let Self::Set(value) = self else {
            return if matches!(key, DisplayOptionKey::SessionAttention { .. }) {
                Ok(())
            } else {
                Err(StatusPushError::InvalidDisplayValue(format!(
                    "{} cannot be unset",
                    key.option_name()
                )))
            };
        };
        if value.chars().any(char::is_control) {
            return Err(StatusPushError::InvalidDisplayValue(format!(
                "{} contains a control character",
                key.option_name()
            )));
        }
        if matches!(key, DisplayOptionKey::PaneStatus(_)) && (value.is_empty() || value == "0") {
            return Err(StatusPushError::InvalidDisplayValue(
                "@vde_status_pane must be neither empty nor 0 so tmux does not select its fallback"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisplayFrame {
    values: BTreeMap<DisplayOptionKey, DisplayOptionValue>,
}

impl DisplayFrame {
    pub fn new(values: BTreeMap<DisplayOptionKey, DisplayOptionValue>) -> Self {
        Self { values }
    }

    pub fn values(&self) -> &BTreeMap<DisplayOptionKey, DisplayOptionValue> {
        &self.values
    }
}

/// Renders one complete display projection without querying tmux or reading display options.
/// Callers rebuild the status snapshots at each revision or render-clock trigger so elapsed
/// attention time advances independently of the canonical snapshot revision.
pub fn build_display_frame(
    config: &Config,
    global_snapshot: &StatusSnapshot,
    session_snapshots: &[StatusSnapshot],
    panes: &[PanePresentation],
    now_epoch: i64,
) -> Result<DisplayFrame, StatusPushError> {
    if global_snapshot.context != StatusContext::Global {
        return Err(StatusPushError::InvalidDisplaySnapshot(
            "global display snapshot must use global context".to_string(),
        ));
    }

    let mut values = BTreeMap::new();
    let global = crate::statusline::render_structured_status_snapshot(config, global_snapshot);
    values.insert(
        DisplayOptionKey::GlobalSummary,
        DisplayOptionValue::Set(global.summary),
    );

    for snapshot in session_snapshots {
        if snapshot.snapshot_revision != global_snapshot.snapshot_revision {
            return Err(StatusPushError::InvalidDisplaySnapshot(format!(
                "session snapshot revision {} differs from global revision {}",
                snapshot.snapshot_revision, global_snapshot.snapshot_revision
            )));
        }
        let StatusContext::Session { session_id } = &snapshot.context else {
            return Err(StatusPushError::InvalidDisplaySnapshot(
                "session display snapshot must use session context".to_string(),
            ));
        };
        let rendered = crate::statusline::render_structured_status_snapshot(config, snapshot);
        let session_values = [
            (
                DisplayOptionKey::SessionCategory {
                    session_id: session_id.clone(),
                },
                rendered.category,
            ),
            (
                DisplayOptionKey::SessionSessions {
                    session_id: session_id.clone(),
                },
                rendered.sessions,
            ),
            (
                DisplayOptionKey::SessionWindows {
                    session_id: session_id.clone(),
                },
                rendered.windows,
            ),
            (
                DisplayOptionKey::SessionAttention {
                    session_id: session_id.clone(),
                },
                rendered.attention,
            ),
        ];
        for (key, value) in session_values {
            if values.insert(key, DisplayOptionValue::Set(value)).is_some() {
                return Err(StatusPushError::InvalidDisplaySnapshot(format!(
                    "duplicate session display snapshot for {session_id}"
                )));
            }
        }
    }

    for pane in panes {
        let key = DisplayOptionKey::PaneStatus(pane.pane_instance.clone());
        let value = DisplayOptionValue::Set(crate::statusline::render_structured_pane_status(
            config, pane, now_epoch,
        ));
        if values.insert(key, value).is_some() {
            return Err(StatusPushError::InvalidDisplaySnapshot(format!(
                "duplicate pane display projection for {}:{}",
                pane.pane_instance.pane_id, pane.pane_instance.pane_pid
            )));
        }
    }

    for (key, value) in &values {
        validate_key(key)?;
        value.validate(key)?;
    }
    Ok(DisplayFrame::new(values))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayOptionWrite {
    pub key: DisplayOptionKey,
    pub value: DisplayOptionValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedDisplayBatch {
    pub expected_server: ServerIdentity,
    pub writes: Vec<DisplayOptionWrite>,
}

impl GuardedDisplayBatch {
    pub fn contains_mixed_scopes(&self) -> bool {
        let mut global = false;
        let mut session = false;
        let mut pane = false;
        for write in &self.writes {
            match write.key.scope() {
                DisplayOptionScope::Global => global = true,
                DisplayOptionScope::Session { .. } => session = true,
                DisplayOptionScope::Pane(_) => pane = true,
            }
        }
        usize::from(global) + usize::from(session) + usize::from(pane) > 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DisplayBatchId(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDisplayBatch {
    pub id: DisplayBatchId,
    pub guarded: GuardedDisplayBatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusPushDecision {
    Ignored,
    NoChanges,
    WaitingForInFlight,
    Coalesced { ready_at: Duration },
    Batch(PreparedDisplayBatch),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusPushError {
    SnapshotRevisionRegressed { previous: u64, received: u64 },
    CounterOverflow,
    ClockOverflow,
    UnknownBatch(DisplayBatchId),
    InvalidDisplaySnapshot(String),
    InvalidDisplayKey(String),
    InvalidDisplayValue(String),
}

impl fmt::Display for StatusPushError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotRevisionRegressed { previous, received } => write!(
                formatter,
                "snapshot revision regressed from {previous} to {received}"
            ),
            Self::CounterOverflow => formatter.write_str("display batch counter overflow"),
            Self::ClockOverflow => formatter.write_str("render clock overflow"),
            Self::UnknownBatch(id) => write!(formatter, "unknown display batch {}", id.0),
            Self::InvalidDisplaySnapshot(message)
            | Self::InvalidDisplayKey(message)
            | Self::InvalidDisplayValue(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StatusPushError {}

#[derive(Debug, PartialEq, Eq)]
pub enum DisplayBatchIoOutcome<E> {
    Succeeded,
    Failed(E),
    ServerIncarnationMismatch,
    PaneInstanceMismatch(PaneInstance),
}

pub trait DisplayBatchIo {
    type Error;

    /// Executes every scope in one tmux client process. Implementations must place all writes in
    /// the true branch of a PID + start-time incarnation guard. The false branch must print
    /// `STATUS_PUSH_SERVER_MISMATCH_SENTINEL` and be decoded as `ServerIncarnationMismatch`.
    /// Pane PID guard failures must be returned as `PaneInstanceMismatch`. `Succeeded` is
    /// reserved for a fully successful command group.
    fn execute_guarded_batch(
        &mut self,
        batch: &GuardedDisplayBatch,
    ) -> DisplayBatchIoOutcome<Self::Error>;
}

pub struct SystemDisplayBatchIo<'a> {
    runner: &'a dyn crate::tmux::TmuxRunner,
    batch_dir: &'a Path,
}

impl<'a> SystemDisplayBatchIo<'a> {
    pub fn new(runner: &'a dyn crate::tmux::TmuxRunner, batch_dir: &'a Path) -> Self {
        Self { runner, batch_dir }
    }
}

struct StatusPushBatchFile {
    path: PathBuf,
}

impl StatusPushBatchFile {
    fn create(batch_dir: &Path, commands: &[String]) -> Result<Self, String> {
        crate::daemon::lifecycle::ensure_secure_socket_dir(batch_dir)
            .map_err(|error| format!("failed to prepare status batch directory: {error:#}"))?;
        let event_id = crate::pane_state::EventId::generate()
            .map_err(|error| format!("failed to generate status batch ID: {error}"))?;
        let path = batch_dir.join(format!(
            "{STATUS_PUSH_BATCH_FILE_PREFIX}{}.conf",
            event_id.as_str()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| {
                format!(
                    "failed to create status batch file {}: {error}",
                    path.display()
                )
            })?;
        let batch_file = Self { path };
        for command in commands {
            writeln!(file, "{command}").map_err(|error| {
                format!(
                    "failed to write status batch file {}: {error}",
                    batch_file.path.display()
                )
            })?;
        }
        file.flush().map_err(|error| {
            format!(
                "failed to flush status batch file {}: {error}",
                batch_file.path.display()
            )
        })?;
        Ok(batch_file)
    }
}

impl Drop for StatusPushBatchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl DisplayBatchIo for SystemDisplayBatchIo<'_> {
    type Error = String;

    fn execute_guarded_batch(
        &mut self,
        batch: &GuardedDisplayBatch,
    ) -> DisplayBatchIoOutcome<Self::Error> {
        let commands = batch
            .writes
            .iter()
            .map(display_write_command)
            .collect::<Vec<_>>();
        let command_file = match StatusPushBatchFile::create(self.batch_dir, &commands) {
            Ok(file) => file,
            Err(error) => return DisplayBatchIoOutcome::Failed(error),
        };
        let source_command = crate::pane_state::store::tmux_command_string(&[
            "source-file".to_string(),
            command_file.path.to_string_lossy().into_owned(),
        ]);
        let guarded = crate::pane_state::store::server_guarded_command_args(
            batch.expected_server.pid,
            batch.expected_server.start_time,
            source_command,
            STATUS_PUSH_SERVER_MISMATCH_SENTINEL,
        );
        let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
        match self.runner.run(&refs) {
            Ok(output)
                if output
                    .lines()
                    .any(|line| line.trim() == STATUS_PUSH_SERVER_MISMATCH_SENTINEL) =>
            {
                DisplayBatchIoOutcome::ServerIncarnationMismatch
            }
            Ok(output) => {
                for write in &batch.writes {
                    let DisplayOptionKey::PaneStatus(pane) = &write.key else {
                        continue;
                    };
                    let sentinel = pane_mismatch_sentinel(pane);
                    if output.lines().any(|line| line.trim() == sentinel) {
                        return DisplayBatchIoOutcome::PaneInstanceMismatch(pane.clone());
                    }
                }
                DisplayBatchIoOutcome::Succeeded
            }
            Err(error) => DisplayBatchIoOutcome::Failed(error.to_string()),
        }
    }
}

fn display_write_command(write: &DisplayOptionWrite) -> String {
    let mut args = vec!["set-option".to_string()];
    match write.key.scope() {
        DisplayOptionScope::Global => args.push("-g".to_string()),
        DisplayOptionScope::Session { session_id } => {
            args.extend(["-t".to_string(), session_id.to_string()]);
        }
        DisplayOptionScope::Pane(pane) => {
            args.extend(["-p".to_string(), "-t".to_string(), pane.pane_id.clone()]);
        }
    }
    match &write.value {
        DisplayOptionValue::Set(value) => {
            args.extend([write.key.option_name().to_string(), value.clone()]);
        }
        DisplayOptionValue::Unset => {
            args.push("-u".to_string());
            args.push(write.key.option_name().to_string());
        }
    }
    let command = crate::pane_state::store::tmux_command_string(&args);
    let DisplayOptionKey::PaneStatus(pane) = &write.key else {
        return command;
    };
    crate::pane_state::store::tmux_command_string(&[
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.pane_id.clone(),
        format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid),
        command,
        format!(
            "display-message -p {}",
            crate::pane_state::store::quote_tmux_command_argument(&pane_mismatch_sentinel(pane))
        ),
    ])
}

fn pane_mismatch_sentinel(pane: &PaneInstance) -> String {
    format!(
        "{STATUS_PUSH_PANE_MISMATCH_PREFIX}:{}:{}",
        pane.pane_id, pane.pane_pid
    )
}

#[derive(Debug, PartialEq, Eq)]
pub enum BatchExecution<E> {
    Committed,
    Failed(E),
    ServerIncarnationMismatch,
    PaneInstanceMismatch(PaneInstance),
}

#[derive(Debug, Clone)]
struct InFlightBatch {
    id: DisplayBatchId,
    attempted: BTreeMap<DisplayOptionKey, DisplayOptionValue>,
}

#[derive(Debug, Clone)]
pub struct StatusPushState {
    expected_server: ServerIdentity,
    desired: BTreeMap<DisplayOptionKey, DisplayOptionValue>,
    dirty: BTreeSet<DisplayOptionKey>,
    last_successful: BTreeMap<DisplayOptionKey, DisplayOptionValue>,
    last_snapshot_revision: Option<u64>,
    next_clock_at: Duration,
    last_attempt_at: Option<Duration>,
    pending_coalesced_trigger: bool,
    shutting_down: bool,
    next_batch_id: u64,
    in_flight: Option<InFlightBatch>,
}

impl StatusPushState {
    pub fn new(
        expected_server: ServerIdentity,
        started_at: Duration,
    ) -> Result<Self, StatusPushError> {
        Ok(Self {
            expected_server,
            desired: BTreeMap::new(),
            dirty: BTreeSet::new(),
            last_successful: BTreeMap::new(),
            last_snapshot_revision: None,
            next_clock_at: checked_add(started_at, RENDER_CLOCK_INTERVAL)?,
            last_attempt_at: None,
            pending_coalesced_trigger: false,
            shutting_down: false,
            next_batch_id: 1,
            in_flight: None,
        })
    }

    pub fn desired(&self) -> &BTreeMap<DisplayOptionKey, DisplayOptionValue> {
        &self.desired
    }

    pub fn dirty(&self) -> &BTreeSet<DisplayOptionKey> {
        &self.dirty
    }

    pub fn last_successful(&self) -> &BTreeMap<DisplayOptionKey, DisplayOptionValue> {
        &self.last_successful
    }

    pub fn last_snapshot_revision(&self) -> Option<u64> {
        self.last_snapshot_revision
    }

    pub fn next_clock_at(&self) -> Duration {
        self.next_clock_at
    }

    pub fn on_snapshot_revision(
        &mut self,
        snapshot_revision: u64,
        now: Duration,
        frame: DisplayFrame,
    ) -> Result<StatusPushDecision, StatusPushError> {
        if self.shutting_down {
            return Ok(StatusPushDecision::Ignored);
        }
        if let Some(previous) = self.last_snapshot_revision {
            if snapshot_revision < previous {
                return Err(StatusPushError::SnapshotRevisionRegressed {
                    previous,
                    received: snapshot_revision,
                });
            }
            if snapshot_revision == previous {
                return Ok(StatusPushDecision::Ignored);
            }
        }
        self.accept_rendered_frame(frame)?;
        self.last_snapshot_revision = Some(snapshot_revision);
        self.pending_coalesced_trigger = true;
        self.prepare_if_due(now)
    }

    pub fn on_render_clock(
        &mut self,
        now: Duration,
        frame: DisplayFrame,
    ) -> Result<StatusPushDecision, StatusPushError> {
        if self.shutting_down {
            return Ok(StatusPushDecision::Ignored);
        }
        if now < self.next_clock_at {
            return Ok(StatusPushDecision::Ignored);
        }
        let mut next_clock_at = self.next_clock_at;
        while next_clock_at <= now {
            next_clock_at = checked_add(next_clock_at, RENDER_CLOCK_INTERVAL)?;
        }
        self.accept_rendered_frame(frame)?;
        self.next_clock_at = next_clock_at;
        self.pending_coalesced_trigger = true;
        self.prepare_if_due(now)
    }

    /// Flushes a render trigger that was coalesced by the 1Hz limiter. This never retries a
    /// failed batch by itself; failures remain dirty until a later revision or render-clock tick.
    pub fn flush_coalesced(
        &mut self,
        now: Duration,
    ) -> Result<StatusPushDecision, StatusPushError> {
        if !self.pending_coalesced_trigger {
            return Ok(StatusPushDecision::NoChanges);
        }
        self.prepare_if_due(now)
    }

    pub fn pane_removed(&mut self, pane: &PaneInstance) {
        self.remove_key(&DisplayOptionKey::PaneStatus(pane.clone()));
    }

    pub fn retain_topology_targets(
        &mut self,
        sessions: &BTreeSet<String>,
        panes: &BTreeSet<PaneInstance>,
    ) {
        let keep = |key: &DisplayOptionKey| match key {
            DisplayOptionKey::GlobalSummary => true,
            DisplayOptionKey::SessionCategory { session_id }
            | DisplayOptionKey::SessionSessions { session_id }
            | DisplayOptionKey::SessionWindows { session_id }
            | DisplayOptionKey::SessionAttention { session_id } => sessions.contains(session_id),
            DisplayOptionKey::PaneStatus(pane) => panes.contains(pane),
        };
        self.desired.retain(|key, _| keep(key));
        self.dirty.retain(keep);
        self.last_successful.retain(|key, _| keep(key));
    }

    pub fn request_shutdown(
        &mut self,
        now: Duration,
        marker: String,
    ) -> Result<StatusPushDecision, StatusPushError> {
        let marker = DisplayOptionValue::Set(marker);
        marker.validate(&DisplayOptionKey::GlobalSummary)?;
        self.shutting_down = true;
        self.desired.insert(DisplayOptionKey::GlobalSummary, marker);
        self.dirty.insert(DisplayOptionKey::GlobalSummary);

        let attention = self
            .desired
            .keys()
            .filter(|key| matches!(key, DisplayOptionKey::SessionAttention { .. }))
            .cloned()
            .collect::<Vec<_>>();
        for key in attention {
            self.desired.insert(key.clone(), DisplayOptionValue::Unset);
            self.dirty.insert(key);
        }
        self.dirty.retain(|key| {
            matches!(
                key,
                DisplayOptionKey::GlobalSummary | DisplayOptionKey::SessionAttention { .. }
            )
        });
        self.pending_coalesced_trigger = true;
        self.prepare_if_due(now)
    }

    pub fn complete_batch(
        &mut self,
        id: DisplayBatchId,
        succeeded: bool,
    ) -> Result<(), StatusPushError> {
        let in_flight = self
            .in_flight
            .take()
            .ok_or(StatusPushError::UnknownBatch(id))?;
        if in_flight.id != id {
            self.in_flight = Some(in_flight);
            return Err(StatusPushError::UnknownBatch(id));
        }

        for (key, attempted) in in_flight.attempted {
            if self.shutting_down
                && !matches!(
                    key,
                    DisplayOptionKey::GlobalSummary | DisplayOptionKey::SessionAttention { .. }
                )
            {
                continue;
            }
            if !self.desired.contains_key(&key) {
                continue;
            }
            if succeeded {
                self.last_successful.insert(key.clone(), attempted);
                self.dirty.remove(&key);
            } else {
                self.dirty.insert(key);
            }
        }
        if self.shutting_down && !succeeded && !self.dirty.is_empty() {
            self.pending_coalesced_trigger = true;
        }
        Ok(())
    }

    pub fn execute_prepared<I: DisplayBatchIo>(
        &mut self,
        prepared: &PreparedDisplayBatch,
        io: &mut I,
    ) -> Result<BatchExecution<I::Error>, StatusPushError> {
        self.validate_prepared(prepared)?;
        match io.execute_guarded_batch(&prepared.guarded) {
            DisplayBatchIoOutcome::Succeeded => {
                self.complete_batch(prepared.id, true)?;
                Ok(BatchExecution::Committed)
            }
            DisplayBatchIoOutcome::Failed(error) => {
                self.complete_batch(prepared.id, false)?;
                Ok(BatchExecution::Failed(error))
            }
            DisplayBatchIoOutcome::ServerIncarnationMismatch => {
                self.complete_batch(prepared.id, false)?;
                Ok(BatchExecution::ServerIncarnationMismatch)
            }
            DisplayBatchIoOutcome::PaneInstanceMismatch(pane) => {
                self.complete_batch(prepared.id, false)?;
                Ok(BatchExecution::PaneInstanceMismatch(pane))
            }
        }
    }

    fn accept_rendered_frame(&mut self, frame: DisplayFrame) -> Result<(), StatusPushError> {
        for (key, value) in &frame.values {
            validate_key(key)?;
            value.validate(key)?;
        }

        let current = frame.values.keys().cloned().collect::<BTreeSet<_>>();
        self.desired.retain(|key, _| current.contains(key));
        self.dirty.retain(|key| current.contains(key));
        self.last_successful.retain(|key, _| current.contains(key));

        for (key, value) in frame.values {
            self.desired.insert(key, value);
        }
        Ok(())
    }

    fn prepare_if_due(&mut self, now: Duration) -> Result<StatusPushDecision, StatusPushError> {
        if self.in_flight.is_some() {
            return Ok(StatusPushDecision::WaitingForInFlight);
        }
        let candidates = self
            .desired
            .iter()
            .filter_map(|(key, value)| {
                let terminal_key = matches!(
                    key,
                    DisplayOptionKey::GlobalSummary | DisplayOptionKey::SessionAttention { .. }
                );
                ((!self.shutting_down || terminal_key)
                    && (self.dirty.contains(key) || self.last_successful.get(key) != Some(value)))
                .then(|| key.clone())
            })
            .collect::<BTreeSet<_>>();
        let attempted = candidates
            .iter()
            .filter_map(|key| {
                self.desired
                    .get(key)
                    .cloned()
                    .map(|value| (key.clone(), value))
            })
            .collect::<BTreeMap<_, _>>();
        if attempted.is_empty() {
            self.pending_coalesced_trigger = false;
            return Ok(StatusPushDecision::NoChanges);
        }
        if let Some(last_attempt_at) = self.last_attempt_at {
            let ready_at = checked_add(last_attempt_at, STATUS_PUSH_MIN_INTERVAL)?;
            if now < ready_at {
                return Ok(StatusPushDecision::Coalesced { ready_at });
            }
        }

        let id = DisplayBatchId(self.next_batch_id);
        self.next_batch_id = self
            .next_batch_id
            .checked_add(1)
            .ok_or(StatusPushError::CounterOverflow)?;
        let writes = attempted
            .iter()
            .map(|(key, value)| DisplayOptionWrite {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();
        let prepared = PreparedDisplayBatch {
            id,
            guarded: GuardedDisplayBatch {
                expected_server: self.expected_server.clone(),
                writes,
            },
        };
        self.in_flight = Some(InFlightBatch { id, attempted });
        self.last_attempt_at = Some(now);
        self.pending_coalesced_trigger = false;
        Ok(StatusPushDecision::Batch(prepared))
    }

    fn remove_key(&mut self, key: &DisplayOptionKey) {
        self.desired.remove(key);
        self.dirty.remove(key);
        self.last_successful.remove(key);
    }

    fn validate_prepared(&self, prepared: &PreparedDisplayBatch) -> Result<(), StatusPushError> {
        let Some(in_flight) = &self.in_flight else {
            return Err(StatusPushError::UnknownBatch(prepared.id));
        };
        let writes = prepared
            .guarded
            .writes
            .iter()
            .map(|write| (write.key.clone(), write.value.clone()))
            .collect::<BTreeMap<_, _>>();
        if in_flight.id != prepared.id
            || prepared.guarded.expected_server != self.expected_server
            || writes != in_flight.attempted
            || prepared.guarded.writes.len() != writes.len()
        {
            return Err(StatusPushError::UnknownBatch(prepared.id));
        }
        Ok(())
    }
}

fn validate_key(key: &DisplayOptionKey) -> Result<(), StatusPushError> {
    match key {
        DisplayOptionKey::GlobalSummary => Ok(()),
        DisplayOptionKey::SessionCategory { session_id }
        | DisplayOptionKey::SessionSessions { session_id }
        | DisplayOptionKey::SessionWindows { session_id }
        | DisplayOptionKey::SessionAttention { session_id } => {
            let valid = session_id.strip_prefix('$').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            });
            if valid {
                Ok(())
            } else {
                Err(StatusPushError::InvalidDisplayKey(format!(
                    "invalid session target {session_id:?}"
                )))
            }
        }
        DisplayOptionKey::PaneStatus(pane) => pane
            .validate()
            .map_err(|error| StatusPushError::InvalidDisplayKey(error.to_string())),
    }
}

fn checked_add(left: Duration, right: Duration) -> Result<Duration, StatusPushError> {
    left.checked_add(right)
        .ok_or(StatusPushError::ClockOverflow)
}

pub enum DisplayTextFragment<'a> {
    Trusted(&'a str),
    External(&'a str),
}

pub fn render_display_text(fragments: &[DisplayTextFragment<'_>]) -> String {
    let mut rendered = String::new();
    for fragment in fragments {
        match fragment {
            DisplayTextFragment::Trusted(value) => rendered.push_str(value),
            DisplayTextFragment::External(value) => {
                rendered.push_str(&escape_external_display_text(value));
            }
        }
    }
    rendered
}

pub fn escape_external_display_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            escaped.push(' ');
        } else if character == '#' {
            escaped.push_str("##");
        } else {
            escaped.push(character);
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    fn server() -> ServerIdentity {
        ServerIdentity {
            pid: 42,
            start_time: 99,
        }
    }

    fn pane(pid: u32) -> PaneInstance {
        PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: pid,
        }
    }

    fn global_snapshot(revision: u64) -> StatusSnapshot {
        StatusSnapshot {
            snapshot_revision: revision,
            context: StatusContext::Global,
            summary: crate::daemon::session_badge::BadgeStateCounts {
                blocked: 1,
                working: 1,
                done: 0,
                idle: 1,
            },
            sessions: Vec::new(),
            windows: Vec::new(),
            categories: Vec::new(),
            attention: Vec::new(),
        }
    }

    fn session_snapshot(session_id: &str, revision: u64, elapsed_seconds: i64) -> StatusSnapshot {
        use crate::daemon::protocol::v2::{
            AttentionEntry, CategoryStatusPresentation, SessionStatusPresentation,
            WindowStatusPresentation,
        };
        use crate::daemon::session_badge::{BadgeState, BadgeStateCounts};

        StatusSnapshot {
            snapshot_revision: revision,
            context: StatusContext::Session {
                session_id: session_id.to_string(),
            },
            summary: BadgeStateCounts::default(),
            sessions: vec![SessionStatusPresentation {
                session_id: session_id.to_string(),
                session_name: "dev#[fg=red]\n".to_string(),
                category: Some("work".to_string()),
                attached: Some(true),
                created_at: Some(1),
                active: true,
                counts: BadgeStateCounts {
                    blocked: 1,
                    ..BadgeStateCounts::default()
                },
            }],
            windows: vec![WindowStatusPresentation {
                window_id: "@1".to_string(),
                window_name: "editor#{pane_id}".to_string(),
                pane_count: 1,
                session_ids: vec![session_id.to_string()],
                window_index: Some(1),
                active: true,
                last: false,
                bell: None,
                activity: None,
                silence: None,
                current_command: Some("nvim#[fg=red]".to_string()),
                counts: BadgeStateCounts {
                    blocked: 1,
                    ..BadgeStateCounts::default()
                },
            }],
            categories: vec![CategoryStatusPresentation {
                category: "work".to_string(),
                session_ids: vec![session_id.to_string()],
                active: true,
                counts: BadgeStateCounts {
                    blocked: 1,
                    ..BadgeStateCounts::default()
                },
            }],
            attention: vec![AttentionEntry {
                pane_instance: pane(700),
                session_name: "dev#(unsafe)".to_string(),
                badge: BadgeState::Blocked,
                reason: Some("PermissionPrompt".to_string()),
                elapsed_seconds,
            }],
        }
    }

    fn pane_presentation() -> PanePresentation {
        use crate::daemon::session_badge::BadgeState;
        use crate::pane_state::{
            AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneState, ResolvedPaneState,
            StateId, TaskState, WaitReason,
        };

        let pane_instance = pane(700);
        PanePresentation {
            pane_instance: pane_instance.clone(),
            session_links: Vec::new(),
            window_id: "@1".to_string(),
            window_name: "editor".to_string(),
            current_path: "/tmp/#work".to_string(),
            current_command: "codex#[fg=red]".to_string(),
            active: true,
            stored: None,
            resolved: Some(ResolvedPaneState {
                canonical: PaneState {
                    schema_version: PANE_STATE_SCHEMA_VERSION,
                    state_id: StateId::parse("00000000000000000000000000000700").unwrap(),
                    revision: 1,
                    pane_instance: pane_instance.clone(),
                    agent: AgentKind::parse("codex").unwrap(),
                    agent_session_id: None,
                    agent_epoch: 1,
                    agent_present: true,
                    scan_verified: true,
                    synthetic_completion_armed: false,
                    lifecycle: LifecycleState::Waiting {
                        reason: WaitReason::PermissionPrompt,
                    },
                    run_seq: 1,
                    completed_seq: 0,
                    acknowledged_seq: 0,
                    started_at: Some(0),
                    completed_at: None,
                    prompt: None,
                    tasks: TaskState::default(),
                    subagents: Vec::new(),
                    worktree_activity: None,
                },
                window_id: "@1".to_string(),
                pane_id: pane_instance.pane_id.clone(),
                current_path: "/tmp/#work".to_string(),
                badge: BadgeState::Blocked,
            }),
            diagnostic: None,
        }
    }

    fn non_agent_pane_presentation() -> PanePresentation {
        PanePresentation {
            pane_instance: PaneInstance {
                pane_id: "%2".to_string(),
                pane_pid: 701,
            },
            session_links: Vec::new(),
            window_id: "@1".to_string(),
            window_name: "editor".to_string(),
            current_path: "/tmp/#{path}\t".to_string(),
            current_command: "zsh#[fg=red]\n".to_string(),
            active: false,
            stored: None,
            resolved: None,
            diagnostic: None,
        }
    }

    #[test]
    fn frame_builder_renders_global_session_and_all_pane_scopes() {
        let mut config = Config::default();
        config.statusline.windows.current.format = "{window}|{command}".to_string();
        let global = global_snapshot(9);
        let sessions = [session_snapshot("$1", 9, 61), session_snapshot("$2", 9, 61)];
        let pane = pane_presentation();
        let non_agent = non_agent_pane_presentation();

        let frame = build_display_frame(
            &config,
            &global,
            &sessions,
            &[pane.clone(), non_agent.clone()],
            61,
        )
        .unwrap();

        assert_eq!(frame.values().len(), 11);
        assert!(matches!(
            frame.values().get(&DisplayOptionKey::GlobalSummary),
            Some(DisplayOptionValue::Set(value)) if value.contains("▲1") && value.contains("●1") && value.contains("○1")
        ));
        for session_id in ["$1", "$2"] {
            for key in [
                DisplayOptionKey::SessionCategory {
                    session_id: session_id.to_string(),
                },
                DisplayOptionKey::SessionSessions {
                    session_id: session_id.to_string(),
                },
                DisplayOptionKey::SessionWindows {
                    session_id: session_id.to_string(),
                },
                DisplayOptionKey::SessionAttention {
                    session_id: session_id.to_string(),
                },
            ] {
                assert!(
                    matches!(frame.values().get(&key), Some(DisplayOptionValue::Set(_))),
                    "missing {key:?}"
                );
            }
        }
        let sessions = frame
            .values()
            .get(&DisplayOptionKey::SessionSessions {
                session_id: "$1".to_string(),
            })
            .unwrap();
        assert!(
            matches!(sessions, DisplayOptionValue::Set(value) if value.contains("dev##[fg=red] ")),
            "{sessions:?}"
        );
        let windows = frame
            .values()
            .get(&DisplayOptionKey::SessionWindows {
                session_id: "$1".to_string(),
            })
            .unwrap();
        assert!(
            matches!(windows, DisplayOptionValue::Set(value) if value.contains("editor##{pane_id}|nvim##[fg=red]")),
            "{windows:?}"
        );
        let attention = frame
            .values()
            .get(&DisplayOptionKey::SessionAttention {
                session_id: "$1".to_string(),
            })
            .unwrap();
        assert!(
            matches!(attention, DisplayOptionValue::Set(value) if value.contains("dev##(unsafe)") && value.contains("1m")),
            "{attention:?}"
        );
        let pane_value = frame
            .values()
            .get(&DisplayOptionKey::PaneStatus(pane.pane_instance))
            .unwrap();
        assert!(
            matches!(pane_value, DisplayOptionValue::Set(value) if !value.is_empty() && value != "0" && value.contains("1m")),
            "{pane_value:?}"
        );
        let non_agent_value = frame
            .values()
            .get(&DisplayOptionKey::PaneStatus(non_agent.pane_instance))
            .unwrap();
        assert!(
            matches!(non_agent_value, DisplayOptionValue::Set(value) if value.contains("zsh##[fg=red] ")),
            "{non_agent_value:?}"
        );
    }

    #[test]
    fn frame_builder_rejects_context_revision_and_identity_mismatches() {
        let config = Config::default();
        let mut wrong_global = global_snapshot(1);
        wrong_global.context = StatusContext::Session {
            session_id: "$1".to_string(),
        };
        assert!(matches!(
            build_display_frame(&config, &wrong_global, &[], &[], 0),
            Err(StatusPushError::InvalidDisplaySnapshot(_))
        ));

        let global = global_snapshot(1);
        assert!(matches!(
            build_display_frame(&config, &global, &[session_snapshot("$1", 2, 0)], &[], 0,),
            Err(StatusPushError::InvalidDisplaySnapshot(_))
        ));
        assert!(matches!(
            build_display_frame(
                &config,
                &global,
                &[session_snapshot("$1", 1, 0), session_snapshot("$1", 1, 0),],
                &[],
                0,
            ),
            Err(StatusPushError::InvalidDisplaySnapshot(_))
        ));
        let pane = pane_presentation();
        assert!(matches!(
            build_display_frame(&config, &global, &[], &[pane.clone(), pane], 0),
            Err(StatusPushError::InvalidDisplaySnapshot(_))
        ));
    }

    #[test]
    fn thirty_second_clock_rebuild_updates_only_time_bearing_options() {
        let config = Config::default();
        let global = global_snapshot(7);
        let pane = pane_presentation();
        let initial = build_display_frame(
            &config,
            &global,
            &[session_snapshot("$1", 7, 59)],
            std::slice::from_ref(&pane),
            59,
        )
        .unwrap();
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(7, Duration::ZERO, initial)
                .unwrap(),
        );
        state.complete_batch(first.id, true).unwrap();

        let clock_frame = build_display_frame(
            &config,
            &global,
            &[session_snapshot("$1", 7, 89)],
            &[pane],
            89,
        )
        .unwrap();
        let tick = take_batch(
            state
                .on_render_clock(Duration::from_secs(30), clock_frame)
                .unwrap(),
        );

        assert_eq!(tick.guarded.writes.len(), 2);
        assert!(
            tick.guarded
                .writes
                .iter()
                .any(|write| matches!(write.key, DisplayOptionKey::SessionAttention { .. }))
        );
        assert!(
            tick.guarded
                .writes
                .iter()
                .any(|write| matches!(write.key, DisplayOptionKey::PaneStatus(_)))
        );
        assert!(tick.guarded.writes.iter().all(|write| {
            matches!(&write.value, DisplayOptionValue::Set(value) if value.contains("1m"))
        }));
    }

    #[derive(Default)]
    struct InspectBatchRunner {
        batch_dir: PathBuf,
        calls: RefCell<Vec<Vec<String>>>,
        bodies: RefCell<Vec<String>>,
        modes: RefCell<Vec<u32>>,
        output: String,
        fail: bool,
    }

    impl crate::tmux::TmuxRunner for InspectBatchRunner {
        fn run(&self, args: &[&str]) -> anyhow::Result<String> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|value| (*value).to_string()).collect());
            let files = std::fs::read_dir(&self.batch_dir)?.collect::<Result<Vec<_>, _>>()?;
            anyhow::ensure!(files.len() == 1, "expected exactly one status batch file");
            let path = files[0].path();
            self.modes
                .borrow_mut()
                .push(std::fs::metadata(&path)?.permissions().mode() & 0o777);
            self.bodies
                .borrow_mut()
                .push(std::fs::read_to_string(path)?);
            if self.fail {
                anyhow::bail!("tmux failed after reading status batch")
            }
            Ok(self.output.clone())
        }
    }

    fn unique_status_batch_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "vde-tmux-status-batch-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn system_io_uses_one_file_backed_server_guarded_process_for_large_batch() {
        let batch_dir = unique_status_batch_dir();
        let mut writes = vec![DisplayOptionWrite {
            key: DisplayOptionKey::GlobalSummary,
            value: DisplayOptionValue::Set("summary".to_string()),
        }];
        writes.extend((1..=64).map(|pid| DisplayOptionWrite {
            key: DisplayOptionKey::PaneStatus(pane(pid)),
            value: DisplayOptionValue::Set(format!("#[fg=red]pane-{pid}-{}", "x".repeat(512))),
        }));
        let batch = GuardedDisplayBatch {
            expected_server: server(),
            writes,
        };
        let runner = InspectBatchRunner {
            batch_dir: batch_dir.clone(),
            ..InspectBatchRunner::default()
        };
        let mut io = SystemDisplayBatchIo::new(&runner, &batch_dir);

        assert_eq!(
            io.execute_guarded_batch(&batch),
            DisplayBatchIoOutcome::Succeeded
        );

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        let argv_bytes = calls[0].iter().map(String::len).sum::<usize>();
        assert!(argv_bytes < 4 * 1024, "guarded argv was {argv_bytes} bytes");
        let rendered_args = calls[0].join(" ");
        assert!(rendered_args.contains("source-file"));
        assert!(rendered_args.contains("#{pid}"));
        assert!(rendered_args.contains("#{start_time}"));
        assert!(!rendered_args.contains(&"x".repeat(512)));

        let bodies = runner.bodies.borrow();
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].len() > 40 * 1024);
        assert_eq!(bodies[0].lines().count(), 65);
        assert!(bodies[0].contains("pane_pid"));
        assert!(bodies[0].contains("64"));
        assert_eq!(runner.modes.borrow().as_slice(), &[0o600]);
        assert_eq!(
            std::fs::metadata(&batch_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(bodies);
        drop(calls);

        assert_eq!(std::fs::read_dir(&batch_dir).unwrap().count(), 0);
        std::fs::remove_dir(batch_dir).unwrap();
    }

    #[test]
    fn system_io_removes_batch_file_after_runner_failure() {
        let batch_dir = unique_status_batch_dir();
        let batch = GuardedDisplayBatch {
            expected_server: server(),
            writes: vec![DisplayOptionWrite {
                key: DisplayOptionKey::GlobalSummary,
                value: DisplayOptionValue::Set("summary".to_string()),
            }],
        };
        let runner = InspectBatchRunner {
            batch_dir: batch_dir.clone(),
            fail: true,
            ..InspectBatchRunner::default()
        };
        let mut io = SystemDisplayBatchIo::new(&runner, &batch_dir);

        assert!(matches!(
            io.execute_guarded_batch(&batch),
            DisplayBatchIoOutcome::Failed(error) if error.contains("tmux failed")
        ));
        assert_eq!(std::fs::read_dir(&batch_dir).unwrap().count(), 0);
        std::fs::remove_dir(batch_dir).unwrap();
    }

    #[test]
    fn system_io_preserves_mismatch_classification_with_file_backed_batch() {
        let batch = GuardedDisplayBatch {
            expected_server: server(),
            writes: vec![DisplayOptionWrite {
                key: DisplayOptionKey::PaneStatus(pane(77)),
                value: DisplayOptionValue::Set("pane".to_string()),
            }],
        };

        let server_dir = unique_status_batch_dir();
        let server_runner = InspectBatchRunner {
            batch_dir: server_dir.clone(),
            output: format!("{STATUS_PUSH_SERVER_MISMATCH_SENTINEL}\n"),
            ..InspectBatchRunner::default()
        };
        let mut server_io = SystemDisplayBatchIo::new(&server_runner, &server_dir);
        assert_eq!(
            server_io.execute_guarded_batch(&batch),
            DisplayBatchIoOutcome::ServerIncarnationMismatch
        );
        assert_eq!(std::fs::read_dir(&server_dir).unwrap().count(), 0);
        std::fs::remove_dir(server_dir).unwrap();

        let pane_dir = unique_status_batch_dir();
        let pane_runner = InspectBatchRunner {
            batch_dir: pane_dir.clone(),
            output: format!("{}\n", pane_mismatch_sentinel(&pane(77))),
            ..InspectBatchRunner::default()
        };
        let mut pane_io = SystemDisplayBatchIo::new(&pane_runner, &pane_dir);
        assert_eq!(
            pane_io.execute_guarded_batch(&batch),
            DisplayBatchIoOutcome::PaneInstanceMismatch(pane(77))
        );
        assert_eq!(std::fs::read_dir(&pane_dir).unwrap().count(), 0);
        std::fs::remove_dir(pane_dir).unwrap();
    }

    fn frame(
        summary: &str,
        attention: &str,
        pane_instance: PaneInstance,
        pane_text: &str,
    ) -> DisplayFrame {
        DisplayFrame::new(BTreeMap::from([
            (
                DisplayOptionKey::GlobalSummary,
                DisplayOptionValue::Set(summary.to_string()),
            ),
            (
                DisplayOptionKey::SessionCategory {
                    session_id: "$1".to_string(),
                },
                DisplayOptionValue::Set("category".to_string()),
            ),
            (
                DisplayOptionKey::SessionSessions {
                    session_id: "$1".to_string(),
                },
                DisplayOptionValue::Set("sessions".to_string()),
            ),
            (
                DisplayOptionKey::SessionWindows {
                    session_id: "$1".to_string(),
                },
                DisplayOptionValue::Set("windows".to_string()),
            ),
            (
                DisplayOptionKey::SessionAttention {
                    session_id: "$1".to_string(),
                },
                DisplayOptionValue::Set(attention.to_string()),
            ),
            (
                DisplayOptionKey::PaneStatus(pane_instance),
                DisplayOptionValue::Set(pane_text.to_string()),
            ),
        ]))
    }

    fn take_batch(decision: StatusPushDecision) -> PreparedDisplayBatch {
        let StatusPushDecision::Batch(batch) = decision else {
            panic!("expected a display batch, received {decision:?}");
        };
        batch
    }

    #[derive(Default)]
    struct FakeIo {
        calls: Vec<GuardedDisplayBatch>,
        fail: bool,
    }

    impl DisplayBatchIo for FakeIo {
        type Error = &'static str;

        fn execute_guarded_batch(
            &mut self,
            batch: &GuardedDisplayBatch,
        ) -> DisplayBatchIoOutcome<Self::Error> {
            self.calls.push(batch.clone());
            if self.fail {
                DisplayBatchIoOutcome::Failed("tmux failed")
            } else {
                DisplayBatchIoOutcome::Succeeded
            }
        }
    }

    struct ServerMismatchIo;

    impl DisplayBatchIo for ServerMismatchIo {
        type Error = &'static str;

        fn execute_guarded_batch(
            &mut self,
            _batch: &GuardedDisplayBatch,
        ) -> DisplayBatchIoOutcome<Self::Error> {
            DisplayBatchIoOutcome::ServerIncarnationMismatch
        }
    }

    #[test]
    fn external_text_is_control_normalized_and_hash_escaped() {
        assert_eq!(
            escape_external_display_text("name#x\n\t\0end"),
            "name##x   end"
        );
        assert_eq!(
            render_display_text(&[
                DisplayTextFragment::Trusted("#[fg=red]"),
                DisplayTextFragment::External("#(run)\n"),
            ]),
            "#[fg=red]##(run) "
        );
    }

    #[test]
    fn revision_writes_all_scopes_in_one_guarded_batch_and_commits_on_success() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        assert_eq!(state.last_snapshot_revision(), None);
        let batch = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "attn", pane(10), "pane"))
                .unwrap(),
        );
        assert_eq!(state.last_snapshot_revision(), Some(1));
        assert_eq!(batch.guarded.expected_server, server());
        assert_eq!(batch.guarded.writes.len(), 6);
        assert!(batch.guarded.contains_mixed_scopes());

        let mut io = FakeIo::default();
        assert_eq!(
            state.execute_prepared(&batch, &mut io).unwrap(),
            BatchExecution::Committed
        );
        assert_eq!(io.calls.len(), 1);
        assert!(state.dirty().is_empty());
        assert_eq!(state.last_successful(), state.desired());
    }

    #[test]
    fn one_hz_limit_coalesces_latest_render() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("one", "", pane(10), "pane"))
                .unwrap(),
        );
        state.complete_batch(first.id, true).unwrap();

        assert_eq!(
            state
                .on_snapshot_revision(
                    2,
                    Duration::from_millis(500),
                    frame("two", "", pane(10), "pane")
                )
                .unwrap(),
            StatusPushDecision::Coalesced {
                ready_at: Duration::from_secs(1)
            }
        );
        assert_eq!(
            state.flush_coalesced(Duration::from_millis(999)).unwrap(),
            StatusPushDecision::Coalesced {
                ready_at: Duration::from_secs(1)
            }
        );
        let second = take_batch(state.flush_coalesced(Duration::from_secs(1)).unwrap());
        assert_eq!(second.guarded.writes.len(), 1);
        assert_eq!(
            second.guarded.writes[0].value,
            DisplayOptionValue::Set("two".to_string())
        );
    }

    #[test]
    fn coalesced_value_that_returns_to_last_successful_does_not_write() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("one", "", pane(10), "pane"))
                .unwrap(),
        );
        state.complete_batch(first.id, true).unwrap();
        assert!(matches!(
            state
                .on_snapshot_revision(
                    2,
                    Duration::from_millis(100),
                    frame("two", "", pane(10), "pane"),
                )
                .unwrap(),
            StatusPushDecision::Coalesced { .. }
        ));
        assert!(matches!(
            state
                .on_snapshot_revision(
                    3,
                    Duration::from_millis(200),
                    frame("one", "", pane(10), "pane"),
                )
                .unwrap(),
            StatusPushDecision::NoChanges
        ));
        assert_eq!(
            state.flush_coalesced(Duration::from_secs(1)).unwrap(),
            StatusPushDecision::NoChanges
        );
    }

    #[test]
    fn failed_batch_stays_dirty_without_an_independent_retry() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "", pane(10), "pane"))
                .unwrap(),
        );
        let mut io = FakeIo {
            fail: true,
            ..FakeIo::default()
        };
        assert_eq!(
            state.execute_prepared(&first, &mut io).unwrap(),
            BatchExecution::Failed("tmux failed")
        );
        assert_eq!(state.dirty().len(), 6);
        assert!(state.last_successful().is_empty());
        assert_eq!(
            state.flush_coalesced(Duration::from_secs(5)).unwrap(),
            StatusPushDecision::NoChanges
        );

        let retry = take_batch(
            state
                .on_render_clock(Duration::from_secs(30), frame("sum", "", pane(10), "pane"))
                .unwrap(),
        );
        assert_eq!(retry.guarded.writes.len(), 6);
    }

    #[test]
    fn trigger_during_in_flight_batch_is_coalesced_and_uses_latest_value() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("one", "", pane(10), "pane"))
                .unwrap(),
        );
        assert_eq!(
            state
                .on_snapshot_revision(
                    2,
                    Duration::from_millis(100),
                    frame("two", "", pane(10), "pane"),
                )
                .unwrap(),
            StatusPushDecision::WaitingForInFlight
        );
        state.complete_batch(first.id, true).unwrap();
        let second = take_batch(state.flush_coalesced(Duration::from_secs(1)).unwrap());
        assert_eq!(second.guarded.writes.len(), 1);
        assert_eq!(
            second.guarded.writes[0].value,
            DisplayOptionValue::Set("two".to_string())
        );
    }

    #[test]
    fn invalid_frame_does_not_consume_snapshot_revision() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let invalid = DisplayFrame::new(BTreeMap::from([(
            DisplayOptionKey::PaneStatus(pane(10)),
            DisplayOptionValue::Set(String::new()),
        )]));
        assert!(matches!(
            state.on_snapshot_revision(1, Duration::ZERO, invalid),
            Err(StatusPushError::InvalidDisplayValue(_))
        ));
        assert!(matches!(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "", pane(10), "pane"))
                .unwrap(),
            StatusPushDecision::Batch(_)
        ));

        let mut zero_state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let zero = DisplayFrame::new(BTreeMap::from([(
            DisplayOptionKey::PaneStatus(pane(10)),
            DisplayOptionValue::Set("0".to_string()),
        )]));
        assert!(matches!(
            zero_state.on_snapshot_revision(1, Duration::ZERO, zero),
            Err(StatusPushError::InvalidDisplayValue(_))
        ));
    }

    #[test]
    fn forged_or_replayed_batch_is_rejected_before_io() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let batch = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "", pane(10), "pane"))
                .unwrap(),
        );
        let mut forged = batch.clone();
        forged.guarded.writes.pop();
        let mut io = FakeIo::default();
        assert!(matches!(
            state.execute_prepared(&forged, &mut io),
            Err(StatusPushError::UnknownBatch(_))
        ));
        assert!(io.calls.is_empty());
        assert_eq!(
            state.execute_prepared(&batch, &mut io).unwrap(),
            BatchExecution::Committed
        );
        assert!(matches!(
            state.execute_prepared(&batch, &mut io),
            Err(StatusPushError::UnknownBatch(_))
        ));
        assert_eq!(io.calls.len(), 1);
    }

    #[test]
    fn server_guard_mismatch_is_typed_and_never_committed() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let batch = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "", pane(10), "pane"))
                .unwrap(),
        );
        assert_eq!(
            state
                .execute_prepared(&batch, &mut ServerMismatchIo)
                .unwrap(),
            BatchExecution::ServerIncarnationMismatch
        );
        assert!(state.last_successful().is_empty());
        assert_eq!(state.dirty().len(), 6);
    }

    #[test]
    fn clock_rerenders_only_changed_values_and_equal_revision_is_not_a_trigger() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let initial_frame = frame("sum", "0m", pane(10), "pane 0m");
        let first = take_batch(
            state
                .on_snapshot_revision(7, Duration::ZERO, initial_frame.clone())
                .unwrap(),
        );
        state.complete_batch(first.id, true).unwrap();
        assert_eq!(
            state
                .on_snapshot_revision(7, Duration::from_secs(5), initial_frame.clone())
                .unwrap(),
            StatusPushDecision::Ignored
        );
        assert_eq!(
            state
                .on_render_clock(Duration::from_secs(29), initial_frame)
                .unwrap(),
            StatusPushDecision::Ignored
        );

        let tick = take_batch(
            state
                .on_render_clock(
                    Duration::from_secs(30),
                    frame("sum", "1m", pane(10), "pane 1m"),
                )
                .unwrap(),
        );
        assert_eq!(tick.guarded.writes.len(), 2);
        assert!(
            tick.guarded
                .writes
                .iter()
                .any(|write| matches!(write.key, DisplayOptionKey::SessionAttention { .. }))
        );
        assert!(
            tick.guarded
                .writes
                .iter()
                .any(|write| matches!(write.key, DisplayOptionKey::PaneStatus(_)))
        );
    }

    #[test]
    fn topology_gc_and_pane_removed_clear_all_three_caches() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let old_pane = pane(10);
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "", old_pane.clone(), "old"))
                .unwrap(),
        );
        state.complete_batch(first.id, true).unwrap();

        state.retain_topology_targets(&BTreeSet::new(), &BTreeSet::new());
        assert_eq!(state.desired().len(), 1);
        assert_eq!(state.last_successful().len(), 1);
        assert!(state.dirty().is_empty());

        let new_pane = pane(11);
        let second = take_batch(
            state
                .on_snapshot_revision(
                    2,
                    Duration::from_secs(1),
                    frame("sum", "", new_pane.clone(), "new"),
                )
                .unwrap(),
        );
        assert!(second.guarded.writes.iter().any(
            |write| matches!(&write.key, DisplayOptionKey::PaneStatus(value) if value == &new_pane)
        ));
        state.complete_batch(second.id, false).unwrap();
        state.pane_removed(&new_pane);
        let key = DisplayOptionKey::PaneStatus(new_pane);
        assert!(!state.desired().contains_key(&key));
        assert!(!state.dirty().contains(&key));
        assert!(!state.last_successful().contains_key(&key));
    }

    #[test]
    fn successful_old_batch_does_not_restore_a_removed_target() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let pane = pane(10);
        let batch = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "", pane.clone(), "pane"))
                .unwrap(),
        );
        state.pane_removed(&pane);
        state.complete_batch(batch.id, true).unwrap();
        assert!(
            !state
                .last_successful()
                .contains_key(&DisplayOptionKey::PaneStatus(pane))
        );
    }

    #[test]
    fn shutdown_sets_marker_and_unsets_all_current_attention_options() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "attn", pane(10), "pane"))
                .unwrap(),
        );
        state.complete_batch(first.id, true).unwrap();

        let shutdown = take_batch(
            state
                .request_shutdown(Duration::from_secs(1), "stopped".to_string())
                .unwrap(),
        );
        assert_eq!(shutdown.guarded.writes.len(), 2);
        assert!(shutdown.guarded.writes.iter().any(|write| {
            write.key == DisplayOptionKey::GlobalSummary
                && write.value == DisplayOptionValue::Set("stopped".to_string())
        }));
        assert!(shutdown.guarded.writes.iter().any(|write| {
            matches!(write.key, DisplayOptionKey::SessionAttention { .. })
                && write.value == DisplayOptionValue::Unset
        }));
        assert_eq!(
            state
                .on_snapshot_revision(
                    2,
                    Duration::from_secs(2),
                    frame("running", "attention", pane(10), "pane"),
                )
                .unwrap(),
            StatusPushDecision::Ignored
        );
    }

    #[test]
    fn shutdown_does_not_let_unrelated_dirty_target_block_terminal_options() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "attn", pane(10), "pane"))
                .unwrap(),
        );
        state.complete_batch(first.id, false).unwrap();
        let shutdown = take_batch(
            state
                .request_shutdown(Duration::from_secs(1), "stopped".to_string())
                .unwrap(),
        );
        assert_eq!(shutdown.guarded.writes.len(), 2);
        assert!(shutdown.guarded.writes.iter().all(|write| matches!(
            write.key,
            DisplayOptionKey::GlobalSummary | DisplayOptionKey::SessionAttention { .. }
        )));
    }

    #[test]
    fn failed_shutdown_batch_can_be_retried_without_a_clock_tick() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "attn", pane(10), "pane"))
                .unwrap(),
        );
        state.complete_batch(first.id, true).unwrap();
        let shutdown = take_batch(
            state
                .request_shutdown(Duration::from_secs(1), "stopped".to_string())
                .unwrap(),
        );
        let mut io = FakeIo {
            fail: true,
            ..FakeIo::default()
        };
        assert_eq!(
            state.execute_prepared(&shutdown, &mut io).unwrap(),
            BatchExecution::Failed("tmux failed")
        );
        assert_eq!(
            state.flush_coalesced(Duration::from_secs(1)).unwrap(),
            StatusPushDecision::Coalesced {
                ready_at: Duration::from_secs(2)
            }
        );
        let retry = take_batch(state.flush_coalesced(Duration::from_secs(2)).unwrap());
        assert_eq!(retry.guarded.writes.len(), 2);
    }

    #[test]
    fn shutdown_ignores_unrelated_in_flight_failure() {
        let mut state = StatusPushState::new(server(), Duration::ZERO).unwrap();
        let first = take_batch(
            state
                .on_snapshot_revision(1, Duration::ZERO, frame("sum", "attn", pane(10), "pane"))
                .unwrap(),
        );
        assert_eq!(
            state
                .request_shutdown(Duration::from_secs(1), "stopped".to_string())
                .unwrap(),
            StatusPushDecision::WaitingForInFlight
        );
        state.complete_batch(first.id, false).unwrap();
        let shutdown = take_batch(state.flush_coalesced(Duration::from_secs(1)).unwrap());
        assert_eq!(shutdown.guarded.writes.len(), 2);
        assert!(shutdown.guarded.writes.iter().all(|write| matches!(
            write.key,
            DisplayOptionKey::GlobalSummary | DisplayOptionKey::SessionAttention { .. }
        )));
    }
}

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::daemon::topology::ServerIdentity;
use crate::tmux::TmuxRunner;

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_START_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_RUNTIME_LOG_BYTES: u64 = 1024 * 1024;
const MAX_RUNTIME_LOG_LINE_BYTES: usize = 8 * 1024;
const LIFECYCLE_RECORD_VERSION: u16 = 1;
const LIFECYCLE_RECORD_FILE: &str = "lifecycle.json";
pub const DISABLED_SERVER_OPTION: &str = "@vde_daemon_disabled";
pub const EXECUTABLE_OPTION: &str = "@vde_executable";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DesiredMode {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleHealth {
    Stable,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonProcessIdentity {
    pub pid: u32,
    pub start_token: String,
    pub daemon_instance_id: String,
    pub socket_device: u64,
    pub socket_inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationProcessIdentity {
    pub process_group_id: i32,
    pub leader_start_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleRecord {
    pub version: u16,
    pub server_identity: String,
    pub desired_mode: DesiredMode,
    pub generation: u64,
    pub health: LifecycleHealth,
    pub last_transition_error: Option<String>,
    pub process: Option<DaemonProcessIdentity>,
    pub active_notification: Option<NotificationProcessIdentity>,
    pub updated_at_epoch_seconds: u64,
}

impl LifecycleRecord {
    pub fn initial(server_identity: impl Into<String>) -> Self {
        Self {
            version: LIFECYCLE_RECORD_VERSION,
            server_identity: server_identity.into(),
            desired_mode: DesiredMode::Enabled,
            generation: 0,
            health: LifecycleHealth::Stable,
            last_transition_error: None,
            process: None,
            active_notification: None,
            updated_at_epoch_seconds: epoch_seconds(),
        }
    }

    pub fn begin_transition(&mut self, desired_mode: DesiredMode) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("lifecycle generation overflow"))?;
        self.desired_mode = desired_mode;
        self.health = LifecycleHealth::Stable;
        self.last_transition_error = None;
        self.updated_at_epoch_seconds = epoch_seconds();
        Ok(())
    }

    pub fn degrade(&mut self, error: impl Into<String>) {
        self.health = LifecycleHealth::Degraded;
        self.last_transition_error = Some(bounded_log_message(&error.into()));
        self.updated_at_epoch_seconds = epoch_seconds();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxServerIncarnation {
    pub socket_path: PathBuf,
    pub identity: ServerIdentity,
    pub hash: String,
}

impl TmuxServerIncarnation {
    pub fn resolve_from_runner(runner: &dyn TmuxRunner) -> Result<Self> {
        let output = runner.run(&[
            "display-message",
            "-p",
            "#{pid}\t#{start_time}\t#{socket_path}",
        ])?;
        Self::from_display_output(&output)
    }

    pub fn resolve(runner: &dyn TmuxRunner, env: &BTreeMap<String, String>) -> Result<Self> {
        let tmux = env
            .get("TMUX")
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("TMUX is required to identify the tmux server"))?;
        let socket_path = tmux
            .split(',')
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("TMUX has an invalid server socket path"))?;
        let actual = Self::resolve_from_runner(runner)?;
        let socket_path = std::fs::canonicalize(socket_path)
            .with_context(|| format!("failed to canonicalize tmux socket path {socket_path}"))?;
        if actual.socket_path != socket_path {
            bail!(
                "tmux runner targets {}, but TMUX identifies {}",
                actual.socket_path.display(),
                socket_path.display()
            );
        }
        Ok(actual)
    }

    fn from_display_output(output: &str) -> Result<Self> {
        let mut fields = output.trim_end().split('\t');
        let pid = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| anyhow::anyhow!("tmux returned an invalid server PID"))?;
        let start_time = fields
            .next()
            .and_then(|value| value.parse::<i64>().ok())
            .ok_or_else(|| anyhow::anyhow!("tmux returned an invalid server start time"))?;
        let reported_socket = fields
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("tmux returned an invalid server socket path"))?;
        if fields.next().is_some() {
            bail!("tmux returned an invalid server incarnation");
        }
        let socket_path = std::fs::canonicalize(reported_socket).with_context(|| {
            format!("failed to canonicalize reported tmux socket path {reported_socket}")
        })?;
        let identity = ServerIdentity { pid, start_time };
        let mut hasher = Sha256::new();
        hasher.update(socket_path.as_os_str().as_bytes());
        hasher.update([0]);
        hasher.update(pid.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(start_time.to_string().as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        Ok(Self {
            socket_path,
            identity,
            hash,
        })
    }

    pub fn verify(&self, runner: &dyn TmuxRunner, env: &BTreeMap<String, String>) -> Result<()> {
        let actual = Self::resolve(runner, env)?;
        if actual != *self {
            bail!(
                "tmux server incarnation mismatch: expected {}, received {}",
                self.hash,
                actual.hash
            );
        }
        Ok(())
    }

    pub fn verify_from_runner(&self, runner: &dyn TmuxRunner) -> Result<()> {
        let actual = Self::resolve_from_runner(runner)?;
        if actual != *self {
            bail!(
                "tmux server incarnation mismatch: expected {}, received {}",
                self.hash,
                actual.hash
            );
        }
        Ok(())
    }
}

pub fn tmux_desired_mode(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<DesiredMode> {
    let incarnation = TmuxServerIncarnation::resolve(runner, env)?;
    let value = match runner.run(&["show-option", "-gqv", DISABLED_SERVER_OPTION]) {
        Ok(value) => value,
        #[cfg(test)]
        Err(error) if error.to_string().contains("no stub registered") => String::new(),
        Err(error) => return Err(error),
    };
    incarnation.verify(runner, env)?;
    Ok(if value.trim() == "1" {
        DesiredMode::Disabled
    } else {
        DesiredMode::Enabled
    })
}

pub fn set_tmux_desired_mode(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    desired_mode: DesiredMode,
) -> Result<()> {
    let incarnation = TmuxServerIncarnation::resolve(runner, env)?;
    set_tmux_desired_mode_for_incarnation(runner, &incarnation, desired_mode)
}

pub(crate) fn set_tmux_desired_mode_for_incarnation(
    runner: &dyn TmuxRunner,
    incarnation: &TmuxServerIncarnation,
    desired_mode: DesiredMode,
) -> Result<()> {
    const SERVER_MISMATCH: &str = "__vde_daemon_mode_server_mismatch__";
    let command = match desired_mode {
        DesiredMode::Disabled => vec![
            "set-option".to_string(),
            "-g".to_string(),
            DISABLED_SERVER_OPTION.to_string(),
            "1".to_string(),
        ],
        DesiredMode::Enabled => vec![
            "set-option".to_string(),
            "-gu".to_string(),
            DISABLED_SERVER_OPTION.to_string(),
        ],
    };
    let guarded = crate::pane_state::store::server_guarded_command_args(
        incarnation.identity.pid,
        incarnation.identity.start_time,
        crate::pane_state::store::tmux_command_string(&command),
        SERVER_MISMATCH,
    );
    let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
    let output = runner.run(&refs)?;
    if output.lines().any(|line| line.trim() == SERVER_MISMATCH) {
        bail!("tmux server incarnation changed while updating daemon mode");
    }
    incarnation.verify_from_runner(runner)?;
    Ok(())
}

pub(crate) fn publish_current_executable(
    runner: &dyn TmuxRunner,
    incarnation: &TmuxServerIncarnation,
) -> Result<PathBuf> {
    const SERVER_MISMATCH: &str = "__vde_executable_server_mismatch__";
    let executable = std::fs::canonicalize(
        std::env::current_exe().context("failed to resolve current executable")?,
    )
    .context("failed to canonicalize current executable")?;
    let executable_value = executable
        .to_str()
        .context("current executable path is not valid UTF-8")?;
    let command = crate::pane_state::store::tmux_command_string(&[
        "set-option".to_string(),
        "-g".to_string(),
        EXECUTABLE_OPTION.to_string(),
        executable_value.to_string(),
    ]);
    let guarded = crate::pane_state::store::server_guarded_command_args(
        incarnation.identity.pid,
        incarnation.identity.start_time,
        command,
        SERVER_MISMATCH,
    );
    let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
    let output = runner.run(&refs)?;
    if output.lines().any(|line| line.trim() == SERVER_MISMATCH) {
        bail!("tmux server incarnation changed while publishing the vde-tmux executable");
    }
    Ok(executable)
}

/// Ensure the daemon socket directory is private and owned by the current user.
///
/// This rejects symlinks and loose permissions, but it is still a best-effort
/// TOCTOU check around normal filesystem operations.
pub fn ensure_secure_socket_dir(path: &Path) -> Result<()> {
    crate::runtime_dir::ensure_secure_runtime_dir(path)
}

pub fn incarnation_log_directory(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
) -> PathBuf {
    let state_root = env
        .get("XDG_STATE_HOME")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .filter(|value| !value.trim().is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/vde-tmux-{}", unsafe { libc::geteuid() })));
    state_root.join("vde-tmux").join(incarnation_hash)
}

pub fn incarnation_state_path(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
    file_name: &str,
) -> PathBuf {
    incarnation_log_directory(env, incarnation_hash).join(file_name)
}

pub fn daemon_log_path(env: &BTreeMap<String, String>, incarnation_hash: &str) -> PathBuf {
    incarnation_state_path(env, incarnation_hash, "daemon.log")
}

pub fn append_daemon_log(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
    message: &str,
) -> Result<PathBuf> {
    let directory = incarnation_log_directory(env, incarnation_hash);
    ensure_private_log_directory(&directory)?;
    let path = daemon_log_path(env, incarnation_hash);
    rotate_runtime_log_if_needed(&path)?;
    let mut file = open_secure_runtime_log(&path)?;
    use std::io::Write as _;
    let message = bounded_log_message(message);
    writeln!(file, "{} {message}", epoch_seconds())?;
    Ok(path)
}

pub fn lifecycle_record_path(env: &BTreeMap<String, String>, incarnation_hash: &str) -> PathBuf {
    incarnation_state_path(env, incarnation_hash, LIFECYCLE_RECORD_FILE)
}

/// Read lifecycle state without creating directories, locks, or marker files.
pub fn read_lifecycle_record(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
) -> Result<LifecycleRecord> {
    let path = lifecycle_record_path(env, incarnation_hash);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LifecycleRecord::initial(incarnation_hash));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to stat {}", path.display()));
        }
    };
    validate_private_directory_read_only(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("lifecycle record has no parent"))?,
    )?;
    validate_private_regular_file(&path, &metadata, 0o600)?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read lifecycle record {}", path.display()))?;
    let record: LifecycleRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid lifecycle record {}", path.display()))?;
    validate_lifecycle_record(&record, incarnation_hash)?;
    Ok(record)
}

pub fn update_lifecycle_record<T>(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
    update: impl FnOnce(&mut LifecycleRecord) -> Result<T>,
) -> Result<T> {
    let directory = incarnation_log_directory(env, incarnation_hash);
    ensure_private_log_directory(&directory)?;
    let lock_path = directory.join("lifecycle.lock");
    let lock = loop {
        if let Some(lock) = try_lock_file(&lock_path)? {
            break lock;
        }
        thread::sleep(DAEMON_START_POLL_INTERVAL);
    };
    let mut record = read_lifecycle_record(env, incarnation_hash)?;
    let result = update(&mut record)?;
    record.updated_at_epoch_seconds = epoch_seconds();
    write_lifecycle_record_atomic(env, &record)?;
    drop(lock);
    Ok(result)
}

pub fn write_lifecycle_record_atomic(
    env: &BTreeMap<String, String>,
    record: &LifecycleRecord,
) -> Result<()> {
    validate_lifecycle_record(record, &record.server_identity)?;
    let directory = incarnation_log_directory(env, &record.server_identity);
    ensure_private_log_directory(&directory)?;
    let path = directory.join(LIFECYCLE_RECORD_FILE);
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        validate_private_regular_file(&path, &metadata, 0o600)?;
    }
    let temporary = directory.join(format!(
        ".lifecycle.{}.{:x}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    let write_result = (|| -> Result<()> {
        use std::io::Write as _;
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        File::open(&directory)?.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

fn validate_lifecycle_record(record: &LifecycleRecord, incarnation_hash: &str) -> Result<()> {
    if record.version != LIFECYCLE_RECORD_VERSION {
        bail!("unsupported lifecycle record version {}", record.version);
    }
    if record.server_identity != incarnation_hash {
        bail!(
            "lifecycle record server mismatch: expected {incarnation_hash}, received {}",
            record.server_identity
        );
    }
    if let Some(process) = &record.process {
        if process.pid == 0 {
            bail!("lifecycle record contains PID 0");
        }
        crate::pane_state::DaemonInstanceId::parse(process.daemon_instance_id.clone())
            .context("lifecycle record contains invalid daemon instance ID")?;
        if process.start_token.trim().is_empty() {
            bail!("lifecycle record contains an empty process start token");
        }
    }
    if let Some(notification) = &record.active_notification
        && (notification.process_group_id <= 0 || notification.leader_start_token.trim().is_empty())
    {
        bail!("lifecycle record contains invalid notification process identity");
    }
    Ok(())
}

fn validate_private_regular_file(
    path: &Path,
    metadata: &std::fs::Metadata,
    required_mode: u32,
) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("private state is not a regular file: {}", path.display());
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        bail!(
            "private state owner mismatch for {}: expected uid {}, got {}",
            path.display(),
            uid,
            metadata.uid()
        );
    }
    if metadata.permissions().mode() & 0o777 != required_mode {
        bail!(
            "private state mode mismatch for {}: expected {:o}, got {:o}",
            path.display(),
            required_mode,
            metadata.permissions().mode() & 0o777
        );
    }
    Ok(())
}

fn validate_private_directory_read_only(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat private state directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "private state directory is not a directory: {}",
            path.display()
        );
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid || metadata.permissions().mode() & 0o777 != 0o700 {
        bail!(
            "private state directory is not owned mode 0700: {}",
            path.display()
        );
    }
    Ok(())
}

pub fn process_start_token(pid: u32) -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .with_context(|| format!("failed to read process identity for PID {pid}"))?;
        let after_name = stat
            .rfind(") ")
            .map(|index| &stat[index + 2..])
            .ok_or_else(|| anyhow::anyhow!("invalid /proc stat for PID {pid}"))?;
        let start = after_name
            .split_whitespace()
            .nth(19)
            .ok_or_else(|| anyhow::anyhow!("missing process start time for PID {pid}"))?;
        Ok(start.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .with_context(|| format!("failed to inspect process identity for PID {pid}"))?;
        if !output.status.success() {
            bail!("process PID {pid} is not available");
        }
        let token = String::from_utf8(output.stdout)
            .context("process start identity was not UTF-8")?
            .trim()
            .to_string();
        if token.is_empty() {
            bail!("process PID {pid} has no start identity");
        }
        Ok(token)
    }
}

pub fn process_identity_is_alive(identity: &DaemonProcessIdentity) -> bool {
    process_start_token(identity.pid).is_ok_and(|token| token == identity.start_token)
        && !process_is_zombie(identity.pid)
}

pub fn terminate_active_notification(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
) -> Result<bool> {
    let record = read_lifecycle_record(env, incarnation_hash)?;
    let Some(identity) = record.active_notification else {
        return Ok(false);
    };
    let leader_pid = u32::try_from(identity.process_group_id)
        .context("notification process group ID does not fit a PID")?;
    match process_start_token(leader_pid) {
        Ok(start_token) if start_token == identity.leader_start_token => {}
        Ok(_) => bail!(
            "refusing to signal notification process group {}: leader identity changed",
            identity.process_group_id
        ),
        Err(_) => {
            update_lifecycle_record(env, incarnation_hash, |record| {
                if record.active_notification.as_ref() == Some(&identity) {
                    record.active_notification = None;
                }
                Ok(())
            })?;
            return Ok(false);
        }
    }
    if unsafe { libc::kill(identity.process_group_id, libc::SIGSTOP) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "failed to suspend notification process group leader {}",
                identity.process_group_id
            )
        });
    }
    if !process_start_token(leader_pid)
        .is_ok_and(|start_token| start_token == identity.leader_start_token)
    {
        let _ = unsafe { libc::kill(identity.process_group_id, libc::SIGCONT) };
        bail!(
            "refusing to signal notification process group {}: leader identity changed after suspension",
            identity.process_group_id
        );
    }
    if unsafe { libc::kill(-identity.process_group_id, libc::SIGKILL) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            let _ = unsafe { libc::kill(identity.process_group_id, libc::SIGCONT) };
            return Err(error).with_context(|| {
                format!(
                    "failed to signal notification process group {}",
                    identity.process_group_id
                )
            });
        }
    }
    update_lifecycle_record(env, incarnation_hash, |record| {
        if record.active_notification.as_ref() == Some(&identity) {
            record.active_notification = None;
        }
        Ok(())
    })?;
    Ok(true)
}

fn process_is_zombie(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rfind(") ").map(|index| stat[index + 2..].to_string()))
            .and_then(|after_name| after_name.split_whitespace().next().map(str::to_string))
            .is_some_and(|state| state == "Z")
    }
    #[cfg(not(target_os = "linux"))]
    {
        Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z')
            })
    }
}

pub fn daemon_process_identity(
    socket: &Path,
    daemon_instance_id: &crate::pane_state::DaemonInstanceId,
) -> Result<DaemonProcessIdentity> {
    let pid = std::process::id();
    let metadata = std::fs::symlink_metadata(socket)
        .with_context(|| format!("failed to stat daemon socket {}", socket.display()))?;
    if !metadata.file_type().is_socket() || metadata.file_type().is_symlink() {
        bail!("daemon socket identity is not a Unix socket");
    }
    Ok(DaemonProcessIdentity {
        pid,
        start_token: process_start_token(pid)?,
        daemon_instance_id: daemon_instance_id.as_str().to_string(),
        socket_device: metadata.dev(),
        socket_inode: metadata.ino(),
    })
}

pub fn verify_force_stop_identity(
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
    socket: &Path,
    expected: &DaemonProcessIdentity,
) -> Result<()> {
    let current_record = read_lifecycle_record(env, incarnation_hash)?;
    let current = current_record
        .process
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("lifecycle record has no daemon process identity"))?;
    if current != expected {
        bail!("daemon process identity changed before force-stop");
    }
    let metadata = std::fs::symlink_metadata(socket)
        .with_context(|| format!("failed to stat daemon socket {}", socket.display()))?;
    if !metadata.file_type().is_socket()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.dev() != current.socket_device
        || metadata.ino() != current.socket_inode
    {
        bail!("daemon socket identity changed before force-stop");
    }
    let start_token = process_start_token(current.pid)?;
    if start_token != current.start_token {
        bail!("daemon PID start identity changed before force-stop");
    }
    crate::pane_state::DaemonInstanceId::parse(current.daemon_instance_id.clone())
        .context("daemon instance identity is invalid")?;
    Ok(())
}

pub fn remove_force_stopped_socket(socket: &Path, expected: &DaemonProcessIdentity) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to stat daemon socket {}", socket.display()));
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.dev() != expected.socket_device
        || metadata.ino() != expected.socket_inode
    {
        bail!(
            "refusing to remove replaced daemon socket {}",
            socket.display()
        );
    }
    std::fs::remove_file(socket)
        .with_context(|| format!("failed to remove daemon socket {}", socket.display()))?;
    if let Some(parent) = socket.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub fn config_hash(config: &crate::config::Config) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{config:#?}").as_bytes());
    format!("{:x}", hasher.finalize())
}

fn ensure_private_log_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "runtime log path is not a private directory: {}",
            path.display()
        );
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        bail!(
            "runtime log directory owner mismatch for {}: expected uid {}, got {}",
            path.display(),
            uid,
            metadata.uid()
        );
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }
    Ok(())
}

fn rotate_runtime_log_if_needed(path: &Path) -> Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    validate_runtime_log_metadata(path, &metadata)?;
    if metadata.len() < MAX_RUNTIME_LOG_BYTES {
        return Ok(());
    }
    let rotated = path.with_extension(format!(
        "{}.1",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("log")
    ));
    if let Ok(rotated_metadata) = std::fs::symlink_metadata(&rotated) {
        validate_runtime_log_metadata(&rotated, &rotated_metadata)?;
        std::fs::remove_file(&rotated)
            .with_context(|| format!("failed to remove {}", rotated.display()))?;
    }
    std::fs::rename(path, &rotated)
        .with_context(|| format!("failed to rotate {}", path.display()))?;
    Ok(())
}

fn open_secure_runtime_log(path: &Path) -> Result<File> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_runtime_log_metadata(path, &metadata)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open runtime log {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat runtime log {}", path.display()))?;
    validate_runtime_log_metadata(path, &metadata)?;
    if metadata.permissions().mode() & 0o777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod runtime log {}", path.display()))?;
    }
    Ok(file)
}

fn validate_runtime_log_metadata(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("runtime log is not a regular file: {}", path.display());
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        bail!(
            "runtime log owner mismatch for {}: expected uid {}, got {}",
            path.display(),
            uid,
            metadata.uid()
        );
    }
    Ok(())
}

fn bounded_log_message(message: &str) -> String {
    let sanitized = message.replace(['\r', '\n'], "\\n");
    if sanitized.len() <= MAX_RUNTIME_LOG_LINE_BYTES {
        return sanitized;
    }
    let mut end = MAX_RUNTIME_LOG_LINE_BYTES;
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &sanitized[..end])
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub fn probe_v2_daemon(
    socket: &Path,
    expected_server_identity: &str,
) -> Option<crate::daemon::protocol::v2::DaemonPhase> {
    probe_v2_daemon_until(
        socket,
        expected_server_identity,
        Instant::now() + Duration::from_millis(150),
    )
    .ok()
    .flatten()
}

fn probe_v2_daemon_until(
    socket: &Path,
    expected_server_identity: &str,
    deadline: Instant,
) -> Result<Option<crate::daemon::protocol::v2::DaemonPhase>> {
    if deadline <= Instant::now() {
        return Ok(None);
    }
    match crate::daemon::protocol::v2::V2Client::connect_with_timeout(
        socket,
        expected_server_identity,
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(150)),
    ) {
        Ok(client) => Ok(Some(client.phase())),
        Err(error) if crate::daemon::protocol::v2::is_protocol_version_mismatch(&error) => {
            Err(incompatible_daemon_error(error))
        }
        Err(_) => Ok(None),
    }
}

fn incompatible_daemon_error(error: anyhow::Error) -> anyhow::Error {
    error.context(
        "incompatible daemon is already running; stop it with the previously installed binary before replacing or starting this version",
    )
}

pub fn ensure_daemon_live_v2(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    ensure_daemon_live_v2_until(
        runner,
        env,
        explicit_socket,
        Instant::now() + DAEMON_START_TIMEOUT,
    )
}

pub fn ensure_daemon_live_v2_until(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
    deadline: Instant,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    ensure_deadline_remaining(deadline, "resolving tmux server incarnation")?;
    if tmux_desired_mode(runner, env)? == DesiredMode::Disabled {
        bail!("daemon is disabled for the current tmux server");
    }
    let incarnation = TmuxServerIncarnation::resolve(runner, env)?;
    incarnation.verify(runner, env)?;
    ensure_daemon_live_v2_for_incarnation_until(incarnation, env, explicit_socket, deadline)
}

pub fn ensure_daemon_live_v2_for_incarnation_until(
    incarnation: TmuxServerIncarnation,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
    deadline: Instant,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    ensure_daemon_live_v2_for_incarnation_until_mode(
        incarnation,
        env,
        explicit_socket,
        deadline,
        true,
    )
}

fn ensure_daemon_live_v2_for_incarnation_until_mode(
    incarnation: TmuxServerIncarnation,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
    deadline: Instant,
    honor_disabled: bool,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    ensure_deadline_remaining(deadline, "checking daemon liveness")?;
    if honor_disabled
        && read_lifecycle_record(env, &incarnation.hash)?.desired_mode == DesiredMode::Disabled
    {
        bail!("daemon is disabled for tmux server {}", incarnation.hash);
    }
    let socket =
        crate::daemon::daemon_socket_path_for_incarnation(env, explicit_socket, &incarnation.hash);
    if probe_v2_daemon_until(&socket, &incarnation.hash, deadline)?.is_some() {
        return Ok((incarnation, socket));
    }
    if let Some(parent) = socket.parent().filter(|path| !path.as_os_str().is_empty()) {
        ensure_deadline_remaining(deadline, "creating daemon socket directory")?;
        ensure_secure_socket_dir(parent)?;
    }
    ensure_deadline_remaining(deadline, "acquiring daemon start lock")?;
    let _start_lock = acquire_daemon_start_lock_until(&socket, deadline)?;
    if probe_v2_daemon_until(&socket, &incarnation.hash, deadline)?.is_some() {
        return Ok((incarnation, socket));
    }
    ensure_deadline_remaining(deadline, "acquiring daemon instance lock")?;
    let stale_guard = try_acquire_daemon_instance_lock(&socket)?;
    if stale_guard.is_none() {
        loop {
            if probe_v2_daemon_until(&socket, &incarnation.hash, deadline)?.is_some() {
                return Ok((incarnation, socket));
            }
            if Instant::now() >= deadline {
                bail!(
                    "daemon instance lock is held but v2 socket is not responsive before deadline"
                );
            }
            sleep_with_deadline(deadline);
        }
    }
    if socket.exists() {
        ensure_deadline_remaining(deadline, "verifying stale daemon socket")?;
        verify_stale_socket_can_be_removed(&socket, deadline)?;
        ensure_deadline_remaining(deadline, "removing stale daemon socket")?;
        std::fs::remove_file(&socket)
            .with_context(|| format!("failed to remove stale socket {}", socket.display()))?;
    }
    drop(stale_guard);
    ensure_deadline_remaining(deadline, "spawning daemon")?;
    let startup_generation = read_lifecycle_record(env, &incarnation.hash)?.generation;
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(&exe);
    command
        .arg("daemon")
        .arg("--socket")
        .arg(&socket)
        .arg("--server-identity")
        .arg(&incarnation.hash)
        .arg("--server-pid")
        .arg(incarnation.identity.pid.to_string())
        .arg("--server-start-time")
        .arg(incarnation.identity.start_time.to_string())
        .arg("--tmux-server-socket")
        .arg(&incarnation.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    ensure_deadline_remaining(deadline, "spawning daemon")?;
    let mut child = command.spawn().map_err(|error| {
        let message = format!(
            "failed to spawn v2 daemon {} --socket {}: {error}",
            exe.display(),
            socket.display()
        );
        let _ = append_daemon_log(
            env,
            &incarnation.hash,
            &format!("daemon startup failed: {message}"),
        );
        anyhow::Error::new(error).context(message)
    })?;
    let child_start_token = match process_start_token(child.id()) {
        Ok(token) => token,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("failed to record spawned daemon process identity");
        }
    };
    loop {
        if probe_v2_daemon_until(&socket, &incarnation.hash, deadline)?.is_some() {
            return Ok((incarnation, socket));
        }
        if let Some(status) = child.try_wait()? {
            let log_path = daemon_log_path(env, &incarnation.hash);
            if let Ok(record) = read_lifecycle_record(env, &incarnation.hash)
                && let Some(error) = startup_failure_for_generation(&record, startup_generation)
            {
                bail!(
                    "v2 daemon exited before becoming live: {error}; see {}",
                    log_path.display()
                );
            }
            bail!(
                "v2 daemon exited with status {status} before becoming live; see {}",
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            let log_path = daemon_log_path(env, &incarnation.hash);
            terminate_timed_out_spawn(
                &mut child,
                &child_start_token,
                env,
                &incarnation.hash,
                &socket,
            );
            bail!(
                "v2 daemon did not become live at {} before the caller deadline ({:?} maximum); see {}",
                socket.display(),
                DAEMON_START_TIMEOUT,
                log_path.display()
            );
        }
        sleep_with_deadline(deadline);
    }
}

fn terminate_timed_out_spawn(
    child: &mut std::process::Child,
    expected_start_token: &str,
    env: &BTreeMap<String, String>,
    incarnation_hash: &str,
    socket: &Path,
) {
    if !process_start_token(child.id()).is_ok_and(|token| token == expected_start_token) {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
    if let Ok(record) = read_lifecycle_record(env, incarnation_hash)
        && let Some(process) = record.process
        && process.pid == child.id()
        && process.start_token == expected_start_token
    {
        let _ = remove_force_stopped_socket(socket, &process);
    }
    let _ = update_lifecycle_record(env, incarnation_hash, |record| {
        if record.process.as_ref().is_some_and(|process| {
            process.pid == child.id() && process.start_token == expected_start_token
        }) {
            record.process = None;
        }
        record.degrade("daemon startup timed out and the spawned process was terminated");
        Ok(())
    });
}

pub fn start_daemon_serving_v2_while_disabled(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    let incarnation = TmuxServerIncarnation::resolve(runner, env)?;
    incarnation.verify(runner, env)?;
    let (incarnation, socket) = ensure_daemon_live_v2_for_incarnation_until_mode(
        incarnation,
        env,
        explicit_socket,
        deadline,
        false,
    )?;
    let startup_generation = read_lifecycle_record(env, &incarnation.hash)?.generation;
    loop {
        if probe_v2_daemon_until(&socket, &incarnation.hash, deadline)?
            == Some(crate::daemon::protocol::v2::DaemonPhase::Serving)
        {
            return Ok((incarnation, socket));
        }
        if let Ok(record) = read_lifecycle_record(env, &incarnation.hash)
            && let Some(error) = startup_failure_for_generation(&record, startup_generation)
        {
            bail!("v2 daemon exited before Serving: {error}");
        }
        if Instant::now() >= deadline {
            terminate_recorded_startup(runner, env, &incarnation, &socket);
            bail!("v2 daemon did not enter Serving before the enable deadline");
        }
        sleep_with_deadline(deadline);
    }
}

fn terminate_recorded_startup(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    incarnation: &TmuxServerIncarnation,
    socket: &Path,
) {
    if incarnation.verify(runner, env).is_err() {
        return;
    }
    let Ok(record) = read_lifecycle_record(env, &incarnation.hash) else {
        return;
    };
    let Some(process) = record.process else {
        return;
    };
    if verify_force_stop_identity(env, &incarnation.hash, socket, &process).is_err() {
        return;
    }
    if unsafe { libc::kill(process.pid as i32, libc::SIGKILL) } != 0 {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_identity_is_alive(&process) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    if process_identity_is_alive(&process) {
        return;
    }
    let _ = remove_force_stopped_socket(socket, &process);
    let _ = update_lifecycle_record(env, &incarnation.hash, |record| {
        if record.process.as_ref() == Some(&process) {
            record.process = None;
        }
        record.degrade("daemon did not enter Serving and was terminated");
        Ok(())
    });
}

pub fn ensure_daemon_serving_v2(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    ensure_daemon_serving_v2_until(runner, env, explicit_socket, deadline)
}

pub fn ensure_daemon_serving_v2_until(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    explicit_socket: Option<&str>,
    deadline: Instant,
) -> Result<(TmuxServerIncarnation, PathBuf)> {
    let (incarnation, socket) =
        ensure_daemon_live_v2_until(runner, env, explicit_socket, deadline)?;
    let startup_generation = read_lifecycle_record(env, &incarnation.hash)?.generation;
    loop {
        if probe_v2_daemon_until(&socket, &incarnation.hash, deadline)?
            == Some(crate::daemon::protocol::v2::DaemonPhase::Serving)
        {
            return Ok((incarnation, socket));
        }
        if let Ok(record) = read_lifecycle_record(env, &incarnation.hash)
            && let Some(error) = startup_failure_for_generation(&record, startup_generation)
        {
            bail!("v2 daemon exited before Serving: {error}");
        }
        if Instant::now() >= deadline {
            bail!("v2 daemon did not enter Serving before the caller deadline");
        }
        sleep_with_deadline(deadline);
    }
}

fn startup_failure_for_generation(
    record: &LifecycleRecord,
    expected_generation: u64,
) -> Option<&str> {
    (record.generation == expected_generation
        && record.health == LifecycleHealth::Degraded
        && record.process.is_none())
    .then_some(record.last_transition_error.as_deref())
    .flatten()
}

#[derive(Debug)]
pub(crate) struct DaemonFileLock {
    file: File,
}

impl Drop for DaemonFileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn acquire_daemon_start_lock_until(socket: &Path, deadline: Instant) -> Result<DaemonFileLock> {
    let path = daemon_lock_path(socket, ".start.lock");
    loop {
        ensure_deadline_remaining(deadline, "acquiring daemon start lock")?;
        if let Some(lock) = try_lock_file(&path)? {
            return Ok(lock);
        }
        sleep_with_deadline(deadline);
    }
}

fn ensure_deadline_remaining(deadline: Instant, stage: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("daemon lifecycle deadline exceeded while {stage}");
    }
    Ok(())
}

fn sleep_with_deadline(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        thread::sleep(remaining.min(DAEMON_START_POLL_INTERVAL));
    }
}

pub(crate) fn try_acquire_daemon_instance_lock(socket: &Path) -> Result<Option<DaemonFileLock>> {
    try_lock_file(&daemon_lock_path(socket, ".lock"))
}

pub(crate) fn try_acquire_writer_lease(namespace: &Path) -> Result<Option<DaemonFileLock>> {
    try_lock_file(&daemon_lock_path(namespace, ".writer.lock"))
}

fn daemon_lock_path(socket: &Path, suffix: &str) -> PathBuf {
    let mut name = socket
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| OsString::from("daemon.sock"));
    name.push(suffix);
    socket.with_file_name(name)
}

pub(crate) fn verify_stale_socket_can_be_removed(socket: &Path, deadline: Instant) -> Result<()> {
    let metadata = std::fs::symlink_metadata(socket)
        .with_context(|| format!("failed to stat stale socket {}", socket.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to remove symlink at daemon socket {}",
            socket.display()
        );
    }
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid {
        bail!(
            "refusing to remove daemon socket {} owned by uid {}",
            socket.display(),
            metadata.uid()
        );
    }
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to remove non-socket daemon path {}",
            socket.display()
        );
    }
    ensure_deadline_remaining(deadline, "checking stale daemon socket owner")?;
    let mut child = Command::new("lsof")
        .args(["-n", "-t", "--"])
        .arg(socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "cannot verify owner process for stale socket {}",
                socket.display()
            )
        })?;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "timed out while verifying owner process for stale socket {}",
                    socket.display()
                );
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).with_context(|| {
                    format!(
                        "cannot verify owner process for stale socket {}",
                        socket.display()
                    )
                });
            }
        }
    }
    let output = child.wait_with_output().with_context(|| {
        format!(
            "cannot collect owner process output for stale socket {}",
            socket.display()
        )
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty()
        || !(output.status.success() || output.status.code() == Some(1) && stdout.trim().is_empty())
    {
        bail!(
            "cannot verify owner process for stale socket {}: {}",
            socket.display(),
            stderr.trim()
        );
    }
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let pid = line.trim().parse::<i32>().map_err(|_| {
            anyhow::anyhow!(
                "cannot parse owner process for stale socket {}",
                socket.display()
            )
        })?;
        // SAFETY: signal 0 performs a process-existence/permission check only.
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            bail!(
                "refusing to remove daemon socket {} owned by live process {}",
                socket.display(),
                pid
            );
        }
    }
    Ok(())
}

fn try_lock_file(path: &Path) -> Result<Option<DaemonFileLock>> {
    let file = open_lock_file(path)?;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != -1 {
            return Ok(Some(DaemonFileLock { file }));
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Ok(None),
            _ => {
                return Err(error).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_regular_file(path, &metadata, 0o600)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open lock file {}", path.display()))?;
    validate_private_regular_file(path, &file.metadata()?, 0o600)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use sha2::{Digest, Sha256};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_dir(label: &str) -> PathBuf {
        let counter = TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        PathBuf::from(format!("/tmp/vt-{label}-{}-{counter}", std::process::id()))
    }

    #[test]
    fn startup_failure_is_scoped_to_the_current_stopped_generation() {
        let mut record = super::LifecycleRecord::initial("server");
        record.generation = 7;
        record.degrade("daemon runtime exited with error: topology failure");

        assert_eq!(
            super::startup_failure_for_generation(&record, 7),
            Some("daemon runtime exited with error: topology failure")
        );
        assert_eq!(super::startup_failure_for_generation(&record, 6), None);

        record.process = Some(super::DaemonProcessIdentity {
            pid: 42,
            start_token: "token".to_string(),
            daemon_instance_id: "00112233445566778899aabbccddeeff".to_string(),
            socket_device: 1,
            socket_inode: 2,
        });
        assert_eq!(super::startup_failure_for_generation(&record, 7), None);

        record.process = None;
        record.health = super::LifecycleHealth::Stable;
        assert_eq!(super::startup_failure_for_generation(&record, 7), None);

        record.health = super::LifecycleHealth::Degraded;
        record.last_transition_error = None;
        assert_eq!(super::startup_failure_for_generation(&record, 7), None);
    }

    #[test]
    fn publishes_the_canonical_current_executable_for_the_expected_server() {
        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let incarnation = super::TmuxServerIncarnation {
            socket_path: PathBuf::from("/tmp/vde-tmux-publish-executable.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 321,
                start_time: 654,
            },
            hash: "a".repeat(64),
        };
        let command = crate::pane_state::store::tmux_command_string(&[
            "set-option".to_string(),
            "-g".to_string(),
            super::EXECUTABLE_OPTION.to_string(),
            executable.display().to_string(),
        ]);
        let guarded = crate::pane_state::store::server_guarded_command_args(
            incarnation.identity.pid,
            incarnation.identity.start_time,
            command,
            "__vde_executable_server_mismatch__",
        );
        let refs = guarded.iter().map(String::as_str).collect::<Vec<_>>();
        let mock = crate::tmux::mock::MockTmuxRunner::new();
        mock.stub(&refs, "");

        assert_eq!(
            super::publish_current_executable(&mock, &incarnation).unwrap(),
            executable
        );
        assert_eq!(mock.calls(), vec![guarded]);
    }

    #[test]
    fn tmux_server_incarnation_uses_canonical_socket_pid_and_start_time() {
        let root = unique_dir("incarnation");
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("tmux.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mock = crate::tmux::mock::MockTmuxRunner::new();
        mock.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("321\t654\t{}\n", socket.display()),
        );
        let env = BTreeMap::from([("TMUX".to_string(), format!("{},321,0", socket.display()))]);

        let first = super::TmuxServerIncarnation::resolve(&mock, &env).unwrap();
        assert_eq!(first.socket_path, std::fs::canonicalize(&socket).unwrap());
        assert_eq!(first.identity.pid, 321);
        assert_eq!(first.identity.start_time, 654);
        assert_eq!(first.hash.len(), 64);

        let second_mock = crate::tmux::mock::MockTmuxRunner::new();
        second_mock.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("321\t655\t{}\n", socket.display()),
        );
        let second = super::TmuxServerIncarnation::resolve(&second_mock, &env).unwrap();
        assert_ne!(first.hash, second.hash);
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tmux_server_incarnation_rejects_runner_target_mismatch() {
        let root = unique_dir("incarnation-mismatch");
        std::fs::create_dir_all(&root).unwrap();
        let expected = root.join("expected.sock");
        let actual = root.join("actual.sock");
        let expected_listener = UnixListener::bind(&expected).unwrap();
        let actual_listener = UnixListener::bind(&actual).unwrap();
        let mock = crate::tmux::mock::MockTmuxRunner::new();
        mock.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("321\t654\t{}\n", actual.display()),
        );
        let env = BTreeMap::from([("TMUX".to_string(), format!("{},321,0", expected.display()))]);

        let error = super::TmuxServerIncarnation::resolve(&mock, &env).unwrap_err();
        assert!(error.to_string().contains("runner targets"));
        drop(expected_listener);
        drop(actual_listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v2_probe_treats_installing_hooks_as_live() {
        let root = unique_dir("v2-probe");
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            let hello: crate::daemon::protocol::v2::ClientMessage =
                serde_json::from_str(request.trim()).unwrap();
            assert_eq!(
                hello,
                crate::daemon::protocol::v2::ClientMessage::Hello {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION
                }
            );
            serde_json::to_writer(
                &mut stream,
                &crate::daemon::protocol::v2::ServerMessage::HelloAck {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION,
                    daemon_instance_id: crate::pane_state::DaemonInstanceId::parse(
                        "00112233445566778899aabbccddeeff",
                    )
                    .unwrap(),
                    server_identity: "server-hash".to_string(),
                    phase: crate::daemon::protocol::v2::DaemonPhase::InstallingHooks,
                    hook_health: crate::daemon::protocol::v2::HookHealth::Healthy,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        assert_eq!(
            super::probe_v2_daemon_until(
                &socket,
                "server-hash",
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap(),
            Some(crate::daemon::protocol::v2::DaemonPhase::InstallingHooks)
        );
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ensure_fails_immediately_for_an_incompatible_live_daemon() {
        let state_root = unique_dir("incompatible-daemon-state");
        std::fs::create_dir_all(&state_root).unwrap();
        let hash = format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "incompatible-daemon-{}-{}",
                    std::process::id(),
                    TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
                )
                .as_bytes()
            )
        );
        let incarnation = super::TmuxServerIncarnation {
            socket_path: state_root.join("tmux.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 10,
                start_time: 20,
            },
            hash: hash.clone(),
        };
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            state_root.to_string_lossy().into_owned(),
        )]);
        let socket = crate::daemon::daemon_socket_path_for_incarnation(&env, None, &hash);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let hello: crate::daemon::protocol::v2::ClientMessage =
                serde_json::from_str(request.trim()).unwrap();
            assert_eq!(
                hello,
                crate::daemon::protocol::v2::ClientMessage::Hello {
                    proto: crate::daemon::protocol::v2::PROTOCOL_VERSION
                }
            );
            serde_json::to_writer(
                &mut stream,
                &crate::daemon::protocol::v2::ServerMessage::error(
                    crate::daemon::protocol::v2::ErrorCode::UnsupportedProtocol,
                    "protocol version 2 is required",
                    None,
                ),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let started = Instant::now();
        let error = super::ensure_daemon_live_v2_for_incarnation_until(
            incarnation,
            &env,
            None,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(1), "{error:#}");
        assert!(
            error.to_string().contains("incompatible daemon"),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("previously installed binary"),
            "{error:#}"
        );
        assert!(crate::daemon::protocol::v2::is_protocol_version_mismatch(
            &error
        ));
        server.join().unwrap();
        std::fs::remove_file(&socket).unwrap();
        std::fs::remove_dir_all(state_root).unwrap();
    }

    #[test]
    fn expired_v2_deadline_does_not_remove_or_spawn() {
        let root = unique_dir("expired-v2");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let base = root.join("daemon.sock");
        let incarnation = super::TmuxServerIncarnation {
            socket_path: root.join("tmux.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 10,
                start_time: 20,
            },
            hash: format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes())),
        };
        let target = crate::daemon::daemon_socket_path_for_incarnation(
            &BTreeMap::new(),
            base.to_str(),
            &incarnation.hash,
        );
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::set_permissions(
            target.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(&target, "sentinel").unwrap();

        let error = super::ensure_daemon_live_v2_for_incarnation_until(
            incarnation,
            &BTreeMap::new(),
            base.to_str(),
            Instant::now() - Duration::from_millis(1),
        )
        .unwrap_err();

        assert!(error.to_string().contains("deadline exceeded"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "sentinel");
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_socket_verification_rejects_regular_file_and_live_owner() {
        let root = unique_dir("stale-verification");
        std::fs::create_dir_all(&root).unwrap();
        let regular = root.join("regular.sock");
        std::fs::write(&regular, "sentinel").unwrap();
        assert!(
            super::verify_stale_socket_can_be_removed(
                &regular,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err()
            .to_string()
            .contains("non-socket")
        );

        let live = root.join("live.sock");
        let listener = UnixListener::bind(&live).unwrap();
        assert!(
            super::verify_stale_socket_can_be_removed(
                &live,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err()
            .to_string()
            .contains("live process")
        );
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_unowned_unix_socket_can_be_removed() {
        let root = unique_dir("stale-unowned");
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("stale.sock");
        drop(UnixListener::bind(&socket).unwrap());

        super::verify_stale_socket_can_be_removed(&socket, Instant::now() + Duration::from_secs(1))
            .unwrap();

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writer_lease_rejects_second_writer_for_same_namespace() {
        let root = unique_dir("writer-lease");
        std::fs::create_dir_all(&root).unwrap();
        let namespace = root.join("server-incarnation");
        let first = super::try_acquire_writer_lease(&namespace).unwrap();
        assert!(first.is_some());
        let second = super::try_acquire_writer_lease(&namespace).unwrap();
        assert!(second.is_none());
        drop(first);
        assert!(
            super::try_acquire_writer_lease(&namespace)
                .unwrap()
                .is_some()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn distinct_server_incarnations_use_independent_socket_and_writer_lease_namespaces() {
        let root = std::env::temp_dir().join(format!(
            "vde-independent-incarnations-{}-{}",
            std::process::id(),
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let first_hash = "1".repeat(64);
        let second_hash = "2".repeat(64);
        let first_socket =
            crate::daemon::daemon_socket_path_for_incarnation(&BTreeMap::new(), None, &first_hash);
        let second_socket =
            crate::daemon::daemon_socket_path_for_incarnation(&BTreeMap::new(), None, &second_hash);
        let first_namespace = crate::daemon::writer_lease_namespace(&first_hash);
        let second_namespace = crate::daemon::writer_lease_namespace(&second_hash);
        let first_test_namespace = root.join(
            first_namespace
                .strip_prefix("/")
                .expect("runtime namespace is absolute"),
        );
        let second_test_namespace = root.join(
            second_namespace
                .strip_prefix("/")
                .expect("runtime namespace is absolute"),
        );
        std::fs::create_dir_all(first_test_namespace.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second_test_namespace.parent().unwrap()).unwrap();

        assert_ne!(first_socket, second_socket);
        assert_ne!(first_namespace, second_namespace);
        let first = super::try_acquire_writer_lease(&first_test_namespace)
            .unwrap()
            .expect("first server acquires its writer lease");
        let second = super::try_acquire_writer_lease(&second_test_namespace)
            .unwrap()
            .expect("second server acquires an independent writer lease");
        assert!(
            super::try_acquire_writer_lease(&first_test_namespace)
                .unwrap()
                .is_none()
        );
        assert!(
            super::try_acquire_writer_lease(&second_test_namespace)
                .unwrap()
                .is_none()
        );

        drop((first, second));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ensure_secure_socket_dir_creates_private_directory() {
        let dir = unique_dir("sec");

        super::ensure_secure_socket_dir(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ensure_secure_socket_dir_tightens_world_readable_directory() {
        let dir = unique_dir("insec");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        super::ensure_secure_socket_dir(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "loose but owned socket dir should be tightened"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn incarnation_log_is_private_bounded_and_single_line() {
        let root = unique_dir("incarnation-log");
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "a".repeat(64);

        let path = super::append_daemon_log(
            &env,
            &hash,
            &format!(
                "failure\n{}",
                "x".repeat(super::MAX_RUNTIME_LOG_LINE_BYTES * 2)
            ),
        )
        .unwrap();

        let directory_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("failure\\n"));
        assert!(contents.len() < super::MAX_RUNTIME_LOG_LINE_BYTES + 64);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incarnation_log_rejects_symlink_target() {
        let root = unique_dir("incarnation-log-symlink");
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "b".repeat(64);
        let directory = super::incarnation_log_directory(&env, &hash);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = root.join("target");
        std::fs::write(&target, "sentinel").unwrap();
        std::os::unix::fs::symlink(&target, directory.join("daemon.log")).unwrap();

        let error = super::append_daemon_log(&env, &hash, "must not follow symlink").unwrap_err();

        assert!(error.to_string().contains("regular file"));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "sentinel");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lifecycle_record_is_private_atomic_and_read_only_when_absent() {
        let root = unique_dir("lifecycle-record");
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "c".repeat(64);

        let absent = super::read_lifecycle_record(&env, &hash).unwrap();
        assert_eq!(absent.desired_mode, super::DesiredMode::Enabled);
        assert!(
            !root.exists(),
            "read-only lookup must not create state paths"
        );

        super::update_lifecycle_record(&env, &hash, |record| {
            record.begin_transition(super::DesiredMode::Disabled)
        })
        .unwrap();
        let path = super::lifecycle_record_path(&env, &hash);
        let stored = super::read_lifecycle_record(&env, &hash).unwrap();
        assert_eq!(stored.desired_mode, super::DesiredMode::Disabled);
        assert_eq!(stored.generation, 1);
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tmux_disabled_marker_is_authoritative_across_different_state_environments() {
        let root = unique_dir("disabled-marker");
        std::fs::create_dir_all(&root).unwrap();
        let tmux_socket = root.join("tmux.sock");
        let listener = UnixListener::bind(&tmux_socket).unwrap();
        let mock = crate::tmux::mock::MockTmuxRunner::new();
        mock.stub(
            &[
                "display-message",
                "-p",
                "#{pid}\t#{start_time}\t#{socket_path}",
            ],
            &format!("123\t456\t{}\n", tmux_socket.display()),
        );
        mock.stub(
            &["show-option", "-gqv", super::DISABLED_SERVER_OPTION],
            "1\n",
        );
        let first = BTreeMap::from([
            (
                "TMUX".to_string(),
                format!("{},123,0", tmux_socket.display()),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                root.join("a").display().to_string(),
            ),
        ]);
        let second = BTreeMap::from([
            (
                "TMUX".to_string(),
                format!("{},123,0", tmux_socket.display()),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                root.join("b").display().to_string(),
            ),
        ]);

        assert_eq!(
            super::tmux_desired_mode(&mock, &first).unwrap(),
            super::DesiredMode::Disabled
        );
        assert_eq!(
            super::tmux_desired_mode(&mock, &second).unwrap(),
            super::DesiredMode::Disabled
        );
        assert!(!root.join("a").exists());
        assert!(!root.join("b").exists());
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminating_active_notification_kills_its_descendant_group_and_clears_record() {
        let root = unique_dir("notification-shutdown");
        std::fs::create_dir_all(&root).unwrap();
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "9".repeat(64);
        let pid_file = root.join("descendant.pid");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!(
                "sleep 30 & echo $! > '{}'; wait",
                pid_file.display()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut leader = command.spawn().unwrap();
        let identity = super::NotificationProcessIdentity {
            process_group_id: leader.id() as i32,
            leader_start_token: super::process_start_token(leader.id()).unwrap(),
        };
        super::update_lifecycle_record(&env, &hash, |record| {
            record.active_notification = Some(identity.clone());
            Ok(())
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let descendant = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "descendant PID was not written");
            thread::sleep(Duration::from_millis(10));
        };

        assert!(super::terminate_active_notification(&env, &hash).unwrap());
        assert!(!leader.wait().unwrap().success());
        let exit_deadline = Instant::now() + Duration::from_secs(1);
        while super::process_start_token(descendant).is_ok() {
            assert!(
                Instant::now() < exit_deadline,
                "notification descendant survived daemon shutdown"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            super::read_lifecycle_record(&env, &hash)
                .unwrap()
                .active_notification
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lifecycle_record_rejects_symlink_without_overwriting_target() {
        let root = unique_dir("lifecycle-record-symlink");
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "d".repeat(64);
        let directory = super::incarnation_log_directory(&env, &hash);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = root.join("target");
        std::fs::write(&target, "sentinel").unwrap();
        std::os::unix::fs::symlink(&target, super::lifecycle_record_path(&env, &hash)).unwrap();

        let error = super::update_lifecycle_record(&env, &hash, |_| Ok(())).unwrap_err();

        assert!(error.to_string().contains("regular file"));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "sentinel");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn force_stop_identity_rejects_process_start_token_mismatch() {
        let root = unique_dir("force-stop-identity");
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "e".repeat(64);
        let socket = root.join("daemon.sock");
        std::fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let metadata = std::fs::metadata(&socket).unwrap();
        let expected = super::DaemonProcessIdentity {
            pid: std::process::id(),
            start_token: "wrong-start-token".to_string(),
            daemon_instance_id: "00112233445566778899aabbccddeeff".to_string(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
        };
        super::update_lifecycle_record(&env, &hash, |record| {
            record.process = Some(expected.clone());
            Ok(())
        })
        .unwrap();

        let error = super::verify_force_stop_identity(&env, &hash, &socket, &expected).unwrap_err();

        assert!(error.to_string().contains("start identity changed"));
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn force_stop_socket_cleanup_rejects_replacement() {
        let root = unique_dir("force-stop-replaced-socket");
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("daemon.sock");
        let first = UnixListener::bind(&socket).unwrap();
        let metadata = std::fs::metadata(&socket).unwrap();
        let expected = super::DaemonProcessIdentity {
            pid: std::process::id(),
            start_token: super::process_start_token(std::process::id()).unwrap(),
            daemon_instance_id: "00112233445566778899aabbccddeeff".to_string(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
        };
        std::fs::remove_file(&socket).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();

        let error = super::remove_force_stopped_socket(&socket, &expected).unwrap_err();

        assert!(error.to_string().contains("replaced"));
        assert!(socket.exists());
        drop((first, replacement));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_timeout_terminates_spawn_and_removes_only_its_recorded_socket() {
        let root = unique_dir("startup-timeout");
        std::fs::create_dir_all(&root).unwrap();
        let env = BTreeMap::from([("XDG_STATE_HOME".to_string(), root.display().to_string())]);
        let hash = "f".repeat(64);
        let socket = root.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let metadata = std::fs::metadata(&socket).unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start_token = super::process_start_token(child.id()).unwrap();
        let process = super::DaemonProcessIdentity {
            pid: child.id(),
            start_token: start_token.clone(),
            daemon_instance_id: "00112233445566778899aabbccddeeff".to_string(),
            socket_device: metadata.dev(),
            socket_inode: metadata.ino(),
        };
        super::update_lifecycle_record(&env, &hash, |record| {
            record.process = Some(process);
            Ok(())
        })
        .unwrap();

        super::terminate_timed_out_spawn(&mut child, &start_token, &env, &hash, &socket);

        assert!(super::process_start_token(child.id()).is_err());
        assert!(!socket.exists());
        let record = super::read_lifecycle_record(&env, &hash).unwrap();
        assert!(record.process.is_none());
        assert_eq!(record.health, super::LifecycleHealth::Degraded);
        assert!(
            record
                .last_transition_error
                .as_deref()
                .is_some_and(|error| error.contains("startup timed out"))
        );
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }
}

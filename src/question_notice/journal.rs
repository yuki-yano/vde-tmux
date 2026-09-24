//! Home-wide loss journal. Locks are acquired by hook/IO workers, never by mutations.
//! The lock file also checkpoints the epoch, so missing/corrupt JSON cannot reset it.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::pane_state::AgentProcessIdentity;

pub const LOCK_BUDGET: Duration = Duration::from_millis(100);
const MAX_BYTES: usize = 1024 * 1024;
const MAX_DIRTY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalFailure {
    Unavailable,
    Contended,
    Capacity,
    Invalid,
    Expired,
}

type Result<T> = std::result::Result<T, JournalFailure>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirtyReason {
    QuestionHook,
    UnclassifiedHook,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirtyEntry {
    pub home_digest: String,
    pub session_digest: String,
    pub turn_digest: String,
    pub tool_digest: String,
    pub writer: AgentProcessIdentity,
    pub reason: DirtyReason,
    pub created_at: i64,
}

impl DirtyEntry {
    pub fn new(
        home_digest: String,
        identifiers: (Option<&str>, Option<&str>, Option<&str>),
        reason: DirtyReason,
        created_at: i64,
    ) -> Result<Self> {
        let pid = std::process::id();
        let start_token = crate::daemon::lifecycle::agent_process_start_token(pid)
            .map_err(|_| JournalFailure::Unavailable)?;
        Ok(Self {
            home_digest,
            session_digest: super::digest(&identifiers.0),
            turn_digest: super::digest(&identifiers.1),
            tool_digest: super::digest(&identifiers.2),
            writer: AgentProcessIdentity { pid, start_token },
            reason,
            created_at,
        })
    }

    pub fn key(&self) -> String {
        super::digest(&(
            &self.home_digest,
            &self.session_digest,
            &self.turn_digest,
            &self.tool_digest,
        ))
    }

    fn valid(&self, home: &str) -> bool {
        self.home_digest == home
            && self.created_at >= 0
            && self.writer.validate().is_ok()
            && [
                &self.home_digest,
                &self.session_digest,
                &self.turn_digest,
                &self.tool_digest,
            ]
            .into_iter()
            .all(|value| valid_digest(value))
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u16,
    home_digest: String,
    home_dirty_epoch: u64,
    dirty: BTreeMap<String, DirtyEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterState {
    Alive,
    Dead,
    Unknown,
}

pub fn writer_state(process: &AgentProcessIdentity) -> WriterState {
    match crate::daemon::lifecycle::agent_process_start_token(process.pid) {
        Ok(token) if token == process.start_token => WriterState::Alive,
        Ok(_) => WriterState::Dead,
        Err(_) => {
            // SAFETY: signal zero only checks existence; it sends no signal.
            if unsafe { libc::kill(process.pid as i32, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                WriterState::Dead
            } else {
                WriterState::Unknown
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalLocation {
    path: PathBuf,
    home_digest: String,
}

impl JournalLocation {
    pub fn new(env: &BTreeMap<String, String>, home_digest: String) -> Result<Self> {
        if !valid_digest(&home_digest) {
            return Err(JournalFailure::Invalid);
        }
        let root =
            crate::daemon::lifecycle::incarnation_log_directory(env, "codex-question-journal-v1");
        Ok(Self {
            path: root.join(format!("{home_digest}.json")),
            home_digest,
        })
    }

    pub fn root_digest(&self) -> Result<String> {
        crate::pane_state::snapshot::ensure_private_parent(&self.path)
            .map_err(|_| JournalFailure::Unavailable)?;
        let root = self
            .path
            .parent()
            .ok_or(JournalFailure::Invalid)?
            .canonicalize()
            .map_err(|_| JournalFailure::Unavailable)?;
        Ok(super::digest(&root))
    }

    /// Hooks cap acquisition at 100 ms; disk commit uses the caller's hook deadline.
    pub fn lock_hook(&self, deadline: Instant) -> Result<JournalGuard> {
        self.acquire(deadline.min(Instant::now() + LOCK_BUDGET), deadline)
    }

    pub fn lock(&self, deadline: Instant) -> Result<JournalGuard> {
        let deadline = deadline.min(Instant::now() + LOCK_BUDGET);
        self.lock_until(deadline)
    }

    fn lock_until(&self, deadline: Instant) -> Result<JournalGuard> {
        self.acquire(deadline, deadline)
    }

    fn acquire(&self, acquire_deadline: Instant, deadline: Instant) -> Result<JournalGuard> {
        crate::pane_state::snapshot::ensure_private_parent(&self.path)
            .map_err(|_| JournalFailure::Unavailable)?;
        let lock_path = self.path.with_extension("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)
            .map_err(|_| JournalFailure::Unavailable)?;
        crate::pane_state::snapshot::validate_private_file(
            &lock_path,
            &file.metadata().map_err(|_| JournalFailure::Unavailable)?,
        )
        .map_err(|_| JournalFailure::Invalid)?;
        loop {
            if Instant::now() >= acquire_deadline {
                return Err(JournalFailure::Contended);
            }
            // SAFETY: file owns this descriptor. LOCK_NB never blocks the caller.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                _ => return Err(JournalFailure::Unavailable),
            }
        }
        let mut guard = JournalGuard {
            file,
            location: self.clone(),
            deadline,
            checkpoint: None,
            journal: Journal {
                schema_version: 1,
                home_digest: self.home_digest.clone(),
                home_dirty_epoch: 0,
                dirty: BTreeMap::new(),
            },
        };
        guard.load()?;
        Ok(guard)
    }
}

/// May be moved to the serial mutation queue; deadline includes that queue wait.
#[derive(Debug)]
pub struct JournalGuard {
    file: File,
    location: JournalLocation,
    deadline: Instant,
    checkpoint: Option<u64>,
    journal: Journal,
}

impl Drop for JournalGuard {
    fn drop(&mut self) {
        // SAFETY: the owned descriptor is still open here.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub epoch: u64,
    pub veto: bool,
    pub session_dirty: bool,
}

impl JournalGuard {
    pub fn epoch(&self) -> u64 {
        self.journal.home_dirty_epoch
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn is_current(&self) -> bool {
        Instant::now() < self.deadline
    }

    fn check_deadline(&self) -> Result<()> {
        if self.is_current() {
            Ok(())
        } else {
            Err(JournalFailure::Expired)
        }
    }

    fn load(&mut self) -> Result<()> {
        let mut checkpoint = Vec::new();
        (&mut self.file)
            .take(9)
            .read_to_end(&mut checkpoint)
            .map_err(|_| JournalFailure::Unavailable)?;
        let expected = !checkpoint.is_empty();
        let epoch = match checkpoint.as_slice() {
            [] => 0,
            bytes if bytes.len() == 8 => u64::from_be_bytes(bytes.try_into().expect("eight bytes")),
            _ => return Err(JournalFailure::Invalid),
        };
        self.checkpoint = expected.then_some(epoch);
        self.journal.home_dirty_epoch = epoch;
        let loaded = (|| -> Result<Option<Journal>> {
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&self.location.path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(JournalFailure::Unavailable),
            };
            crate::pane_state::snapshot::validate_private_file(
                &self.location.path,
                &file.metadata().map_err(|_| JournalFailure::Unavailable)?,
            )
            .map_err(|_| JournalFailure::Invalid)?;
            let mut bytes = Vec::new();
            file.take((MAX_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| JournalFailure::Unavailable)?;
            if bytes.len() > MAX_BYTES {
                return Err(JournalFailure::Capacity);
            }
            let journal: Journal =
                serde_json::from_slice(&bytes).map_err(|_| JournalFailure::Invalid)?;
            if journal.schema_version != 1
                || journal.home_digest != self.location.home_digest
                || journal.dirty.len() > MAX_DIRTY
                || journal.dirty.iter().any(|(key, entry)| {
                    key != &entry.key() || !entry.valid(&self.location.home_digest)
                })
            {
                return Err(JournalFailure::Invalid);
            }
            Ok(Some(journal))
        })();
        match loaded {
            Ok(Some(journal)) => {
                self.journal = journal;
                if !expected || self.journal.home_dirty_epoch != epoch {
                    self.journal.home_dirty_epoch = self.journal.home_dirty_epoch.max(epoch);
                    self.bump()
                } else {
                    Ok(())
                }
            }
            Ok(None) if !expected => self.save(),
            Ok(None) | Err(JournalFailure::Capacity | JournalFailure::Invalid) => self.bump(),
            Err(error) => Err(error),
        }
    }

    fn save(&mut self) -> Result<()> {
        self.check_deadline()?;
        let bytes = serde_json::to_vec(&self.journal).map_err(|_| JournalFailure::Invalid)?;
        if bytes.len() > MAX_BYTES {
            return Err(JournalFailure::Capacity);
        }
        if self.checkpoint != Some(self.journal.home_dirty_epoch) {
            self.file
                .seek(SeekFrom::Start(0))
                .map_err(|_| JournalFailure::Unavailable)?;
            self.file
                .write_all(&self.journal.home_dirty_epoch.to_be_bytes())
                .map_err(|_| JournalFailure::Unavailable)?;
            self.file
                .set_len(8)
                .map_err(|_| JournalFailure::Unavailable)?;
            self.file
                .sync_all()
                .map_err(|_| JournalFailure::Unavailable)?;
            self.checkpoint = Some(self.journal.home_dirty_epoch);
        }
        let parent = self.location.path.parent().ok_or(JournalFailure::Invalid)?;
        let id = crate::pane_state::EventId::generate().map_err(|_| JournalFailure::Unavailable)?;
        let temp = parent.join(format!(".journal-{}.tmp", id.as_str()));
        let result = (|| -> Result<()> {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temp)
                .map_err(|_| JournalFailure::Unavailable)?;
            output
                .write_all(&bytes)
                .map_err(|_| JournalFailure::Unavailable)?;
            output.sync_all().map_err(|_| JournalFailure::Unavailable)?;
            self.check_deadline()?;
            // Refuse a target symlink even though rename itself would replace the link.
            if let Ok(metadata) = std::fs::symlink_metadata(&self.location.path) {
                crate::pane_state::snapshot::validate_private_file(&self.location.path, &metadata)
                    .map_err(|_| JournalFailure::Invalid)?;
            }
            std::fs::rename(&temp, &self.location.path).map_err(|_| JournalFailure::Unavailable)?;
            File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| JournalFailure::Unavailable)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }

    pub fn bump(&mut self) -> Result<()> {
        self.journal.home_dirty_epoch = self
            .journal
            .home_dirty_epoch
            .checked_add(1)
            .ok_or(JournalFailure::Capacity)?;
        self.save()
    }

    pub fn insert(&mut self, entry: DirtyEntry) -> Result<String> {
        self.check_deadline()?;
        if !entry.valid(&self.location.home_digest) {
            return Err(JournalFailure::Invalid);
        }
        let key = entry.key();
        if self.journal.dirty.contains_key(&key) {
            return Ok(key);
        }
        if self.journal.dirty.len() >= MAX_DIRTY {
            self.bump()?;
            return Err(JournalFailure::Capacity);
        }
        self.journal.dirty.insert(key.clone(), entry);
        if let Err(error) = self.save() {
            self.journal.dirty.remove(&key);
            if error == JournalFailure::Capacity {
                self.bump()?;
            }
            return Err(error);
        }
        Ok(key)
    }

    /// Call only after the exact issue key has a Persisted response.
    pub fn clear_persisted(&mut self, key: &str) -> Result<()> {
        self.check_deadline()?;
        if self.journal.dirty.remove(key).is_some() {
            self.save()?;
        }
        Ok(())
    }

    pub fn evaluate(
        &mut self,
        session_digest: Option<&str>,
        mut liveness: impl FnMut(&AgentProcessIdentity) -> WriterState,
    ) -> Result<Evaluation> {
        self.check_deadline()?;
        let mut abandoned = false;
        let mut session_dirty = false;
        self.journal.dirty.retain(|_, entry| {
            session_dirty |= session_digest == Some(entry.session_digest.as_str());
            if liveness(&entry.writer) == WriterState::Dead {
                abandoned = true;
                false
            } else {
                true
            }
        });
        if abandoned {
            self.bump()?;
        }
        self.check_deadline()?;
        Ok(Evaluation {
            epoch: self.journal.home_dirty_epoch,
            veto: !self.journal.dirty.is_empty(),
            session_dirty,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn fixture() -> (PathBuf, JournalLocation, DirtyEntry) {
        let root = std::env::temp_dir().join(format!(
            "vt-journal-{}",
            crate::pane_state::EventId::generate().unwrap().as_str()
        ));
        let env = BTreeMap::from([(
            "XDG_STATE_HOME".to_string(),
            root.to_string_lossy().to_string(),
        )]);
        let home = super::super::digest(&"synthetic-home");
        let entry = DirtyEntry::new(
            home.clone(),
            (Some("synthetic-session"), Some("turn"), Some("tool")),
            DirtyReason::QuestionHook,
            1,
        )
        .unwrap();
        (root, JournalLocation::new(&env, home).unwrap(), entry)
    }

    fn lock(location: &JournalLocation) -> JournalGuard {
        // State/IO assertions do not depend on host fsync latency; the separate contention
        // test exercises the production 100 ms budget and an explicitly expired guard.
        location
            .lock_until(Instant::now() + Duration::from_secs(10))
            .unwrap()
    }

    #[test]
    fn canonical_state_root_binding_and_hook_io_budget_are_distinct_from_acquisition() {
        let (root, location, entry) = fixture();
        let initial = location.root_digest().unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let alias_env = BTreeMap::from([("XDG_STATE_HOME".into(), alias.to_string_lossy().into())]);
        let same = JournalLocation::new(&alias_env, entry.home_digest.clone()).unwrap();
        assert_eq!(same.root_digest().unwrap(), initial);
        let other_env = BTreeMap::from([(
            "XDG_STATE_HOME".into(),
            root.join("other").to_string_lossy().into(),
        )]);
        let other = JournalLocation::new(&other_env, entry.home_digest.clone()).unwrap();
        assert_ne!(other.root_digest().unwrap(), initial);
        let mut guard = location
            .lock_hook(Instant::now() + Duration::from_secs(2))
            .unwrap();
        let stamp = guard.file.metadata().unwrap().mtime_nsec();
        std::thread::sleep(Duration::from_millis(120));
        let key = guard.insert(entry).unwrap();
        assert_eq!(
            guard.file.metadata().unwrap().mtime_nsec(),
            stamp,
            "unchanged epoch rewrote checkpoint"
        );
        guard.clear_persisted(&key).unwrap();
        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inflight_and_unknown_veto_abandoned_bumps_once_and_persisted_clears() {
        let (root, location, entry) = fixture();
        let mut guard = lock(&location);
        let key = guard.insert(entry.clone()).unwrap();
        for state in [WriterState::Alive, WriterState::Unknown] {
            assert_eq!(
                guard
                    .evaluate(Some(&entry.session_digest), |_| state)
                    .unwrap(),
                Evaluation {
                    epoch: 0,
                    veto: true,
                    session_dirty: true
                }
            );
        }
        drop(guard);
        let mut guard = lock(&location);
        assert_eq!(
            guard.evaluate(None, |_| WriterState::Dead).unwrap().epoch,
            1
        );
        assert_eq!(
            guard.evaluate(None, |_| WriterState::Dead).unwrap().epoch,
            1
        );
        guard.insert(entry).unwrap();
        guard.clear_persisted(&key).unwrap();
        assert!(!guard.evaluate(None, |_| WriterState::Alive).unwrap().veto);
        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn contention_is_bounded_and_expired_queue_guard_cannot_mutate() {
        let (root, location, entry) = fixture();
        let mut guard = lock(&location);
        let started = Instant::now();
        assert!(matches!(
            location.lock(Instant::now() + Duration::from_millis(10)),
            Err(JournalFailure::Contended)
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
        guard.deadline = Instant::now();
        assert_eq!(guard.insert(entry), Err(JournalFailure::Expired));
        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_expected_or_corrupt_journal_never_resets_epoch() {
        let (root, location, _) = fixture();
        lock(&location).bump().unwrap();
        std::fs::remove_file(&location.path).unwrap();
        assert_eq!(
            lock(&location)
                .evaluate(None, |_| WriterState::Alive)
                .unwrap()
                .epoch,
            2
        );
        std::fs::write(&location.path, b"invalid").unwrap();
        assert_eq!(
            lock(&location)
                .evaluate(None, |_| WriterState::Alive)
                .unwrap()
                .epoch,
            3
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_is_private_digest_only_and_rejects_symlinks() {
        let (root, location, entry) = fixture();
        lock(&location).insert(entry).unwrap();
        let bytes = std::fs::read_to_string(&location.path).unwrap();
        for raw in ["synthetic-session", "synthetic-home", "prompt", "answer"] {
            assert!(!bytes.contains(raw));
        }
        let meta = std::fs::metadata(&location.path).unwrap();
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let target = root.join("other");
        std::fs::rename(&location.path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &location.path).unwrap();
        assert!(location.lock(Instant::now() + LOCK_BUDGET).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), bytes);
        std::fs::remove_dir_all(root).unwrap();
    }
}

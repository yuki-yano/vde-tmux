use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::{MAX_KEYS, MAX_KEYS_PER_OWNER, MAX_OWNERS, QuestionNoticeState, QuestionNotices};

const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitResult {
    PreCommitFailed,
    Committed,
    CommittedDurabilityUnknown,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FaultPoint {
    BeforeRename,
    AfterRename,
    DirectorySync,
}

fn marker_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_file_name("question-notices-v1.expected")
}

fn marker_expected(path: &std::path::Path) -> Result<bool> {
    let path = marker_path(path);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    crate::pane_state::snapshot::validate_private_file(&path, &file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(3).read_to_end(&mut bytes)?;
    ensure!(
        bytes == b"1\n",
        "invalid question notice expectation marker"
    );
    Ok(true)
}

fn create_marker(path: &std::path::Path) -> Result<()> {
    if marker_expected(path)? {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(marker_path(path))?;
    file.write_all(b"1\n")?;
    file.sync_all()?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u16,
    server_hash: String,
    owners: Vec<QuestionNoticeState>,
    // An additive on-disk upgrade preserves existing notification watermarks.
    // Missing old metadata never establishes that a question was answered.
    #[serde(default)]
    reply_items: Vec<(String, u64, super::ReplyItems)>,
    #[serde(default)]
    reply_calls: BTreeMap<String, std::collections::BTreeSet<String>>,
    #[serde(default)]
    reply_blocked_owners: std::collections::BTreeSet<String>,
}

impl QuestionNotices {
    pub(super) fn load(&mut self) -> Result<BTreeMap<String, QuestionNoticeState>> {
        let Some(path) = &self.path else {
            return Ok(BTreeMap::new());
        };
        let expected = marker_expected(path)?;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(!expected, "expected question notice sidecar is missing");
                return Ok(BTreeMap::new());
            }
            Err(error) => return Err(error.into()),
        };
        crate::pane_state::snapshot::ensure_private_parent(path)?;
        crate::pane_state::snapshot::validate_private_file(path, &file.metadata()?)?;
        let mut bytes = Vec::new();
        file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "question notice sidecar exceeds limit"
        );
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        ensure!(
            snapshot.schema_version == 1 && snapshot.server_hash == self.server_hash,
            "question notice sidecar identity mismatch"
        );
        ensure!(
            snapshot.owners.len() <= MAX_OWNERS,
            "too many question owners"
        );
        let mut owners = BTreeMap::new();
        let mut keys = 0;
        for owner in snapshot.owners {
            owner.pane.validate()?;
            owner.process.validate()?;
            ensure!(
                owner.owner_ref == self.owner_ref(&owner.pane, &owner.process),
                "invalid notice owner reference"
            );
            ensure!(
                owner.acknowledged_order <= owner.latest_order
                    && owner.latest_order == owner.seen.len() as u64
                    && owner.last_issued_at >= 0,
                "invalid notice order"
            );
            ensure!(
                owner.seen.len() <= MAX_KEYS_PER_OWNER,
                "too many question keys"
            );
            ensure!(
                owner.seen.iter().all(|key| key.len() == 64
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
                "invalid question key"
            );
            keys += owner.seen.len();
            ensure!(keys <= MAX_KEYS, "question keys exceed total limit");
            ensure!(
                owners.insert(owner.owner_ref.clone(), owner).is_none(),
                "duplicate question owner"
            );
        }
        let valid_digest = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        ensure!(
            snapshot.reply_items.len() <= super::MAX_TEXT_NOTICES,
            "too many reply entries"
        );
        let mut replies = BTreeMap::new();
        let mut per_owner = BTreeMap::<String, usize>::new();
        for (owner, order, items) in snapshot.reply_items {
            let state = owners
                .get(&owner)
                .ok_or_else(|| anyhow::anyhow!("unknown reply owner"))?;
            ensure!(
                order > 0
                    && order <= state.latest_order
                    && valid_digest(&items.session)
                    && !items.items.is_empty()
                    && items.items.len() <= super::reply::MAX_ISSUED_ITEMS
                    && items.items.iter().all(|id| valid_digest(id))
                    && items.pending.is_subset(&items.items),
                "invalid reply metadata"
            );
            // A committed prefix can include consumed entries until memory cleanup.
            if order <= state.acknowledged_order {
                continue;
            }
            let count = per_owner.entry(owner.clone()).or_default();
            *count += 1;
            ensure!(
                *count <= super::MAX_TEXT_NOTICES_PER_OWNER,
                "too many owner replies"
            );
            ensure!(
                replies.insert((owner, order), items).is_none(),
                "duplicate reply order"
            );
        }
        let mut calls = 0;
        for (owner, ids) in &snapshot.reply_calls {
            ensure!(
                owners.contains_key(owner)
                    && ids.len() <= MAX_KEYS_PER_OWNER
                    && ids.iter().all(|id| valid_digest(id)),
                "invalid reply call history"
            );
            calls += ids.len();
            ensure!(calls <= MAX_KEYS, "too many reply calls");
        }
        ensure!(
            snapshot
                .reply_blocked_owners
                .iter()
                .all(|owner| owners.contains_key(owner)),
            "invalid blocked reply owner"
        );
        self.reply_items = replies;
        self.reply_calls = snapshot.reply_calls;
        self.reply_blocked_owners = snapshot.reply_blocked_owners;
        Ok(owners)
    }

    pub(super) fn save(&self, deadline: Option<std::time::Instant>) -> CommitResult {
        let _lock = self
            .save_lock
            .lock()
            .expect("question sidecar save lock poisoned");
        let mut renamed = false;
        match self.save_inner(&mut renamed, deadline) {
            Ok(()) => CommitResult::Committed,
            Err(_) if renamed => CommitResult::CommittedDurabilityUnknown,
            Err(_) => CommitResult::PreCommitFailed,
        }
    }

    fn save_inner(&self, renamed: &mut bool, deadline: Option<std::time::Instant>) -> Result<()> {
        ensure!(
            !self.invalid_sidecar,
            "question sidecar needs explicit repair"
        );
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(&Snapshot {
            schema_version: 1,
            server_hash: self.server_hash.clone(),
            owners: self.owners.values().cloned().collect(),
            reply_items: self
                .reply_items
                .iter()
                .map(|((owner, order), items)| (owner.clone(), *order, items.clone()))
                .collect(),
            reply_calls: self.reply_calls.clone(),
            reply_blocked_owners: self.reply_blocked_owners.clone(),
        })?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "question notice sidecar exceeds limit"
        );
        crate::pane_state::snapshot::ensure_private_parent(path)?;
        match std::fs::symlink_metadata(path) {
            Ok(meta) => crate::pane_state::snapshot::validate_private_file(path, &meta)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let parent = path.parent().expect("validated parent");
        let temp = parent.join(format!(
            ".question-notices.{}.tmp",
            crate::pane_state::EventId::generate()?.as_str()
        ));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            #[cfg(test)]
            ensure!(
                self.storage_fault != Some(FaultPoint::BeforeRename),
                "injected precommit failure"
            );
            ensure!(
                deadline.is_none_or(|at| std::time::Instant::now() < at),
                "question commit guard expired"
            );
            std::fs::rename(&temp, path)?;
            *renamed = true;
            #[cfg(test)]
            ensure!(
                self.storage_fault != Some(FaultPoint::AfterRename),
                "injected postcommit failure"
            );
            create_marker(path)?;
            #[cfg(test)]
            ensure!(
                self.storage_fault != Some(FaultPoint::DirectorySync),
                "injected directory sync failure"
            );
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }
}

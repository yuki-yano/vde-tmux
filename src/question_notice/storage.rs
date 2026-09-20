use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::{MAX_KEYS, MAX_KEYS_PER_OWNER, MAX_OWNERS, QuestionNoticeState, QuestionNotices};

const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u16,
    server_hash: String,
    owners: Vec<QuestionNoticeState>,
}

impl QuestionNotices {
    pub(super) fn load(&self) -> Result<BTreeMap<String, QuestionNoticeState>> {
        let Some(path) = &self.path else {
            return Ok(BTreeMap::new());
        };
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
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
        Ok(owners)
    }

    pub(super) fn save(&self) -> Result<()> {
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
            std::fs::rename(&temp, path)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{PaneInstance, PaneState, StoreError};

pub const PANE_SNAPSHOT_SCHEMA_VERSION: u16 = 1;
pub const PANE_SNAPSHOT_FILE: &str = "pane-state-v1.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneStateSnapshot {
    pub schema_version: u16,
    pub server_identity: crate::daemon::topology::ServerIdentity,
    pub records: Vec<PaneState>,
}

pub trait PaneSnapshotStoreIo {
    fn save(&mut self, records: &BTreeMap<PaneInstance, PaneState>) -> Result<(), StoreError>;
}

pub struct FilePaneSnapshotStore {
    path: PathBuf,
    server_identity: crate::daemon::topology::ServerIdentity,
}

impl FilePaneSnapshotStore {
    pub fn new(path: PathBuf, server_identity: crate::daemon::topology::ServerIdentity) -> Self {
        Self {
            path,
            server_identity,
        }
    }
}

impl PaneSnapshotStoreIo for FilePaneSnapshotStore {
    fn save(&mut self, records: &BTreeMap<PaneInstance, PaneState>) -> Result<(), StoreError> {
        save_snapshot(&self.path, &self.server_identity, records)
    }
}

pub fn snapshot_path(env: &BTreeMap<String, String>, incarnation_hash: &str) -> PathBuf {
    crate::daemon::lifecycle::incarnation_state_path(env, incarnation_hash, PANE_SNAPSHOT_FILE)
}

pub fn encode_snapshot(
    server_identity: &crate::daemon::topology::ServerIdentity,
    records: &BTreeMap<PaneInstance, PaneState>,
) -> Result<Vec<u8>, StoreError> {
    validate_records(records)?;
    let encoded = serde_json::to_vec_pretty(&PaneStateSnapshot {
        schema_version: PANE_SNAPSHOT_SCHEMA_VERSION,
        server_identity: server_identity.clone(),
        records: records.values().cloned().collect(),
    })
    .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
    if encoded.len() > super::MAX_RESPONSE_FRAME_BYTES {
        return Err(StoreError::StateTooLarge);
    }
    Ok(encoded)
}

pub fn decode_snapshot(
    encoded: &[u8],
    expected_identity: &crate::daemon::topology::ServerIdentity,
) -> Result<BTreeMap<PaneInstance, PaneState>, StoreError> {
    if encoded.len() > super::MAX_RESPONSE_FRAME_BYTES {
        return Err(StoreError::StateTooLarge);
    }
    let snapshot = serde_json::from_slice::<PaneStateSnapshot>(encoded)
        .map_err(|error| StoreError::PersistFailed(format!("corrupt pane snapshot: {error}")))?;
    if snapshot.schema_version != PANE_SNAPSHOT_SCHEMA_VERSION {
        return Err(StoreError::PersistFailed(format!(
            "unsupported pane snapshot schema version {}",
            snapshot.schema_version
        )));
    }
    if &snapshot.server_identity != expected_identity {
        return Err(StoreError::PersistFailed(
            "pane snapshot server identity does not match current tmux server".to_string(),
        ));
    }
    let mut records = BTreeMap::new();
    for state in snapshot.records {
        state
            .validate()
            .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
        let pane = state.pane_instance.clone();
        if records.insert(pane.clone(), state).is_some() {
            return Err(StoreError::PersistFailed(format!(
                "pane snapshot contains duplicate pane instance {}:{}",
                pane.pane_id, pane.pane_pid
            )));
        }
    }
    Ok(records)
}

pub fn load_snapshot(
    path: &Path,
    expected_identity: &crate::daemon::topology::ServerIdentity,
) -> Result<BTreeMap<PaneInstance, PaneState>, StoreError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(StoreError::PersistFailed(error.to_string())),
    };
    validate_private_file(path, &metadata)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
    validate_private_file(
        path,
        &file
            .metadata()
            .map_err(|e| StoreError::PersistFailed(e.to_string()))?,
    )?;
    let mut encoded = Vec::new();
    file.take((super::MAX_RESPONSE_FRAME_BYTES + 1) as u64)
        .read_to_end(&mut encoded)
        .map_err(|error| StoreError::PersistFailed(error.to_string()))?;
    decode_snapshot(&encoded, expected_identity)
}

pub fn save_snapshot(
    path: &Path,
    server_identity: &crate::daemon::topology::ServerIdentity,
    records: &BTreeMap<PaneInstance, PaneState>,
) -> Result<(), StoreError> {
    let encoded = encode_snapshot(server_identity, records)?;
    ensure_private_parent(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_file(path, &metadata)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StoreError::PersistFailed("pane snapshot path has no file name".into()))?;
    let temp = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|e| StoreError::PersistFailed(e.to_string()))?;
        file.write_all(&encoded)
            .map_err(|e| StoreError::PersistFailed(e.to_string()))?;
        file.sync_all()
            .map_err(|e| StoreError::PersistFailed(e.to_string()))?;
        drop(file);
        std::fs::rename(&temp, path).map_err(|e| StoreError::PersistFailed(e.to_string()))?;
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| StoreError::PersistFailed(e.to_string()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

pub fn retain_topology_records(
    records: &mut BTreeMap<PaneInstance, PaneState>,
    topology: impl IntoIterator<Item = PaneInstance>,
) {
    let present = topology.into_iter().collect::<BTreeSet<_>>();
    records.retain(|pane, _| present.contains(pane));
}

fn validate_records(records: &BTreeMap<PaneInstance, PaneState>) -> Result<(), StoreError> {
    for (pane, state) in records {
        if &state.pane_instance != pane {
            return Err(StoreError::PersistFailed(
                "pane snapshot key and state identity disagree".into(),
            ));
        }
        state
            .validate()
            .map_err(|e| StoreError::PersistFailed(e.to_string()))?;
    }
    Ok(())
}

fn ensure_private_parent(path: &Path) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::PersistFailed("pane snapshot path has no parent".into()))?;
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) => validate_private_directory(parent, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent.parent().unwrap_or_else(|| Path::new(".")))
                .map_err(|e| StoreError::PersistFailed(e.to_string()))?;
            match std::fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(StoreError::PersistFailed(error.to_string())),
            }
            validate_private_directory(
                parent,
                &std::fs::symlink_metadata(parent)
                    .map_err(|e| StoreError::PersistFailed(e.to_string()))?,
            )
        }
        Err(error) => Err(StoreError::PersistFailed(error.to_string())),
    }
}

fn validate_private_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<(), StoreError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(StoreError::PersistFailed(format!(
            "insecure pane snapshot directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_private_file(path: &Path, metadata: &std::fs::Metadata) -> Result<(), StoreError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(StoreError::PersistFailed(format!(
            "insecure pane snapshot file: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn identity() -> crate::daemon::topology::ServerIdentity {
        crate::daemon::topology::ServerIdentity {
            pid: 123,
            start_time: 456,
        }
    }

    fn state(index: u32) -> PaneState {
        PaneState {
            schema_version: super::super::PANE_STATE_SCHEMA_VERSION,
            state_id: super::super::StateId::parse(format!("{index:032x}")).unwrap(),
            revision: 7,
            pane_instance: PaneInstance {
                pane_id: format!("%{index}"),
                pane_pid: 1000 + index,
            },
            agent: super::super::AgentKind::parse("codex").unwrap(),
            agent_session_id: Some(
                super::super::AgentSessionId::parse(format!("session-{index}")).unwrap(),
            ),
            agent_epoch: 3,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: super::super::LifecycleState::Waiting {
                reason: super::super::WaitReason::PermissionPrompt,
            },
            run_seq: 2,
            completed_seq: 1,
            acknowledged_seq: 1,
            started_at: Some(10),
            completed_at: Some(9),
            prompt: Some(super::super::PromptState {
                text: format!("prompt-{index}"),
                source: "hook".to_string(),
            }),
            tasks: super::super::TaskState {
                progress: super::super::TaskProgress { done: 1, total: 2 },
                items: vec![
                    super::super::TaskItemState {
                        id: Some(format!("task-{index}-done")),
                        step: "write snapshot".to_string(),
                        status: super::super::TaskItemStatus::Completed,
                    },
                    super::super::TaskItemState {
                        id: Some(format!("task-{index}-active")),
                        step: "verify snapshot".to_string(),
                        status: super::super::TaskItemStatus::InProgress,
                    },
                ],
            },
            subagents: vec![super::super::SubagentState {
                agent_id: format!("worker-{index}"),
                agent_type: "review".to_string(),
                display_name: Some("Reviewer".to_string()),
            }],
            worktree_activity: Some(super::super::WorktreeActivity {
                kind: super::super::WorktreeActivityKind::VwExec,
                name: "feature".to_string(),
                path: "/tmp/worktree".to_string(),
                command: "cargo test".to_string(),
                observed_at: 11,
            }),
        }
    }

    fn private_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vde-pane-snapshot-{name}-{}-{}",
            std::process::id(),
            super::super::EventId::generate().unwrap().as_str()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn missing_snapshot_is_empty_and_full_state_roundtrips_privately() {
        let root = private_root("roundtrip");
        let path = root.join("incarnation").join(PANE_SNAPSHOT_FILE);
        assert!(load_snapshot(&path, &identity()).unwrap().is_empty());
        let expected = BTreeMap::from([(state(1).pane_instance.clone(), state(1))]);

        save_snapshot(&path, &identity(), &expected).unwrap();

        assert_eq!(load_snapshot(&path, &identity()).unwrap(), expected);
        assert_eq!(
            std::fs::symlink_metadata(&path).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::symlink_metadata(path.parent().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn atomic_replace_changes_the_complete_snapshot_without_temp_residue() {
        let root = private_root("replace");
        let path = root.join("incarnation").join(PANE_SNAPSHOT_FILE);
        let first = BTreeMap::from([(state(1).pane_instance.clone(), state(1))]);
        let second = BTreeMap::from([(state(2).pane_instance.clone(), state(2))]);
        save_snapshot(&path, &identity(), &first).unwrap();
        save_snapshot(&path, &identity(), &second).unwrap();
        assert_eq!(load_snapshot(&path, &identity()).unwrap(), second);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_unknown_identity_and_symlink_snapshots_are_rejected() {
        let root = private_root("strict");
        let path = root.join("incarnation").join(PANE_SNAPSHOT_FILE);
        let records = BTreeMap::from([(state(1).pane_instance.clone(), state(1))]);
        save_snapshot(&path, &identity(), &records).unwrap();
        assert!(
            load_snapshot(
                &path,
                &crate::daemon::topology::ServerIdentity {
                    pid: 999,
                    start_time: 456,
                }
            )
            .is_err()
        );

        std::fs::write(&path, b"{broken").unwrap();
        assert!(load_snapshot(&path, &identity()).is_err());
        let mut unknown = serde_json::to_value(PaneStateSnapshot {
            schema_version: PANE_SNAPSHOT_SCHEMA_VERSION,
            server_identity: identity(),
            records: vec![state(1)],
        })
        .unwrap();
        unknown["unknown"] = serde_json::json!(true);
        assert!(decode_snapshot(&serde_json::to_vec(&unknown).unwrap(), &identity()).is_err());

        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("target", &path).unwrap();
        assert!(load_snapshot(&path, &identity()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn topology_prune_requires_exact_pane_instance() {
        let exact = state(1);
        let missing = state(2);
        let mut records = BTreeMap::from([
            (exact.pane_instance.clone(), exact.clone()),
            (missing.pane_instance.clone(), missing),
        ]);
        retain_topology_records(
            &mut records,
            [
                exact.pane_instance.clone(),
                PaneInstance {
                    pane_id: "%2".to_string(),
                    pane_pid: 9999,
                },
            ],
        );
        assert_eq!(
            records,
            BTreeMap::from([(exact.pane_instance.clone(), exact)])
        );
    }

    #[test]
    fn encoded_snapshot_is_bounded_to_sixteen_mibibytes() {
        let mut records = BTreeMap::new();
        for index in 1..=4100 {
            let mut record = state(index);
            record.prompt.as_mut().unwrap().text = "x".repeat(super::super::BODY_MAX_BYTES);
            records.insert(record.pane_instance.clone(), record);
        }
        assert_eq!(
            encode_snapshot(&identity(), &records),
            Err(StoreError::StateTooLarge)
        );
        assert_eq!(
            decode_snapshot(
                &vec![b' '; super::super::MAX_RESPONSE_FRAME_BYTES + 1],
                &identity()
            ),
            Err(StoreError::StateTooLarge)
        );
    }
}

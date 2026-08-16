use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    ArtifactStoreCompleteness, DispatchState, OperationId, OperationRecord, PROMPT_BODY_MAX_BYTES,
    ProviderCompleteness, RESPONSE_ARTIFACT_BODY_MAX_BYTES, ResponseArtifactMetadata, RunRecord,
    SemanticOutcome, Sha256Digest, StableRunId, StateGeneration, StateMeta, StateMetaStatus,
    artifact_file_name,
};
use crate::hook::provider::ResponseCandidate;

pub const STATE_META_FILE: &str = "state-meta.json";
pub const RUN_STORE_MAX_RECORDS: usize = 2048;
pub const RUN_STORE_MAX_BYTES: u64 = 96 * 1024 * 1024;
pub const OPERATION_STORE_MAX_RECORDS: usize = 65_536;
pub const OPERATION_STORE_MAX_BYTES: u64 = 128 * 1024 * 1024;
pub const PROMPT_STORE_MAX_RECORDS: usize = 128;
pub const PROMPT_STORE_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const ARTIFACT_STORE_MAX_FILES: usize = 4096;
pub const ARTIFACT_STORE_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const RUN_RETENTION_PER_PANE: usize = 64;
pub const RUN_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

const RUNS_DIRECTORY: &str = "runs";
const OPERATIONS_DIRECTORY: &str = "operations";
const PROMPTS_DIRECTORY: &str = "prompts";
const ARTIFACTS_DIRECTORY: &str = "artifacts";

#[derive(Debug)]
pub enum StoreError {
    Io(String),
    Corrupt(String),
    Invalid(String),
    Capacity(String),
    Conflict(String),
    PromptDispatchBusy(String),
    OperationConflict(String),
    OperationStoreFull(String),
    OperationNotFound(String),
    OperationGenerationReplaced(String),
    RunNotFound(String),
    RunGenerationReplaced(String),
    ProviderEventConflict(String),
    ResolutionConflict(String),
    RunAlreadyResolved(String),
    StalePrecondition(String),
    RecoveryNotAllowed(String),
    NotFound(String),
    ArtifactUnavailable,
    ArtifactExpired,
    StateUninitialized,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message)
            | Self::Corrupt(message)
            | Self::Invalid(message)
            | Self::Capacity(message)
            | Self::Conflict(message)
            | Self::PromptDispatchBusy(message)
            | Self::OperationConflict(message)
            | Self::OperationStoreFull(message)
            | Self::OperationNotFound(message)
            | Self::OperationGenerationReplaced(message)
            | Self::RunNotFound(message)
            | Self::RunGenerationReplaced(message)
            | Self::ProviderEventConflict(message)
            | Self::ResolutionConflict(message)
            | Self::RunAlreadyResolved(message)
            | Self::StalePrecondition(message)
            | Self::RecoveryNotAllowed(message)
            | Self::NotFound(message) => formatter.write_str(message),
            Self::ArtifactUnavailable => formatter.write_str("response artifact is unavailable"),
            Self::ArtifactExpired => formatter.write_str("response artifact has expired"),
            Self::StateUninitialized => formatter.write_str("agent state is not initialized"),
        }
    }
}

impl std::error::Error for StoreError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreRegionUsage {
    pub records: usize,
    pub bytes: u64,
    pub record_limit: usize,
    pub byte_limit: u64,
    pub oldest_retained_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStateUsage {
    pub generation: StateGeneration,
    pub state_format_version: u16,
    pub runs: StoreRegionUsage,
    pub operations: StoreRegionUsage,
    pub prompts: StoreRegionUsage,
    pub artifacts: StoreRegionUsage,
    pub in_flight_operations: usize,
}

#[derive(Debug, Clone)]
pub struct ArtifactObservation {
    pub provider_completeness: ProviderCompleteness,
    pub source: String,
    pub observed_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunRetentionReserve {
    pub records: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetentionCleanup {
    pub removed_run_ids: Vec<StableRunId>,
}

#[derive(Debug)]
pub struct AgentStateStore {
    root: PathBuf,
    meta: StateMeta,
    run_capacity: Cell<RegionCapacity>,
    operation_capacity: Cell<RegionCapacity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RegionCapacity {
    records: usize,
    bytes: u64,
}

pub fn state_root(env: &BTreeMap<String, String>, incarnation_hash: &str) -> PathBuf {
    crate::daemon::lifecycle::incarnation_state_path(env, incarnation_hash, "agent-state-v1")
}

impl AgentStateStore {
    pub fn open_or_initialize(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        let root_was_missing = match std::fs::symlink_metadata(&root) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(io_error("inspect agent state root", error)),
        };
        ensure_private_directory(&root)?;
        for directory in [
            RUNS_DIRECTORY,
            OPERATIONS_DIRECTORY,
            PROMPTS_DIRECTORY,
            ARTIFACTS_DIRECTORY,
        ] {
            ensure_private_directory(&root.join(directory))?;
        }

        let meta_path = root.join(STATE_META_FILE);
        let meta = if meta_path.exists() {
            read_json(&meta_path, 4096, "state metadata")?
        } else {
            if !root_was_missing {
                return Err(StoreError::StateUninitialized);
            }
            let meta =
                StateMeta::new_ready().map_err(|error| StoreError::Invalid(error.to_string()))?;
            write_json_atomic(&meta_path, &meta, 4096, "state metadata")?;
            meta
        };
        meta.validate()
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        if meta.status != StateMetaStatus::Ready {
            return Err(StoreError::StateUninitialized);
        }
        let store = Self {
            root,
            meta,
            run_capacity: Cell::new(RegionCapacity::default()),
            operation_capacity: Cell::new(RegionCapacity::default()),
        };
        cleanup_temporary_files(&store.runs_dir())?;
        cleanup_temporary_files(&store.operations_dir())?;
        store.validate_canonical_records()?;
        store.cleanup_orphans()?;
        store.validate_body_references()?;
        store.run_capacity.set(region_capacity(
            &store.runs_dir(),
            RUN_STORE_MAX_RECORDS,
            "run store",
        )?);
        store.operation_capacity.set(region_capacity(
            &store.operations_dir(),
            OPERATION_STORE_MAX_RECORDS,
            "operation store",
        )?);
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn meta(&self) -> &StateMeta {
        &self.meta
    }

    pub fn generation(&self) -> &StateGeneration {
        &self.meta.generation
    }

    pub fn load_run(&self, run_id: &StableRunId) -> Result<Option<RunRecord>, StoreError> {
        let path = self.run_path(run_id);
        if !path.exists() {
            return Ok(None);
        }
        let record: RunRecord = read_json(&path, super::RUN_RECORD_MAX_BYTES, "run record")?;
        self.validate_run(&record)?;
        if &record.run_id != run_id {
            return Err(StoreError::Corrupt(format!(
                "run record identity does not match {}",
                path.display()
            )));
        }
        Ok(Some(record))
    }

    pub fn save_run(&self, record: &RunRecord) -> Result<(), StoreError> {
        self.validate_run(record)?;
        let path = self.run_path(&record.run_id);
        if let Some(existing) = self.load_run(&record.run_id)? {
            if &existing == record {
                return Ok(());
            }
            validate_run_replacement(&existing, record)?;
        }
        let encoded = encode_json(record, super::RUN_RECORD_MAX_BYTES, "run record")?;
        let next_capacity = next_region_capacity(
            self.run_capacity.get(),
            existing_file_size(&path)?,
            encoded.len() as u64,
            RUN_STORE_MAX_RECORDS,
            RUN_STORE_MAX_BYTES,
            "run store",
        )?;
        match write_bytes_atomic(&path, &encoded) {
            Ok(()) => {
                self.run_capacity.set(next_capacity);
                Ok(())
            }
            Err(error) => {
                self.run_capacity.set(region_capacity(
                    &self.runs_dir(),
                    RUN_STORE_MAX_RECORDS,
                    "run store",
                )?);
                Err(error)
            }
        }
    }

    pub fn list_runs(&self) -> Result<Vec<RunRecord>, StoreError> {
        let mut records = Vec::new();
        for path in canonical_files(
            &self.runs_dir(),
            ".json",
            RUN_STORE_MAX_RECORDS,
            "run store",
        )? {
            let record: RunRecord = read_json(&path, super::RUN_RECORD_MAX_BYTES, "run record")?;
            self.validate_run(&record)?;
            if path != self.run_path(&record.run_id) {
                return Err(StoreError::Corrupt(format!(
                    "run record has non-canonical file name: {}",
                    path.display()
                )));
            }
            records.push(record);
        }
        records.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        Ok(records)
    }

    pub fn load_operation(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let path = self.operation_path(operation_id);
        if !path.exists() {
            return Ok(None);
        }
        let record: OperationRecord =
            read_json(&path, super::OPERATION_RECORD_MAX_BYTES, "operation record")?;
        self.validate_operation(&record)?;
        if &record.operation_id != operation_id {
            return Err(StoreError::Corrupt(format!(
                "operation record identity does not match {}",
                path.display()
            )));
        }
        Ok(Some(record))
    }

    pub fn save_operation(&self, record: &OperationRecord) -> Result<(), StoreError> {
        self.validate_operation(record)?;
        let path = self.operation_path(&record.operation_id);
        if let Some(existing) = self.load_operation(&record.operation_id)? {
            if &existing == record {
                return Ok(());
            }
            validate_operation_replacement(&existing, record)?;
        }
        let encoded = encode_json(
            record,
            super::OPERATION_RECORD_MAX_BYTES,
            "operation record",
        )?;
        let next_capacity = next_region_capacity(
            self.operation_capacity.get(),
            existing_file_size(&path)?,
            encoded.len() as u64,
            OPERATION_STORE_MAX_RECORDS,
            OPERATION_STORE_MAX_BYTES,
            "operation store",
        )?;
        match write_bytes_atomic(&path, &encoded) {
            Ok(()) => {
                self.operation_capacity.set(next_capacity);
                Ok(())
            }
            Err(error) => {
                self.operation_capacity.set(region_capacity(
                    &self.operations_dir(),
                    OPERATION_STORE_MAX_RECORDS,
                    "operation store",
                )?);
                Err(error)
            }
        }
    }

    pub fn list_operations(&self) -> Result<Vec<OperationRecord>, StoreError> {
        let mut records = Vec::new();
        for path in canonical_files(
            &self.operations_dir(),
            ".json",
            OPERATION_STORE_MAX_RECORDS,
            "operation store",
        )? {
            let record: OperationRecord =
                read_json(&path, super::OPERATION_RECORD_MAX_BYTES, "operation record")?;
            self.validate_operation(&record)?;
            if path != self.operation_path(&record.operation_id) {
                return Err(StoreError::Corrupt(format!(
                    "operation record has non-canonical file name: {}",
                    path.display()
                )));
            }
            records.push(record);
        }
        records.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
        Ok(records)
    }

    pub fn for_each_operation(
        &self,
        mut visitor: impl FnMut(OperationRecord) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        for_each_canonical_file(
            &self.operations_dir(),
            ".json",
            OPERATION_STORE_MAX_RECORDS,
            "operation store",
            |path| {
                let record: OperationRecord =
                    read_json(&path, super::OPERATION_RECORD_MAX_BYTES, "operation record")?;
                self.validate_operation(&record)?;
                if path != self.operation_path(&record.operation_id) {
                    return Err(StoreError::Corrupt(format!(
                        "operation record has non-canonical file name: {}",
                        path.display()
                    )));
                }
                visitor(record)?;
                Ok(())
            },
        )
    }

    pub fn stage_prompt(&self, operation_id: &OperationId, body: &[u8]) -> Result<(), StoreError> {
        if body.len() > PROMPT_BODY_MAX_BYTES {
            return Err(StoreError::Capacity(
                "prompt exceeds the 65,536-byte limit".to_string(),
            ));
        }
        let path = self.prompt_path(operation_id);
        if path.exists() {
            let existing = read_private_file(&path, PROMPT_BODY_MAX_BYTES, "prompt staging")?;
            return if existing == body {
                Ok(())
            } else {
                Err(StoreError::Conflict(format!(
                    "prompt staging conflict for {}",
                    operation_id.as_str()
                )))
            };
        }
        ensure_region_capacity(
            &self.prompts_dir(),
            None,
            body.len() as u64,
            PROMPT_STORE_MAX_RECORDS,
            PROMPT_STORE_MAX_BYTES,
            "prompt staging store",
        )?;
        write_bytes_atomic(&path, body)
    }

    pub fn read_prompt(&self, operation_id: &OperationId) -> Result<Vec<u8>, StoreError> {
        let path = self.prompt_path(operation_id);
        if !path.exists() {
            return Err(StoreError::NotFound(format!(
                "prompt staging not found for {}",
                operation_id.as_str()
            )));
        }
        read_private_file(&path, PROMPT_BODY_MAX_BYTES, "prompt staging")
    }

    pub fn delete_prompt(&self, operation_id: &OperationId) -> Result<(), StoreError> {
        remove_file_and_sync(&self.prompt_path(operation_id))
    }

    pub fn store_artifact(
        &self,
        run_id: &StableRunId,
        body: &str,
        observation: ArtifactObservation,
    ) -> Result<ResponseArtifactMetadata, StoreError> {
        let mut run = self
            .load_run(run_id)?
            .ok_or_else(|| StoreError::NotFound(format!("run {} not found", run_id.as_str())))?;
        if run.semantic_outcome != SemanticOutcome::Completed {
            return Err(StoreError::Invalid(
                "cannot store an artifact for an unresolved run".to_string(),
            ));
        }
        let original = body.as_bytes();
        let original_digest = Sha256Digest::of(original);
        if let Some(existing) = &run.artifact {
            if existing.original_digest != original_digest {
                return Err(StoreError::Conflict(
                    "response artifact digest conflicts with existing completion".to_string(),
                ));
            }
            if existing.store_completeness != ArtifactStoreCompleteness::Unavailable {
                return Ok(existing.clone());
            }
        }

        let stored = utf8_suffix(body, RESPONSE_ARTIFACT_BODY_MAX_BYTES).as_bytes();
        let completeness = if stored.len() == original.len() {
            ArtifactStoreCompleteness::Complete
        } else {
            ArtifactStoreCompleteness::Truncated
        };
        let file_name = artifact_file_name(run_id);
        let path = self.artifacts_dir().join(&file_name);
        let can_store = self.ensure_artifact_capacity(run_id, &path, stored.len() as u64);

        let metadata = match can_store.and_then(|()| write_bytes_atomic(&path, stored)) {
            Ok(()) => ResponseArtifactMetadata {
                run_id: run.run_id.clone(),
                operation_id: run.operation_id.clone(),
                provider_session_id: run.binding.provider_session_id.clone(),
                observed_process: run.binding.process.clone(),
                original_byte_count: original.len() as u64,
                stored_byte_count: stored.len() as u64,
                original_digest,
                stored_digest: Some(Sha256Digest::of(stored)),
                provider_completeness: observation.provider_completeness,
                store_completeness: completeness,
                source: observation.source,
                encoding: "utf-8".to_string(),
                observed_at: observation.observed_at,
                file_name: Some(file_name),
            },
            Err(_) => ResponseArtifactMetadata {
                run_id: run.run_id.clone(),
                operation_id: run.operation_id.clone(),
                provider_session_id: run.binding.provider_session_id.clone(),
                observed_process: run.binding.process.clone(),
                original_byte_count: original.len() as u64,
                stored_byte_count: 0,
                original_digest,
                stored_digest: None,
                provider_completeness: observation.provider_completeness,
                store_completeness: ArtifactStoreCompleteness::Unavailable,
                source: observation.source,
                encoding: "utf-8".to_string(),
                observed_at: observation.observed_at,
                file_name: None,
            },
        };
        metadata
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        run.artifact = Some(metadata.clone());
        run.revision = run.revision.saturating_add(1);
        run.updated_at = run.updated_at.max(observation.observed_at);
        self.save_run(&run)?;
        Ok(metadata)
    }

    pub fn store_artifact_candidate(
        &self,
        run_id: &StableRunId,
        candidate: &ResponseCandidate,
        source: impl Into<String>,
        observed_at: i64,
    ) -> Result<ResponseArtifactMetadata, StoreError> {
        let body = candidate
            .decode_body()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        if candidate.stored_bytes > RESPONSE_ARTIFACT_BODY_MAX_BYTES as u64
            || candidate.truncated != (candidate.stored_bytes != candidate.original_bytes)
            || (!candidate.truncated
                && (candidate.original_bytes != candidate.stored_bytes
                    || candidate.original_digest != candidate.stored_digest))
        {
            return Err(StoreError::Invalid(
                "response candidate metadata is inconsistent".to_string(),
            ));
        }
        let original_digest = Sha256Digest::parse(candidate.original_digest.clone())
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let stored_digest = Sha256Digest::parse(candidate.stored_digest.clone())
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let provider_completeness = match candidate.provider_completeness {
            crate::hook::provider::ProviderCompleteness::Complete => ProviderCompleteness::Complete,
            crate::hook::provider::ProviderCompleteness::Unknown => ProviderCompleteness::Unknown,
        };

        let mut run = self
            .load_run(run_id)?
            .ok_or_else(|| StoreError::NotFound(format!("run {} not found", run_id.as_str())))?;
        if run.semantic_outcome != SemanticOutcome::Completed {
            return Err(StoreError::Invalid(
                "cannot store an artifact for an unresolved run".to_string(),
            ));
        }
        if let Some(existing) = &run.artifact {
            if existing.original_digest != original_digest
                || existing.original_byte_count != candidate.original_bytes
            {
                return Err(StoreError::Conflict(
                    "response artifact digest conflicts with existing completion".to_string(),
                ));
            }
            if existing.store_completeness != ArtifactStoreCompleteness::Unavailable {
                return Ok(existing.clone());
            }
        }

        let source = source.into();
        let file_name = artifact_file_name(run_id);
        let path = self.artifacts_dir().join(&file_name);
        let store_result = self
            .ensure_artifact_capacity(run_id, &path, body.len() as u64)
            .and_then(|()| write_bytes_atomic(&path, &body));
        let metadata = match store_result {
            Ok(()) => ResponseArtifactMetadata {
                run_id: run.run_id.clone(),
                operation_id: run.operation_id.clone(),
                provider_session_id: run.binding.provider_session_id.clone(),
                observed_process: run.binding.process.clone(),
                original_byte_count: candidate.original_bytes,
                stored_byte_count: candidate.stored_bytes,
                original_digest,
                stored_digest: Some(stored_digest),
                provider_completeness,
                store_completeness: if candidate.truncated {
                    ArtifactStoreCompleteness::Truncated
                } else {
                    ArtifactStoreCompleteness::Complete
                },
                source,
                encoding: "utf-8".to_string(),
                observed_at,
                file_name: Some(file_name),
            },
            Err(_) => ResponseArtifactMetadata {
                run_id: run.run_id.clone(),
                operation_id: run.operation_id.clone(),
                provider_session_id: run.binding.provider_session_id.clone(),
                observed_process: run.binding.process.clone(),
                original_byte_count: candidate.original_bytes,
                stored_byte_count: candidate.stored_bytes,
                original_digest,
                stored_digest: Some(stored_digest),
                provider_completeness,
                store_completeness: ArtifactStoreCompleteness::Unavailable,
                source,
                encoding: "utf-8".to_string(),
                observed_at,
                file_name: None,
            },
        };
        metadata
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        run.artifact = Some(metadata.clone());
        run.revision = run
            .revision
            .checked_add(1)
            .ok_or_else(|| StoreError::Invalid("run revision overflow".to_string()))?;
        run.updated_at = run.updated_at.max(observed_at);
        self.save_run(&run)?;
        Ok(metadata)
    }

    pub fn read_artifact(&self, run_id: &StableRunId) -> Result<String, StoreError> {
        let run = self
            .load_run(run_id)?
            .ok_or_else(|| StoreError::NotFound(format!("run {} not found", run_id.as_str())))?;
        let artifact = run.artifact.ok_or(StoreError::ArtifactUnavailable)?;
        match artifact.store_completeness {
            ArtifactStoreCompleteness::Unavailable => return Err(StoreError::ArtifactUnavailable),
            ArtifactStoreCompleteness::Expired => return Err(StoreError::ArtifactExpired),
            ArtifactStoreCompleteness::Complete | ArtifactStoreCompleteness::Truncated => {}
        }
        let file_name = artifact
            .file_name
            .as_deref()
            .ok_or_else(|| StoreError::Corrupt("artifact file name is absent".to_string()))?;
        let body = read_private_file(
            &self.artifacts_dir().join(file_name),
            RESPONSE_ARTIFACT_BODY_MAX_BYTES,
            "response artifact",
        )?;
        if body.len() as u64 != artifact.stored_byte_count
            || artifact.stored_digest.as_ref() != Some(&Sha256Digest::of(&body))
        {
            return Err(StoreError::Corrupt(
                "response artifact body does not match metadata".to_string(),
            ));
        }
        String::from_utf8(body)
            .map_err(|_| StoreError::Corrupt("response artifact is not UTF-8".to_string()))
    }

    pub fn expire_artifact(&self, run_id: &StableRunId) -> Result<(), StoreError> {
        let mut run = self
            .load_run(run_id)?
            .ok_or_else(|| StoreError::NotFound(format!("run {} not found", run_id.as_str())))?;
        let Some(mut artifact) = run.artifact.clone() else {
            return Ok(());
        };
        if artifact.store_completeness == ArtifactStoreCompleteness::Expired {
            return Ok(());
        }
        let file_name = artifact.file_name.take();
        artifact.store_completeness = ArtifactStoreCompleteness::Expired;
        run.artifact = Some(artifact);
        run.revision = run.revision.saturating_add(1);
        self.save_run(&run)?;
        if let Some(file_name) = file_name {
            remove_file_and_sync(&self.artifacts_dir().join(file_name))?;
        }
        Ok(())
    }

    pub fn collect_run_retention(
        &self,
        now: i64,
        protected_run_ids: impl IntoIterator<Item = StableRunId>,
        reserve: RunRetentionReserve,
        incoming_pane: Option<&crate::pane_state::PaneInstance>,
    ) -> Result<RetentionCleanup, StoreError> {
        if now < 0 || reserve.records > RUN_STORE_MAX_RECORDS || reserve.bytes > RUN_STORE_MAX_BYTES
        {
            return Err(StoreError::Invalid(
                "invalid run retention request".to_string(),
            ));
        }
        let mut protected = BTreeSet::new();
        for run_id in protected_run_ids
            .into_iter()
            .take(RUN_STORE_MAX_RECORDS + 1)
        {
            protected.insert(run_id);
            if protected.len() > RUN_STORE_MAX_RECORDS {
                return Err(StoreError::Capacity(
                    "run retention protection set exceeds limit".to_string(),
                ));
            }
        }

        let runs = self.list_runs()?;
        let mut removable = runs
            .iter()
            .filter(|run| !run.execution_active() && !protected.contains(&run.run_id))
            .collect::<Vec<_>>();
        removable.sort_by(|left, right| {
            (left.created_at, &left.run_id).cmp(&(right.created_at, &right.run_id))
        });

        let cutoff = now.saturating_sub(RUN_RETENTION_SECONDS);
        let mut remove_ids = removable
            .iter()
            .filter(|run| run.created_at < cutoff)
            .map(|run| run.run_id.clone())
            .collect::<BTreeSet<_>>();

        let mut by_pane = BTreeMap::<_, Vec<&RunRecord>>::new();
        for run in &removable {
            if !remove_ids.contains(&run.run_id) {
                by_pane
                    .entry(run.binding.pane_instance.clone())
                    .or_default()
                    .push(run);
            }
        }
        for (pane, pane_runs) in &mut by_pane {
            pane_runs.sort_by(|left, right| {
                (left.created_at, &left.run_id).cmp(&(right.created_at, &right.run_id))
            });
            let protected_current_becomes_historical = incoming_pane == Some(pane)
                && runs.iter().any(|run| {
                    run.binding.pane_instance == *pane && protected.contains(&run.run_id)
                });
            let retained_limit = RUN_RETENTION_PER_PANE
                .saturating_sub(usize::from(protected_current_becomes_historical));
            let excess = pane_runs.len().saturating_sub(retained_limit);
            remove_ids.extend(pane_runs.iter().take(excess).map(|run| run.run_id.clone()));
        }

        let current_bytes = self.run_capacity.get().bytes;
        let target_records = RUN_STORE_MAX_RECORDS.saturating_sub(reserve.records);
        let target_bytes = RUN_STORE_MAX_BYTES.saturating_sub(reserve.bytes);
        let mut retained_records = runs.len().saturating_sub(remove_ids.len());
        let mut retained_bytes = current_bytes;
        for run in &runs {
            if remove_ids.contains(&run.run_id) {
                retained_bytes = retained_bytes
                    .saturating_sub(existing_file_size(&self.run_path(&run.run_id))?.unwrap_or(0));
            }
        }
        for run in removable {
            if retained_records <= target_records && retained_bytes <= target_bytes {
                break;
            }
            if remove_ids.insert(run.run_id.clone()) {
                retained_records = retained_records.saturating_sub(1);
                retained_bytes = retained_bytes
                    .saturating_sub(existing_file_size(&self.run_path(&run.run_id))?.unwrap_or(0));
            }
        }
        if retained_records > target_records || retained_bytes > target_bytes {
            return Err(StoreError::Capacity(
                "run store cannot satisfy retention reserve without protected records".to_string(),
            ));
        }

        let removed_run_ids = remove_ids.into_iter().collect::<Vec<_>>();
        self.remove_runs_atomically(&removed_run_ids)?;
        Ok(RetentionCleanup { removed_run_ids })
    }

    pub fn reset_offline(
        root: impl Into<PathBuf>,
        expected_generation: &StateGeneration,
    ) -> Result<StateGeneration, StoreError> {
        let root = root.into();
        validate_private_directory(&root)?;
        for directory in [
            RUNS_DIRECTORY,
            OPERATIONS_DIRECTORY,
            PROMPTS_DIRECTORY,
            ARTIFACTS_DIRECTORY,
        ] {
            validate_private_directory(&root.join(directory))?;
        }
        let meta_path = root.join(STATE_META_FILE);
        let mut meta: StateMeta = read_json(&meta_path, 4096, "state metadata")?;
        meta.validate()
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        match meta.status {
            StateMetaStatus::Ready => {
                if &meta.generation != expected_generation {
                    return Err(StoreError::StalePrecondition(
                        "state generation does not match reset precondition".to_string(),
                    ));
                }
                Self {
                    root: root.clone(),
                    meta: meta.clone(),
                    run_capacity: Cell::new(RegionCapacity::default()),
                    operation_capacity: Cell::new(RegionCapacity::default()),
                }
                .ensure_reset_quiescent()?;
                let target = StateGeneration::generate()
                    .map_err(|error| StoreError::Invalid(error.to_string()))?;
                meta = StateMeta::resetting(meta.generation, target)
                    .map_err(|error| StoreError::Invalid(error.to_string()))?;
                write_json_atomic(&meta_path, &meta, 4096, "state metadata")?;
            }
            StateMetaStatus::Resetting if &meta.generation == expected_generation => {}
            StateMetaStatus::Resetting => {
                return Err(StoreError::StalePrecondition(
                    "reset marker generation does not match reset precondition".to_string(),
                ));
            }
        }
        let target_generation = meta
            .target_generation
            .clone()
            .ok_or_else(|| StoreError::Corrupt("reset target generation is absent".to_string()))?;
        for directory in [
            RUNS_DIRECTORY,
            OPERATIONS_DIRECTORY,
            PROMPTS_DIRECTORY,
            ARTIFACTS_DIRECTORY,
        ] {
            let path = root.join(directory);
            validate_private_directory(&path)?;
            clear_directory_and_sync(&path)?;
        }
        let ready = StateMeta {
            state_format_version: meta.state_format_version,
            status: StateMetaStatus::Ready,
            generation: target_generation.clone(),
            target_generation: None,
        };
        ready
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        write_json_atomic(&meta_path, &ready, 4096, "state metadata")?;
        Ok(target_generation)
    }

    pub fn recover_uninitialized_offline(
        root: impl Into<PathBuf>,
    ) -> Result<StateGeneration, StoreError> {
        let root = root.into();
        ensure_private_directory(&root)?;
        for directory in [
            RUNS_DIRECTORY,
            OPERATIONS_DIRECTORY,
            PROMPTS_DIRECTORY,
            ARTIFACTS_DIRECTORY,
        ] {
            ensure_private_directory(&root.join(directory))?;
        }
        let meta_path = root.join(STATE_META_FILE);
        match std::fs::symlink_metadata(&meta_path) {
            Ok(metadata) => {
                validate_private_file(&meta_path, &metadata)?;
                match read_json::<StateMeta>(&meta_path, 4096, "state metadata") {
                    Ok(meta) if meta.validate().is_ok() => {
                        return Err(StoreError::RecoveryNotAllowed(
                            "state metadata is valid; use generation-bound reset".to_string(),
                        ));
                    }
                    Ok(_) | Err(StoreError::Corrupt(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect state metadata", error)),
        }

        for directory in [
            RUNS_DIRECTORY,
            OPERATIONS_DIRECTORY,
            PROMPTS_DIRECTORY,
            ARTIFACTS_DIRECTORY,
        ] {
            clear_directory_and_sync(&root.join(directory))?;
        }
        let ready =
            StateMeta::new_ready().map_err(|error| StoreError::Invalid(error.to_string()))?;
        let generation = ready.generation.clone();
        write_json_atomic(&meta_path, &ready, 4096, "state metadata")?;
        Ok(generation)
    }

    fn ensure_reset_quiescent(&self) -> Result<(), StoreError> {
        if self.list_runs()?.iter().any(RunRecord::execution_active) {
            return Err(StoreError::RecoveryNotAllowed(
                "offline reset requires no execution-active runs".to_string(),
            ));
        }
        self.for_each_operation(|operation| {
            if operation.in_flight() {
                return Err(StoreError::RecoveryNotAllowed(
                    "offline reset requires no in-flight operations".to_string(),
                ));
            }
            Ok(())
        })
    }

    fn ensure_artifact_capacity(
        &self,
        target_run_id: &StableRunId,
        target_path: &Path,
        new_size: u64,
    ) -> Result<(), StoreError> {
        self.ensure_artifact_capacity_with_limits(
            target_run_id,
            target_path,
            new_size,
            ARTIFACT_STORE_MAX_FILES,
            ARTIFACT_STORE_MAX_BYTES,
        )
    }

    fn ensure_artifact_capacity_with_limits(
        &self,
        target_run_id: &StableRunId,
        target_path: &Path,
        new_size: u64,
        file_limit: usize,
        byte_limit: u64,
    ) -> Result<(), StoreError> {
        loop {
            let capacity = ensure_region_capacity(
                &self.artifacts_dir(),
                existing_file_size(target_path)?,
                new_size,
                file_limit,
                byte_limit,
                "response artifact store",
            );
            match capacity {
                Ok(()) => return Ok(()),
                Err(StoreError::Capacity(_)) => {}
                Err(error) => return Err(error),
            }
            let mut candidates = self
                .list_runs()?
                .into_iter()
                .filter(|run| &run.run_id != target_run_id)
                .filter_map(|run| {
                    let completed_at = run.resolution.as_ref()?.resolved_at;
                    let stored = matches!(
                        run.artifact.as_ref()?.store_completeness,
                        ArtifactStoreCompleteness::Complete | ArtifactStoreCompleteness::Truncated
                    );
                    stored.then_some((completed_at, run.created_at, run.run_id))
                })
                .collect::<Vec<_>>();
            candidates.sort();
            let Some((_, _, run_id)) = candidates.into_iter().next() else {
                return Err(StoreError::Capacity(
                    "response artifact store is full".to_string(),
                ));
            };
            self.expire_artifact(&run_id)?;
        }
    }

    fn remove_runs_atomically(&self, run_ids: &[StableRunId]) -> Result<(), StoreError> {
        let mut artifact_paths = Vec::new();
        let mut removed_run_records = 0_usize;
        let mut removed_run_bytes = 0_u64;
        for run_id in run_ids {
            if let Some(size) = existing_file_size(&self.run_path(run_id))? {
                removed_run_records += 1;
                removed_run_bytes = removed_run_bytes.checked_add(size).ok_or_else(|| {
                    StoreError::Corrupt("run removal byte accounting overflow".to_string())
                })?;
            }
            if let Some(run) = self.load_run(run_id)?
                && let Some(file_name) = run.artifact.and_then(|artifact| artifact.file_name)
            {
                artifact_paths.push(self.artifacts_dir().join(file_name));
            }
        }
        remove_files_and_sync(
            &self.runs_dir(),
            run_ids.iter().map(|run_id| self.run_path(run_id)),
        )?;
        let capacity = self.run_capacity.get();
        self.run_capacity.set(RegionCapacity {
            records: capacity.records.saturating_sub(removed_run_records),
            bytes: capacity.bytes.saturating_sub(removed_run_bytes),
        });
        remove_files_and_sync(&self.artifacts_dir(), artifact_paths)?;
        Ok(())
    }

    pub fn usage(&self) -> Result<AgentStateUsage, StoreError> {
        let runs = self.list_runs()?;
        let mut operation_oldest_at: Option<i64> = None;
        let mut in_flight_operations = 0;
        self.for_each_operation(|record| {
            operation_oldest_at = Some(
                operation_oldest_at
                    .map_or(record.created_at, |oldest| oldest.min(record.created_at)),
            );
            in_flight_operations += usize::from(record.in_flight());
            Ok(())
        })?;
        Ok(AgentStateUsage {
            generation: self.meta.generation.clone(),
            state_format_version: self.meta.state_format_version,
            runs: region_usage(
                &self.runs_dir(),
                RUN_STORE_MAX_RECORDS,
                RUN_STORE_MAX_BYTES,
                "run store",
                runs.iter().map(|record| record.created_at),
            )?,
            operations: region_usage(
                &self.operations_dir(),
                OPERATION_STORE_MAX_RECORDS,
                OPERATION_STORE_MAX_BYTES,
                "operation store",
                operation_oldest_at.into_iter(),
            )?,
            prompts: region_usage(
                &self.prompts_dir(),
                PROMPT_STORE_MAX_RECORDS,
                PROMPT_STORE_MAX_BYTES,
                "prompt staging store",
                std::iter::empty(),
            )?,
            artifacts: region_usage(
                &self.artifacts_dir(),
                ARTIFACT_STORE_MAX_FILES,
                ARTIFACT_STORE_MAX_BYTES,
                "response artifact store",
                runs.iter().filter_map(|record| {
                    record
                        .artifact
                        .as_ref()
                        .map(|artifact| artifact.observed_at)
                }),
            )?,
            in_flight_operations,
        })
    }

    pub fn cleanup_orphans(&self) -> Result<(), StoreError> {
        let mut referenced_prompts = BTreeSet::new();
        self.for_each_operation(|record| {
            if record.prompt_staging_required() {
                referenced_prompts.insert(format!("{}.prompt", record.operation_id.as_str()));
                if referenced_prompts.len() > PROMPT_STORE_MAX_RECORDS {
                    return Err(StoreError::Corrupt(
                        "prompt staging references exceed store limit".to_string(),
                    ));
                }
            }
            Ok(())
        })?;
        let runs = self.list_runs()?;
        let referenced_artifacts = runs
            .iter()
            .filter_map(|record| record.artifact.as_ref()?.file_name.clone())
            .collect::<BTreeSet<_>>();
        cleanup_body_directory(&self.prompts_dir(), &referenced_prompts)?;
        cleanup_body_directory(&self.artifacts_dir(), &referenced_artifacts)?;
        cleanup_temporary_files(&self.runs_dir())?;
        cleanup_temporary_files(&self.operations_dir())?;
        Ok(())
    }

    fn validate_canonical_records(&self) -> Result<(), StoreError> {
        self.list_runs()?;
        self.for_each_operation(|_| Ok(()))
    }

    fn validate_body_references(&self) -> Result<(), StoreError> {
        self.for_each_operation(|operation| {
            if operation.prompt_staging_required() {
                let path = self.prompt_path(&operation.operation_id);
                let body = read_private_file(&path, PROMPT_BODY_MAX_BYTES, "prompt staging")
                    .map_err(|error| {
                        missing_reference_error(
                            error,
                            &format!(
                                "operation {} references invalid prompt staging",
                                operation.operation_id.as_str()
                            ),
                        )
                    })?;
                let prompt = std::str::from_utf8(&body).map_err(|_| {
                    StoreError::Corrupt(format!(
                        "operation {} prompt staging is not UTF-8",
                        operation.operation_id.as_str()
                    ))
                })?;
                let digest = Sha256Digest::parse(
                    crate::pane_state::PromptState::digest_decoded_prompt(prompt),
                )
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
                if digest != operation.prompt_digest {
                    return Err(StoreError::Corrupt(format!(
                        "operation {} prompt staging digest mismatch",
                        operation.operation_id.as_str()
                    )));
                }
            }
            Ok(())
        })?;
        for run in self.list_runs()? {
            if let Some(artifact) = run.artifact
                && let Some(file_name) = artifact.file_name
            {
                let expected_name = artifact_file_name(&run.run_id);
                if file_name != expected_name {
                    return Err(StoreError::Corrupt(format!(
                        "run {} references a non-canonical response artifact",
                        run.run_id.as_str()
                    )));
                }
                let body = read_private_file(
                    &self.artifacts_dir().join(&file_name),
                    RESPONSE_ARTIFACT_BODY_MAX_BYTES,
                    "response artifact",
                )
                .map_err(|error| {
                    missing_reference_error(
                        error,
                        &format!(
                            "run {} references an invalid response artifact",
                            run.run_id.as_str()
                        ),
                    )
                })?;
                if body.len() as u64 != artifact.stored_byte_count
                    || artifact.stored_digest.as_ref() != Some(&Sha256Digest::of(&body))
                    || std::str::from_utf8(&body).is_err()
                {
                    return Err(StoreError::Corrupt(format!(
                        "run {} response artifact body does not match metadata",
                        run.run_id.as_str()
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_run(&self, record: &RunRecord) -> Result<(), StoreError> {
        record
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        if record.generation != self.meta.generation {
            return Err(StoreError::Invalid("run generation mismatch".to_string()));
        }
        Ok(())
    }

    fn validate_operation(&self, record: &OperationRecord) -> Result<(), StoreError> {
        record
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        if record.generation != self.meta.generation {
            return Err(StoreError::Invalid(
                "operation generation mismatch".to_string(),
            ));
        }
        Ok(())
    }

    fn runs_dir(&self) -> PathBuf {
        self.root.join(RUNS_DIRECTORY)
    }

    fn operations_dir(&self) -> PathBuf {
        self.root.join(OPERATIONS_DIRECTORY)
    }

    fn prompts_dir(&self) -> PathBuf {
        self.root.join(PROMPTS_DIRECTORY)
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.root.join(ARTIFACTS_DIRECTORY)
    }

    fn run_path(&self, run_id: &StableRunId) -> PathBuf {
        self.runs_dir().join(format!("{}.json", run_id.as_str()))
    }

    fn operation_path(&self, operation_id: &OperationId) -> PathBuf {
        self.operations_dir()
            .join(format!("{}.json", operation_id.as_str()))
    }

    fn prompt_path(&self, operation_id: &OperationId) -> PathBuf {
        self.prompts_dir()
            .join(format!("{}.prompt", operation_id.as_str()))
    }
}

fn validate_run_replacement(existing: &RunRecord, next: &RunRecord) -> Result<(), StoreError> {
    if existing.generation != next.generation
        || existing.run_id != next.run_id
        || existing.run_seq != next.run_seq
        || existing.binding != next.binding
        || existing.provider_turn_key != next.provider_turn_key
        || existing.operation_id != next.operation_id
        || existing.created_at != next.created_at
    {
        return Err(StoreError::Conflict(
            "run replacement changes immutable identity fields".to_string(),
        ));
    }
    if next.revision != existing.revision.saturating_add(1) || next.updated_at < existing.updated_at
    {
        return Err(StoreError::Conflict(
            "run replacement must advance revision and time exactly once".to_string(),
        ));
    }
    if existing.semantic_outcome == SemanticOutcome::Completed
        && next.semantic_outcome != SemanticOutcome::Completed
    {
        return Err(StoreError::Conflict(
            "completed run cannot become unresolved".to_string(),
        ));
    }
    if let Some(resolution) = &existing.resolution
        && next.resolution.as_ref() != Some(resolution)
    {
        return Err(StoreError::Conflict(
            "run resolution is immutable once assigned".to_string(),
        ));
    }
    validate_evidence_replacement(&existing.evidence, &next.evidence)?;
    validate_artifact_replacement(existing.artifact.as_ref(), next.artifact.as_ref())
}

fn validate_evidence_replacement(
    existing: &super::RunEvidenceSummary,
    next: &super::RunEvidenceSummary,
) -> Result<(), StoreError> {
    if next.activity_count < existing.activity_count
        || next.permission_request_count < existing.permission_request_count
        || next.user_input_request_count < existing.user_input_request_count
        || next.process_absence_count < existing.process_absence_count
        || next.terminal_still_count < existing.terminal_still_count
        || next.provider_events.len() < existing.provider_events.len()
    {
        return Err(StoreError::Conflict(
            "run evidence replacement is not monotonic".to_string(),
        ));
    }
    for (old, new) in existing
        .provider_events
        .iter()
        .zip(next.provider_events.iter())
    {
        if old.event_ref != new.event_ref
            || old.ingress_request_id != new.ingress_request_id
            || old.payload_digest != new.payload_digest
            || old.disposition != new.disposition
            || old.receipt != new.receipt
            || old.first_observed_at != new.first_observed_at
            || new.count < old.count
            || new.last_observed_at < old.last_observed_at
        {
            return Err(StoreError::Conflict(
                "provider evidence identity changed during replacement".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_artifact_replacement(
    existing: Option<&ResponseArtifactMetadata>,
    next: Option<&ResponseArtifactMetadata>,
) -> Result<(), StoreError> {
    let (Some(existing), Some(next)) = (existing, next) else {
        return if existing.is_none() {
            Ok(())
        } else {
            Err(StoreError::Conflict(
                "response artifact metadata cannot be removed".to_string(),
            ))
        };
    };
    if existing.original_digest != next.original_digest
        || existing.original_byte_count != next.original_byte_count
        || existing.run_id != next.run_id
        || existing.operation_id != next.operation_id
    {
        return Err(StoreError::Conflict(
            "response artifact identity changed during replacement".to_string(),
        ));
    }
    let monotonic = match (existing.store_completeness, next.store_completeness) {
        (left, right) if left == right => true,
        (
            ArtifactStoreCompleteness::Unavailable,
            ArtifactStoreCompleteness::Complete | ArtifactStoreCompleteness::Truncated,
        )
        | (
            ArtifactStoreCompleteness::Complete
            | ArtifactStoreCompleteness::Truncated
            | ArtifactStoreCompleteness::Unavailable,
            ArtifactStoreCompleteness::Expired,
        ) => true,
        _ => false,
    };
    if !monotonic {
        return Err(StoreError::Conflict(
            "response artifact state cannot move backward".to_string(),
        ));
    }
    Ok(())
}

fn validate_operation_replacement(
    existing: &OperationRecord,
    next: &OperationRecord,
) -> Result<(), StoreError> {
    if existing.generation != next.generation
        || existing.operation_id != next.operation_id
        || existing.request_fingerprint != next.request_fingerprint
        || existing.target_agent_ref != next.target_agent_ref
        || existing.prompt_digest != next.prompt_digest
        || existing.dispatch_option != next.dispatch_option
        || existing.binding != next.binding
        || existing.expected_pane_version != next.expected_pane_version
        || existing.expected_current_run != next.expected_current_run
        || existing.expected_run_seq != next.expected_run_seq
        || existing.confirmation_deadline_at != next.confirmation_deadline_at
        || existing.created_at != next.created_at
    {
        return Err(StoreError::Conflict(
            "operation replacement changes immutable request fields".to_string(),
        ));
    }
    if next.revision != existing.revision.saturating_add(1) || next.updated_at < existing.updated_at
    {
        return Err(StoreError::Conflict(
            "operation replacement must advance revision and time exactly once".to_string(),
        ));
    }
    let valid_transition = matches!(
        (existing.dispatch_state, next.dispatch_state),
        (
            DispatchState::Prepared,
            DispatchState::DispatchStarted | DispatchState::Rejected
        ) | (
            DispatchState::DispatchStarted,
            DispatchState::PromptConfirmed
                | DispatchState::DeliveryUnknown
                | DispatchState::Rejected
        ) | (
            DispatchState::DeliveryUnknown,
            DispatchState::PromptConfirmed
        )
    );
    if !valid_transition {
        return Err(StoreError::Conflict(
            "invalid dispatch operation state transition".to_string(),
        ));
    }
    if existing.run_id.is_some() && existing.run_id != next.run_id {
        return Err(StoreError::Conflict(
            "operation run binding cannot be replaced".to_string(),
        ));
    }
    if existing.result_receipt.is_some()
        && existing.result_receipt != next.result_receipt
        && !matches!(
            (existing.dispatch_state, next.dispatch_state),
            (
                DispatchState::DeliveryUnknown,
                DispatchState::PromptConfirmed
            )
        )
    {
        return Err(StoreError::Conflict(
            "operation result receipt cannot be replaced".to_string(),
        ));
    }
    Ok(())
}

fn encode_json<T: serde::Serialize>(
    value: &T,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, StoreError> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| StoreError::Invalid(format!("failed to encode {label}: {error}")))?;
    if encoded.len() > max_bytes {
        return Err(StoreError::Capacity(format!(
            "{label} exceeds the {max_bytes}-byte limit"
        )));
    }
    Ok(encoded)
}

fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    max_bytes: usize,
    label: &str,
) -> Result<T, StoreError> {
    let encoded = read_private_file(path, max_bytes, label)?;
    serde_json::from_slice(&encoded).map_err(|error| {
        StoreError::Corrupt(format!("invalid {label} {}: {error}", path.display()))
    })
}

fn write_json_atomic<T: serde::Serialize>(
    path: &Path,
    value: &T,
    max_bytes: usize,
    label: &str,
) -> Result<(), StoreError> {
    write_bytes_atomic(path, &encode_json(value, max_bytes, label)?)
}

fn write_bytes_atomic(path: &Path, body: &[u8]) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::Io(format!("{} has no parent", path.display())))?;
    validate_private_directory(parent)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        validate_private_file(path, &metadata)?;
    }
    let temporary = temporary_path(path)?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| io_error("create temporary state file", error))?;
        file.write_all(body)
            .map_err(|error| io_error("write temporary state file", error))?;
        file.sync_all()
            .map_err(|error| io_error("sync temporary state file", error))?;
        drop(file);
        std::fs::rename(&temporary, path).map_err(|error| io_error("replace state file", error))?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn read_private_file(path: &Path, max_bytes: usize, label: &str) -> Result<Vec<u8>, StoreError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error(&format!("inspect {label}"), error))?;
    validate_private_file(path, &metadata)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io_error(&format!("open {label}"), error))?;
    validate_private_file(
        path,
        &file
            .metadata()
            .map_err(|error| io_error(&format!("inspect open {label}"), error))?,
    )?;
    let mut body = Vec::new();
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| io_error(&format!("read {label}"), error))?;
    if body.len() > max_bytes {
        return Err(StoreError::Corrupt(format!(
            "{label} {} exceeds size limit",
            path.display()
        )));
    }
    Ok(body)
}

fn ensure_private_directory(path: &Path) -> Result<(), StoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory_metadata(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|error| io_error("create private state directory", error))?;
            validate_private_directory(path)
        }
        Err(error) => Err(io_error("inspect private state directory", error)),
    }
}

fn validate_private_directory(path: &Path) -> Result<(), StoreError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect private state directory", error))?;
    validate_private_directory_metadata(path, &metadata)
}

fn validate_private_directory_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), StoreError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(StoreError::Io(format!(
            "insecure agent state directory: {}",
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
        return Err(StoreError::Io(format!(
            "insecure agent state file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn temporary_path(path: &Path) -> Result<PathBuf, StoreError> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| StoreError::Io(format!("invalid state path: {}", path.display())))?;
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| StoreError::Io(format!("obtain temporary file randomness: {error}")))?;
    Ok(path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".{file_name}.tmp.{}.{:016x}",
            std::process::id(),
            u64::from_be_bytes(random)
        )))
}

fn ensure_region_capacity(
    directory: &Path,
    replaced_size: Option<u64>,
    new_size: u64,
    record_limit: usize,
    byte_limit: u64,
    label: &str,
) -> Result<(), StoreError> {
    let current = region_capacity(directory, record_limit, label)?;
    next_region_capacity(
        current,
        replaced_size,
        new_size,
        record_limit,
        byte_limit,
        label,
    )?;
    Ok(())
}

fn next_region_capacity(
    current: RegionCapacity,
    replaced_size: Option<u64>,
    new_size: u64,
    record_limit: usize,
    byte_limit: u64,
    label: &str,
) -> Result<RegionCapacity, StoreError> {
    let next_records = current.records + usize::from(replaced_size.is_none());
    let next_bytes = current
        .bytes
        .saturating_sub(replaced_size.unwrap_or(0))
        .saturating_add(new_size);
    if next_records > record_limit || next_bytes > byte_limit {
        return Err(StoreError::Capacity(format!("{label} is full")));
    }
    Ok(RegionCapacity {
        records: next_records,
        bytes: next_bytes,
    })
}

fn existing_file_size(path: &Path) -> Result<Option<u64>, StoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_private_file(path, &metadata)?;
            Ok(Some(metadata.len()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("inspect existing state file", error)),
    }
}

fn directory_usage(
    directory: &Path,
    record_limit: usize,
    label: &str,
) -> Result<(usize, u64), StoreError> {
    let mut count = 0;
    let mut bytes = 0_u64;
    for entry in
        std::fs::read_dir(directory).map_err(|error| io_error("scan state directory", error))?
    {
        let entry = entry.map_err(|error| io_error("scan state directory entry", error))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| io_error("inspect state directory entry", error))?;
        if entry.file_name().to_string_lossy().contains(".tmp.") {
            continue;
        }
        validate_private_file(&path, &metadata)?;
        count += 1;
        if count > record_limit {
            return Err(StoreError::Capacity(format!(
                "{label} exceeds its record limit"
            )));
        }
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or_else(|| StoreError::Corrupt(format!("{label} byte accounting overflow")))?;
    }
    Ok((count, bytes))
}

fn region_capacity(
    directory: &Path,
    record_limit: usize,
    label: &str,
) -> Result<RegionCapacity, StoreError> {
    let (records, bytes) = directory_usage(directory, record_limit, label)?;
    Ok(RegionCapacity { records, bytes })
}

fn region_usage(
    directory: &Path,
    record_limit: usize,
    byte_limit: u64,
    label: &str,
    observed_times: impl Iterator<Item = i64>,
) -> Result<StoreRegionUsage, StoreError> {
    let (records, bytes) = directory_usage(directory, record_limit, label)?;
    Ok(StoreRegionUsage {
        records,
        bytes,
        record_limit,
        byte_limit,
        oldest_retained_at: observed_times.min(),
    })
}

fn canonical_files(
    directory: &Path,
    suffix: &str,
    record_limit: usize,
    label: &str,
) -> Result<Vec<PathBuf>, StoreError> {
    let mut paths = Vec::new();
    for_each_canonical_file(directory, suffix, record_limit, label, |path| {
        paths.push(path);
        Ok(())
    })?;
    paths.sort();
    Ok(paths)
}

fn for_each_canonical_file(
    directory: &Path,
    suffix: &str,
    record_limit: usize,
    label: &str,
    mut visitor: impl FnMut(PathBuf) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let mut records = 0;
    for entry in
        std::fs::read_dir(directory).map_err(|error| io_error("scan record directory", error))?
    {
        let entry = entry.map_err(|error| io_error("scan record directory entry", error))?;
        let name = entry.file_name().into_string().map_err(|_| {
            StoreError::Corrupt(format!(
                "non-UTF-8 file name in {label}: {}",
                entry.path().display()
            ))
        })?;
        if name.contains(".tmp.") {
            continue;
        }
        if !name.ends_with(suffix) {
            return Err(StoreError::Corrupt(format!(
                "unexpected canonical state file: {}",
                entry.path().display()
            )));
        }
        records += 1;
        if records > record_limit {
            return Err(StoreError::Capacity(format!(
                "{label} exceeds its record limit"
            )));
        }
        visitor(entry.path())?;
    }
    Ok(())
}

fn cleanup_body_directory(
    directory: &Path,
    referenced: &BTreeSet<String>,
) -> Result<(), StoreError> {
    let mut changed = false;
    for entry in
        std::fs::read_dir(directory).map_err(|error| io_error("scan body directory", error))?
    {
        let entry = entry.map_err(|error| io_error("scan body directory entry", error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(".tmp.") || !referenced.contains(&name) {
            std::fs::remove_file(entry.path())
                .map_err(|error| io_error("remove orphan body", error))?;
            changed = true;
        }
    }
    if changed {
        sync_directory(directory)?;
    }
    Ok(())
}

fn cleanup_temporary_files(directory: &Path) -> Result<(), StoreError> {
    let mut changed = false;
    for entry in
        std::fs::read_dir(directory).map_err(|error| io_error("scan record directory", error))?
    {
        let entry = entry.map_err(|error| io_error("scan record directory entry", error))?;
        if entry.file_name().to_string_lossy().contains(".tmp.") {
            std::fs::remove_file(entry.path())
                .map_err(|error| io_error("remove temporary record", error))?;
            changed = true;
        }
    }
    if changed {
        sync_directory(directory)?;
    }
    Ok(())
}

fn remove_file_and_sync(path: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_directory(path.parent().unwrap_or_else(|| Path::new("."))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("remove private state file", error)),
    }
}

fn remove_files_and_sync(
    directory: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<(), StoreError> {
    let mut changed = false;
    for path in paths {
        if path.parent() != Some(directory) {
            return Err(StoreError::Invalid(format!(
                "cleanup path escapes state directory: {}",
                path.display()
            )));
        }
        match std::fs::remove_file(&path) {
            Ok(()) => changed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("remove private state file", error)),
        }
    }
    if changed {
        sync_directory(directory)?;
    }
    Ok(())
}

fn clear_directory_and_sync(directory: &Path) -> Result<(), StoreError> {
    for entry in
        std::fs::read_dir(directory).map_err(|error| io_error("scan reset directory", error))?
    {
        let entry = entry.map_err(|error| io_error("scan reset directory entry", error))?;
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|error| io_error("inspect reset directory entry", error))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Err(StoreError::Corrupt(format!(
                "unexpected directory in reset region: {}",
                entry.path().display()
            )));
        }
        std::fs::remove_file(entry.path())
            .map_err(|error| io_error("clear reset directory entry", error))?;
    }
    sync_directory(directory)
}

fn sync_directory(directory: &Path) -> Result<(), StoreError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("sync state directory", error))
}

fn utf8_suffix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

fn io_error(action: &str, error: std::io::Error) -> StoreError {
    StoreError::Io(format!("failed to {action}: {error}"))
}

fn missing_reference_error(error: StoreError, context: &str) -> StoreError {
    StoreError::Corrupt(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

    use super::*;
    use crate::agent_state::{
        AgentBinding, DispatchState, ExecutionPhase, OperationResultReceipt, ResolutionId,
        ResolutionKind, RunEvidenceSummary, RunResolution,
    };
    use crate::daemon::topology::ServerIdentity;
    use crate::hook::provider::{ProviderCompleteness as CandidateCompleteness, ResponseCandidate};
    use crate::pane_state::{
        AgentKind, AgentProcessIdentity, AgentSessionId, PaneInstance, StateId, StateVersion,
    };

    fn temp_root() -> PathBuf {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        std::env::temp_dir().join(format!(
            "vde-tmux-agent-state-test-{}-{:016x}",
            std::process::id(),
            u64::from_be_bytes(random)
        ))
    }

    fn binding() -> AgentBinding {
        AgentBinding {
            server_identity: ServerIdentity {
                pid: 123,
                start_time: 456,
            },
            pane_instance: PaneInstance {
                pane_id: "%7".to_string(),
                pane_pid: 700,
            },
            pane_state_id: StateId::parse("3".repeat(32)).unwrap(),
            agent_epoch: 2,
            agent_kind: AgentKind::parse("codex").unwrap(),
            provider_session_id: AgentSessionId::parse("session-7").unwrap(),
            process: AgentProcessIdentity {
                pid: 701,
                start_token: "process-token".to_string(),
            },
        }
    }

    fn expected_pane_version() -> StateVersion {
        StateVersion {
            state_id: StateId::parse("3".repeat(32)).unwrap(),
            agent_epoch: 2,
            revision: 9,
        }
    }

    fn prompt_digest(prompt: &str) -> Sha256Digest {
        Sha256Digest::parse(crate::pane_state::PromptState::digest_decoded_prompt(
            prompt,
        ))
        .unwrap()
    }

    fn completed_run(store: &AgentStateStore) -> RunRecord {
        RunRecord {
            state_format_version: super::super::PRIVATE_STATE_FORMAT_VERSION,
            generation: store.generation().clone(),
            run_id: StableRunId::parse("4".repeat(32)).unwrap(),
            run_seq: 1,
            revision: 1,
            binding: binding(),
            provider_turn_key: Some("turn-1".to_string()),
            operation_id: None,
            execution_phase: ExecutionPhase::Ended,
            semantic_outcome: SemanticOutcome::Completed,
            evidence: RunEvidenceSummary::default(),
            resolution: Some(RunResolution {
                resolution_id: ResolutionId::parse("resolution_12345").unwrap(),
                kind: ResolutionKind::ProviderCompleted,
                resolved_at: 2,
                operator_audit: None,
            }),
            artifact: None,
            created_at: 1,
            updated_at: 2,
        }
    }

    fn completed_run_with(
        store: &AgentStateStore,
        ordinal: u64,
        created_at: i64,
        pane_number: u32,
    ) -> RunRecord {
        let mut run = completed_run(store);
        run.run_id = StableRunId::parse(format!("{ordinal:032x}")).unwrap();
        run.run_seq = ordinal;
        run.binding.pane_instance.pane_id = format!("%{pane_number}");
        run.binding.pane_instance.pane_pid = pane_number + 700;
        run.created_at = created_at;
        run.updated_at = created_at + 1;
        run.resolution.as_mut().unwrap().resolved_at = created_at + 1;
        run
    }

    fn prepared_operation(store: &AgentStateStore, operation_id: OperationId) -> OperationRecord {
        OperationRecord {
            state_format_version: super::super::PRIVATE_STATE_FORMAT_VERSION,
            generation: store.generation().clone(),
            operation_id,
            revision: 1,
            request_fingerprint: Sha256Digest::of(b"request"),
            target_agent_ref: "vta3-example".to_string(),
            prompt_digest: prompt_digest("ship it"),
            dispatch_option: "stdin".to_string(),
            binding: binding(),
            expected_pane_version: expected_pane_version(),
            expected_current_run: None,
            expected_run_seq: 2,
            confirmation_deadline_at: 30,
            dispatch_state: DispatchState::Prepared,
            run_id: None,
            result_receipt: None,
            created_at: 3,
            updated_at: 3,
        }
    }

    #[test]
    fn initializes_private_layout_and_reopens_same_generation() {
        let root = temp_root();
        let first = AgentStateStore::open_or_initialize(&root).unwrap();
        let generation = first.generation().clone();
        drop(first);
        let second = AgentStateStore::open_or_initialize(&root).unwrap();
        assert_eq!(second.generation(), &generation);
        for path in [
            root.clone(),
            root.join(RUNS_DIRECTORY),
            root.join(OPERATIONS_DIRECTORY),
            root.join(PROMPTS_DIRECTORY),
            root.join(ARTIFACTS_DIRECTORY),
        ] {
            assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o700);
        }
        let meta = std::fs::metadata(root.join(STATE_META_FILE)).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_staging_is_bounded_private_and_idempotent() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let operation_id = OperationId::parse("operation_123456").unwrap();
        store.stage_prompt(&operation_id, b"hello\nworld").unwrap();
        store.stage_prompt(&operation_id, b"hello\nworld").unwrap();
        assert!(store.stage_prompt(&operation_id, b"different").is_err());
        assert_eq!(store.read_prompt(&operation_id).unwrap(), b"hello\nworld");
        let metadata = std::fs::metadata(store.prompt_path(&operation_id)).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        store.delete_prompt(&operation_id).unwrap();
        assert!(store.read_prompt(&operation_id).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn utf8_suffix_preserves_boundary_and_maximum() {
        let body = format!("{}終わり", "a".repeat(RESPONSE_ARTIFACT_BODY_MAX_BYTES));
        let suffix = utf8_suffix(&body, RESPONSE_ARTIFACT_BODY_MAX_BYTES);
        assert!(suffix.is_char_boundary(0));
        assert!(suffix.len() <= RESPONSE_ARTIFACT_BODY_MAX_BYTES);
        assert!(suffix.ends_with("終わり"));
    }

    #[test]
    fn cleanup_removes_unreferenced_final_and_temporary_bodies() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        write_bytes_atomic(&store.prompts_dir().join("orphan.prompt"), b"secret").unwrap();
        write_bytes_atomic(&store.artifacts_dir().join("orphan.response"), b"body").unwrap();
        write_bytes_atomic(&store.prompts_dir().join(".body.tmp.1"), b"secret").unwrap();
        store.cleanup_orphans().unwrap();
        assert_eq!(std::fs::read_dir(store.prompts_dir()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(store.artifacts_dir()).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_roundtrip_as_independent_atomic_files() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let run = completed_run(&store);
        store.save_run(&run).unwrap();
        assert_eq!(store.load_run(&run.run_id).unwrap(), Some(run.clone()));
        assert_eq!(store.list_runs().unwrap(), vec![run]);

        let operation_id = OperationId::parse("operation_123456").unwrap();
        store.stage_prompt(&operation_id, b"ship it").unwrap();
        let operation = OperationRecord {
            state_format_version: super::super::PRIVATE_STATE_FORMAT_VERSION,
            generation: store.generation().clone(),
            operation_id: operation_id.clone(),
            revision: 1,
            request_fingerprint: Sha256Digest::of(b"request"),
            target_agent_ref: "vta3-example".to_string(),
            prompt_digest: prompt_digest("ship it"),
            dispatch_option: "stdin".to_string(),
            binding: binding(),
            expected_pane_version: expected_pane_version(),
            expected_current_run: None,
            expected_run_seq: 2,
            confirmation_deadline_at: 30,
            dispatch_state: DispatchState::Prepared,
            run_id: None,
            result_receipt: None,
            created_at: 3,
            updated_at: 3,
        };
        store.save_operation(&operation).unwrap();
        assert_eq!(
            store.load_operation(&operation_id).unwrap(),
            Some(operation.clone())
        );
        assert_eq!(store.list_operations().unwrap(), vec![operation]);
        assert!(std::fs::read_dir(store.runs_dir()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_is_truncated_at_utf8_boundary_verified_and_expired_in_order() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let run = completed_run(&store);
        store.save_run(&run).unwrap();
        let body = format!(
            "prefix-{}終端",
            "x".repeat(RESPONSE_ARTIFACT_BODY_MAX_BYTES)
        );
        let metadata = store
            .store_artifact(
                &run.run_id,
                &body,
                ArtifactObservation {
                    provider_completeness: ProviderCompleteness::Unknown,
                    source: "provider_hook".to_string(),
                    observed_at: 4,
                },
            )
            .unwrap();
        assert_eq!(
            metadata.store_completeness,
            ArtifactStoreCompleteness::Truncated
        );
        let stored = store.read_artifact(&run.run_id).unwrap();
        assert!(stored.len() <= RESPONSE_ARTIFACT_BODY_MAX_BYTES);
        assert!(stored.ends_with("終端"));
        let file = store.artifacts_dir().join(metadata.file_name.unwrap());
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);

        store.expire_artifact(&run.run_id).unwrap();
        assert!(matches!(
            store.read_artifact(&run.run_id),
            Err(StoreError::ArtifactExpired)
        ));
        assert!(!file.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_candidate_preserves_provider_counts_digests_and_completeness() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let run = completed_run(&store);
        store.save_run(&run).unwrap();
        let body = format!("{}応答", "x".repeat(RESPONSE_ARTIFACT_BODY_MAX_BYTES));
        let candidate = ResponseCandidate::from_body(&body, CandidateCompleteness::Unknown);

        let metadata = store
            .store_artifact_candidate(&run.run_id, &candidate, "provider_transcript", 5)
            .unwrap();

        assert_eq!(metadata.original_byte_count, candidate.original_bytes);
        assert_eq!(metadata.stored_byte_count, candidate.stored_bytes);
        assert_eq!(metadata.original_digest.as_str(), candidate.original_digest);
        assert_eq!(
            metadata.stored_digest.as_ref().unwrap().as_str(),
            candidate.stored_digest
        );
        assert_eq!(
            metadata.provider_completeness,
            ProviderCompleteness::Unknown
        );
        assert_eq!(
            metadata.store_completeness,
            ArtifactStoreCompleteness::Truncated
        );
        assert_eq!(
            store.read_artifact(&run.run_id).unwrap().len() as u64,
            candidate.stored_bytes
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_candidate_keeps_metadata_when_body_write_is_unavailable() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let run = completed_run(&store);
        store.save_run(&run).unwrap();
        let candidate =
            ResponseCandidate::from_body("final response", CandidateCompleteness::Complete);
        std::fs::set_permissions(
            store.artifacts_dir(),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();

        let metadata = store
            .store_artifact_candidate(&run.run_id, &candidate, "provider_hook", 5)
            .unwrap();

        assert_eq!(
            metadata.store_completeness,
            ArtifactStoreCompleteness::Unavailable
        );
        assert_eq!(metadata.original_byte_count, candidate.original_bytes);
        assert_eq!(metadata.stored_byte_count, candidate.stored_bytes);
        assert_eq!(metadata.original_digest.as_str(), candidate.original_digest);
        assert_eq!(
            metadata.stored_digest.as_ref().unwrap().as_str(),
            candidate.stored_digest
        );
        assert!(metadata.file_name.is_none());
        std::fs::set_permissions(
            store.artifacts_dir(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_candidate_rejects_tampered_body_metadata_before_run_update() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let run = completed_run(&store);
        store.save_run(&run).unwrap();
        let mut candidate =
            ResponseCandidate::from_body("final response", CandidateCompleteness::Complete);
        candidate.stored_digest = "0".repeat(64);

        assert!(matches!(
            store.store_artifact_candidate(&run.run_id, &candidate, "provider_hook", 5),
            Err(StoreError::Invalid(_))
        ));
        assert!(
            store
                .load_run(&run.run_id)
                .unwrap()
                .unwrap()
                .artifact
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn final_operation_can_be_saved_without_prompt_body_in_record() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let operation = OperationRecord {
            state_format_version: super::super::PRIVATE_STATE_FORMAT_VERSION,
            generation: store.generation().clone(),
            operation_id: OperationId::parse("operation_654321").unwrap(),
            revision: 2,
            request_fingerprint: Sha256Digest::of(b"request"),
            target_agent_ref: "vta3-example".to_string(),
            prompt_digest: prompt_digest("private prompt"),
            dispatch_option: "stdin".to_string(),
            binding: binding(),
            expected_pane_version: expected_pane_version(),
            expected_current_run: None,
            expected_run_seq: 2,
            confirmation_deadline_at: 30,
            dispatch_state: DispatchState::Rejected,
            run_id: None,
            result_receipt: Some(OperationResultReceipt {
                code: "preflight_rejected".to_string(),
                observed_at: 4,
                confirmation_basis: None,
                source_attribution: None,
            }),
            created_at: 3,
            updated_at: 4,
        };
        store.save_operation(&operation).unwrap();
        let encoded = std::fs::read(store.operation_path(&operation.operation_id)).unwrap();
        assert!(
            !encoded
                .windows(b"private prompt".len())
                .any(|window| window == b"private prompt")
        );
        AgentStateStore::open_or_initialize(&root).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_resetting_metadata() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let resetting = StateMeta::resetting(
            store.generation().clone(),
            StateGeneration::generate().unwrap(),
        )
        .unwrap();
        write_json_atomic(
            &root.join(STATE_META_FILE),
            &resetting,
            4096,
            "state metadata",
        )
        .unwrap();
        assert!(matches!(
            AgentStateStore::open_or_initialize(&root),
            Err(StoreError::StateUninitialized)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_to_regenerate_metadata_for_an_existing_empty_root() {
        let root = temp_root();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        assert!(matches!(
            AgentStateStore::open_or_initialize(&root),
            Err(StoreError::StateUninitialized)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_uninitialized_recovery_replaces_only_invalid_metadata() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        assert!(matches!(
            AgentStateStore::recover_uninitialized_offline(&root),
            Err(StoreError::RecoveryNotAllowed(_))
        ));
        write_bytes_atomic(&store.prompts_dir().join("orphan.body"), b"private").unwrap();
        std::fs::remove_file(root.join(STATE_META_FILE)).unwrap();

        let recovered = AgentStateStore::recover_uninitialized_offline(&root).unwrap();
        let reopened = AgentStateStore::open_or_initialize(&root).unwrap();
        assert_eq!(reopened.generation(), &recovered);
        assert!(
            std::fs::read_dir(reopened.prompts_dir())
                .unwrap()
                .next()
                .is_none()
        );
        drop(reopened);

        let mut future = StateMeta::new_ready().unwrap();
        future.state_format_version = super::super::PRIVATE_STATE_FORMAT_VERSION + 1;
        write_json_atomic(&root.join(STATE_META_FILE), &future, 4096, "state metadata").unwrap();
        let future_recovered = AgentStateStore::recover_uninitialized_offline(&root).unwrap();
        assert_ne!(future_recovered, recovered);
        assert_eq!(
            AgentStateStore::open_or_initialize(&root)
                .unwrap()
                .generation(),
            &future_recovered
        );
        std::fs::remove_dir_all(root).unwrap();

        let absent_root = temp_root();
        let absent_recovered =
            AgentStateStore::recover_uninitialized_offline(&absent_root).unwrap();
        assert_eq!(
            AgentStateStore::open_or_initialize(&absent_root)
                .unwrap()
                .generation(),
            &absent_recovered
        );
        std::fs::remove_dir_all(absent_root).unwrap();

        let empty_root = temp_root();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&empty_root)
            .unwrap();
        let empty_recovered = AgentStateStore::recover_uninitialized_offline(&empty_root).unwrap();
        assert_eq!(
            AgentStateStore::open_or_initialize(&empty_root)
                .unwrap()
                .generation(),
            &empty_recovered
        );
        std::fs::remove_dir_all(empty_root).unwrap();
    }

    #[test]
    fn canonical_scan_fails_closed_at_limit_plus_one_without_growing_the_result() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        write_bytes_atomic(&store.runs_dir().join("a.json"), b"{}").unwrap();
        write_bytes_atomic(&store.runs_dir().join("b.json"), b"{}").unwrap();

        assert!(matches!(
            canonical_files(&store.runs_dir(), ".json", 1, "test run store"),
            Err(StoreError::Capacity(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_store_capacity_boundaries_fail_closed_without_requiring_large_fixtures() {
        for (label, record_limit, byte_limit) in [
            ("run store", RUN_STORE_MAX_RECORDS, RUN_STORE_MAX_BYTES),
            (
                "operation store",
                OPERATION_STORE_MAX_RECORDS,
                OPERATION_STORE_MAX_BYTES,
            ),
            (
                "prompt store",
                PROMPT_STORE_MAX_RECORDS,
                PROMPT_STORE_MAX_BYTES,
            ),
            (
                "artifact store",
                ARTIFACT_STORE_MAX_FILES,
                ARTIFACT_STORE_MAX_BYTES,
            ),
        ] {
            assert!(matches!(
                next_region_capacity(
                    RegionCapacity {
                        records: record_limit,
                        bytes: 0,
                    },
                    None,
                    1,
                    record_limit,
                    byte_limit,
                    label,
                ),
                Err(StoreError::Capacity(_))
            ));
            assert!(matches!(
                next_region_capacity(
                    RegionCapacity {
                        records: 1,
                        bytes: byte_limit,
                    },
                    Some(1),
                    2,
                    record_limit,
                    byte_limit,
                    label,
                ),
                Err(StoreError::Capacity(_))
            ));
            assert_eq!(
                next_region_capacity(
                    RegionCapacity {
                        records: record_limit,
                        bytes: byte_limit,
                    },
                    Some(1),
                    1,
                    record_limit,
                    byte_limit,
                    label,
                )
                .unwrap(),
                RegionCapacity {
                    records: record_limit,
                    bytes: byte_limit,
                }
            );
        }
    }

    #[test]
    fn startup_rejects_insecure_or_tampered_referenced_bodies() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let operation_id = OperationId::parse("operation_permission").unwrap();
        store.stage_prompt(&operation_id, b"ship it").unwrap();
        store
            .save_operation(&prepared_operation(&store, operation_id.clone()))
            .unwrap();
        std::fs::set_permissions(
            store.prompt_path(&operation_id),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(matches!(
            AgentStateStore::open_or_initialize(&root),
            Err(StoreError::Corrupt(_))
        ));
        std::fs::set_permissions(
            store.prompt_path(&operation_id),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        let run = completed_run(&store);
        store.save_run(&run).unwrap();
        let metadata = store
            .store_artifact(
                &run.run_id,
                "trusted response",
                ArtifactObservation {
                    provider_completeness: ProviderCompleteness::Complete,
                    source: "provider_hook".to_string(),
                    observed_at: 4,
                },
            )
            .unwrap();
        let artifact_path = store
            .artifacts_dir()
            .join(metadata.file_name.as_ref().unwrap());
        write_bytes_atomic(&artifact_path, b"untrust response").unwrap();
        assert!(matches!(
            AgentStateStore::open_or_initialize(&root),
            Err(StoreError::Corrupt(_))
        ));

        std::fs::remove_file(&artifact_path).unwrap();
        let target = root.join("symlink-target");
        write_bytes_atomic(&target, b"trusted response").unwrap();
        symlink(&target, &artifact_path).unwrap();
        assert!(matches!(
            AgentStateStore::open_or_initialize(&root),
            Err(StoreError::Corrupt(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_retention_preserves_protected_run_and_recovers_capacity() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let mut protected = None;
        for ordinal in 1..=66 {
            let run = completed_run_with(&store, ordinal, 100 + ordinal as i64, 7);
            if ordinal == 1 {
                protected = Some(run.run_id.clone());
            }
            store.save_run(&run).unwrap();
        }

        let cleanup = store
            .collect_run_retention(
                1_000,
                [protected.clone().unwrap()],
                RunRetentionReserve::default(),
                None,
            )
            .unwrap();
        assert_eq!(cleanup.removed_run_ids.len(), 1);
        assert!(store.load_run(&protected.unwrap()).unwrap().is_some());
        assert_eq!(store.list_runs().unwrap().len(), 65);

        let replacement = completed_run_with(&store, 67, 1_001, 7);
        store.save_run(&replacement).unwrap();
        assert_eq!(store.list_runs().unwrap().len(), 66);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_capacity_expires_oldest_completed_body_first() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let first = completed_run_with(&store, 1, 1, 7);
        let second = completed_run_with(&store, 2, 2, 7);
        let third = completed_run_with(&store, 3, 3, 7);
        for run in [&first, &second, &third] {
            store.save_run(run).unwrap();
        }
        for (run, observed_at) in [(&first, 10), (&second, 20)] {
            store
                .store_artifact(
                    &run.run_id,
                    "body",
                    ArtifactObservation {
                        provider_completeness: ProviderCompleteness::Complete,
                        source: "provider_hook".to_string(),
                        observed_at,
                    },
                )
                .unwrap();
        }

        store
            .ensure_artifact_capacity_with_limits(
                &third.run_id,
                &store
                    .artifacts_dir()
                    .join(artifact_file_name(&third.run_id)),
                4,
                2,
                8,
            )
            .unwrap();
        assert!(matches!(
            store.read_artifact(&first.run_id),
            Err(StoreError::ArtifactExpired)
        ));
        assert_eq!(store.read_artifact(&second.run_id).unwrap(), "body");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_reset_resumes_existing_marker_and_publishes_target_generation_last() {
        let root = temp_root();
        let store = AgentStateStore::open_or_initialize(&root).unwrap();
        let old_generation = store.generation().clone();
        let run = completed_run(&store);
        store.save_run(&run).unwrap();
        let operation_id = OperationId::parse("operation_reset_1").unwrap();
        store.stage_prompt(&operation_id, b"ship it").unwrap();
        store
            .save_operation(&prepared_operation(&store, operation_id))
            .unwrap();
        store
            .store_artifact(
                &run.run_id,
                "response",
                ArtifactObservation {
                    provider_completeness: ProviderCompleteness::Complete,
                    source: "provider_hook".to_string(),
                    observed_at: 4,
                },
            )
            .unwrap();

        let target_generation = StateGeneration::generate().unwrap();
        let resetting =
            StateMeta::resetting(old_generation.clone(), target_generation.clone()).unwrap();
        write_json_atomic(
            &root.join(STATE_META_FILE),
            &resetting,
            4096,
            "state metadata",
        )
        .unwrap();
        std::fs::remove_file(store.run_path(&run.run_id)).unwrap();

        assert_eq!(
            AgentStateStore::reset_offline(&root, &old_generation).unwrap(),
            target_generation
        );
        let reopened = AgentStateStore::open_or_initialize(&root).unwrap();
        assert_eq!(reopened.generation(), &target_generation);
        for directory in [
            reopened.runs_dir(),
            reopened.operations_dir(),
            reopened.prompts_dir(),
            reopened.artifacts_dir(),
        ] {
            assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
        }
        assert!(matches!(
            AgentStateStore::reset_offline(&root, &old_generation),
            Err(StoreError::StalePrecondition(_))
        ));
        let following_generation =
            AgentStateStore::reset_offline(&root, &target_generation).unwrap();
        assert_ne!(following_generation, target_generation);
        assert_eq!(
            AgentStateStore::open_or_initialize(&root)
                .unwrap()
                .generation(),
            &following_generation
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_reset_rejects_active_store_before_writing_reset_marker() {
        let active_root = temp_root();
        let active_store = AgentStateStore::open_or_initialize(&active_root).unwrap();
        let active_generation = active_store.generation().clone();
        let mut active_run = completed_run(&active_store);
        active_run.execution_phase = ExecutionPhase::Running;
        active_run.semantic_outcome = SemanticOutcome::Unresolved;
        active_run.resolution = None;
        active_store.save_run(&active_run).unwrap();

        assert!(matches!(
            AgentStateStore::reset_offline(&active_root, &active_generation),
            Err(StoreError::RecoveryNotAllowed(_))
        ));
        let active_meta: StateMeta =
            read_json(&active_root.join(STATE_META_FILE), 4096, "state metadata").unwrap();
        assert_eq!(active_meta.status, StateMetaStatus::Ready);
        std::fs::remove_dir_all(active_root).unwrap();

        let operation_root = temp_root();
        let operation_store = AgentStateStore::open_or_initialize(&operation_root).unwrap();
        let operation_generation = operation_store.generation().clone();
        let operation_id = OperationId::parse("operation_reset_busy").unwrap();
        operation_store
            .stage_prompt(&operation_id, b"ship it")
            .unwrap();
        operation_store
            .save_operation(&prepared_operation(&operation_store, operation_id))
            .unwrap();

        assert!(matches!(
            AgentStateStore::reset_offline(&operation_root, &operation_generation),
            Err(StoreError::RecoveryNotAllowed(_))
        ));
        let operation_meta: StateMeta = read_json(
            &operation_root.join(STATE_META_FILE),
            4096,
            "state metadata",
        )
        .unwrap();
        assert_eq!(operation_meta.status, StateMetaStatus::Ready);
        std::fs::remove_dir_all(operation_root).unwrap();
    }
}

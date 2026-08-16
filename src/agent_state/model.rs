use std::fmt;

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest as _, Sha256};

use crate::daemon::topology::ServerIdentity;
use crate::pane_state::{
    AgentKind, AgentProcessIdentity, AgentSessionId, CurrentDurableRunProjection, LifecycleState,
    PaneInstance, StateId, StateVersion,
};

pub const PRIVATE_STATE_FORMAT_VERSION: u16 = 1;
pub const RUN_RECORD_MAX_BYTES: usize = 16 * 1024;
pub const RUN_EVIDENCE_MAX_BYTES: usize = 8 * 1024;
pub const RUN_EVENT_REFERENCE_MAX_COUNT: usize = 16;
pub const OPERATION_RECORD_MAX_BYTES: usize = 4 * 1024;
pub const PROMPT_BODY_MAX_BYTES: usize = 65_536;
pub const RESPONSE_ARTIFACT_BODY_MAX_BYTES: usize = 512 * 1024;
pub const VIEWPORT_FINGERPRINT_CONVENTION_VERSION: u16 = 1;

const IDENTIFIER_MAX_BYTES: usize = 256;
const REASON_MAX_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub String);

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ModelError {}

fn random_hex_128() -> Result<String, ModelError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| ModelError(format!("failed to obtain OS randomness: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn validate_random_id(value: &str, field: &str) -> Result<(), ModelError> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ModelError(format!(
            "{field} must be exactly 32 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

macro_rules! random_id_type {
    ($name:ident, $label:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn generate() -> Result<Self, ModelError> {
                Ok(Self(random_hex_128()?))
            }

            pub fn parse(value: impl Into<String>) -> Result<Self, ModelError> {
                let value = value.into();
                validate_random_id(&value, $label)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }
    };
}

random_id_type!(StateGeneration, "state generation");
random_id_type!(StableRunId, "stable run ID");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(String);

impl OperationId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        validate_caller_id(&value, "operation ID")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for OperationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolutionId(String);

impl ResolutionId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        validate_caller_id(&value, "resolution ID")?;
        Ok(Self(value))
    }

    pub fn generate() -> Result<Self, ModelError> {
        Ok(Self(random_hex_128()?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for ResolutionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ResolutionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

fn validate_caller_id(value: &str, field: &str) -> Result<(), ModelError> {
    if !(16..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ModelError(format!(
            "{field} must be 16 to 128 ASCII [A-Za-z0-9_-] characters"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ModelError("invalid SHA-256 digest".to_string()));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateMetaStatus {
    Ready,
    Resetting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateMeta {
    pub state_format_version: u16,
    pub status: StateMetaStatus,
    pub generation: StateGeneration,
    pub target_generation: Option<StateGeneration>,
}

impl StateMeta {
    pub fn new_ready() -> Result<Self, ModelError> {
        Ok(Self {
            state_format_version: PRIVATE_STATE_FORMAT_VERSION,
            status: StateMetaStatus::Ready,
            generation: StateGeneration::generate()?,
            target_generation: None,
        })
    }

    pub fn resetting(
        generation: StateGeneration,
        target_generation: StateGeneration,
    ) -> Result<Self, ModelError> {
        let meta = Self {
            state_format_version: PRIVATE_STATE_FORMAT_VERSION,
            status: StateMetaStatus::Resetting,
            generation,
            target_generation: Some(target_generation),
        };
        meta.validate()?;
        Ok(meta)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.state_format_version != PRIVATE_STATE_FORMAT_VERSION {
            return Err(ModelError(format!(
                "unsupported private state format version {}",
                self.state_format_version
            )));
        }
        match (&self.status, &self.target_generation) {
            (StateMetaStatus::Ready, None) => Ok(()),
            (StateMetaStatus::Resetting, Some(target)) if target != &self.generation => Ok(()),
            _ => Err(ModelError("invalid state generation metadata".to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBinding {
    pub server_identity: ServerIdentity,
    pub pane_instance: PaneInstance,
    pub pane_state_id: StateId,
    pub agent_epoch: u64,
    pub agent_kind: AgentKind,
    pub provider_session_id: AgentSessionId,
    pub process: AgentProcessIdentity,
}

impl AgentBinding {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.server_identity.pid == 0 || self.server_identity.start_time < 0 {
            return Err(ModelError("invalid tmux server identity".to_string()));
        }
        self.pane_instance
            .validate()
            .map_err(|error| ModelError(error.to_string()))?;
        if self.agent_epoch == 0 {
            return Err(ModelError("agent epoch must be positive".to_string()));
        }
        self.process
            .validate()
            .map_err(|error| ModelError(error.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    Running,
    Waiting,
    Error,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticOutcome {
    Unresolved,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionKind {
    ProviderCompleted,
    OperatorCompleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorAudit {
    pub actor_uid: u32,
    pub actor_pid: u32,
    pub reason: String,
    pub reason_digest: Sha256Digest,
    pub pre_revision: u64,
    pub post_revision: u64,
    pub observed_at: i64,
}

impl OperatorAudit {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_required_text(&self.reason, "operator reason", REASON_MAX_BYTES)?;
        if self.reason_digest != Sha256Digest::of(self.reason.as_bytes()) {
            return Err(ModelError("operator reason digest mismatch".to_string()));
        }
        if self.actor_pid == 0
            || self.post_revision != self.pre_revision.saturating_add(1)
            || self.observed_at < 0
        {
            return Err(ModelError("invalid operator audit fields".to_string()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResolution {
    pub resolution_id: ResolutionId,
    pub kind: ResolutionKind,
    pub resolved_at: i64,
    pub operator_audit: Option<OperatorAudit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryProcessExpectation {
    ExactAbsent,
    ReplacedBy { process: AgentProcessIdentity },
    ExactPresentStable { process: AgentProcessIdentity },
}

impl RecoveryProcessExpectation {
    pub fn validate(&self) -> Result<(), ModelError> {
        match self {
            Self::ExactAbsent => Ok(()),
            Self::ReplacedBy { process } | Self::ExactPresentStable { process } => process
                .validate()
                .map_err(|error| ModelError(error.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPaneFence {
    pub state_id: StateId,
    pub revision: u64,
    pub current_run: CurrentDurableRunProjection,
    pub lifecycle: LifecycleState,
    pub subagent_count: u32,
}

impl RecoveryPaneFence {
    pub fn validate(&self) -> Result<(), ModelError> {
        self.current_run
            .validate()
            .map_err(|error| ModelError(error.to_string()))?;
        if self.revision == 0 || self.subagent_count != 0 {
            return Err(ModelError("invalid recovery pane fence".to_string()));
        }
        if matches!(self.lifecycle, LifecycleState::Waiting { .. }) {
            return Err(ModelError(
                "a waiting pane cannot be operator-completed".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryViewportFingerprint {
    pub convention_version: u16,
    pub pane_width: u16,
    pub pane_height: u16,
    pub digest: Sha256Digest,
}

impl RecoveryViewportFingerprint {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.convention_version != VIEWPORT_FINGERPRINT_CONVENTION_VERSION
            || self.pane_width == 0
            || self.pane_height == 0
        {
            return Err(ModelError(
                "invalid recovery viewport fingerprint".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPrecondition {
    pub run_ref: String,
    pub binding: AgentBinding,
    pub run_revision: u64,
    pub evidence_digest: Sha256Digest,
    pub pane: RecoveryPaneFence,
    pub viewport_fingerprint: Option<RecoveryViewportFingerprint>,
    pub process_expectation: RecoveryProcessExpectation,
    pub issued_at: i64,
    pub expires_at: i64,
}

impl RecoveryPrecondition {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_required_text(&self.run_ref, "run reference", IDENTIFIER_MAX_BYTES * 4)?;
        self.binding.validate()?;
        self.pane.validate()?;
        self.process_expectation.validate()?;
        if self.pane.state_id != self.binding.pane_state_id {
            return Err(ModelError(
                "recovery pane state ID disagrees with binding".to_string(),
            ));
        }
        match (&self.process_expectation, &self.viewport_fingerprint) {
            (RecoveryProcessExpectation::ExactPresentStable { .. }, Some(fingerprint)) => {
                fingerprint.validate()?;
            }
            (RecoveryProcessExpectation::ExactPresentStable { .. }, None) => {
                return Err(ModelError(
                    "exact-present recovery requires a viewport fingerprint".to_string(),
                ));
            }
            (_, None) => {}
            (_, Some(_)) => {
                return Err(ModelError(
                    "only exact-present recovery may carry a viewport fingerprint".to_string(),
                ));
            }
        }
        if self.run_revision == 0
            || self.issued_at < 0
            || self.expires_at != self.issued_at.saturating_add(60)
        {
            return Err(ModelError(
                "invalid recovery precondition revision or validity window".to_string(),
            ));
        }
        Ok(())
    }
}

impl RunResolution {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.resolved_at < 0 {
            return Err(ModelError(
                "resolution timestamp must be non-negative".to_string(),
            ));
        }
        match (&self.kind, &self.operator_audit) {
            (ResolutionKind::ProviderCompleted, None) => Ok(()),
            (ResolutionKind::OperatorCompleted, Some(audit)) => audit.validate(),
            _ => Err(ModelError(
                "resolution and operator audit disagree".to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEventReference {
    pub event_ref: String,
    pub ingress_request_id: String,
    pub payload_digest: Sha256Digest,
    pub disposition: String,
    pub receipt: String,
    pub count: u32,
    pub first_observed_at: i64,
    pub last_observed_at: i64,
}

impl ProviderEventReference {
    pub fn validate(&self) -> Result<(), ModelError> {
        for (value, field) in [
            (&self.event_ref, "provider event reference"),
            (&self.ingress_request_id, "ingress request ID"),
            (&self.disposition, "provider event disposition"),
            (&self.receipt, "provider event receipt"),
        ] {
            validate_required_text(value, field, IDENTIFIER_MAX_BYTES)?;
        }
        if self.count == 0
            || self.first_observed_at < 0
            || self.last_observed_at < self.first_observed_at
        {
            return Err(ModelError("invalid provider event observation".to_string()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEvidenceSummary {
    pub provider_events: Vec<ProviderEventReference>,
    pub activity_count: u64,
    pub permission_request_count: u64,
    pub user_input_request_count: u64,
    pub process_absence_count: u64,
    pub terminal_still_count: u64,
    pub first_observed_at: Option<i64>,
    pub last_observed_at: Option<i64>,
}

impl RunEvidenceSummary {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.provider_events.len() > RUN_EVENT_REFERENCE_MAX_COUNT {
            return Err(ModelError("too many provider event references".to_string()));
        }
        for event in &self.provider_events {
            event.validate()?;
        }
        match (self.first_observed_at, self.last_observed_at) {
            (None, None) => {}
            (Some(first), Some(last)) if first >= 0 && last >= first => {}
            _ => return Err(ModelError("invalid evidence observation range".to_string())),
        }
        let encoded = serde_json::to_vec(self)
            .map_err(|error| ModelError(format!("failed to encode run evidence: {error}")))?;
        if encoded.len() > RUN_EVIDENCE_MAX_BYTES {
            return Err(ModelError(
                "run evidence exceeds the 8 KiB limit".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCompleteness {
    Complete,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStoreCompleteness {
    Complete,
    Truncated,
    Unavailable,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseArtifactMetadata {
    pub run_id: StableRunId,
    pub operation_id: Option<OperationId>,
    pub provider_session_id: AgentSessionId,
    pub observed_process: AgentProcessIdentity,
    pub original_byte_count: u64,
    pub stored_byte_count: u64,
    pub original_digest: Sha256Digest,
    pub stored_digest: Option<Sha256Digest>,
    pub provider_completeness: ProviderCompleteness,
    pub store_completeness: ArtifactStoreCompleteness,
    pub source: String,
    pub encoding: String,
    pub observed_at: i64,
    pub file_name: Option<String>,
}

impl ResponseArtifactMetadata {
    pub fn validate(&self) -> Result<(), ModelError> {
        self.observed_process
            .validate()
            .map_err(|error| ModelError(error.to_string()))?;
        validate_required_text(&self.source, "artifact source", IDENTIFIER_MAX_BYTES)?;
        if self.encoding != "utf-8" || self.observed_at < 0 {
            return Err(ModelError(
                "invalid response artifact encoding or time".to_string(),
            ));
        }
        match self.store_completeness {
            ArtifactStoreCompleteness::Complete | ArtifactStoreCompleteness::Truncated => {
                if self.stored_digest.is_none()
                    || self.file_name.as_deref() != Some(artifact_file_name(&self.run_id).as_str())
                    || self.stored_byte_count > RESPONSE_ARTIFACT_BODY_MAX_BYTES as u64
                    || (self.store_completeness == ArtifactStoreCompleteness::Complete
                        && (self.stored_byte_count != self.original_byte_count
                            || self.stored_digest.as_ref() != Some(&self.original_digest)))
                    || (self.store_completeness == ArtifactStoreCompleteness::Truncated
                        && self.stored_byte_count >= self.original_byte_count)
                {
                    return Err(ModelError(
                        "invalid stored response artifact metadata".to_string(),
                    ));
                }
            }
            ArtifactStoreCompleteness::Unavailable => {
                let unavailable_body_metadata_is_valid = match &self.stored_digest {
                    None => self.stored_byte_count == 0,
                    Some(_) => {
                        self.stored_byte_count <= RESPONSE_ARTIFACT_BODY_MAX_BYTES as u64
                            && self.stored_byte_count <= self.original_byte_count
                    }
                };
                if self.file_name.is_some() || !unavailable_body_metadata_is_valid {
                    return Err(ModelError(
                        "unavailable artifact has invalid candidate metadata".to_string(),
                    ));
                }
            }
            ArtifactStoreCompleteness::Expired => {
                if self.file_name.is_some() {
                    return Err(ModelError(
                        "expired artifact must not reference a body".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn artifact_file_name(run_id: &StableRunId) -> String {
    format!("{}.response", run_id.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub state_format_version: u16,
    pub generation: StateGeneration,
    pub run_id: StableRunId,
    pub run_seq: u64,
    pub revision: u64,
    pub binding: AgentBinding,
    pub provider_turn_key: Option<String>,
    pub operation_id: Option<OperationId>,
    pub execution_phase: ExecutionPhase,
    pub semantic_outcome: SemanticOutcome,
    pub evidence: RunEvidenceSummary,
    pub resolution: Option<RunResolution>,
    pub artifact: Option<ResponseArtifactMetadata>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl RunRecord {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.state_format_version != PRIVATE_STATE_FORMAT_VERSION {
            return Err(ModelError("run record state format mismatch".to_string()));
        }
        self.binding.validate()?;
        if self.run_seq == 0
            || self.created_at < 0
            || self.updated_at < self.created_at
            || self.provider_turn_key.as_ref().is_some_and(|value| {
                validate_required_text(value, "provider turn key", IDENTIFIER_MAX_BYTES).is_err()
            })
        {
            return Err(ModelError(
                "invalid run record identity or time".to_string(),
            ));
        }
        self.evidence.validate()?;
        match (
            self.execution_phase,
            self.semantic_outcome,
            &self.resolution,
        ) {
            (_, SemanticOutcome::Unresolved, None) => {}
            (ExecutionPhase::Ended, SemanticOutcome::Completed, Some(resolution)) => {
                resolution.validate()?;
            }
            _ => {
                return Err(ModelError(
                    "invalid run phase/outcome/resolution combination".to_string(),
                ));
            }
        }
        if let Some(artifact) = &self.artifact {
            artifact.validate()?;
            if artifact.run_id != self.run_id || artifact.operation_id != self.operation_id {
                return Err(ModelError(
                    "artifact identity disagrees with run".to_string(),
                ));
            }
            if self.semantic_outcome != SemanticOutcome::Completed {
                return Err(ModelError(
                    "unresolved run must not have a response artifact".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn execution_active(&self) -> bool {
        self.semantic_outcome == SemanticOutcome::Unresolved
            && !matches!(self.execution_phase, ExecutionPhase::Ended)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Prepared,
    DispatchStarted,
    PromptConfirmed,
    DeliveryUnknown,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationResultReceipt {
    pub code: String,
    pub observed_at: i64,
    pub confirmation_basis: Option<String>,
    pub source_attribution: Option<String>,
}

impl OperationResultReceipt {
    pub fn validate(&self) -> Result<(), ModelError> {
        validate_required_text(&self.code, "operation receipt code", IDENTIFIER_MAX_BYTES)?;
        if self.observed_at < 0 {
            return Err(ModelError(
                "invalid operation receipt timestamp".to_string(),
            ));
        }
        for (value, field) in [
            (&self.confirmation_basis, "confirmation basis"),
            (&self.source_attribution, "source attribution"),
        ] {
            if let Some(value) = value {
                validate_required_text(value, field, IDENTIFIER_MAX_BYTES)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub state_format_version: u16,
    pub generation: StateGeneration,
    pub operation_id: OperationId,
    pub revision: u64,
    pub request_fingerprint: Sha256Digest,
    pub target_agent_ref: String,
    pub prompt_digest: Sha256Digest,
    pub dispatch_option: String,
    pub binding: AgentBinding,
    pub expected_pane_version: StateVersion,
    pub expected_current_run: Option<CurrentDurableRunProjection>,
    pub expected_run_seq: u64,
    pub confirmation_deadline_at: i64,
    pub dispatch_state: DispatchState,
    pub run_id: Option<StableRunId>,
    pub result_receipt: Option<OperationResultReceipt>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl OperationRecord {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.state_format_version != PRIVATE_STATE_FORMAT_VERSION {
            return Err(ModelError(
                "operation record state format mismatch".to_string(),
            ));
        }
        self.binding.validate()?;
        if self.expected_pane_version.state_id != self.binding.pane_state_id
            || self.expected_pane_version.agent_epoch != self.binding.agent_epoch
        {
            return Err(ModelError(
                "operation pane precondition disagrees with Agent Binding".to_string(),
            ));
        }
        if let Some(current_run) = &self.expected_current_run {
            current_run
                .validate()
                .map_err(|error| ModelError(error.to_string()))?;
        }
        validate_required_text(
            &self.target_agent_ref,
            "target agent reference",
            IDENTIFIER_MAX_BYTES,
        )?;
        validate_required_text(
            &self.dispatch_option,
            "dispatch option",
            IDENTIFIER_MAX_BYTES,
        )?;
        if self.expected_run_seq == 0
            || self.created_at < 0
            || self.updated_at < self.created_at
            || self.confirmation_deadline_at < self.created_at
        {
            return Err(ModelError(
                "invalid operation record sequence or time".to_string(),
            ));
        }
        match (self.dispatch_state, &self.run_id, &self.result_receipt) {
            (DispatchState::Prepared | DispatchState::DispatchStarted, None, None) => {}
            (DispatchState::PromptConfirmed, Some(_), Some(receipt))
            | (DispatchState::DeliveryUnknown | DispatchState::Rejected, _, Some(receipt)) => {
                receipt.validate()?;
            }
            _ => {
                return Err(ModelError(
                    "invalid operation state/receipt combination".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn prompt_staging_required(&self) -> bool {
        matches!(
            self.dispatch_state,
            DispatchState::Prepared | DispatchState::DispatchStarted
        )
    }

    pub fn in_flight(&self) -> bool {
        matches!(
            self.dispatch_state,
            DispatchState::Prepared | DispatchState::DispatchStarted
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRef {
    pub server_identity: String,
    pub generation: StateGeneration,
    pub run_id: StableRunId,
}

impl RunRef {
    pub fn encode(&self) -> Result<String, ModelError> {
        validate_reference_server_identity(&self.server_identity)?;
        encode_reference("vtr3", self)
    }

    pub fn decode(value: &str) -> Result<Self, ModelError> {
        let reference: Self = decode_reference("vtr3", value)?;
        validate_reference_server_identity(&reference.server_identity)?;
        Ok(reference)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRef {
    pub server_identity: String,
    pub generation: StateGeneration,
    pub operation_id: OperationId,
}

impl OperationRef {
    pub fn encode(&self) -> Result<String, ModelError> {
        validate_reference_server_identity(&self.server_identity)?;
        encode_reference("vto3", self)
    }

    pub fn decode(value: &str) -> Result<Self, ModelError> {
        let reference: Self = decode_reference("vto3", value)?;
        validate_reference_server_identity(&reference.server_identity)?;
        Ok(reference)
    }
}

fn encode_reference<T: Serialize>(prefix: &str, value: &T) -> Result<String, ModelError> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| ModelError(format!("failed to encode reference: {error}")))?;
    Ok(format!(
        "{prefix}:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(encoded)
    ))
}

fn decode_reference<T: for<'de> Deserialize<'de>>(
    prefix: &str,
    value: &str,
) -> Result<T, ModelError> {
    let body = value
        .strip_prefix(&format!("{prefix}:"))
        .ok_or_else(|| ModelError("invalid reference prefix".to_string()))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| ModelError("invalid reference encoding".to_string()))?;
    if decoded.len() > 1024 {
        return Err(ModelError("reference exceeds size limit".to_string()));
    }
    serde_json::from_slice(&decoded)
        .map_err(|error| ModelError(format!("invalid reference payload: {error}")))
}

fn validate_reference_server_identity(value: &str) -> Result<(), ModelError> {
    if value.is_empty()
        || value.len() > IDENTIFIER_MAX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ModelError("invalid reference server identity".to_string()));
    }
    Ok(())
}

fn validate_required_text(value: &str, field: &str, max_bytes: usize) -> Result<(), ModelError> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(ModelError(format!("invalid {field}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_roundtrip_without_exposing_delimiter_constraints() {
        let run_ref = RunRef {
            server_identity: "server_hash-1".to_string(),
            generation: StateGeneration::parse("1".repeat(32)).unwrap(),
            run_id: StableRunId::parse("2".repeat(32)).unwrap(),
        };
        assert_eq!(RunRef::decode(&run_ref.encode().unwrap()).unwrap(), run_ref);

        let operation_ref = OperationRef {
            server_identity: "server_hash-1".to_string(),
            generation: StateGeneration::parse("1".repeat(32)).unwrap(),
            operation_id: OperationId::parse("operation_123456").unwrap(),
        };
        assert_eq!(
            OperationRef::decode(&operation_ref.encode().unwrap()).unwrap(),
            operation_ref
        );
    }

    #[test]
    fn state_meta_rejects_ready_target_and_same_reset_target() {
        let generation = StateGeneration::parse("1".repeat(32)).unwrap();
        let mut meta = StateMeta {
            state_format_version: PRIVATE_STATE_FORMAT_VERSION,
            status: StateMetaStatus::Ready,
            generation: generation.clone(),
            target_generation: Some(StateGeneration::parse("2".repeat(32)).unwrap()),
        };
        assert!(meta.validate().is_err());
        meta.status = StateMetaStatus::Resetting;
        meta.target_generation = Some(generation);
        assert!(meta.validate().is_err());
    }

    #[test]
    fn suffix_artifact_metadata_requires_strictly_smaller_stored_body() {
        let body = "response".as_bytes();
        let metadata = ResponseArtifactMetadata {
            run_id: StableRunId::parse("2".repeat(32)).unwrap(),
            operation_id: None,
            provider_session_id: AgentSessionId::parse("session").unwrap(),
            observed_process: AgentProcessIdentity {
                pid: 42,
                start_token: "token".to_string(),
            },
            original_byte_count: body.len() as u64,
            stored_byte_count: body.len() as u64,
            original_digest: Sha256Digest::of(body),
            stored_digest: Some(Sha256Digest::of(body)),
            provider_completeness: ProviderCompleteness::Unknown,
            store_completeness: ArtifactStoreCompleteness::Truncated,
            source: "provider_hook".to_string(),
            encoding: "utf-8".to_string(),
            observed_at: 1,
            file_name: Some("22222222222222222222222222222222.response".to_string()),
        };
        assert!(metadata.validate().is_err());
    }
}

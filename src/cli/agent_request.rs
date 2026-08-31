use std::collections::BTreeMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agent_state::{DispatchState, OperationId, OperationRef, Sha256Digest};
use crate::api::{
    ApiError, ApiErrorCode, ApiErrorStage, ApiRetryAction, ApiSideEffect,
    MAX_PROMPT_CONFIRM_TIMEOUT, OperationErrorReceipt,
};
use crate::tmux::TmuxRunner;

const REQUEST_STATE_FORMAT_VERSION: u16 = 1;
const REQUEST_STATE_MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RequestPhase {
    Active,
    OperationKnown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestState {
    format_version: u16,
    operation_id: OperationId,
    target_agent_ref: String,
    prompt_digest: Sha256Digest,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_body: Option<String>,
    phase: RequestPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_ref: Option<String>,
}

impl RequestState {
    fn active(target: &str, prompt: &str) -> Result<Self> {
        crate::api::validate_prompt(prompt)?;
        let operation_id = OperationId::generate().map_err(|error| {
            ApiError::new(
                ApiErrorCode::InternalError,
                format!("could not generate request operation ID: {error}"),
            )
        })?;
        let prompt_digest = prompt_digest(prompt);
        Ok(Self {
            format_version: REQUEST_STATE_FORMAT_VERSION,
            operation_id,
            target_agent_ref: target.to_string(),
            prompt_digest,
            prompt_body: Some(prompt.to_string()),
            phase: RequestPhase::Active,
            operation_ref: None,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.format_version != REQUEST_STATE_FORMAT_VERSION {
            return Err(request_state_invalid(format!(
                "unsupported request-state format version {}",
                self.format_version
            ))
            .into());
        }
        if !self.target_agent_ref.starts_with("vta1:") {
            return Err(
                request_state_invalid("request-state target is not an exact agent_ref").into(),
            );
        }
        match (self.phase, &self.prompt_body, &self.operation_ref) {
            (RequestPhase::Active, Some(prompt), None) => {
                crate::api::validate_prompt(prompt).map_err(|error| {
                    request_state_invalid(format!(
                        "request-state contains an invalid prompt body: {error:#}"
                    ))
                })?;
                if prompt_digest(prompt) != self.prompt_digest {
                    return Err(request_state_invalid(
                        "request-state prompt digest does not match its body",
                    )
                    .into());
                }
            }
            (RequestPhase::OperationKnown, None, Some(operation_ref)) => {
                let reference = OperationRef::decode(operation_ref).map_err(|error| {
                    request_state_invalid(format!(
                        "request-state contains an invalid operation_ref: {error}"
                    ))
                })?;
                if reference.operation_id != self.operation_id {
                    return Err(request_state_invalid(
                        "request-state operation_ref does not match its operation ID",
                    )
                    .into());
                }
            }
            _ => {
                return Err(request_state_invalid(
                    "request-state phase, prompt body, and operation_ref are inconsistent",
                )
                .into());
            }
        }
        Ok(())
    }

    fn remember_operation(&mut self, operation_ref: String) {
        self.phase = RequestPhase::OperationKnown;
        self.prompt_body = None;
        self.operation_ref = Some(operation_ref);
    }
}

#[derive(Debug)]
struct RequestLock {
    file: File,
}

impl RequestLock {
    fn acquire(state_path: &Path) -> Result<Self> {
        let parent = request_parent(state_path)?;
        validate_private_parent(parent)?;
        let lock_path = sidecar_lock_path(state_path)?;
        let file = open_or_create_lock(&lock_path)?;
        validate_private_file(
            &file.metadata().map_err(|error| {
                request_state_invalid(format!(
                    "could not inspect request-state lock {}: {error}",
                    lock_path.display()
                ))
            })?,
            &lock_path,
            "request-state lock",
        )?;

        // SAFETY: flock only observes the valid file descriptor owned by `file`.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                return Err(ApiError::new(
                    ApiErrorCode::RequestStateBusy,
                    format!(
                        "request-state {} is already in use by another process",
                        state_path.display()
                    ),
                )
                .into());
            }
            return Err(request_state_invalid(format!(
                "could not lock request-state {}: {error}",
                state_path.display()
            ))
            .into());
        }
        Ok(Self { file })
    }
}

impl Drop for RequestLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid for the duration of this call.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(super) fn execute(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    target: &str,
    state_path: &Path,
    supplied_prompt: Option<&str>,
    confirm_timeout: Duration,
) -> Result<String> {
    validate_request(target, supplied_prompt, confirm_timeout)?;
    let _lock = RequestLock::acquire(state_path)?;
    let supplied_digest = supplied_prompt.map(prompt_digest);
    let mut state = match read_state_if_present(state_path)? {
        Some(state) => state,
        None => {
            let prompt = supplied_prompt.ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::InvalidArguments,
                    "the first agent request call requires --stdin or --prompt-file",
                )
            })?;
            publish_initial_state(state_path, RequestState::active(target, prompt)?)?
        }
    };
    state.validate()?;
    validate_intent(&state, target, supplied_digest.as_ref())?;

    match state.phase {
        RequestPhase::Active => {
            execute_active(runner, env, state_path, &mut state, confirm_timeout)
        }
        RequestPhase::OperationKnown => crate::api::agent_prompt_resume(
            runner,
            env,
            observed_at,
            state
                .operation_ref
                .as_deref()
                .expect("validated operation-known request-state"),
            crate::api::PromptRequestIdentity {
                operation_id: &state.operation_id,
                target: &state.target_agent_ref,
                prompt_digest: &state.prompt_digest,
            },
            confirm_timeout,
        ),
    }
}

fn execute_active(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    state_path: &Path,
    state: &mut RequestState,
    confirm_timeout: Duration,
) -> Result<String> {
    let prompt = state
        .prompt_body
        .as_deref()
        .expect("validated active request-state");
    match crate::api::agent_prompt(
        runner,
        env,
        &state.target_agent_ref,
        state.operation_id.as_str(),
        prompt,
        confirm_timeout,
    ) {
        Ok(json) => {
            let receipt = success_operation_receipt(&json)?;
            validate_operation_receipt(state, &receipt)?;
            state.remember_operation(receipt.operation_ref.clone());
            if let Err(error) = replace_state(state_path, state) {
                return Err(persistence_after_operation(error, receipt).into());
            }
            Ok(json)
        }
        Err(error) => {
            let receipt = error
                .chain()
                .find_map(|source| source.downcast_ref::<ApiError>())
                .and_then(ApiError::operation_receipt);
            if let Some(receipt) = receipt
                && receipt_makes_request_observation_only(receipt.operation.dispatch_state)
            {
                validate_operation_receipt(state, &receipt)?;
                state.remember_operation(receipt.operation_ref.clone());
                if let Err(persist_error) = replace_state(state_path, state) {
                    return Err(persistence_after_operation(persist_error, receipt).into());
                }
            }
            Err(error)
        }
    }
}

fn receipt_makes_request_observation_only(dispatch_state: DispatchState) -> bool {
    dispatch_state != DispatchState::Prepared
}

fn validate_request(
    target: &str,
    supplied_prompt: Option<&str>,
    confirm_timeout: Duration,
) -> Result<()> {
    if !target.starts_with("vta1:") {
        return Err(ApiError::new(
            ApiErrorCode::InvalidArguments,
            "agent request requires an exact agent_ref target",
        )
        .into());
    }
    if confirm_timeout.is_zero() || confirm_timeout > MAX_PROMPT_CONFIRM_TIMEOUT {
        return Err(ApiError::new(
            ApiErrorCode::InvalidArguments,
            format!(
                "--confirm-timeout-ms must be between 1 and {}",
                MAX_PROMPT_CONFIRM_TIMEOUT.as_millis()
            ),
        )
        .into());
    }
    if let Some(prompt) = supplied_prompt {
        crate::api::validate_prompt(prompt)?;
    }
    Ok(())
}

fn validate_intent(
    state: &RequestState,
    target: &str,
    supplied_digest: Option<&Sha256Digest>,
) -> Result<()> {
    if state.target_agent_ref != target {
        return Err(ApiError::new(
            ApiErrorCode::RequestStateMismatch,
            "request-state belongs to a different exact agent target",
        )
        .into());
    }
    if supplied_digest.is_some_and(|digest| digest != &state.prompt_digest) {
        return Err(ApiError::new(
            ApiErrorCode::RequestStateMismatch,
            "supplied prompt does not match the request-state prompt digest",
        )
        .into());
    }
    Ok(())
}

fn prompt_digest(prompt: &str) -> Sha256Digest {
    Sha256Digest::parse(crate::pane_state::PromptState::digest_decoded_prompt(
        prompt,
    ))
    .expect("PromptState emits a valid SHA-256 digest")
}

fn request_parent(state_path: &Path) -> Result<&Path> {
    if state_path.file_name().is_none() {
        return Err(request_state_invalid("request-state path must name a file").into());
    }
    state_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            request_state_invalid(
                "request-state path must include an explicit private parent directory",
            )
            .into()
        })
}

fn validate_private_parent(parent: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(parent).map_err(|error| {
        request_state_invalid(format!(
            "could not inspect request-state parent {}: {error}",
            parent.display()
        ))
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(request_state_invalid(format!(
            "request-state parent {} must be a directory, not a symlink",
            parent.display()
        ))
        .into());
    }
    validate_owner_mode(&metadata, parent, "request-state parent", 0o700)
}

fn validate_private_file(metadata: &Metadata, path: &Path, label: &str) -> Result<()> {
    if !metadata.file_type().is_file() {
        return Err(request_state_invalid(format!(
            "{label} {} must be a regular file",
            path.display()
        ))
        .into());
    }
    validate_owner_mode(metadata, path, label, 0o600)
}

fn validate_owner_mode(
    metadata: &Metadata,
    path: &Path,
    label: &str,
    expected_mode: u32,
) -> Result<()> {
    // SAFETY: geteuid has no preconditions and does not modify process state.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        return Err(request_state_invalid(format!(
            "{label} {} must be owned by the current effective user",
            path.display()
        ))
        .into());
    }
    let actual_mode = metadata.mode() & 0o7777;
    if actual_mode != expected_mode {
        return Err(request_state_invalid(format!(
            "{label} {} must have mode {expected_mode:04o}, found {actual_mode:04o}",
            path.display()
        ))
        .into());
    }
    Ok(())
}

fn sidecar_lock_path(state_path: &Path) -> Result<PathBuf> {
    let mut name = state_path
        .file_name()
        .ok_or_else(|| request_state_invalid("request-state path must name a file"))?
        .to_os_string();
    name.push(".lock");
    Ok(request_parent(state_path)?.join(name))
}

fn open_or_create_lock(path: &Path) -> Result<File> {
    match secure_options().create_new(true).open(path) {
        Ok(file) => {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    request_state_invalid(format!(
                        "could not secure request-state lock {}: {error}",
                        path.display()
                    ))
                })?;
            Ok(file)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            secure_options().open(path).map_err(|error| {
                request_state_invalid(format!(
                    "could not open request-state lock {}: {error}",
                    path.display()
                ))
                .into()
            })
        }
        Err(error) => Err(request_state_invalid(format!(
            "could not create request-state lock {}: {error}",
            path.display()
        ))
        .into()),
    }
}

fn secure_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    options
}

fn read_state_if_present(path: &Path) -> Result<Option<RequestState>> {
    let mut file = match secure_options().open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(request_state_invalid(format!(
                "could not open request-state {}: {error}",
                path.display()
            ))
            .into());
        }
    };
    let metadata = file.metadata().map_err(|error| {
        request_state_invalid(format!(
            "could not inspect request-state {}: {error}",
            path.display()
        ))
    })?;
    validate_private_file(&metadata, path, "request-state")?;
    if metadata.len() > REQUEST_STATE_MAX_BYTES as u64 {
        return Err(request_state_invalid(format!(
            "request-state {} exceeds the {REQUEST_STATE_MAX_BYTES} byte limit",
            path.display()
        ))
        .into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((REQUEST_STATE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            request_state_invalid(format!(
                "could not read request-state {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() > REQUEST_STATE_MAX_BYTES {
        return Err(request_state_invalid(format!(
            "request-state {} exceeds the {REQUEST_STATE_MAX_BYTES} byte limit",
            path.display()
        ))
        .into());
    }
    let state: RequestState = serde_json::from_slice(&bytes).map_err(|error| {
        request_state_invalid(format!(
            "request-state {} is not valid JSON: {error}",
            path.display()
        ))
    })?;
    state.validate()?;
    Ok(Some(state))
}

fn publish_initial_state(path: &Path, state: RequestState) -> Result<RequestState> {
    let (temporary_path, temporary_file) = write_temporary_state(path, &state)?;
    let publish_result = std::fs::hard_link(&temporary_path, path);
    let cleanup_result = std::fs::remove_file(&temporary_path);
    if let Err(error) = cleanup_result {
        return Err(request_state_invalid(format!(
            "could not remove request-state temporary file {}: {error}",
            temporary_path.display()
        ))
        .into());
    }
    drop(temporary_file);
    match publish_result {
        Ok(()) => {
            sync_parent(path)?;
            Ok(state)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => read_state_if_present(path)?
            .ok_or_else(|| {
                request_state_invalid("request-state publish winner disappeared").into()
            }),
        Err(error) => Err(request_state_invalid(format!(
            "could not publish request-state {}: {error}",
            path.display()
        ))
        .into()),
    }
}

fn replace_state(path: &Path, state: &RequestState) -> Result<()> {
    state.validate()?;
    let (temporary_path, temporary_file) = write_temporary_state(path, state)?;
    drop(temporary_file);
    if let Err(error) = std::fs::rename(&temporary_path, path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(request_state_invalid(format!(
            "could not replace request-state {}: {error}",
            path.display()
        ))
        .into());
    }
    sync_parent(path)
}

fn write_temporary_state(path: &Path, state: &RequestState) -> Result<(PathBuf, File)> {
    let bytes = serde_json::to_vec(state).map_err(|error| {
        request_state_invalid(format!("could not encode request-state: {error}"))
    })?;
    if bytes.len() > REQUEST_STATE_MAX_BYTES {
        return Err(request_state_invalid(format!(
            "encoded request-state exceeds the {REQUEST_STATE_MAX_BYTES} byte limit"
        ))
        .into());
    }
    let parent = request_parent(path)?;
    for _ in 0..8 {
        let nonce = OperationId::generate().map_err(|error| {
            ApiError::new(
                ApiErrorCode::InternalError,
                format!("could not generate request-state temporary name: {error}"),
            )
        })?;
        let temporary_path = parent.join(format!(".vt-agent-request-{}.tmp", nonce.as_str()));
        match secure_options().create_new(true).open(&temporary_path) {
            Ok(mut file) => {
                if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
                    drop(file);
                    let _ = std::fs::remove_file(&temporary_path);
                    return Err(request_state_invalid(format!(
                        "could not secure request-state temporary file {}: {error}",
                        temporary_path.display()
                    ))
                    .into());
                }
                if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                    let _ = std::fs::remove_file(&temporary_path);
                    return Err(request_state_invalid(format!(
                        "could not persist request-state temporary file {}: {error}",
                        temporary_path.display()
                    ))
                    .into());
                }
                return Ok((temporary_path, file));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(request_state_invalid(format!(
                    "could not create request-state temporary file in {}: {error}",
                    parent.display()
                ))
                .into());
            }
        }
    }
    Err(request_state_invalid("could not allocate a unique request-state temporary file").into())
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = request_parent(path)?;
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            request_state_invalid(format!(
                "could not sync request-state parent {}: {error}",
                parent.display()
            ))
            .into()
        })
}

fn success_operation_receipt(json: &str) -> Result<OperationErrorReceipt> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
        ApiError::new(
            ApiErrorCode::InternalError,
            format!("agent prompt returned invalid JSON: {error}"),
        )
    })?;
    let operation_ref = value
        .pointer("/result/operation_ref")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::InternalError,
                "agent prompt success omitted result.operation_ref",
            )
        })?
        .to_string();
    let operation =
        serde_json::from_value(value.pointer("/result/operation").cloned().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::InternalError,
                "agent prompt success omitted result.operation",
            )
        })?)
        .map_err(|error| {
            ApiError::new(
                ApiErrorCode::InternalError,
                format!("agent prompt returned an invalid operation receipt: {error}"),
            )
        })?;
    Ok(OperationErrorReceipt {
        operation_ref,
        operation,
    })
}

fn validate_operation_receipt(state: &RequestState, receipt: &OperationErrorReceipt) -> Result<()> {
    let reference = OperationRef::decode(&receipt.operation_ref).map_err(|error| {
        ApiError::new(
            ApiErrorCode::InvalidDaemonResponse,
            format!("agent prompt returned an invalid operation_ref: {error}"),
        )
    })?;
    if reference.operation_id != state.operation_id
        || receipt.operation.operation_id != state.operation_id
        || receipt.operation.target_agent_ref != state.target_agent_ref
        || receipt.operation.prompt_digest != state.prompt_digest
    {
        return Err(ApiError::new(
            ApiErrorCode::InvalidDaemonResponse,
            "agent prompt receipt does not match the persisted request-state intent",
        )
        .into());
    }
    Ok(())
}

fn persistence_after_operation(error: anyhow::Error, receipt: OperationErrorReceipt) -> ApiError {
    let (side_effect, retry_action) = match receipt.operation.dispatch_state {
        DispatchState::Prepared => (ApiSideEffect::None, ApiRetryAction::RetrySameRequest),
        DispatchState::Rejected => (ApiSideEffect::None, ApiRetryAction::RefreshTarget),
        DispatchState::PromptConfirmed => {
            (ApiSideEffect::Confirmed, ApiRetryAction::InspectManually)
        }
        DispatchState::DispatchStarted | DispatchState::DeliveryUnknown => {
            (ApiSideEffect::Possible, ApiRetryAction::InspectManually)
        }
    };
    ApiError::new(
        ApiErrorCode::InternalError,
        format!(
            "operation {} was observed, but request-state persistence failed: {error:#}",
            receipt.operation_ref
        ),
    )
    .with_dispatch_context(
        ApiErrorStage::AfterDispatch,
        side_effect,
        retry_action,
        Some(receipt),
    )
}

fn request_state_invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::RequestStateInvalid, message)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    struct PrivateDirectory(PathBuf);

    impl PrivateDirectory {
        fn new() -> Self {
            let nonce = OperationId::generate().unwrap();
            let path = std::env::temp_dir().join(format!(
                "vde-tmux-request-state-test-{}-{}",
                std::process::id(),
                nonce.as_str()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn state_path(&self) -> PathBuf {
            self.0.join("request.json")
        }
    }

    impl Drop for PrivateDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn target() -> &'static str {
        "vta1:test_exact_agent_reference_1234567890"
    }

    fn operation_ref(operation_id: OperationId) -> String {
        OperationRef {
            server_identity: "a".repeat(64),
            generation: crate::agent_state::StateGeneration::generate().unwrap(),
            operation_id,
        }
        .encode()
        .unwrap()
    }

    fn api_code(error: &anyhow::Error) -> Option<&'static str> {
        error
            .chain()
            .find_map(|source| source.downcast_ref::<ApiError>())
            .map(ApiError::code)
    }

    #[test]
    fn initial_publish_is_private_and_same_intent_adopts_the_winner() {
        let directory = PrivateDirectory::new();
        let path = directory.state_path();
        let state = RequestState::active(target(), "review this").unwrap();
        let winner = publish_initial_state(&path, state.clone()).unwrap();
        let loser = publish_initial_state(
            &path,
            RequestState::active(target(), "review this").unwrap(),
        )
        .unwrap();

        assert_eq!(winner, state);
        assert_eq!(loser.operation_id, winner.operation_id);
        assert_eq!(loser.prompt_body, winner.prompt_body);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }

    #[test]
    fn state_rejects_mismatch_corruption_and_unsafe_permissions() {
        let directory = PrivateDirectory::new();
        let path = directory.state_path();
        let state = publish_initial_state(
            &path,
            RequestState::active(target(), "review this").unwrap(),
        )
        .unwrap();

        assert_eq!(
            api_code(&validate_intent(&state, "vta1:another_exact_target_1234", None).unwrap_err()),
            Some("request_state_mismatch")
        );
        assert_eq!(
            api_code(
                &validate_intent(&state, target(), Some(&prompt_digest("different"))).unwrap_err()
            ),
            Some("request_state_mismatch")
        );

        std::fs::write(&path, b"{").unwrap();
        assert_eq!(
            api_code(&read_state_if_present(&path).unwrap_err()),
            Some("request_state_invalid")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            api_code(&read_state_if_present(&path).unwrap_err()),
            Some("request_state_invalid")
        );
    }

    #[test]
    fn state_rejects_unknown_format_oversize_and_mismatched_operation_reference() {
        let directory = PrivateDirectory::new();
        let path = directory.state_path();
        let mut state = RequestState::active(target(), "review this").unwrap();
        state.format_version += 1;
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            api_code(&read_state_if_present(&path).unwrap_err()),
            Some("request_state_invalid")
        );

        std::fs::write(&path, vec![b' '; REQUEST_STATE_MAX_BYTES + 1]).unwrap();
        assert_eq!(
            api_code(&read_state_if_present(&path).unwrap_err()),
            Some("request_state_invalid")
        );

        let mut state = RequestState::active(target(), "review this").unwrap();
        state.remember_operation(operation_ref(OperationId::generate().unwrap()));
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(
            api_code(&read_state_if_present(&path).unwrap_err()),
            Some("request_state_invalid")
        );
    }

    #[test]
    fn state_rejects_symlinks_and_non_private_parents() {
        let directory = PrivateDirectory::new();
        let real = directory.0.join("real.json");
        std::fs::write(&real, b"{}").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.state_path();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            api_code(&read_state_if_present(&link).unwrap_err()),
            Some("request_state_invalid")
        );

        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            api_code(&RequestLock::acquire(&directory.state_path()).unwrap_err()),
            Some("request_state_invalid")
        );
    }

    #[test]
    fn sidecar_lock_is_stable_across_state_replacement_and_reports_busy() {
        let directory = PrivateDirectory::new();
        let path = directory.state_path();
        let first = RequestLock::acquire(&path).unwrap();
        let error = RequestLock::acquire(&path).unwrap_err();
        assert_eq!(api_code(&error), Some("request_state_busy"));

        let mut state = publish_initial_state(
            &path,
            RequestState::active(target(), "review this").unwrap(),
        )
        .unwrap();
        state.remember_operation(operation_ref(state.operation_id.clone()));
        replace_state(&path, &state).unwrap();
        assert_eq!(
            api_code(&RequestLock::acquire(&path).unwrap_err()),
            Some("request_state_busy")
        );
        drop(first);
        RequestLock::acquire(&path).unwrap();
    }

    #[test]
    fn encoded_maximum_prompt_fits_the_request_state_limit() {
        let prompt = "\\".repeat(crate::api::MAX_PROMPT_BYTES);
        assert_eq!(prompt.len(), crate::api::MAX_PROMPT_BYTES);
        let state = RequestState::active(target(), &prompt).unwrap();
        let encoded = serde_json::to_vec(&state).unwrap();
        assert!(encoded.len() <= REQUEST_STATE_MAX_BYTES);
        assert!(encoded.len() > crate::api::MAX_PROMPT_BYTES);
    }

    #[test]
    fn only_a_prepared_receipt_keeps_the_request_active() {
        assert!(!receipt_makes_request_observation_only(
            DispatchState::Prepared
        ));
        for dispatch_state in [
            DispatchState::DispatchStarted,
            DispatchState::PromptConfirmed,
            DispatchState::DeliveryUnknown,
            DispatchState::Rejected,
        ] {
            assert!(receipt_makes_request_observation_only(dispatch_state));
        }
    }
}

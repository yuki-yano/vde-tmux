use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;

use super::common::{success_category_mutation_json, success_json};
use super::connection::{ApiConnection, daemon_api_error};
use super::contract::{
    ApiCategoryMutationReceipt, ApiCategoryPlacement, ApiCategoryPlacementState, ApiCategorySource,
    ApiCategorySummary, ApiCategoryTarget, ApiError, ApiErrorCode, ApiErrorStage, ApiRepoSummary,
    ApiResult, ApiRetryAction, ApiSideEffect,
};
use crate::daemon::protocol::v2::{
    ClientMessage, PROTOCOL_VERSION, ServerMessage, V2RequestFailureStage,
};
use crate::pane_state::EventId;
use crate::tmux::TmuxRunner;

pub fn category_list(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
) -> Result<String> {
    let config = load_category_api_config(env)?;
    let mut connection = connect_category_api(runner, env, &config)?;
    let snapshot = connection.query_snapshot()?;
    let model = crate::category::EffectiveCategoryModel::build(
        &config,
        &snapshot.sidebar_model.category_state,
        std::iter::empty(),
    )
    .map_err(|message| api_error!("invalid_daemon_response", message))?;
    let categories = model
        .categories
        .iter()
        .enumerate()
        .map(|(index, category)| ApiCategorySummary {
            index: index + 1,
            name: category.name.to_string(),
            display_name: category.display_name.clone(),
            source: match category.source {
                crate::category::CategorySource::Configured => ApiCategorySource::Configured,
                crate::category::CategorySource::Dynamic => ApiCategorySource::Dynamic,
                crate::category::CategorySource::System => ApiCategorySource::System,
            },
        })
        .collect();
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::CategoryList {
            category_state_revision: model.revision,
            categories,
        },
    )
}

pub fn category_get(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    repo_path: &str,
) -> Result<String> {
    let config = load_category_api_config(env)?;
    let repo = resolve_category_api_repo(repo_path)?;
    let mut connection = connect_category_api(runner, env, &config)?;
    let snapshot = connection.query_snapshot()?;
    let model = crate::category::EffectiveCategoryModel::build(
        &config,
        &snapshot.sidebar_model.category_state,
        [repo.clone()],
    )
    .map_err(|message| api_error!("invalid_daemon_response", message))?;
    let placement = model.placements.get(&repo.key).ok_or_else(|| {
        api_error!(
            "invalid_daemon_response",
            format!("category was not resolved for repository {}", repo.key),
        )
    })?;
    success_json(
        &connection,
        &snapshot,
        observed_at,
        ApiResult::CategoryGet {
            category_state_revision: model.revision,
            placement: api_category_placement(placement),
        },
    )
}

pub fn category_assign(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    category: crate::category::CategoryName,
    repo_path: &str,
) -> Result<String> {
    category_mutation(
        runner,
        env,
        observed_at,
        repo_path,
        ApiCategoryTarget::Category {
            category: category.to_string(),
        },
        |repo| crate::category::CategoryIntent::AssignRepo { repo, category },
    )
}

pub fn category_automatic(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    repo_path: &str,
) -> Result<String> {
    category_mutation(
        runner,
        env,
        observed_at,
        repo_path,
        ApiCategoryTarget::Automatic,
        |repo| crate::category::CategoryIntent::SetRepoAutomatic { repo },
    )
}

fn category_mutation(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    observed_at: i64,
    repo_path: &str,
    requested: ApiCategoryTarget,
    intent: impl FnOnce(crate::category::RepoKey) -> crate::category::CategoryIntent,
) -> Result<String> {
    let config = load_category_api_config(env)?;
    let repo = resolve_category_api_repo(repo_path)?;
    crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, None).map_err(|error| {
        api_error!(
            "daemon_unavailable",
            format!("failed to ensure daemon for category mutation: {error:#}"),
        )
    })?;
    let mut connection = connect_category_api(runner, env, &config)?;
    let event_id =
        EventId::generate().map_err(|error| api_error!("internal_error", format!("{error:#}")))?;
    let daemon_instance_id = connection.client.daemon_instance_id().clone();
    let response = connection
        .client
        .request_with_stage(&ClientMessage::SidebarCommand {
            proto: PROTOCOL_VERSION,
            daemon_instance_id,
            event_id: event_id.clone(),
            command: crate::daemon::protocol::v2::SidebarCommand::CategoryIntent {
                intent: intent(repo.key.clone()),
            },
        })
        .map_err(category_mutation_transport_error)?;
    let (accepted_seq, snapshot_revision, category_state_revision, changed, repo_effect) =
        match response {
            ServerMessage::CategoryMutationResult {
                event_id: response_event_id,
                accepted_seq,
                snapshot_revision,
                category_state_revision,
                changed,
                repo_effect: Some(repo_effect),
            } if response_event_id == event_id && repo_effect.repo == repo.key => (
                accepted_seq,
                snapshot_revision,
                category_state_revision,
                changed,
                repo_effect,
            ),
            ServerMessage::Error {
                event_id: Some(response_event_id),
                ..
            } if response_event_id != event_id => {
                return Err(api_error!(
                    "invalid_daemon_response",
                    "category mutation response event ID did not match the request",
                )
                .into());
            }
            ServerMessage::Error { code, message, .. } => {
                return Err(daemon_api_error(code, message).into());
            }
            other => {
                return Err(api_error!(
                    "invalid_daemon_response",
                    format!("unexpected category mutation response: {other:?}"),
                )
                .into());
            }
        };
    let before =
        resolve_category_placement_state(&config, &repo, repo_effect.before_override.as_ref())?;
    let after =
        resolve_category_placement_state(&config, &repo, repo_effect.after_override.as_ref())?;
    success_category_mutation_json(
        &connection,
        observed_at,
        snapshot_revision,
        ApiResult::CategoryMutation {
            receipt: ApiCategoryMutationReceipt {
                accepted_seq,
                repo: api_repo_summary(&repo),
                requested,
                before,
                after,
                changed,
                category_state_revision,
            },
        },
    )
}

fn load_category_api_config(env: &BTreeMap<String, String>) -> Result<crate::config::Config> {
    crate::config::load::load_config_strict(env).map_err(|error| {
        api_error!(
            "stale_precondition",
            format!("category API requires a valid disk config: {error}; fix it and run `vt daemon reload`"),
        )
        .into()
    })
}

fn connect_category_api(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &crate::config::Config,
) -> Result<ApiConnection> {
    let mut connection = ApiConnection::connect(runner, env, None)?;
    if connection.client.phase() != crate::daemon::protocol::v2::DaemonPhase::Serving {
        return Err(api_error!(
            "daemon_not_ready",
            "category API requires the daemon to be Serving",
        )
        .into());
    }
    let info = connection.query_runtime_info()?;
    verify_category_config_hash(
        &crate::daemon::lifecycle::config_hash(config),
        &info.config_hash,
    )?;
    connection.reconnect()
}

fn verify_category_config_hash(disk_hash: &str, active_hash: &str) -> Result<()> {
    if active_hash.trim().is_empty() || disk_hash != active_hash {
        return Err(api_error!(
            "stale_precondition",
            "disk config does not match the daemon active config; run `vt daemon reload`",
        )
        .into());
    }
    Ok(())
}

fn resolve_category_api_repo(path: &str) -> Result<crate::category::RepoIdentity> {
    let path = path.trim();
    if path.is_empty() {
        return Err(api_error!("invalid_target", "repository path is empty").into());
    }
    let metadata = std::fs::metadata(path).map_err(|error| {
        api_error!(
            "invalid_target",
            format!("repository path is not available: {path}: {error}"),
        )
    })?;
    if !metadata.is_dir() {
        return Err(api_error!(
            "invalid_target",
            format!("repository path is not a directory: {path}"),
        )
        .into());
    }
    let git = crate::daemon::workers::system_git_runner(Duration::from_millis(500));
    crate::category::resolve_repo_identity(&git, path).map_err(|error| {
        api_error!(
            "identity_verification_failed",
            format!("failed to resolve canonical repository identity for {path}: {error:#}"),
        )
        .into()
    })
}

fn api_repo_summary(repo: &crate::category::RepoIdentity) -> ApiRepoSummary {
    ApiRepoSummary {
        key: repo.key.to_string(),
        rule_path: repo.rule_path.clone(),
        display_name: repo.display_name.clone(),
    }
}

fn api_category_placement(
    placement: &crate::category::EffectiveRepoPlacement,
) -> ApiCategoryPlacement {
    ApiCategoryPlacement {
        repo: api_repo_summary(&placement.repo),
        category: placement.category.to_string(),
        explicit: placement.explicit,
    }
}

fn resolve_category_placement_state(
    config: &crate::config::Config,
    repo: &crate::category::RepoIdentity,
    category_override: Option<&crate::category::CategoryName>,
) -> Result<ApiCategoryPlacementState> {
    let mut state = crate::category::CategoryState::default();
    if let Some(category) = category_override {
        state
            .repo_overrides
            .insert(repo.key.clone(), category.clone());
    }
    let model = crate::category::EffectiveCategoryModel::build(config, &state, [repo.clone()])
        .map_err(|message| api_error!("invalid_daemon_response", message))?;
    let placement = model.placements.get(&repo.key).ok_or_else(|| {
        api_error!(
            "invalid_daemon_response",
            format!("category was not resolved for repository {}", repo.key),
        )
    })?;
    Ok(ApiCategoryPlacementState {
        category: placement.category.to_string(),
        explicit: placement.explicit,
    })
}

fn category_mutation_transport_error(
    error: crate::daemon::protocol::v2::V2RequestError,
) -> ApiError {
    match error.stage {
        V2RequestFailureStage::BeforeFullWrite => ApiError::new(
            ApiErrorCode::DaemonQueryFailed,
            format!("category mutation was not dispatched: {}", error.message),
        )
        .with_dispatch_context(
            ApiErrorStage::BeforeDispatch,
            ApiSideEffect::None,
            ApiRetryAction::RetrySameRequest,
            None,
        ),
        V2RequestFailureStage::AfterFullWrite => ApiError::new(
            ApiErrorCode::DeliveryUnknown,
            format!(
                "category mutation was dispatched but its receipt was not received: {}",
                error.message
            ),
        )
        .with_dispatch_context(
            ApiErrorStage::AfterDispatch,
            ApiSideEffect::Possible,
            ApiRetryAction::InspectManually,
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::contract::schema_json;

    #[test]
    fn category_api_projection_keeps_public_shapes_and_resolves_automatic_membership() {
        let repo = crate::category::RepoIdentity {
            key: crate::category::RepoKey::git("/tmp/repo/.git"),
            rule_path: "/tmp/repo".to_string(),
            display_name: "repo".to_string(),
        };
        let mut config = crate::config::Config::default();
        config
            .categories
            .display_names
            .insert("work".to_string(), "Work".to_string());
        config.categories.rules.push(crate::config::CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["/tmp/**".to_string()],
        });
        let automatic = resolve_category_placement_state(&config, &repo, None).unwrap();
        assert_eq!(automatic.category, "work");
        assert!(!automatic.explicit);
        let explicit = resolve_category_placement_state(
            &config,
            &repo,
            Some(&crate::category::CategoryName::uncategorized()),
        )
        .unwrap();
        assert_eq!(explicit.category, crate::category::UNCATEGORIZED);
        assert!(explicit.explicit);

        let result = ApiResult::CategoryMutation {
            receipt: ApiCategoryMutationReceipt {
                accepted_seq: 12,
                repo: api_repo_summary(&repo),
                requested: ApiCategoryTarget::Category {
                    category: "work".to_string(),
                },
                before: automatic,
                after: ApiCategoryPlacementState {
                    category: "work".to_string(),
                    explicit: true,
                },
                changed: true,
                category_state_revision: 8,
            },
        };
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["type"], "category_mutation");
        assert_eq!(value["receipt"]["repo"]["key"], "git:/tmp/repo/.git");
        assert_eq!(value["receipt"]["requested"]["type"], "category");
        assert_eq!(value["receipt"]["requested"]["category"], "work");
        assert_eq!(value["receipt"]["before"]["explicit"], false);
        assert_eq!(value["receipt"]["after"]["explicit"], true);
        assert!(value["receipt"]["repo"].get("repo_overrides").is_none());
    }

    #[test]
    fn category_requests_are_published_in_the_v4_schema() {
        let value: serde_json::Value = serde_json::from_str(&schema_json(123).unwrap()).unwrap();
        let request_schema = serde_json::to_string(&value["result"]["schemas"]["request"]).unwrap();
        for command in [
            "category_list",
            "category_get",
            "category_assign",
            "category_automatic",
        ] {
            assert!(request_schema.contains(command), "missing {command}");
        }
        let success_schema = serde_json::to_string(&value["result"]["schemas"]["success"]).unwrap();
        for result in ["category_list", "category_get", "category_mutation"] {
            assert!(success_schema.contains(result), "missing {result}");
        }
    }

    #[test]
    fn category_config_and_transport_failures_keep_typed_recovery_contracts() {
        let mismatch = verify_category_config_hash("disk", "active").unwrap_err();
        assert_eq!(
            mismatch.downcast_ref::<ApiError>().unwrap().code(),
            "stale_precondition"
        );

        let before =
            category_mutation_transport_error(crate::daemon::protocol::v2::V2RequestError {
                stage: V2RequestFailureStage::BeforeFullWrite,
                message: "not written".to_string(),
            });
        assert_eq!(before.stage, ApiErrorStage::BeforeDispatch);
        assert_eq!(before.side_effect, ApiSideEffect::None);
        assert_eq!(before.retry_action, ApiRetryAction::RetrySameRequest);

        let after =
            category_mutation_transport_error(crate::daemon::protocol::v2::V2RequestError {
                stage: V2RequestFailureStage::AfterFullWrite,
                message: "connection closed".to_string(),
            });
        assert_eq!(after.code, ApiErrorCode::DeliveryUnknown);
        assert_eq!(after.stage, ApiErrorStage::AfterDispatch);
        assert_eq!(after.side_effect, ApiSideEffect::Possible);
        assert_eq!(after.retry_action, ApiRetryAction::InspectManually);
    }
}

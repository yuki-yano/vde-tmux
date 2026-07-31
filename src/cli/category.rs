use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::category::{
    CategoryIntent, CategoryName, EffectiveCategoryModel, MembershipTarget, UNCATEGORIZED,
};
use crate::config::Config;
use crate::tmux::TmuxRunner;

pub fn parse_category(value: &str) -> Result<CategoryName> {
    if value.trim() == UNCATEGORIZED {
        Ok(CategoryName::uncategorized())
    } else {
        CategoryName::parse(value).map_err(anyhow::Error::msg)
    }
}

pub fn parse_dynamic_category(value: &str) -> Result<CategoryName> {
    CategoryName::parse(value).map_err(anyhow::Error::msg)
}

pub fn send_intent(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    intent: CategoryIntent,
) -> Result<()> {
    let (incarnation, socket) =
        crate::daemon::lifecycle::ensure_daemon_serving_v2(runner, env, None)
            .context("failed to ensure daemon for category mutation")?;
    crate::sidebar::client::send_category_intent_v2(&socket, &incarnation.hash, intent)
}

pub fn repo_identity(path: &str) -> Result<crate::category::RepoIdentity> {
    let git = crate::daemon::workers::system_git_runner(Duration::from_millis(500));
    crate::category::resolve_repo_identity(&git, path)
}

pub fn list(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &Config,
) -> Result<String> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let path = crate::category::store::state_path(env, &incarnation.socket_path);
    let state = crate::category::store::load_state(&path)?;
    let model = EffectiveCategoryModel::build(config, &state, std::iter::empty())
        .map_err(anyhow::Error::msg)?;
    Ok(model
        .categories
        .iter()
        .enumerate()
        .map(|(index, category)| {
            format!(
                "{}\t{}\t{}",
                index + 1,
                category.name,
                match category.source {
                    crate::category::CategorySource::Configured => "configured",
                    crate::category::CategorySource::Dynamic => "dynamic",
                    crate::category::CategorySource::System => "system",
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub fn delete_target(move_to: Option<&str>, automatic: bool) -> Result<MembershipTarget> {
    match (move_to, automatic) {
        (Some(category), false) => Ok(MembershipTarget::Category(parse_category(category)?)),
        (None, true) => Ok(MembershipTarget::Automatic),
        (Some(_), true) => bail!("--move-to and --automatic are mutually exclusive"),
        (None, false) => bail!("category delete requires --move-to or --automatic"),
    }
}

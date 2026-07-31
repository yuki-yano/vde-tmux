use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::git::GitRunner;
use crate::session::SessionInfo;
use crate::tmux::TmuxRunner;

use super::{
    CategoryName, CategoryState, EffectiveCategoryModel, RepoIdentity, RepoKey,
    resolve_repo_identity,
};

#[derive(Debug, Clone)]
pub struct ResolvedSessionCategories {
    pub state: CategoryState,
    pub model: EffectiveCategoryModel,
    by_session_id: BTreeMap<String, CategoryName>,
}

impl ResolvedSessionCategories {
    pub fn category_for(&self, session: &SessionInfo) -> Result<&CategoryName> {
        self.by_session_id
            .get(&session_identity(session))
            .with_context(|| format!("category was not resolved for session {}", session.name))
    }

    pub fn sessions_in_category<'a>(
        &self,
        sessions: &'a [SessionInfo],
        category: &str,
    ) -> Vec<&'a SessionInfo> {
        sessions
            .iter()
            .filter(|session| {
                self.category_for(session)
                    .is_ok_and(|resolved| resolved.as_str() == category)
            })
            .collect()
    }

    pub fn categories_with_sessions(&self, sessions: &[SessionInfo]) -> Vec<CategoryName> {
        let occupied = sessions
            .iter()
            .filter_map(|session| self.category_for(session).ok().cloned())
            .collect::<BTreeSet<_>>();
        self.model
            .categories
            .iter()
            .filter(|category| occupied.contains(&category.name))
            .map(|category| category.name.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn from_assignments(assignments: &[(&SessionInfo, &str)]) -> Self {
        let mut state = CategoryState::default();
        let mut by_session_id = BTreeMap::new();
        for (session, category) in assignments {
            let category = if *category == super::UNCATEGORIZED {
                CategoryName::uncategorized()
            } else {
                CategoryName::parse(*category).unwrap()
            };
            if category.as_str() != super::UNCATEGORIZED {
                state.dynamic_categories.insert(category.clone());
            }
            by_session_id.insert(session_identity(session), category);
        }
        let model =
            EffectiveCategoryModel::build(&Config::default(), &state, std::iter::empty()).unwrap();
        Self {
            state,
            model,
            by_session_id,
        }
    }
}

pub fn load_state_for_server(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<CategoryState> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve(runner, env)?;
    let path = super::store::state_path(env, &incarnation.socket_path);
    super::store::load_state(&path)
        .with_context(|| format!("failed to load category state {}", path.display()))
}

pub fn load_state_for_runner(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
) -> Result<CategoryState> {
    let incarnation = crate::daemon::lifecycle::TmuxServerIncarnation::resolve_from_runner(runner)?;
    let path = super::store::state_path(env, &incarnation.socket_path);
    super::store::load_state(&path)
        .with_context(|| format!("failed to load category state {}", path.display()))
}

pub fn resolve_project_category(
    config: &Config,
    state: &CategoryState,
    git: &dyn GitRunner,
    project_path: &str,
) -> Result<(RepoIdentity, CategoryName)> {
    let repo = resolve_repo_identity(git, project_path)?;
    let model =
        EffectiveCategoryModel::build(config, state, [repo.clone()]).map_err(anyhow::Error::msg)?;
    let placement = model
        .placements
        .get(&repo.key)
        .with_context(|| format!("category was not resolved for repository {}", repo.key))?;
    Ok((repo, placement.category.clone()))
}

pub fn resolve_project_category_from_server(
    runner: &dyn TmuxRunner,
    config: &Config,
    env: &BTreeMap<String, String>,
    project_path: &str,
) -> Result<(RepoIdentity, CategoryName)> {
    let state = load_state_for_server(runner, env)?;
    let git = crate::daemon::workers::system_git_runner(Duration::from_millis(500));
    resolve_project_category(config, &state, &git, project_path)
}

pub fn resolve_session_categories(
    config: &Config,
    state: CategoryState,
    git: &dyn GitRunner,
    sessions: &[SessionInfo],
) -> Result<ResolvedSessionCategories> {
    let mut repos = BTreeMap::<RepoKey, RepoIdentity>::new();
    let mut session_repos = BTreeMap::new();
    let mut by_session_id = BTreeMap::new();
    for session in sessions {
        if session.project_path.trim().is_empty() {
            by_session_id.insert(session_identity(session), CategoryName::uncategorized());
            continue;
        }
        let repo = resolve_repo_identity(git, &session.project_path).with_context(|| {
            format!("failed to resolve repository for session {}", session.name)
        })?;
        session_repos.insert(session_identity(session), repo.key.clone());
        repos.entry(repo.key.clone()).or_insert(repo);
    }
    let model = EffectiveCategoryModel::build(config, &state, repos.into_values())
        .map_err(anyhow::Error::msg)?;
    for (session, repo) in session_repos {
        let category = model
            .placements
            .get(&repo)
            .with_context(|| format!("category was not resolved for repository {repo}"))?
            .category
            .clone();
        by_session_id.insert(session, category);
    }
    Ok(ResolvedSessionCategories {
        state,
        model,
        by_session_id,
    })
}

pub fn resolve_session_categories_from_server(
    runner: &dyn TmuxRunner,
    config: &Config,
    env: &BTreeMap<String, String>,
    sessions: &[SessionInfo],
) -> Result<ResolvedSessionCategories> {
    let state = load_state_for_server(runner, env)?;
    let git = crate::daemon::workers::system_git_runner(Duration::from_millis(500));
    resolve_session_categories(config, state, &git, sessions)
}

pub fn resolve_session_categories_from_runner(
    runner: &dyn TmuxRunner,
    config: &Config,
    env: &BTreeMap<String, String>,
    sessions: &[SessionInfo],
) -> Result<ResolvedSessionCategories> {
    let state = load_state_for_runner(runner, env)?;
    let git = crate::daemon::workers::system_git_runner(Duration::from_millis(500));
    resolve_session_categories(config, state, &git, sessions)
}

fn session_identity(session: &SessionInfo) -> String {
    if session.id.is_empty() {
        format!("name:{}", session.name)
    } else {
        format!("id:{}", session.id)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, bail};

    use super::*;

    struct NonGitRunner;

    impl GitRunner for NonGitRunner {
        fn run(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
            bail!("not a git repository")
        }

        fn run_vw(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
            bail!("unexpected vw call")
        }
    }

    #[test]
    fn explicit_repository_membership_is_restored_without_an_active_session() {
        let root = std::env::temp_dir().join(format!(
            "vde-category-resolver-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical = std::fs::canonicalize(&repo).unwrap();
        let category = CategoryName::parse("restored").unwrap();
        let mut state = CategoryState::default();
        state.dynamic_categories.insert(category.clone());
        state
            .repo_overrides
            .insert(RepoKey::path(canonical.display()), category.clone());

        let (_, resolved) = resolve_project_category(
            &Config::default(),
            &state,
            &NonGitRunner,
            repo.to_str().unwrap(),
        )
        .unwrap();

        assert_eq!(resolved, category);
        std::fs::remove_dir_all(root).unwrap();
    }
}

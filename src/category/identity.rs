use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::git::GitRunner;

use super::model::RepoKey;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoIdentity {
    pub key: RepoKey,
    pub rule_path: String,
    pub display_name: String,
}

pub fn resolve_repo_identity(runner: &dyn GitRunner, project_path: &str) -> Result<RepoIdentity> {
    let path = project_path.trim();
    if path.is_empty() {
        bail!("repository project path is empty");
    }
    if let Some(identity) = crate::git::probe_worktree_identity_result(runner, path)? {
        return RepoIdentity::from_worktree(&identity);
    }
    RepoIdentity::from_project_path(path)
}

impl RepoIdentity {
    pub fn from_worktree(identity: &crate::git::WorktreeIdentity) -> Result<Self> {
        let common_dir = canonicalize_existing(Path::new(&identity.git_common_dir))?;
        let rule_path = repository_root_from_common_dir(&common_dir, &identity.top_level)?;
        Ok(Self {
            key: RepoKey::git(path_string(&common_dir)?),
            display_name: path_label(&rule_path),
            rule_path: path_string(&rule_path)?,
        })
    }

    pub fn from_project_path(project_path: &str) -> Result<Self> {
        let canonical = canonicalize_existing(Path::new(project_path))?;
        Ok(Self {
            key: RepoKey::path(path_string(&canonical)?),
            display_name: path_label(&canonical),
            rule_path: path_string(&canonical)?,
        })
    }
}

fn repository_root_from_common_dir(common_dir: &Path, top_level: &str) -> Result<PathBuf> {
    if common_dir.file_name().and_then(|value| value.to_str()) == Some(".git") {
        return common_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("git common dir has no repository root"));
    }
    canonicalize_existing(Path::new(top_level))
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize repository path {}", path.display()))
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("repository path is not valid UTF-8: {}", path.display()))
}

fn path_label(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("repo")
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MockGitRunner {
        responses: BTreeMap<Vec<String>, anyhow::Result<String, String>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl MockGitRunner {
        fn stub(&mut self, cwd: &Path, output: &str) {
            let mut key = vec![cwd.to_string_lossy().into_owned()];
            key.extend(
                [
                    "rev-parse",
                    "--path-format=absolute",
                    "--show-toplevel",
                    "--git-dir",
                    "--git-common-dir",
                    "--show-superproject-working-tree",
                ]
                .into_iter()
                .map(str::to_string),
            );
            self.responses.insert(key, Ok(output.to_string()));
        }
    }

    impl GitRunner for MockGitRunner {
        fn run(&self, cwd: &str, args: &[&str]) -> Result<String> {
            let mut key = vec![cwd.to_string()];
            key.extend(args.iter().map(|value| value.to_string()));
            self.calls.lock().unwrap().push(key.clone());
            self.responses
                .get(&key)
                .map(|result| result.clone().map_err(anyhow::Error::msg))
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!("not a git repository"))
        }

        fn run_vw(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
            bail!("unexpected vw call")
        }
    }

    #[test]
    fn linked_worktree_and_main_repo_share_common_dir_key() {
        let root = unique_temp_dir("linked");
        let repo = root.join("repo");
        let linked = root.join("linked");
        let common = repo.join(".git");
        let linked_git = common.join("worktrees/linked");
        std::fs::create_dir_all(&linked_git).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        let mut runner = MockGitRunner::default();
        runner.stub(
            &repo,
            &format!(
                "{}\n{}\n{}\n",
                repo.display(),
                common.display(),
                common.display()
            ),
        );
        runner.stub(
            &linked,
            &format!(
                "{}\n{}\n{}\n",
                linked.display(),
                linked_git.display(),
                common.display()
            ),
        );

        let main = resolve_repo_identity(&runner, repo.to_str().unwrap()).unwrap();
        let worktree = resolve_repo_identity(&runner, linked.to_str().unwrap()).unwrap();
        let canonical_repo = std::fs::canonicalize(&repo).unwrap();

        assert_eq!(main.key, worktree.key);
        assert_eq!(main.rule_path, canonical_repo.to_string_lossy());
        assert_eq!(worktree.rule_path, canonical_repo.to_string_lossy());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn non_git_project_uses_canonical_path_key() {
        let root = unique_temp_dir("non-git");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();

        let identity =
            resolve_repo_identity(&MockGitRunner::default(), project.to_str().unwrap()).unwrap();

        assert_eq!(
            identity.key,
            RepoKey::path(std::fs::canonicalize(&project).unwrap().to_string_lossy())
        );
        assert_eq!(identity.display_name, "project");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_probe_failure_does_not_fall_back_to_a_non_git_identity() {
        struct TimedOutGitRunner;

        impl GitRunner for TimedOutGitRunner {
            fn run(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
                bail!("git probe timed out")
            }

            fn run_vw(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
                bail!("unexpected vw call")
            }
        }

        let root = unique_temp_dir("timeout");
        std::fs::create_dir_all(&root).unwrap();
        let error = resolve_repo_identity(&TimedOutGitRunner, root.to_str().unwrap())
            .unwrap_err()
            .to_string();

        assert!(error.contains("timed out"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "vde-tmux-category-identity-{label}-{}-{stamp}",
            std::process::id()
        ))
    }
}

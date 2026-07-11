use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBadge {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeSource {
    GitLinked,
    VwManaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub name: String,
    pub path: String,
    pub source: WorktreeSource,
    pub branch: Option<String>,
    pub dirty: Option<bool>,
    pub locked: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VwListOutput {
    #[serde(default)]
    pub repo_root: Option<String>,
    #[serde(default)]
    pub managed_worktree_root: Option<String>,
    #[serde(default)]
    pub worktrees: Vec<VwWorktreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VwWorktreeEntry {
    #[serde(default)]
    pub branch: Option<String>,
    pub path: String,
    #[serde(default)]
    pub dirty: Option<bool>,
    #[serde(default)]
    pub locked: Option<VwLockedState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum VwLockedState {
    Bool(bool),
    Detail { value: bool },
}

impl VwLockedState {
    fn value(&self) -> bool {
        match self {
            Self::Bool(value) | Self::Detail { value } => *value,
        }
    }
}

pub trait GitRunner: Send + Sync {
    fn run(&self, cwd: &str, args: &[&str]) -> Result<String>;
    fn run_vw(&self, cwd: &str, args: &[&str]) -> Result<String>;
}

#[derive(Debug, Clone)]
pub struct SystemGitRunner {
    timeout: Duration,
}

impl Default for SystemGitRunner {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(500),
        }
    }
}

impl SystemGitRunner {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl GitRunner for SystemGitRunner {
    fn run(&self, cwd: &str, args: &[&str]) -> Result<String> {
        run_git_command(cwd, args, self.timeout)
    }

    fn run_vw(&self, cwd: &str, args: &[&str]) -> Result<String> {
        run_process_command("vw", cwd, args, self.timeout)
    }
}

pub fn query_git_badge(runner: &dyn GitRunner, cwd: &str) -> Result<Option<GitBadge>> {
    let branch = runner.run(cwd, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Ok(None);
    }
    let (ahead, behind) = match runner.run(
        cwd,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    ) {
        Ok(counts) => parse_ahead_behind(&counts)?,
        Err(_) => (0, 0),
    };
    Ok(Some(GitBadge {
        branch: branch.to_string(),
        ahead,
        behind,
    }))
}

pub fn parse_ahead_behind(raw: &str) -> Result<(u32, u32)> {
    let fields = raw.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2 {
        bail!("invalid ahead/behind output: {raw:?}");
    }
    Ok((fields[1].parse()?, fields[0].parse()?))
}

pub fn collect_git_badges_for_paths<'a>(
    runner: &dyn GitRunner,
    paths: impl IntoIterator<Item = &'a str>,
) -> BTreeMap<String, GitBadge> {
    let mut badges = BTreeMap::new();
    for path in paths
        .into_iter()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        if badges.contains_key(path) {
            continue;
        }
        if let Ok(Some(badge)) = query_git_badge(runner, path) {
            badges.insert(path.to_string(), badge);
        }
    }
    badges
}

pub fn query_worktree_info(runner: &dyn GitRunner, cwd: &str) -> Result<Option<WorktreeInfo>> {
    let top_level = runner.run(cwd, &["rev-parse", "--show-toplevel"])?;
    let top_level = top_level.trim();
    if top_level.is_empty() {
        return Ok(None);
    }
    let git_dir = runner.run(cwd, &["rev-parse", "--git-dir"])?;
    let git_common_dir = runner.run(cwd, &["rev-parse", "--git-common-dir"])?;
    let superproject = runner
        .run(cwd, &["rev-parse", "--show-superproject-working-tree"])
        .unwrap_or_default();
    if !superproject.trim().is_empty() {
        return Ok(None);
    }
    if git_dir.trim() == git_common_dir.trim() {
        return Ok(None);
    }

    let mut info = WorktreeInfo {
        name: path_basename(top_level).unwrap_or_else(|| top_level.to_string()),
        path: top_level.to_string(),
        source: WorktreeSource::GitLinked,
        branch: None,
        dirty: None,
        locked: None,
    };
    if let Some(vw_list) = query_vw_worktrees(runner, cwd)? {
        info = enrich_with_vw_metadata(info, &vw_list);
    }
    Ok(Some(info))
}

pub fn query_vw_worktrees(runner: &dyn GitRunner, cwd: &str) -> Result<Option<VwListOutput>> {
    let output = match runner.run_vw(cwd, &["list", "--json"]) {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    Ok(serde_json::from_str(&output).ok())
}

pub fn enrich_with_vw_metadata(mut info: WorktreeInfo, vw_list: &VwListOutput) -> WorktreeInfo {
    let info_path = normalize_path_for_compare(&info.path);
    let Some(entry) = vw_list
        .worktrees
        .iter()
        .find(|entry| normalize_path_for_compare(&entry.path) == info_path)
    else {
        return info;
    };

    info.source = WorktreeSource::VwManaged;
    info.branch = entry.branch.clone();
    info.dirty = entry.dirty;
    info.locked = entry.locked.as_ref().map(VwLockedState::value);
    info.name = vw_list
        .managed_worktree_root
        .as_deref()
        .and_then(|root| relative_suffix(root, &entry.path))
        .or_else(|| path_basename(&entry.path))
        .or_else(|| entry.branch.clone())
        .unwrap_or_else(|| info.name.clone());
    info
}

pub fn collect_worktree_infos_for_paths<'a>(
    runner: &dyn GitRunner,
    paths: impl IntoIterator<Item = &'a str>,
) -> BTreeMap<String, WorktreeInfo> {
    let mut infos = BTreeMap::new();
    for path in paths
        .into_iter()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        if infos.contains_key(path) {
            continue;
        }
        if let Ok(Some(info)) = query_worktree_info(runner, path) {
            infos.insert(path.to_string(), info);
        }
    }
    infos
}

fn normalize_path_for_compare(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    std::fs::canonicalize(trimmed)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| trimmed.to_string())
}

fn path_basename(raw: &str) -> Option<String> {
    Path::new(raw.trim_end_matches('/'))
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn relative_suffix(root: &str, path: &str) -> Option<String> {
    let root = Path::new(root.trim_end_matches('/'));
    let path = Path::new(path.trim_end_matches('/'));
    let suffix = path.strip_prefix(root).ok()?;
    let label = suffix.to_string_lossy().replace('\\', "/");
    (!label.is_empty()).then_some(label)
}

fn run_git_command(cwd: &str, args: &[&str], timeout: Duration) -> Result<String> {
    run_process_command("git", cwd, args, timeout)
}

fn run_process_command(
    binary: &str,
    cwd: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String> {
    let mut child = Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {binary} in {cwd}"))?;

    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            let output = child
                .wait_with_output()
                .with_context(|| format!("failed to collect git output in {cwd}"))?;
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            bail!(
                "{binary} {args:?} failed in {cwd}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{binary} {args:?} timed out after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockGitRunner {
        responses: std::collections::BTreeMap<Vec<String>, anyhow::Result<String, String>>,
        vw_responses: std::collections::BTreeMap<Vec<String>, anyhow::Result<String, String>>,
    }

    impl MockGitRunner {
        fn stub(&mut self, args: &[&str], output: &str) {
            self.responses.insert(
                args.iter().map(|value| value.to_string()).collect(),
                Ok(output.to_string()),
            );
        }

        fn stub_error(&mut self, args: &[&str], message: &str) {
            self.responses.insert(
                args.iter().map(|value| value.to_string()).collect(),
                Err(message.to_string()),
            );
        }

        fn stub_vw(&mut self, args: &[&str], output: &str) {
            self.vw_responses.insert(
                args.iter().map(|value| value.to_string()).collect(),
                Ok(output.to_string()),
            );
        }

        fn stub_vw_error(&mut self, args: &[&str], message: &str) {
            self.vw_responses.insert(
                args.iter().map(|value| value.to_string()).collect(),
                Err(message.to_string()),
            );
        }
    }

    impl GitRunner for MockGitRunner {
        fn run(&self, cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            let mut key = vec![cwd.to_string()];
            key.extend(args.iter().map(|value| value.to_string()));
            self.responses
                .get(&key)
                .map(|response| response.clone().map_err(|message| anyhow::anyhow!(message)))
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!("missing git stub: {key:?}"))
        }

        fn run_vw(&self, cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            let mut key = vec![cwd.to_string()];
            key.extend(args.iter().map(|value| value.to_string()));
            self.vw_responses
                .get(&key)
                .map(|response| response.clone().map_err(|message| anyhow::anyhow!(message)))
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!("missing vw stub: {key:?}"))
        }
    }

    fn stub_worktree_probe(
        runner: &mut MockGitRunner,
        cwd: &str,
        top_level: &str,
        git_dir: &str,
        common_dir: &str,
        superproject: &str,
    ) {
        runner.stub(
            &[cwd, "rev-parse", "--show-toplevel"],
            &format!("{top_level}\n"),
        );
        runner.stub(&[cwd, "rev-parse", "--git-dir"], &format!("{git_dir}\n"));
        runner.stub(
            &[cwd, "rev-parse", "--git-common-dir"],
            &format!("{common_dir}\n"),
        );
        runner.stub(
            &[cwd, "rev-parse", "--show-superproject-working-tree"],
            &format!("{superproject}\n"),
        );
    }

    #[test]
    fn parse_ahead_behind_counts() {
        assert_eq!(parse_ahead_behind("3\t2\n").unwrap(), (2, 3));
    }

    #[test]
    fn query_git_badge_reads_branch_and_upstream_counts() {
        let mut runner = MockGitRunner::default();
        runner.stub(&["/tmp/repo", "branch", "--show-current"], "main\n");
        runner.stub(
            &[
                "/tmp/repo",
                "rev-list",
                "--left-right",
                "--count",
                "@{upstream}...HEAD",
            ],
            "3\t2\n",
        );

        let badge = query_git_badge(&runner, "/tmp/repo").unwrap().unwrap();

        assert_eq!(
            badge,
            GitBadge {
                branch: "main".to_string(),
                ahead: 2,
                behind: 3
            }
        );
    }

    #[test]
    fn query_git_badge_keeps_branch_when_upstream_is_absent() {
        let mut runner = MockGitRunner::default();
        runner.stub(&["/tmp/repo", "branch", "--show-current"], "main\n");
        runner.stub_error(
            &[
                "/tmp/repo",
                "rev-list",
                "--left-right",
                "--count",
                "@{upstream}...HEAD",
            ],
            "fatal: no upstream configured",
        );

        let badge = query_git_badge(&runner, "/tmp/repo").unwrap().unwrap();

        assert_eq!(
            badge,
            GitBadge {
                branch: "main".to_string(),
                ahead: 0,
                behind: 0
            }
        );
    }

    #[test]
    fn collect_git_badges_for_paths_uses_unique_paths() {
        let mut runner = MockGitRunner::default();
        runner.stub(&["/tmp/repo", "branch", "--show-current"], "main\n");
        runner.stub(
            &[
                "/tmp/repo",
                "rev-list",
                "--left-right",
                "--count",
                "@{upstream}...HEAD",
            ],
            "0\t1\n",
        );
        let badges =
            collect_git_badges_for_paths(&runner, ["/tmp/repo", "/tmp/repo", "/tmp/not-repo"]);

        assert_eq!(badges.len(), 1);
        assert_eq!(badges["/tmp/repo"].branch, "main");
    }

    #[test]
    fn query_worktree_info_detects_linked_worktree() {
        let mut runner = MockGitRunner::default();
        stub_worktree_probe(
            &mut runner,
            "/tmp/worktrees/feature",
            "/tmp/worktrees/feature",
            "/tmp/repo/.git/worktrees/feature",
            "/tmp/repo/.git",
            "",
        );
        runner.stub_vw_error(&["/tmp/worktrees/feature", "list", "--json"], "vw missing");

        let info = query_worktree_info(&runner, "/tmp/worktrees/feature")
            .unwrap()
            .unwrap();

        assert_eq!(info.source, WorktreeSource::GitLinked);
        assert_eq!(info.name, "feature");
        assert_eq!(info.path, "/tmp/worktrees/feature");
        assert_eq!(info.branch, None);
        assert_eq!(info.dirty, None);
        assert_eq!(info.locked, None);
    }

    #[test]
    fn query_worktree_info_ignores_main_worktree() {
        let mut runner = MockGitRunner::default();
        stub_worktree_probe(
            &mut runner,
            "/tmp/repo",
            "/tmp/repo",
            "/tmp/repo/.git",
            "/tmp/repo/.git",
            "",
        );

        assert_eq!(query_worktree_info(&runner, "/tmp/repo").unwrap(), None);
    }

    #[test]
    fn query_worktree_info_ignores_submodule() {
        let mut runner = MockGitRunner::default();
        stub_worktree_probe(
            &mut runner,
            "/tmp/repo/submodule",
            "/tmp/repo/submodule",
            "/tmp/repo/.git/modules/submodule",
            "/tmp/repo/.git",
            "/tmp/repo",
        );

        assert_eq!(
            query_worktree_info(&runner, "/tmp/repo/submodule").unwrap(),
            None
        );
    }

    #[test]
    fn query_worktree_info_enriches_vw_managed_worktree() {
        let mut runner = MockGitRunner::default();
        stub_worktree_probe(
            &mut runner,
            "/tmp/worktrees/feature",
            "/tmp/worktrees/feature",
            "/tmp/repo/.git/worktrees/feature",
            "/tmp/repo/.git",
            "",
        );
        runner.stub_vw(
            &["/tmp/worktrees/feature", "list", "--json"],
            r#"{
              "repoRoot": "/tmp/repo",
              "managedWorktreeRoot": "/tmp/worktrees",
              "worktrees": [
                {
                  "branch": "feature",
                  "path": "/tmp/worktrees/feature",
                  "dirty": true,
                  "locked": false
                }
              ]
            }"#,
        );

        let info = query_worktree_info(&runner, "/tmp/worktrees/feature")
            .unwrap()
            .unwrap();

        assert_eq!(info.source, WorktreeSource::VwManaged);
        assert_eq!(info.name, "feature");
        assert_eq!(info.branch.as_deref(), Some("feature"));
        assert_eq!(info.dirty, Some(true));
        assert_eq!(info.locked, Some(false));
    }

    #[test]
    fn query_worktree_info_accepts_vw_locked_object() {
        let mut runner = MockGitRunner::default();
        stub_worktree_probe(
            &mut runner,
            "/tmp/worktrees/feature",
            "/tmp/worktrees/feature",
            "/tmp/repo/.git/worktrees/feature",
            "/tmp/repo/.git",
            "",
        );
        runner.stub_vw(
            &["/tmp/worktrees/feature", "list", "--json"],
            r#"{
              "repoRoot": "/tmp/repo",
              "managedWorktreeRoot": "/tmp/worktrees",
              "worktrees": [
                {
                  "branch": "feature",
                  "path": "/tmp/worktrees/feature",
                  "dirty": false,
                  "locked": { "value": true, "reason": "review", "owner": "me" }
                }
              ]
            }"#,
        );

        let info = query_worktree_info(&runner, "/tmp/worktrees/feature")
            .unwrap()
            .unwrap();

        assert_eq!(info.source, WorktreeSource::VwManaged);
        assert_eq!(info.locked, Some(true));
    }

    #[test]
    fn query_worktree_info_uses_git_linked_when_vw_json_is_malformed() {
        let mut runner = MockGitRunner::default();
        stub_worktree_probe(
            &mut runner,
            "/tmp/worktrees/feature",
            "/tmp/worktrees/feature",
            "/tmp/repo/.git/worktrees/feature",
            "/tmp/repo/.git",
            "",
        );
        runner.stub_vw(&["/tmp/worktrees/feature", "list", "--json"], "{not-json");

        let info = query_worktree_info(&runner, "/tmp/worktrees/feature")
            .unwrap()
            .unwrap();

        assert_eq!(info.source, WorktreeSource::GitLinked);
        assert_eq!(info.name, "feature");
    }

    #[test]
    fn query_worktree_info_uses_git_linked_when_vw_has_no_match() {
        let mut runner = MockGitRunner::default();
        stub_worktree_probe(
            &mut runner,
            "/tmp/worktrees/feature",
            "/tmp/worktrees/feature",
            "/tmp/repo/.git/worktrees/feature",
            "/tmp/repo/.git",
            "",
        );
        runner.stub_vw(
            &["/tmp/worktrees/feature", "list", "--json"],
            r#"{
              "repoRoot": "/tmp/repo",
              "managedWorktreeRoot": "/tmp/worktrees",
              "worktrees": [
                {
                  "branch": "other",
                  "path": "/tmp/worktrees/other",
                  "dirty": false,
                  "locked": false
                }
              ]
            }"#,
        );

        let info = query_worktree_info(&runner, "/tmp/worktrees/feature")
            .unwrap()
            .unwrap();

        assert_eq!(info.source, WorktreeSource::GitLinked);
        assert_eq!(info.name, "feature");
        assert_eq!(info.branch, None);
    }

    #[test]
    fn collect_worktree_infos_for_paths_uses_unique_paths() {
        let mut runner = MockGitRunner::default();
        stub_worktree_probe(
            &mut runner,
            "/tmp/worktrees/feature",
            "/tmp/worktrees/feature",
            "/tmp/repo/.git/worktrees/feature",
            "/tmp/repo/.git",
            "",
        );
        runner.stub_vw_error(&["/tmp/worktrees/feature", "list", "--json"], "vw missing");
        let infos = collect_worktree_infos_for_paths(
            &runner,
            [
                "/tmp/worktrees/feature",
                "/tmp/worktrees/feature",
                "/tmp/not-agent",
            ],
        );

        assert_eq!(infos.len(), 1);
        assert_eq!(infos["/tmp/worktrees/feature"].name, "feature");
    }
}

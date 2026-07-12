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

    pub fn timeout(&self) -> Duration {
        self.timeout
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

/// Parsed `# branch.*` headers of `git status --porcelain=v2 --branch`.
/// `branch` is `None` on a detached HEAD; without an upstream the ahead/behind
/// counters stay `0/0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PorcelainBranchStatus {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

pub fn parse_porcelain_branch_status(raw: &str) -> Result<PorcelainBranchStatus> {
    let mut head: Option<String> = None;
    let mut ab: Option<(u32, u32)> = None;
    for line in raw.lines() {
        let Some(header) = line.strip_prefix("# ") else {
            continue;
        };
        if let Some(value) = header.strip_prefix("branch.head ") {
            let value = value.trim();
            if value.is_empty() {
                bail!("porcelain v2 branch.head header is empty");
            }
            head = Some(value.to_string());
        } else if let Some(value) = header.strip_prefix("branch.ab ") {
            ab = Some(parse_branch_ab(value)?);
        }
    }
    let head = head.ok_or_else(|| anyhow::anyhow!("porcelain v2 output lacks branch.head"))?;
    let branch = (head != "(detached)").then_some(head);
    let (ahead, behind) = ab.unwrap_or((0, 0));
    Ok(PorcelainBranchStatus {
        branch,
        ahead,
        behind,
    })
}

fn parse_branch_ab(value: &str) -> Result<(u32, u32)> {
    let fields = value.split_whitespace().collect::<Vec<_>>();
    let [ahead, behind] = fields.as_slice() else {
        bail!("invalid porcelain v2 branch.ab header: {value:?}");
    };
    let ahead = ahead
        .strip_prefix('+')
        .ok_or_else(|| anyhow::anyhow!("invalid ahead field: {value:?}"))?
        .parse()?;
    let behind = behind
        .strip_prefix('-')
        .ok_or_else(|| anyhow::anyhow!("invalid behind field: {value:?}"))?
        .parse()?;
    Ok((ahead, behind))
}

/// Identity of the worktree that contains a pane path, resolved by a single
/// `git rev-parse` probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeIdentity {
    pub top_level: String,
    pub git_dir: String,
    pub git_common_dir: String,
    pub superproject: Option<String>,
}

impl WorktreeIdentity {
    fn is_linked_worktree(&self) -> bool {
        self.superproject.is_none() && self.git_dir != self.git_common_dir
    }
}

fn probe_worktree_identity(runner: &dyn GitRunner, path: &str) -> Option<WorktreeIdentity> {
    let output = runner
        .run(
            path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
                "--show-superproject-working-tree",
            ],
        )
        .ok()?;
    let lines = output.lines().map(str::trim).collect::<Vec<_>>();
    // `--show-superproject-working-tree` prints nothing outside a submodule,
    // so a plain worktree yields exactly three lines.
    match lines.as_slice() {
        [top_level, git_dir, git_common_dir] | [top_level, git_dir, git_common_dir, ""]
            if !top_level.is_empty() =>
        {
            Some(WorktreeIdentity {
                top_level: (*top_level).to_string(),
                git_dir: (*git_dir).to_string(),
                git_common_dir: (*git_common_dir).to_string(),
                superproject: None,
            })
        }
        [top_level, git_dir, git_common_dir, superproject]
            if !top_level.is_empty() && !superproject.is_empty() =>
        {
            Some(WorktreeIdentity {
                top_level: (*top_level).to_string(),
                git_dir: (*git_dir).to_string(),
                git_common_dir: (*git_common_dir).to_string(),
                superproject: Some((*superproject).to_string()),
            })
        }
        _ => None,
    }
}

pub const GIT_PROBE_CACHE_CAPACITY: usize = 256;
pub const GIT_PROBE_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct ProbeCacheEntry {
    identity: Option<WorktreeIdentity>,
    cached_at: Instant,
    last_used: u64,
}

/// Stateful steady-state poller owned by the daemon git worker. Pane paths are
/// resolved to worktree identities through a bounded TTL cache, deduplicated by
/// worktree top-level, and each worktree is refreshed with a single
/// `git status --porcelain=v2 --branch --untracked-files=no` invocation.
#[derive(Debug, Default)]
pub struct GitPoller {
    cache: BTreeMap<String, ProbeCacheEntry>,
    use_counter: u64,
}

impl GitPoller {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn poll<'a>(
        &mut self,
        runner: &dyn GitRunner,
        paths: impl IntoIterator<Item = &'a str>,
        now: Instant,
    ) -> (BTreeMap<String, GitBadge>, BTreeMap<String, WorktreeInfo>) {
        let mut identities: BTreeMap<String, WorktreeIdentity> = BTreeMap::new();
        for path in paths
            .into_iter()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            if identities.contains_key(path) {
                continue;
            }
            if let Some(identity) = self.resolve_identity(runner, path, now) {
                identities.insert(path.to_string(), identity);
            }
        }

        let mut group_identity: BTreeMap<&str, &WorktreeIdentity> = BTreeMap::new();
        for identity in identities.values() {
            group_identity
                .entry(identity.top_level.as_str())
                .or_insert(identity);
        }

        let mut top_badges: BTreeMap<String, GitBadge> = BTreeMap::new();
        let mut top_infos: BTreeMap<String, WorktreeInfo> = BTreeMap::new();
        let mut vw_by_common_dir: BTreeMap<String, Option<VwListOutput>> = BTreeMap::new();
        for (top_level, identity) in &group_identity {
            if let Ok(output) = runner.run(
                top_level,
                &[
                    "status",
                    "--porcelain=v2",
                    "--branch",
                    "--untracked-files=no",
                ],
            ) && let Ok(status) = parse_porcelain_branch_status(&output)
                && let Some(branch) = status.branch
            {
                top_badges.insert(
                    (*top_level).to_string(),
                    GitBadge {
                        branch,
                        ahead: status.ahead,
                        behind: status.behind,
                    },
                );
            }
            if identity.is_linked_worktree() {
                let mut info = WorktreeInfo {
                    name: path_basename(top_level).unwrap_or_else(|| (*top_level).to_string()),
                    path: (*top_level).to_string(),
                    source: WorktreeSource::GitLinked,
                    branch: None,
                    dirty: None,
                    locked: None,
                };
                let vw_list = vw_by_common_dir
                    .entry(identity.git_common_dir.clone())
                    .or_insert_with(|| query_vw_worktrees(runner, top_level).ok().flatten());
                if let Some(vw_list) = vw_list.as_ref() {
                    info = enrich_with_vw_metadata(info, vw_list);
                }
                top_infos.insert((*top_level).to_string(), info);
            }
        }

        let mut badges = BTreeMap::new();
        let mut worktrees = BTreeMap::new();
        for (path, identity) in &identities {
            if let Some(badge) = top_badges.get(&identity.top_level) {
                badges.insert(path.clone(), badge.clone());
            }
            if let Some(info) = top_infos.get(&identity.top_level) {
                worktrees.insert(path.clone(), info.clone());
            }
        }
        (badges, worktrees)
    }

    fn resolve_identity(
        &mut self,
        runner: &dyn GitRunner,
        path: &str,
        now: Instant,
    ) -> Option<WorktreeIdentity> {
        self.use_counter += 1;
        if let Some(entry) = self.cache.get_mut(path)
            && now.duration_since(entry.cached_at) < GIT_PROBE_CACHE_TTL
        {
            entry.last_used = self.use_counter;
            return entry.identity.clone();
        }
        let identity = probe_worktree_identity(runner, path);
        self.cache.insert(
            path.to_string(),
            ProbeCacheEntry {
                identity: identity.clone(),
                cached_at: now,
                last_used: self.use_counter,
            },
        );
        while self.cache.len() > GIT_PROBE_CACHE_CAPACITY {
            let Some(least_recent) = self
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.cache.remove(&least_recent);
        }
        identity
    }
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
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        vw_calls: std::sync::Mutex<Vec<Vec<String>>>,
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

        fn git_calls(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn probe_calls(&self) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.get(1).map(String::as_str) == Some("rev-parse"))
                .count()
        }

        fn status_calls(&self) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.get(1).map(String::as_str) == Some("status"))
                .count()
        }

        fn vw_call_count(&self) -> usize {
            self.vw_calls.lock().unwrap().len()
        }
    }

    impl GitRunner for MockGitRunner {
        fn run(&self, cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            let mut key = vec![cwd.to_string()];
            key.extend(args.iter().map(|value| value.to_string()));
            self.calls.lock().unwrap().push(key.clone());
            self.responses
                .get(&key)
                .map(|response| response.clone().map_err(|message| anyhow::anyhow!(message)))
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!("missing git stub: {key:?}"))
        }

        fn run_vw(&self, cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            let mut key = vec![cwd.to_string()];
            key.extend(args.iter().map(|value| value.to_string()));
            self.vw_calls.lock().unwrap().push(key.clone());
            self.vw_responses
                .get(&key)
                .map(|response| response.clone().map_err(|message| anyhow::anyhow!(message)))
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!("missing vw stub: {key:?}"))
        }
    }

    const PROBE_ARGS: [&str; 5] = [
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-dir",
        "--git-common-dir",
    ];

    fn stub_identity_probe(
        runner: &mut MockGitRunner,
        cwd: &str,
        top_level: &str,
        git_dir: &str,
        common_dir: &str,
        superproject: &str,
    ) {
        let _ = PROBE_ARGS;
        let mut output = format!("{top_level}\n{git_dir}\n{common_dir}\n");
        if !superproject.is_empty() {
            output.push_str(superproject);
            output.push('\n');
        }
        runner.stub(
            &[
                cwd,
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
                "--show-superproject-working-tree",
            ],
            &output,
        );
    }

    fn stub_status(runner: &mut MockGitRunner, cwd: &str, body: &str) {
        runner.stub(
            &[
                cwd,
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=no",
            ],
            body,
        );
    }

    #[test]
    fn porcelain_branch_status_parses_branch_with_upstream_counts() {
        let status = parse_porcelain_branch_status(
            "# branch.oid 0123abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -3\n1 .M N... 100644 100644 100644 abc def src/lib.rs\n",
        )
        .unwrap();

        assert_eq!(
            status,
            PorcelainBranchStatus {
                branch: Some("main".to_string()),
                ahead: 2,
                behind: 3,
            }
        );
    }

    #[test]
    fn porcelain_branch_status_defaults_to_zero_without_upstream() {
        let status =
            parse_porcelain_branch_status("# branch.oid 0123abc\n# branch.head feature\n").unwrap();

        assert_eq!(
            status,
            PorcelainBranchStatus {
                branch: Some("feature".to_string()),
                ahead: 0,
                behind: 0,
            }
        );
    }

    #[test]
    fn porcelain_branch_status_parses_ahead_only_behind_only_and_diverged() {
        let ahead_only = parse_porcelain_branch_status(
            "# branch.head main\n# branch.upstream origin/main\n# branch.ab +4 -0\n",
        )
        .unwrap();
        assert_eq!((ahead_only.ahead, ahead_only.behind), (4, 0));

        let behind_only = parse_porcelain_branch_status(
            "# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -7\n",
        )
        .unwrap();
        assert_eq!((behind_only.ahead, behind_only.behind), (0, 7));

        let diverged = parse_porcelain_branch_status(
            "# branch.head main\n# branch.upstream origin/main\n# branch.ab +5 -6\n",
        )
        .unwrap();
        assert_eq!((diverged.ahead, diverged.behind), (5, 6));
    }

    #[test]
    fn porcelain_branch_status_reports_detached_head_without_branch() {
        let status =
            parse_porcelain_branch_status("# branch.oid 0123abc\n# branch.head (detached)\n")
                .unwrap();

        assert_eq!(status.branch, None);
    }

    #[test]
    fn porcelain_branch_status_rejects_malformed_headers() {
        assert!(parse_porcelain_branch_status("").is_err());
        assert!(parse_porcelain_branch_status("1 .M N... file\n").is_err());
        assert!(parse_porcelain_branch_status("# branch.head main\n# branch.ab bogus\n").is_err());
        assert!(parse_porcelain_branch_status("# branch.head main\n# branch.ab 2 -3\n").is_err());
        assert!(parse_porcelain_branch_status("# branch.head main\n# branch.ab +2 3\n").is_err());
    }

    fn main_and_linked_runner() -> MockGitRunner {
        let mut runner = MockGitRunner::default();
        stub_identity_probe(
            &mut runner,
            "/tmp/main",
            "/tmp/main",
            "/tmp/main/.git",
            "/tmp/main/.git",
            "",
        );
        stub_identity_probe(
            &mut runner,
            "/tmp/main/sub",
            "/tmp/main",
            "/tmp/main/.git",
            "/tmp/main/.git",
            "",
        );
        stub_identity_probe(
            &mut runner,
            "/tmp/worktrees/feature",
            "/tmp/worktrees/feature",
            "/tmp/main/.git/worktrees/feature",
            "/tmp/main/.git",
            "",
        );
        stub_status(
            &mut runner,
            "/tmp/main",
            "# branch.head main\n# branch.upstream origin/main\n# branch.ab +1 -2\n",
        );
        stub_status(
            &mut runner,
            "/tmp/worktrees/feature",
            "# branch.head feature\n",
        );
        runner.stub_vw(
            &["/tmp/worktrees/feature", "list", "--json"],
            r#"{"repoRoot": "/tmp/main", "managedWorktreeRoot": "/tmp/worktrees", "worktrees": [{"branch": "feature", "path": "/tmp/worktrees/feature", "dirty": true, "locked": false}]}"#,
        );
        runner
    }

    #[test]
    fn steady_state_poll_dedupes_status_by_worktree_top_level() {
        let runner = main_and_linked_runner();
        let mut poller = GitPoller::new();
        let paths = ["/tmp/main", "/tmp/main/sub", "/tmp/worktrees/feature"];
        let now = Instant::now();

        let (badges, worktrees) = poller.poll(&runner, paths, now);

        assert_eq!(badges.len(), 3);
        assert_eq!(badges["/tmp/main"], badges["/tmp/main/sub"]);
        assert_eq!(badges["/tmp/main"].branch, "main");
        assert_eq!(badges["/tmp/main"].ahead, 1);
        assert_eq!(badges["/tmp/main"].behind, 2);
        assert_eq!(badges["/tmp/worktrees/feature"].branch, "feature");
        assert_eq!(worktrees.len(), 1);
        assert_eq!(
            worktrees["/tmp/worktrees/feature"].source,
            WorktreeSource::VwManaged
        );
        assert_eq!(worktrees["/tmp/worktrees/feature"].dirty, Some(true));
        // Cold cache: one probe per unique path, one status per worktree.
        assert_eq!(runner.probe_calls(), 3);
        assert_eq!(runner.status_calls(), 2);
        assert_eq!(runner.vw_call_count(), 1);

        let (warm_badges, warm_worktrees) =
            poller.poll(&runner, paths, now + Duration::from_secs(30));

        assert_eq!(warm_badges, badges);
        assert_eq!(warm_worktrees, worktrees);
        // Warm cache: no probes, one status per worktree, one vw per common dir.
        assert_eq!(runner.probe_calls(), 3);
        assert_eq!(runner.status_calls(), 4);
        assert_eq!(runner.vw_call_count(), 2);
    }

    #[test]
    fn nine_worktrees_cost_one_status_command_each_per_poll_when_warm() {
        let mut runner = MockGitRunner::default();
        let mut paths = Vec::new();
        for index in 0..9 {
            let top = format!("/tmp/worktrees/wt{index}");
            stub_identity_probe(
                &mut runner,
                &top,
                &top,
                &format!("/tmp/main/.git/worktrees/wt{index}"),
                "/tmp/main/.git",
                "",
            );
            let sub = format!("{top}/src");
            stub_identity_probe(
                &mut runner,
                &sub,
                &top,
                &format!("/tmp/main/.git/worktrees/wt{index}"),
                "/tmp/main/.git",
                "",
            );
            stub_status(&mut runner, &top, &format!("# branch.head wt{index}\n"));
            runner.stub_vw_error(&[&top, "list", "--json"], "vw missing");
            paths.push(top);
            paths.push(sub);
        }
        let mut poller = GitPoller::new();
        let now = Instant::now();

        let (badges, worktrees) = poller.poll(&runner, paths.iter().map(String::as_str), now);
        assert_eq!(badges.len(), 18);
        assert_eq!(worktrees.len(), 18);
        // Cold: 18 probes + 9 status commands.
        assert_eq!(runner.probe_calls(), 18);
        assert_eq!(runner.status_calls(), 9);

        poller.poll(
            &runner,
            paths.iter().map(String::as_str),
            now + Duration::from_secs(10),
        );
        // Warm: exactly one git command per worktree per poll.
        assert_eq!(runner.probe_calls(), 18);
        assert_eq!(runner.status_calls(), 18);
        // vw is shared per common git dir: one probe per poll.
        assert_eq!(runner.vw_call_count(), 2);
    }

    #[test]
    fn probe_cache_ttl_expiry_reprobes_paths() {
        let runner = main_and_linked_runner();
        let mut poller = GitPoller::new();
        let paths = ["/tmp/main", "/tmp/main/sub", "/tmp/worktrees/feature"];
        let now = Instant::now();

        poller.poll(&runner, paths, now);
        assert_eq!(runner.probe_calls(), 3);

        poller.poll(
            &runner,
            paths,
            now + GIT_PROBE_CACHE_TTL + Duration::from_secs(1),
        );
        assert_eq!(runner.probe_calls(), 6);
    }

    #[test]
    fn probe_cache_evicts_least_recently_used_beyond_capacity() {
        let mut runner = MockGitRunner::default();
        let paths = (0..=GIT_PROBE_CACHE_CAPACITY)
            .map(|index| format!("/tmp/repo/sub{index:04}"))
            .collect::<Vec<_>>();
        for path in &paths {
            stub_identity_probe(
                &mut runner,
                path,
                "/tmp/repo",
                "/tmp/repo/.git",
                "/tmp/repo/.git",
                "",
            );
        }
        stub_status(&mut runner, "/tmp/repo", "# branch.head main\n");
        let mut poller = GitPoller::new();
        let now = Instant::now();

        let first_256 = paths[..GIT_PROBE_CACHE_CAPACITY]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        poller.poll(&runner, first_256.iter().copied(), now);
        assert_eq!(runner.probe_calls(), 256);

        poller.poll(
            &runner,
            first_256.iter().copied(),
            now + Duration::from_secs(1),
        );
        assert_eq!(runner.probe_calls(), 256);

        // The 257th path overflows the capacity and evicts the least recently
        // used entry, which is the first path touched in this poll.
        poller.poll(
            &runner,
            paths.iter().map(String::as_str),
            now + Duration::from_secs(2),
        );
        assert_eq!(runner.probe_calls(), 257);

        poller.poll(&runner, [paths[0].as_str()], now + Duration::from_secs(3));
        assert_eq!(runner.probe_calls(), 258);
    }

    #[test]
    fn submodule_gets_badge_but_no_worktree_info() {
        let mut runner = MockGitRunner::default();
        stub_identity_probe(
            &mut runner,
            "/tmp/app/vendor/lib",
            "/tmp/app/vendor/lib",
            "/tmp/app/.git/modules/lib",
            "/tmp/app/.git/modules/lib",
            "/tmp/app",
        );
        stub_status(&mut runner, "/tmp/app/vendor/lib", "# branch.head main\n");
        let mut poller = GitPoller::new();

        let (badges, worktrees) = poller.poll(&runner, ["/tmp/app/vendor/lib"], Instant::now());

        assert_eq!(badges["/tmp/app/vendor/lib"].branch, "main");
        assert!(worktrees.is_empty());
        assert_eq!(runner.vw_call_count(), 0);
    }

    #[test]
    fn detached_head_has_no_badge_but_keeps_worktree_info() {
        let mut runner = MockGitRunner::default();
        stub_identity_probe(
            &mut runner,
            "/tmp/worktrees/feature",
            "/tmp/worktrees/feature",
            "/tmp/main/.git/worktrees/feature",
            "/tmp/main/.git",
            "",
        );
        stub_status(
            &mut runner,
            "/tmp/worktrees/feature",
            "# branch.oid abc\n# branch.head (detached)\n",
        );
        runner.stub_vw_error(&["/tmp/worktrees/feature", "list", "--json"], "vw missing");
        let mut poller = GitPoller::new();

        let (badges, worktrees) = poller.poll(&runner, ["/tmp/worktrees/feature"], Instant::now());

        assert!(badges.is_empty());
        assert_eq!(worktrees["/tmp/worktrees/feature"].name, "feature");
        assert_eq!(
            worktrees["/tmp/worktrees/feature"].source,
            WorktreeSource::GitLinked
        );
    }

    #[test]
    fn linked_worktrees_on_same_common_dir_share_vw_but_keep_separate_metadata() {
        let mut runner = MockGitRunner::default();
        stub_identity_probe(
            &mut runner,
            "/tmp/worktrees/alpha",
            "/tmp/worktrees/alpha",
            "/tmp/main/.git/worktrees/alpha",
            "/tmp/main/.git",
            "",
        );
        stub_identity_probe(
            &mut runner,
            "/tmp/worktrees/beta",
            "/tmp/worktrees/beta",
            "/tmp/main/.git/worktrees/beta",
            "/tmp/main/.git",
            "",
        );
        stub_status(&mut runner, "/tmp/worktrees/alpha", "# branch.head alpha\n");
        stub_status(&mut runner, "/tmp/worktrees/beta", "# branch.head beta\n");
        let vw_output = r#"{"repoRoot": "/tmp/main", "managedWorktreeRoot": "/tmp/worktrees", "worktrees": [{"branch": "alpha", "path": "/tmp/worktrees/alpha", "dirty": false, "locked": false}, {"branch": "beta", "path": "/tmp/worktrees/beta", "dirty": true, "locked": {"value": true, "reason": "review"}}]}"#;
        runner.stub_vw(&["/tmp/worktrees/alpha", "list", "--json"], vw_output);
        runner.stub_vw(&["/tmp/worktrees/beta", "list", "--json"], vw_output);
        let mut poller = GitPoller::new();

        let (badges, worktrees) = poller.poll(
            &runner,
            ["/tmp/worktrees/alpha", "/tmp/worktrees/beta"],
            Instant::now(),
        );

        assert_eq!(runner.vw_call_count(), 1);
        assert_eq!(badges["/tmp/worktrees/alpha"].branch, "alpha");
        assert_eq!(badges["/tmp/worktrees/beta"].branch, "beta");
        assert_eq!(
            worktrees["/tmp/worktrees/alpha"].branch.as_deref(),
            Some("alpha")
        );
        assert_eq!(worktrees["/tmp/worktrees/alpha"].dirty, Some(false));
        assert_eq!(worktrees["/tmp/worktrees/alpha"].locked, Some(false));
        assert_eq!(
            worktrees["/tmp/worktrees/beta"].branch.as_deref(),
            Some("beta")
        );
        assert_eq!(worktrees["/tmp/worktrees/beta"].dirty, Some(true));
        assert_eq!(worktrees["/tmp/worktrees/beta"].locked, Some(true));
        assert_eq!(
            worktrees["/tmp/worktrees/alpha"].source,
            WorktreeSource::VwManaged
        );
        assert_eq!(
            worktrees["/tmp/worktrees/beta"].source,
            WorktreeSource::VwManaged
        );
    }

    #[test]
    fn vw_failure_or_malformed_json_leaves_worktree_git_linked() {
        for vw_setup in ["missing", "malformed", "no-match"] {
            let mut runner = MockGitRunner::default();
            stub_identity_probe(
                &mut runner,
                "/tmp/worktrees/feature",
                "/tmp/worktrees/feature",
                "/tmp/main/.git/worktrees/feature",
                "/tmp/main/.git",
                "",
            );
            stub_status(
                &mut runner,
                "/tmp/worktrees/feature",
                "# branch.head feature\n",
            );
            match vw_setup {
                "missing" => runner
                    .stub_vw_error(&["/tmp/worktrees/feature", "list", "--json"], "vw missing"),
                "malformed" => {
                    runner.stub_vw(&["/tmp/worktrees/feature", "list", "--json"], "{not-json")
                }
                _ => runner.stub_vw(
                    &["/tmp/worktrees/feature", "list", "--json"],
                    r#"{"repoRoot": "/tmp/main", "managedWorktreeRoot": "/tmp/worktrees", "worktrees": [{"branch": "other", "path": "/tmp/worktrees/other", "dirty": false, "locked": false}]}"#,
                ),
            }
            let mut poller = GitPoller::new();

            let (_, worktrees) = poller.poll(&runner, ["/tmp/worktrees/feature"], Instant::now());

            assert_eq!(
                worktrees["/tmp/worktrees/feature"].source,
                WorktreeSource::GitLinked,
                "vw setup: {vw_setup}"
            );
            assert_eq!(worktrees["/tmp/worktrees/feature"].name, "feature");
            assert_eq!(worktrees["/tmp/worktrees/feature"].branch, None);
        }
    }

    #[test]
    fn non_git_paths_produce_no_results_and_are_negatively_cached() {
        let mut runner = MockGitRunner::default();
        runner.stub_error(
            &[
                "/tmp/plain",
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
                "--show-superproject-working-tree",
            ],
            "not a git repository",
        );
        let mut poller = GitPoller::new();
        let now = Instant::now();

        let (badges, worktrees) = poller.poll(&runner, ["/tmp/plain"], now);
        assert!(badges.is_empty());
        assert!(worktrees.is_empty());
        assert_eq!(runner.git_calls(), 1);

        poller.poll(&runner, ["/tmp/plain"], now + Duration::from_secs(1));
        // The negative probe result is cached for the TTL as well.
        assert_eq!(runner.git_calls(), 1);
    }
}

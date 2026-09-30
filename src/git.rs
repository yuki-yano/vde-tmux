use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
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
    pub insertions: u64,
    pub deletions: u64,
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
    fn untracked_insertions(&self, cwd: &str) -> Result<u64>;

    fn probe_worktree(&self, cwd: &str, args: &[&str]) -> Result<Option<String>> {
        match self.run(cwd, args) {
            Ok(output) => Ok(Some(output)),
            Err(error)
                if error
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("not a git repository") =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
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

    fn untracked_insertions(&self, cwd: &str) -> Result<u64> {
        untracked_insertions(cwd, self.timeout)
    }

    fn probe_worktree(&self, cwd: &str, args: &[&str]) -> Result<Option<String>> {
        run_git_probe_command(cwd, args, self.timeout)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitDiffStat {
    pub insertions: u64,
    pub deletions: u64,
}

pub fn parse_numstat(raw: &str) -> Result<GitDiffStat> {
    let mut stat = GitDiffStat::default();
    for line in raw.lines() {
        let mut fields = line.splitn(3, '\t');
        let insertions = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("numstat line lacks insertions: {line:?}"))?;
        let deletions = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("numstat line lacks deletions: {line:?}"))?;
        let path = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("numstat line lacks path: {line:?}"))?;
        if path.is_empty() {
            bail!("numstat line has an empty path: {line:?}");
        }
        if insertions == "-" && deletions == "-" {
            continue;
        }
        if insertions == "-" || deletions == "-" {
            bail!("numstat line mixes binary and text counts: {line:?}");
        }
        let insertions = insertions
            .parse::<u64>()
            .with_context(|| format!("invalid numstat insertions: {line:?}"))?;
        let deletions = deletions
            .parse::<u64>()
            .with_context(|| format!("invalid numstat deletions: {line:?}"))?;
        stat.insertions = stat
            .insertions
            .checked_add(insertions)
            .ok_or_else(|| anyhow::anyhow!("numstat insertion total overflowed"))?;
        stat.deletions = stat
            .deletions
            .checked_add(deletions)
            .ok_or_else(|| anyhow::anyhow!("numstat deletion total overflowed"))?;
    }
    Ok(stat)
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

pub fn probe_worktree_identity(runner: &dyn GitRunner, path: &str) -> Option<WorktreeIdentity> {
    probe_worktree_identity_result(runner, path).ok().flatten()
}

pub fn probe_worktree_identity_result(
    runner: &dyn GitRunner,
    path: &str,
) -> Result<Option<WorktreeIdentity>> {
    let Some(output) = runner.probe_worktree(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
            "--show-superproject-working-tree",
        ],
    )?
    else {
        return Ok(None);
    };
    let lines = output.lines().map(str::trim).collect::<Vec<_>>();
    // `--show-superproject-working-tree` prints nothing outside a submodule,
    // so a plain worktree yields exactly three lines.
    match lines.as_slice() {
        [top_level, git_dir, git_common_dir] | [top_level, git_dir, git_common_dir, ""]
            if !top_level.is_empty() =>
        {
            Ok(Some(WorktreeIdentity {
                top_level: (*top_level).to_string(),
                git_dir: (*git_dir).to_string(),
                git_common_dir: (*git_common_dir).to_string(),
                superproject: None,
            }))
        }
        [top_level, git_dir, git_common_dir, superproject]
            if !top_level.is_empty() && !superproject.is_empty() =>
        {
            Ok(Some(WorktreeIdentity {
                top_level: (*top_level).to_string(),
                git_dir: (*git_dir).to_string(),
                git_common_dir: (*git_common_dir).to_string(),
                superproject: Some((*superproject).to_string()),
            }))
        }
        _ => bail!("git rev-parse returned an invalid worktree identity for {path}"),
    }
}

pub const GIT_PROBE_CACHE_CAPACITY: usize = 256;
pub const GIT_PROBE_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct ProbeCacheEntry {
    identity: Option<WorktreeIdentity>,
    failed: bool,
    cached_at: Instant,
    last_used: u64,
}

/// Stateful steady-state poller owned by the daemon git worker. Pane paths are
/// resolved to worktree identities through a bounded TTL cache, deduplicated by
/// worktree top-level, and each worktree is refreshed with a single
/// branch status, tracked diff, and untracked-file scan.
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
        let (badges, worktrees, _) = self.poll_with_identities(runner, paths, now);
        (badges, worktrees)
    }

    pub fn poll_with_identities<'a>(
        &mut self,
        runner: &dyn GitRunner,
        paths: impl IntoIterator<Item = &'a str>,
        now: Instant,
    ) -> (
        BTreeMap<String, GitBadge>,
        BTreeMap<String, WorktreeInfo>,
        BTreeMap<String, crate::category::RepoIdentity>,
    ) {
        let input_paths = paths
            .into_iter()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let mut identities: BTreeMap<String, WorktreeIdentity> = BTreeMap::new();
        let mut failed_probes = BTreeSet::new();
        for path in &input_paths {
            if identities.contains_key(path) {
                continue;
            }
            let (identity, failed) = self.resolve_identity(runner, path, now);
            if failed {
                failed_probes.insert(path.clone());
            }
            if let Some(identity) = identity {
                identities.insert(path.clone(), identity);
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
                let diff = runner
                    .run(
                        top_level,
                        &["diff", "--no-ext-diff", "--numstat", "HEAD", "--"],
                    )
                    .ok()
                    .and_then(|output| parse_numstat(&output).ok())
                    .unwrap_or_default();
                top_badges.insert(
                    (*top_level).to_string(),
                    GitBadge {
                        branch,
                        ahead: status.ahead,
                        behind: status.behind,
                        insertions: diff.insertions.saturating_add(
                            runner.untracked_insertions(top_level).unwrap_or_default(),
                        ),
                        deletions: diff.deletions,
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
        let repo_identities = input_paths
            .into_iter()
            .filter_map(|path| {
                if failed_probes.contains(&path) {
                    return None;
                }
                let identity = identities
                    .get(&path)
                    .map(crate::category::RepoIdentity::from_worktree)
                    .unwrap_or_else(|| crate::category::RepoIdentity::from_project_path(&path))
                    .ok()?;
                Some((path, identity))
            })
            .collect();
        (badges, worktrees, repo_identities)
    }

    fn resolve_identity(
        &mut self,
        runner: &dyn GitRunner,
        path: &str,
        now: Instant,
    ) -> (Option<WorktreeIdentity>, bool) {
        self.use_counter += 1;
        if let Some(entry) = self.cache.get_mut(path)
            && now.duration_since(entry.cached_at) < GIT_PROBE_CACHE_TTL
        {
            entry.last_used = self.use_counter;
            return (entry.identity.clone(), entry.failed);
        }
        let (identity, failed) = match probe_worktree_identity_result(runner, path) {
            Ok(identity) => (identity, false),
            Err(_) => (None, true),
        };
        self.cache.insert(
            path.to_string(),
            ProbeCacheEntry {
                identity: identity.clone(),
                failed,
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
        (identity, failed)
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

/// Enumerate once per worktree, preserving arbitrary Unix filenames. The index
/// is only read: a newly staged file moves to the HEAD diff instead of being
/// counted twice. Git's standard excludes include repository and global ignores.
fn untracked_insertions(cwd: &str, timeout: Duration) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    let paths = run_untracked_git(
        cwd,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
        deadline,
    )?;
    if paths.is_empty() {
        return Ok(0);
    }
    let attributes = run_untracked_git(
        cwd,
        &["check-attr", "-z", "--stdin", "diff"],
        Some(&paths),
        deadline,
    )?;
    let fields = attributes.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.last() != Some(&b"".as_slice()) || (fields.len() - 1) % 3 != 0 {
        bail!("invalid untracked diff attributes");
    }
    let (attributes, _) = fields[..fields.len() - 1].as_chunks::<3>();
    let mut drivers = BTreeMap::new();
    if attributes
        .iter()
        .any(|fields| !matches!(fields[2], b"set" | b"unset" | b"unspecified"))
    {
        // A named driver can override binary detection. Query driver settings
        // in one batch, never launch one subprocess per untracked file.
        if let Ok(config) = run_untracked_git(
            cwd,
            &[
                "config",
                "--type=bool",
                "--null",
                "--get-regexp",
                r"^diff\..*\.binary$",
            ],
            None,
            deadline,
        ) {
            for record in config.split(|byte| *byte == 0) {
                let Some(separator) = record.iter().position(|byte| *byte == b'\n') else {
                    continue;
                };
                let (key, rest) = record.split_at(separator);
                let value = &rest[1..];
                let Some(name) = key
                    .strip_prefix(b"diff.")
                    .and_then(|key| key.strip_suffix(b".binary"))
                else {
                    continue;
                };
                let binary = match value {
                    b"true" | b"yes" | b"on" | b"1" | b"" => true,
                    b"false" | b"no" | b"off" | b"0" => false,
                    _ => continue,
                };
                drivers.insert(name.to_vec(), binary);
            }
        }
    }
    let mut total = 0u64;
    for [path, attribute, value] in attributes {
        let driver_binary = drivers.get(*value).copied();
        if *attribute != b"diff" || *value == b"unset" || driver_binary == Some(true) {
            continue;
        }
        let path = Path::new(cwd).join(std::ffi::OsStr::from_bytes(path));
        let lines = untracked_file_lines(
            &path,
            *value == b"set" || driver_binary == Some(false),
            deadline,
        )?;
        total = total
            .checked_add(lines)
            .ok_or_else(|| anyhow::anyhow!("untracked insertion total overflowed"))?;
    }
    Ok(total)
}

fn untracked_file_lines(path: &Path, force_text: bool, deadline: Instant) -> Result<u64> {
    if Instant::now() >= deadline {
        bail!("untracked file scan timed out");
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_symlink() {
        // Git stores the link target, not the contents of the destination.
        let target = std::fs::read_link(path)?;
        let bytes = target.as_os_str().as_bytes();
        return Ok(bytes.iter().filter(|byte| **byte == b'\n').count() as u64
            + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n")));
    }
    if !metadata.is_file() {
        return Ok(0);
    }
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    // A replacement FIFO must neither block the worker nor become file data.
    if !file.metadata()?.is_file() {
        return Ok(0);
    }
    let mut prefix = Vec::with_capacity(8000);
    Read::by_ref(&mut file)
        .take(8000)
        .read_to_end(&mut prefix)?;
    if !force_text && prefix.contains(&0) {
        return Ok(0);
    }
    let mut lines = prefix.iter().filter(|byte| **byte == b'\n').count() as u64;
    let mut last = prefix.last().copied();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if Instant::now() >= deadline {
            bail!("untracked file scan timed out");
        }
        let len = file.read(&mut buffer)?;
        if len == 0 {
            return Ok(lines + u64::from(last.is_some_and(|byte| byte != b'\n')));
        }
        lines += buffer[..len].iter().filter(|byte| **byte == b'\n').count() as u64;
        last = Some(buffer[len - 1]);
    }
}

/// ls-files/check-attr may exceed a pipe buffer. Drain both streams while
/// waiting, and bound the complete enumeration rather than returning a prefix.
fn run_untracked_git(
    cwd: &str,
    args: &[&str],
    input: Option<&[u8]>,
    deadline: Instant,
) -> Result<Vec<u8>> {
    const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;
    if Instant::now() >= deadline {
        bail!("untracked file scan timed out");
    }
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    fn read_pipe(
        pipe: impl Read + Send + 'static,
    ) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes)?;
            Ok(bytes)
        })
    }
    let stdout = read_pipe(child.stdout.take().expect("piped stdout"));
    let stderr = read_pipe(child.stderr.take().expect("piped stderr"));
    let writer = input.map(|input| {
        let input = input.to_vec();
        let mut stdin = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || stdin.write_all(&input))
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(anyhow::Error::from(error)),
            Ok(None) if Instant::now() >= deadline => {
                break Err(anyhow::anyhow!("untracked file scan timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = stdout.join().expect("stdout reader panicked");
    let stderr = stderr.join().expect("stderr reader panicked");
    let written = writer.map(|writer| writer.join().expect("stdin writer panicked"));
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;
    if !status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&stderr));
    }
    if stdout.len() as u64 > OUTPUT_LIMIT || stderr.len() as u64 > OUTPUT_LIMIT {
        bail!("untracked file enumeration exceeded byte limit");
    }
    if let Some(written) = written {
        written?;
    }
    Ok(stdout)
}

fn run_git_command(cwd: &str, args: &[&str], timeout: Duration) -> Result<String> {
    run_process_command("git", cwd, args, timeout)
}

fn run_git_probe_command(cwd: &str, args: &[&str], timeout: Duration) -> Result<Option<String>> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn git in {cwd}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            let output = child
                .wait_with_output()
                .with_context(|| format!("failed to collect git probe output in {cwd}"))?;
            if output.status.success() {
                return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
            }
            return Ok(None);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {args:?} timed out after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
        untracked: BTreeMap<String, u64>,
        untracked_calls: std::sync::Mutex<Vec<String>>,
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

        fn diff_calls(&self) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.get(1).map(String::as_str) == Some("diff"))
                .count()
        }

        fn vw_call_count(&self) -> usize {
            self.vw_calls.lock().unwrap().len()
        }
    }

    impl GitRunner for MockGitRunner {
        fn untracked_insertions(&self, cwd: &str) -> Result<u64> {
            self.untracked_calls.lock().unwrap().push(cwd.to_string());
            Ok(self.untracked.get(cwd).copied().unwrap_or_default())
        }

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
        stub_diff(runner, cwd, "");
    }

    fn stub_diff(runner: &mut MockGitRunner, cwd: &str, body: &str) {
        runner.stub(
            &[cwd, "diff", "--no-ext-diff", "--numstat", "HEAD", "--"],
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

    #[test]
    fn numstat_sums_text_changes_and_ignores_binary_files() {
        let stat = parse_numstat(
            "12\t3\tsrc/lib.rs\n4\t9\told name => new name\n-\t-\tassets/image.png\n",
        )
        .unwrap();

        assert_eq!(
            stat,
            GitDiffStat {
                insertions: 16,
                deletions: 12,
            }
        );
    }

    #[test]
    fn numstat_rejects_malformed_or_overflowing_counts() {
        assert!(parse_numstat("1\t2\n").is_err());
        assert!(parse_numstat("-\t2\tfile\n").is_err());
        assert!(parse_numstat("x\t2\tfile\n").is_err());
        assert!(parse_numstat(&format!("{}\t0\ta\n1\t0\tb\n", u64::MAX)).is_err());
    }

    struct GitFixture(std::path::PathBuf);

    impl GitFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "vde-git-stat-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&root).unwrap();
            let fixture = Self(root);
            fixture.git(&["init", "-b", "main"]);
            fixture.git(&["config", "user.name", "Fixture"]);
            fixture.git(&["config", "user.email", "fixture@example.invalid"]);
            fixture.git(&["config", "core.excludesFile", "/dev/null"]);
            fixture
        }

        fn git(&self, args: &[&str]) {
            let output = Command::new("git")
                .args(args)
                .current_dir(&self.0)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn write(&self, path: impl AsRef<Path>, body: impl AsRef<[u8]>) {
            std::fs::write(self.0.join(path), body).unwrap();
        }

        fn count(&self) -> u64 {
            untracked_insertions(self.0.to_str().unwrap(), Duration::from_secs(5)).unwrap()
        }
    }

    impl Drop for GitFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn untracked_scan_preserves_names_excludes_ignored_binary_and_counts_link_targets() {
        let fixture = GitFixture::new();
        fixture.write(".gitignore", "ignored\n");
        fixture.write(
            ".gitattributes",
            "opaque -diff\nforced diff\ndriver diff=opaque\n",
        );
        fixture.git(&["add", ".gitignore", ".gitattributes"]);
        fixture.git(&["config", "diff.opaque.binary", "true"]);
        fixture.write("new\n\tfile", "one\ntwo");
        fixture.write("-leading-option", b"non-UTF-8 text: \xff\n");
        // APFS rejects non-UTF-8 names; Linux accepts them. Non-UTF-8 file
        // contents are covered on both platforms above.
        let invalid_name_lines = if cfg!(target_os = "macos") {
            0
        } else {
            fixture.write(std::ffi::OsStr::from_bytes(b"invalid-\xff"), "one\n");
            1
        };
        fixture.write("empty", "");
        fixture.write("binary", b"line\n\0binary\n");
        fixture.write("ignored", "not counted\n");
        fixture.write("opaque", "not counted\n");
        fixture.write("driver", "not counted\n");
        fixture.write("forced", b"one\0\ntwo\n");
        std::os::unix::fs::symlink("ignored", fixture.0.join("link")).unwrap();
        let fifo = std::ffi::CString::new(fixture.0.join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_eq!(fixture.count(), 6 + invalid_name_lines);
        fixture.git(&["add", "--", "new\n\tfile"]);
        assert_eq!(fixture.count(), 4 + invalid_name_lines);
    }

    #[test]
    fn poll_combines_head_diff_and_untracked_without_double_counting_partial_staging() {
        let fixture = GitFixture::new();
        fixture.write("tracked", "base\n");
        fixture.git(&["add", "tracked"]);
        fixture.git(&["commit", "-m", "Baseline"]);
        fixture.write("tracked", "base\nstaged\n");
        fixture.git(&["add", "tracked"]);
        fixture.write("tracked", "base\nreplacement\nthird\n");
        fixture.write("new", "one\ntwo");
        std::fs::create_dir(fixture.0.join("sub")).unwrap();
        let cwd = fixture.0.to_str().unwrap();
        let sub = fixture.0.join("sub");
        let runner = SystemGitRunner::new(Duration::from_secs(5));
        let mut poller = GitPoller::new();
        let (badges, _) = poller.poll(&runner, [cwd, sub.to_str().unwrap()], Instant::now());
        assert_eq!(badges[cwd].insertions, 4);
        assert_eq!(badges[cwd].deletions, 0);
        assert_eq!(badges[cwd], badges[sub.to_str().unwrap()]);
        fixture.git(&["add", "new"]);
        let (badges, _) = poller.poll(&runner, [cwd], Instant::now());
        assert_eq!(badges[cwd].insertions, 4);
    }

    #[test]
    fn untracked_scan_drains_large_enumerations_and_streams_large_files() {
        let fixture = GitFixture::new();
        for index in 0..900 {
            fixture.write(format!("{index:04}-{}", "x".repeat(96)), "one\n");
        }
        fixture.write("large", "one\n".repeat(40_000));
        assert_eq!(fixture.count(), 40_900);
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
        stub_diff(&mut runner, "/tmp/main", "12\t3\tsrc/lib.rs\n");
        stub_status(
            &mut runner,
            "/tmp/worktrees/feature",
            "# branch.head feature\n",
        );
        stub_diff(
            &mut runner,
            "/tmp/worktrees/feature",
            "4\t9\tsrc/sidebar.rs\n",
        );
        runner.stub_vw(
            &["/tmp/worktrees/feature", "list", "--json"],
            r#"{"repoRoot": "/tmp/main", "managedWorktreeRoot": "/tmp/worktrees", "worktrees": [{"branch": "feature", "path": "/tmp/worktrees/feature", "dirty": true, "locked": false}]}"#,
        );
        runner
    }

    #[test]
    fn steady_state_poll_dedupes_status_by_worktree_top_level() {
        let mut runner = main_and_linked_runner();
        runner.untracked.insert("/tmp/main".to_string(), 5);
        runner
            .untracked
            .insert("/tmp/worktrees/feature".to_string(), 8);
        let mut poller = GitPoller::new();
        let paths = ["/tmp/main", "/tmp/main/sub", "/tmp/worktrees/feature"];
        let now = Instant::now();

        let (badges, worktrees) = poller.poll(&runner, paths, now);

        assert_eq!(badges.len(), 3);
        assert_eq!(badges["/tmp/main"], badges["/tmp/main/sub"]);
        assert_eq!(badges["/tmp/main"].branch, "main");
        assert_eq!(badges["/tmp/main"].ahead, 1);
        assert_eq!(badges["/tmp/main"].behind, 2);
        assert_eq!(badges["/tmp/main"].insertions, 17);
        assert_eq!(badges["/tmp/main"].deletions, 3);
        assert_eq!(badges["/tmp/worktrees/feature"].branch, "feature");
        assert_eq!(badges["/tmp/worktrees/feature"].insertions, 12);
        assert_eq!(badges["/tmp/worktrees/feature"].deletions, 9);
        assert_eq!(worktrees.len(), 1);
        assert_eq!(
            worktrees["/tmp/worktrees/feature"].source,
            WorktreeSource::VwManaged
        );
        assert_eq!(worktrees["/tmp/worktrees/feature"].dirty, Some(true));
        // Cold cache: one probe per unique path, one status per worktree.
        assert_eq!(runner.probe_calls(), 3);
        assert_eq!(runner.status_calls(), 2);
        assert_eq!(runner.diff_calls(), 2);
        assert_eq!(runner.untracked_calls.lock().unwrap().len(), 2);
        assert_eq!(runner.vw_call_count(), 1);

        let (warm_badges, warm_worktrees) =
            poller.poll(&runner, paths, now + Duration::from_secs(30));

        assert_eq!(warm_badges, badges);
        assert_eq!(warm_worktrees, worktrees);
        // Warm cache: no probes, one status per worktree, one vw per common dir.
        assert_eq!(runner.probe_calls(), 3);
        assert_eq!(runner.status_calls(), 4);
        assert_eq!(runner.diff_calls(), 4);
        assert_eq!(runner.untracked_calls.lock().unwrap().len(), 4);
        assert_eq!(runner.vw_call_count(), 2);

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

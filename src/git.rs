use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::options::snapshot::PaneSnapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBadge {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
}

pub trait GitRunner {
    fn run(&self, cwd: &str, args: &[&str]) -> Result<String>;
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

pub fn collect_git_badges(
    runner: &dyn GitRunner,
    panes: &[PaneSnapshot],
) -> std::collections::BTreeMap<String, GitBadge> {
    let mut badges = std::collections::BTreeMap::new();
    for path in panes
        .iter()
        .filter(|pane| !pane.agent.trim().is_empty())
        .map(|pane| pane.current_path.trim())
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

fn run_git_command(cwd: &str, args: &[&str], timeout: Duration) -> Result<String> {
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
                .with_context(|| format!("failed to collect git output in {cwd}"))?;
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            bail!(
                "git {args:?} failed in {cwd}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {args:?} timed out after {timeout:?}");
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
    fn collect_git_badges_uses_unique_agent_paths() {
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
        let panes = vec![
            crate::options::snapshot::PaneSnapshot {
                current_path: "/tmp/repo".to_string(),
                agent: "codex".to_string(),
                ..Default::default()
            },
            crate::options::snapshot::PaneSnapshot {
                current_path: "/tmp/repo".to_string(),
                agent: "claude".to_string(),
                ..Default::default()
            },
            crate::options::snapshot::PaneSnapshot {
                current_path: "/tmp/not-repo".to_string(),
                agent: "".to_string(),
                ..Default::default()
            },
        ];

        let badges = collect_git_badges(&runner, &panes);

        assert_eq!(badges.len(), 1);
        assert_eq!(badges["/tmp/repo"].branch, "main");
    }
}

use std::collections::BTreeMap;

use anyhow::Result;

use crate::config::{Config, SidebarWidth};
use crate::tmux::TmuxRunner;

pub fn render_once(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &Config,
) -> Result<String> {
    render_once_with_git_runner(runner, env, config, &crate::git::SystemGitRunner::default())
}

fn render_once_with_git_runner(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    config: &Config,
    git_runner: &dyn crate::git::GitRunner,
) -> Result<String> {
    let panes = crate::options::snapshot::read_all_panes(runner)?;
    let state_path = crate::sidebar::store::state_path(env);
    let state = crate::sidebar::store::load_state(&state_path)?;
    let git = crate::git::collect_git_badges(git_runner, &panes);
    let worktrees = crate::git::collect_worktree_infos(git_runner, &panes);
    let rows = crate::sidebar::tree::build_rows_with_git_and_worktrees(
        config, &panes, &state, &git, &worktrees,
    );
    let width = match config.sidebar.width {
        SidebarWidth::Columns(width) => width,
        SidebarWidth::Percent(_) => config.sidebar.min_width,
    };
    Ok(crate::sidebar::render::render_rows(
        &rows,
        &state,
        width as usize,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::snapshot::snapshot_format;
    use crate::tmux::mock::MockTmuxRunner;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingGitRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl RecordingGitRunner {
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl crate::git::GitRunner for RecordingGitRunner {
        fn run(&self, cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            let mut call = vec!["git".to_string(), cwd.to_string()];
            call.extend(args.iter().map(|arg| arg.to_string()));
            self.calls.lock().unwrap().push(call);
            match args {
                ["branch", "--show-current"] => anyhow::bail!("no branch"),
                ["rev-parse", "--show-toplevel"] => Ok("/tmp/worktrees/feature\n".to_string()),
                ["rev-parse", "--git-dir"] => Ok("/tmp/repo/.git/worktrees/feature\n".to_string()),
                ["rev-parse", "--git-common-dir"] => Ok("/tmp/repo/.git\n".to_string()),
                ["rev-parse", "--show-superproject-working-tree"] => Ok("\n".to_string()),
                _ => anyhow::bail!("unexpected git args: {args:?}"),
            }
        }

        fn run_vw(&self, cwd: &str, args: &[&str]) -> anyhow::Result<String> {
            let mut call = vec!["vw".to_string(), cwd.to_string()];
            call.extend(args.iter().map(|arg| arg.to_string()));
            self.calls.lock().unwrap().push(call);
            anyhow::bail!("vw unavailable")
        }
    }

    fn snapshot_line(fields: &[&str]) -> String {
        fields.join("\u{1f}")
    }

    #[test]
    fn render_once_collects_worktree_infos() {
        let tmux = MockTmuxRunner::new();
        let format = snapshot_format();
        tmux.stub(
            &["list-panes", "-a", "-F", &format],
            &snapshot_line(&[
                "main",
                "@1",
                "%1",
                "/tmp/worktrees/feature",
                "codex",
                "/dev/ttys001",
                "123",
                "1",
                "1",
                "",
                "codex",
                "running",
                "fix bug",
                "user",
                "",
                "",
                "100",
                "",
                "",
                "",
                "",
                "",
            ]),
        );
        let git = RecordingGitRunner::default();

        let _ =
            render_once_with_git_runner(&tmux, &BTreeMap::new(), &Config::default(), &git).unwrap();

        assert!(git.calls().iter().any(|call| {
            call == &[
                "git",
                "/tmp/worktrees/feature",
                "rev-parse",
                "--show-toplevel",
            ]
        }));
    }
}

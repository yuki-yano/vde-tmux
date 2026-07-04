//! vtm session-manager 相当の tmux command。

use anyhow::Result;

use crate::tmux::TmuxRunner;

pub fn popup_shell_command() -> String {
    "tmux list-sessions -F '#{session_name}' | fzf --reverse | xargs -r tmux switch-client -t"
        .to_string()
}

pub fn open_tree(runner: &dyn TmuxRunner) -> Result<()> {
    runner.run(&["choose-tree", "-Zw"])?;
    Ok(())
}

pub fn open_popup(runner: &dyn TmuxRunner) -> Result<()> {
    let command = popup_shell_command();
    runner.run(&["display-popup", "-E", "-w", "80%", "-h", "70%", &command])?;
    Ok(())
}

pub fn kill_window(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    runner.run(&["kill-window", "-t", target])?;
    Ok(())
}

pub fn kill_pane(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    runner.run(&["kill-pane", "-t", target])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    #[test]
    fn choose_tree_uses_tmux_choose_tree() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["choose-tree", "-Zw"], "");
        open_tree(&mock).unwrap();
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn popup_uses_display_popup() {
        let mock = MockTmuxRunner::new();
        let command = popup_shell_command();
        mock.stub(
            &["display-popup", "-E", "-w", "80%", "-h", "70%", &command],
            "",
        );
        open_popup(&mock).unwrap();
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn kill_window_issues_tmux_kill_window() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["kill-window", "-t", "@2"], "");
        kill_window(&mock, "@2").unwrap();
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn kill_pane_issues_tmux_kill_pane() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["kill-pane", "-t", "%3"], "");
        kill_pane(&mock, "%3").unwrap();
        assert_eq!(mock.calls().len(), 1);
    }
}

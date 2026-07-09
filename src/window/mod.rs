use anyhow::Result;

use crate::tmux::TmuxRunner;

const FIELD_SEP: char = '\u{1f}';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub session: String,
    pub index: String,
    pub id: String,
    pub name: String,
    pub panes: i64,
    pub active: bool,
    pub last: bool,
    pub bell: bool,
    pub activity: bool,
    pub silence: bool,
    pub command: String,
    pub badge: String,
    pub state: String,
    pub agent_counts: String,
}

pub fn window_list_format() -> String {
    [
        "#{session_name}",
        "#{window_index}",
        "#{window_id}",
        "#{window_name}",
        "#{window_panes}",
        "#{window_active}",
        "#{window_last_flag}",
        "#{window_bell_flag}",
        "#{window_activity_flag}",
        "#{window_silence_flag}",
        "#{pane_current_command}",
        "#{@vde_window_status}",
        "#{@vde_window_state}",
        "#{@vde_window_agent_counts}",
    ]
    .join(&FIELD_SEP.to_string())
}

pub fn parse_windows(output: &str) -> Vec<WindowInfo> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEP).collect::<Vec<_>>();
            if fields.len() != 14 || fields[0].is_empty() || fields[2].is_empty() {
                return None;
            }
            Some(WindowInfo {
                session: fields[0].to_string(),
                index: fields[1].to_string(),
                id: fields[2].to_string(),
                name: if fields[3].is_empty() {
                    "(unnamed)".to_string()
                } else {
                    fields[3].to_string()
                },
                panes: fields[4].parse().unwrap_or_default(),
                active: fields[5] == "1",
                last: fields[6] == "1",
                bell: fields[7] == "1",
                activity: fields[8] == "1",
                silence: fields[9] == "1",
                command: fields[10].to_string(),
                badge: fields[11].to_string(),
                state: fields[12].to_string(),
                agent_counts: fields[13].to_string(),
            })
        })
        .collect()
}

pub fn list_windows(runner: &dyn TmuxRunner) -> Result<Vec<WindowInfo>> {
    let format = window_list_format();
    let output = runner.run(&["list-windows", "-a", "-F", &format])?;
    Ok(parse_windows(&output))
}

pub fn list_windows_for_target(runner: &dyn TmuxRunner, target: &str) -> Result<Vec<WindowInfo>> {
    let format = window_list_format();
    let output = runner.run(&["list-windows", "-t", target, "-F", &format])?;
    Ok(parse_windows(&output))
}

pub fn select_window(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    runner.run(&["select-window", "-t", target])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    #[test]
    fn parse_windows_reads_flags_and_defaults_empty_name() {
        let sep = FIELD_SEP.to_string();
        let output = [
            "main",
            "2",
            "@9",
            "",
            "3",
            "1",
            "0",
            "1",
            "0",
            "1",
            "nvim",
            "▲",
            "blocked",
            "{\"blocked\":1}",
        ]
        .join(&sep);
        let windows = parse_windows(&(output + "\n"));

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].session, "main");
        assert_eq!(windows[0].index, "2");
        assert_eq!(windows[0].id, "@9");
        assert_eq!(windows[0].name, "(unnamed)");
        assert_eq!(windows[0].panes, 3);
        assert!(windows[0].active);
        assert!(!windows[0].last);
        assert!(windows[0].bell);
        assert!(!windows[0].activity);
        assert!(windows[0].silence);
        assert_eq!(windows[0].command, "nvim");
        assert_eq!(windows[0].badge, "▲");
        assert_eq!(windows[0].state, "blocked");
        assert_eq!(windows[0].agent_counts, "{\"blocked\":1}");
    }

    #[test]
    fn parse_windows_ignores_malformed_and_missing_identity() {
        let sep = FIELD_SEP.to_string();
        let valid = [
            "main", "1", "@1", "zsh", "1", "0", "0", "0", "0", "0", "zsh", "", "", "",
        ]
        .join(&sep);
        let missing_session = [
            "", "1", "@2", "zsh", "1", "0", "0", "0", "0", "0", "zsh", "", "", "",
        ]
        .join(&sep);
        let missing_window = [
            "main", "1", "", "zsh", "1", "0", "0", "0", "0", "0", "zsh", "", "", "",
        ]
        .join(&sep);
        let output = format!("{valid}\n{missing_session}\n{missing_window}\nshort\n");

        let windows = parse_windows(&output);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "@1");
    }

    #[test]
    fn list_windows_for_target_uses_shared_format() {
        let mock = MockTmuxRunner::new();
        let format = window_list_format();
        mock.stub(&["list-windows", "-t", "=main:", "-F", &format], "");

        let windows = list_windows_for_target(&mock, "=main:").unwrap();

        assert!(windows.is_empty());
        assert_eq!(
            mock.calls(),
            vec![vec![
                "list-windows".to_string(),
                "-t".to_string(),
                "=main:".to_string(),
                "-F".to_string(),
                format,
            ]]
        );
    }
}

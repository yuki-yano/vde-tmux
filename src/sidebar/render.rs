use crate::hook::RollupLevel;
use crate::sidebar::state::SidebarState;
use crate::sidebar::tree::{SidebarRow, SidebarRowKind};

pub fn render_rows(rows: &[SidebarRow], state: &SidebarState, width: usize) -> String {
    if width <= 2 {
        return render_rail(rows);
    }
    rows.iter()
        .map(|row| render_row(row, state, width))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_row(row: &SidebarRow, state: &SidebarState, width: usize) -> String {
    let selected = if state.selection.as_deref() == Some(row.id.as_str()) {
        "> "
    } else {
        "  "
    };
    let indent = "  ".repeat(row.depth);
    let line = match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            let marker = if row.expanded { "v" } else { ">" };
            let git = row.git.as_ref().map(format_git_badge).unwrap_or_default();
            format!(
                "{selected}{indent}{marker} {}{} [{}:{}]",
                row.label,
                git,
                rollup_label(row.rollup),
                row.chat_count
            )
        }
        SidebarRowKind::Chat => {
            format!(
                "{selected}{indent}{} [{}]",
                row.label,
                rollup_label(row.rollup)
            )
        }
    };
    truncate_width(&line, width)
}

fn format_git_badge(badge: &crate::git::GitBadge) -> String {
    let mut parts = vec![badge.branch.clone()];
    if badge.ahead > 0 {
        parts.push(format!("+{}", badge.ahead));
    }
    if badge.behind > 0 {
        parts.push(format!("-{}", badge.behind));
    }
    format!(" {}", parts.join(" "))
}

fn render_rail(rows: &[SidebarRow]) -> String {
    rows.iter()
        .filter(|row| row.kind == SidebarRowKind::Chat)
        .map(|row| rollup_glyph(row.rollup).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn rollup_label(level: RollupLevel) -> &'static str {
    match level {
        RollupLevel::Error => "error",
        RollupLevel::Running => "running",
        RollupLevel::Permission => "permission",
        RollupLevel::Background => "background",
        RollupLevel::Waiting => "waiting",
        RollupLevel::Idle => "idle",
    }
}

fn rollup_glyph(level: RollupLevel) -> char {
    match level {
        RollupLevel::Error => 'E',
        RollupLevel::Running => 'R',
        RollupLevel::Permission => 'P',
        RollupLevel::Background => 'B',
        RollupLevel::Waiting => 'W',
        RollupLevel::Idle => 'I',
    }
}

fn truncate_width(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_string();
    }
    line.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::RollupLevel;
    use crate::sidebar::state::SidebarState;
    use crate::sidebar::tree::{SidebarRow, SidebarRowKind};

    fn row(
        id: &str,
        kind: SidebarRowKind,
        depth: usize,
        label: &str,
        rollup: RollupLevel,
    ) -> SidebarRow {
        SidebarRow {
            id: id.to_string(),
            kind,
            depth,
            label: label.to_string(),
            chat_count: 1,
            rollup,
            expanded: true,
            pane_id: None,
            git: None,
        }
    }

    #[test]
    fn render_rows_includes_selection_indentation_and_rollup() {
        let rows = vec![
            row(
                "repo::misc::app",
                SidebarRowKind::Repo,
                0,
                "app",
                RollupLevel::Running,
            ),
            row(
                "pane::%1",
                SidebarRowKind::Chat,
                1,
                "codex %1",
                RollupLevel::Running,
            ),
        ];
        let state = SidebarState {
            selection: Some("pane::%1".to_string()),
            ..SidebarState::default()
        };

        let rendered = render_rows(&rows, &state, 32);

        assert!(rendered.contains(" app [running:1]"));
        assert!(rendered.contains(">   codex %1 [running]"));
    }

    #[test]
    fn render_rows_uses_rail_for_narrow_width() {
        let rows = vec![row(
            "pane::%1",
            SidebarRowKind::Chat,
            0,
            "codex %1",
            RollupLevel::Permission,
        )];
        let rendered = render_rows(&rows, &SidebarState::default(), 2);
        assert_eq!(rendered, "P");
    }

    #[test]
    fn render_repo_row_includes_git_badge() {
        let mut repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Running,
        );
        repo.git = Some(crate::git::GitBadge {
            branch: "main".to_string(),
            ahead: 2,
            behind: 1,
        });

        let rendered = render_rows(&[repo], &SidebarState::default(), 80);

        assert!(rendered.contains("main +2 -1"));
    }

    #[test]
    fn render_repo_row_omits_zero_git_counts() {
        let mut repo = row(
            "repo::misc::app",
            SidebarRowKind::Repo,
            0,
            "app",
            RollupLevel::Idle,
        );
        repo.git = Some(crate::git::GitBadge {
            branch: "main".to_string(),
            ahead: 0,
            behind: 0,
        });

        let rendered = render_rows(&[repo], &SidebarState::default(), 80);

        assert!(rendered.contains("app main [idle:1]"));
        assert!(!rendered.contains("+0"));
        assert!(!rendered.contains("-0"));
    }
}

use crate::sidebar::state::ViewMode;
use crate::sidebar::tree::{SidebarRow, SidebarRowKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarInputAction {
    MoveNext,
    MovePrevious,
    Activate,
    ToggleExpand,
    SetViewMode(ViewMode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarCommand {
    JumpPane(String),
    ToggleExpand(String),
}

pub fn parse_key(key: &str) -> Option<SidebarInputAction> {
    match key {
        "j" | "down" => Some(SidebarInputAction::MoveNext),
        "k" | "up" => Some(SidebarInputAction::MovePrevious),
        "enter" | "\n" => Some(SidebarInputAction::Activate),
        "h" | "l" | "space" => Some(SidebarInputAction::ToggleExpand),
        "1" => Some(SidebarInputAction::SetViewMode(ViewMode::Flat)),
        "2" => Some(SidebarInputAction::SetViewMode(ViewMode::ByRepo)),
        "3" => Some(SidebarInputAction::SetViewMode(ViewMode::ByCategory)),
        _ => None,
    }
}

pub fn activate_selected(selection: Option<&str>, rows: &[SidebarRow]) -> Option<SidebarCommand> {
    let selection = selection?;
    let row = rows.iter().find(|row| row.id == selection)?;
    match row.kind {
        SidebarRowKind::Chat => row.pane_id.clone().map(SidebarCommand::JumpPane),
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            Some(SidebarCommand::ToggleExpand(row.id.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::state::ViewMode;
    use crate::sidebar::tree::{SidebarRow, SidebarRowKind};

    fn row(id: &str, kind: SidebarRowKind, pane_id: Option<&str>) -> SidebarRow {
        SidebarRow {
            id: id.to_string(),
            kind,
            depth: 0,
            label: id.to_string(),
            chat_count: 1,
            rollup: crate::hook::RollupLevel::Idle,
            expanded: true,
            pane_id: pane_id.map(ToOwned::to_owned),
            git: None,
        }
    }

    #[test]
    fn parse_key_maps_sidebar_actions() {
        assert_eq!(parse_key("j"), Some(SidebarInputAction::MoveNext));
        assert_eq!(parse_key("k"), Some(SidebarInputAction::MovePrevious));
        assert_eq!(parse_key("enter"), Some(SidebarInputAction::Activate));
        assert_eq!(
            parse_key("3"),
            Some(SidebarInputAction::SetViewMode(ViewMode::ByCategory))
        );
        assert_eq!(parse_key("unknown"), None);
    }

    #[test]
    fn activation_on_chat_row_requests_jump() {
        let rows = vec![
            row("repo::misc::app", SidebarRowKind::Repo, None),
            row("pane::%1", SidebarRowKind::Chat, Some("%1")),
        ];

        let command = activate_selected(Some("pane::%1"), &rows);

        assert_eq!(command, Some(SidebarCommand::JumpPane("%1".to_string())));
    }
}

use crate::sidebar::state::{StatusFilter, ViewMode};
use crate::sidebar::tree::{SidebarRow, SidebarRowKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarInputAction {
    MoveNext,
    MovePrevious,
    MoveFirst,
    MoveLast,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    Activate,
    ToggleExpand,
    SetViewMode(ViewMode),
    CycleViewMode,
    SetFilter(StatusFilter),
    CycleFilterForward,
    CycleFilterBackward,
    ToggleRow(String),
    FocusNextAttention,
    FocusPreviousAttention,
    ReorderUp,
    ReorderDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarCommand {
    JumpPane(String),
    ToggleExpand(String),
}

pub fn parse_key(key: &str) -> Option<SidebarInputAction> {
    if let Some(row_id) = key.strip_prefix("toggle:") {
        return Some(SidebarInputAction::ToggleRow(row_id.to_string()));
    }
    match key {
        "j" | "down" => Some(SidebarInputAction::MoveNext),
        "k" | "up" => Some(SidebarInputAction::MovePrevious),
        "gg" => Some(SidebarInputAction::MoveFirst),
        "G" => Some(SidebarInputAction::MoveLast),
        "C-d" => Some(SidebarInputAction::HalfPageDown),
        "C-u" => Some(SidebarInputAction::HalfPageUp),
        "C-f" => Some(SidebarInputAction::PageDown),
        "C-b" => Some(SidebarInputAction::PageUp),
        "enter" | "\n" => Some(SidebarInputAction::Activate),
        "space" => Some(SidebarInputAction::ToggleExpand),
        "v" => Some(SidebarInputAction::CycleViewMode),
        "tab" => Some(SidebarInputAction::CycleFilterForward),
        "backtab" => Some(SidebarInputAction::CycleFilterBackward),
        "n" => Some(SidebarInputAction::FocusNextAttention),
        "N" => Some(SidebarInputAction::FocusPreviousAttention),
        "J" => Some(SidebarInputAction::ReorderDown),
        "K" => Some(SidebarInputAction::ReorderUp),
        "1" => Some(SidebarInputAction::SetViewMode(ViewMode::Flat)),
        "2" => Some(SidebarInputAction::SetViewMode(ViewMode::ByRepo)),
        "3" => Some(SidebarInputAction::SetViewMode(ViewMode::ByCategory)),
        "all" => Some(SidebarInputAction::SetFilter(StatusFilter::All)),
        "attn" => Some(SidebarInputAction::SetFilter(StatusFilter::AttentionOnly)),
        "working" => Some(SidebarInputAction::SetFilter(StatusFilter::WorkingOnly)),
        "done" => Some(SidebarInputAction::SetFilter(StatusFilter::DoneOnly)),
        "idle" => Some(SidebarInputAction::SetFilter(StatusFilter::IdleOnly)),
        _ => None,
    }
}

pub fn activate_selected(selection: Option<&str>, rows: &[SidebarRow]) -> Option<SidebarCommand> {
    let selection = selection?;
    let row = rows.iter().find(|row| row.id == selection)?;
    match row.kind {
        SidebarRowKind::Chat | SidebarRowKind::Detail => {
            row.pane_id.clone().map(SidebarCommand::JumpPane)
        }
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            Some(SidebarCommand::ToggleExpand(row.id.clone()))
        }
        SidebarRowKind::Zone => None,
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
            badge_state: None,
            expanded: true,
            pane_id: pane_id.map(ToOwned::to_owned),
            git: None,
            active: false,
            meta: None,
        }
    }

    #[test]
    fn parse_key_maps_sidebar_actions() {
        assert_eq!(parse_key("j"), Some(SidebarInputAction::MoveNext));
        assert_eq!(parse_key("k"), Some(SidebarInputAction::MovePrevious));
        assert_eq!(parse_key("enter"), Some(SidebarInputAction::Activate));
        assert_eq!(parse_key("v"), Some(SidebarInputAction::CycleViewMode));
        assert_eq!(
            parse_key("tab"),
            Some(SidebarInputAction::CycleFilterForward)
        );
        assert_eq!(
            parse_key("backtab"),
            Some(SidebarInputAction::CycleFilterBackward)
        );
        assert_eq!(parse_key("J"), Some(SidebarInputAction::ReorderDown));
        assert_eq!(parse_key("K"), Some(SidebarInputAction::ReorderUp));
        assert_eq!(parse_key("gg"), Some(SidebarInputAction::MoveFirst));
        assert_eq!(parse_key("G"), Some(SidebarInputAction::MoveLast));
        assert_eq!(parse_key("C-d"), Some(SidebarInputAction::HalfPageDown));
        assert_eq!(parse_key("C-u"), Some(SidebarInputAction::HalfPageUp));
        assert_eq!(parse_key("C-f"), Some(SidebarInputAction::PageDown));
        assert_eq!(parse_key("C-b"), Some(SidebarInputAction::PageUp));
        assert_eq!(parse_key("h"), None);
        assert_eq!(parse_key("l"), None);
        assert_eq!(parse_key("right"), None);
        assert_eq!(parse_key("left"), None);
        assert_eq!(
            parse_key("toggle:chat::%1"),
            Some(SidebarInputAction::ToggleRow("chat::%1".to_string()))
        );
        assert_eq!(
            parse_key("3"),
            Some(SidebarInputAction::SetViewMode(ViewMode::ByCategory))
        );
        assert_eq!(parse_key("unknown"), None);
    }

    #[test]
    fn parse_key_maps_attention_navigation() {
        assert_eq!(parse_key("n"), Some(SidebarInputAction::FocusNextAttention));
        assert_eq!(
            parse_key("N"),
            Some(SidebarInputAction::FocusPreviousAttention)
        );
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

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    Flat,
    #[default]
    ByRepo,
    ByCategory,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SidebarState {
    pub version: u64,
    pub view_mode: ViewMode,
    pub selection: Option<String>,
    pub collapsed: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRowRef {
    pub id: String,
}

impl SidebarRowRef {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarAction {
    MoveNext,
    MovePrevious,
    ToggleExpand,
    SetViewMode(ViewMode),
}

impl SidebarState {
    pub fn apply(&mut self, action: SidebarAction, rows: &[SidebarRowRef]) -> bool {
        match action {
            SidebarAction::MoveNext => self.move_selection(rows, Direction::Next),
            SidebarAction::MovePrevious => self.move_selection(rows, Direction::Previous),
            SidebarAction::ToggleExpand => {
                let Some(id) = self.selection.clone() else {
                    return false;
                };
                if !self.collapsed.insert(id.clone()) {
                    self.collapsed.remove(&id);
                }
                self.bump();
                true
            }
            SidebarAction::SetViewMode(view_mode) => {
                if self.view_mode == view_mode {
                    return false;
                }
                self.view_mode = view_mode;
                self.bump();
                true
            }
        }
    }

    pub fn is_expanded(&self, id: &str) -> bool {
        !self.collapsed.contains(id)
    }

    fn move_selection(&mut self, rows: &[SidebarRowRef], direction: Direction) -> bool {
        if rows.is_empty() {
            return false;
        }
        let current = self
            .selection
            .as_ref()
            .and_then(|id| rows.iter().position(|row| &row.id == id));
        let next = match (current, direction) {
            (None, Direction::Next) => 0,
            (None, Direction::Previous) => rows.len() - 1,
            (Some(index), Direction::Next) => (index + 1).min(rows.len() - 1),
            (Some(index), Direction::Previous) => index.saturating_sub(1),
        };
        let next_id = rows[next].id.clone();
        if self.selection.as_deref() == Some(next_id.as_str()) {
            return false;
        }
        self.selection = Some(next_id);
        self.bump();
        true
    }

    fn bump(&mut self) {
        self.version += 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Next,
    Previous,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_updates_selection_and_expansion() {
        let rows = vec![
            SidebarRowRef::new("repo::app"),
            SidebarRowRef::new("pane::%1"),
            SidebarRowRef::new("pane::%2"),
        ];
        let mut state = SidebarState::default();

        state.apply(SidebarAction::MoveNext, &rows);
        assert_eq!(state.selection.as_deref(), Some("repo::app"));
        state.apply(SidebarAction::MoveNext, &rows);
        assert_eq!(state.selection.as_deref(), Some("pane::%1"));
        state.apply(SidebarAction::MovePrevious, &rows);
        assert_eq!(state.selection.as_deref(), Some("repo::app"));

        state.apply(SidebarAction::ToggleExpand, &rows);
        assert!(!state.is_expanded("repo::app"));
        assert_eq!(state.version, 4);
    }

    #[test]
    fn state_switches_view_mode() {
        let mut state = SidebarState::default();
        state.apply(SidebarAction::SetViewMode(ViewMode::ByCategory), &[]);
        assert_eq!(state.view_mode, ViewMode::ByCategory);
        assert_eq!(state.version, 1);
    }
}

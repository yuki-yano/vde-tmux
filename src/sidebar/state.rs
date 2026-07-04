use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoId {
    pub category: String,
    pub repo: String,
}

impl RepoId {
    pub fn new(category: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            category: category.into(),
            repo: repo.into(),
        }
    }

    pub fn from_row_id(id: &str) -> Option<Self> {
        let rest = id.strip_prefix("repo::")?;
        let (category, repo) = rest.split_once("::")?;
        Some(Self::new(category, repo))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    Flat,
    #[default]
    ByRepo,
    ByCategory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusFilter {
    #[default]
    All,
    AttentionOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SidebarState {
    pub version: u64,
    #[serde(default)]
    pub view_mode: ViewMode,
    #[serde(default)]
    pub filter: StatusFilter,
    #[serde(default)]
    pub selection: Option<String>,
    #[serde(default)]
    pub collapsed: BTreeSet<String>,
    #[serde(default)]
    pub manual_order: Vec<RepoId>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarAction {
    MoveNext,
    MovePrevious,
    ToggleExpand,
    Expand,
    Collapse,
    SetViewMode(ViewMode),
    CycleViewMode,
    ToggleFilter,
    ReorderUp(RepoId),
    ReorderDown(RepoId),
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
                self.toggle_expanded(&id)
            }
            SidebarAction::Expand => {
                let Some(id) = self.selection.clone() else {
                    return false;
                };
                self.set_expanded(&id, true)
            }
            SidebarAction::Collapse => {
                let Some(id) = self.selection.clone() else {
                    return false;
                };
                self.set_expanded(&id, false)
            }
            SidebarAction::SetViewMode(view_mode) => {
                if self.view_mode == view_mode {
                    return false;
                }
                self.view_mode = view_mode;
                self.bump();
                true
            }
            SidebarAction::CycleViewMode => {
                self.view_mode = self.view_mode.next();
                self.bump();
                true
            }
            SidebarAction::ToggleFilter => {
                self.filter = self.filter.next();
                self.bump();
                true
            }
            SidebarAction::ReorderUp(repo) => self.manual_move_up(&repo),
            SidebarAction::ReorderDown(repo) => self.manual_move_down(&repo),
        }
    }

    pub fn is_expanded(&self, id: &str) -> bool {
        self.is_expanded_with_default(id, true)
    }

    pub fn is_expanded_with_default(&self, id: &str, default_open: bool) -> bool {
        default_open ^ self.collapsed.contains(id)
    }

    pub fn toggle_expanded(&mut self, id: &str) -> bool {
        if !self.collapsed.insert(id.to_string()) {
            self.collapsed.remove(id);
        }
        self.bump();
        true
    }

    pub fn set_expanded(&mut self, id: &str, expanded: bool) -> bool {
        let default_open = !id.starts_with("chat::");
        if self.is_expanded_with_default(id, default_open) == expanded {
            return false;
        }
        self.toggle_expanded(id)
    }

    pub fn set_view_mode(&mut self, view_mode: ViewMode) -> bool {
        self.apply(SidebarAction::SetViewMode(view_mode), &[])
    }

    pub fn set_filter(&mut self, filter: StatusFilter) -> bool {
        if self.filter == filter {
            return false;
        }
        self.filter = filter;
        self.bump();
        true
    }

    pub fn manual_insert(&mut self, repo: RepoId, index: usize) -> bool {
        if self.manual_order.contains(&repo) {
            return false;
        }
        let index = index.min(self.manual_order.len());
        self.manual_order.insert(index, repo);
        self.bump();
        true
    }

    pub fn manual_move_up(&mut self, repo: &RepoId) -> bool {
        let Some(index) = self.manual_order.iter().position(|item| item == repo) else {
            return false;
        };
        if index == 0 {
            return false;
        }
        self.manual_order.swap(index, index - 1);
        self.bump();
        true
    }

    pub fn manual_move_down(&mut self, repo: &RepoId) -> bool {
        let Some(index) = self.manual_order.iter().position(|item| item == repo) else {
            return false;
        };
        if index + 1 >= self.manual_order.len() {
            return false;
        }
        self.manual_order.swap(index, index + 1);
        self.bump();
        true
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

impl ViewMode {
    pub fn next(self) -> Self {
        match self {
            ViewMode::Flat => ViewMode::ByRepo,
            ViewMode::ByRepo => ViewMode::ByCategory,
            ViewMode::ByCategory => ViewMode::Flat,
        }
    }
}

impl StatusFilter {
    pub fn next(self) -> Self {
        match self {
            StatusFilter::All => StatusFilter::AttentionOnly,
            StatusFilter::AttentionOnly => StatusFilter::All,
        }
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

    #[test]
    fn state_persists_filter_and_manual_order() {
        let state = SidebarState {
            filter: StatusFilter::AttentionOnly,
            manual_order: vec![RepoId::new("misc", "app")],
            ..SidebarState::default()
        };

        let json = serde_json::to_string(&state).unwrap();

        assert!(json.contains(r#""filter":"attention_only""#));
        assert!(json.contains(r#""manual_order""#));
    }

    #[test]
    fn state_cycles_view_mode_and_filter() {
        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            ..SidebarState::default()
        };

        assert!(state.apply(SidebarAction::CycleViewMode, &[]));
        assert_eq!(state.view_mode, ViewMode::ByRepo);
        assert!(state.apply(SidebarAction::CycleViewMode, &[]));
        assert_eq!(state.view_mode, ViewMode::ByCategory);
        assert!(state.apply(SidebarAction::CycleViewMode, &[]));
        assert_eq!(state.view_mode, ViewMode::Flat);

        assert!(state.apply(SidebarAction::ToggleFilter, &[]));
        assert_eq!(state.filter, StatusFilter::AttentionOnly);
        assert!(state.apply(SidebarAction::ToggleFilter, &[]));
        assert_eq!(state.filter, StatusFilter::All);
    }

    #[test]
    fn manual_reorder_moves_existing_repos_only() {
        let mut state = SidebarState::default();
        state.manual_insert(RepoId::new("misc", "a"), 0);
        state.manual_insert(RepoId::new("misc", "b"), 1);
        let version = state.version;

        assert!(state.manual_move_up(&RepoId::new("misc", "b")));
        assert_eq!(
            state.manual_order,
            vec![RepoId::new("misc", "b"), RepoId::new("misc", "a")]
        );
        assert_eq!(state.version, version + 1);
        assert!(!state.manual_move_up(&RepoId::new("misc", "b")));
        assert!(!state.manual_move_down(&RepoId::new("misc", "missing")));
    }
}

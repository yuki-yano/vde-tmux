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
    ByRepo,
    #[default]
    ByCategory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusFilter {
    #[default]
    All,
    AttentionOnly,
    WorkingOnly,
    DoneOnly,
    IdleOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarState {
    pub version: u64,
    pub view_mode: ViewMode,
    pub filter: StatusFilter,
    pub selection: Option<String>,
    pub collapsed: BTreeSet<String>,
    pub scroll: usize,
    pub return_target: Option<crate::pane_state::PaneInstance>,
}

pub const SIDEBAR_ORDER_SCHEMA_VERSION: u32 = 1;
pub const SIDEBAR_EXPANSION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarExpansionPreferences {
    pub schema_version: u32,
    pub version: u64,
    pub overrides: BTreeSet<String>,
}

impl Default for SidebarExpansionPreferences {
    fn default() -> Self {
        Self {
            schema_version: SIDEBAR_EXPANSION_SCHEMA_VERSION,
            version: 0,
            overrides: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarOrderPreferences {
    pub schema_version: u32,
    pub version: u64,
    #[serde(default)]
    pub manual_order: Vec<RepoId>,
    #[serde(default)]
    pub manual_chat_order: Vec<String>,
    #[serde(default)]
    pub view_mode: ViewMode,
    #[serde(default)]
    pub filter: StatusFilter,
}

impl Default for SidebarOrderPreferences {
    fn default() -> Self {
        Self {
            schema_version: SIDEBAR_ORDER_SCHEMA_VERSION,
            version: 0,
            manual_order: Vec::new(),
            manual_chat_order: Vec::new(),
            view_mode: ViewMode::default(),
            filter: StatusFilter::default(),
        }
    }
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
    SetViewMode(ViewMode),
    CycleViewMode,
    ToggleFilter,
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

    fn bump(&mut self) {
        self.version += 1;
    }
}

impl SidebarOrderPreferences {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SIDEBAR_ORDER_SCHEMA_VERSION {
            return Err(format!(
                "unsupported sidebar order schema version {}",
                self.schema_version
            ));
        }
        Ok(())
    }

    pub fn manual_insert(&mut self, repo: RepoId, index: usize) -> bool {
        if self.manual_order.contains(&repo) {
            return false;
        }
        let index = index.min(self.manual_order.len());
        self.manual_order.insert(index, repo);
        self.version += 1;
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
        self.version += 1;
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
        self.version += 1;
        true
    }

    pub fn manual_chat_insert(&mut self, pane_id: impl Into<String>, index: usize) -> bool {
        let pane_id = pane_id.into();
        if self.manual_chat_order.contains(&pane_id) {
            return false;
        }
        let index = index.min(self.manual_chat_order.len());
        self.manual_chat_order.insert(index, pane_id);
        self.version += 1;
        true
    }

    pub fn manual_chat_move_up(&mut self, pane_id: &str) -> bool {
        let Some(index) = self
            .manual_chat_order
            .iter()
            .position(|item| item == pane_id)
        else {
            return false;
        };
        if index == 0 {
            return false;
        }
        self.manual_chat_order.swap(index, index - 1);
        self.version += 1;
        true
    }

    pub fn manual_chat_move_down(&mut self, pane_id: &str) -> bool {
        let Some(index) = self
            .manual_chat_order
            .iter()
            .position(|item| item == pane_id)
        else {
            return false;
        };
        if index + 1 >= self.manual_chat_order.len() {
            return false;
        }
        self.manual_chat_order.swap(index, index + 1);
        self.version += 1;
        true
    }

    pub fn replace_manual_order(
        &mut self,
        expected_version: u64,
        manual_order: Vec<RepoId>,
        manual_chat_order: Vec<String>,
    ) -> Result<bool, u64> {
        if self.version != expected_version {
            return Err(self.version);
        }
        if self.manual_order == manual_order && self.manual_chat_order == manual_chat_order {
            return Ok(false);
        }
        let Some(next_version) = self.version.checked_add(1) else {
            return Err(self.version);
        };
        self.manual_order = manual_order;
        self.manual_chat_order = manual_chat_order;
        self.version = next_version;
        Ok(true)
    }

    pub fn replace_view_preferences(
        &mut self,
        expected_version: u64,
        view_mode: ViewMode,
        filter: StatusFilter,
    ) -> Result<bool, u64> {
        if self.version != expected_version {
            return Err(self.version);
        }
        if self.view_mode == view_mode && self.filter == filter {
            return Ok(false);
        }
        let Some(next_version) = self.version.checked_add(1) else {
            return Err(self.version);
        };
        self.view_mode = view_mode;
        self.filter = filter;
        self.version = next_version;
        Ok(true)
    }
}

impl SidebarExpansionPreferences {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SIDEBAR_EXPANSION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported sidebar expansion schema version {}",
                self.schema_version
            ));
        }
        if self.overrides.iter().any(|row_id| row_id.trim().is_empty()) {
            return Err("sidebar expansion row ID must not be empty".to_string());
        }
        Ok(())
    }

    pub fn set_override(
        &mut self,
        expected_version: u64,
        row_id: String,
        overridden: bool,
    ) -> Result<bool, u64> {
        if self.version != expected_version {
            return Err(self.version);
        }
        if self.overrides.contains(&row_id) == overridden {
            return Ok(false);
        }
        let Some(next_version) = self.version.checked_add(1) else {
            return Err(self.version);
        };
        if overridden {
            self.overrides.insert(row_id);
        } else {
            self.overrides.remove(&row_id);
        }
        self.version = next_version;
        Ok(true)
    }
}

impl SidebarState {
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
            StatusFilter::AttentionOnly => StatusFilter::WorkingOnly,
            StatusFilter::WorkingOnly => StatusFilter::DoneOnly,
            StatusFilter::DoneOnly => StatusFilter::IdleOnly,
            StatusFilter::IdleOnly => StatusFilter::All,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            StatusFilter::All => "all",
            StatusFilter::AttentionOnly => "attn",
            StatusFilter::WorkingOnly => "working",
            StatusFilter::DoneOnly => "done",
            StatusFilter::IdleOnly => "idle",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::AttentionOnly => "Needs action",
            StatusFilter::WorkingOnly => "Working",
            StatusFilter::DoneOnly => "Done",
            StatusFilter::IdleOnly => "Idle",
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
        assert_eq!(state.view_mode, ViewMode::ByCategory);
        state.apply(SidebarAction::SetViewMode(ViewMode::Flat), &[]);
        assert_eq!(state.view_mode, ViewMode::Flat);
        assert_eq!(state.version, 1);
    }

    #[test]
    fn preferences_serialize_view_and_filter_without_instance_local_state() {
        let state = SidebarOrderPreferences {
            manual_order: vec![RepoId::new("misc", "app")],
            view_mode: ViewMode::ByCategory,
            filter: StatusFilter::DoneOnly,
            ..SidebarOrderPreferences::default()
        };

        let json = serde_json::to_string(&state).unwrap();

        assert!(json.contains(r#""manual_order""#));
        assert!(json.contains(r#""view_mode":"by_category""#));
        assert!(json.contains(r#""filter":"done_only""#));
        assert!(!json.contains("selection"));
        assert!(!json.contains("collapsed"));
        assert!(!json.contains("scroll"));
        assert!(!json.contains("return_target"));
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
        assert_eq!(state.filter, StatusFilter::WorkingOnly);
        assert!(state.apply(SidebarAction::ToggleFilter, &[]));
        assert_eq!(state.filter, StatusFilter::DoneOnly);
        assert!(state.apply(SidebarAction::ToggleFilter, &[]));
        assert_eq!(state.filter, StatusFilter::IdleOnly);
        assert!(state.apply(SidebarAction::ToggleFilter, &[]));
        assert_eq!(state.filter, StatusFilter::All);
    }

    #[test]
    fn filter_cycles_through_all_states() {
        let mut filter = StatusFilter::All;
        let mut seen = Vec::new();

        for _ in 0..6 {
            seen.push(filter);
            filter = filter.next();
        }

        assert_eq!(
            seen,
            vec![
                StatusFilter::All,
                StatusFilter::AttentionOnly,
                StatusFilter::WorkingOnly,
                StatusFilter::DoneOnly,
                StatusFilter::IdleOnly,
                StatusFilter::All,
            ]
        );
    }

    #[test]
    fn status_filter_key_is_shared_by_filter_ui() {
        assert_eq!(StatusFilter::All.key(), "all");
        assert_eq!(StatusFilter::AttentionOnly.key(), "attn");
        assert_eq!(StatusFilter::WorkingOnly.key(), "working");
        assert_eq!(StatusFilter::DoneOnly.key(), "done");
        assert_eq!(StatusFilter::IdleOnly.key(), "idle");
    }

    #[test]
    fn manual_reorder_moves_existing_repos_only() {
        let mut state = SidebarOrderPreferences::default();
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

    #[test]
    fn manual_chat_reorder_moves_existing_chats_only() {
        let mut state = SidebarOrderPreferences::default();
        state.manual_chat_insert("%1", 0);
        state.manual_chat_insert("%2", 1);
        let version = state.version;

        assert!(state.manual_chat_move_up("%2"));
        assert_eq!(state.manual_chat_order, vec!["%2", "%1"]);
        assert_eq!(state.version, version + 1);
        assert!(!state.manual_chat_move_up("%2"));
        assert!(!state.manual_chat_move_down("%9"));
    }

    #[test]
    fn expansion_override_is_versioned_and_idempotent() {
        let mut state = SidebarExpansionPreferences::default();

        assert!(
            state
                .set_override(0, "repo::misc::app".to_string(), true)
                .unwrap()
        );
        assert_eq!(state.version, 1);
        assert!(
            !state
                .set_override(1, "repo::misc::app".to_string(), true)
                .unwrap()
        );
        assert!(
            state
                .set_override(1, "repo::misc::app".to_string(), false)
                .unwrap()
        );
        assert_eq!(state.version, 2);
        assert!(state.overrides.is_empty());
    }

    #[test]
    fn expansion_override_does_not_mutate_when_version_cannot_advance() {
        let mut state = SidebarExpansionPreferences {
            version: u64::MAX,
            ..SidebarExpansionPreferences::default()
        };

        assert_eq!(
            state.set_override(u64::MAX, "repo::misc::app".to_string(), true),
            Err(u64::MAX)
        );
        assert!(state.overrides.is_empty());
    }
}

use std::collections::{BTreeSet, VecDeque};

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

pub const SIDEBAR_PREFERENCES_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarPreferences {
    pub schema_version: u32,
    #[serde(default)]
    pub manual_order: Vec<RepoId>,
    #[serde(default)]
    pub manual_chat_order: Vec<String>,
    #[serde(default)]
    pub view_mode: ViewMode,
    #[serde(default)]
    pub filter: StatusFilter,
    #[serde(default)]
    pub expansion_overrides: BTreeSet<String>,
}

impl Default for SidebarPreferences {
    fn default() -> Self {
        Self {
            schema_version: SIDEBAR_PREFERENCES_SCHEMA_VERSION,
            manual_order: Vec::new(),
            manual_chat_order: Vec::new(),
            view_mode: ViewMode::default(),
            filter: StatusFilter::default(),
            expansion_overrides: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SidebarPreferenceIntent {
    MoveRepo {
        repo: RepoId,
        neighbor: RepoId,
        direction: MoveDirection,
    },
    MoveChat {
        pane_id: String,
        neighbor_pane_id: String,
        direction: MoveDirection,
    },
    SetDefaultViewMode {
        view_mode: ViewMode,
    },
    SetDefaultFilter {
        filter: StatusFilter,
    },
    SetExpanded {
        row_id: String,
        expanded: bool,
    },
}

#[derive(Debug, Default)]
pub struct SidebarIntentDedupe {
    order: VecDeque<crate::pane_state::EventId>,
    seen: BTreeSet<crate::pane_state::EventId>,
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
    CycleFilterForward,
    CycleFilterBackward,
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
            SidebarAction::CycleFilterForward => {
                self.filter = self.filter.next();
                self.bump();
                true
            }
            SidebarAction::CycleFilterBackward => {
                self.filter = self.filter.previous();
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

impl SidebarPreferences {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SIDEBAR_PREFERENCES_SCHEMA_VERSION {
            return Err(format!(
                "unsupported sidebar preferences schema version {}",
                self.schema_version
            ));
        }
        if self
            .expansion_overrides
            .iter()
            .any(|row_id| row_id.trim().is_empty())
        {
            return Err("sidebar expansion row ID must not be empty".to_string());
        }
        Ok(())
    }

    pub fn apply_intent(
        &mut self,
        intent: &SidebarPreferenceIntent,
        known_rows: &BTreeSet<String>,
    ) -> bool {
        match intent {
            SidebarPreferenceIntent::MoveRepo {
                repo,
                neighbor,
                direction,
            } => {
                let row = format!("repo::{}::{}", repo.category, repo.repo);
                let neighbor_row = format!("repo::{}::{}", neighbor.category, neighbor.repo);
                if !known_rows.contains(&row) || !known_rows.contains(&neighbor_row) {
                    return false;
                }
                move_relative(&mut self.manual_order, repo, neighbor, *direction)
            }
            SidebarPreferenceIntent::MoveChat {
                pane_id,
                neighbor_pane_id,
                direction,
            } => {
                if !known_rows
                    .iter()
                    .any(|row_id| chat_row_matches(row_id, pane_id))
                    || !known_rows
                        .iter()
                        .any(|row_id| chat_row_matches(row_id, neighbor_pane_id))
                {
                    return false;
                }
                move_relative(
                    &mut self.manual_chat_order,
                    pane_id,
                    neighbor_pane_id,
                    *direction,
                )
            }
            SidebarPreferenceIntent::SetDefaultViewMode { view_mode } => {
                if self.view_mode == *view_mode {
                    return false;
                }
                self.view_mode = *view_mode;
                true
            }
            SidebarPreferenceIntent::SetDefaultFilter { filter } => {
                if self.filter == *filter {
                    return false;
                }
                self.filter = *filter;
                true
            }
            SidebarPreferenceIntent::SetExpanded { row_id, expanded } => {
                if !known_rows.contains(row_id) {
                    return false;
                }
                let default_open = !row_id.starts_with("chat::");
                let overridden = default_open != *expanded;
                if self.expansion_overrides.contains(row_id) == overridden {
                    return false;
                }
                if overridden {
                    self.expansion_overrides.insert(row_id.clone());
                } else {
                    self.expansion_overrides.remove(row_id);
                }
                true
            }
        }
    }
}

fn chat_row_matches(row_id: &str, pane_id: &str) -> bool {
    row_id
        .strip_prefix("chat::")
        .and_then(|rest| rest.split_once("::"))
        .is_some_and(|(candidate, _)| candidate == pane_id)
}

fn move_relative<T: Clone + Eq>(
    order: &mut Vec<T>,
    item: &T,
    neighbor: &T,
    direction: MoveDirection,
) -> bool {
    if item == neighbor {
        return false;
    }
    let before = order.clone();
    order.retain(|entry| entry != item);
    if let Some(neighbor_index) = order.iter().position(|entry| entry == neighbor) {
        let insert_at = match direction {
            MoveDirection::Up => neighbor_index,
            MoveDirection::Down => neighbor_index + 1,
        };
        order.insert(insert_at, item.clone());
    } else {
        match direction {
            MoveDirection::Up => {
                order.push(item.clone());
                order.push(neighbor.clone());
            }
            MoveDirection::Down => {
                order.push(neighbor.clone());
                order.push(item.clone());
            }
        }
    }
    *order != before
}

impl SidebarIntentDedupe {
    const CAPACITY: usize = 256;

    pub fn accept(&mut self, event_id: crate::pane_state::EventId) -> bool {
        if !self.seen.insert(event_id.clone()) {
            return false;
        }
        self.order.push_back(event_id);
        if self.order.len() > Self::CAPACITY
            && let Some(expired) = self.order.pop_front()
        {
            self.seen.remove(&expired);
        }
        true
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

    pub fn previous(self) -> Self {
        match self {
            StatusFilter::All => StatusFilter::IdleOnly,
            StatusFilter::AttentionOnly => StatusFilter::All,
            StatusFilter::WorkingOnly => StatusFilter::AttentionOnly,
            StatusFilter::DoneOnly => StatusFilter::WorkingOnly,
            StatusFilter::IdleOnly => StatusFilter::DoneOnly,
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
        let state = SidebarPreferences {
            manual_order: vec![RepoId::new("misc", "app")],
            view_mode: ViewMode::ByCategory,
            filter: StatusFilter::DoneOnly,
            expansion_overrides: BTreeSet::from(["repo::misc::app".to_string()]),
            ..SidebarPreferences::default()
        };

        let json = serde_json::to_string(&state).unwrap();

        assert!(json.contains(r#""manual_order""#));
        assert!(json.contains(r#""view_mode":"by_category""#));
        assert!(json.contains(r#""filter":"done_only""#));
        assert!(!json.contains("selection"));
        assert!(json.contains("expansion_overrides"));
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

        assert!(state.apply(SidebarAction::CycleFilterForward, &[]));
        assert_eq!(state.filter, StatusFilter::AttentionOnly);
        assert!(state.apply(SidebarAction::CycleFilterForward, &[]));
        assert_eq!(state.filter, StatusFilter::WorkingOnly);
        assert!(state.apply(SidebarAction::CycleFilterForward, &[]));
        assert_eq!(state.filter, StatusFilter::DoneOnly);
        assert!(state.apply(SidebarAction::CycleFilterForward, &[]));
        assert_eq!(state.filter, StatusFilter::IdleOnly);
        assert!(state.apply(SidebarAction::CycleFilterForward, &[]));
        assert_eq!(state.filter, StatusFilter::All);

        assert!(state.apply(SidebarAction::CycleFilterBackward, &[]));
        assert_eq!(state.filter, StatusFilter::IdleOnly);
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
    fn filter_cycles_backward_through_all_states() {
        let mut filter = StatusFilter::All;
        let mut seen = Vec::new();

        for _ in 0..6 {
            seen.push(filter);
            filter = filter.previous();
        }

        assert_eq!(
            seen,
            vec![
                StatusFilter::All,
                StatusFilter::IdleOnly,
                StatusFilter::DoneOnly,
                StatusFilter::WorkingOnly,
                StatusFilter::AttentionOnly,
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
    fn preference_intent_reorders_known_repos_only() {
        let a = RepoId::new("misc", "a");
        let b = RepoId::new("misc", "b");
        let mut state = SidebarPreferences {
            manual_order: vec![a.clone(), b.clone()],
            ..SidebarPreferences::default()
        };
        let known = BTreeSet::from(["repo::misc::a".to_string(), "repo::misc::b".to_string()]);

        assert!(state.apply_intent(
            &SidebarPreferenceIntent::MoveRepo {
                repo: b.clone(),
                neighbor: a.clone(),
                direction: MoveDirection::Up,
            },
            &known,
        ));
        assert_eq!(
            state.manual_order,
            vec![RepoId::new("misc", "b"), RepoId::new("misc", "a")]
        );
        assert!(!state.apply_intent(
            &SidebarPreferenceIntent::MoveRepo {
                repo: RepoId::new("misc", "missing"),
                neighbor: a,
                direction: MoveDirection::Up,
            },
            &known,
        ));
    }

    #[test]
    fn preference_intent_reorder_preserves_prior_relative_changes_and_boundaries() {
        let a = RepoId::new("misc", "a");
        let b = RepoId::new("misc", "b");
        let c = RepoId::new("misc", "c");
        let known = BTreeSet::from([
            "repo::misc::a".to_string(),
            "repo::misc::b".to_string(),
            "repo::misc::c".to_string(),
        ]);
        let mut state = SidebarPreferences {
            manual_order: vec![a.clone(), b.clone(), c.clone()],
            ..SidebarPreferences::default()
        };

        assert!(state.apply_intent(
            &SidebarPreferenceIntent::MoveRepo {
                repo: c.clone(),
                neighbor: b.clone(),
                direction: MoveDirection::Up,
            },
            &known,
        ));
        assert!(state.apply_intent(
            &SidebarPreferenceIntent::MoveRepo {
                repo: a.clone(),
                neighbor: c.clone(),
                direction: MoveDirection::Down,
            },
            &known,
        ));
        assert_eq!(state.manual_order, vec![c, a, b]);
        let before = state.clone();
        assert!(!state.apply_intent(
            &SidebarPreferenceIntent::MoveRepo {
                repo: RepoId::new("misc", "a"),
                neighbor: RepoId::new("misc", "a"),
                direction: MoveDirection::Up,
            },
            &known,
        ));
        assert_eq!(state, before);
    }

    #[test]
    fn preference_intent_reorders_known_chats_only() {
        let mut state = SidebarPreferences {
            manual_chat_order: vec!["%1".to_string(), "%2".to_string()],
            ..SidebarPreferences::default()
        };
        let known = BTreeSet::from(["chat::%1::10".to_string(), "chat::%2::20".to_string()]);

        assert!(state.apply_intent(
            &SidebarPreferenceIntent::MoveChat {
                pane_id: "%2".to_string(),
                neighbor_pane_id: "%1".to_string(),
                direction: MoveDirection::Up,
            },
            &known,
        ));
        assert_eq!(state.manual_chat_order, vec!["%2", "%1"]);
    }

    #[test]
    fn absolute_expansion_intent_is_idempotent() {
        let mut state = SidebarPreferences::default();
        let known = BTreeSet::from(["repo::misc::app".to_string()]);
        let collapse = SidebarPreferenceIntent::SetExpanded {
            row_id: "repo::misc::app".to_string(),
            expanded: false,
        };

        assert!(state.apply_intent(&collapse, &known));
        assert!(!state.apply_intent(&collapse, &known));
        assert!(state.expansion_overrides.contains("repo::misc::app"));
    }

    #[test]
    fn absolute_expansion_intents_from_multiple_sidebars_converge() {
        let mut state = SidebarPreferences::default();
        let row_id = "chat::%1::10";
        let known = BTreeSet::from([row_id.to_string()]);

        assert!(state.apply_intent(
            &SidebarPreferenceIntent::SetExpanded {
                row_id: row_id.to_string(),
                expanded: true,
            },
            &known,
        ));
        assert!(!state.apply_intent(
            &SidebarPreferenceIntent::SetExpanded {
                row_id: row_id.to_string(),
                expanded: true,
            },
            &known,
        ));
        assert!(state.apply_intent(
            &SidebarPreferenceIntent::SetExpanded {
                row_id: row_id.to_string(),
                expanded: false,
            },
            &known,
        ));
        assert!(state.expansion_overrides.is_empty());
    }

    #[test]
    fn intent_dedupe_is_bounded_and_rejects_duplicates() {
        let mut dedupe = SidebarIntentDedupe::default();
        let event = crate::pane_state::EventId::parse("00000000000000000000000000000001").unwrap();

        assert!(dedupe.accept(event.clone()));
        assert!(!dedupe.accept(event));
        for sequence in 2..=258 {
            assert!(
                dedupe
                    .accept(crate::pane_state::EventId::parse(format!("{sequence:032x}")).unwrap())
            );
        }
        assert!(dedupe.accept(
            crate::pane_state::EventId::parse("00000000000000000000000000000001").unwrap()
        ));
    }
}

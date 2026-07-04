use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::category::resolve_category_for_session;
use crate::config::Config;
use crate::hook::{AgentStatus, RollupLevel, pane_rollup_level};
use crate::options::snapshot::PaneSnapshot;
use crate::session::SessionInfo;
use crate::sidebar::state::{SidebarRowRef, SidebarState, ViewMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarRowKind {
    Category,
    Repo,
    Chat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarRow {
    pub id: String,
    pub kind: SidebarRowKind,
    pub depth: usize,
    pub label: String,
    pub chat_count: usize,
    pub rollup: RollupLevel,
    pub expanded: bool,
    pub pane_id: Option<String>,
    pub git: Option<crate::git::GitBadge>,
}

#[derive(Debug, Clone)]
struct AgentPane {
    pane_id: String,
    repo: String,
    category: String,
    agent: String,
    rollup: RollupLevel,
    repo_path: String,
    attention: bool,
}

pub fn build_rows(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
) -> Vec<SidebarRow> {
    build_rows_with_git(config, panes, state, &BTreeMap::new())
}

pub fn build_rows_with_git(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
    git: &BTreeMap<String, crate::git::GitBadge>,
) -> Vec<SidebarRow> {
    let mut groups: BTreeMap<(String, String), Vec<AgentPane>> = BTreeMap::new();
    for pane in panes {
        if pane.is_sidebar || pane.agent.trim().is_empty() {
            continue;
        }
        let repo = repo_label(pane);
        let category = category_for_pane(config, pane, &repo);
        let rollup = rollup_for_pane(pane);
        groups
            .entry((category.clone(), repo.clone()))
            .or_default()
            .push(AgentPane {
                pane_id: pane.pane_id.clone(),
                repo,
                category,
                agent: pane.agent.clone(),
                rollup,
                repo_path: pane.current_path.clone(),
                attention: pane.attention == "1",
            });
    }
    for panes in groups.values_mut() {
        panes.sort_by(compare_agent_panes);
    }

    match state.view_mode {
        ViewMode::Flat => flat_rows(groups),
        ViewMode::ByRepo => repo_rows(groups, state, 0, git),
        ViewMode::ByCategory => category_rows(groups, state, git),
    }
}

pub fn row_refs(rows: &[SidebarRow]) -> Vec<SidebarRowRef> {
    rows.iter()
        .map(|row| SidebarRowRef::new(row.id.clone()))
        .collect()
}

fn category_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    git: &BTreeMap<String, crate::git::GitBadge>,
) -> Vec<SidebarRow> {
    let mut by_category: BTreeMap<String, BTreeMap<String, Vec<AgentPane>>> = BTreeMap::new();
    for ((category, repo), panes) in groups {
        by_category.entry(category).or_default().insert(repo, panes);
    }

    let mut rows = Vec::new();
    for (category, repos) in by_category {
        let category_id = format!("category::{category}");
        let all_panes = repos.values().flatten().cloned().collect::<Vec<_>>();
        let expanded = state.is_expanded(&category_id);
        rows.push(SidebarRow {
            id: category_id,
            kind: SidebarRowKind::Category,
            depth: 0,
            label: category,
            chat_count: all_panes.len(),
            rollup: rollup(&all_panes),
            expanded,
            pane_id: None,
            git: None,
        });
        if expanded {
            rows.extend(repo_rows_from_map(repos, state, 1, git));
        }
    }
    rows
}

fn repo_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    depth: usize,
    git: &BTreeMap<String, crate::git::GitBadge>,
) -> Vec<SidebarRow> {
    let mut repos = BTreeMap::new();
    for ((category, repo), panes) in groups {
        repos.insert(format!("{category}\u{1f}{repo}"), panes);
    }
    repo_rows_from_keyed_map(repos, state, depth, git)
}

fn repo_rows_from_map(
    repos: BTreeMap<String, Vec<AgentPane>>,
    state: &SidebarState,
    depth: usize,
    git: &BTreeMap<String, crate::git::GitBadge>,
) -> Vec<SidebarRow> {
    let keyed = repos
        .into_iter()
        .map(|(repo, panes)| {
            let category = panes
                .first()
                .map(|pane| pane.category.clone())
                .unwrap_or_else(|| "misc".to_string());
            (format!("{category}\u{1f}{repo}"), panes)
        })
        .collect();
    repo_rows_from_keyed_map(keyed, state, depth, git)
}

fn repo_rows_from_keyed_map(
    repos: BTreeMap<String, Vec<AgentPane>>,
    state: &SidebarState,
    depth: usize,
    git: &BTreeMap<String, crate::git::GitBadge>,
) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    for (_key, panes) in repos {
        let Some(first) = panes.first() else {
            continue;
        };
        let repo_id = repo_id(&first.category, &first.repo);
        let expanded = state.is_expanded(&repo_id);
        rows.push(SidebarRow {
            id: repo_id,
            kind: SidebarRowKind::Repo,
            depth,
            label: first.repo.clone(),
            chat_count: panes.len(),
            rollup: rollup(&panes),
            expanded,
            pane_id: None,
            git: git.get(&first.repo_path).cloned(),
        });
        if expanded {
            rows.extend(panes.iter().map(|pane| chat_row(pane, depth + 1)));
        }
    }
    rows
}

fn flat_rows(groups: BTreeMap<(String, String), Vec<AgentPane>>) -> Vec<SidebarRow> {
    groups
        .values()
        .flat_map(|panes| panes.iter().map(|pane| chat_row(pane, 0)))
        .collect()
}

fn chat_row(pane: &AgentPane, depth: usize) -> SidebarRow {
    SidebarRow {
        id: format!("pane::{}", pane.pane_id),
        kind: SidebarRowKind::Chat,
        depth,
        label: format!("{} {}", pane.agent, pane.pane_id),
        chat_count: 1,
        rollup: pane.rollup,
        expanded: true,
        pane_id: Some(pane.pane_id.clone()),
        git: None,
    }
}

fn compare_agent_panes(left: &AgentPane, right: &AgentPane) -> std::cmp::Ordering {
    right
        .attention
        .cmp(&left.attention)
        .then_with(|| left.rollup.cmp(&right.rollup))
        .then_with(|| left.pane_id.cmp(&right.pane_id))
}

fn repo_id(category: &str, repo: &str) -> String {
    format!("repo::{category}::{repo}")
}

fn rollup(panes: &[AgentPane]) -> RollupLevel {
    panes
        .iter()
        .map(|pane| pane.rollup)
        .min()
        .unwrap_or(RollupLevel::Idle)
}

fn category_for_pane(config: &Config, pane: &PaneSnapshot, repo: &str) -> String {
    let session = SessionInfo {
        name: pane.session.clone(),
        project_path: pane.current_path.clone(),
        ..SessionInfo::default()
    };
    let category = resolve_category_for_session(config, &session);
    if category.is_empty() {
        "misc".to_string()
    } else {
        category
    }
    .replace('\u{1f}', " ")
    .trim()
    .to_string()
    .if_empty("misc")
    .unwrap_or_else(|| repo.to_string())
}

fn repo_label(pane: &PaneSnapshot) -> String {
    let path = pane.current_path.trim_end_matches('/');
    let repo = path
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(&pane.session);
    let repo = repo.trim();
    if repo.is_empty() {
        "repo".to_string()
    } else {
        repo.replace('\u{1f}', " ")
    }
}

fn rollup_for_pane(pane: &PaneSnapshot) -> RollupLevel {
    pane_rollup_level(parse_status(&pane.status), non_empty(&pane.wait_reason))
}

fn parse_status(raw: &str) -> Option<AgentStatus> {
    match raw {
        "running" => Some(AgentStatus::Running),
        "waiting" => Some(AgentStatus::Waiting),
        "idle" => Some(AgentStatus::Idle),
        "error" => Some(AgentStatus::Error),
        _ => None,
    }
}

fn non_empty(raw: &str) -> Option<&str> {
    (!raw.trim().is_empty()).then(|| raw.trim())
}

trait EmptyStringExt {
    fn if_empty(self, value: &str) -> Option<String>;
}

impl EmptyStringExt for String {
    fn if_empty(self, value: &str) -> Option<String> {
        if self.is_empty() {
            Some(value.to_string())
        } else {
            Some(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CategoryRule, Config};
    use crate::hook::RollupLevel;
    use crate::options::snapshot::PaneSnapshot;
    use crate::sidebar::state::{SidebarState, ViewMode};

    fn pane(session: &str, pane_id: &str, path: &str, agent: &str, status: &str) -> PaneSnapshot {
        PaneSnapshot {
            session: session.to_string(),
            pane_id: pane_id.to_string(),
            current_path: path.to_string(),
            agent: agent.to_string(),
            status: status.to_string(),
            ..PaneSnapshot::default()
        }
    }

    fn category_rule(category: &str, pattern: &str) -> CategoryRule {
        CategoryRule {
            category: category.to_string(),
            ghq_patterns: vec![pattern.to_string()],
        }
    }

    #[test]
    fn empty_panes_render_no_rows() {
        let rows = build_rows(
            &Config::default(),
            &[],
            &SidebarState {
                view_mode: ViewMode::ByCategory,
                ..SidebarState::default()
            },
        );

        assert!(rows.is_empty());
    }

    #[test]
    fn build_rows_excludes_sidebar_and_non_agent_panes() {
        let mut sidebar = pane("main", "%9", "/tmp/sidebar", "codex", "running");
        sidebar.is_sidebar = true;
        let rows = build_rows(
            &Config::default(),
            &[
                sidebar,
                pane("shell", "%2", "/tmp/shell", "", ""),
                pane("main", "%1", "/tmp/app", "codex", "running"),
            ],
            &SidebarState::default(),
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "app");
        assert_eq!(rows[1].pane_id.as_deref(), Some("%1"));
    }

    #[test]
    fn build_rows_groups_agent_panes_by_category_and_repo() {
        let mut config = Config::default();
        config.categories.default_category = Some("misc".to_string());
        config
            .categories
            .rules
            .push(category_rule("work", "github.com/acme/*"));
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            ..SidebarState::default()
        };

        let rows = build_rows(
            &config,
            &[
                pane("main", "%1", "/ghq/github.com/acme/app", "codex", "running"),
                pane(
                    "main",
                    "%2",
                    "/ghq/github.com/acme/app",
                    "claude",
                    "waiting",
                ),
                pane("shell", "%3", "/tmp", "", ""),
            ],
            &state,
        );

        assert_eq!(rows[0].kind, SidebarRowKind::Category);
        assert_eq!(rows[0].label, "work");
        assert_eq!(rows[0].chat_count, 2);
        assert_eq!(rows[0].rollup, RollupLevel::Running);
        assert_eq!(rows[1].kind, SidebarRowKind::Repo);
        assert_eq!(rows[1].label, "app");
        assert_eq!(rows[2].kind, SidebarRowKind::Chat);
        assert_eq!(rows[2].pane_id.as_deref(), Some("%1"));
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn by_category_rows_keep_same_repo_name_in_distinct_categories() {
        let mut config = Config::default();
        config
            .categories
            .rules
            .push(category_rule("alpha", "github.com/acme/*"));
        config
            .categories
            .rules
            .push(category_rule("beta", "github.com/other/*"));
        let state = SidebarState {
            view_mode: ViewMode::ByCategory,
            ..SidebarState::default()
        };

        let rows = build_rows(
            &config,
            &[
                pane("a", "%1", "/ghq/github.com/acme/app", "codex", "running"),
                pane("b", "%2", "/ghq/github.com/other/app", "claude", "idle"),
            ],
            &state,
        );

        let repo_rows = rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Repo)
            .collect::<Vec<_>>();
        assert_eq!(repo_rows.len(), 2);
        assert!(repo_rows.iter().all(|row| row.label == "app"));
        assert_eq!(repo_rows[0].id, "repo::alpha::app");
        assert_eq!(repo_rows[1].id, "repo::beta::app");
    }

    #[test]
    fn by_repo_rows_are_sorted_by_category_then_repo() {
        let mut config = Config::default();
        config
            .categories
            .rules
            .push(category_rule("work", "github.com/work/*"));
        config
            .categories
            .rules
            .push(category_rule("oss", "github.com/oss/*"));
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };

        let rows = build_rows(
            &config,
            &[
                pane("work", "%2", "/ghq/github.com/work/zeta", "codex", "idle"),
                pane("oss", "%1", "/ghq/github.com/oss/alpha", "codex", "idle"),
            ],
            &state,
        );

        let repo_labels = rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Repo)
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(repo_labels, vec!["alpha", "zeta"]);
    }

    #[test]
    fn flat_view_contains_only_chat_rows() {
        let state = SidebarState {
            view_mode: ViewMode::Flat,
            ..SidebarState::default()
        };

        let rows = build_rows(
            &Config::default(),
            &[
                pane("main", "%1", "/tmp/app", "codex", "running"),
                pane("main", "%2", "/tmp/app", "claude", "idle"),
            ],
            &state,
        );

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.kind == SidebarRowKind::Chat));
        assert_eq!(rows[0].pane_id.as_deref(), Some("%1"));
        assert_eq!(rows[1].pane_id.as_deref(), Some("%2"));
    }

    #[test]
    fn collapsed_repo_hides_chat_rows() {
        let mut state = SidebarState::default();
        state.collapsed.insert("repo::misc::app".to_string());
        let rows = build_rows(
            &Config::default(),
            &[pane("main", "%1", "/tmp/app", "codex", "running")],
            &state,
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, SidebarRowKind::Repo);
        assert_eq!(rows[0].chat_count, 1);
    }

    #[test]
    fn collapsed_category_hides_repo_rows() {
        let mut state = SidebarState {
            view_mode: ViewMode::ByCategory,
            ..SidebarState::default()
        };
        state.collapsed.insert("category::misc".to_string());

        let rows = build_rows(
            &Config::default(),
            &[pane("main", "%1", "/tmp/app", "codex", "running")],
            &state,
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, SidebarRowKind::Category);
        assert_eq!(rows[0].chat_count, 1);
    }

    #[test]
    fn unknown_status_rolls_up_to_background() {
        let rows = build_rows(
            &Config::default(),
            &[pane("main", "%1", "/tmp/app", "codex", "unknown")],
            &SidebarState::default(),
        );

        assert_eq!(rows[0].rollup, RollupLevel::Background);
    }

    #[test]
    fn repo_row_includes_git_badge_when_available() {
        let mut git = std::collections::BTreeMap::new();
        git.insert(
            "/tmp/app".to_string(),
            crate::git::GitBadge {
                branch: "main".to_string(),
                ahead: 2,
                behind: 1,
            },
        );
        let rows = build_rows_with_git(
            &Config::default(),
            &[pane("main", "%1", "/tmp/app", "codex", "running")],
            &SidebarState::default(),
            &git,
        );

        assert_eq!(rows[0].git.as_ref().unwrap().branch, "main");
    }

    #[test]
    fn repo_row_has_no_git_badge_when_path_is_absent_from_cache() {
        let rows = build_rows_with_git(
            &Config::default(),
            &[pane("main", "%1", "/tmp/app", "codex", "running")],
            &SidebarState::default(),
            &std::collections::BTreeMap::new(),
        );

        assert!(rows[0].git.is_none());
    }

    #[test]
    fn attention_panes_sort_before_background_panes() {
        let mut quiet = pane("main", "%1", "/tmp/app", "codex", "idle");
        quiet.attention = "0".to_string();
        let mut attention = pane("main", "%2", "/tmp/app", "claude", "idle");
        attention.attention = "1".to_string();
        let state = SidebarState {
            view_mode: ViewMode::Flat,
            ..SidebarState::default()
        };

        let rows = build_rows(&Config::default(), &[quiet, attention], &state);

        assert_eq!(rows[0].pane_id.as_deref(), Some("%2"));
        assert_eq!(rows[1].pane_id.as_deref(), Some("%1"));
    }
}

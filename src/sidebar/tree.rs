use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::agent::display_agent_name;
use crate::config::Config;
use crate::daemon::session_badge::BadgeState;
use crate::git::WorktreeInfo;
use crate::hook::{RollupLevel, TaskItem, TaskItemStatus, WorktreeActivity};
use crate::pane_state::PaneInstance;
use crate::sidebar::state::{
    CategoryScope, PresentationMode, SidebarPreferences, SidebarRowRef, SidebarState, StatusFilter,
};

pub(crate) const PRIORITY_PINNED_ZONE_ID: &str = "zone::priority::pinned";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarRowKind {
    Zone,
    Category,
    Repo,
    Chat,
    Detail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarRow {
    pub id: String,
    pub kind: SidebarRowKind,
    pub depth: usize,
    pub label: String,
    pub chat_count: usize,
    pub rollup: RollupLevel,
    pub badge_state: Option<BadgeState>,
    pub expanded: bool,
    pub pane_id: Option<String>,
    pub git: Option<crate::git::GitBadge>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub meta: Option<RowMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RowMeta {
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub task_summary: Option<String>,
    pub wait_reason: Option<String>,
    pub elapsed_secs: Option<i64>,
    pub completed_age_secs: Option<i64>,
    pub tasks_done: Option<i64>,
    pub tasks_total: Option<i64>,
    pub subagent_count: Option<usize>,
    pub attention_count: Option<usize>,
    pub origin: Option<String>,
    pub flash: Option<bool>,
    pub is_unread: bool,
    pub pinned: bool,
    pub latest_unread_order: Option<u64>,
    pub latest_unread_reason: Option<crate::pane_state::UnreadReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BadgeCounts {
    pub total: usize,
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
}

impl BadgeCounts {
    pub fn count_for_filter(self, filter: StatusFilter) -> usize {
        match filter {
            StatusFilter::All => self.total,
            StatusFilter::AttentionOnly => self.blocked,
            StatusFilter::WorkingOnly => self.working,
            StatusFilter::DoneOnly => self.done,
            StatusFilter::IdleOnly => self.idle,
        }
    }

    pub fn filter_is_available(self, filter: StatusFilter) -> bool {
        filter == StatusFilter::All || self.count_for_filter(filter) > 0
    }
}

#[derive(Debug, Clone)]
struct AgentPane {
    pane_instance: PaneInstance,
    pane_id: String,
    repo: String,
    repo_key: crate::category::RepoKey,
    category: String,
    agent: String,
    prompt: String,
    latest_response: String,
    task_summary: String,
    wait_reason: String,
    started_at: String,
    completed_at: String,
    tasks: String,
    task_items: Vec<TaskItem>,
    subagents: Vec<SubagentDetail>,
    worktree_activity: Option<WorktreeActivity>,
    background_process: Option<crate::pane_state::BackgroundProcessState>,
    listening_ports: Vec<u16>,
    worktree: Option<WorktreeInfo>,
    git: Option<crate::git::GitBadge>,
    rollup: RollupLevel,
    badge_state: BadgeState,
    repo_path: String,
    flash: bool,
    active: bool,
    is_unread: bool,
    pinned: bool,
    latest_unread_order: Option<u64>,
    latest_unread_reason: Option<crate::pane_state::UnreadReason>,
}

#[derive(Debug, Clone, Default)]
pub struct RowBuildContext {
    pub git: BTreeMap<String, crate::git::GitBadge>,
    pub worktrees: BTreeMap<String, crate::git::WorktreeInfo>,
    pub triage: BTreeSet<PaneInstance>,
    pub flash: BTreeSet<PaneInstance>,
    pub active_sessions: BTreeSet<String>,
    pub active_categories: BTreeSet<String>,
    pub category_state: crate::category::CategoryState,
    pub categories: crate::category::EffectiveCategoryModel,
    pub repo_identities: BTreeMap<String, crate::category::RepoIdentity>,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarProjection {
    pub rows: Vec<SidebarRow>,
    pub counts: BadgeCounts,
}

pub fn project_sidebar(
    config: &Config,
    panes: &[crate::daemon::protocol::v2::PanePresentation],
    model: &crate::daemon::SidebarModel,
    state: &SidebarState,
    now: i64,
) -> SidebarProjection {
    let context = RowBuildContext {
        git: model.git.clone(),
        worktrees: model.worktrees.clone(),
        triage: model.needs_action.clone(),
        flash: model.flashing.clone(),
        active_sessions: model.active_sessions.clone(),
        active_categories: model.active_categories.clone(),
        category_state: model.category_state.clone(),
        categories: model.categories.clone(),
        repo_identities: model.repo_identities.clone(),
        now,
    };
    let (rows, counts) =
        build_rows_from_presentations(config, panes, state, &model.preferences, &context);
    SidebarProjection { rows, counts }
}

pub fn build_rows_from_presentations(
    _config: &Config,
    panes: &[crate::daemon::protocol::v2::PanePresentation],
    state: &SidebarState,
    order: &SidebarPreferences,
    ctx: &RowBuildContext,
) -> (Vec<SidebarRow>, BadgeCounts) {
    let mut groups: BTreeMap<(String, String), Vec<AgentPane>> = BTreeMap::new();
    for pane in panes {
        let Some(resolved) = pane.resolved.as_ref() else {
            continue;
        };
        let canonical = &resolved.canonical;
        let session_name = pane
            .session_links
            .first()
            .map(|link| link.session_name.as_str())
            .unwrap_or("repo");
        let fallback_repo = repo_label_from_values(&pane.current_path, session_name);
        let identity = ctx.repo_identities.get(&pane.current_path);
        let repo = identity
            .map(|identity| identity.display_name.clone())
            .unwrap_or(fallback_repo);
        let repo_key = identity
            .map(|identity| identity.key.clone())
            .unwrap_or_else(|| crate::category::RepoKey::path(&pane.current_path));
        let category = ctx
            .categories
            .placements
            .get(&repo_key)
            .map(|placement| placement.category.to_string())
            .unwrap_or_else(|| crate::category::UNCATEGORIZED.to_string());
        let (rollup, wait_reason) = match &canonical.lifecycle {
            crate::pane_state::LifecycleState::Idle => (RollupLevel::Idle, String::new()),
            crate::pane_state::LifecycleState::Running => (RollupLevel::Running, String::new()),
            crate::pane_state::LifecycleState::Waiting { reason } => match reason {
                crate::pane_state::WaitReason::PermissionPrompt => {
                    (RollupLevel::Permission, "permission_prompt".to_string())
                }
                crate::pane_state::WaitReason::Other(reason) => {
                    (RollupLevel::Waiting, reason.clone())
                }
            },
            crate::pane_state::LifecycleState::Error { reason } => {
                (RollupLevel::Error, reason.clone().unwrap_or_default())
            }
        };
        let task_items = canonical
            .tasks
            .items
            .iter()
            .map(|item| TaskItem {
                step: item.step.clone(),
                status: match item.status {
                    crate::pane_state::TaskItemStatus::Pending => TaskItemStatus::Pending,
                    crate::pane_state::TaskItemStatus::InProgress => TaskItemStatus::InProgress,
                    crate::pane_state::TaskItemStatus::Completed => TaskItemStatus::Completed,
                },
            })
            .collect::<Vec<_>>();
        let subagents = canonical
            .subagents
            .iter()
            .map(|subagent| SubagentDetail {
                agent_id: subagent.agent_id.clone(),
                agent_type: subagent.agent_type.clone(),
                display_name: subagent.display_name.clone(),
            })
            .collect::<Vec<_>>();
        let worktree_activity =
            canonical
                .worktree_activity
                .as_ref()
                .map(|activity| crate::hook::WorktreeActivity {
                    kind: crate::hook::WorktreeActivityKind::VwExec,
                    name: activity.name.clone(),
                    path: activity.path.clone(),
                    command: activity.command.clone(),
                    observed_at: activity.observed_at,
                });
        groups
            .entry((category.clone(), repo_key.to_string()))
            .or_default()
            .push(AgentPane {
                pane_instance: pane.pane_instance.clone(),
                pane_id: pane.pane_instance.pane_id.clone(),
                repo,
                repo_key,
                category,
                agent: canonical.agent.as_str().to_string(),
                prompt: canonical
                    .prompt
                    .as_ref()
                    .map(|prompt| prompt.text.clone())
                    .unwrap_or_default(),
                latest_response: canonical
                    .latest_response
                    .as_ref()
                    .map(|response| response.text.clone())
                    .unwrap_or_default(),
                task_summary: canonical
                    .task_context
                    .summary
                    .as_ref()
                    .and_then(|summary| summary.text.clone())
                    .unwrap_or_default(),
                wait_reason,
                started_at: canonical
                    .started_at
                    .map_or_else(String::new, |value| value.to_string()),
                completed_at: canonical
                    .completed_at
                    .map_or_else(String::new, |value| value.to_string()),
                tasks: format!(
                    "{}/{}",
                    canonical.tasks.progress.done, canonical.tasks.progress.total
                ),
                task_items,
                subagents,
                worktree_activity,
                background_process: canonical.background_process.clone(),
                listening_ports: canonical.listening_ports.clone(),
                worktree: ctx.worktrees.get(&pane.current_path).cloned(),
                git: ctx.git.get(&pane.current_path).cloned(),
                rollup,
                badge_state: resolved.badge,
                repo_path: pane.current_path.clone(),
                flash: ctx.flash.contains(&pane.pane_instance),
                active: pane
                    .session_links
                    .iter()
                    .any(|link| ctx.active_sessions.contains(&link.session_id)),
                is_unread: canonical.unread.is_unread(),
                pinned: order.pinned_panes.contains(&pane.pane_instance),
                latest_unread_order: canonical
                    .unread
                    .latest_unread()
                    .map(|occurrence| occurrence.order),
                latest_unread_reason: canonical
                    .unread
                    .latest_unread()
                    .map(|occurrence| occurrence.reason),
            });
    }
    build_rows_from_groups(groups, state, order, ctx)
}

fn build_rows_from_groups(
    mut groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    order: &SidebarPreferences,
    ctx: &RowBuildContext,
) -> (Vec<SidebarRow>, BadgeCounts) {
    if state.category_scope == CategoryScope::Current {
        groups.retain(|(category, _), _| state.current_category.as_ref() == Some(category));
    }
    for panes in groups.values_mut() {
        order_agent_panes(panes, order);
    }
    let counts = badge_counts_from_agent_panes(groups.values().flat_map(|panes| panes.iter()));
    if state.presentation_mode == PresentationMode::Priority {
        for panes in groups.values_mut() {
            panes.retain(|pane| pane_matches_filter(pane, state.filter));
        }
        groups.retain(|_, panes| !panes.is_empty());
        return (priority_rows(groups, state, order, ctx.now), counts);
    }
    let group_metas = groups
        .iter()
        .map(|(key, panes)| (key.clone(), group_meta(panes)))
        .collect::<BTreeMap<_, _>>();
    let mut triage_panes = Vec::new();
    for panes in groups.values_mut() {
        let mut index = 0;
        while index < panes.len() {
            if ctx.triage.contains(&panes[index].pane_instance) && !panes[index].pinned {
                triage_panes.push(panes.remove(index));
            } else {
                index += 1;
            }
        }
    }
    order_agent_panes(&mut triage_panes, order);
    for panes in groups.values_mut() {
        panes.retain(|pane| pane_matches_filter(pane, state.filter));
    }
    groups.retain(|_, panes| !panes.is_empty());

    let mut rows = triage_zone_rows(&triage_panes, state, ctx.now);
    let mut fleet_rows = match state.presentation_mode {
        PresentationMode::Flat => flat_rows(
            groups,
            state,
            order,
            ctx.now,
            state.category_scope == CategoryScope::All,
        ),
        PresentationMode::Tree if state.category_scope == CategoryScope::Current => repo_rows(
            groups,
            state,
            &ctx.category_state,
            0,
            &ctx.git,
            ctx.now,
            &group_metas,
        ),
        PresentationMode::Tree => category_rows(groups, state, ctx, &group_metas),
        PresentationMode::Priority => {
            unreachable!("priority returns before structural projection")
        }
    };
    rows.append(&mut fleet_rows);
    (rows, counts)
}

fn badge_counts_from_agent_panes<'a>(
    panes: impl IntoIterator<Item = &'a AgentPane>,
) -> BadgeCounts {
    let mut counts = BadgeCounts::default();
    for pane in panes {
        counts.total += 1;
        match pane.badge_state {
            BadgeState::Blocked => counts.blocked += 1,
            BadgeState::Working => counts.working += 1,
            BadgeState::Done => counts.done += 1,
            BadgeState::Idle => counts.idle += 1,
        }
    }
    counts
}

pub fn row_refs(rows: &[SidebarRow]) -> Vec<SidebarRowRef> {
    rows.iter()
        .filter(|row| !matches!(row.kind, SidebarRowKind::Detail | SidebarRowKind::Zone))
        .map(|row| SidebarRowRef::new(row.id.clone()))
        .collect()
}

pub(crate) fn chat_row_id(pane: &PaneInstance) -> String {
    format!("chat::{}::{}", pane.pane_id, pane.pane_pid)
}

pub(crate) fn pane_instance_from_row_id(id: &str) -> Option<PaneInstance> {
    let rest = id
        .strip_prefix("chat::")
        .or_else(|| id.strip_prefix("detail::"))?;
    let mut fields = rest.split("::");
    let pane_id = fields.next()?.to_string();
    let pane_pid = fields.next()?.parse().ok()?;
    let pane = PaneInstance { pane_id, pane_pid };
    pane.validate().ok()?;
    Some(pane)
}

fn category_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    ctx: &RowBuildContext,
    metas: &BTreeMap<(String, String), RowMeta>,
) -> Vec<SidebarRow> {
    let mut by_category: BTreeMap<String, BTreeMap<String, Vec<AgentPane>>> = BTreeMap::new();
    for ((category, repo), panes) in groups {
        by_category.entry(category).or_default().insert(repo, panes);
    }
    if state.filter == StatusFilter::All {
        for category in &ctx.categories.categories {
            by_category.entry(category.name.to_string()).or_default();
        }
    }

    let mut rows = Vec::new();
    let mut categories = by_category.into_iter().collect::<Vec<_>>();
    categories.sort_by(|(left_name, left_repos), (right_name, right_repos)| {
        let has_pin = |repos: &BTreeMap<String, Vec<AgentPane>>| {
            repos.values().flatten().any(|pane| pane.pinned)
        };
        has_pin(right_repos)
            .cmp(&has_pin(left_repos))
            .then_with(|| {
                let position = |name: &String| {
                    ctx.categories
                        .categories
                        .iter()
                        .position(|category| category.name.as_str() == name)
                        .unwrap_or(usize::MAX)
                };
                position(left_name).cmp(&position(right_name))
            })
    });
    for (category, repos) in categories {
        let category_id = format!("category::{category}");
        let all_panes = repos.values().flatten().cloned().collect::<Vec<_>>();
        let active =
            ctx.active_categories.contains(&category) || all_panes.iter().any(|pane| pane.active);
        let attention_count = repos
            .keys()
            .filter_map(|repo| {
                metas
                    .get(&(category.clone(), repo.clone()))
                    .and_then(|meta| meta.attention_count)
            })
            .sum();
        let expanded = state.is_expanded(&category_id);
        rows.push(SidebarRow {
            id: category_id,
            kind: SidebarRowKind::Category,
            depth: 0,
            label: category.clone(),
            chat_count: all_panes.len(),
            rollup: rollup(&all_panes),
            badge_state: badge_rollup(&all_panes),
            expanded,
            pane_id: None,
            git: None,
            active,
            meta: Some(RowMeta {
                attention_count: Some(attention_count),
                pinned: all_panes.iter().any(|pane| pane.pinned),
                ..RowMeta::default()
            }),
        });
        if expanded {
            let repo_rows = repo_rows_from_map(
                repos,
                state,
                &ctx.category_state,
                1,
                &ctx.git,
                ctx.now,
                metas,
            );
            rows.extend(merge_category_repository_rows(
                &category, repo_rows, state, ctx,
            ));
        }
    }
    rows
}

fn merge_category_repository_rows(
    category: &str,
    rows: Vec<SidebarRow>,
    state: &SidebarState,
    ctx: &RowBuildContext,
) -> Vec<SidebarRow> {
    if state.filter != StatusFilter::All {
        return rows;
    }
    let Some(category_model) = ctx
        .categories
        .categories
        .iter()
        .find(|candidate| candidate.name.as_str() == category)
    else {
        return rows;
    };

    let mut chunks = Vec::<(String, Vec<SidebarRow>)>::new();
    for row in rows {
        if row.kind == SidebarRowKind::Repo {
            chunks.push((row.id.clone(), vec![row]));
        } else if let Some((_, chunk)) = chunks.last_mut() {
            chunk.push(row);
        }
    }

    let mut merged = Vec::new();
    let mut index = 0;
    while index < chunks.len() {
        let pinned = chunks[index]
            .1
            .first()
            .and_then(|row| row.meta.as_ref())
            .is_some_and(|meta| meta.pinned);
        if pinned {
            merged.extend(chunks.remove(index).1);
        } else {
            index += 1;
        }
    }
    for repo in ctx
        .categories
        .ordered_repos(&ctx.category_state, &category_model.name)
    {
        let id = repo_id(category, &repo);
        if let Some(index) = chunks.iter().position(|(candidate, _)| candidate == &id) {
            merged.extend(chunks.remove(index).1);
            continue;
        }
        let Some(placement) = ctx.categories.placements.get(&repo) else {
            continue;
        };
        let label = sanitize_detail_label(&placement.repo.display_name);
        merged.push(SidebarRow {
            id: id.clone(),
            kind: SidebarRowKind::Repo,
            depth: 1,
            label: if label.is_empty() {
                "repo".to_string()
            } else {
                label
            },
            chat_count: 0,
            rollup: RollupLevel::Idle,
            badge_state: None,
            expanded: state.is_expanded(&id),
            pane_id: None,
            git: None,
            active: false,
            meta: Some(RowMeta {
                attention_count: Some(0),
                ..RowMeta::default()
            }),
        });
    }
    for (_, chunk) in chunks {
        merged.extend(chunk);
    }
    merged
}

fn repo_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    category_state: &crate::category::CategoryState,
    depth: usize,
    git: &BTreeMap<String, crate::git::GitBadge>,
    now: i64,
    metas: &BTreeMap<(String, String), RowMeta>,
) -> Vec<SidebarRow> {
    let mut repos = BTreeMap::new();
    for ((category, repo), panes) in groups {
        repos.insert((category, repo), panes);
    }
    repo_rows_from_keyed_map(repos, state, category_state, depth, git, now, metas)
}

fn repo_rows_from_map(
    repos: BTreeMap<String, Vec<AgentPane>>,
    state: &SidebarState,
    category_state: &crate::category::CategoryState,
    depth: usize,
    git: &BTreeMap<String, crate::git::GitBadge>,
    now: i64,
    metas: &BTreeMap<(String, String), RowMeta>,
) -> Vec<SidebarRow> {
    let keyed = repos
        .into_iter()
        .map(|(repo, panes)| {
            let category = panes
                .first()
                .map(|pane| pane.category.clone())
                .unwrap_or_else(|| crate::category::UNCATEGORIZED.to_string());
            ((category, repo), panes)
        })
        .collect();
    repo_rows_from_keyed_map(keyed, state, category_state, depth, git, now, metas)
}

fn repo_rows_from_keyed_map(
    repos: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    category_state: &crate::category::CategoryState,
    depth: usize,
    git: &BTreeMap<String, crate::git::GitBadge>,
    now: i64,
    metas: &BTreeMap<(String, String), RowMeta>,
) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    let mut groups = repos.into_values().collect::<Vec<_>>();
    order_repo_groups(&mut groups, category_state);
    for panes in groups {
        let Some(first) = panes.first() else {
            continue;
        };
        let repo_id = repo_id(&first.category, &first.repo_key);
        let expanded = state.is_expanded(&repo_id);
        rows.push(SidebarRow {
            id: repo_id,
            kind: SidebarRowKind::Repo,
            depth,
            label: first.repo.clone(),
            chat_count: panes.len(),
            rollup: rollup(&panes),
            badge_state: badge_rollup(&panes),
            expanded,
            pane_id: None,
            git: git.get(&first.repo_path).cloned(),
            active: panes.iter().any(|pane| pane.active),
            meta: Some({
                let mut meta = metas
                    .get(&(first.category.clone(), first.repo.clone()))
                    .cloned()
                    .unwrap_or_else(|| group_meta(&panes));
                meta.pinned = panes.iter().any(|pane| pane.pinned);
                meta
            }),
        });
        if expanded {
            for pane in &panes {
                push_chat_row(pane, depth + 1, state, now, false, &mut rows);
            }
        }
    }
    rows
}

fn triage_zone_rows(panes: &[AgentPane], state: &SidebarState, now: i64) -> Vec<SidebarRow> {
    if panes.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![SidebarRow {
        id: "zone::triage".to_string(),
        kind: SidebarRowKind::Zone,
        depth: 0,
        label: "TRIAGE".to_string(),
        chat_count: panes.len(),
        rollup: rollup(panes),
        badge_state: badge_rollup(panes),
        expanded: true,
        pane_id: None,
        git: None,
        active: false,
        meta: None,
    }];
    for pane in panes {
        let id = chat_row_id(&pane.pane_instance);
        let expanded = state.is_expanded_with_default(&id, false);
        let origin = format!("{}/{}", pane.category, pane.repo);
        let mut meta = chat_meta(pane, now);
        meta.origin = Some(origin.clone());
        rows.push(SidebarRow {
            id,
            kind: SidebarRowKind::Chat,
            depth: 1,
            label: if expanded {
                expanded_chat_label(pane)
            } else {
                format!("{} · {}", display_agent_name(&pane.agent), pane.repo)
            },
            chat_count: 1,
            rollup: pane.rollup,
            badge_state: Some(pane.badge_state),
            expanded,
            pane_id: Some(pane.pane_id.clone()),
            git: None,
            active: pane.active,
            meta: Some(meta),
        });
        if expanded {
            rows.push(detail_row(pane, 2, "origin", format!("origin: {origin}")));
            push_chat_detail_rows(pane, 2, &mut rows);
        }
    }
    rows
}

fn priority_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    order: &SidebarPreferences,
    now: i64,
) -> Vec<SidebarRow> {
    let mut panes = groups.into_values().flatten().collect::<Vec<_>>();
    order_agent_panes(&mut panes, order);
    let mut rows = Vec::new();
    let mut pinned = Vec::new();
    panes.retain(|pane| {
        if pane.pinned {
            pinned.push(pane.clone());
            false
        } else {
            true
        }
    });
    if !pinned.is_empty() {
        rows.push(SidebarRow {
            id: PRIORITY_PINNED_ZONE_ID.to_string(),
            kind: SidebarRowKind::Zone,
            depth: 0,
            label: "PINNED".to_string(),
            chat_count: pinned.len(),
            rollup: pinned
                .iter()
                .map(|pane| pane.rollup)
                .min()
                .unwrap_or(RollupLevel::Idle),
            badge_state: None,
            expanded: true,
            pane_id: None,
            git: None,
            active: false,
            meta: None,
        });
        for pane in &pinned {
            push_priority_chat_row(pane, state, now, &mut rows);
        }
    }
    for (badge, key, label) in [
        (BadgeState::Blocked, "needs-input", "NEEDS INPUT"),
        (BadgeState::Done, "unread-done", "UNREAD DONE"),
        (BadgeState::Working, "running", "RUNNING"),
        (BadgeState::Idle, "idle", "IDLE"),
    ] {
        let section = panes
            .iter()
            .filter(|pane| pane.badge_state == badge)
            .collect::<Vec<_>>();
        if section.is_empty() {
            continue;
        }
        rows.push(SidebarRow {
            id: format!("zone::priority::{key}"),
            kind: SidebarRowKind::Zone,
            depth: 0,
            label: label.to_string(),
            chat_count: section.len(),
            rollup: section
                .iter()
                .map(|pane| pane.rollup)
                .min()
                .unwrap_or(RollupLevel::Idle),
            badge_state: Some(badge),
            expanded: true,
            pane_id: None,
            git: None,
            active: false,
            meta: None,
        });
        for pane in section {
            push_priority_chat_row(pane, state, now, &mut rows);
        }
    }
    rows
}

fn push_priority_chat_row(
    pane: &AgentPane,
    state: &SidebarState,
    now: i64,
    rows: &mut Vec<SidebarRow>,
) {
    let id = chat_row_id(&pane.pane_instance);
    let expanded = state.is_expanded_with_default(&id, false);
    let origin = format!("{}/{}", pane.category, pane.repo);
    let mut meta = chat_meta(pane, now);
    meta.origin = Some(origin.clone());
    rows.push(SidebarRow {
        id,
        kind: SidebarRowKind::Chat,
        depth: 1,
        label: format!("{} · {origin}", expanded_chat_label(pane)),
        chat_count: 1,
        rollup: pane.rollup,
        badge_state: Some(pane.badge_state),
        expanded,
        pane_id: Some(pane.pane_id.clone()),
        git: None,
        active: pane.active,
        meta: Some(meta),
    });
    if expanded {
        push_chat_detail_rows(pane, 2, rows);
    }
}

fn flat_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    order: &SidebarPreferences,
    now: i64,
    show_origin: bool,
) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    let mut panes = groups.into_values().flatten().collect::<Vec<_>>();
    order_agent_panes(&mut panes, order);
    for pane in &panes {
        push_chat_row(pane, 0, state, now, show_origin, &mut rows);
    }
    rows
}

fn push_chat_row(
    pane: &AgentPane,
    depth: usize,
    state: &SidebarState,
    now: i64,
    show_origin: bool,
    rows: &mut Vec<SidebarRow>,
) {
    let id = chat_row_id(&pane.pane_instance);
    let expanded = state.is_expanded_with_default(&id, false);
    let mut meta = chat_meta(pane, now);
    let mut label = if expanded {
        expanded_chat_label(pane)
    } else {
        chat_label(pane)
    };
    if show_origin {
        let origin = format!("{}/{}", pane.category, pane.repo);
        meta.origin = Some(origin.clone());
        label.push_str(" · ");
        label.push_str(&origin);
    }
    rows.push(SidebarRow {
        id,
        kind: SidebarRowKind::Chat,
        depth,
        label,
        chat_count: 1,
        rollup: pane.rollup,
        badge_state: Some(pane.badge_state),
        expanded,
        pane_id: Some(pane.pane_id.clone()),
        git: None,
        active: pane.active,
        meta: Some(meta),
    });
    if expanded {
        push_chat_detail_rows(pane, depth + 1, rows);
    }
}

fn detail_row(pane: &AgentPane, depth: usize, suffix: &str, label: String) -> SidebarRow {
    SidebarRow {
        id: format!(
            "detail::{}::{}::{suffix}",
            pane.pane_id, pane.pane_instance.pane_pid
        ),
        kind: SidebarRowKind::Detail,
        depth,
        label,
        chat_count: 0,
        rollup: pane.rollup,
        badge_state: Some(pane.badge_state),
        expanded: true,
        pane_id: Some(pane.pane_id.clone()),
        git: None,
        active: pane.active,
        meta: None,
    }
}

fn push_chat_detail_rows(pane: &AgentPane, depth: usize, rows: &mut Vec<SidebarRow>) {
    if let Some(summary) = non_empty(&pane.task_summary) {
        rows.push(detail_row(pane, depth, "summary", summary.to_string()));
    }
    if let Some(signal) = agent_signal_label(pane) {
        let mut row = detail_row(pane, depth, "signal", signal);
        if let Some((done, total)) = parse_tasks(&pane.tasks) {
            row.meta = Some(RowMeta {
                tasks_done: Some(done),
                tasks_total: Some(total),
                ..RowMeta::default()
            });
        }
        rows.push(row);
    }
    if let Some(prompt) = non_empty(&pane.prompt) {
        rows.push(detail_row(pane, depth, "prompt", prompt.to_string()));
    }
    if let Some(process) = &pane.background_process {
        rows.push(detail_row(
            pane,
            depth,
            "background",
            format!("◎ $ {}", sanitize_detail_label(&process.command)),
        ));
    }
    if let Some(response) = non_empty(&pane.latest_response) {
        rows.push(detail_row(
            pane,
            depth,
            "response",
            format!("▷ {}", sanitize_detail_label(response)),
        ));
    }

    if let Some(activity) = pane
        .worktree_activity
        .as_ref()
        .filter(|activity| !same_worktree_path(pane.worktree.as_ref(), activity))
    {
        rows.push(detail_row(
            pane,
            depth,
            "worktree-activity",
            format!("vw exec {}", sanitize_detail_label(&activity.name)),
        ));
    }

    if let Some(last_index) = pane.task_items.len().checked_sub(1) {
        for (index, item) in pane.task_items.iter().enumerate() {
            rows.push(detail_row(
                pane,
                depth,
                &format!("task::{index}::{}", task_status_key(item.status)),
                task_detail_label(index, last_index, item),
            ));
        }
    }

    if let Some(last_index) = pane.subagents.len().checked_sub(1) {
        for (index, subagent) in pane.subagents.iter().enumerate() {
            rows.push(detail_row(
                pane,
                depth,
                &format!("subagent::{index}"),
                subagent_detail_label(index, last_index, subagent),
            ));
        }
    }
}

fn agent_signal_label(pane: &AgentPane) -> Option<String> {
    const MAX_SIGNAL_PORTS: usize = 3;

    let mut parts = Vec::new();
    let worktree_branch = pane
        .worktree
        .as_ref()
        .and_then(|worktree| worktree.branch.as_deref());
    let branch = worktree_branch
        .or_else(|| pane.git.as_ref().map(|git| git.branch.as_str()))
        .or_else(|| {
            pane.worktree
                .as_ref()
                .map(|worktree| worktree.name.as_str())
        });
    if let Some(branch) = branch.filter(|branch| !branch.trim().is_empty()) {
        let marker = if pane.worktree.is_some() { "+ " } else { "" };
        parts.push(format!("{marker}{}", sanitize_detail_label(branch)));
    }
    if let Some(git) = &pane.git {
        if git.ahead > 0 {
            parts.push(format!("↑ {}", git.ahead));
        }
        if git.behind > 0 {
            parts.push(format!("↓ {}", git.behind));
        }
    }

    let progress = parse_tasks(&pane.tasks).unwrap_or((0, 0));
    if let Some(label) = task_progress_label(progress.0, progress.1) {
        parts.push(label);
    }

    if !pane.listening_ports.is_empty() {
        let mut ports = pane
            .listening_ports
            .iter()
            .take(MAX_SIGNAL_PORTS)
            .map(|port| format!(":{port}"))
            .collect::<Vec<_>>();
        if pane.listening_ports.len() > MAX_SIGNAL_PORTS {
            ports.push(format!(
                "+{}",
                pane.listening_ports.len() - MAX_SIGNAL_PORTS
            ));
        }
        parts.push(ports.join(" "));
    }

    (!parts.is_empty()).then(|| parts.join("  "))
}

pub(crate) fn task_progress_label(done: i64, total: i64) -> Option<String> {
    (total > 0).then(|| format!("☑ {done}/{total}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SubagentDetail {
    agent_id: String,
    agent_type: String,
    display_name: Option<String>,
}

impl SubagentDetail {
    fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.agent_type)
    }
}

fn same_worktree_path(worktree: Option<&WorktreeInfo>, activity: &WorktreeActivity) -> bool {
    worktree
        .map(|worktree| worktree.path == activity.path)
        .unwrap_or(false)
}

fn task_detail_label(index: usize, last_index: usize, item: &TaskItem) -> String {
    format!(
        "{} {} Task - {}",
        tree_connector(index, last_index),
        task_status_icon(item.status),
        sanitize_detail_label(&item.step)
    )
}

fn subagent_detail_label(index: usize, last_index: usize, subagent: &SubagentDetail) -> String {
    format!(
        "{} Agent - {}{}",
        tree_connector(index, last_index),
        sanitize_detail_label(subagent.label()),
        subagent_id_suffix(&subagent.agent_id)
    )
}

fn tree_connector(index: usize, last_index: usize) -> &'static str {
    if index == last_index {
        "\u{2514}"
    } else {
        "\u{251c}"
    }
}

fn task_status_icon(status: TaskItemStatus) -> &'static str {
    match status {
        TaskItemStatus::Completed => "✓",
        TaskItemStatus::InProgress => "●",
        TaskItemStatus::Pending => "○",
    }
}

fn task_status_key(status: TaskItemStatus) -> &'static str {
    match status {
        TaskItemStatus::Completed => "completed",
        TaskItemStatus::InProgress => "in_progress",
        TaskItemStatus::Pending => "pending",
    }
}

fn sanitize_detail_label(raw: &str) -> String {
    raw.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn expanded_chat_label(pane: &AgentPane) -> String {
    display_agent_name(&pane.agent)
}

fn chat_meta(pane: &AgentPane, now: i64) -> RowMeta {
    let tasks = parse_tasks(&pane.tasks);
    RowMeta {
        agent: Some(display_agent_name(&pane.agent)),
        prompt: non_empty(&pane.prompt).map(str::to_string),
        task_summary: non_empty(&pane.task_summary).map(str::to_string),
        wait_reason: non_empty(&pane.wait_reason).map(str::to_string),
        elapsed_secs: pane
            .started_at
            .parse::<i64>()
            .ok()
            .map(|started_at| (now - started_at).max(0)),
        completed_age_secs: pane
            .completed_at
            .parse::<i64>()
            .ok()
            .map(|completed_at| (now - completed_at).max(0)),
        tasks_done: tasks.map(|(done, _)| done),
        tasks_total: tasks.map(|(_, total)| total),
        subagent_count: Some(pane.subagents.len()),
        attention_count: None,
        origin: None,
        flash: pane.flash.then_some(true),
        is_unread: pane.is_unread,
        pinned: pane.pinned,
        latest_unread_order: pane.latest_unread_order,
        latest_unread_reason: pane.latest_unread_reason,
    }
}

fn group_meta(panes: &[AgentPane]) -> RowMeta {
    RowMeta {
        attention_count: Some(
            panes
                .iter()
                .filter(|pane| pane_matches_attention_filter(pane))
                .count(),
        ),
        pinned: panes.iter().any(|pane| pane.pinned),
        ..RowMeta::default()
    }
}

pub fn humanize_secs(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 10 {
        return format!("{minutes}m{:02}s", secs % 60);
    }
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 10 {
        let rest = minutes % 60;
        if rest == 0 {
            return format!("{hours}h");
        }
        return format!("{hours}h{rest:02}m");
    }
    if hours < 48 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

pub fn humanize_secs_full(secs: i64) -> String {
    let secs = secs.max(0);
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn subagent_id_suffix(agent_id: &str) -> String {
    let prefix = agent_id.chars().take(4).collect::<String>();
    if prefix.is_empty() {
        String::new()
    } else {
        format!(" #{prefix}")
    }
}

fn chat_label(pane: &AgentPane) -> String {
    let agent = display_agent_name(&pane.agent);
    let base = if let Some(prompt) = non_empty(&pane.prompt) {
        format!("{agent}: {prompt}")
    } else {
        format!("{agent} ({})", pane.pane_id)
    };
    if let Some((done, total)) = parse_tasks(&pane.tasks).filter(|(_, total)| *total > 0) {
        format!("{base} {done}/{total}")
    } else {
        base
    }
}

fn parse_tasks(raw: &str) -> Option<(i64, i64)> {
    let (done, total) = raw.split_once('/')?;
    Some((done.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn pane_matches_filter(pane: &AgentPane, filter: StatusFilter) -> bool {
    match filter {
        StatusFilter::All => true,
        StatusFilter::AttentionOnly => pane_matches_attention_filter(pane),
        StatusFilter::WorkingOnly => pane.badge_state == BadgeState::Working,
        StatusFilter::DoneOnly => pane.badge_state == BadgeState::Done,
        StatusFilter::IdleOnly => pane.badge_state == BadgeState::Idle,
    }
}

fn pane_matches_attention_filter(pane: &AgentPane) -> bool {
    badge_needs_user_input(pane.badge_state)
}

pub(crate) fn badge_needs_user_input(badge: BadgeState) -> bool {
    badge == BadgeState::Blocked
}

fn order_repo_groups(
    groups: &mut [Vec<AgentPane>],
    category_state: &crate::category::CategoryState,
) {
    let position = |panes: &Vec<AgentPane>| -> usize {
        let Some(first) = panes.first() else {
            return usize::MAX;
        };
        let category = if first.category == crate::category::UNCATEGORIZED {
            crate::category::CategoryName::uncategorized()
        } else {
            match crate::category::CategoryName::parse(&first.category) {
                Ok(category) => category,
                Err(_) => return usize::MAX,
            }
        };
        category_state
            .repo_order
            .get(&category)
            .and_then(|repos| repos.iter().position(|repo| repo == &first.repo_key))
            .unwrap_or(usize::MAX)
    };
    groups.sort_by(|left, right| {
        let has_pin = |panes: &Vec<AgentPane>| panes.iter().any(|pane| pane.pinned);
        has_pin(right)
            .cmp(&has_pin(left))
            .then_with(|| position(left).cmp(&position(right)))
            .then_with(|| {
                let left = left.first();
                let right = right.first();
                left.map(|pane| (&pane.category, &pane.repo))
                    .cmp(&right.map(|pane| (&pane.category, &pane.repo)))
            })
    });
}

pub(crate) fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn order_agent_panes(panes: &mut [AgentPane], order: &SidebarPreferences) {
    panes.sort_by(|left, right| compare_agent_panes(left, right, order));
}

fn compare_agent_panes(
    left: &AgentPane,
    right: &AgentPane,
    order: &SidebarPreferences,
) -> std::cmp::Ordering {
    let manual_position = |pane: &AgentPane| {
        order
            .manual_chat_order
            .iter()
            .position(|pane_id| pane_id == &pane.pane_id)
            .unwrap_or(usize::MAX)
    };
    right
        .pinned
        .cmp(&left.pinned)
        .then_with(|| {
            if left.pinned && right.pinned {
                manual_position(left).cmp(&manual_position(right))
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| chat_sort_bucket(left).cmp(&chat_sort_bucket(right)))
        .then_with(|| Reverse(chat_sort_time(left)).cmp(&Reverse(chat_sort_time(right))))
        .then_with(|| manual_position(left).cmp(&manual_position(right)))
        .then_with(|| left.pane_id.cmp(&right.pane_id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ChatSortBucket {
    NeedsAttention,
    Running,
    Done,
    Idle,
}

fn chat_sort_bucket(pane: &AgentPane) -> ChatSortBucket {
    match pane.badge_state {
        BadgeState::Blocked => ChatSortBucket::NeedsAttention,
        BadgeState::Working => ChatSortBucket::Running,
        BadgeState::Done => ChatSortBucket::Done,
        BadgeState::Idle => ChatSortBucket::Idle,
    }
}

fn chat_sort_time(pane: &AgentPane) -> Option<i64> {
    match chat_sort_bucket(pane) {
        ChatSortBucket::NeedsAttention | ChatSortBucket::Running => parse_epoch(&pane.started_at),
        ChatSortBucket::Done => parse_epoch(&pane.completed_at),
        ChatSortBucket::Idle => None,
    }
}

fn parse_epoch(raw: &str) -> Option<i64> {
    raw.trim().parse().ok()
}

fn repo_id(category: &str, repo: &crate::category::RepoKey) -> String {
    format!("repo::{category}::{repo}")
}

fn rollup(panes: &[AgentPane]) -> RollupLevel {
    panes
        .iter()
        .map(|pane| pane.rollup)
        .min()
        .unwrap_or(RollupLevel::Idle)
}

fn badge_rollup(panes: &[AgentPane]) -> Option<BadgeState> {
    panes.iter().map(|pane| pane.badge_state).min()
}

fn repo_label_from_values(path: &str, session_name: &str) -> String {
    let path = path.trim_end_matches('/');
    let repo = path
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(session_name);
    let repo = sanitize_detail_label(repo);
    if repo.is_empty() {
        "repo".to_string()
    } else {
        repo
    }
}

fn non_empty(raw: &str) -> Option<&str> {
    (!raw.trim().is_empty()).then(|| raw.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_pane(badge_state: BadgeState, completed_at: &str) -> AgentPane {
        AgentPane {
            pane_instance: PaneInstance {
                pane_id: "%1".to_string(),
                pane_pid: 1,
            },
            pane_id: "%1".to_string(),
            repo: "repo".to_string(),
            repo_key: crate::category::RepoKey::path("/repo"),
            category: "misc".to_string(),
            agent: "codex".to_string(),
            prompt: String::new(),
            latest_response: String::new(),
            task_summary: String::new(),
            wait_reason: String::new(),
            started_at: "100".to_string(),
            completed_at: completed_at.to_string(),
            tasks: "0/0".to_string(),
            task_items: Vec::new(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
            worktree: None,
            git: None,
            rollup: RollupLevel::Idle,
            badge_state,
            repo_path: "/tmp/repo".to_string(),
            flash: false,
            active: false,
            is_unread: false,
            pinned: false,
            latest_unread_order: None,
            latest_unread_reason: None,
        }
    }

    #[test]
    fn repo_label_strips_control_chars_from_directory_name() {
        // A malicious directory name embedding a terminal escape must not reach the label.
        let label = repo_label_from_values("/work/re\u{1b}[31mpo", "session");
        assert!(!label.chars().any(|ch| ch.is_control()));
        assert_eq!(label, "re [31mpo");
    }

    #[test]
    fn repo_label_falls_back_when_segment_is_all_control_chars() {
        let label = repo_label_from_values("/work/\u{1b}\u{7}", "session");
        assert_eq!(label, "repo");
    }

    #[test]
    fn repo_label_uses_sanitized_session_name_when_path_has_no_segment() {
        let label = repo_label_from_values("/", "ses\u{1b}sion");
        assert!(!label.chars().any(|ch| ch.is_control()));
        assert_eq!(label, "ses sion");
    }

    #[test]
    fn repo_label_normalizes_unit_separator() {
        assert_eq!(repo_label_from_values("/work/a\u{1f}b", "session"), "a b");
    }

    #[test]
    fn humanize_secs_formats_by_magnitude() {
        assert_eq!(humanize_secs(0), "0s");
        assert_eq!(humanize_secs(30), "30s");
        assert_eq!(humanize_secs(60), "1m00s");
        assert_eq!(humanize_secs(127), "2m07s");
        assert_eq!(humanize_secs(599), "9m59s");
        assert_eq!(humanize_secs(600), "10m");
        assert_eq!(humanize_secs(12 * 60 + 30), "12m");
        assert_eq!(humanize_secs(90 * 60), "1h30m");
        assert_eq!(humanize_secs(10 * 3600), "10h");
        assert_eq!(humanize_secs(38 * 3600 + 59 * 60), "38h");
        assert_eq!(humanize_secs(48 * 3600), "2d");
        assert_eq!(humanize_secs(100 * 3600), "4d");
        assert_eq!(humanize_secs(-5), "0s");
        assert_eq!(humanize_secs_full(0), "0s");
        assert_eq!(humanize_secs_full(59), "59s");
        assert_eq!(humanize_secs_full(60), "1m 00s");
        assert_eq!(humanize_secs_full(127), "2m 07s");
        assert_eq!(humanize_secs_full(3_600), "1h 00m 00s");
        assert_eq!(humanize_secs_full(5_527), "1h 32m 07s");
    }

    #[test]
    fn row_refs_exclude_non_focusable_rows() {
        let rows = [
            SidebarRow {
                id: "zone::triage".to_string(),
                kind: SidebarRowKind::Zone,
                depth: 0,
                label: "TRIAGE".to_string(),
                chat_count: 1,
                rollup: RollupLevel::Permission,
                badge_state: None,
                expanded: true,
                pane_id: None,
                git: None,
                active: false,
                meta: None,
            },
            SidebarRow {
                id: "detail::%1::prompt".to_string(),
                kind: SidebarRowKind::Detail,
                depth: 1,
                label: "fix bug".to_string(),
                chat_count: 0,
                rollup: RollupLevel::Running,
                badge_state: None,
                expanded: true,
                pane_id: Some("%1".to_string()),
                git: None,
                active: false,
                meta: None,
            },
            SidebarRow {
                id: "repo::misc::app".to_string(),
                kind: SidebarRowKind::Repo,
                depth: 0,
                label: "app".to_string(),
                chat_count: 1,
                rollup: RollupLevel::Running,
                badge_state: None,
                expanded: true,
                pane_id: None,
                git: None,
                active: false,
                meta: None,
            },
        ];

        assert_eq!(row_refs(&rows), vec![SidebarRowRef::new("repo::misc::app")]);
    }

    #[test]
    fn empty_presentations_render_no_rows() {
        let (rows, counts) = build_rows_from_presentations(
            &Config::default(),
            &[],
            &SidebarState::default(),
            &SidebarPreferences::default(),
            &RowBuildContext::default(),
        );

        assert!(rows.is_empty());
        assert_eq!(counts, BadgeCounts::default());
    }

    #[test]
    fn category_view_shows_repository_placements_without_agents() {
        let mut config = Config::default();
        config.categories.rules.push(crate::config::CategoryRule {
            category: "private".to_string(),
            path_patterns: vec!["dotfiles".to_string()],
        });
        let category_state = crate::category::CategoryState::default();
        let repo = crate::category::RepoIdentity {
            key: crate::category::RepoKey::git("/Users/me/dotfiles/.git"),
            rule_path: "/Users/me/dotfiles".to_string(),
            display_name: "dotfiles".to_string(),
        };
        let context = RowBuildContext {
            categories: crate::category::EffectiveCategoryModel::build(
                &config,
                &category_state,
                [repo.clone()],
            )
            .unwrap(),
            category_state,
            ..RowBuildContext::default()
        };

        let (rows, counts) = build_rows_from_presentations(
            &config,
            &[],
            &SidebarState {
                category_scope: CategoryScope::All,
                ..SidebarState::default()
            },
            &SidebarPreferences::default(),
            &context,
        );

        let row = rows
            .iter()
            .find(|row| row.id == repo_id("private", &repo.key))
            .expect("repository placement should remain visible without an agent pane");
        assert_eq!(row.label, "dotfiles");
        assert_eq!(row.chat_count, 0);
        assert_eq!(row.badge_state, None);
        assert_eq!(counts, BadgeCounts::default());
    }

    #[test]
    fn grouping_sort_and_triage_follow_the_canonical_tree_order() {
        let mut blocked = agent_pane(BadgeState::Blocked, "");
        blocked.pane_instance = PaneInstance {
            pane_id: "%3".to_string(),
            pane_pid: 3,
        };
        blocked.pane_id = "%3".to_string();
        blocked.repo = "zeta".to_string();
        blocked.rollup = RollupLevel::Permission;

        let mut running = agent_pane(BadgeState::Working, "");
        running.pane_instance = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 2,
        };
        running.pane_id = "%2".to_string();
        running.repo = "alpha".to_string();
        running.rollup = RollupLevel::Running;

        let mut idle = agent_pane(BadgeState::Idle, "");
        idle.pane_instance = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 1,
        };
        idle.pane_id = "%1".to_string();
        idle.repo = "alpha".to_string();

        let groups = BTreeMap::from([
            (
                ("misc".to_string(), "zeta".to_string()),
                vec![blocked.clone()],
            ),
            (
                ("misc".to_string(), "alpha".to_string()),
                vec![idle, running],
            ),
        ]);
        let context = RowBuildContext {
            triage: BTreeSet::from([blocked.pane_instance.clone()]),
            now: 100,
            ..RowBuildContext::default()
        };

        let (rows, counts) = build_rows_from_groups(
            groups,
            &SidebarState {
                current_category: Some("misc".to_string()),
                ..SidebarState::default()
            },
            &SidebarPreferences::default(),
            &context,
        );

        assert_eq!(rows[0].kind, SidebarRowKind::Zone);
        assert_eq!(rows[1].id, chat_row_id(&blocked.pane_instance));
        let repo_labels = rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Repo)
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(repo_labels, vec!["alpha"]);
        let alpha_chats = rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Chat && row.id != rows[1].id)
            .map(|row| row.pane_id.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(alpha_chats, vec!["%2", "%1"]);
        assert_eq!(counts.total, 3);
        assert_eq!(counts.blocked, 1);
    }

    #[test]
    fn current_and_all_scopes_compose_with_every_presentation() {
        let mut work = agent_pane(BadgeState::Working, "");
        work.pane_instance = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 1,
        };
        work.pane_id = "%1".to_string();
        work.repo = "work-repo".to_string();
        work.repo_key = crate::category::RepoKey::path("/work-repo");
        work.category = "work".to_string();

        let mut private = agent_pane(BadgeState::Idle, "");
        private.pane_instance = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 2,
        };
        private.pane_id = "%2".to_string();
        private.repo = "private-repo".to_string();
        private.repo_key = crate::category::RepoKey::path("/private-repo");
        private.category = "private".to_string();

        let groups = BTreeMap::from([
            (
                ("work".to_string(), work.repo_key.to_string()),
                vec![work.clone()],
            ),
            (
                ("private".to_string(), private.repo_key.to_string()),
                vec![private.clone()],
            ),
        ]);
        let context = RowBuildContext {
            active_categories: BTreeSet::from(["work".to_string()]),
            ..RowBuildContext::default()
        };

        for presentation_mode in [
            PresentationMode::Tree,
            PresentationMode::Priority,
            PresentationMode::Flat,
        ] {
            let state = SidebarState {
                current_category: Some("work".to_string()),
                presentation_mode,
                ..SidebarState::default()
            };
            let (rows, counts) = build_rows_from_groups(
                groups.clone(),
                &state,
                &SidebarPreferences::default(),
                &context,
            );
            assert!(rows.iter().any(|row| row.pane_id.as_deref() == Some("%1")));
            assert!(rows.iter().all(|row| row.pane_id.as_deref() != Some("%2")));
            assert_eq!(counts.total, 1);
        }

        for presentation_mode in [
            PresentationMode::Tree,
            PresentationMode::Priority,
            PresentationMode::Flat,
        ] {
            let (rows, counts) = build_rows_from_groups(
                groups.clone(),
                &SidebarState {
                    category_scope: CategoryScope::All,
                    presentation_mode,
                    ..SidebarState::default()
                },
                &SidebarPreferences::default(),
                &context,
            );
            assert!(rows.iter().any(|row| row.pane_id.as_deref() == Some("%1")));
            assert!(rows.iter().any(|row| row.pane_id.as_deref() == Some("%2")));
            assert_eq!(counts.total, 2);
            if presentation_mode == PresentationMode::Tree {
                assert!(rows.iter().any(|row| row.id == "category::work"));
                assert!(rows.iter().any(|row| row.id == "category::private"));
            }
            if presentation_mode == PresentationMode::Flat {
                assert!(
                    rows.iter()
                        .filter(|row| row.kind == SidebarRowKind::Chat)
                        .all(|row| row
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.origin.as_deref())
                            .is_some())
                );
                assert!(rows.iter().any(|row| {
                    row.pane_id.as_deref() == Some("%2")
                        && row.label.contains("private/private-repo")
                }));
            }
        }

        let (rows, counts) = build_rows_from_groups(
            groups,
            &SidebarState::default(),
            &SidebarPreferences::default(),
            &context,
        );
        assert!(rows.is_empty());
        assert_eq!(counts, BadgeCounts::default());
    }

    #[test]
    fn pure_projection_advances_elapsed_boundary_without_snapshot_revision_change() {
        use crate::pane_state::{
            AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneState, PromptState, StateId,
            TaskState,
        };

        let pane_instance = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 101,
        };
        let canonical = PaneState {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            state_id: StateId::parse("00112233445566778899aabbccddeeff").unwrap(),
            revision: 1,
            pane_instance: pane_instance.clone(),
            agent: AgentKind::parse("codex").unwrap(),
            agent_session_id: None,
            agent_process: None,
            agent_epoch: 1,
            agent_present: true,
            scan_verified: true,
            synthetic_completion_armed: false,
            lifecycle: LifecycleState::Running,
            run_seq: 1,
            completed_seq: 0,
            unread: crate::pane_state::UnreadState::default(),
            started_at: Some(1_000),
            completed_at: None,
            prompt: Some(PromptState {
                text: "working".to_string(),
                source: "test".to_string(),
                digest: None,
            }),
            latest_response: None,
            task_context: crate::pane_state::TaskContextState::default(),
            tasks: TaskState::default(),
            subagents: Vec::new(),
            worktree_activity: None,
            background_process: None,
            listening_ports: Vec::new(),
        };
        let pane = crate::daemon::protocol::v2::PanePresentation {
            pane_instance: pane_instance.clone(),
            session_links: vec![crate::daemon::protocol::v2::SessionLinkPresentation {
                session_id: "$1".to_string(),
                session_name: "main".to_string(),
                window_index: 0,
                window_active: true,
                window_last: false,
            }],
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            current_path: "/tmp/app".to_string(),
            current_command: "codex".to_string(),
            pane_width: 80,
            active: true,
            agent_process: None,
            stored: Some(crate::pane_state::StoredStateDescriptor::Canonical {
                version: canonical.version(),
            }),
            resolved: Some(crate::pane_state::ResolvedPaneState {
                canonical,
                window_id: "@1".to_string(),
                pane_id: "%1".to_string(),
                current_path: "/tmp/app".to_string(),
                badge: BadgeState::Working,
            }),
            retained_state: None,
        };
        let snapshot = crate::daemon::protocol::v2::ResolvedSnapshot {
            snapshot_revision: 7,
            panes: vec![pane],
            sidebar_model: crate::daemon::SidebarModel::default(),
            attention: Vec::new(),
            events: Vec::new(),
            diagnostics: Vec::new(),
        };
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let state = SidebarState {
            category_scope: CategoryScope::All,
            ..SidebarState::default()
        };

        let before = project_sidebar(
            &Config::default(),
            &snapshot.panes,
            &snapshot.sidebar_model,
            &state,
            1_059,
        );
        let after = project_sidebar(
            &Config::default(),
            &snapshot.panes,
            &snapshot.sidebar_model,
            &state,
            1_060,
        );
        let before_text = crate::sidebar::render::render_rows(&before.rows, &state, 36);
        let after_text = crate::sidebar::render::render_rows(&after.rows, &state, 36);

        assert!(before_text.contains("59s"), "{before_text}");
        assert!(after_text.contains("1m00s"), "{after_text}");
        assert_eq!(snapshot.snapshot_revision, 7);
        assert_eq!(serde_json::to_vec(&snapshot).unwrap(), encoded);
    }

    #[test]
    fn filters_and_sorting_use_canonical_badge_not_completion_history() {
        let idle_with_history = agent_pane(BadgeState::Idle, "200");
        assert!(!pane_matches_attention_filter(&idle_with_history));
        assert_eq!(chat_sort_bucket(&idle_with_history), ChatSortBucket::Idle);

        let blocked_with_history = agent_pane(BadgeState::Blocked, "200");
        assert!(pane_matches_attention_filter(&blocked_with_history));
        assert_eq!(
            chat_sort_bucket(&blocked_with_history),
            ChatSortBucket::NeedsAttention
        );

        let done = agent_pane(BadgeState::Done, "200");
        assert!(!pane_matches_attention_filter(&done));
        assert!(badge_needs_user_input(BadgeState::Blocked));
        assert!(!badge_needs_user_input(BadgeState::Done));
        assert!(!badge_needs_user_input(BadgeState::Working));
        assert!(!badge_needs_user_input(BadgeState::Idle));
    }

    #[test]
    fn expanded_chat_places_summary_before_latest_prompt() {
        let mut pane = agent_pane(BadgeState::Working, "");
        pane.task_summary = "サイドバー要約表示".to_string();
        pane.prompt = "実装してlocal installして".to_string();
        let mut rows = Vec::new();

        push_chat_detail_rows(&pane, 1, &mut rows);

        assert_eq!(rows[0].id, "detail::%1::1::summary");
        assert_eq!(rows[0].label, "サイドバー要約表示");
        assert_eq!(rows[1].id, "detail::%1::1::prompt");
        assert_eq!(rows[1].label, "実装してlocal installして");
    }

    #[test]
    fn expanded_chat_combines_branch_tasks_ports_and_shows_background_response() {
        let mut pane = agent_pane(BadgeState::Working, "");
        pane.git = Some(crate::git::GitBadge {
            branch: "feature/sidebar".to_string(),
            ahead: 2,
            behind: 1,
        });
        pane.tasks = "1/3".to_string();
        pane.task_items = vec![
            TaskItem {
                step: "done".to_string(),
                status: TaskItemStatus::Completed,
            },
            TaskItem {
                step: "working".to_string(),
                status: TaskItemStatus::InProgress,
            },
            TaskItem {
                step: "pending".to_string(),
                status: TaskItemStatus::Pending,
            },
        ];
        pane.listening_ports = vec![3000, 5173];
        pane.background_process = Some(crate::pane_state::BackgroundProcessState {
            command: "pnpm dev".to_string(),
            observed_at: 1,
        });
        pane.latest_response = "server is ready".to_string();
        let mut rows = Vec::new();

        push_chat_detail_rows(&pane, 1, &mut rows);

        assert_eq!(rows[0].id, "detail::%1::1::signal");
        assert_eq!(
            rows[0].label,
            "feature/sidebar  ↑ 2  ↓ 1  ☑ 1/3  :3000 :5173"
        );
        assert!(rows.iter().any(|row| row.label == "◎ $ pnpm dev"));
        assert!(rows.iter().any(|row| row.label == "▷ server is ready"));
    }

    #[test]
    fn priority_groups_all_categories_by_attention_then_done_running_and_idle() {
        let mut blocked = agent_pane(BadgeState::Blocked, "");
        blocked.pane_instance = PaneInstance {
            pane_id: "%4".to_string(),
            pane_pid: 4,
        };
        blocked.pane_id = "%4".to_string();
        blocked.category = "work".to_string();
        blocked.repo = "frontend".to_string();
        blocked.repo_key = crate::category::RepoKey::path("/frontend");
        blocked.rollup = RollupLevel::Permission;

        let mut done = agent_pane(BadgeState::Done, "300");
        done.pane_instance = PaneInstance {
            pane_id: "%3".to_string(),
            pane_pid: 3,
        };
        done.pane_id = "%3".to_string();
        done.category = "private".to_string();
        done.repo = "dotfiles".to_string();
        done.repo_key = crate::category::RepoKey::path("/dotfiles");

        let mut working = agent_pane(BadgeState::Working, "");
        working.pane_instance = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 2,
        };
        working.pane_id = "%2".to_string();
        working.category = "work".to_string();
        working.repo = "backend".to_string();
        working.repo_key = crate::category::RepoKey::path("/backend");
        working.rollup = RollupLevel::Running;

        let mut idle = agent_pane(BadgeState::Idle, "");
        idle.pane_instance = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 1,
        };
        idle.pane_id = "%1".to_string();
        idle.category = "private".to_string();
        idle.repo = "notes".to_string();
        idle.repo_key = crate::category::RepoKey::path("/notes");

        let groups = BTreeMap::from([
            (
                (blocked.category.clone(), blocked.repo_key.to_string()),
                vec![blocked.clone()],
            ),
            (
                (done.category.clone(), done.repo_key.to_string()),
                vec![done.clone()],
            ),
            (
                (working.category.clone(), working.repo_key.to_string()),
                vec![working.clone()],
            ),
            (
                (idle.category.clone(), idle.repo_key.to_string()),
                vec![idle.clone()],
            ),
        ]);
        let context = RowBuildContext {
            triage: BTreeSet::from([blocked.pane_instance.clone()]),
            active_categories: BTreeSet::from(["work".to_string()]),
            now: 400,
            ..RowBuildContext::default()
        };
        let state = SidebarState {
            category_scope: CategoryScope::All,
            presentation_mode: PresentationMode::Priority,
            ..SidebarState::default()
        };

        let (rows, counts) = build_rows_from_groups(
            groups.clone(),
            &state,
            &SidebarPreferences::default(),
            &context,
        );

        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Zone)
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["NEEDS INPUT", "UNREAD DONE", "RUNNING", "IDLE"]
        );
        assert!(!rows.iter().any(|row| row.id == "zone::triage"));
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .map(|row| row.pane_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["%4", "%3", "%2", "%1"]
        );
        assert!(rows.iter().any(|row| {
            row.pane_id.as_deref() == Some("%3")
                && row.meta.as_ref().and_then(|meta| meta.origin.as_deref())
                    == Some("private/dotfiles")
        }));
        assert_eq!(counts.total, 4);
        assert_eq!(counts.blocked, 1);
        assert_eq!(counts.done, 1);

        for (filter, expected) in [
            (StatusFilter::AttentionOnly, vec!["%4"]),
            (StatusFilter::WorkingOnly, vec!["%2"]),
            (StatusFilter::DoneOnly, vec!["%3"]),
            (StatusFilter::IdleOnly, vec!["%1"]),
        ] {
            let filtered = SidebarState {
                category_scope: CategoryScope::All,
                presentation_mode: PresentationMode::Priority,
                filter,
                ..SidebarState::default()
            };
            let (rows, _) = build_rows_from_groups(
                groups.clone(),
                &filtered,
                &SidebarPreferences::default(),
                &context,
            );
            assert_eq!(
                rows.iter()
                    .filter(|row| row.kind == SidebarRowKind::Chat)
                    .map(|row| row.pane_id.as_deref().unwrap())
                    .collect::<Vec<_>>(),
                expected,
                "filter {filter:?}"
            );
        }
    }

    #[test]
    fn priority_extracts_filtered_pins_independent_of_unread_state() {
        let mut newest = agent_pane(BadgeState::Working, "");
        newest.pane_instance = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 2,
        };
        newest.pane_id = "%2".to_string();
        newest.is_unread = true;
        newest.pinned = true;
        newest.latest_unread_order = Some(20);
        newest.latest_unread_reason = Some(crate::pane_state::UnreadReason::Waiting);

        let mut tied = newest.clone();
        tied.pane_instance = PaneInstance {
            pane_id: "%0".to_string(),
            pane_pid: 3,
        };
        tied.pane_id = "%0".to_string();

        let mut older = agent_pane(BadgeState::Done, "10");
        older.pane_instance = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 1,
        };
        older.pane_id = "%1".to_string();
        older.is_unread = false;
        older.pinned = true;
        older.latest_unread_order = Some(10);
        older.latest_unread_reason = Some(crate::pane_state::UnreadReason::Completed);

        let groups = BTreeMap::from([(
            (
                "misc".to_string(),
                crate::category::RepoKey::path("/repo").to_string(),
            ),
            vec![older, newest, tied],
        )]);
        let state = SidebarState {
            category_scope: CategoryScope::All,
            presentation_mode: PresentationMode::Priority,
            ..SidebarState::default()
        };
        let (rows, counts) = build_rows_from_groups(
            groups.clone(),
            &state,
            &SidebarPreferences::default(),
            &RowBuildContext::default(),
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Zone)
                .map(|row| (row.label.as_str(), row.chat_count))
                .collect::<Vec<_>>(),
            vec![("PINNED", 3)]
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .map(|row| row.pane_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["%0", "%2", "%1"]
        );
        assert_eq!(counts.total, 3);
        assert_eq!(counts.working, 2);
        assert_eq!(counts.done, 1);

        let filtered = SidebarState {
            category_scope: CategoryScope::All,
            presentation_mode: PresentationMode::Priority,
            filter: StatusFilter::DoneOnly,
            ..SidebarState::default()
        };
        let (rows, counts) = build_rows_from_groups(
            groups,
            &filtered,
            &SidebarPreferences::default(),
            &RowBuildContext::default(),
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .map(|row| row.pane_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["%1"]
        );
        assert_eq!(counts.total, 3, "header count is filter-independent");
    }

    #[test]
    fn flat_places_pinned_agents_first_across_repositories() {
        let mut regular = agent_pane(BadgeState::Blocked, "");
        regular.pane_id = "%1".to_string();
        regular.pane_instance = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 1,
        };
        regular.repo = "alpha".to_string();
        regular.repo_key = crate::category::RepoKey::path("/alpha");

        let mut pinned = agent_pane(BadgeState::Idle, "");
        pinned.pane_id = "%2".to_string();
        pinned.pane_instance = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 2,
        };
        pinned.repo = "zeta".to_string();
        pinned.repo_key = crate::category::RepoKey::path("/zeta");
        pinned.pinned = true;

        let groups = BTreeMap::from([
            (("misc".to_string(), "alpha".to_string()), vec![regular]),
            (("misc".to_string(), "zeta".to_string()), vec![pinned]),
        ]);
        let state = SidebarState {
            category_scope: CategoryScope::All,
            presentation_mode: PresentationMode::Flat,
            ..SidebarState::default()
        };
        let (rows, _) = build_rows_from_groups(
            groups,
            &state,
            &SidebarPreferences::default(),
            &RowBuildContext::default(),
        );

        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .map(|row| row.pane_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["%2", "%1"]
        );
    }

    #[test]
    fn current_tree_promotes_pinned_repository_and_agent_without_triage_extraction() {
        let mut regular_repo = agent_pane(BadgeState::Working, "");
        regular_repo.pane_id = "%1".to_string();
        regular_repo.pane_instance = PaneInstance {
            pane_id: "%1".to_string(),
            pane_pid: 1,
        };
        regular_repo.repo = "alpha".to_string();
        regular_repo.repo_key = crate::category::RepoKey::path("/alpha");

        let mut pinned = agent_pane(BadgeState::Blocked, "");
        pinned.pane_id = "%2".to_string();
        pinned.pane_instance = PaneInstance {
            pane_id: "%2".to_string(),
            pane_pid: 2,
        };
        pinned.repo = "zeta".to_string();
        pinned.repo_key = crate::category::RepoKey::path("/zeta");
        pinned.pinned = true;

        let mut sibling = agent_pane(BadgeState::Working, "");
        sibling.pane_id = "%3".to_string();
        sibling.pane_instance = PaneInstance {
            pane_id: "%3".to_string(),
            pane_pid: 3,
        };
        sibling.repo = "zeta".to_string();
        sibling.repo_key = crate::category::RepoKey::path("/zeta");

        let groups = BTreeMap::from([
            (
                ("misc".to_string(), "alpha".to_string()),
                vec![regular_repo],
            ),
            (
                ("misc".to_string(), "zeta".to_string()),
                vec![sibling, pinned.clone()],
            ),
        ]);
        let state = SidebarState {
            category_scope: CategoryScope::Current,
            presentation_mode: PresentationMode::Tree,
            current_category: Some("misc".to_string()),
            ..SidebarState::default()
        };
        let context = RowBuildContext {
            triage: BTreeSet::from([pinned.pane_instance]),
            ..RowBuildContext::default()
        };
        let (rows, _) =
            build_rows_from_groups(groups, &state, &SidebarPreferences::default(), &context);

        assert!(!rows.iter().any(|row| row.id == "zone::triage"));
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Repo)
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .map(|row| row.pane_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["%2", "%3", "%1"]
        );
    }

    #[test]
    fn all_tree_promotes_pinned_category_then_repository() {
        let make = |pane_id: &str, pid, category: &str, repo: &str, pinned| {
            let mut pane = agent_pane(BadgeState::Idle, "");
            pane.pane_id = pane_id.to_string();
            pane.pane_instance = PaneInstance {
                pane_id: pane_id.to_string(),
                pane_pid: pid,
            };
            pane.category = category.to_string();
            pane.repo = repo.to_string();
            pane.repo_key = crate::category::RepoKey::path(format!("/{category}/{repo}"));
            pane.pinned = pinned;
            pane
        };
        let groups = BTreeMap::from([
            (
                ("alpha".to_string(), "first".to_string()),
                vec![make("%1", 1, "alpha", "first", false)],
            ),
            (
                ("zeta".to_string(), "alpha".to_string()),
                vec![make("%2", 2, "zeta", "alpha", false)],
            ),
            (
                ("zeta".to_string(), "zeta".to_string()),
                vec![make("%3", 3, "zeta", "zeta", true)],
            ),
        ]);
        let state = SidebarState {
            category_scope: CategoryScope::All,
            presentation_mode: PresentationMode::Tree,
            ..SidebarState::default()
        };
        let (rows, _) = build_rows_from_groups(
            groups,
            &state,
            &SidebarPreferences::default(),
            &RowBuildContext::default(),
        );

        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Category)
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Repo)
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha", "first"]
        );
    }

    #[test]
    fn chat_label_omits_empty_task_progress() {
        let pane = agent_pane(BadgeState::Working, "");

        assert_eq!(chat_label(&pane), "Codex (%1)");
    }
}

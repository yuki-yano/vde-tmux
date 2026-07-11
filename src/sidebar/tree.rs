use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::agent::display_agent_name;
use crate::category::resolve_category_for_session;
use crate::config::Config;
use crate::daemon::session_badge::BadgeState;
use crate::git::WorktreeInfo;
use crate::hook::{RollupLevel, TaskItem, TaskItemStatus, WorktreeActivity};
use crate::session::SessionInfo;
use crate::sidebar::state::{SidebarRowRef, SidebarState, StatusFilter, ViewMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarRowKind {
    Zone,
    Category,
    Repo,
    Chat,
    Detail,
    Jump,
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
    pub wait_reason: Option<String>,
    pub elapsed_secs: Option<i64>,
    pub completed_age_secs: Option<i64>,
    pub tasks_done: Option<i64>,
    pub tasks_total: Option<i64>,
    pub subagent_count: Option<usize>,
    pub attention_count: Option<usize>,
    pub origin: Option<String>,
    pub flash: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BadgeCounts {
    pub total: usize,
    pub attention: usize,
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
}

impl BadgeCounts {
    pub fn count_for_filter(self, filter: StatusFilter) -> usize {
        match filter {
            StatusFilter::All => self.total,
            StatusFilter::AttentionOnly => self.attention,
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
    pane_id: String,
    repo: String,
    category: String,
    agent: String,
    prompt: String,
    wait_reason: String,
    started_at: String,
    completed_at: String,
    tasks: String,
    task_items: Vec<TaskItem>,
    subagents: Vec<SubagentDetail>,
    worktree_activity: Option<WorktreeActivity>,
    worktree: Option<WorktreeInfo>,
    rollup: RollupLevel,
    badge_state: BadgeState,
    repo_path: String,
    flash: bool,
    active: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RowBuildContext {
    pub git: BTreeMap<String, crate::git::GitBadge>,
    pub worktrees: BTreeMap<String, crate::git::WorktreeInfo>,
    pub triage: BTreeSet<String>,
    pub flash: BTreeSet<String>,
    pub now: i64,
}

pub fn build_rows_from_presentations(
    config: &Config,
    panes: &[crate::daemon::protocol::v2::PanePresentation],
    state: &SidebarState,
    ctx: &RowBuildContext,
    visible_panes: &BTreeSet<String>,
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
        let repo = repo_label_from_values(&pane.current_path, session_name);
        let category = category_for_values(config, session_name, &pane.current_path, &repo);
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
            .entry((category.clone(), repo.clone()))
            .or_default()
            .push(AgentPane {
                pane_id: pane.pane_instance.pane_id.clone(),
                repo,
                category,
                agent: canonical.agent.as_str().to_string(),
                prompt: canonical
                    .prompt
                    .as_ref()
                    .map(|prompt| prompt.text.clone())
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
                worktree: ctx.worktrees.get(&pane.current_path).cloned(),
                rollup,
                badge_state: resolved.badge,
                repo_path: pane.current_path.clone(),
                flash: ctx.flash.contains(&pane.pane_instance.pane_id),
                active: visible_panes.contains(&pane.pane_instance.pane_id),
            });
    }
    build_rows_from_groups(groups, state, ctx)
}

fn build_rows_from_groups(
    mut groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    ctx: &RowBuildContext,
) -> (Vec<SidebarRow>, BadgeCounts) {
    for panes in groups.values_mut() {
        order_agent_panes(panes, state);
    }
    let group_metas = groups
        .iter()
        .map(|(key, panes)| (key.clone(), group_meta(panes, &ctx.triage)))
        .collect::<BTreeMap<_, _>>();
    let mut triage_panes = Vec::new();
    for panes in groups.values_mut() {
        let mut index = 0;
        while index < panes.len() {
            if ctx.triage.contains(&panes[index].pane_id) {
                triage_panes.push(panes.remove(index));
            } else {
                index += 1;
            }
        }
    }
    order_agent_panes(&mut triage_panes, state);
    let counts = badge_counts_from_agent_panes(
        groups
            .values()
            .flat_map(|panes| panes.iter())
            .chain(triage_panes.iter()),
        &ctx.triage,
    );
    for panes in groups.values_mut() {
        panes.retain(|pane| pane_matches_filter(pane, state.filter));
    }
    groups.retain(|_, panes| !panes.is_empty());

    let mut rows = triage_zone_rows(&triage_panes, state, ctx.now);
    let mut fleet_rows = match state.view_mode {
        ViewMode::Flat => flat_rows(groups, state, ctx.now),
        ViewMode::ByRepo => repo_rows(groups, state, 0, &ctx.git, ctx.now, &group_metas),
        ViewMode::ByCategory => category_rows(groups, state, &ctx.git, ctx.now, &group_metas),
    };
    rows.append(&mut fleet_rows);
    (rows, counts)
}

fn badge_counts_from_agent_panes<'a>(
    panes: impl IntoIterator<Item = &'a AgentPane>,
    triage: &BTreeSet<String>,
) -> BadgeCounts {
    let mut counts = BadgeCounts::default();
    for pane in panes {
        counts.total += 1;
        if pane_matches_attention_filter(pane) || triage.contains(&pane.pane_id) {
            counts.attention += 1;
        }
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
        .filter(|row| {
            !matches!(
                row.kind,
                SidebarRowKind::Detail | SidebarRowKind::Jump | SidebarRowKind::Zone
            )
        })
        .map(|row| SidebarRowRef::new(row.id.clone()))
        .collect()
}

pub(crate) fn chat_row_id(pane_id: &str) -> String {
    format!("chat::{pane_id}")
}

fn category_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    git: &BTreeMap<String, crate::git::GitBadge>,
    now: i64,
    metas: &BTreeMap<(String, String), RowMeta>,
) -> Vec<SidebarRow> {
    let mut by_category: BTreeMap<String, BTreeMap<String, Vec<AgentPane>>> = BTreeMap::new();
    for ((category, repo), panes) in groups {
        by_category.entry(category).or_default().insert(repo, panes);
    }

    let mut rows = Vec::new();
    for (category, repos) in by_category {
        let category_id = format!("category::{category}");
        let all_panes = repos.values().flatten().cloned().collect::<Vec<_>>();
        let active = all_panes.iter().any(|pane| pane.active);
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
            label: category,
            chat_count: all_panes.len(),
            rollup: rollup(&all_panes),
            badge_state: badge_rollup(&all_panes),
            expanded,
            pane_id: None,
            git: None,
            active,
            meta: Some(RowMeta {
                attention_count: Some(attention_count),
                ..RowMeta::default()
            }),
        });
        if expanded {
            rows.extend(repo_rows_from_map(repos, state, 1, git, now, metas));
        }
    }
    rows
}

fn repo_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    depth: usize,
    git: &BTreeMap<String, crate::git::GitBadge>,
    now: i64,
    metas: &BTreeMap<(String, String), RowMeta>,
) -> Vec<SidebarRow> {
    let mut repos = BTreeMap::new();
    for ((category, repo), panes) in groups {
        repos.insert((category, repo), panes);
    }
    repo_rows_from_keyed_map(repos, state, depth, git, now, metas)
}

fn repo_rows_from_map(
    repos: BTreeMap<String, Vec<AgentPane>>,
    state: &SidebarState,
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
                .unwrap_or_else(|| "misc".to_string());
            ((category, repo), panes)
        })
        .collect();
    repo_rows_from_keyed_map(keyed, state, depth, git, now, metas)
}

fn repo_rows_from_keyed_map(
    repos: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    depth: usize,
    git: &BTreeMap<String, crate::git::GitBadge>,
    now: i64,
    metas: &BTreeMap<(String, String), RowMeta>,
) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    let mut groups = repos.into_values().collect::<Vec<_>>();
    order_repo_groups(&mut groups, state);
    for panes in groups {
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
            badge_state: badge_rollup(&panes),
            expanded,
            pane_id: None,
            git: git.get(&first.repo_path).cloned(),
            active: panes.iter().any(|pane| pane.active),
            meta: Some(
                metas
                    .get(&(first.category.clone(), first.repo.clone()))
                    .cloned()
                    .unwrap_or_else(|| group_meta(&panes, &BTreeSet::new())),
            ),
        });
        if expanded {
            for pane in &panes {
                push_chat_row(pane, depth + 1, state, now, &mut rows);
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
        let id = format!("chat::{}", pane.pane_id);
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

fn flat_rows(
    groups: BTreeMap<(String, String), Vec<AgentPane>>,
    state: &SidebarState,
    now: i64,
) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    for pane in groups.values().flat_map(|panes| panes.iter()) {
        push_chat_row(pane, 0, state, now, &mut rows);
    }
    rows
}

fn push_chat_row(
    pane: &AgentPane,
    depth: usize,
    state: &SidebarState,
    now: i64,
    rows: &mut Vec<SidebarRow>,
) {
    let id = format!("chat::{}", pane.pane_id);
    let expanded = state.is_expanded_with_default(&id, false);
    let meta = chat_meta(pane, now);
    let label = if expanded {
        expanded_chat_label(pane)
    } else {
        chat_label(pane)
    };
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
        id: format!("detail::{}::{suffix}", pane.pane_id),
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
    if let Some(worktree) = &pane.worktree {
        rows.push(detail_row(
            pane,
            depth,
            "worktree",
            format!("+ {}", sanitize_detail_label(&worktree.name)),
        ));
    }
    if let Some(prompt) = non_empty(&pane.prompt) {
        rows.push(detail_row(pane, depth, "prompt", prompt.to_string()));
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
    rows.push(SidebarRow {
        id: format!("jump::{}", pane.pane_id),
        kind: SidebarRowKind::Jump,
        depth,
        label: "jump".to_string(),
        chat_count: 0,
        rollup: pane.rollup,
        badge_state: Some(pane.badge_state),
        expanded: true,
        pane_id: Some(pane.pane_id.clone()),
        git: None,
        active: pane.active,
        meta: None,
    });
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
    }
}

fn group_meta(panes: &[AgentPane], triage: &BTreeSet<String>) -> RowMeta {
    RowMeta {
        attention_count: Some(
            panes
                .iter()
                .filter(|pane| {
                    pane_matches_attention_filter(pane) || triage.contains(&pane.pane_id)
                })
                .count(),
        ),
        ..RowMeta::default()
    }
}

pub fn humanize_secs(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
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
    if let Some((done, total)) = parse_tasks(&pane.tasks) {
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
    pane.badge_state == BadgeState::Blocked
}

fn order_repo_groups(groups: &mut [Vec<AgentPane>], state: &SidebarState) {
    let position = |panes: &Vec<AgentPane>| -> usize {
        let Some(first) = panes.first() else {
            return usize::MAX;
        };
        state
            .manual_order
            .iter()
            .position(|repo| repo.category == first.category && repo.repo == first.repo)
            .unwrap_or(usize::MAX)
    };
    groups.sort_by(|left, right| {
        position(left).cmp(&position(right)).then_with(|| {
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

fn order_agent_panes(panes: &mut [AgentPane], state: &SidebarState) {
    panes.sort_by(|left, right| compare_agent_panes(left, right, state));
}

fn compare_agent_panes(
    left: &AgentPane,
    right: &AgentPane,
    state: &SidebarState,
) -> std::cmp::Ordering {
    let manual_position = |pane: &AgentPane| {
        state
            .manual_chat_order
            .iter()
            .position(|pane_id| pane_id == &pane.pane_id)
            .unwrap_or(usize::MAX)
    };
    chat_sort_bucket(left)
        .cmp(&chat_sort_bucket(right))
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

fn badge_rollup(panes: &[AgentPane]) -> Option<BadgeState> {
    panes.iter().map(|pane| pane.badge_state).min()
}

fn category_for_values(config: &Config, session_name: &str, path: &str, repo: &str) -> String {
    let session = SessionInfo {
        name: session_name.to_string(),
        project_path: path.to_string(),
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

fn repo_label_from_values(path: &str, session_name: &str) -> String {
    let path = path.trim_end_matches('/');
    let repo = path
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(session_name);
    let repo = repo.trim();
    if repo.is_empty() {
        "repo".to_string()
    } else {
        repo.replace('\u{1f}', " ")
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

    fn agent_pane(badge_state: BadgeState, completed_at: &str) -> AgentPane {
        AgentPane {
            pane_id: "%1".to_string(),
            repo: "repo".to_string(),
            category: "misc".to_string(),
            agent: "codex".to_string(),
            prompt: String::new(),
            wait_reason: String::new(),
            started_at: "100".to_string(),
            completed_at: completed_at.to_string(),
            tasks: "0/0".to_string(),
            task_items: Vec::new(),
            subagents: Vec::new(),
            worktree_activity: None,
            worktree: None,
            rollup: RollupLevel::Idle,
            badge_state,
            repo_path: "/tmp/repo".to_string(),
            flash: false,
            active: false,
        }
    }

    #[test]
    fn humanize_secs_formats_by_magnitude() {
        assert_eq!(humanize_secs(0), "0s");
        assert_eq!(humanize_secs(45), "45s");
        assert_eq!(humanize_secs(60), "1m");
        assert_eq!(humanize_secs(12 * 60 + 30), "12m");
        assert_eq!(humanize_secs(90 * 60), "1h30m");
        assert_eq!(humanize_secs(10 * 3600), "10h");
        assert_eq!(humanize_secs(38 * 3600 + 59 * 60), "38h");
        assert_eq!(humanize_secs(48 * 3600), "2d");
        assert_eq!(humanize_secs(100 * 3600), "4d");
        assert_eq!(humanize_secs(-5), "0s");
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
                id: "jump::%1".to_string(),
                kind: SidebarRowKind::Jump,
                depth: 1,
                label: "jump".to_string(),
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
            &RowBuildContext::default(),
            &BTreeSet::new(),
        );

        assert!(rows.is_empty());
        assert_eq!(counts, BadgeCounts::default());
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
    }
}

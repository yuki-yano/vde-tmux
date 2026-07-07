use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::category::resolve_category_for_session;
use crate::config::Config;
use crate::daemon::session_badge::{BadgeState, badge_state};
use crate::hook::{AgentStatus, RollupLevel, pane_rollup_level};
use crate::options::snapshot::{PaneSnapshot, effective_agent, is_live_agent_pane};
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
    session: String,
    pane_id: String,
    repo: String,
    category: String,
    agent: String,
    prompt: String,
    wait_reason: String,
    started_at: String,
    completed_at: String,
    tasks: String,
    subagents: String,
    rollup: RollupLevel,
    badge_state: BadgeState,
    repo_path: String,
    attention: bool,
    flash: bool,
    active: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RowBuildContext {
    pub git: BTreeMap<String, crate::git::GitBadge>,
    pub unread: BTreeMap<String, bool>,
    pub triage: BTreeSet<String>,
    pub flash: BTreeSet<String>,
    pub now: i64,
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
    build_rows_with_git_and_unread(config, panes, state, git, &BTreeMap::new())
}

pub fn build_rows_with_git_and_unread(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
    git: &BTreeMap<String, crate::git::GitBadge>,
    unread: &BTreeMap<String, bool>,
) -> Vec<SidebarRow> {
    build_rows_at_with_git_and_unread(config, panes, state, git, unread, now_epoch_secs())
}

pub fn build_rows_at(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
    now: i64,
) -> Vec<SidebarRow> {
    build_rows_at_with_git_and_unread(
        config,
        panes,
        state,
        &BTreeMap::new(),
        &BTreeMap::new(),
        now,
    )
}

pub fn build_rows_at_with_git(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
    git: &BTreeMap<String, crate::git::GitBadge>,
    now: i64,
) -> Vec<SidebarRow> {
    build_rows_at_with_git_and_unread(config, panes, state, git, &BTreeMap::new(), now)
}

pub fn build_rows_at_with_git_and_unread(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
    git: &BTreeMap<String, crate::git::GitBadge>,
    unread: &BTreeMap<String, bool>,
    now: i64,
) -> Vec<SidebarRow> {
    build_rows_ctx(
        config,
        panes,
        state,
        &RowBuildContext {
            git: git.clone(),
            unread: unread.clone(),
            triage: BTreeSet::new(),
            flash: BTreeSet::new(),
            now,
        },
    )
    .0
}

pub fn build_rows_ctx(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
    ctx: &RowBuildContext,
) -> (Vec<SidebarRow>, BadgeCounts) {
    let mut groups: BTreeMap<(String, String), Vec<AgentPane>> = BTreeMap::new();
    for pane in panes {
        if !is_live_agent_pane(pane) {
            continue;
        }
        let agent = effective_agent(pane).unwrap_or_default().to_string();
        let repo = repo_label(pane);
        let category = category_for_pane(config, pane, &repo);
        let rollup = rollup_for_pane(pane);
        let unread = ctx.unread.get(&pane.pane_id).copied().unwrap_or(false);
        groups
            .entry((category.clone(), repo.clone()))
            .or_default()
            .push(AgentPane {
                session: pane.session.clone(),
                pane_id: pane.pane_id.clone(),
                repo,
                category,
                agent,
                prompt: pane.prompt.clone(),
                wait_reason: pane.wait_reason.clone(),
                started_at: pane.started_at.clone(),
                completed_at: pane.completed_at.clone(),
                tasks: pane.tasks.clone(),
                subagents: pane.subagents.clone(),
                rollup,
                badge_state: badge_state(rollup, unread),
                repo_path: pane.current_path.clone(),
                attention: pane.attention == "1",
                flash: ctx.flash.contains(&pane.pane_id),
                active: pane.window_active && pane.session_attached,
            });
    }
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
) -> BadgeCounts {
    let mut counts = BadgeCounts::default();
    for pane in panes {
        counts.total += 1;
        if pane_matches_attention_filter(pane) {
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
                format!("{} · {}", pane.agent, pane.repo)
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
    if let Some(prompt) = non_empty(&pane.prompt) {
        rows.push(detail_row(pane, depth, "prompt", prompt.to_string()));
    }
    rows.push(detail_row(
        pane,
        depth,
        "place",
        format!("{} · {}", pane.session, pane.pane_id),
    ));
    let subagents = decode_subagents(&pane.subagents);
    if let Some(last_index) = subagents.len().checked_sub(1) {
        for (index, (agent_id, agent_type)) in subagents.iter().enumerate() {
            let connector = if index == last_index {
                "\u{2514}"
            } else {
                "\u{251c}"
            };
            rows.push(detail_row(
                pane,
                depth,
                &format!("subagent::{index}"),
                format!("{connector} {agent_type}{}", subagent_id_suffix(agent_id)),
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

fn expanded_chat_label(pane: &AgentPane) -> String {
    pane.agent.clone()
}

fn chat_meta(pane: &AgentPane, now: i64) -> RowMeta {
    let tasks = parse_tasks(&pane.tasks);
    RowMeta {
        agent: Some(pane.agent.clone()),
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
        subagent_count: Some(decode_subagents(&pane.subagents).len()),
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
                    pane.badge_state == BadgeState::Blocked || triage.contains(&pane.pane_id)
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

fn decode_subagents(raw: &str) -> Vec<(String, String)> {
    raw.split('|')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            entry
                .split_once(':')
                .map(|(id, agent_type)| (id.to_string(), agent_type.to_string()))
        })
        .collect()
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
    let base = if let Some(prompt) = non_empty(&pane.prompt) {
        format!("{}: {prompt}", pane.agent)
    } else {
        format!("{} ({})", pane.agent, pane.pane_id)
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
    pane.completed_at.trim().is_empty()
        && (pane.attention
            || pane.badge_state == BadgeState::Blocked
            || pane.badge_state == BadgeState::Working)
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
    right
        .attention
        .cmp(&left.attention)
        .then_with(|| left.rollup.cmp(&right.rollup))
        .then_with(|| manual_position(left).cmp(&manual_position(right)))
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

fn badge_rollup(panes: &[AgentPane]) -> Option<BadgeState> {
    panes.iter().map(|pane| pane.badge_state).min()
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

pub(crate) fn rollup_for_pane(pane: &PaneSnapshot) -> RollupLevel {
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
            current_command: agent.to_string(),
            agent: agent.to_string(),
            status: status.to_string(),
            ..PaneSnapshot::default()
        }
    }

    fn category_rule(category: &str, pattern: &str) -> CategoryRule {
        CategoryRule {
            category: category.to_string(),
            path_patterns: vec![pattern.to_string()],
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
        let rows = vec![
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

        let refs = row_refs(&rows);

        assert_eq!(refs, vec![SidebarRowRef::new("repo::misc::app")]);
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
    fn build_rows_ignores_stale_hook_agent_when_command_is_not_agent() {
        let mut hook_marked = pane("main", "%1", "/tmp/app", "codex", "running");
        hook_marked.current_command = "node".to_string();

        let rows = build_rows(&Config::default(), &[hook_marked], &SidebarState::default());

        assert!(rows.is_empty());
    }

    #[test]
    fn build_rows_uses_command_agent_when_hook_options_are_missing() {
        let mut command_agent = pane("main", "%1", "/tmp/app", "", "");
        command_agent.current_command = "claude".to_string();

        let rows = build_rows(
            &Config::default(),
            &[command_agent],
            &SidebarState::default(),
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1].meta.as_ref().and_then(|meta| meta.agent.as_deref()),
            Some("claude")
        );
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
    fn active_pane_marks_chat_row_and_ancestors() {
        let mut config = Config::default();
        config
            .categories
            .rules
            .push(category_rule("active", "/tmp/active/*"));
        config
            .categories
            .rules
            .push(category_rule("idle", "/tmp/idle/*"));
        let mut state = SidebarState {
            view_mode: ViewMode::ByCategory,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");
        let mut active = pane("main", "%1", "/tmp/active/app", "codex", "running");
        active.window_active = true;
        active.session_attached = true;
        let inactive = pane("main", "%2", "/tmp/idle/other", "codex", "running");

        let rows = build_rows(&config, &[active, inactive], &state);

        assert!(
            rows.iter()
                .find(|row| row.id == "category::active")
                .unwrap()
                .active
        );
        assert!(
            rows.iter()
                .find(|row| row.id == "repo::active::app")
                .unwrap()
                .active
        );
        assert!(rows.iter().find(|row| row.id == "chat::%1").unwrap().active);
        assert!(
            rows.iter()
                .find(|row| row.id == "detail::%1::place")
                .unwrap()
                .active
        );
        assert!(rows.iter().find(|row| row.id == "jump::%1").unwrap().active);
        assert!(
            !rows
                .iter()
                .find(|row| row.id == "category::idle")
                .unwrap()
                .active
        );
        assert!(
            !rows
                .iter()
                .find(|row| row.id == "repo::idle::other")
                .unwrap()
                .active
        );
        assert!(!rows.iter().find(|row| row.id == "chat::%2").unwrap().active);
    }

    #[test]
    fn detached_session_is_not_active() {
        let mut pane = pane("main", "%1", "/tmp/app", "codex", "running");
        pane.window_active = true;
        pane.session_attached = false;

        let rows = build_rows(&Config::default(), &[pane], &SidebarState::default());

        assert!(!rows.iter().any(|row| row.active));
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

    #[test]
    fn chat_detail_rows_are_hidden_by_default_and_shown_when_toggled_open() {
        let mut agent = pane("main", "%5", "/tmp/app", "codex", "running");
        agent.prompt = "fix the bug".to_string();
        agent.started_at = "1000".to_string();

        let rows = build_rows_at(
            &Config::default(),
            &[agent.clone()],
            &SidebarState::default(),
            1075,
        );

        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Detail)
                .count(),
            0
        );
        assert!(
            !rows
                .iter()
                .find(|row| row.id == "chat::%5")
                .unwrap()
                .expanded
        );

        let mut state = SidebarState::default();
        state.toggle_expanded("chat::%5");
        let rows = build_rows_at(&Config::default(), &[agent], &state, 1075);

        assert!(
            rows.iter()
                .any(|row| row.kind == SidebarRowKind::Detail && row.label == "fix the bug")
        );
        assert!(
            rows.iter()
                .any(|row| row.kind == SidebarRowKind::Chat && row.label == "codex")
        );
        assert!(!rows.iter().any(|row| row.id == "detail::%5::state"));
        assert!(
            rows.iter()
                .any(|row| row.kind == SidebarRowKind::Detail && row.label == "main · %5")
        );
        assert_eq!(rows.last().unwrap().kind, SidebarRowKind::Jump);
    }

    #[test]
    fn expanded_chat_row_shows_agent_without_inline_state() {
        let mut agent = pane("main", "%1", "/tmp/app", "claude", "running");
        agent.prompt = "review the long plan".to_string();
        agent.started_at = "925".to_string();
        let collapsed_state = SidebarState {
            view_mode: ViewMode::Flat,
            ..SidebarState::default()
        };
        let collapsed = build_rows_at(&Config::default(), &[agent.clone()], &collapsed_state, 1000);
        let collapsed_chat = collapsed
            .iter()
            .find(|row| row.id == "chat::%1")
            .expect("collapsed chat row");

        assert_eq!(collapsed_chat.label, "claude: review the long plan");

        let mut expanded_state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        expanded_state.toggle_expanded("chat::%1");
        let expanded = build_rows_at(&Config::default(), &[agent.clone()], &expanded_state, 1000);
        let expanded_chat = expanded
            .iter()
            .find(|row| row.id == "chat::%1")
            .expect("expanded chat row");

        assert_eq!(expanded_chat.label, "claude");

        let triage_ctx = RowBuildContext {
            triage: BTreeSet::from(["%1".to_string()]),
            now: 1000,
            ..RowBuildContext::default()
        };
        let triage = build_rows_ctx(&Config::default(), &[agent], &expanded_state, &triage_ctx).0;
        let triage_chat = triage
            .iter()
            .find(|row| row.id == "chat::%1")
            .expect("triage chat row");

        assert_eq!(triage_chat.label, "claude");
    }

    #[test]
    fn selected_chat_row_does_not_auto_expand_full_detail_rows() {
        let mut agent = pane("main", "%1", "/tmp/app", "codex", "running");
        agent.prompt = "fix bug".to_string();
        let state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };

        let rows = build_rows_at(&Config::default(), &[agent], &state, 1000);

        let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
        assert!(!chat.expanded);
        assert!(!rows.iter().any(|row| row.id == "detail::%1::prompt"));
        assert!(!rows.iter().any(|row| row.id == "jump::%1"));
    }

    #[test]
    fn manually_expanded_chat_row_expands_full_detail_rows() {
        let mut agent = pane("main", "%1", "/tmp/app", "codex", "running");
        agent.prompt = "fix bug".to_string();
        agent.started_at = "185".to_string();
        agent.tasks = "2/5".to_string();
        agent.subagents = "sub1:Explore|ab12:general-purpose".to_string();
        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");

        let rows = build_rows_at(&Config::default(), &[agent], &state, 1000);

        assert!(!rows.iter().any(|row| row.id == "meta::%1"));
        assert!(rows.iter().any(|row| row.id == "detail::%1::prompt"));
        assert!(!rows.iter().any(|row| row.id == "detail::%1::state"));
        assert!(rows.iter().any(|row| row.id == "detail::%1::place"));
        assert!(rows.iter().any(|row| row.id == "jump::%1"));
    }

    #[test]
    fn expanded_chat_row_carries_state_and_place_detail_remains() {
        let mut agent = pane("vde-tmux", "%1", "/tmp/app", "codex", "running");
        agent.prompt = "fix bug".to_string();
        agent.started_at = "280".to_string();
        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");

        let rows = build_rows_at(&Config::default(), &[agent], &state, 1000);

        assert_eq!(
            rows.iter()
                .find(|row| row.id == "chat::%1")
                .map(|row| row.label.as_str()),
            Some("codex")
        );
        assert!(!rows.iter().any(|row| row.id == "detail::%1::state"));
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "detail::%1::place")
                .map(|row| row.label.as_str()),
            Some("vde-tmux · %1")
        );
        assert!(!rows.iter().any(|row| row.id == "detail::%1::status"));
        assert!(!rows.iter().any(|row| row.id == "detail::%1::elapsed"));
        assert!(!rows.iter().any(|row| row.id == "detail::%1::session"));
    }

    #[test]
    fn idle_state_line_uses_completed_at() {
        let mut done = pane("main", "%1", "/tmp/app", "codex", "idle");
        done.completed_at = (1000 - 38 * 3600).to_string();
        let mut missing = pane("main", "%2", "/tmp/app", "claude", "idle");
        missing.completed_at.clear();
        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");

        let rows = build_rows_at(&Config::default(), &[done], &state, 1000);
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "chat::%1")
                .map(|row| row.label.as_str()),
            Some("codex")
        );
        assert!(!rows.iter().any(|row| row.id == "detail::%1::state"));

        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%2".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%2");
        let rows = build_rows_at(&Config::default(), &[missing], &state, 1000);
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "chat::%2")
                .map(|row| row.label.as_str()),
            Some("claude")
        );
        assert!(!rows.iter().any(|row| row.id == "detail::%2::state"));
    }

    #[test]
    fn blocked_state_line_keeps_wait_reason() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        blocked.started_at = "880".to_string();
        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");

        let rows = build_rows_at(&Config::default(), &[blocked], &state, 1000);

        assert_eq!(
            rows.iter()
                .find(|row| row.id == "chat::%1")
                .map(|row| row.label.as_str()),
            Some("codex")
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "chat::%1")
                .and_then(|row| row.meta.as_ref())
                .and_then(|meta| meta.wait_reason.as_deref()),
            Some("permission_prompt")
        );
        assert!(!rows.iter().any(|row| row.id == "detail::%1::state"));
    }

    #[test]
    fn running_state_line_appends_tasks_progress() {
        let mut with_tasks = pane("main", "%1", "/tmp/app", "codex", "running");
        with_tasks.started_at = "280".to_string();
        with_tasks.tasks = "3/5".to_string();
        let mut zero_total = pane("main", "%2", "/tmp/app", "claude", "running");
        zero_total.started_at = "280".to_string();
        zero_total.tasks = "0/0".to_string();

        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");
        let rows = build_rows_at(&Config::default(), &[with_tasks], &state, 1000);
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "chat::%1")
                .map(|row| row.label.as_str()),
            Some("codex")
        );
        assert!(!rows.iter().any(|row| row.id == "detail::%1::state"));

        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%2".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%2");
        let rows = build_rows_at(&Config::default(), &[zero_total], &state, 1000);
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "chat::%2")
                .map(|row| row.label.as_str()),
            Some("claude")
        );
        assert!(!rows.iter().any(|row| row.id == "detail::%2::state"));
    }

    #[test]
    fn unselected_or_expanded_chat_rows_have_no_meta_row() {
        let agent = pane("main", "%1", "/tmp/app", "codex", "running");
        let state = SidebarState {
            view_mode: ViewMode::Flat,
            ..SidebarState::default()
        };
        let rows = build_rows_at(
            &Config::default(),
            std::slice::from_ref(&agent),
            &state,
            1000,
        );
        assert!(!rows.iter().any(|row| row.id == "meta::%1"));

        let mut expanded = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        expanded.toggle_expanded("chat::%1");
        let rows = build_rows_at(&Config::default(), &[agent], &expanded, 1000);
        assert!(!rows.iter().any(|row| row.id == "meta::%1"));
        assert!(rows.iter().any(|row| row.id == "jump::%1"));
    }

    #[test]
    fn chat_detail_rows_include_running_subagents_with_tree_connectors() {
        let mut agent = pane("main", "%5", "/tmp/app", "claude", "running");
        agent.subagents = "sub12345:Explore|ab120000:general-purpose".to_string();
        let mut state = SidebarState::default();
        state.toggle_expanded("chat::%5");

        let rows = build_rows_at(&Config::default(), &[agent], &state, 1075);
        let labels = rows
            .iter()
            .filter(|row| {
                row.kind == SidebarRowKind::Detail
                    && (row.label.starts_with('\u{251c}') || row.label.starts_with('\u{2514}'))
            })
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            labels,
            vec!["\u{251c} Explore #sub1", "\u{2514} general-purpose #ab12"]
        );
    }

    #[test]
    fn chat_detail_subagent_rows_appear_before_jump_row() {
        let mut agent = pane("main", "%5", "/tmp/app", "claude", "running");
        agent.subagents = "sub12345:Explore".to_string();
        let mut state = SidebarState::default();
        state.toggle_expanded("chat::%5");

        let rows = build_rows_at(&Config::default(), &[agent], &state, 1075);
        let subagent_index = rows
            .iter()
            .position(|row| row.label == "\u{2514} Explore #sub1")
            .expect("subagent row should exist");
        let jump_index = rows
            .iter()
            .position(|row| row.kind == SidebarRowKind::Jump)
            .expect("jump row should exist");

        assert!(subagent_index < jump_index);
    }

    #[test]
    fn chat_detail_omits_subagent_section_when_no_subagents_running() {
        let agent = pane("main", "%5", "/tmp/app", "claude", "running");
        let mut state = SidebarState::default();
        state.toggle_expanded("chat::%5");

        let rows = build_rows_at(&Config::default(), &[agent], &state, 1075);

        assert!(!rows.iter().any(|row| {
            row.kind == SidebarRowKind::Detail
                && (row.label.starts_with('\u{251c}') || row.label.starts_with('\u{2514}'))
        }));
    }

    #[test]
    fn attention_only_filter_drops_calm_panes_and_empty_groups() {
        let mut calm = pane("main", "%1", "/tmp/calm", "codex", "idle");
        calm.attention = "0".to_string();
        let running = pane("main", "%2", "/tmp/active", "codex", "running");
        let mut attention = pane("main", "%3", "/tmp/active", "claude", "waiting");
        attention.wait_reason = "permission_prompt".to_string();
        let state = SidebarState {
            filter: crate::sidebar::state::StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };

        let rows = build_rows(&Config::default(), &[calm, running, attention], &state);

        assert!(rows.iter().all(|row| !row.id.contains("%1")));
        assert!(rows.iter().any(|row| row.id.contains("%2")));
        assert!(rows.iter().any(|row| row.id.contains("%3")));
        assert!(
            rows.iter()
                .any(|row| row.kind == SidebarRowKind::Repo && row.label == "active")
        );
    }

    #[test]
    fn attention_only_filter_drops_completed_attention_panes() {
        let mut completed = pane("main", "%1", "/tmp/done", "codex", "idle");
        completed.attention = "1".to_string();
        completed.completed_at = "900".to_string();
        let state = SidebarState {
            filter: crate::sidebar::state::StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };

        let rows = build_rows_at(&Config::default(), &[completed], &state, 1000);

        assert!(rows.iter().all(|row| row.id != "chat::%1"));
        assert!(rows.iter().all(|row| row.kind != SidebarRowKind::Repo));
    }

    #[test]
    fn working_done_idle_filters_partition_fleet_panes() {
        let working = pane("main", "%1", "/tmp/app", "codex", "running");
        let mut done = pane("main", "%2", "/tmp/app", "claude", "idle");
        done.window_active = false;
        done.session_attached = false;
        let idle = pane("main", "%3", "/tmp/app", "opencode", "idle");

        for (filter, expected) in [
            (crate::sidebar::state::StatusFilter::WorkingOnly, "%1"),
            (crate::sidebar::state::StatusFilter::DoneOnly, "%2"),
            (crate::sidebar::state::StatusFilter::IdleOnly, "%3"),
        ] {
            let state = SidebarState {
                view_mode: ViewMode::Flat,
                filter,
                ..SidebarState::default()
            };

            let rows = build_rows_ctx(
                &Config::default(),
                &[working.clone(), done.clone(), idle.clone()],
                &state,
                &RowBuildContext {
                    unread: BTreeMap::from([("%2".to_string(), true)]),
                    ..RowBuildContext::default()
                },
            )
            .0;

            assert_eq!(rows.len(), 1, "{filter:?}");
            assert_eq!(rows[0].pane_id.as_deref(), Some(expected), "{filter:?}");
        }
    }

    #[test]
    fn repo_row_badge_state_is_minimum_of_children() {
        let mut done = pane("main", "%1", "/tmp/app", "codex", "idle");
        done.window_active = false;
        done.session_attached = false;
        let mut blocked = pane("main", "%2", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let unread = BTreeMap::from([("%1".to_string(), true)]);

        let rows = build_rows_at_with_git_and_unread(
            &Config::default(),
            &[done, blocked],
            &SidebarState::default(),
            &BTreeMap::new(),
            &unread,
            1000,
        );

        let repo = rows
            .iter()
            .find(|row| row.kind == SidebarRowKind::Repo)
            .unwrap();
        assert_eq!(
            repo.badge_state,
            Some(crate::daemon::session_badge::BadgeState::Blocked)
        );
    }

    #[test]
    fn chat_rows_carry_row_meta() {
        let mut agent = pane("main", "%1", "/tmp/app", "codex", "running");
        agent.prompt = "fix bug".to_string();
        agent.started_at = "925".to_string();
        agent.tasks = "2/5".to_string();
        agent.subagents = "sub1:Explore|ab12:general-purpose".to_string();
        let state = SidebarState {
            view_mode: ViewMode::Flat,
            ..SidebarState::default()
        };

        let rows = build_rows_at(&Config::default(), &[agent], &state, 1000);

        let chat = rows
            .iter()
            .find(|row| row.kind == SidebarRowKind::Chat)
            .expect("chat row");
        let meta = chat.meta.as_ref().expect("chat meta");
        assert_eq!(meta.agent.as_deref(), Some("codex"));
        assert_eq!(meta.prompt.as_deref(), Some("fix bug"));
        assert_eq!(meta.elapsed_secs, Some(75));
        assert_eq!(meta.tasks_done, Some(2));
        assert_eq!(meta.tasks_total, Some(5));
        assert_eq!(meta.subagent_count, Some(2));
    }

    #[test]
    fn completed_chat_rows_carry_completed_age_meta() {
        let mut agent = pane("main", "%1", "/tmp/app", "codex", "idle");
        agent.completed_at = "925".to_string();
        let state = SidebarState {
            view_mode: ViewMode::Flat,
            ..SidebarState::default()
        };

        let rows = build_rows_at(&Config::default(), &[agent], &state, 1000);

        let chat = rows
            .iter()
            .find(|row| row.kind == SidebarRowKind::Chat)
            .expect("chat row");
        let meta = chat.meta.as_ref().expect("chat meta");
        assert_eq!(meta.completed_age_secs, Some(75));
    }

    #[test]
    fn repo_rows_carry_blocked_count_in_meta() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let running = pane("main", "%2", "/tmp/app", "claude", "running");
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };

        let rows = build_rows_at(&Config::default(), &[blocked, running], &state, 1000);

        let repo = rows
            .iter()
            .find(|row| row.kind == SidebarRowKind::Repo)
            .expect("repo row");
        assert_eq!(
            repo.meta.as_ref().and_then(|meta| meta.attention_count),
            Some(1)
        );
    }

    #[test]
    fn blocked_panes_move_to_triage_zone_on_top() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let running = pane("main", "%2", "/tmp/app", "claude", "running");
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };
        let ctx = RowBuildContext {
            triage: BTreeSet::from(["%1".to_string()]),
            now: 1000,
            ..RowBuildContext::default()
        };

        let rows = build_rows_ctx(&Config::default(), &[blocked, running], &state, &ctx).0;

        assert_eq!(rows[0].id, "zone::triage");
        assert_eq!(rows[0].chat_count, 1);
        assert_eq!(rows[1].id, "chat::%1");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].label, "codex · app");
        assert!(!rows[2..].iter().any(|row| row.id == "chat::%1"));
    }

    #[test]
    fn triage_zone_is_absent_when_empty() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };

        let rows = build_rows_ctx(
            &Config::default(),
            &[blocked],
            &state,
            &RowBuildContext {
                now: 1000,
                ..RowBuildContext::default()
            },
        )
        .0;

        assert!(rows.iter().all(|row| row.kind != SidebarRowKind::Zone));
    }

    #[test]
    fn triage_ignores_attention_filter() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let idle = pane("main", "%2", "/tmp/app", "claude", "idle");
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            filter: crate::sidebar::state::StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };
        let ctx = RowBuildContext {
            triage: BTreeSet::from(["%1".to_string()]),
            now: 1000,
            ..RowBuildContext::default()
        };

        let rows = build_rows_ctx(&Config::default(), &[blocked, idle], &state, &ctx).0;

        assert!(rows.iter().any(|row| row.id == "chat::%1"));
        assert!(rows.iter().all(|row| row.id != "chat::%2"));
        assert_eq!(
            rows.first().map(|row| row.id.as_str()),
            Some("zone::triage")
        );
    }

    #[test]
    fn counts_are_computed_before_filter_and_include_triage() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let working = pane("main", "%2", "/tmp/app", "claude", "running");
        let idle_a = pane("main", "%3", "/tmp/app", "opencode", "idle");
        let idle_b = pane("main", "%4", "/tmp/app", "claude", "idle");
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            filter: crate::sidebar::state::StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };
        let ctx = RowBuildContext {
            triage: BTreeSet::from(["%1".to_string()]),
            now: 1000,
            ..RowBuildContext::default()
        };

        let (rows, counts) = build_rows_ctx(
            &Config::default(),
            &[blocked, working, idle_a, idle_b],
            &state,
            &ctx,
        );

        assert!(rows.iter().any(|row| row.id == "chat::%1"));
        assert!(rows.iter().any(|row| row.id == "chat::%2"));
        assert!(rows.iter().all(|row| row.id != "chat::%3"));
        assert!(rows.iter().all(|row| row.id != "chat::%4"));
        assert_eq!(counts.total, 4);
        assert_eq!(counts.attention, 2);
        assert_eq!(counts.blocked, 1);
        assert_eq!(counts.working, 1);
        assert_eq!(counts.done, 0);
        assert_eq!(counts.idle, 2);
    }

    #[test]
    fn attention_count_matches_attention_filter_predicate_without_blocked_panes() {
        let working_a = pane("main", "%1", "/tmp/app", "codex", "running");
        let working_b = pane("main", "%2", "/tmp/app", "claude", "running");
        let idle = pane("main", "%3", "/tmp/app", "opencode", "idle");
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            filter: crate::sidebar::state::StatusFilter::AttentionOnly,
            ..SidebarState::default()
        };

        let (rows, counts) = build_rows_ctx(
            &Config::default(),
            &[working_a, working_b, idle],
            &state,
            &RowBuildContext {
                now: 1000,
                ..RowBuildContext::default()
            },
        );

        assert_eq!(counts.blocked, 0);
        assert_eq!(counts.working, 2);
        assert_eq!(counts.attention, 2);
        assert_eq!(
            counts.count_for_filter(crate::sidebar::state::StatusFilter::AttentionOnly),
            2
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == SidebarRowKind::Chat)
                .count(),
            2
        );
        assert!(rows.iter().any(|row| row.id == "chat::%1"));
        assert!(rows.iter().any(|row| row.id == "chat::%2"));
        assert!(rows.iter().all(|row| row.id != "chat::%3"));
    }

    #[test]
    fn badge_counts_filter_availability_uses_filter_counts() {
        let counts = BadgeCounts {
            total: 2,
            attention: 0,
            blocked: 0,
            working: 2,
            done: 0,
            idle: 0,
        };

        assert!(counts.filter_is_available(crate::sidebar::state::StatusFilter::All));
        assert!(!counts.filter_is_available(crate::sidebar::state::StatusFilter::AttentionOnly));
        assert!(counts.filter_is_available(crate::sidebar::state::StatusFilter::WorkingOnly));
        assert!(!counts.filter_is_available(crate::sidebar::state::StatusFilter::DoneOnly));
        assert!(!counts.filter_is_available(crate::sidebar::state::StatusFilter::IdleOnly));
    }

    #[test]
    fn repo_attention_count_includes_triaged_panes() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let running = pane("main", "%2", "/tmp/app", "claude", "running");
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };
        let ctx = RowBuildContext {
            triage: BTreeSet::from(["%1".to_string()]),
            now: 1000,
            ..RowBuildContext::default()
        };

        let rows = build_rows_ctx(&Config::default(), &[blocked, running], &state, &ctx).0;
        let repo = rows
            .iter()
            .find(|row| row.kind == SidebarRowKind::Repo)
            .expect("repo row");

        assert_eq!(
            repo.meta.as_ref().and_then(|meta| meta.attention_count),
            Some(1)
        );
    }

    #[test]
    fn repo_attention_count_keeps_triaged_pane_during_debounce() {
        let calm = pane("main", "%1", "/tmp/app", "codex", "running");
        let running = pane("main", "%2", "/tmp/app", "claude", "running");
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };
        let ctx = RowBuildContext {
            triage: BTreeSet::from(["%1".to_string()]),
            now: 1000,
            ..RowBuildContext::default()
        };

        let rows = build_rows_ctx(&Config::default(), &[calm, running], &state, &ctx).0;
        let repo = rows
            .iter()
            .find(|row| row.kind == SidebarRowKind::Repo)
            .expect("repo row");

        assert_eq!(
            repo.meta.as_ref().and_then(|meta| meta.attention_count),
            Some(1)
        );
    }

    #[test]
    fn triage_rows_carry_origin_in_meta() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        blocked.started_at = "940".to_string();
        let state = SidebarState {
            view_mode: ViewMode::ByRepo,
            ..SidebarState::default()
        };
        let ctx = RowBuildContext {
            triage: BTreeSet::from(["%1".to_string()]),
            now: 1000,
            ..RowBuildContext::default()
        };

        let rows = build_rows_ctx(&Config::default(), &[blocked], &state, &ctx).0;
        let chat = rows.iter().find(|row| row.id == "chat::%1").expect("chat");
        let meta = chat.meta.as_ref().expect("chat meta");

        assert_eq!(meta.origin.as_deref(), Some("misc/app"));
    }

    #[test]
    fn selected_triage_row_shows_origin_detail() {
        let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
        blocked.wait_reason = "permission_prompt".to_string();
        let mut state = SidebarState {
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");
        let ctx = RowBuildContext {
            triage: BTreeSet::from(["%1".to_string()]),
            now: 1000,
            ..RowBuildContext::default()
        };

        let rows = build_rows_ctx(&Config::default(), &[blocked], &state, &ctx).0;
        let origin_row = rows
            .iter()
            .find(|row| row.id == "detail::%1::origin")
            .expect("origin detail row");

        assert!(origin_row.label.contains("misc/app"));
    }

    #[test]
    fn manual_chat_expands_full_and_others_stay_single() {
        let selected = pane("main", "%1", "/tmp/app", "codex", "running");
        let second = pane("main", "%2", "/tmp/app", "claude", "running");
        let other = pane("main", "%3", "/tmp/app", "opencode", "running");
        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("chat::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");
        let ctx = RowBuildContext {
            now: 1000,
            ..RowBuildContext::default()
        };

        let rows = build_rows_ctx(&Config::default(), &[selected, second, other], &state, &ctx).0;

        assert!(!rows.iter().any(|row| row.id == "detail::%1::state"));
        assert!(rows.iter().any(|row| row.id == "jump::%1"));
        assert!(!rows.iter().any(|row| row.id == "detail::%2::state"));
        assert!(!rows.iter().any(|row| row.id == "meta::%3"));
        assert!(!rows.iter().any(|row| row.id == "detail::%3::state"));
    }

    #[test]
    fn selection_on_child_row_keeps_chat_expanded() {
        let p = pane("main", "%1", "/tmp/app", "codex", "running");
        let mut state = SidebarState {
            view_mode: ViewMode::Flat,
            selection: Some("jump::%1".to_string()),
            ..SidebarState::default()
        };
        state.toggle_expanded("chat::%1");

        let rows = build_rows_ctx(
            &Config::default(),
            &[p],
            &state,
            &RowBuildContext::default(),
        )
        .0;

        assert!(rows.iter().any(|row| row.id == "jump::%1"));
    }

    #[test]
    fn manual_order_reorders_repo_rows() {
        let state = SidebarState {
            manual_order: vec![
                crate::sidebar::state::RepoId::new("misc", "zeta"),
                crate::sidebar::state::RepoId::new("misc", "alpha"),
            ],
            ..SidebarState::default()
        };

        let rows = build_rows(
            &Config::default(),
            &[
                pane("main", "%1", "/tmp/alpha", "codex", "idle"),
                pane("main", "%2", "/tmp/zeta", "codex", "idle"),
            ],
            &state,
        );

        let repo_labels = rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Repo)
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(repo_labels, vec!["zeta", "alpha"]);
    }

    #[test]
    fn manual_chat_order_reorders_chat_rows() {
        let state = SidebarState {
            view_mode: ViewMode::Flat,
            manual_chat_order: vec!["%2".to_string(), "%1".to_string()],
            ..SidebarState::default()
        };

        let rows = build_rows(
            &Config::default(),
            &[
                pane("main", "%1", "/tmp/app", "codex", "idle"),
                pane("main", "%2", "/tmp/app", "claude", "idle"),
            ],
            &state,
        );

        let chat_ids = rows
            .iter()
            .filter(|row| row.kind == SidebarRowKind::Chat)
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(chat_ids, vec!["chat::%2", "chat::%1"]);
    }
}

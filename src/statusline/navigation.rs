use anyhow::{Result, anyhow};

use crate::session::Direction;
use crate::tmux::TmuxRunner;
use crate::window::select_window;

use super::targets::{
    CATEGORY_RANGE_PREFIX, CURRENT_CATEGORY_RANGE_PREFIX, resolve_category_target,
};

pub fn switch_statusline_session(
    runner: &dyn TmuxRunner,
    client_name: &str,
    session_id: &str,
    index: usize,
) -> Result<()> {
    let targets = displayed_targets(
        runner,
        session_id,
        crate::options::KEY_STATUS_SESSIONS,
        "session:",
    )?;
    let Some(target) = targets.get(index) else {
        return Ok(());
    };
    validate_tmux_target(target, '$', "session")?;
    runner.run(&["switch-client", "-c", client_name, "-t", target])?;
    Ok(())
}

pub fn cycle_statusline_session(
    runner: &dyn TmuxRunner,
    client_name: &str,
    session_id: &str,
    direction: Direction,
) -> Result<()> {
    let targets = displayed_targets(
        runner,
        session_id,
        crate::options::KEY_STATUS_SESSIONS,
        "session:",
    )?;
    let current = targets
        .iter()
        .position(|target| target == session_id)
        .ok_or_else(|| {
            anyhow!(
                "current session {session_id} is not present in the displayed status model; wait for the status line to redraw"
            )
        })?;
    let next = match direction {
        Direction::Next => (current + 1) % targets.len(),
        Direction::Previous => (current + targets.len() - 1) % targets.len(),
    };
    let target = &targets[next];
    validate_tmux_target(target, '$', "session")?;
    runner.run(&["switch-client", "-c", client_name, "-t", target])?;
    Ok(())
}

pub fn switch_statusline_window(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    select_window(runner, target)
}

pub fn switch_statusline_category(
    runner: &dyn TmuxRunner,
    snapshot: &crate::daemon::protocol::v2::StatusSnapshot,
    client_name: &str,
    session_id: &str,
    index: usize,
) -> Result<()> {
    let targets = displayed_category_targets(runner, session_id)?;
    let target = targets
        .get(index)
        .ok_or_else(|| {
            anyhow!(
                "displayed category index {} is no longer available; wait for the status line to redraw",
                index + 1
            )
    })?;
    let category = resolve_category_target(snapshot, target)?;
    crate::session::use_category_for_client_from_status_snapshot(
        runner,
        snapshot,
        &category,
        client_name,
    )
}

pub fn cycle_statusline_category(
    runner: &dyn TmuxRunner,
    snapshot: &crate::daemon::protocol::v2::StatusSnapshot,
    client_name: &str,
    session_id: &str,
    direction: Direction,
) -> Result<()> {
    cycle_statusline_category_with_snapshot(runner, snapshot, client_name, session_id, direction)
}

pub(super) fn cycle_statusline_category_with_snapshot(
    runner: &dyn TmuxRunner,
    snapshot: &crate::daemon::protocol::v2::StatusSnapshot,
    client_name: &str,
    session_id: &str,
    direction: Direction,
) -> Result<()> {
    if snapshot.context != crate::daemon::protocol::v2::StatusContext::Global {
        return Err(anyhow!(
            "category navigation requires a global daemon status snapshot"
        ));
    }
    let current_session = snapshot
        .sessions
        .iter()
        .find(|session| session.session_id == session_id)
        .ok_or_else(|| anyhow!("current session {session_id} is not present in daemon state"))?;
    let current_category = current_session
        .category
        .as_deref()
        .ok_or_else(|| anyhow!("current session {session_id} has no resolved category"))?;
    let targets = snapshot
        .categories
        .iter()
        .filter(|category| !category.session_ids.is_empty())
        .collect::<Vec<_>>();
    if targets.len() <= 1 {
        return Err(anyhow!(
            "category cycle requires at least two categories with sessions"
        ));
    }
    let current_index = targets
        .iter()
        .position(|category| category.category == current_category)
        .ok_or_else(|| {
            anyhow!("current category {current_category} is not present in the category cycle")
        })?;
    let next = match direction {
        Direction::Next => (current_index + 1) % targets.len(),
        Direction::Previous => (current_index + targets.len() - 1) % targets.len(),
    };
    crate::session::use_category_for_client_from_status_snapshot(
        runner,
        snapshot,
        &targets[next].category,
        client_name,
    )
}

pub fn handle_statusline_click(
    runner: &dyn TmuxRunner,
    category_snapshot: Option<&crate::daemon::protocol::v2::StatusSnapshot>,
    client_name: Option<&str>,
    range: Option<&str>,
) -> Result<()> {
    let Some(range) = range.map(str::trim).filter(|range| !range.is_empty()) else {
        return Ok(());
    };
    if let Some(target) = range.strip_prefix("window:") {
        if !target.trim().is_empty() {
            return select_window(runner, target);
        }
        return Ok(());
    }
    if let Some(target) = range.strip_prefix("session:") {
        validate_tmux_target(target, '$', "session")?;
        let client_name = client_name
            .ok_or_else(|| anyhow!("session click is missing an invoking tmux client"))?;
        runner.run(&["switch-client", "-c", client_name, "-t", target])?;
        return Ok(());
    }
    if let Some(target) = range.strip_prefix(CATEGORY_RANGE_PREFIX) {
        let client_name = client_name
            .ok_or_else(|| anyhow!("category click is missing an invoking tmux client"))?;
        let snapshot = category_snapshot
            .ok_or_else(|| anyhow!("category click is missing the canonical status snapshot"))?;
        return switch_category_target(runner, snapshot, client_name, target);
    }
    if let Some(target) = range.strip_prefix(CURRENT_CATEGORY_RANGE_PREFIX) {
        let client_name = client_name
            .ok_or_else(|| anyhow!("category click is missing an invoking tmux client"))?;
        let snapshot = category_snapshot
            .ok_or_else(|| anyhow!("category click is missing the canonical status snapshot"))?;
        return switch_category_target(runner, snapshot, client_name, target);
    }
    if range.starts_with('$') {
        validate_tmux_target(range, '$', "session")?;
        let client_name = client_name
            .ok_or_else(|| anyhow!("session click is missing an invoking tmux client"))?;
        runner.run(&["switch-client", "-c", client_name, "-t", range])?;
        return Ok(());
    }
    Ok(())
}

fn switch_category_target(
    runner: &dyn TmuxRunner,
    snapshot: &crate::daemon::protocol::v2::StatusSnapshot,
    client_name: &str,
    target: &str,
) -> Result<()> {
    let category = resolve_category_target(snapshot, target)?;
    crate::session::use_category_for_client_from_status_snapshot(
        runner,
        snapshot,
        &category,
        client_name,
    )
}

fn displayed_targets(
    runner: &dyn TmuxRunner,
    session_id: &str,
    option: &str,
    prefix: &str,
) -> Result<Vec<String>> {
    validate_tmux_target(session_id, '$', "session")?;
    let rendered =
        crate::options::show_session_option(runner, session_id, option)?.ok_or_else(|| {
            anyhow!("{option} is empty for {session_id}; wait for the status line to redraw")
        })?;
    let targets = top_level_user_ranges(&rendered)?
        .into_iter()
        .filter_map(|range| range.strip_prefix(prefix).map(str::to_string))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return Err(anyhow!(
            "{option} has no trusted {prefix} targets for {session_id}; wait for the status line to redraw"
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for target in &targets {
        validate_tmux_target(target, '$', "session")?;
        if !seen.insert(target) {
            return Err(anyhow!(
                "{option} contains duplicate session targets; wait for the status line to redraw"
            ));
        }
    }
    Ok(targets)
}

pub(super) fn displayed_category_targets(
    runner: &dyn TmuxRunner,
    session_id: &str,
) -> Result<Vec<String>> {
    validate_tmux_target(session_id, '$', "session")?;
    let option = crate::options::KEY_STATUS_CATEGORY;
    let rendered = crate::options::show_session_option(runner, session_id, option)?
        .ok_or_else(|| anyhow!("{option} is empty for {session_id}; wait for redraw"))?;
    let mut targets = Vec::new();
    let mut has_current = false;
    for range in top_level_user_ranges(&rendered)? {
        if let Some(target) = range.strip_prefix(CURRENT_CATEGORY_RANGE_PREFIX) {
            if targets.iter().any(|existing| existing == target) {
                return Err(anyhow!(
                    "{option} contains duplicate category targets; wait for redraw"
                ));
            }
            if has_current {
                return Err(anyhow!(
                    "{option} contains multiple active categories; wait for redraw"
                ));
            }
            has_current = true;
            targets.push(target.to_string());
        } else if let Some(target) = range.strip_prefix(CATEGORY_RANGE_PREFIX) {
            if targets.iter().any(|existing| existing == target) {
                return Err(anyhow!(
                    "{option} contains duplicate category targets; wait for redraw"
                ));
            }
            targets.push(target.to_string());
        }
    }
    if targets.is_empty() {
        return Err(anyhow!("{option} has no category targets; wait for redraw"));
    }
    Ok(targets)
}

pub(super) fn top_level_user_ranges(rendered: &str) -> Result<Vec<String>> {
    let bytes = rendered.as_bytes();
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"##") {
            index += 2;
            continue;
        }
        if !bytes[index..].starts_with(b"#[") {
            index += 1;
            continue;
        }
        let Some(relative_end) = bytes[index + 2..].iter().position(|byte| *byte == b']') else {
            return Err(anyhow!(
                "displayed status option contains an unterminated tmux directive"
            ));
        };
        let end = index + 2 + relative_end;
        let directive = &rendered[index + 2..end];
        if let Some(range) = directive.strip_prefix("range=user|") {
            if depth == 0 {
                ranges.push(range.to_string());
            }
            depth = depth
                .checked_add(1)
                .ok_or_else(|| anyhow!("displayed status range nesting overflow"))?;
        } else if directive == "norange" {
            if depth == 0 {
                return Err(anyhow!(
                    "displayed status option contains an unmatched #[norange]"
                ));
            }
            depth -= 1;
        }
        index = end + 1;
    }
    if depth != 0 {
        return Err(anyhow!(
            "displayed status option contains an unclosed user range"
        ));
    }
    Ok(ranges)
}

fn validate_tmux_target(target: &str, prefix: char, kind: &str) -> Result<()> {
    let valid = target.strip_prefix(prefix).is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    if valid {
        Ok(())
    } else {
        Err(anyhow!("invalid {kind} target: {target}"))
    }
}

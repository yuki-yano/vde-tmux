use serde::{Deserialize, Serialize};

use crate::config::{AgentBadgeConfig, BadgeGlyphs, SessionBadgeMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BadgeState {
    Blocked,
    Working,
    Done,
    Idle,
}

impl BadgeState {
    pub fn as_str(self) -> &'static str {
        match self {
            BadgeState::Blocked => "blocked",
            BadgeState::Working => "working",
            BadgeState::Done => "done",
            BadgeState::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BadgeStateCounts {
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
}

impl BadgeStateCounts {
    pub fn from_states(states: impl IntoIterator<Item = BadgeState>) -> Self {
        let mut counts = Self::default();
        for state in states {
            counts.push(state);
        }
        counts
    }

    pub fn push(&mut self, state: BadgeState) {
        match state {
            BadgeState::Blocked => self.blocked += 1,
            BadgeState::Working => self.working += 1,
            BadgeState::Done => self.done += 1,
            BadgeState::Idle => self.idle += 1,
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.blocked += other.blocked;
        self.working += other.working;
        self.done += other.done;
        self.idle += other.idle;
    }

    pub fn total(self) -> usize {
        self.blocked + self.working + self.done + self.idle
    }

    pub fn rollup_state(self) -> Option<BadgeState> {
        [
            (BadgeState::Blocked, self.blocked),
            (BadgeState::Working, self.working),
            (BadgeState::Done, self.done),
            (BadgeState::Idle, self.idle),
        ]
        .into_iter()
        .find_map(|(state, count)| (count > 0).then_some(state))
    }
}

pub fn agent_badge_value_from_counts(
    counts: BadgeStateCounts,
    glyphs: &BadgeGlyphs,
    config: &AgentBadgeConfig,
) -> Option<String> {
    badge_value_from_counts(
        counts,
        glyphs,
        config.mode,
        &config.suffix,
        config.hide_idle,
    )
}

pub fn badge_value_from_counts(
    mut counts: BadgeStateCounts,
    glyphs: &BadgeGlyphs,
    mode: SessionBadgeMode,
    suffix: &str,
    hide_idle: bool,
) -> Option<String> {
    if hide_idle {
        counts.idle = 0;
    }
    match mode {
        SessionBadgeMode::Rollup => badge_rollup_value(counts, glyphs, suffix),
        SessionBadgeMode::Counts => badge_counts_value(counts, glyphs, suffix),
    }
}

fn badge_rollup_value(
    counts: BadgeStateCounts,
    glyphs: &BadgeGlyphs,
    suffix: &str,
) -> Option<String> {
    let state = counts.rollup_state()?;
    Some(format!("{}{suffix}", glyph_for_state(state, glyphs)))
}

fn badge_counts_value(
    counts: BadgeStateCounts,
    glyphs: &BadgeGlyphs,
    suffix: &str,
) -> Option<String> {
    let parts = [
        (BadgeState::Blocked, counts.blocked),
        (BadgeState::Working, counts.working),
        (BadgeState::Done, counts.done),
        (BadgeState::Idle, counts.idle),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(state, count)| format!("{} {count}", glyph_for_state(state, glyphs)))
    .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }
    Some(format!("{}{suffix}", parts.join(" ")))
}

pub fn glyph_for_state(state: BadgeState, glyphs: &BadgeGlyphs) -> &str {
    match state {
        BadgeState::Blocked => &glyphs.blocked,
        BadgeState::Working => &glyphs.working,
        BadgeState::Done => &glyphs.done,
        BadgeState::Idle => &glyphs.idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BadgeGlyphs;

    #[test]
    fn rollup_picks_most_urgent_typed_count() {
        let glyphs = BadgeGlyphs::default();
        let value = badge_value_from_counts(
            BadgeStateCounts::from_states([
                BadgeState::Idle,
                BadgeState::Blocked,
                BadgeState::Working,
            ]),
            &glyphs,
            SessionBadgeMode::Rollup,
            "",
            false,
        );
        assert_eq!(value.as_deref(), Some("▲"));
    }

    #[test]
    fn typed_count_appends_suffix_and_respects_custom_glyphs() {
        let glyphs = BadgeGlyphs {
            done: "D".to_string(),
            ..BadgeGlyphs::default()
        };
        let value = badge_value_from_counts(
            BadgeStateCounts::from_states([BadgeState::Done]),
            &glyphs,
            SessionBadgeMode::Rollup,
            "|",
            false,
        );
        assert_eq!(value.as_deref(), Some("D|"));
    }

    #[test]
    fn hide_idle_suppresses_idle_badge_only() {
        let glyphs = BadgeGlyphs::default();
        assert_eq!(
            badge_value_from_counts(
                BadgeStateCounts::from_states([BadgeState::Idle]),
                &glyphs,
                SessionBadgeMode::Rollup,
                "",
                true,
            ),
            None
        );
        assert_eq!(
            badge_value_from_counts(
                BadgeStateCounts::from_states([BadgeState::Done]),
                &glyphs,
                SessionBadgeMode::Rollup,
                "",
                true,
            )
            .as_deref(),
            Some("✓")
        );
        assert_eq!(
            badge_value_from_counts(
                BadgeStateCounts::from_states([BadgeState::Idle]),
                &glyphs,
                SessionBadgeMode::Rollup,
                "",
                false,
            )
            .as_deref(),
            Some("○")
        );
    }

    #[test]
    fn typed_count_is_none_for_no_agents() {
        assert_eq!(
            badge_value_from_counts(
                BadgeStateCounts::default(),
                &BadgeGlyphs::default(),
                SessionBadgeMode::Rollup,
                "",
                false,
            ),
            None
        );
    }

    #[test]
    fn counts_mode_renders_non_zero_state_counts_in_priority_order() {
        let value = badge_value_from_counts(
            BadgeStateCounts::from_states([
                BadgeState::Idle,
                BadgeState::Blocked,
                BadgeState::Working,
                BadgeState::Blocked,
            ]),
            &BadgeGlyphs::default(),
            SessionBadgeMode::Counts,
            "",
            false,
        );

        assert_eq!(value.as_deref(), Some("▲ 2 ● 1 ○ 1"));
    }

    #[test]
    fn counts_mode_hides_idle_and_appends_suffix() {
        assert_eq!(
            badge_value_from_counts(
                BadgeStateCounts::from_states([BadgeState::Idle]),
                &BadgeGlyphs::default(),
                SessionBadgeMode::Counts,
                "|",
                true,
            ),
            None
        );
        assert_eq!(
            badge_value_from_counts(
                BadgeStateCounts::from_states([BadgeState::Done, BadgeState::Idle]),
                &BadgeGlyphs::default(),
                SessionBadgeMode::Counts,
                "|",
                true,
            )
            .as_deref(),
            Some("✓ 1|")
        );
    }
}

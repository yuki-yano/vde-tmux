use serde::{Deserialize, Serialize};

use crate::config::{AgentBadgeConfig, BadgeGlyphs, SessionBadgeConfig, SessionBadgeMode};
use crate::hook::RollupLevel;

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

    pub fn encode(self) -> String {
        serde_json::to_string(&self).expect("BadgeStateCounts serialization should not fail")
    }

    pub fn decode(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

pub fn badge_state(level: RollupLevel, unread: bool) -> BadgeState {
    match level {
        RollupLevel::Error | RollupLevel::Permission | RollupLevel::Waiting => BadgeState::Blocked,
        RollupLevel::Running => BadgeState::Working,
        RollupLevel::Background | RollupLevel::Idle => {
            if unread {
                BadgeState::Done
            } else {
                BadgeState::Idle
            }
        }
    }
}

pub fn session_badge_value(
    states: impl IntoIterator<Item = BadgeState>,
    glyphs: &BadgeGlyphs,
    config: &SessionBadgeConfig,
) -> Option<String> {
    badge_value_from_counts(
        BadgeStateCounts::from_states(states),
        glyphs,
        config.mode,
        &config.suffix,
        config.hide_idle,
    )
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
    use crate::config::{BadgeGlyphs, SessionBadgeConfig};
    use crate::hook::RollupLevel;

    #[test]
    fn blocked_covers_error_permission_waiting() {
        for level in [
            RollupLevel::Error,
            RollupLevel::Permission,
            RollupLevel::Waiting,
        ] {
            assert_eq!(badge_state(level, false), BadgeState::Blocked);
            assert_eq!(badge_state(level, true), BadgeState::Blocked);
        }
    }

    #[test]
    fn working_covers_running_only() {
        assert_eq!(
            badge_state(RollupLevel::Running, false),
            BadgeState::Working
        );
    }

    #[test]
    fn background_without_explicit_agent_status_is_idle() {
        assert_eq!(
            badge_state(RollupLevel::Background, false),
            BadgeState::Idle
        );
        assert_eq!(badge_state(RollupLevel::Background, true), BadgeState::Done);
    }

    #[test]
    fn idle_splits_by_unread_flag() {
        assert_eq!(badge_state(RollupLevel::Idle, true), BadgeState::Done);
        assert_eq!(badge_state(RollupLevel::Idle, false), BadgeState::Idle);
    }

    #[test]
    fn session_rollup_picks_most_urgent_state() {
        let config = SessionBadgeConfig::default();
        let glyphs = BadgeGlyphs::default();
        let value = session_badge_value(
            [BadgeState::Idle, BadgeState::Blocked, BadgeState::Working],
            &glyphs,
            &config,
        );
        assert_eq!(value.as_deref(), Some("▲"));
    }

    #[test]
    fn session_badge_value_appends_suffix_and_respects_custom_glyphs() {
        let config = SessionBadgeConfig {
            suffix: "|".to_string(),
            ..SessionBadgeConfig::default()
        };
        let glyphs = BadgeGlyphs {
            done: "D".to_string(),
            ..BadgeGlyphs::default()
        };
        let value = session_badge_value([BadgeState::Done], &glyphs, &config);
        assert_eq!(value.as_deref(), Some("D|"));
    }

    #[test]
    fn hide_idle_suppresses_idle_badge_only() {
        let glyphs = BadgeGlyphs::default();
        let config = SessionBadgeConfig {
            hide_idle: true,
            ..SessionBadgeConfig::default()
        };
        assert_eq!(
            session_badge_value([BadgeState::Idle], &glyphs, &config),
            None
        );
        assert_eq!(
            session_badge_value([BadgeState::Done], &glyphs, &config).as_deref(),
            Some("✓")
        );
        let config = SessionBadgeConfig::default();
        assert_eq!(
            session_badge_value([BadgeState::Idle], &glyphs, &config).as_deref(),
            Some("○")
        );
    }

    #[test]
    fn session_badge_value_is_none_for_no_agents() {
        let config = SessionBadgeConfig::default();
        assert_eq!(
            session_badge_value([], &BadgeGlyphs::default(), &config),
            None
        );
    }

    #[test]
    fn session_badge_value_uses_top_level_badge_glyphs_and_statusline_suffix() {
        let glyphs = crate::config::BadgeGlyphs {
            working: "W".to_string(),
            ..crate::config::BadgeGlyphs::default()
        };

        let config = SessionBadgeConfig {
            suffix: "|".to_string(),
            ..SessionBadgeConfig::default()
        };
        let value = session_badge_value([BadgeState::Working], &glyphs, &config);

        assert_eq!(value.as_deref(), Some("W|"));
    }

    #[test]
    fn counts_mode_renders_non_zero_state_counts_in_priority_order() {
        let config = SessionBadgeConfig {
            mode: SessionBadgeMode::Counts,
            ..SessionBadgeConfig::default()
        };

        let value = session_badge_value(
            [
                BadgeState::Idle,
                BadgeState::Blocked,
                BadgeState::Working,
                BadgeState::Blocked,
            ],
            &BadgeGlyphs::default(),
            &config,
        );

        assert_eq!(value.as_deref(), Some("▲ 2 ● 1 ○ 1"));
    }

    #[test]
    fn counts_mode_hides_idle_and_appends_suffix() {
        let config = SessionBadgeConfig {
            mode: SessionBadgeMode::Counts,
            hide_idle: true,
            suffix: "|".to_string(),
            ..SessionBadgeConfig::default()
        };

        assert_eq!(
            session_badge_value([BadgeState::Idle], &BadgeGlyphs::default(), &config),
            None
        );
        assert_eq!(
            session_badge_value(
                [BadgeState::Done, BadgeState::Idle],
                &BadgeGlyphs::default(),
                &config,
            )
            .as_deref(),
            Some("✓ 1|")
        );
    }
}

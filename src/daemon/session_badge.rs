use serde::{Deserialize, Serialize};

use crate::config::BadgeGlyphs;
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
    suffix: &str,
    hide_idle: bool,
) -> Option<String> {
    let state = states.into_iter().min()?;
    if hide_idle && state == BadgeState::Idle {
        return None;
    }
    Some(format!("{}{suffix}", glyph_for_state(state, glyphs)))
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
            &config.suffix,
            false,
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
        let value = session_badge_value([BadgeState::Done], &glyphs, &config.suffix, false);
        assert_eq!(value.as_deref(), Some("D|"));
    }

    #[test]
    fn hide_idle_suppresses_idle_badge_only() {
        let glyphs = BadgeGlyphs::default();
        assert_eq!(
            session_badge_value([BadgeState::Idle], &glyphs, "", true),
            None
        );
        assert_eq!(
            session_badge_value([BadgeState::Done], &glyphs, "", true).as_deref(),
            Some("✓")
        );
        assert_eq!(
            session_badge_value([BadgeState::Idle], &glyphs, "", false).as_deref(),
            Some("○")
        );
    }

    #[test]
    fn session_badge_value_is_none_for_no_agents() {
        let config = SessionBadgeConfig::default();
        assert_eq!(
            session_badge_value([], &BadgeGlyphs::default(), &config.suffix, false),
            None
        );
    }

    #[test]
    fn session_badge_value_uses_top_level_badge_glyphs_and_statusline_suffix() {
        let glyphs = crate::config::BadgeGlyphs {
            working: "W".to_string(),
            ..crate::config::BadgeGlyphs::default()
        };

        let value = session_badge_value([BadgeState::Working], &glyphs, "|", false);

        assert_eq!(value.as_deref(), Some("W|"));
    }
}

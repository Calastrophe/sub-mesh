//! The balancer's score for an account, and the per-window room arithmetic
//! it shares with the reserve rule in [`admission()`](crate::admission()).

use chrono::{DateTime, Utc};

use crate::reserve::ReserveConfig;
use crate::window::Window;

/// The reserve margin below which the safety factor tapers toward zero.
const SAFETY_ROOM: f64 = 0.15;

/// A floor on time to reset keeps imminent resets from dominating the score.
const MIN_HOURS: f64 = 5.0 / 60.0;

/// One week, used when the window duration is unknown.
const UNKNOWN_HOURS: f64 = 168.0;

/// How attractive an account is for a new session.
///
/// The score is urgency times safety times load. Urgency rewards capacity
/// that will expire unused. The tightest window governs the reserve margin
/// so a generous window cannot mask a nearly spent one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Score {
    /// Higher is better. Zero for an account that has no room.
    pub value: f64,
    /// The room in the account's tightest window. Breaks ties.
    pub min_room: f64,
}

impl Score {
    /// Score an account.
    ///
    /// `in_flight` is the number of requests being relayed through the
    /// account right now.
    #[must_use]
    pub fn new(
        windows: &[Window],
        reserve: &ReserveConfig,
        in_flight: u32,
        now: DateTime<Utc>,
    ) -> Self {
        let load = 1.0 / (1.0 + f64::from(in_flight));
        if windows.is_empty() {
            let room = default_room(reserve);
            return Self {
                value: room / UNKNOWN_HOURS * load,
                min_room: room,
            };
        }
        let mut urgency = 0.0_f64;
        let mut min_room = f64::INFINITY;
        for window in windows {
            let room = room(window, reserve, now);
            urgency = urgency.max(room.max(0.0) / hours_to_reset(window, now));
            min_room = min_room.min(room);
        }
        let safety = (min_room / SAFETY_ROOM).clamp(0.0, 1.0);
        Self {
            value: urgency * safety * load,
            min_room,
        }
    }
}

/// The part of `window` the mesh may still use: what is left after the
/// reserve and the usage. A window that has reset counts as unused. A value
/// that cannot be computed counts as no room.
pub(crate) fn room(window: &Window, reserve: &ReserveConfig, now: DateTime<Utc>) -> f64 {
    non_nan((1.0 - reserve.reserve_for(window)) - window.used_at(now))
}

/// The room of an account with no known windows: what the default reserve
/// leaves.
pub(crate) fn default_room(reserve: &ReserveConfig) -> f64 {
    non_nan(1.0 - reserve.default.clamp(0.0, 1.0))
}

/// Hours until `window` resets, never below [`MIN_HOURS`].
///
/// A window whose reset time has passed (its report is out of date) or is
/// unknown has just started or is of unknown age. Its next reset is one
/// full length away: the window's positive duration, or [`UNKNOWN_HOURS`].
fn hours_to_reset(window: &Window, now: DateTime<Utc>) -> f64 {
    let hours = match window.resets_at.filter(|at| *at > now) {
        Some(at) => (at - now).num_milliseconds() as f64 / 3_600_000.0,
        None => window
            .duration_minutes
            .filter(|minutes| *minutes > 0)
            .map_or(UNKNOWN_HOURS, |minutes| f64::from(minutes) / 60.0),
    };
    hours.max(MIN_HOURS)
}

fn non_nan(value: f64) -> f64 {
    if value.is_nan() { 0.0 } else { value }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-01-01T12:00:00Z".parse().unwrap()
    }

    fn hours(n: f64) -> TimeDelta {
        TimeDelta::milliseconds((n * 3_600_000.0).round() as i64)
    }

    fn five_hour(used: f64, resets_in_hours: f64) -> Window {
        Window {
            id: "five_hour".to_owned(),
            duration_minutes: Some(300),
            used,
            resets_at: Some(now() + hours(resets_in_hours)),
            limited: false,
        }
    }

    fn weekly(used: f64, resets_in_hours: f64) -> Window {
        Window {
            id: "seven_day".to_owned(),
            duration_minutes: Some(10_080),
            used,
            resets_at: Some(now() + hours(resets_in_hours)),
            limited: false,
        }
    }

    fn score(windows: &[Window], in_flight: u32) -> Score {
        Score::new(windows, &ReserveConfig::default(), in_flight, now())
    }

    /// The score of an idle account with the default reserve (0.3).
    fn value(windows: &[Window]) -> f64 {
        score(windows, 0).value
    }

    fn close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn account_without_windows_scores_like_a_week_away() {
        let idle = score(&[], 0);
        close(idle.value, 0.7 / 168.0);
        close(idle.min_room, 0.7);
        close(score(&[], 1).value, 0.7 / 168.0 / 2.0);
        let custom = ReserveConfig {
            default: 0.5,
            ..ReserveConfig::default()
        };
        close(Score::new(&[], &custom, 0, now()).value, 0.5 / 168.0);
    }

    #[test]
    fn urgency_is_room_per_hour_until_reset() {
        let score = score(&[five_hour(0.3, 2.0)], 0);
        close(score.value, 0.4 / 2.0);
        close(score.min_room, 0.4);
    }

    #[test]
    fn the_most_urgent_window_decides_urgency() {
        let windows = [five_hour(0.2, 1.0), weekly(0.1, 100.0)];
        close(value(&windows), 0.5);
    }

    #[test]
    fn reserve_comes_from_the_reserve_in_effect() {
        let strict = ReserveConfig {
            five_hour: Some(0.5),
            ..ReserveConfig::default()
        };
        let score = Score::new(&[five_hour(0.1, 1.0)], &strict, 0, now());
        close(score.value, 0.4);
        close(score.min_room, 0.4);
    }

    #[test]
    fn reset_time_is_floored_at_five_minutes() {
        let windows = [five_hour(0.3, 30.0 / 3600.0)];
        close(value(&windows), 0.4 * 12.0);
    }

    #[test]
    fn an_unknown_reset_uses_the_window_length() {
        let mut window = five_hour(0.3, 0.0);
        window.resets_at = None;
        close(value(&[window]), 0.4 / 5.0);
    }

    #[test]
    fn an_unknown_reset_and_length_use_a_week() {
        let mut window = five_hour(0.3, 0.0);
        window.resets_at = None;
        window.duration_minutes = None;
        close(value(&[window]), 0.4 / 168.0);
    }

    #[test]
    fn a_zero_length_without_a_future_reset_uses_a_week() {
        let mut window = five_hour(0.3, 0.0);
        window.resets_at = None;
        window.duration_minutes = Some(0);
        close(value(&[window]), 0.4 / 168.0);
    }

    #[test]
    fn infinite_window_usage_is_clamped() {
        close(value(&[five_hour(f64::INFINITY, 1.0)]), 0.0);
        close(value(&[five_hour(f64::NEG_INFINITY, 1.0)]), 0.7);
    }

    #[test]
    fn a_window_that_has_reset_counts_as_unused_and_a_full_length_away() {
        let stale = five_hour(0.9, -1.0);
        close(value(&[stale]), 0.7 / 5.0);
    }

    #[test]
    fn safety_fades_in_over_the_last_fifteen_percent() {
        let windows = [five_hour(0.625, 1.0)];
        close(value(&windows), 0.075 * 0.5);
        close(value(&[five_hour(0.55, 1.0)]), 0.15);
        close(value(&[five_hour(0.4, 1.0)]), 0.3);
        close(value(&[five_hour(0.7, 1.0)]), 0.0);
        close(value(&[five_hour(0.95, 1.0)]), 0.0);
    }

    #[test]
    fn any_window_near_its_reserve_lowers_the_score() {
        let windows = [five_hour(0.3, 1.0), weekly(0.67, 100.0)];
        close(value(&windows), 0.4 * 0.2);
    }

    #[test]
    fn load_divides_the_score() {
        let windows = [five_hour(0.3, 2.0)];
        let idle = value(&windows);
        close(score(&windows, 1).value, idle / 2.0);
        close(score(&windows, 3).value, idle / 4.0);
    }

    #[test]
    fn a_much_better_account_still_wins_under_load() {
        let busy_but_urgent = score(&[five_hour(0.3, 0.5)], 1).value;
        let idle_but_late = value(&[five_hour(0.3, 4.0)]);
        assert!(busy_but_urgent > idle_but_late);
    }

    #[test]
    fn values_that_cannot_be_computed_count_as_no_room() {
        let broken = Window {
            used: f64::NAN,
            ..five_hour(0.0, 1.0)
        };
        let score = score(&[broken], 0);
        assert_eq!(score.value, 0.0);
        assert_eq!(score.min_room, 0.0);
        let nan_default = ReserveConfig {
            default: f64::NAN,
            ..ReserveConfig::default()
        };
        let empty = Score::new(&[], &nan_default, 0, now());
        assert_eq!(empty.value, 0.0);
        assert!(!empty.min_room.is_nan());
    }

    #[test]
    fn soon_resetting_capacity_beats_late_resetting_capacity() {
        let soon = value(&[five_hour(0.5, 0.5)]);
        let late = value(&[five_hour(0.5, 4.0)]);
        assert!(soon > late, "{soon} vs {late}");
    }

    #[test]
    fn near_the_weekly_reserve_loses_even_when_the_five_hour_window_is_urgent() {
        let near = [five_hour(0.3, 1.0), weekly(0.69, 100.0)];
        let calm = [five_hour(0.3, 3.0), weekly(0.2, 100.0)];
        assert!(value(&[five_hour(0.3, 1.0)]) > 2.5 * value(&[five_hour(0.3, 3.0)]));
        assert!(value(&near) < value(&calm));
    }

    #[test]
    fn a_tighter_window_lowers_min_room_without_changing_an_equal_score() {
        let roomy = score(&[five_hour(0.4, 3.0)], 0);
        let tight = score(&[five_hour(0.4, 3.0), weekly(0.5, 1000.0)], 0);
        close(roomy.value, tight.value);
        assert!(roomy.min_room > tight.min_room);
    }
}

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One usage window reported by a provider: Claude's `five_hour` or
/// `seven_day`, or Codex's `primary` or `secondary`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Stable provider-specific id: `"five_hour"`, `"seven_day"`,
    /// `"primary"`, or `"secondary"`.
    pub id: String,
    /// Length of the window in minutes, when known.
    pub duration_minutes: Option<u32>,
    /// Fraction of the window used, from 0.0 to 1.0.
    pub used: f64,
    /// When the window resets, when known.
    pub resets_at: Option<DateTime<Utc>>,
    /// True when the provider says this window is currently rate limited.
    #[serde(default)]
    pub limited: bool,
}

impl Window {
    /// The class used to pick a reserve for this window.
    #[must_use]
    pub fn class(&self) -> WindowClass {
        WindowClass::from_minutes(self.duration_minutes)
    }

    /// The usage at `now`, clamped to `0.0..=1.0`. A reset window
    /// counts as empty. Otherwise, `NaN` is returned unchanged.
    #[must_use]
    pub fn used_at(&self, now: DateTime<Utc>) -> f64 {
        if self.has_reset(now) {
            0.0
        } else {
            self.used.clamp(0.0, 1.0)
        }
    }

    /// Whether the provider's limit flag still applies at `now`.
    #[must_use]
    pub fn limited_at(&self, now: DateTime<Utc>) -> bool {
        self.limited && !self.has_reset(now)
    }

    fn has_reset(&self, now: DateTime<Utc>) -> bool {
        self.resets_at.is_some_and(|reset| reset <= now)
    }
}

/// Which of an account's two usage windows a [`Window`] is.
///
/// An account has at most a short window and a long one. A window is the
/// short one when its length is known and at most six hours. Every other
/// window, including one of unknown length, is the long one. That keeps a
/// Codex window of any length (a weekly window, or a longer one on some
/// plans) on the weekly reserve, with no duration cutoff to maintain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowClass {
    /// The 5-hour limit.
    FiveHour,
    /// The weekly limit.
    Weekly,
}

impl WindowClass {
    /// Classify a window by its length in minutes.
    #[must_use]
    pub fn from_minutes(minutes: Option<u32>) -> Self {
        match minutes {
            Some(minutes) if minutes <= 6 * 60 => Self::FiveHour,
            _ => Self::Weekly,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_windows_are_five_hour_and_all_others_weekly() {
        assert_eq!(WindowClass::from_minutes(Some(300)), WindowClass::FiveHour);
        assert_eq!(WindowClass::from_minutes(Some(361)), WindowClass::Weekly);
        assert_eq!(WindowClass::from_minutes(Some(10_080)), WindowClass::Weekly);
        assert_eq!(WindowClass::from_minutes(Some(43_200)), WindowClass::Weekly);
        assert_eq!(WindowClass::from_minutes(None), WindowClass::Weekly);
    }
}

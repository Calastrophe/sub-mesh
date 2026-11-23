use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::reserve::ReserveConfig;
use crate::score;
use crate::window::Window;

/// Whether an account may take mesh traffic right now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Admission {
    Admit {
        /// The smallest remaining room before any window's reserve, from
        /// 0.0 to 1.0. The scheduler prefers accounts with more headroom.
        headroom: f64,
    },
    Deny {
        reason: DenyReason,
    },
}

impl Admission {
    /// True for [`Admission::Admit`].
    #[must_use]
    pub fn is_admit(&self) -> bool {
        matches!(self, Self::Admit { .. })
    }

    /// The headroom when admitted.
    #[must_use]
    pub fn headroom(&self) -> Option<f64> {
        match self {
            Self::Admit { headroom } => Some(*headroom),
            Self::Deny { .. } => None,
        }
    }
}

/// Why an account refuses mesh traffic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DenyReason {
    /// The owner paused sharing.
    Paused,
    /// The account's login is broken and needs `meshctl login` again.
    AuthError,
    /// The window has no headroom or its headroom cannot be computed.
    Reserve {
        /// ID of the affected window.
        window: String,
    },
    /// The provider reports the window as rate limited.
    Limited {
        /// ID of the affected window.
        window: String,
    },
}

/// Apply the reserve rule to an account's windows.
///
/// With no known windows, use the clamped default reserve. An active limit
/// denies admission for that window. Nonpositive or `NaN` headroom triggers
/// [`DenyReason::Reserve`].
#[must_use = "the admission decision must be checked before routing traffic"]
pub fn admission(windows: &[Window], reserve: &ReserveConfig, now: DateTime<Utc>) -> Admission {
    if windows.is_empty() {
        let headroom = score::default_room(reserve);
        return if headroom > 0.0 {
            Admission::Admit { headroom }
        } else {
            Admission::Deny {
                reason: DenyReason::Reserve {
                    window: "unknown".to_owned(),
                },
            }
        };
    }

    let mut headroom = 1.0_f64;
    for window in windows {
        if window.limited_at(now) {
            return Admission::Deny {
                reason: DenyReason::Limited {
                    window: window.id.clone(),
                },
            };
        }
        let room = score::room(window, reserve, now);
        if room <= 0.0 {
            return Admission::Deny {
                reason: DenyReason::Reserve {
                    window: window.id.clone(),
                },
            };
        }
        headroom = headroom.min(room);
    }
    Admission::Admit { headroom }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, minutes: u32, used: f64) -> Window {
        Window {
            id: id.to_owned(),
            duration_minutes: Some(minutes),
            used,
            resets_at: None,
            limited: false,
        }
    }

    #[test]
    fn picks_the_class_reserve_over_the_default() {
        let config = ReserveConfig {
            default: 0.3,
            five_hour: Some(0.1),
            weekly: None,
        };
        assert_eq!(config.reserve_for(&window("five_hour", 300, 0.0)), 0.1);
        assert_eq!(config.reserve_for(&window("seven_day", 10080, 0.0)), 0.3);
        let weekly = ReserveConfig {
            weekly: Some(0.6),
            ..config
        };
        assert_eq!(weekly.reserve_for(&window("secondary", 43_200, 0.0)), 0.6);
        assert_eq!(weekly.reserve_for(&window("primary", 10080, 0.0)), 0.6);
    }

    #[test]
    fn a_per_window_reserve_map_is_rejected() {
        let text = r#"{"default":0.2,"windows":{"seven_day_opus":0.9}}"#;
        assert!(serde_json::from_str::<ReserveConfig>(text).is_err());
    }

    #[test]
    fn admits_with_smallest_headroom() {
        let now = Utc::now();
        let windows = [
            window("five_hour", 300, 0.2),
            window("seven_day", 10080, 0.6),
        ];
        let result = admission(&windows, &ReserveConfig::default(), now);
        let headroom = result.headroom().expect("should admit");
        assert!((headroom - 0.1).abs() < 1e-9, "headroom was {headroom}");
    }

    #[test]
    fn denies_at_reserve() {
        let now = Utc::now();
        let windows = [window("seven_day", 10080, 0.7)];
        assert_eq!(
            admission(&windows, &ReserveConfig::default(), now),
            Admission::Deny {
                reason: DenyReason::Reserve {
                    window: "seven_day".to_owned()
                }
            }
        );
    }

    #[test]
    fn denies_when_window_headroom_is_nan() {
        let now = Utc::now();
        let invalid_reserve = ReserveConfig {
            five_hour: Some(f64::NAN),
            ..ReserveConfig::default()
        };
        let expected = Admission::Deny {
            reason: DenyReason::Reserve {
                window: "five_hour".to_owned(),
            },
        };
        assert_eq!(
            admission(&[window("five_hour", 300, 0.1)], &invalid_reserve, now),
            expected
        );
        let windows = [
            window("weekly", 10080, 0.2),
            window("five_hour", 300, f64::NAN),
        ];
        assert_eq!(
            admission(&windows, &ReserveConfig::default(), now),
            expected
        );
    }

    #[test]
    fn limit_expires_at_reset() {
        let now = Utc::now();
        let mut windows = [window("primary", 300, 0.1)];
        windows[0].limited = true;
        let reserve = ReserveConfig::default();
        assert_eq!(
            admission(&windows, &reserve, now),
            Admission::Deny {
                reason: DenyReason::Limited {
                    window: "primary".to_owned(),
                },
            }
        );
        windows[0].used = 1.0;
        windows[0].resets_at = Some(now);
        assert_eq!(admission(&windows, &reserve, now).headroom(), Some(0.7));
    }

    #[test]
    fn no_windows_admits_with_default_headroom() {
        let now = Utc::now();
        let mut reserve = ReserveConfig::default();
        assert_eq!(admission(&[], &reserve, now).headroom(), Some(0.7));
        for default in [1.0, f64::NAN] {
            reserve.default = default;
            assert!(!admission(&[], &reserve, now).is_admit());
        }
    }
}

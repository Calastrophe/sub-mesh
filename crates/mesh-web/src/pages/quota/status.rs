//! The status badges shown on a subscription row.

use chrono::{DateTime, Utc};
use mesh_core::api::Subscription;
use mesh_core::{Admission, DenyReason};

use crate::format;

/// What a badge says about the subscription, which sets its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeKind {
    Offline,
    Paused,
    Denied,
    Cooldown,
}

impl BadgeKind {
    pub fn class(self) -> &'static str {
        match self {
            Self::Offline => "badge-offline",
            Self::Paused => "badge-paused",
            Self::Denied => "badge-denied",
            Self::Cooldown => "badge-cooldown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Badge {
    pub kind: BadgeKind,
    pub text: String,
}

impl Badge {
    fn new(kind: BadgeKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// Name of the window `id` on `sub`, falling back to the raw id.
fn window_name(sub: &Subscription, id: &str) -> String {
    sub.windows
        .iter()
        .find(|w| w.id == id)
        .map_or_else(|| id.to_owned(), |w| format::window_label(w).to_owned())
}

/// Badges for a subscription at `now`: offline, paused, why it is denied,
/// and an active cooldown. A healthy subscription has none.
pub fn badges(sub: &Subscription, now: DateTime<Utc>) -> Vec<Badge> {
    let mut badges = Vec::new();
    if !sub.online {
        badges.push(Badge::new(BadgeKind::Offline, "offline"));
    }
    if sub.paused {
        badges.push(Badge::new(BadgeKind::Paused, "paused"));
    }
    if let Admission::Deny { reason } = &sub.admission {
        match reason {
            DenyReason::Paused if sub.paused => {}
            DenyReason::Paused => badges.push(Badge::new(BadgeKind::Paused, "paused")),
            DenyReason::AuthError => {
                badges.push(Badge::new(BadgeKind::Denied, "login expired"));
            }
            DenyReason::Reserve { window } => badges.push(Badge::new(
                BadgeKind::Denied,
                format!("reserve reached · {}", window_name(sub, window)),
            )),
            DenyReason::Limited { window } => badges.push(Badge::new(
                BadgeKind::Denied,
                format!("rate limited · {}", window_name(sub, window)),
            )),
        }
    }
    if let Some(until) = sub.cooldown_until.filter(|&until| until > now) {
        badges.push(Badge::new(
            BadgeKind::Cooldown,
            format!("cooldown {}", format::span(until - now)),
        ));
    }
    badges
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::mock;

    fn healthy() -> Subscription {
        let mut sub = mock::overview().subscriptions.remove(0);
        sub.online = true;
        sub.paused = false;
        sub.admission = Admission::Admit { headroom: 0.5 };
        sub.cooldown_until = None;
        sub
    }

    #[test]
    fn healthy_has_no_badges() {
        assert!(badges(&healthy(), Utc::now()).is_empty());
    }

    #[test]
    fn paused_is_not_reported_twice() {
        let mut sub = healthy();
        sub.paused = true;
        sub.admission = Admission::Deny {
            reason: DenyReason::Paused,
        };
        let found = badges(&sub, Utc::now());
        assert_eq!(found, [Badge::new(BadgeKind::Paused, "paused")]);
    }

    #[test]
    fn reports_offline_denial_and_cooldown() {
        let now = Utc::now();
        let mut sub = healthy();
        sub.online = false;
        sub.admission = Admission::Deny {
            reason: DenyReason::Reserve {
                window: "five_hour".to_owned(),
            },
        };
        sub.cooldown_until = Some(now + Duration::minutes(4));
        sub.windows.truncate(1);
        sub.windows[0].id = "five_hour".to_owned();
        sub.windows[0].duration_minutes = Some(300);
        let texts: Vec<String> = badges(&sub, now).into_iter().map(|b| b.text).collect();
        assert_eq!(
            texts,
            ["offline", "reserve reached · 5-hour", "cooldown 4m"]
        );
    }

    #[test]
    fn expired_cooldown_is_hidden() {
        let now = Utc::now();
        let mut sub = healthy();
        sub.cooldown_until = Some(now - Duration::minutes(1));
        assert!(badges(&sub, now).is_empty());
    }
}

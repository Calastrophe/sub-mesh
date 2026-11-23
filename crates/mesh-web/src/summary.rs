//! Picking and aggregating usage windows across subscriptions.

use chrono::{DateTime, Utc};
use mesh_core::api::Subscription;
use mesh_core::{AccountId, Provider, Window, WindowClass};

/// Providers in display order.
pub const PROVIDERS: [Provider; 2] = [Provider::Claude, Provider::Codex];

pub fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
    }
}

/// Which providers a list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Only(Provider),
}

impl Filter {
    pub fn matches(self, provider: Provider) -> bool {
        match self {
            Self::All => true,
            Self::Only(only) => only == provider,
        }
    }

    /// The providers this filter shows, in display order.
    pub fn providers(self) -> impl Iterator<Item = Provider> {
        PROVIDERS.into_iter().filter(move |&p| self.matches(p))
    }

    /// How many of `subs` this filter shows.
    pub fn count(self, subs: &[Subscription]) -> usize {
        subs.iter().filter(|s| self.matches(s.provider)).count()
    }
}

/// The subscription's window of `class`, if it reports one.
pub fn window_of(windows: &[Window], class: WindowClass) -> Option<&Window> {
    windows.iter().find(|w| w.class() == class)
}

/// One window summed over a provider's subscriptions.
#[derive(Clone, Debug, PartialEq)]
pub struct Aggregate {
    /// Sum of the used fractions, so 5 subscriptions at 80% give 4.0.
    pub used_sum: f64,
    /// Used fraction per subscription; `None` where it has no such window.
    pub segments: Vec<Option<f64>>,
    /// The soonest reset still ahead of `now`.
    pub earliest_reset: Option<DateTime<Utc>>,
}

impl Aggregate {
    /// Whether any subscription reported this window.
    pub fn has_data(&self) -> bool {
        self.segments.iter().any(Option::is_some)
    }
}

fn aggregate(subs: &[&Subscription], now: DateTime<Utc>, class: WindowClass) -> Aggregate {
    let windows: Vec<Option<&Window>> = subs.iter().map(|s| window_of(&s.windows, class)).collect();
    let segments: Vec<Option<f64>> = windows.iter().map(|w| w.map(|w| w.used_at(now))).collect();
    Aggregate {
        used_sum: segments.iter().flatten().sum(),
        earliest_reset: windows
            .iter()
            .flatten()
            .filter_map(|w| w.resets_at)
            .filter(|&at| at > now)
            .min(),
        segments,
    }
}

/// Everything a provider summary card shows.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderSummary {
    pub subscriptions: usize,
    pub weekly: Aggregate,
}

pub fn summarize(provider: Provider, subs: &[Subscription], now: DateTime<Utc>) -> ProviderSummary {
    let mine: Vec<&Subscription> = subs.iter().filter(|s| s.provider == provider).collect();
    ProviderSummary {
        subscriptions: mine.len(),
        weekly: aggregate(&mine, now, WindowClass::Weekly),
    }
}

/// Whether any of a provider's subscriptions reports a short window. A
/// provider where none does, like Codex on weekly-only plans, has no 5-hour
/// limit to show.
pub fn reports_five_hour(subs: &[Subscription], provider: Provider) -> bool {
    subs.iter()
        .filter(|s| s.provider == provider)
        .any(|s| window_of(&s.windows, WindowClass::FiveHour).is_some())
}

/// Ids of a provider's subscriptions in a stable display order: by label,
/// then id.
pub fn sorted_ids(subs: &[Subscription], provider: Provider) -> Vec<AccountId> {
    let mut mine: Vec<&Subscription> = subs.iter().filter(|s| s.provider == provider).collect();
    mine.sort_by_cached_key(|s| {
        (
            s.label.as_deref().unwrap_or_default().to_lowercase(),
            s.id.clone(),
        )
    });
    mine.into_iter().map(|s| s.id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::mock;

    fn window(id: &str, minutes: u32, used: f64, reset: Option<DateTime<Utc>>) -> Window {
        Window {
            id: id.to_owned(),
            duration_minutes: Some(minutes),
            used,
            resets_at: reset,
            limited: false,
        }
    }

    fn sub_with(windows: Vec<Window>, provider: Provider, online: bool) -> Subscription {
        let mut sub = mock::overview().subscriptions.remove(0);
        sub.provider = provider;
        sub.online = online;
        sub.windows = windows;
        sub
    }

    #[test]
    fn picks_each_window_by_length() {
        let windows = vec![
            window("secondary", 10_080, 0.2, None),
            window("primary", 300, 0.3, None),
        ];
        assert_eq!(
            window_of(&windows, WindowClass::FiveHour).unwrap().id,
            "primary"
        );
        assert_eq!(
            window_of(&windows, WindowClass::Weekly).unwrap().id,
            "secondary"
        );
        assert!(window_of(&[], WindowClass::Weekly).is_none());
    }

    #[test]
    fn a_weekly_only_plan_has_no_five_hour_window() {
        let windows = vec![window("primary", 10_080, 0.4, None)];
        assert!(window_of(&windows, WindowClass::FiveHour).is_none());
        assert_eq!(
            window_of(&windows, WindowClass::Weekly).unwrap().id,
            "primary"
        );
    }

    #[test]
    fn five_hour_is_shown_only_where_a_subscription_reports_it() {
        let weekly =
            |provider| sub_with(vec![window("primary", 10_080, 0.4, None)], provider, true);
        let short = sub_with(
            vec![window("five_hour", 300, 0.2, None)],
            Provider::Claude,
            true,
        );
        let subs = vec![weekly(Provider::Codex), weekly(Provider::Claude), short];
        assert!(reports_five_hour(&subs, Provider::Claude));
        assert!(!reports_five_hour(&subs, Provider::Codex));
        assert!(!reports_five_hour(&[], Provider::Claude));
        assert!(
            summarize(Provider::Codex, &subs, Utc::now())
                .weekly
                .has_data()
        );
    }

    #[test]
    fn mock_codex_mixes_weekly_only_and_five_hour_plans() {
        let subs = mock::overview().subscriptions;
        assert!(reports_five_hour(&subs, Provider::Codex));
        assert!(reports_five_hour(&subs, Provider::Claude));
        let classes: Vec<Vec<WindowClass>> = subs
            .iter()
            .filter(|s| s.provider == Provider::Codex)
            .map(|s| s.windows.iter().map(Window::class).collect())
            .collect();
        assert!(classes.contains(&vec![WindowClass::Weekly]));
        assert!(classes.contains(&vec![WindowClass::FiveHour, WindowClass::Weekly]));
    }

    #[test]
    fn sums_usage_and_finds_earliest_future_reset() {
        let now = Utc::now();
        let soon = now + Duration::days(1);
        let subs = vec![
            sub_with(
                vec![window(
                    "seven_day",
                    10_080,
                    0.8,
                    Some(now + Duration::days(3)),
                )],
                Provider::Claude,
                true,
            ),
            sub_with(
                vec![window("seven_day", 10_080, 0.6, Some(soon))],
                Provider::Claude,
                false,
            ),
            sub_with(
                vec![window(
                    "seven_day",
                    10_080,
                    1.0,
                    Some(now - Duration::hours(1)),
                )],
                Provider::Claude,
                true,
            ),
            sub_with(
                vec![window("five_hour", 300, 0.5, None)],
                Provider::Claude,
                true,
            ),
            sub_with(
                vec![window("secondary", 10_080, 0.1, None)],
                Provider::Codex,
                true,
            ),
        ];
        let summary = summarize(Provider::Claude, &subs, now);
        assert_eq!(summary.subscriptions, 4);
        assert!((summary.weekly.used_sum - 1.4).abs() < 1e-9);
        assert_eq!(
            summary.weekly.segments,
            [Some(0.8), Some(0.6), Some(0.0), None]
        );
        assert_eq!(summary.weekly.earliest_reset, Some(soon));
    }

    #[test]
    fn empty_provider_has_no_data() {
        let summary = summarize(Provider::Codex, &[], Utc::now());
        assert_eq!(summary.subscriptions, 0);
        assert!(!summary.weekly.has_data());
        assert_eq!(summary.weekly.earliest_reset, None);
    }

    #[test]
    fn filter_counts_and_providers() {
        let subs = mock::overview().subscriptions;
        let claude = Filter::Only(Provider::Claude).count(&subs);
        let codex = Filter::Only(Provider::Codex).count(&subs);
        assert_eq!(Filter::All.count(&subs), subs.len());
        assert_eq!(claude + codex, subs.len());
        assert_eq!(Filter::All.providers().count(), 2);
        assert_eq!(
            Filter::Only(Provider::Codex)
                .providers()
                .collect::<Vec<_>>(),
            [Provider::Codex]
        );
    }

    #[test]
    fn sorted_ids_are_stable_and_provider_scoped() {
        let subs = mock::overview().subscriptions;
        let ids = sorted_ids(&subs, Provider::Claude);
        assert_eq!(ids.len(), Filter::Only(Provider::Claude).count(&subs));
        let mut again = ids.clone();
        again.reverse();
        assert_ne!(ids, again);
        assert_eq!(ids, sorted_ids(&subs, Provider::Claude));
    }
}

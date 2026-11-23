//! Pure helpers for the usage page: the periods, the per-user aggregation of
//! the server's rows, and the short texts its tables show.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use mesh_core::api::{TokenUsage, UsageRow};
use mesh_core::{AccountId, Provider};

use crate::format::{display_label, percent};
use crate::summary::Filter;

/// A period the page can cover.
#[derive(Clone, Copy, Debug)]
pub struct Period {
    /// The short name on the control.
    pub label: &'static str,
    /// The name in a sentence, after "the last".
    pub name: &'static str,
    pub hours: u32,
}

/// The periods in display order.
pub const PERIODS: [Period; 4] = [
    Period {
        label: "5h",
        name: "5 hours",
        hours: 5,
    },
    Period {
        label: "24h",
        name: "24 hours",
        hours: 24,
    },
    Period {
        label: "7d",
        name: "7 days",
        hours: 168,
    },
    Period {
        label: "14d",
        name: "14 days",
        hours: 336,
    },
];

/// The name of a period in a sentence, like `7 days`.
pub fn period_name(hours: u32) -> String {
    PERIODS
        .iter()
        .find(|period| period.hours == hours)
        .map_or_else(|| format!("{hours} hours"), |period| period.name.to_owned())
}

/// How many users get a color of their own in the share bar; the rest share
/// one color.
pub const NAMED_SHARES: usize = 6;

/// The CSS class of the color for the user at `rank`, counting from zero.
pub fn share_class(rank: usize) -> &'static str {
    const CLASSES: [&str; NAMED_SHARES] = [
        "share-0", "share-1", "share-2", "share-3", "share-4", "share-5",
    ];
    CLASSES.get(rank).copied().unwrap_or("share-other")
}

/// One user's requests with one provider's subscriptions, summed over their
/// subscriptions and models.
#[derive(Clone, Debug, PartialEq)]
pub struct UserUsage {
    pub login: Option<String>,
    pub requests: u64,
    pub tokens: TokenUsage,
    pub spend: f64,
    pub last_used: DateTime<Utc>,
    /// The model that took most of the user's spend.
    pub top_model: Option<String>,
    /// The rows behind the sums, grouped by subscription, the one with the
    /// most spend first, and largest spend first within it.
    pub rows: Vec<UsageRow>,
}

/// Everything the mesh used of one provider's subscriptions.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderUsage {
    pub provider: Provider,
    pub requests: u64,
    pub tokens: TokenUsage,
    pub spend: f64,
    /// Users, largest spend first.
    pub users: Vec<UserUsage>,
}

impl ProviderUsage {
    /// The fraction of the provider's spend that `spend` is.
    pub fn share(&self, spend: f64) -> f64 {
        if self.spend > 0.0 {
            spend / self.spend
        } else {
            0.0
        }
    }
}

fn add_tokens(total: &mut TokenUsage, more: &TokenUsage) {
    total.input = total.input.saturating_add(more.input);
    total.cache_read = total.cache_read.saturating_add(more.cache_read);
    total.cache_write = total.cache_write.saturating_add(more.cache_write);
    total.output = total.output.saturating_add(more.output);
    total.reasoning = total.reasoning.saturating_add(more.reasoning);
}

fn top_model(rows: &[&UsageRow]) -> Option<String> {
    let mut by_model: HashMap<&str, f64> = HashMap::new();
    for row in rows {
        if let Some(model) = row.model.as_deref() {
            *by_model.entry(model).or_default() += row.spend;
        }
    }
    by_model
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(model, _)| model.to_owned())
}

fn sorted_rows(rows: &[&UsageRow]) -> Vec<UsageRow> {
    let mut by_account: HashMap<&AccountId, f64> = HashMap::new();
    for row in rows {
        *by_account.entry(&row.account_id).or_default() += row.spend;
    }
    let mut sorted: Vec<&UsageRow> = rows.to_vec();
    sorted.sort_by(|a, b| {
        by_account[&b.account_id]
            .total_cmp(&by_account[&a.account_id])
            .then_with(|| a.account_id.cmp(&b.account_id))
            .then_with(|| b.spend.total_cmp(&a.spend))
            .then_with(|| a.model.cmp(&b.model))
    });
    sorted.into_iter().cloned().collect()
}

fn user_usage(login: Option<&str>, rows: &[&UsageRow]) -> UserUsage {
    let mut tokens = TokenUsage::default();
    let mut requests = 0_u64;
    let mut spend = 0.0;
    let mut last_used = None;
    for row in rows {
        add_tokens(&mut tokens, &row.tokens);
        requests = requests.saturating_add(row.requests);
        spend += row.spend;
        last_used = last_used.max(Some(row.last_used));
    }
    UserUsage {
        login: login.map(str::to_owned),
        requests,
        tokens,
        spend,
        last_used: last_used.expect("a user has at least one row"),
        top_model: top_model(rows),
        rows: sorted_rows(rows),
    }
}

/// Fold the rows of `provider` into per-user usage, largest spend first.
/// `None` when no row is of that provider.
pub fn provider_usage(rows: &[UsageRow], provider: Provider) -> Option<ProviderUsage> {
    let mut by_user: HashMap<Option<&str>, Vec<&UsageRow>> = HashMap::new();
    for row in rows.iter().filter(|row| row.provider == provider) {
        by_user.entry(row.login.as_deref()).or_default().push(row);
    }
    if by_user.is_empty() {
        return None;
    }
    let mut users: Vec<UserUsage> = by_user
        .into_iter()
        .map(|(login, rows)| user_usage(login, &rows))
        .collect();
    users.sort_by(|a, b| {
        b.spend
            .total_cmp(&a.spend)
            .then_with(|| a.login.cmp(&b.login))
    });
    let mut tokens = TokenUsage::default();
    let mut requests = 0_u64;
    let mut spend = 0.0;
    for user in &users {
        add_tokens(&mut tokens, &user.tokens);
        requests = requests.saturating_add(user.requests);
        spend += user.spend;
    }
    Some(ProviderUsage {
        provider,
        requests,
        tokens,
        spend,
        users,
    })
}

/// How many distinct users have rows of the providers `filter` shows.
pub fn user_count(rows: &[UsageRow], filter: Filter) -> usize {
    rows.iter()
        .filter(|row| filter.matches(row.provider))
        .map(|row| row.login.as_deref())
        .collect::<HashSet<_>>()
        .len()
}

/// One part of the share bar.
#[derive(Clone, Debug, PartialEq)]
pub struct Slice {
    pub class: &'static str,
    /// The user the part belongs to, when it belongs to one.
    pub login: Option<String>,
    /// How many users a part that belongs to none stands for.
    pub others: usize,
    pub share: f64,
}

/// The parts of the share bar: a part for each of the first [`NAMED_SHARES`]
/// users and one for all the rest.
pub fn slices(usage: &ProviderUsage) -> Vec<Slice> {
    let mut slices: Vec<Slice> = usage
        .users
        .iter()
        .take(NAMED_SHARES)
        .enumerate()
        .map(|(rank, user)| Slice {
            class: share_class(rank),
            login: user.login.clone(),
            others: 0,
            share: usage.share(user.spend),
        })
        .collect();
    let rest = usage.users.get(NAMED_SHARES..).unwrap_or_default();
    if !rest.is_empty() {
        slices.push(Slice {
            class: share_class(NAMED_SHARES),
            login: None,
            others: rest.len(),
            share: usage.share(rest.iter().map(|user| user.spend).sum()),
        });
    }
    slices
}

/// How a user is named: `you` for the caller, the login masked unless
/// `show_full` for anyone else, and `unknown` for requests without a login.
pub fn user_label(login: Option<&str>, me: Option<&str>, show_full: bool) -> String {
    match login {
        None => "unknown".to_owned(),
        Some(login) if Some(login) == me => "you".to_owned(),
        Some(login) => display_label(login, show_full),
    }
}

/// The name of the users a part of the share bar stands for when it has no
/// single one.
pub fn others_label(count: usize) -> String {
    match count {
        1 => "1 other".to_owned(),
        count => format!("{count} others"),
    }
}

/// A share of spend, like `46%`, or `<1%` for a share that rounds to none.
pub fn share_text(fraction: f64) -> String {
    if fraction > 0.0 && (fraction * 100.0).round() < 1.0 {
        "<1%".to_owned()
    } else {
        percent(fraction)
    }
}

/// How much of the input came from the prompt cache, like `82%`, or a dash
/// when there was no input.
pub fn cached_text(tokens: &TokenUsage) -> String {
    if tokens.input == 0 {
        return "—".to_owned();
    }
    percent((tokens.cache_read as f64 / tokens.input as f64).min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;

    fn id(text: &str) -> AccountId {
        text.parse().unwrap()
    }

    fn row(
        account: &str,
        login: &str,
        model: &str,
        input: u64,
        output: u64,
        spend: f64,
        minutes_ago: i64,
    ) -> UsageRow {
        UsageRow {
            account_id: id(account),
            provider: Provider::Claude,
            login: Some(login.to_owned()),
            model: Some(model.to_owned()),
            requests: 10,
            responses: 10,
            tokens: TokenUsage {
                input,
                cache_read: input / 2,
                cache_write: 0,
                output,
                reasoning: 0,
            },
            spend,
            last_used: DateTime::UNIX_EPOCH + chrono::Duration::minutes(1000 - minutes_ago),
        }
    }

    #[test]
    fn periods_fit_the_servers_limits() {
        use mesh_core::api::{DEFAULT_USAGE_HOURS, MAX_USAGE_HOURS};
        assert!(
            PERIODS
                .iter()
                .all(|p| (1..=MAX_USAGE_HOURS).contains(&p.hours))
        );
        assert!(PERIODS.iter().any(|p| p.hours == DEFAULT_USAGE_HOURS));
        assert_eq!(period_name(168), "7 days");
        assert_eq!(period_name(5), "5 hours");
        assert_eq!(period_name(48), "48 hours");
    }

    #[test]
    fn users_are_summed_over_subscriptions_and_models() {
        let rows = [
            row("claude-aaa111", "bob", "opus", 1000, 100, 50.0, 30),
            row("claude-bbb222", "bob", "sonnet", 4000, 400, 90.0, 5),
            row("claude-aaa111", "carol", "haiku", 500, 50, 10.0, 60),
            UsageRow {
                provider: Provider::Codex,
                ..row("codex-ccc333", "bob", "gpt", 9, 9, 999.0, 1)
            },
        ];
        let usage = provider_usage(&rows, Provider::Claude).unwrap();
        assert_eq!(usage.requests, 30);
        assert_eq!(usage.tokens.input, 5500);
        assert_eq!(usage.tokens.output, 550);
        assert!((usage.spend - 150.0).abs() < 1e-9);
        let logins: Vec<_> = usage.users.iter().map(|u| u.login.as_deref()).collect();
        assert_eq!(logins, [Some("bob"), Some("carol")]);
        let bob = &usage.users[0];
        assert_eq!(bob.requests, 20);
        assert_eq!(bob.tokens.input, 5000);
        assert!((bob.spend - 140.0).abs() < 1e-9);
        assert_eq!(bob.top_model.as_deref(), Some("sonnet"));
        assert_eq!(bob.last_used, rows[1].last_used);
        assert!((usage.share(bob.spend) - 140.0 / 150.0).abs() < 1e-9);
        assert!(provider_usage(&rows, Provider::Codex).is_some());
        assert!(provider_usage(&rows[2..3], Provider::Codex).is_none());
        assert!(provider_usage(&[], Provider::Claude).is_none());
    }

    #[test]
    fn breakdown_groups_rows_by_subscription() {
        let rows = [
            row("claude-aaa111", "bob", "haiku", 1, 1, 5.0, 1),
            row("claude-bbb222", "bob", "opus", 1, 1, 60.0, 1),
            row("claude-aaa111", "bob", "sonnet", 1, 1, 30.0, 1),
            row("claude-bbb222", "bob", "sonnet", 1, 1, 10.0, 1),
        ];
        let usage = provider_usage(&rows, Provider::Claude).unwrap();
        let parts: Vec<_> = usage.users[0]
            .rows
            .iter()
            .map(|r| (r.account_id.as_str(), r.model.as_deref().unwrap()))
            .collect();
        assert_eq!(
            parts,
            [
                ("claude-bbb222", "opus"),
                ("claude-bbb222", "sonnet"),
                ("claude-aaa111", "sonnet"),
                ("claude-aaa111", "haiku"),
            ]
        );
    }

    #[test]
    fn a_user_without_models_has_no_top_model() {
        let mut quiet = row("claude-aaa111", "bob", "x", 1, 1, 1.0, 1);
        quiet.model = None;
        let usage = provider_usage(&[quiet], Provider::Claude).unwrap();
        assert_eq!(usage.users[0].top_model, None);
    }

    #[test]
    fn no_spend_has_no_share() {
        let usage = provider_usage(
            &[row("claude-aaa111", "bob", "opus", 0, 0, 0.0, 1)],
            Provider::Claude,
        )
        .unwrap();
        assert!(usage.share(0.0).abs() < 1e-9);
        assert_eq!(slices(&usage)[0].share, 0.0);
    }

    #[test]
    fn the_bar_folds_the_smallest_users_together() {
        let rows: Vec<UsageRow> = (0..9)
            .map(|n| {
                row(
                    "claude-aaa111",
                    &format!("user{n}"),
                    "opus",
                    1,
                    1,
                    100.0 - n as f64,
                    1,
                )
            })
            .collect();
        let usage = provider_usage(&rows, Provider::Claude).unwrap();
        let parts = slices(&usage);
        assert_eq!(parts.len(), NAMED_SHARES + 1);
        assert_eq!(parts[0].class, "share-0");
        assert_eq!(parts[0].login.as_deref(), Some("user0"));
        let last = parts.last().unwrap();
        assert_eq!(
            (last.class, last.others, last.login.as_deref()),
            ("share-other", 3, None)
        );
        assert!((parts.iter().map(|p| p.share).sum::<f64>() - 1.0).abs() < 1e-9);
        let few = provider_usage(&rows[..2], Provider::Claude).unwrap();
        assert_eq!(slices(&few).len(), 2);
    }

    #[test]
    fn users_are_named_for_the_viewer() {
        let me = Some("alice@example.com");
        assert_eq!(user_label(me, me, false), "you");
        assert_eq!(user_label(me, me, true), "you");
        assert_eq!(
            user_label(Some("bob@example.com"), me, true),
            "bob@example.com"
        );
        assert_eq!(
            user_label(Some("bob@example.com"), me, false),
            "b•••@ex•••.com"
        );
        assert_eq!(user_label(None, me, true), "unknown");
        assert_eq!(
            user_label(Some("bob@example.com"), None, true),
            "bob@example.com"
        );
        assert_eq!(others_label(1), "1 other");
        assert_eq!(others_label(4), "4 others");
    }

    #[test]
    fn shares_never_round_to_nothing() {
        assert_eq!(share_text(0.0), "0%");
        assert_eq!(share_text(0.002), "<1%");
        assert_eq!(share_text(0.006), "1%");
        assert_eq!(share_text(0.464), "46%");
        assert_eq!(share_text(1.0), "100%");
    }

    #[test]
    fn cached_is_the_cache_read_share_of_input() {
        let tokens = TokenUsage {
            input: 1000,
            cache_read: 820,
            ..TokenUsage::default()
        };
        assert_eq!(cached_text(&tokens), "82%");
        assert_eq!(cached_text(&TokenUsage::default()), "—");
    }

    #[test]
    fn counts_distinct_users_of_the_filtered_providers() {
        let report = mock::usage(168);
        let all = user_count(&report.rows, Filter::All);
        let claude = user_count(&report.rows, Filter::Only(Provider::Claude));
        let codex = user_count(&report.rows, Filter::Only(Provider::Codex));
        assert_eq!(all, 3);
        assert!(claude <= all && codex <= all && claude.max(codex) >= 2);
    }

    #[test]
    fn mock_usage_aggregates_for_both_providers() {
        let report = mock::usage(168);
        for provider in [Provider::Claude, Provider::Codex] {
            let usage = provider_usage(&report.rows, provider).unwrap();
            assert!(usage.users.len() >= 2);
            assert!(usage.users.windows(2).all(|p| p[0].spend >= p[1].spend));
            let shares: f64 = usage.users.iter().map(|u| usage.share(u.spend)).sum();
            assert!((shares - 1.0).abs() < 1e-9);
        }
    }
}

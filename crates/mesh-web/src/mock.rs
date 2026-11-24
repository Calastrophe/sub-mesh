//! Built-in sample data for `?mock`. Mutations change this in-memory state.

use std::cell::RefCell;

use chrono::{DateTime, Duration, Utc};
use mesh_core::api::{
    DaemonInfo, Direction, LogEntry, Me, Overview, Subscription, TokenUsage, Transport,
    UsageReport, UsageRow, UserActivity,
};
use mesh_core::spend::spend;
use mesh_core::{
    AccountId, Admission, DenyReason, ModelAllowlist, Provider, ReserveConfig, Score, Window,
    admission,
};

use crate::api::{Error, Result};

mod settings;

pub use settings::{decide, request_access, session, set_entry, set_value, settings};

const ME: &str = "alice@example.com";
const BOB: &str = "bob@example.com";
const CAROL: &str = "carol@example.com";
const USERS: [&str; 3] = [ME, BOB, CAROL];

/// Whether a URL query string (like `?mock&x=1`) asks for mock mode.
pub fn query_enables_mock(search: &str) -> bool {
    search
        .trim_start_matches('?')
        .split('&')
        .any(|pair| pair == "mock" || pair.starts_with("mock="))
}

/// Whether this page was opened with `?mock`.
pub fn enabled() -> bool {
    thread_local! {
        static ENABLED: bool = web_sys::window()
            .and_then(|window| window.location().search().ok())
            .is_some_and(|search| query_enables_mock(&search));
    }
    ENABLED.with(|enabled| *enabled)
}

thread_local! {
    static STATE: RefCell<Overview> = RefCell::new(sample(Utc::now()));
}

/// The current mock state, stamped with the current time.
pub fn overview() -> Overview {
    STATE.with_borrow(|state| Overview {
        now: Utc::now(),
        ..state.clone()
    })
}

pub fn set_reserve(id: &AccountId, reserve: Option<ReserveConfig>) -> Result<Subscription> {
    change(id, |sub| sub.reserve = reserve.unwrap_or_default())
}

pub fn set_models(id: &AccountId, models: Option<ModelAllowlist>) -> Result<Subscription> {
    change(id, |sub| sub.models = models)
}

pub fn set_paused(id: &AccountId, paused: bool) -> Result<Subscription> {
    change(id, |sub| sub.paused = paused)
}

pub fn remove(id: &AccountId) -> Result<()> {
    STATE.with_borrow_mut(|state| {
        let index = owned_index(state, id)?;
        state.subscriptions.remove(index);
        Ok(())
    })
}

/// How many entries the sample log holds.
const LOG_LEN: i64 = 160;

/// The sample log of the signed-in user, newest first: the entries of
/// `direction` (both when `None`) with an id below `before`, at most `limit`
/// of them. Ids are fixed, so paging with `before` walks one stable list.
pub fn logs(limit: u32, before: Option<i64>, direction: Option<Direction>) -> Vec<LogEntry> {
    let now = Utc::now();
    STATE.with_borrow(|state| {
        (0..LOG_LEN)
            .map(|index| log_entry(index, now, &state.subscriptions))
            .filter(|entry| direction.is_none_or(|direction| entry.direction == direction))
            .filter(|entry| before.is_none_or(|before| entry.id < before))
            .take(limit as usize)
            .collect()
    })
}

/// One sample group of requests, as they ran in a week: who made them with
/// which of the signed-in user's subscriptions and model.
struct UsageSpec {
    account: &'static str,
    login: &'static str,
    model: &'static str,
    requests: u64,
    cached_percent: u64,
    minutes_ago: i64,
    min_hours: u32,
}

const fn usage_spec(
    account: &'static str,
    login: &'static str,
    model: &'static str,
    requests: u64,
    cached_percent: u64,
    minutes_ago: i64,
    min_hours: u32,
) -> UsageSpec {
    UsageSpec {
        account,
        login,
        model,
        requests,
        cached_percent,
        minutes_ago,
        min_hours,
    }
}

const USAGE_SPECS: [UsageSpec; 16] = [
    usage_spec("claude-3f9a01", ME, "claude-opus-5-5", 410, 93, 12, 1),
    usage_spec("claude-3f9a01", ME, "claude-sonnet-5-5", 980, 91, 4, 1),
    usage_spec("claude-3f9a01", BOB, "claude-opus-5-5", 140, 88, 95, 24),
    usage_spec("claude-3f9a01", BOB, "claude-sonnet-5-5", 520, 90, 31, 5),
    usage_spec("claude-3f9a01", CAROL, "claude-haiku-5-5", 260, 80, 380, 24),
    usage_spec("claude-3f9a01", CAROL, "claude-sonnet-5-5", 90, 86, 600, 24),
    usage_spec("claude-77b2c4", ME, "claude-sonnet-5-5", 300, 92, 22, 5),
    usage_spec("claude-77b2c4", ME, "claude-haiku-5-5", 600, 70, 9, 5),
    usage_spec("claude-77b2c4", BOB, "claude-sonnet-5-5", 210, 89, 140, 24),
    usage_spec("claude-5e5e5e", BOB, "claude-sonnet-5-5", 70, 90, 3000, 168),
    usage_spec("codex-1a2b3c", ME, "gpt-6.1-sol", 220, 62, 17, 5),
    usage_spec("codex-1a2b3c", ME, "gpt-6-luna", 90, 58, 40, 24),
    usage_spec("codex-1a2b3c", BOB, "gpt-6.1-sol", 160, 55, 70, 24),
    usage_spec("codex-1a2b3c", BOB, "gpt-6-luna", 340, 48, 55, 24),
    usage_spec("codex-1a2b3c", CAROL, "gpt-6-luna", 520, 52, 210, 24),
    usage_spec("codex-1a2b3c", CAROL, "gpt-6.1-sol", 40, 60, 1800, 168),
];

/// The tokens one request of `model` uses on average, as input and output.
fn average_request(model: &str) -> (u64, u64) {
    match model {
        m if m.contains("opus") => (46_000, 1_400),
        m if m.contains("sonnet") => (31_000, 1_000),
        m if m.contains("haiku") => (14_000, 600),
        m if m.contains("luna") => (24_000, 1_300),
        _ => (52_000, 2_600),
    }
}

fn usage_row(index: usize, spec: &UsageSpec, hours: u32, now: DateTime<Utc>) -> UsageRow {
    let provider = if spec.account.starts_with("claude") {
        Provider::Claude
    } else {
        Provider::Codex
    };
    let scale = f64::from(hours) / 168.0;
    let requests = ((spec.requests as f64 * scale).round() as u64).max(1);
    let (input, output) = average_request(spec.model);
    let variety = 100 + (index as u64 % 4) * 7;
    let input = input * requests * variety / 100;
    let output = output * requests * variety / 100;
    let tokens = TokenUsage {
        input,
        cache_read: input * spec.cached_percent / 100,
        cache_write: if provider == Provider::Claude {
            input * 3 / 100
        } else {
            0
        },
        output,
        reasoning: if provider == Provider::Codex {
            output * 38 / 100
        } else {
            0
        },
    };
    let age = ((spec.minutes_ago as f64 * scale).round() as i64).max(1);
    UsageRow {
        account_id: spec.account.parse().expect("sample ids are valid"),
        provider,
        login: Some(spec.login.to_owned()),
        model: Some(spec.model.to_owned()),
        requests,
        responses: if provider == Provider::Codex {
            requests * 13 / 10
        } else {
            requests
        },
        tokens,
        spend: spend(provider, Some(spec.model), &tokens),
        last_used: now - Duration::minutes(age),
    }
}

/// What the mesh used of the signed-in user's subscriptions in the last
/// `hours`. The sample covers a week and scales with the period; some users
/// only show up in longer ones.
pub fn usage(hours: u32) -> UsageReport {
    let now = Utc::now();
    STATE.with_borrow(|state| {
        let mine: Vec<&AccountId> = state
            .subscriptions
            .iter()
            .filter(|sub| sub.owner.as_deref() == Some(ME))
            .map(|sub| &sub.id)
            .collect();
        let mut rows: Vec<UsageRow> = USAGE_SPECS
            .iter()
            .enumerate()
            .filter(|(_, spec)| spec.min_hours <= hours.max(1))
            .map(|(index, spec)| usage_row(index, spec, hours, now))
            .filter(|row| mine.contains(&&row.account_id))
            .collect();
        rows.sort_by(|a, b| b.spend.total_cmp(&a.spend));
        UsageReport {
            since: now - Duration::hours(i64::from(hours)),
            rows,
        }
    })
}

/// A tiny deterministic generator, so the sample log looks the same on every
/// load.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn session(&mut self) -> String {
        format!(
            "{:08x}-{:04x}-4{:03x}",
            self.next() as u32,
            self.next() as u16,
            self.next() as u16 & 0xfff
        )
    }

    fn claude_usage(&mut self) -> TokenUsage {
        let cache_read = 2_000 + self.below(60_000);
        let cache_write = self.below(4_000);
        TokenUsage {
            input: cache_read + cache_write + self.below(3_000),
            cache_read,
            cache_write,
            output: 80 + self.below(2_400),
            reasoning: 0,
        }
    }

    fn codex_usage(&mut self, responses: u64) -> TokenUsage {
        let input = (3_000 + self.below(30_000)) * responses;
        let output = (150 + self.below(2_500)) * responses;
        TokenUsage {
            input,
            cache_read: input * self.below(90) / 100,
            cache_write: 0,
            output,
            reasoning: output * self.below(70) / 100,
        }
    }
}

fn log_entry(index: i64, now: DateTime<Utc>, subs: &[Subscription]) -> LogEntry {
    let mut rng = Rng((index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let age = Duration::seconds(index * 83 + rng.below(40) as i64 + 20);
    let claude_share = rng.below(5) < 2;
    let inbound = matches!(index, 3 | 4 | 7) || (index > 9 && rng.below(4) == 0);
    let provider = match index {
        1 | 4 | 5 | 7 => Provider::Claude,
        0 | 2 | 3 | 9 => Provider::Codex,
        _ if claude_share => Provider::Claude,
        _ => Provider::Codex,
    };
    let of_provider: Vec<&Subscription> = subs
        .iter()
        .filter(|sub| sub.provider == provider)
        .filter(|sub| !inbound || sub.owner.as_deref() == Some(ME))
        .collect();
    let sub = (!of_provider.is_empty())
        .then(|| of_provider[rng.below(of_provider.len() as u64) as usize]);
    let (direction, login, consumer) = if inbound {
        let (login, nodes) = if rng.below(2) == 0 {
            (BOB, ["mac-mini", "bob-laptop"])
        } else {
            (CAROL, ["lab-gpu", "carol-mbp"])
        };
        (Direction::Inbound, login, nodes[rng.below(2) as usize])
    } else {
        let nodes = ["bb-1", "alice-mbp", "alice-desktop"];
        (Direction::Outbound, ME, nodes[rng.below(3) as usize])
    };
    let (path, client, models) = match provider {
        Provider::Claude => (
            "v1/messages",
            "claude-cli/2.4.0 (external, cli)",
            &CLAUDE_MODELS[..3],
        ),
        Provider::Codex => ("responses", "codex_cli_rs/0.9.1", &CODEX_MODELS[..3]),
    };
    let mut entry = LogEntry {
        id: LOG_LEN - index,
        ts: now - age,
        direction,
        login: Some(login.to_owned()),
        consumer: consumer.to_owned(),
        provider,
        transport: Transport::Http,
        method: "POST".to_owned(),
        path: path.to_owned(),
        model: Some(models[rng.below(3) as usize].to_owned()),
        client: Some(client.to_owned()),
        session: None,
        account_id: sub.map(|sub| sub.id.clone()),
        owner: sub.and_then(|sub| sub.owner.clone()),
        node: sub.map(|sub| sub.node.clone()),
        status: 200,
        error: None,
        latency_ms: 180 + rng.below(1800),
        duration_ms: None,
        attempts: 1,
        responses: 1,
        usage: None,
    };
    let kind = rng.below(6);
    match (index, provider, kind) {
        (3, ..) => {
            entry.transport = Transport::Websocket;
            entry.method = "GET".to_owned();
            entry.status = 101;
            entry.latency_ms = 140;
            entry.session = Some(rng.session());
            entry.responses = 3;
            entry.usage = Some(rng.codex_usage(3));
            entry.duration_ms = Some(184_000);
        }
        (4, ..) => {
            entry.model = Some("claude-sonnet-5-5".to_owned());
            entry.session = Some(rng.session());
            entry.usage = Some(rng.claude_usage());
            entry.duration_ms = Some(entry.latency_ms + 9_300);
        }
        (7, ..) => {
            entry.status = 529;
            entry.error = Some("overloaded_error: Overloaded".to_owned());
            entry.attempts = 2;
            entry.responses = 0;
            entry.duration_ms = Some(entry.latency_ms + 25);
        }
        (0, ..) => {
            entry.transport = Transport::Websocket;
            entry.method = "GET".to_owned();
            entry.status = 101;
            entry.latency_ms = 210;
            entry.session = Some(rng.session());
            entry.responses = 4;
            entry.usage = Some(rng.codex_usage(4));
        }
        (1, ..) => {
            entry.latency_ms = 640;
            entry.session = Some(rng.session());
        }
        (2, ..) => {
            entry.model = Some("gpt-7-sol".to_owned());
            entry.status = 404;
            entry.error = Some("model_not_found: no subscription allows gpt-7-sol".to_owned());
            entry.account_id = None;
            entry.node = None;
            entry.latency_ms = 3;
            entry.duration_ms = Some(4);
            entry.attempts = 0;
            entry.responses = 0;
        }
        (5, ..) => {
            entry.status = 429;
            entry.error = Some(
                "rate_limit_error: this request would exceed the account's rate limit".to_owned(),
            );
            entry.attempts = 3;
            entry.responses = 0;
            entry.duration_ms = Some(entry.latency_ms + 6);
        }
        (9, ..) => {
            entry.status = 502;
            entry.error =
                Some("upstream connect error: every codex subscription failed".to_owned());
            entry.attempts = 2;
            entry.responses = 0;
            entry.duration_ms = Some(entry.latency_ms + 40);
        }
        (_, Provider::Codex, 0..=2) => {
            entry.transport = Transport::Websocket;
            entry.method = "GET".to_owned();
            entry.status = 101;
            entry.latency_ms = 90 + rng.below(400);
            entry.session = Some(rng.session());
            entry.responses = 2 + rng.below(7) as u32;
            entry.usage = Some(rng.codex_usage(u64::from(entry.responses)));
            entry.duration_ms = Some(20_000 + rng.below(900_000));
        }
        (_, Provider::Codex, _) => {
            entry.session = (rng.below(2) == 0).then(|| rng.session());
            entry.usage = Some(rng.codex_usage(1));
            entry.duration_ms = Some(entry.latency_ms + 500 + rng.below(20_000));
        }
        (_, Provider::Claude, _) => {
            entry.session = (rng.below(3) != 0).then(|| rng.session());
            entry.usage = Some(rng.claude_usage());
            entry.duration_ms = Some(entry.latency_ms + 400 + rng.below(25_000));
            if rng.below(9) == 0 {
                entry.attempts = 2;
            }
        }
    }
    entry
}

fn owned_index(state: &Overview, id: &AccountId) -> Result<usize> {
    let index = state
        .subscriptions
        .iter()
        .position(|sub| &sub.id == id)
        .ok_or_else(|| Error::Api {
            status: 404,
            message: format!("unknown subscription {id}"),
        })?;
    if state.subscriptions[index].can_manage {
        Ok(index)
    } else {
        Err(Error::Api {
            status: 403,
            message: "only the owner can change this subscription".to_owned(),
        })
    }
}

fn change(id: &AccountId, edit: impl FnOnce(&mut Subscription)) -> Result<Subscription> {
    STATE.with_borrow_mut(|state| {
        let index = owned_index(state, id)?;
        let sub = &mut state.subscriptions[index];
        edit(sub);
        refresh_admission(sub, Utc::now());
        Ok(sub.clone())
    })
}

/// Recompute the decision and score the way the server would.
fn refresh_admission(sub: &mut Subscription, now: DateTime<Utc>) {
    sub.admission = if sub.paused {
        Admission::Deny {
            reason: DenyReason::Paused,
        }
    } else {
        admission(&sub.windows, &sub.reserve, now)
    };
    let routable = sub.online && sub.cooldown_until.is_none_or(|until| until <= now);
    sub.score = (sub.admission.is_admit() && routable)
        .then(|| Score::new(&sub.windows, &sub.reserve, sub.in_flight, now).value);
}

fn window(
    id: &str,
    minutes: u32,
    used: f64,
    reset: Option<Duration>,
    now: DateTime<Utc>,
) -> Window {
    Window {
        id: id.to_owned(),
        duration_minutes: Some(minutes),
        used,
        resets_at: reset.map(|after| now + after),
        limited: false,
    }
}

const CLAUDE_MODELS: [&str; 5] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-opus-4-5",
    "claude-sonnet-4-5",
];
const CODEX_MODELS: [&str; 4] = [
    "gpt-6.2-sol",
    "gpt-6.1-sol",
    "gpt-6-luna",
    "gpt-6-luna-mini",
];

fn listed(models: &[&str]) -> Vec<String> {
    models.iter().map(|&model| model.to_owned()).collect()
}

fn allowlist(patterns: &[&str]) -> Option<ModelAllowlist> {
    Some(ModelAllowlist::new(patterns.iter().copied()).expect("sample patterns are valid"))
}

const FIVE_HOURS: u32 = 300;
const WEEK: u32 = 10_080;

fn reserve(default: f64, five_hour: Option<f64>, weekly: Option<f64>) -> ReserveConfig {
    ReserveConfig {
        default,
        five_hour,
        weekly,
    }
}

fn sample(now: DateTime<Utc>) -> Overview {
    let h = Duration::hours;
    let m = Duration::minutes;
    let subscription =
        |id: &str, provider, label: &str, owner: &str, node: &str, windows| Subscription {
            id: id.parse().expect("sample ids are valid"),
            provider,
            label: Some(label.to_owned()),
            owner: Some(owner.to_owned()),
            node: node.to_owned(),
            online: true,
            paused: false,
            reserve: ReserveConfig::default(),
            models: None,
            available_models: listed(match provider {
                Provider::Claude => &CLAUDE_MODELS,
                Provider::Codex => &CODEX_MODELS,
            }),
            windows,
            admission: Admission::Admit { headroom: 1.0 },
            cooldown_until: None,
            in_flight: 0,
            requests_last_hour: 0,
            score: None,
            can_manage: owner == ME,
            updated_at: now - Duration::seconds(20),
        };
    let claude = |id, label, owner, node, windows| {
        subscription(id, Provider::Claude, label, owner, node, windows)
    };
    let codex = |id, label, owner, node, windows| {
        subscription(id, Provider::Codex, label, owner, node, windows)
    };
    let claude_windows = |five: (f64, Option<Duration>), week: (f64, Duration)| {
        vec![
            window("five_hour", FIVE_HOURS, five.0, five.1, now),
            window("seven_day", WEEK, week.0, Some(week.1), now),
        ]
    };
    let codex_windows =
        |week: (f64, Duration)| vec![window("primary", WEEK, week.0, Some(week.1), now)];
    let codex_plus_windows = |five: (f64, Duration), week: (f64, Duration)| {
        vec![
            window("primary", FIVE_HOURS, five.0, Some(five.1), now),
            window("secondary", WEEK, week.0, Some(week.1), now),
        ]
    };
    let busy = codex_windows((0.17, h(49)));
    let mut limited = claude_windows((0.99, Some(h(3))), (0.93, h(100)));
    limited[0].limited = true;
    let cooling = codex_windows((0.88, h(30)));

    let mut subscriptions = vec![
        claude(
            "claude-3f9a01",
            "claude-tester@lab.example.com",
            ME,
            "bb-1",
            claude_windows((0.12, Some(h(3))), (0.58, h(37))),
        ),
        Subscription {
            reserve: reserve(0.3, Some(0.2), Some(0.15)),
            models: allowlist(&["claude-sonnet-*", "claude-haiku-5-5"]),
            in_flight: 2,
            ..claude(
                "claude-77b2c4",
                "claude-dev@gmail.com",
                ME,
                "bb-1",
                claude_windows((0.62, Some(m(70))), (0.81, h(96))),
            )
        },
        claude(
            "claude-c0de11",
            "claude-team@acme.io",
            BOB,
            "mac-mini",
            limited,
        ),
        Subscription {
            online: false,
            available_models: Vec::new(),
            ..claude(
                "claude-91aa7e",
                "claude-old@proton.me",
                BOB,
                "old-box",
                claude_windows((0.05, None), (0.34, h(60))),
            )
        },
        Subscription {
            reserve: reserve(0.5, None, None),
            paused: true,
            ..claude(
                "claude-5e5e5e",
                "claude-spare@fastmail.com",
                ME,
                "bb-1",
                claude_windows((0.0, None), (0.12, h(140))),
            )
        },
        Subscription {
            in_flight: 1,
            ..codex("codex-1a2b3c", "codex-main@example.com", ME, "bb-1", busy)
        },
        Subscription {
            reserve: reserve(0.2, None, None),
            cooldown_until: Some(now + m(4)),
            ..codex(
                "codex-4d5e6f",
                "codex-lab@example.org",
                CAROL,
                "lab-gpu",
                cooling,
            )
        },
        Subscription {
            models: allowlist(&["gpt-6.*-sol", "gpt-6-luna"]),
            ..codex(
                "codex-7a8b9c",
                "codex-extra@example.net",
                BOB,
                "mac-mini",
                codex_plus_windows((0.36, h(2)), (0.42, h(121))),
            )
        },
    ];
    for sub in &mut subscriptions {
        sub.requests_last_hour = u64::from(sub.in_flight) * 9 + 3;
        refresh_admission(sub, now);
    }

    let daemon = |node: &str, owner: &str, seen: Duration, online: bool| DaemonInfo {
        node: node.to_owned(),
        owner: Some(owner.to_owned()),
        connected_since: online.then(|| now - seen - Duration::hours(3)),
        last_seen: now - seen,
        online,
    };
    let users = USERS
        .into_iter()
        .map(|login| UserActivity {
            login: login.to_owned(),
            subscriptions: subscriptions
                .iter()
                .filter(|sub| sub.owner.as_deref() == Some(login))
                .count() as u32,
            requests_last_hour: login.len() as u64 * 3,
        })
        .collect();
    Overview {
        now,
        me: Me {
            login: Some(ME.to_owned()),
            node: "bb-1".to_owned(),
        },
        subscriptions,
        daemons: vec![
            daemon("bb-1", ME, Duration::seconds(6), true),
            daemon("mac-mini", BOB, Duration::seconds(11), true),
            daemon("lab-gpu", CAROL, Duration::seconds(4), true),
            daemon("old-box", BOB, Duration::hours(3), false),
        ],
        users,
    }
}

#[cfg(test)]
mod tests {
    use mesh_core::WindowClass;

    use super::*;
    use crate::summary::window_of;

    #[test]
    fn detects_mock_query() {
        assert!(query_enables_mock("?mock"));
        assert!(query_enables_mock("?a=1&mock"));
        assert!(query_enables_mock("?mock=1"));
        assert!(!query_enables_mock(""));
        assert!(!query_enables_mock("?mocked"));
    }

    #[test]
    fn sample_has_varied_subscriptions() {
        let overview = sample(Utc::now());
        let subs = &overview.subscriptions;
        assert!(subs.iter().any(|s| s.provider == Provider::Claude));
        assert!(subs.iter().any(|s| s.provider == Provider::Codex));
        assert!(subs.iter().any(|s| s.can_manage) && subs.iter().any(|s| !s.can_manage));
        assert!(subs.iter().any(|s| !s.online));
        assert!(subs.iter().any(|s| s.paused));
        assert!(
            subs.iter()
                .any(|s| window_of(&s.windows, WindowClass::FiveHour).is_none())
        );
        assert!(subs.iter().all(|s| s.windows.len() <= 2));
        assert!(subs.iter().any(|s| s.cooldown_until.is_some()));
        assert!(subs.iter().any(|s| !s.admission.is_admit() && !s.paused));
        assert!(subs.iter().any(|s| s.models.is_some() && s.can_manage));
        assert!(subs.iter().any(|s| s.models.is_none()));
        assert!(subs.iter().any(|s| s.available_models.is_empty()));
        assert!(subs.iter().any(|s| !s.available_models.is_empty()));
    }

    #[test]
    fn mutations_follow_ownership_and_recompute() {
        let overview = sample(Utc::now());
        let mine = overview
            .subscriptions
            .iter()
            .find(|s| s.can_manage && !s.paused)
            .unwrap();
        let theirs = overview
            .subscriptions
            .iter()
            .find(|s| !s.can_manage)
            .unwrap();
        STATE.with_borrow_mut(|state| *state = overview.clone());

        let paused = set_paused(&mine.id, true).unwrap();
        assert!(paused.paused && !paused.admission.is_admit());
        let resumed = set_paused(&mine.id, false).unwrap();
        assert!(!resumed.paused);

        let tight = ReserveConfig {
            default: 1.0,
            ..ReserveConfig::default()
        };
        let updated = set_reserve(&mine.id, Some(tight.clone())).unwrap();
        assert_eq!(updated.reserve, tight);
        assert!(!updated.admission.is_admit());
        assert_eq!(
            set_reserve(&mine.id, None).unwrap().reserve,
            ReserveConfig::default()
        );

        let restricted = ModelAllowlist::new(["claude-sonnet-*"]).unwrap();
        let updated = set_models(&mine.id, Some(restricted.clone())).unwrap();
        assert_eq!(updated.models, Some(restricted));
        assert_eq!(set_models(&mine.id, None).unwrap().models, None);

        assert!(matches!(
            set_models(&theirs.id, None),
            Err(Error::Api { status: 403, .. })
        ));
        assert!(matches!(
            set_paused(&theirs.id, true),
            Err(Error::Api { status: 403, .. })
        ));
        assert!(matches!(
            remove(&theirs.id),
            Err(Error::Api { status: 403, .. })
        ));
        assert!(remove(&mine.id).is_ok());
        assert!(matches!(
            remove(&mine.id),
            Err(Error::Api { status: 404, .. })
        ));
    }

    #[test]
    fn logs_are_newest_first_and_page_by_id() {
        STATE.with_borrow_mut(|state| *state = sample(Utc::now()));
        let all = logs(1000, None, None);
        assert_eq!(all.len() as i64, LOG_LEN);
        assert!(all.windows(2).all(|pair| pair[0].id == pair[1].id + 1));
        assert!(all.windows(2).all(|pair| pair[0].ts >= pair[1].ts));
        let first = logs(25, None, None);
        assert_eq!(first.len(), 25);
        let second = logs(25, first.last().map(|entry| entry.id), None);
        assert_eq!(second.len(), 25);
        assert_eq!(second[0].id, first[24].id - 1);
        assert!(logs(25, Some(1), None).is_empty());
    }

    #[test]
    fn logs_cover_the_cases_the_page_shows() {
        STATE.with_borrow_mut(|state| *state = sample(Utc::now()));
        let all = logs(1000, None, None);
        assert!(
            all.iter()
                .filter(|e| e.direction == Direction::Outbound)
                .all(|e| e.login.as_deref() == Some(ME))
        );
        let has = |test: &dyn Fn(&LogEntry) -> bool| all.iter().any(test);
        assert!(has(&|e| e.transport == Transport::Websocket
            && e.responses > 1
            && e.usage.is_some()));
        assert!(has(&|e| e.duration_ms.is_none()));
        assert!(has(&|e| e.status == 404 && e.error.is_some()));
        assert!(has(&|e| e.status == 429));
        assert!(has(&|e| e.status == 502));
        assert!(has(&|e| e.session.is_some()) && has(&|e| e.session.is_none()));
        assert!(has(
            &|e| e.provider == Provider::Claude && e.transport == Transport::Http
        ));
        assert!(has(
            &|e| e.provider == Provider::Codex && e.transport == Transport::Http
        ));
        assert!(has(&|e| e.usage.is_some_and(|u| u.reasoning > 0)));
        assert!(has(&|e| e.usage.is_some_and(|u| u.cache_write > 0)));
    }

    #[test]
    fn usage_scales_with_the_period_and_stays_on_my_subscriptions() {
        STATE.with_borrow_mut(|state| *state = sample(Utc::now()));
        let requests = |hours| -> u64 { usage(hours).rows.iter().map(|row| row.requests).sum() };
        assert!(requests(5) < requests(24) && requests(24) < requests(168));
        assert!(requests(168) < requests(336));
        let week = usage(168);
        assert!(week.rows.windows(2).all(|p| p[0].spend >= p[1].spend));
        assert!(
            week.rows
                .iter()
                .all(|row| row.spend > 0.0 && row.last_used >= week.since)
        );
        let mine: Vec<AccountId> = STATE.with_borrow(|state| {
            state
                .subscriptions
                .iter()
                .filter(|sub| sub.owner.as_deref() == Some(ME))
                .map(|sub| sub.id.clone())
                .collect()
        });
        assert!(week.rows.iter().all(|row| mine.contains(&row.account_id)));
        let logins = |report: &UsageReport, provider| {
            report
                .rows
                .iter()
                .filter(|row| row.provider == provider)
                .filter_map(|row| row.login.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        for provider in [Provider::Claude, Provider::Codex] {
            assert_eq!(logins(&week, provider).len(), 3);
        }
        let short = usage(5);
        assert!(logins(&short, Provider::Claude).len() >= 2);
        assert_eq!(logins(&short, Provider::Codex).len(), 1);
        assert!(short.rows.iter().all(|row| row.last_used >= short.since));
    }

    #[test]
    fn usage_spend_follows_the_shared_prices() {
        STATE.with_borrow_mut(|state| *state = sample(Utc::now()));
        for row in usage(168).rows {
            assert!(
                (row.spend - spend(row.provider, row.model.as_deref(), &row.tokens)).abs() < 1e-6
            );
            assert!(row.tokens.cache_read + row.tokens.cache_write <= row.tokens.input);
        }
    }

    #[test]
    fn the_direction_filter_splits_the_log() {
        STATE.with_borrow_mut(|state| *state = sample(Utc::now()));
        let all = logs(1000, None, None);
        let outbound = logs(1000, None, Some(Direction::Outbound));
        let inbound = logs(1000, None, Some(Direction::Inbound));
        assert!(!outbound.is_empty() && !inbound.is_empty());
        assert_eq!(outbound.len() + inbound.len(), all.len());
        assert!(outbound.iter().all(|e| e.direction == Direction::Outbound));
        assert!(inbound.iter().all(|e| e.direction == Direction::Inbound));
        assert!(inbound.windows(2).all(|pair| pair[0].id > pair[1].id));

        let page = logs(3, None, Some(Direction::Inbound));
        assert_eq!(page.len(), 3);
        let next = logs(3, page.last().map(|e| e.id), Some(Direction::Inbound));
        assert!(
            next.iter()
                .all(|e| e.direction == Direction::Inbound && e.id < page[2].id)
        );
        assert_eq!(next[0].id, inbound[3].id);
    }

    #[test]
    fn inbound_entries_use_the_signed_in_users_subscriptions() {
        STATE.with_borrow_mut(|state| *state = sample(Utc::now()));
        let inbound = logs(1000, None, Some(Direction::Inbound));
        let mine: Vec<AccountId> = STATE.with_borrow(|state| {
            state
                .subscriptions
                .iter()
                .filter(|sub| sub.owner.as_deref() == Some(ME))
                .map(|sub| sub.id.clone())
                .collect()
        });
        assert!(inbound.iter().all(|e| {
            e.owner.as_deref() == Some(ME)
                && e.login.as_deref() != Some(ME)
                && e.account_id.as_ref().is_none_or(|id| mine.contains(id))
        }));
        let logins = |login: &str| inbound.iter().any(|e| e.login.as_deref() == Some(login));
        assert!(logins(BOB) && logins(CAROL));
        let has = |test: &dyn Fn(&LogEntry) -> bool| inbound.iter().any(test);
        assert!(has(
            &|e| e.transport == Transport::Websocket && e.usage.is_some()
        ));
        assert!(has(&|e| e.provider == Provider::Claude
            && e.transport == Transport::Http
            && e.usage.is_some()));
        assert!(has(&|e| e.status >= 500 && e.error.is_some()));
    }
}

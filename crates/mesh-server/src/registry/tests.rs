use mesh_core::protocol::HeadroomHint;
use mesh_core::{Admission, DenyReason, ModelAllowlist, Provider, ReserveConfig, Window};

use super::{
    AccountId, AccountReport, Caller, Candidate, DaemonReport, DaemonRow, DateTime, Registry,
    Scope, TimeDelta, Utc, WarmthKey,
};

fn t0() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

fn secs(n: i64) -> TimeDelta {
    TimeDelta::seconds(n)
}

fn id(s: &str) -> AccountId {
    s.parse().unwrap()
}

fn account(name: &str, provider: Provider, headroom: f64) -> AccountReport {
    AccountReport {
        provider,
        updated_at: t0(),
        ..crate::testing::account(name, headroom)
    }
}

fn report(node: &str, accounts: Vec<AccountReport>) -> DaemonReport {
    DaemonReport {
        sent_at: t0(),
        ..crate::testing::report(node, accounts)
    }
}

fn registry() -> Registry {
    Registry::new(secs(60), 0.02, false)
}

fn ids(list: &[Candidate]) -> Vec<&str> {
    list.iter().map(|c| c.id.as_str()).collect()
}

/// Apply a report from a daemon without a known owner. The daemon gets a
/// tunnel first unless it has one.
fn apply(r: &mut Registry, report: DaemonReport, now: DateTime<Utc>) {
    if r.tunnel(&report.node).is_none() {
        r.attach_tunnel(&report.node, None, now).unwrap();
    }
    r.apply_report(report, None, now);
}

/// A Claude account with a 5h window used `five` and a weekly window used
/// `week`. The windows reset `five_resets` and `week_resets` hours after t0.
/// `headroom` is what the daemon reports.
fn windowed(
    name: &str,
    headroom: f64,
    (five, five_resets): (f64, i64),
    (week, week_resets): (f64, i64),
) -> AccountReport {
    let window = |id: &str, minutes: u32, used: f64, resets: i64| Window {
        id: id.to_owned(),
        duration_minutes: Some(minutes),
        used,
        resets_at: Some(t0() + TimeDelta::hours(resets)),
        limited: false,
    };
    AccountReport {
        windows: vec![
            window("five_hour", 300, five, five_resets),
            window("seven_day", 10_080, week, week_resets),
        ],
        ..account(name, Provider::Claude, headroom)
    }
}

const USER: &str = "alice@example.com";

fn caller(session: Option<&str>) -> Caller<'_> {
    Caller {
        session,
        user: USER,
    }
}

fn candidates(r: &Registry, session: Option<&str>) -> Vec<Candidate> {
    r.candidates(Provider::Claude, &caller(session), None, t0())
}

#[test]
fn accounts_without_windows_tie_by_id() {
    let mut r = registry();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                account("c-low", Provider::Claude, 0.1),
                account("c-high", Provider::Claude, 0.9),
                account("c-mid-b", Provider::Claude, 0.5),
                account("c-mid-a", Provider::Claude, 0.5),
                account("x-other", Provider::Codex, 1.0),
            ],
        ),
        t0(),
    );
    let list = candidates(&r, None);
    assert_eq!(ids(&list), ["c-high", "c-low", "c-mid-a", "c-mid-b"]);
    assert_eq!(
        Some(list[0].generation),
        r.tunnel("n1").map(|t| t.generation)
    );
    assert_eq!(
        ids(&r.candidates(Provider::Codex, &Caller::default(), None, t0())),
        ["x-other"]
    );
}

#[test]
fn excludes_denied_accounts_and_stale_daemons() {
    let mut r = registry();
    let mut denied = account("c-denied", Provider::Claude, 0.0);
    denied.admission = Admission::Deny {
        reason: DenyReason::Paused,
    };
    apply(
        &mut r,
        report("n1", vec![denied, account("c-ok", Provider::Claude, 0.5)]),
        t0(),
    );
    apply(
        &mut r,
        report("n2", vec![account("c-old", Provider::Claude, 0.9)]),
        t0() - secs(120),
    );
    assert_eq!(ids(&candidates(&r, None)), ["c-ok"]);
    assert!(
        r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(61))
            .is_empty()
    );
    assert_eq!(
        ids(&r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(60))),
        ["c-ok"]
    );
}

#[test]
fn a_soon_resetting_account_with_room_beats_a_late_resetting_one() {
    let mut r = registry();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                windowed("c-a-late", 0.2, (0.5, 4), (0.1, 100)),
                windowed("c-b-soon", 0.2, (0.5, 1), (0.1, 100)),
            ],
        ),
        t0(),
    );
    assert_eq!(ids(&candidates(&r, None)), ["c-b-soon", "c-a-late"]);
}

#[test]
fn an_account_near_its_weekly_reserve_is_deprioritized() {
    let mut r = registry();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                windowed("c-a-urgent", 0.01, (0.3, 1), (0.69, 100)),
                windowed("c-b-calm", 0.4, (0.3, 3), (0.2, 100)),
            ],
        ),
        t0(),
    );
    assert_eq!(ids(&candidates(&r, None)), ["c-b-calm", "c-a-urgent"]);
}

#[test]
fn the_reserve_in_effect_shapes_the_score() {
    let mut r = registry();
    let strict = AccountReport {
        reserve: ReserveConfig {
            five_hour: Some(0.6),
            ..ReserveConfig::default()
        },
        ..windowed("c-a-strict", 0.1, (0.3, 2), (0.1, 100))
    };
    let lax = windowed("c-b-lax", 0.4, (0.3, 2), (0.1, 100));
    apply(&mut r, report("n1", vec![strict, lax]), t0());
    assert_eq!(ids(&candidates(&r, None)), ["c-b-lax", "c-a-strict"]);
}

#[test]
fn requests_in_flight_spread_load_over_equal_accounts() {
    let mut r = registry();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                account("c-a", Provider::Claude, 0.7),
                account("c-b", Provider::Claude, 0.7),
            ],
        ),
        t0(),
    );
    assert_eq!(ids(&candidates(&r, None)), ["c-a", "c-b"]);
    let first = candidates(&r, None).remove(0);
    let guard = r
        .begin(&first, Provider::Claude, t0())
        .expect("c-a is usable");
    assert_eq!(ids(&candidates(&r, None)), ["c-b", "c-a"]);
    assert_eq!(r.view(&id("c-a"), t0()).unwrap().in_flight, 1);
    let view = r.view(&id("c-a"), t0()).unwrap();
    assert_eq!(view.score, Some(first.score.value / 2.0));
    assert_eq!(view.score, Some(candidates(&r, None)[1].score.value));
    drop(guard);
    assert_eq!(ids(&candidates(&r, None)), ["c-a", "c-b"]);
    assert_eq!(r.view(&id("c-a"), t0()).unwrap().in_flight, 0);
}

#[test]
fn begin_refuses_accounts_that_are_no_longer_usable() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    let chosen = candidates(&r, None).remove(0);
    r.cooldown_account(&id("c-a"), t0() + secs(30));
    assert!(r.begin(&chosen, Provider::Claude, t0()).is_none());
    assert!(
        r.begin(&chosen, Provider::Claude, t0() + secs(31))
            .is_some()
    );
    assert!(r.begin(&chosen, Provider::Codex, t0() + secs(31)).is_none());
    r.attach_tunnel("n1", None, t0() + secs(32)).unwrap();
    assert!(
        r.begin(&chosen, Provider::Claude, t0() + secs(32))
            .is_none()
    );
}

#[test]
fn the_in_flight_count_survives_reports() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    let chosen = candidates(&r, None).remove(0);
    let guard = r.begin(&chosen, Provider::Claude, t0()).unwrap();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.4)]),
        t0() + secs(10),
    );
    assert_eq!(r.view(&id("c-a"), t0() + secs(10)).unwrap().in_flight, 1);
    drop(guard);
    assert_eq!(r.view(&id("c-a"), t0() + secs(10)).unwrap().in_flight, 0);
}

/// A registry whose daemons stay fresh long enough to test cache lifetimes.
fn patient() -> Registry {
    Registry::new(secs(1_000_000), 0.02, false)
}

/// Two accounts where `c-a-best` scores far above `c-b-sticky`, with the
/// given headroom reported for the second.
fn sticky_pair(sticky_headroom: f64) -> Registry {
    let mut r = patient();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                windowed("c-a-best", 0.4, (0.3, 1), (0.1, 100)),
                windowed("c-b-sticky", sticky_headroom, (0.3, 4), (0.1, 100)),
            ],
        ),
        t0(),
    );
    r
}

/// Record that `account` served `session` of [`USER`] on Claude at `at`.
fn serve(r: &mut Registry, session: Option<&str>, account: &str, ttl: i64, at: DateTime<Utc>) {
    r.record_warmth(
        Provider::Claude,
        &caller(session),
        &id(account),
        secs(ttl),
        at,
    );
}

#[test]
fn the_threshold_is_configurable() {
    let mut r = Registry::new(secs(60), 0.1, false);
    apply(
        &mut r,
        report(
            "n1",
            vec![
                windowed("c-a-best", 0.4, (0.3, 1), (0.1, 100)),
                windowed("c-b-sticky", 0.09, (0.3, 4), (0.1, 100)),
            ],
        ),
        t0(),
    );
    serve(&mut r, Some("s1"), "c-b-sticky", 300, t0());
    assert_eq!(ids(&candidates(&r, Some("s1"))), ["c-a-best", "c-b-sticky"]);
}

#[test]
fn warmth_is_scoped_by_provider() {
    let mut r = sticky_pair(0.4);
    r.record_warmth(
        Provider::Codex,
        &caller(Some("s1")),
        &id("c-b-sticky"),
        secs(300),
        t0(),
    );
    assert_eq!(ids(&candidates(&r, Some("s1"))), ["c-a-best", "c-b-sticky"]);
    serve(&mut r, Some("s1"), "c-b-sticky", 300, t0());
    assert_eq!(ids(&candidates(&r, Some("s1"))), ["c-b-sticky", "c-a-best"]);
}

#[test]
fn every_use_renews_the_cache() {
    let mut r = sticky_pair(0.4);
    serve(&mut r, Some("s1"), "c-b-sticky", 300, t0());
    serve(&mut r, Some("s1"), "c-b-sticky", 300, t0() + secs(250));
    let at = |after| {
        r.candidates(
            Provider::Claude,
            &caller(Some("s1")),
            None,
            t0() + secs(after),
        )
    };
    assert_eq!(ids(&at(550)), ["c-b-sticky", "c-a-best"]);
    assert_eq!(ids(&at(551)), ["c-a-best", "c-b-sticky"]);
}

#[test]
fn a_new_session_of_the_same_user_prefers_the_user_warm_account() {
    let mut r = patient();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                windowed("c-a-best", 0.4, (0.3, 3), (0.1, 100)),
                windowed("c-b-warm", 0.4, (0.3, 4), (0.1, 100)),
            ],
        ),
        t0(),
    );
    let score = |name: &str| r.usable(&id(name), t0()).unwrap().score.value;
    let ratio = score("c-a-best") / score("c-b-warm");
    assert!(
        1.0 < ratio && ratio < super::balancer::USER_WARM_BOOST,
        "{ratio}"
    );
    assert_eq!(ids(&candidates(&r, Some("s2"))), ["c-a-best", "c-b-warm"]);
    serve(&mut r, Some("s1"), "c-b-warm", 300, t0());
    assert_eq!(ids(&candidates(&r, Some("s2"))), ["c-b-warm", "c-a-best"]);
    assert_eq!(ids(&candidates(&r, None)), ["c-b-warm", "c-a-best"]);
    assert_eq!(
        ids(&r.candidates(
            Provider::Claude,
            &Caller {
                session: Some("s2"),
                user: "bob@example.com",
            },
            None,
            t0()
        )),
        ["c-a-best", "c-b-warm"]
    );
    assert_eq!(
        ids(&r.candidates(
            Provider::Claude,
            &caller(Some("s2")),
            None,
            t0() + secs(301)
        )),
        ["c-a-best", "c-b-warm"]
    );
}

#[test]
fn the_user_scope_keeps_the_latest_account() {
    let mut r = sticky_pair(0.4);
    serve(&mut r, Some("s1"), "c-b-sticky", 300, t0());
    serve(&mut r, Some("s2"), "c-a-best", 300, t0() + secs(1));
    let key = |scope, name: &str| WarmthKey {
        provider: Provider::Claude,
        scope,
        name: name.to_owned(),
    };
    assert_eq!(r.warmth[&key(Scope::User, USER)].account, id("c-a-best"));
    assert_eq!(
        r.warmth[&key(Scope::Session, "s1")].account,
        id("c-b-sticky")
    );
    assert_eq!(r.warmth[&key(Scope::Session, "s2")].account, id("c-a-best"));
}

#[test]
fn a_shorter_request_does_not_cut_a_longer_cache_short() {
    let mut r = sticky_pair(0.4);
    let start = DateTime::<Utc>::MAX_UTC - secs(4000);
    let mut record = |account, ttl, elapsed| {
        r.record_warmth(
            Provider::Claude,
            &caller(None),
            &id(account),
            secs(ttl),
            start + secs(elapsed),
        )
    };
    let first = record("c-b-sticky", 3600, 0);
    assert_eq!(first.len(), 1);
    let renewed = record("c-b-sticky", 300, 60);
    assert_eq!(renewed.len(), 1);
    assert_eq!(renewed[0].1.ttl, secs(3600));
    let longer = record("c-b-sticky", i64::from(u32::MAX), 60);
    assert_eq!(longer.len(), 1);
    assert!(longer[0].1.is_warm(DateTime::<Utc>::MAX_UTC));
    assert!(record("c-b-sticky", 300, 61).is_empty());
    let moved = record("c-a-best", 300, 62);
    assert_eq!(moved[0].1.account, id("c-a-best"));
    assert!(moved[0].1.is_warm(start + secs(362)));
    assert!(
        !moved[0]
            .1
            .is_warm(start + secs(362) + TimeDelta::microseconds(1))
    );
}

fn session_warmth(r: &Registry, session: &str) -> super::Warmth {
    r.warmth[&WarmthKey {
        provider: Provider::Claude,
        scope: Scope::Session,
        name: session.to_owned(),
    }]
        .clone()
}

#[test]
fn a_session_keeps_its_longest_lifetime_on_the_same_account() {
    let mut r = sticky_pair(0.4);
    serve(&mut r, Some("s1"), "c-b-sticky", 3600, t0());
    let rows = r.record_warmth(
        Provider::Claude,
        &caller(Some("s1")),
        &id("c-b-sticky"),
        secs(300),
        t0() + secs(1000),
    );
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(_, warmth)| warmth.ttl == secs(3600)));
    let held = session_warmth(&r, "s1");
    assert_eq!(held.ttl, secs(3600));
    assert_eq!(held.last_used, t0() + secs(1000));
    assert!(held.is_warm(t0() + secs(4600)));
    assert!(!held.is_warm(t0() + secs(4601)));
}

#[test]
fn a_longer_lifetime_replaces_a_shorter_one_on_the_same_account() {
    let mut r = sticky_pair(0.4);
    serve(&mut r, Some("s1"), "c-b-sticky", 300, t0());
    serve(&mut r, Some("s1"), "c-b-sticky", 3600, t0() + secs(10));
    assert_eq!(session_warmth(&r, "s1").ttl, secs(3600));
}

#[test]
fn a_cold_entry_takes_the_new_lifetime() {
    let mut r = sticky_pair(0.4);
    serve(&mut r, Some("s1"), "c-b-sticky", 3600, t0());
    serve(&mut r, Some("s1"), "c-b-sticky", 300, t0() + secs(3601));
    assert_eq!(session_warmth(&r, "s1").ttl, secs(300));
}

#[test]
fn a_different_account_takes_the_new_lifetime() {
    let mut r = sticky_pair(0.4);
    serve(&mut r, Some("s1"), "c-b-sticky", 3600, t0());
    serve(&mut r, Some("s1"), "c-a-best", 300, t0() + secs(10));
    let held = session_warmth(&r, "s1");
    assert_eq!(held.account, id("c-a-best"));
    assert_eq!(held.ttl, secs(300));
}

#[test]
fn the_user_scope_merges_lifetimes_like_a_session() {
    let mut r = sticky_pair(0.4);
    serve(&mut r, Some("s1"), "c-b-sticky", 3600, t0());
    serve(&mut r, Some("s2"), "c-b-sticky", 300, t0() + secs(10));
    let user = &r.warmth[&WarmthKey {
        provider: Provider::Claude,
        scope: Scope::User,
        name: USER.to_owned(),
    }];
    assert_eq!(user.ttl, secs(3600));
    assert_eq!(session_warmth(&r, "s2").ttl, secs(300));
}

#[test]
fn the_warmth_table_is_bounded() {
    let mut r = registry();
    for n in 0..16_400 {
        let name = format!("s{n}");
        r.record_warmth(
            Provider::Claude,
            &Caller {
                session: Some(&name),
                user: "u",
            },
            &id("c-a"),
            secs(300),
            t0(),
        );
    }
    assert_eq!(r.warmth.len(), 16_384);
    let renewed = r.record_warmth(
        Provider::Claude,
        &Caller {
            session: Some("s0"),
            user: "u",
        },
        &id("c-b"),
        secs(300),
        t0() + secs(1),
    );
    assert_eq!(renewed.len(), 2);
    assert_eq!(r.warmth.len(), 16_384);
}

#[test]
fn loaded_warmth_steers_candidates() {
    let mut source = sticky_pair(0.4);
    serve(&mut source, Some("s1"), "c-b-sticky", 300, t0());
    let mut r = sticky_pair(0.4);
    let mut rows = source.warmth;
    let (key, mut orphan) = rows
        .iter()
        .next()
        .map(|(key, row)| (key.clone(), row.clone()))
        .unwrap();
    orphan.account = id("missing");
    rows.insert(
        WarmthKey {
            name: "orphan".to_owned(),
            ..key
        },
        orphan,
    );
    r.load_warmth(rows);
    assert_eq!(r.warmth.len(), 2);
    assert_eq!(ids(&candidates(&r, Some("s1"))), ["c-b-sticky", "c-a-best"]);
}

#[test]
fn cooldowns_expire_and_never_shorten() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    r.cooldown_account(&id("c-a"), t0() + secs(60));
    r.cooldown_account(&id("c-a"), t0() + secs(10));
    assert!(r.usable(&id("c-a"), t0() + secs(30)).is_none());
    assert!(r.usable(&id("c-a"), t0() + secs(60)).is_some());

    r.cooldown_daemon("n1", t0() + secs(15));
    assert!(
        r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(14))
            .is_empty()
    );
    assert_eq!(
        r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(60))
            .len(),
        1
    );
}

#[test]
fn cooldown_survives_a_report_from_the_same_daemon() {
    let mut r = registry();
    let rep = report("n1", vec![account("c-a", Provider::Claude, 0.5)]);
    apply(&mut r, rep.clone(), t0());
    r.cooldown_account(&id("c-a"), t0() + secs(30));
    r.cooldown_daemon("n1", t0() + secs(30));
    apply(&mut r, rep, t0() + secs(10));
    assert!(
        r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(10))
            .is_empty()
    );
    assert_eq!(
        r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(31))
            .len(),
        1
    );
}

#[test]
fn cooldown_survives_a_report_but_not_a_new_tunnel() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    r.cooldown_daemon("n1", t0() + secs(300));
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0() + secs(2),
    );
    assert!(
        r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(2))
            .is_empty()
    );
    r.attach_tunnel("n1", None, t0() + secs(5)).unwrap();
    assert_eq!(
        r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(5))
            .len(),
        1
    );
}

#[test]
fn report_removes_accounts_the_node_dropped() {
    let mut r = registry();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                account("c-a", Provider::Claude, 0.5),
                account("c-b", Provider::Claude, 0.5),
            ],
        ),
        t0(),
    );
    apply(
        &mut r,
        report("n2", vec![account("c-z", Provider::Claude, 0.5)]),
        t0(),
    );
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.4)]),
        t0(),
    );
    assert_eq!(ids(&candidates(&r, None)), ["c-a", "c-z"]);
    let snap = r.snapshot(t0());
    assert_eq!(snap.accounts.len(), 2);
}

#[test]
fn headroom_hints() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    r.apply_headroom(&id("c-a"), HeadroomHint::Value(0.25));
    assert_eq!(r.usable(&id("c-a"), t0()).unwrap().headroom, 0.25);
    r.apply_headroom(&id("c-a"), HeadroomHint::Deny);
    assert!(r.usable(&id("c-a"), t0()).is_none());
    r.apply_headroom(&id("c-a"), HeadroomHint::Value(0.9));
    assert!(r.usable(&id("c-a"), t0()).is_none());
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    assert!(r.usable(&id("c-a"), t0()).is_some());
}

#[test]
fn loaded_daemons_are_stale_until_they_report() {
    let mut r = registry();
    r.load(
        [DaemonRow {
            node: "n1".to_owned(),
            owner: Some("alice@example.com".to_owned()),
            last_seen: t0(),
        }],
        [("n1".to_owned(), account("c-a", Provider::Claude, 0.5))],
    );
    assert!(candidates(&r, None).is_empty());
    let snap = r.snapshot(t0());
    assert!(!snap.daemons[0].online && !snap.accounts[0].online);
    assert!(r.node_taken("n1", Some("bob@example.com"), t0()));
    let mut claim = report("n2", vec![account("c-a", Provider::Claude, 0.5)]);
    r.filter_report(&mut claim, t0());
    assert!(claim.accounts.is_empty());
    assert_eq!(snap.accounts[0].owner.as_deref(), Some("alice@example.com"));
    assert!(r.attach_tunnel("n1", None, t0()).is_none());
    r.attach_tunnel("n1", Some("alice@example.com".to_owned()), t0())
        .unwrap();
    r.apply_report(
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        Some("alice@example.com".to_owned()),
        t0(),
    );
    assert_eq!(candidates(&r, None).len(), 1);
}

#[test]
fn snapshot_shows_only_active_cooldowns_and_reflects_them_in_admission() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    assert!(r.snapshot(t0()).accounts[0].admission.is_admit());
    r.cooldown_account(&id("c-a"), t0() + secs(30));
    r.cooldown_daemon("n1", t0() + secs(40));
    let during = &r.snapshot(t0()).accounts[0];
    assert_eq!(during.cooldown_until, Some(t0() + secs(40)));
    assert!(!during.admission.is_admit());
    assert_eq!(during.score, None);
    let after = &r.snapshot(t0() + secs(41)).accounts[0];
    assert_eq!(after.cooldown_until, None);
    assert!(after.admission.is_admit());
    assert!(after.score.is_some());
}

#[test]
fn snapshot_scores_only_candidates() {
    let mut r = registry();
    let mut paused = account("c-paused", Provider::Claude, 0.0);
    paused.paused = true;
    paused.admission = Admission::Deny {
        reason: DenyReason::Paused,
    };
    apply(
        &mut r,
        report(
            "n1",
            vec![paused, windowed("c-ok", 0.4, (0.3, 2), (0.1, 100))],
        ),
        t0(),
    );
    let snap = r.snapshot(t0());
    let by_id = |name: &str| snap.accounts.iter().find(|a| a.report.id.as_str() == name);
    assert_eq!(by_id("c-paused").unwrap().score, None);
    assert!(by_id("c-paused").unwrap().report.paused);
    let score = by_id("c-ok").unwrap().score.unwrap();
    assert!((score - 0.4 / 2.0).abs() < 1e-9, "{score}");
    assert_eq!(r.usable(&id("c-ok"), t0()).unwrap().score.value, score);
}

#[test]
fn prune_drops_cold_caches_and_expired_cooldowns() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    serve(&mut r, Some("s1"), "c-a", 300, t0());
    serve(&mut r, Some("s2"), "c-a", 3600, t0());
    r.cooldown_account(&id("c-a"), t0() + secs(30));
    r.cooldown_daemon("n1", t0() + secs(15));
    r.prune(t0() + secs(301));
    assert_eq!(r.warmth.len(), 2);
    r.prune(t0() + secs(3601));
    assert!(r.warmth.is_empty());
    assert_eq!(r.accounts[&id("c-a")].cooldown_until, None);
    assert_eq!(r.daemons["n1"].cooldown_until, None);
}

#[test]
fn a_stale_owner_does_not_block_a_claim() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    let now = t0() + secs(61);
    let mut next = report("n2", vec![account("c-a", Provider::Claude, 0.6)]);
    r.filter_report(&mut next, now);
    apply(&mut r, next, now);
    assert_eq!(r.usable(&id("c-a"), now).unwrap().node, "n2");
}

#[test]
fn a_subscription_belongs_to_its_daemons_owner() {
    let mut r = registry();
    r.attach_tunnel("n1", Some("alice@example.com".to_owned()), t0())
        .unwrap();
    r.apply_report(
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        Some("alice@example.com".to_owned()),
        t0(),
    );
    r.apply_report(
        report("n2", vec![account("c-b", Provider::Claude, 0.5)]),
        None,
        t0(),
    );
    let snap = r.snapshot(t0());
    let owner_of = |name: &str| {
        snap.accounts
            .iter()
            .find(|a| a.report.id.as_str() == name)
            .unwrap()
            .owner
            .clone()
    };
    assert_eq!(owner_of("c-a").as_deref(), Some("alice@example.com"));
    assert_eq!(owner_of("c-b"), None);
    assert_eq!(snap.daemons[0].owner.as_deref(), Some("alice@example.com"));
    let target = r.control_target(&id("c-a")).unwrap();
    assert_eq!(target.node, "n1");
    assert_eq!(target.generation, r.tunnel("n1").map(|t| t.generation));
    assert_eq!(r.control_target(&id("c-b")).unwrap().generation, None);
    assert_eq!(target.owner.as_deref(), Some("alice@example.com"));
    assert!(r.control_target(&id("nope")).is_none());
}

#[test]
fn a_live_node_cannot_be_taken_over_by_another_login() {
    let mut r = registry();
    r.apply_report(
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        Some("alice@example.com".to_owned()),
        t0(),
    );
    let now = t0() + secs(10);
    assert!(r.node_taken("n1", Some("bob@example.com"), now));
    assert!(!r.node_taken("n1", Some("Alice@Example.com"), now));
    assert!(r.node_taken("n1", None, now));
    assert!(!r.node_taken("other-node", Some("bob@example.com"), now));
    assert!(!r.node_taken("n1", Some("bob@example.com"), t0() + secs(61)));
    r.apply_report(report("n2", vec![]), None, t0());
    assert!(r.node_taken("n2", Some("bob@example.com"), now));
}

#[test]
fn control_results_replace_or_remove_the_account_at_once() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    r.cooldown_account(&id("c-a"), t0() + secs(30));
    let mut paused = account("c-a", Provider::Claude, 0.0);
    paused.paused = true;
    paused.admission = Admission::Deny {
        reason: DenyReason::Paused,
    };
    let target = r.control_target(&id("c-a")).unwrap();
    assert!(r.replace_account(&target, paused));
    let view = r.view(&id("c-a"), t0()).unwrap();
    assert!(view.report.paused);
    assert_eq!(view.cooldown_until, Some(t0() + secs(30)));

    r.apply_report(
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        Some("bob@example.com".to_owned()),
        t0() + secs(61),
    );
    assert!(!r.replace_account(&target, view.report));
    assert!(!r.remove_account(&target));
    assert!(!r.view(&target.id, t0() + secs(61)).unwrap().report.paused);
    let current = r.control_target(&target.id).unwrap();
    assert!(r.remove_account(&current));
    assert!(r.view(&target.id, t0() + secs(61)).is_none());
}

#[test]
fn a_daemon_is_a_candidate_only_with_a_tunnel_and_fresh_reports() {
    let mut r = registry();
    r.apply_report(
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        None,
        t0(),
    );
    assert!(candidates(&r, None).is_empty());
    let snap = r.snapshot(t0());
    assert!(!snap.daemons[0].online && !snap.accounts[0].online);
    assert_eq!(snap.daemons[0].connected_since, None);

    let generation = r.attach_tunnel("n1", None, t0()).unwrap();
    assert_eq!(candidates(&r, None).len(), 1);
    let snap = r.snapshot(t0());
    assert!(snap.daemons[0].online && snap.accounts[0].online);
    assert_eq!(snap.daemons[0].connected_since, Some(t0()));
    assert!(
        r.candidates(Provider::Claude, &Caller::default(), None, t0() + secs(61))
            .is_empty()
    );
    assert!(!r.snapshot(t0() + secs(61)).daemons[0].online);

    assert!(r.detach_tunnel("n1", generation));
    assert!(candidates(&r, None).is_empty());
    assert!(!r.snapshot(t0()).accounts[0].online);
}

#[test]
fn a_tunnel_of_a_node_replaces_the_older_one() {
    let mut r = registry();
    let first = r.attach_tunnel("n1", None, t0()).unwrap();
    let second = r.attach_tunnel("n1", None, t0() + secs(5)).unwrap();
    assert_ne!(first, second);
    assert_eq!(r.tunnel("n1").unwrap().generation, second);
    assert_eq!(r.tunnel("n1").unwrap().connected_since, t0() + secs(5));
}

#[test]
fn detaching_an_old_generation_keeps_the_newer_tunnel() {
    let mut r = registry();
    let first = r.attach_tunnel("n1", None, t0()).unwrap();
    let second = r.attach_tunnel("n1", None, t0()).unwrap();
    assert!(!r.detach_tunnel("n1", first));
    assert_eq!(r.tunnel("n1").unwrap().generation, second);
    assert!(r.detach_tunnel("n1", second));
    assert!(r.tunnel("n1").is_none());
    assert!(!r.detach_tunnel("n1", second));
}

#[test]
fn a_tunnel_is_refused_for_a_node_of_another_login() {
    let mut r = registry();
    r.apply_report(
        report("n1", vec![]),
        Some("alice@example.com".to_owned()),
        t0(),
    );
    let now = t0() + secs(10);
    assert!(
        r.attach_tunnel("n1", Some("bob@example.com".to_owned()), now)
            .is_none()
    );
    assert!(r.attach_tunnel("n1", None, now).is_none());
    assert!(r.tunnel("n1").is_none());
    assert!(
        r.attach_tunnel("n1", Some("ALICE@example.com".to_owned()), now)
            .is_some()
    );
    assert!(
        r.attach_tunnel("n1", Some("bob@example.com".to_owned()), now)
            .is_none(),
        "an open tunnel holds the name against another login"
    );
    assert!(
        r.attach_tunnel("n1", Some("bob@example.com".to_owned()), t0() + secs(100))
            .is_none(),
        "an open tunnel holds the name even when the reports are stale"
    );
    assert!(r.attach_tunnel("n1", None, t0() + secs(100)).is_none());
}

#[test]
fn a_stale_subscription_cannot_send_controls_over_the_replacement_owners_tunnel() {
    let mut r = registry();
    r.apply_report(
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        Some("alice@example.com".to_owned()),
        t0(),
    );
    r.attach_tunnel("n1", Some("bob@example.com".to_owned()), t0() + secs(61))
        .unwrap();
    let target = r.control_target(&id("c-a")).unwrap();
    assert_eq!(target.owner.as_deref(), Some("alice@example.com"));
    assert_eq!(target.generation, None);
}

#[test]
fn a_report_must_match_the_login_of_the_open_tunnel() {
    let mut r = registry();
    r.attach_tunnel("n1", Some("alice@example.com".to_owned()), t0())
        .unwrap();
    assert!(r.node_taken("n1", Some("bob@example.com"), t0()));
    assert!(!r.node_taken("n1", Some("Alice@example.com"), t0()));
    assert!(r.node_taken("n1", None, t0()));
}

fn allowlist(patterns: &[&str]) -> ModelAllowlist {
    ModelAllowlist::new(patterns.iter().copied()).unwrap()
}

fn restricted(name: &str, patterns: &[&str]) -> AccountReport {
    AccountReport {
        models: Some(allowlist(patterns)),
        ..account(name, Provider::Claude, 0.5)
    }
}

fn model_candidates(r: &Registry, model: Option<&str>) -> Vec<String> {
    r.candidates(Provider::Claude, &Caller::default(), model, t0())
        .into_iter()
        .map(|candidate| candidate.id.to_string())
        .collect()
}

fn listed(r: &Registry, name: &str) -> Vec<String> {
    r.view(&id(name), t0()).unwrap().available_models
}

#[test]
fn a_request_for_a_model_goes_only_to_accounts_that_allow_it() {
    let mut r = registry();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                account("c-open", Provider::Claude, 0.1),
                restricted("c-sonnet", &["claude-sonnet-*"]),
                restricted("c-opus", &["Claude-Opus-4"]),
            ],
        ),
        t0(),
    );
    assert_eq!(
        model_candidates(&r, Some("claude-sonnet-4-5")),
        ["c-open", "c-sonnet"]
    );
    assert_eq!(
        model_candidates(&r, Some("CLAUDE-OPUS-4")),
        ["c-open", "c-opus"]
    );
    assert_eq!(model_candidates(&r, Some("claude-haiku-4")), ["c-open"]);
    assert_eq!(model_candidates(&r, None), ["c-open", "c-opus", "c-sonnet"]);
}

#[test]
fn a_request_for_a_model_goes_only_to_accounts_whose_known_list_names_it() {
    let mut r = registry();
    apply(
        &mut r,
        report(
            "n1",
            vec![
                account("c-a", Provider::Claude, 0.5),
                account("c-b", Provider::Claude, 0.4),
                account("c-c", Provider::Claude, 0.3),
            ],
        ),
        t0(),
    );
    r.record_models(&id("c-a"), vec!["claude-opus-4".to_owned()]);
    r.record_models(&id("c-b"), vec!["Claude-Haiku-4".to_owned()]);
    assert_eq!(model_candidates(&r, Some("claude-haiku-4")), ["c-b", "c-c"]);
    assert_eq!(model_candidates(&r, Some("claude-opus-4")), ["c-a", "c-c"]);
    assert_eq!(
        model_candidates(&r, Some("claude-other")),
        ["c-a", "c-b", "c-c"],
        "a model no account listed may go to any account"
    );
    assert_eq!(model_candidates(&r, None), ["c-a", "c-b", "c-c"]);
}

#[test]
fn the_allowlist_and_the_known_list_must_both_allow_the_model() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![restricted("c-a", &["claude-sonnet-*"])]),
        t0(),
    );
    r.record_models(
        &id("c-a"),
        vec!["claude-sonnet-4".to_owned(), "claude-opus-4".to_owned()],
    );
    assert_eq!(model_candidates(&r, Some("claude-sonnet-4")), ["c-a"]);
    assert!(model_candidates(&r, Some("claude-opus-4")).is_empty());
    assert_eq!(
        model_candidates(&r, Some("claude-sonnet-4-5")),
        ["c-a"],
        "an allowed model that no account listed, such as an alias, is not refused"
    );
}

#[test]
fn a_model_filter_does_not_change_whether_an_account_has_room() {
    let mut r = registry();
    let mut paused = restricted("c-paused", &["claude-sonnet-4"]);
    paused.admission = Admission::Deny {
        reason: DenyReason::Paused,
    };
    apply(
        &mut r,
        report("n1", vec![paused, account("c-open", Provider::Claude, 0.5)]),
        t0(),
    );
    r.cooldown_account(&id("c-open"), t0() + secs(30));
    assert!(model_candidates(&r, Some("claude-sonnet-4")).is_empty());
    assert!(!r.model_unserved(Provider::Claude, "claude-sonnet-4"));
    assert!(model_candidates(&r, None).is_empty());
}

#[test]
fn a_model_is_unserved_only_when_no_account_of_the_provider_may_serve_it() {
    let mut r = registry();
    assert!(
        !r.model_unserved(Provider::Claude, "claude-sonnet-4"),
        "a provider without accounts is not about models"
    );
    apply(
        &mut r,
        report(
            "n1",
            vec![
                restricted("c-a", &["claude-sonnet-*"]),
                restricted("c-b", &["claude-opus-4"]),
                account("x-codex", Provider::Codex, 0.5),
            ],
        ),
        t0(),
    );
    r.record_models(&id("c-b"), vec!["claude-opus-4".to_owned()]);
    assert!(!r.model_unserved(Provider::Claude, "claude-sonnet-4"));
    assert!(!r.model_unserved(Provider::Claude, "claude-opus-4"));
    assert!(r.model_unserved(Provider::Claude, "claude-haiku-4"));
    assert!(!r.model_unserved(Provider::Codex, "claude-haiku-4"));
}

#[test]
fn listed_models_are_sorted_and_survive_reports_of_the_same_node() {
    let mut r = registry();
    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    assert!(listed(&r, "c-a").is_empty());
    r.record_models(
        &id("c-a"),
        ["m-b", "m-a", "m-b"].map(str::to_owned).to_vec(),
    );
    assert_eq!(listed(&r, "c-a"), ["m-a", "m-b"]);

    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.4)]),
        t0(),
    );
    assert_eq!(listed(&r, "c-a"), ["m-a", "m-b"]);
    r.record_models(&id("c-a"), Vec::new());
    assert_eq!(
        listed(&r, "c-a"),
        ["m-a", "m-b"],
        "an empty list says nothing"
    );
    r.record_models(&id("c-missing"), vec!["m-c".to_owned()]);

    apply(
        &mut r,
        report("n2", vec![account("c-a", Provider::Claude, 0.4)]),
        t0() + secs(61),
    );
    assert!(
        listed(&r, "c-a").is_empty(),
        "an account that moved to another daemon must be listed again"
    );
}

#[test]
fn the_models_revision_changes_when_accounts_or_allowlists_change() {
    let mut r = registry();
    let revision = |r: &Registry| r.models_revision();
    let unchanged = |r: &mut Registry, reports: Vec<AccountReport>| {
        let before = revision(r);
        apply(r, report("n1", reports), t0());
        assert_eq!(revision(r), before);
    };
    let changed = |r: &mut Registry, reports: Vec<AccountReport>| {
        let before = revision(r);
        apply(r, report("n1", reports), t0());
        assert!(revision(r) > before);
    };

    changed(&mut r, vec![account("c-a", Provider::Claude, 0.5)]);
    unchanged(&mut r, vec![account("c-a", Provider::Claude, 0.2)]);
    changed(&mut r, vec![restricted("c-a", &["m-a"])]);
    unchanged(&mut r, vec![restricted("c-a", &["m-a"])]);
    changed(&mut r, vec![restricted("c-a", &["m-a", "m-b"])]);
    changed(
        &mut r,
        vec![
            restricted("c-a", &["m-a", "m-b"]),
            account("c-b", Provider::Claude, 0.5),
        ],
    );
    changed(&mut r, vec![restricted("c-a", &["m-a", "m-b"])]);
    changed(&mut r, vec![]);

    apply(
        &mut r,
        report("n1", vec![account("c-a", Provider::Claude, 0.5)]),
        t0(),
    );
    let target = r.control_target(&id("c-a")).unwrap();
    let before = revision(&r);
    assert!(r.replace_account(&target, account("c-a", Provider::Claude, 0.1)));
    assert_eq!(revision(&r), before);
    assert!(r.replace_account(&target, restricted("c-a", &["m-a"])));
    assert!(revision(&r) > before);
    let before = revision(&r);
    r.record_models(&id("c-a"), vec!["m-a".to_owned()]);
    assert_eq!(
        revision(&r),
        before,
        "listing models is not an allowlist change"
    );
    assert!(r.remove_account(&target));
    assert!(revision(&r) > before);
}

#[test]
fn eligible_accounts_are_the_usable_ones_of_the_provider_by_id() {
    let mut r = registry();
    let mut denied = account("c-denied", Provider::Claude, 0.5);
    denied.admission = Admission::Deny {
        reason: DenyReason::Paused,
    };
    apply(
        &mut r,
        report(
            "n1",
            vec![
                account("c-b", Provider::Claude, 0.1),
                account("c-a", Provider::Claude, 0.9),
                account("c-rest", Provider::Claude, 0.9),
                denied,
                account("x-codex", Provider::Codex, 0.5),
            ],
        ),
        t0(),
    );
    r.cooldown_account(&id("c-rest"), t0() + secs(30));
    let eligible: Vec<_> = r
        .eligible(Provider::Claude, t0())
        .into_iter()
        .map(|candidate| candidate.id.to_string())
        .collect();
    assert_eq!(eligible, ["c-a", "c-b"]);
}

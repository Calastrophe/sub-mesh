use super::*;

#[tokio::test]
async fn the_overview_describes_the_mesh_for_the_caller() {
    let state = strict_state();
    connect_alice(&state, &Stub::new()).await;
    two_owners(&state).await;
    state.with_registry(|r| r.cooldown_account(&id("b"), Utc::now() + TimeDelta::seconds(30)));
    log_request(&state, Some(ALICE), Some("a"), TimeDelta::minutes(5)).await;
    log_request(&state, Some(ALICE), Some("a"), TimeDelta::minutes(50)).await;
    log_request(
        &state,
        Some("Carol@example.com"),
        Some("b"),
        TimeDelta::minutes(1),
    )
    .await;
    log_request(&state, Some(ALICE), Some("a"), TimeDelta::minutes(90)).await;
    log_request(&state, Some(ALICE), None, TimeDelta::minutes(1)).await;

    let api = Api::new(&state).await;
    let overview = api.overview(Some(ALICE)).await;

    assert_eq!(overview.me.login.as_deref(), Some(ALICE));
    assert_eq!(overview.me.node, "test-node");
    assert!((Utc::now() - overview.now).num_seconds().abs() < 5);

    let a = subscription(&overview, "a");
    assert_eq!(a.provider, Provider::Claude);
    assert_eq!(a.label.as_deref(), Some("a@mail.example"));
    assert_eq!(a.owner.as_deref(), Some(ALICE));
    assert_eq!(a.node, "alice-box");
    assert!(a.online && !a.paused && a.can_manage);
    assert_eq!(a.in_flight, 0);
    assert_eq!(a.requests_last_hour, 2);
    assert_eq!(a.cooldown_until, None);
    assert_eq!(a.admission, Admission::Admit { headroom: 0.7 });
    assert_eq!(a.reserve, ReserveConfig::default());
    let score = a.score.expect("a is a candidate");
    assert!((score - 0.7 / 168.0).abs() < 1e-9, "{score}");

    let b = subscription(&overview, "b");
    assert_eq!(b.owner.as_deref(), Some(BOB));
    assert!(!b.can_manage);
    assert_eq!(b.requests_last_hour, 1);
    assert!(b.cooldown_until.is_some());
    assert_eq!(b.score, None);
    assert!(!b.admission.is_admit());

    assert_eq!(overview.daemons.len(), 2);
    let daemon = |node: &str| overview.daemons.iter().find(|d| d.node == node).unwrap();
    assert_eq!(daemon("alice-box").owner.as_deref(), Some(ALICE));
    assert!(daemon("alice-box").online);
    assert!(daemon("alice-box").connected_since.is_some());
    assert!(!daemon("bob-box").online);
    assert_eq!(daemon("bob-box").connected_since, None);

    let users: Vec<_> = overview
        .users
        .iter()
        .map(|u| (u.login.as_str(), u.subscriptions, u.requests_last_hour))
        .collect();
    assert_eq!(
        users,
        [(ALICE, 1, 3), ("Carol@example.com", 0, 1), (BOB, 1, 0)]
    );
}

#[tokio::test]
async fn can_manage_compares_logins_ignoring_case() {
    let state = strict_state();
    two_owners(&state).await;
    let api = Api::new(&state).await;
    let overview = api.overview(Some("ALICE@Example.COM")).await;
    assert!(subscription(&overview, "a").can_manage);
    assert!(!subscription(&overview, "b").can_manage);
    assert_eq!(overview.users.len(), 2);
}

#[tokio::test]
async fn nobody_manages_without_a_known_login_or_owner() {
    let state = strict_state();
    apply(&state, report("ownerless", vec![claude_account("o")]), None).await;
    two_owners(&state).await;
    let api = Api::new(&state).await;
    let anonymous = api.overview(None).await;
    assert!(anonymous.subscriptions.iter().all(|s| !s.can_manage));
    assert_eq!(anonymous.me.login, None);
    let alice = api.overview(Some(ALICE)).await;
    assert!(!subscription(&alice, "o").can_manage);
    assert_eq!(subscription(&alice, "o").owner, None);
}

#[tokio::test]
async fn without_whois_a_caller_with_no_login_manages_everything() {
    let state = lax_state();
    apply(&state, report("ownerless", vec![claude_account("o")]), None).await;
    two_owners(&state).await;
    let api = Api::new(&state).await;
    let anonymous = api.overview(None).await;
    assert!(anonymous.subscriptions.iter().all(|s| s.can_manage));
    let alice = api.overview(Some(ALICE)).await;
    assert!(subscription(&alice, "a").can_manage);
    assert!(!subscription(&alice, "b").can_manage);
}

async fn fetch_logs(api: &Api, query: &str, login: Option<&str>) -> reqwest::Response {
    api.request(reqwest::Method::GET, &format!("{LOGS_PATH}{query}"), login)
        .send()
        .await
        .unwrap()
}

async fn logs(api: &Api, query: &str, login: Option<&str>) -> Vec<LogEntry> {
    let response = fetch_logs(api, query, login).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

fn log_ids(entries: &[LogEntry]) -> Vec<i64> {
    entries.iter().map(|entry| entry.id).collect()
}

#[tokio::test]
async fn the_log_lists_the_callers_requests_newest_first() {
    let state = strict_state();
    for n in 0..5 {
        log_request(&state, Some(ALICE), Some("a"), TimeDelta::minutes(10 - n)).await;
        log_request(&state, Some(BOB), Some("b"), TimeDelta::minutes(10 - n)).await;
    }
    let api = Api::new(&state).await;

    let all = logs(&api, "", Some(ALICE)).await;
    assert_eq!(all.len(), 5);
    assert!(all.windows(2).all(|pair| pair[0].id > pair[1].id));
    assert!(
        all.iter()
            .all(|entry| entry.login.as_deref() == Some(ALICE))
    );
    assert_eq!(all[0].consumer, "laptop");
    assert_eq!(all[0].provider, Provider::Claude);
    assert_eq!(all[0].transport, Transport::Http);
    assert_eq!(
        (all[0].method.as_str(), all[0].path.as_str()),
        ("POST", "v1/messages")
    );
    assert_eq!(all[0].account_id, Some(id("a")));
    assert_eq!(
        (all[0].status, all[0].latency_ms, all[0].attempts),
        (200, 10, 1)
    );
    assert_eq!(all[0].duration_ms, None);
    assert_eq!((all[0].responses, all[0].usage), (0, None));

    let bobs = logs(&api, "", Some(BOB)).await;
    assert_eq!(bobs.len(), 5);
    assert!(bobs.iter().all(|entry| entry.account_id == Some(id("b"))));
    assert!(logs(&api, "", Some("carol@example.com")).await.is_empty());

    assert_eq!(logs(&api, "?limit=2", Some(ALICE)).await.len(), 2);
    assert_eq!(logs(&api, "?limit=0", Some(ALICE)).await.len(), 1);
    assert_eq!(logs(&api, "?limit=5000", Some(ALICE)).await.len(), 5);

    let bad = fetch_logs(&api, "?limit=many", Some(ALICE)).await;
    assert_eq!(bad.status(), 400);
    let error: ApiError = bad.json().await.unwrap();
    assert!(!error.error.is_empty());
    assert_eq!(
        fetch_logs(&api, "?before=soon", Some(ALICE)).await.status(),
        400
    );
}

#[tokio::test]
async fn before_pages_back_through_the_callers_log() {
    let state = strict_state();
    for n in 0..6 {
        let owner = if n % 3 == 2 { BOB } else { ALICE };
        log_request(&state, Some(owner), Some("a"), TimeDelta::minutes(10 - n)).await;
    }
    let api = Api::new(&state).await;

    let first = logs(&api, "?limit=2", Some(ALICE)).await;
    assert_eq!(first.len(), 2);
    let cursor = first[1].id;
    let second = logs(&api, &format!("?limit=2&before={cursor}"), Some(ALICE)).await;
    assert_eq!(second.len(), 2);
    assert!(second[0].id < cursor, "before is exclusive");
    let cursor = second[1].id;
    let rest = logs(&api, &format!("?before={cursor}"), Some(ALICE)).await;
    assert!(rest.is_empty());

    let mut seen: Vec<i64> = [first, second]
        .concat()
        .iter()
        .map(|entry| entry.id)
        .collect();
    seen.dedup();
    assert_eq!(seen.len(), 4);
    assert_eq!(
        log_ids(&logs(&api, "", Some(ALICE)).await),
        seen,
        "the pages are the whole log"
    );
}

#[tokio::test]
async fn logins_match_the_log_ignoring_case() {
    let state = strict_state();
    log_request(
        &state,
        Some("Alice@Example.com"),
        Some("a"),
        TimeDelta::minutes(2),
    )
    .await;
    log_request(
        &state,
        Some("alice@example.com"),
        Some("a"),
        TimeDelta::minutes(1),
    )
    .await;
    log_request(
        &state,
        Some("alice@example.org"),
        Some("a"),
        TimeDelta::minutes(1),
    )
    .await;
    let api = Api::new(&state).await;
    let found = logs(&api, "", Some("ALICE@EXAMPLE.COM")).await;
    assert_eq!(found.len(), 2);
}

#[tokio::test]
async fn a_caller_without_a_login_sees_the_loginless_requests_of_its_node() {
    let state = lax_state();
    let mut elsewhere = logged_request(None, Some("a"), TimeDelta::minutes(3));
    elsewhere.consumer = "another-node".to_owned();
    store_request(&state, elsewhere).await;
    let mut here = logged_request(None, Some("a"), TimeDelta::minutes(2));
    here.consumer = "test-node".to_owned();
    store_request(&state, here).await;
    let mut named = logged_request(Some(ALICE), Some("a"), TimeDelta::minutes(1));
    named.consumer = "test-node".to_owned();
    store_request(&state, named).await;
    let api = Api::new(&state).await;

    let anonymous = logs(&api, "", None).await;
    assert_eq!(anonymous.len(), 1);
    assert_eq!(anonymous[0].consumer, "test-node");
    assert_eq!(anonymous[0].login, None);

    let alice = logs(&api, "", Some(ALICE)).await;
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].login.as_deref(), Some(ALICE));
}

fn sides(entries: &[LogEntry]) -> Vec<Direction> {
    entries.iter().map(|entry| entry.direction).collect()
}

#[tokio::test]
async fn the_log_holds_the_requests_others_made_with_the_callers_subscriptions() {
    let state = strict_state();
    let minutes = |n| TimeDelta::minutes(n);
    log_served(&state, Some(ALICE), Some(ALICE), minutes(9)).await;
    log_served(&state, Some(BOB), Some(ALICE), minutes(8)).await;
    log_served(&state, Some(ALICE), Some(BOB), minutes(7)).await;
    log_served(&state, Some(BOB), Some(BOB), minutes(6)).await;
    log_served(&state, Some(BOB), None, minutes(5)).await;
    let api = Api::new(&state).await;

    let alice = logs(&api, "", Some(ALICE)).await;
    assert_eq!(alice.len(), 3, "bob's own requests are not alice's");
    assert_eq!(
        sides(&alice),
        [Direction::Outbound, Direction::Inbound, Direction::Outbound]
    );
    assert_eq!(alice[1].login.as_deref(), Some(BOB));
    assert_eq!(alice[1].owner.as_deref(), Some(ALICE));
    assert_eq!(alice[0].owner.as_deref(), Some(BOB));
    assert_eq!(
        alice[2].owner.as_deref(),
        Some(ALICE),
        "a request made with the caller's own subscription is outbound only"
    );

    let bob = logs(&api, "", Some(BOB)).await;
    assert_eq!(
        sides(&bob),
        [
            Direction::Outbound,
            Direction::Outbound,
            Direction::Inbound,
            Direction::Outbound,
        ]
    );
    assert!(logs(&api, "", Some("carol@example.com")).await.is_empty());
}

#[tokio::test]
async fn direction_restricts_the_log_to_one_side() {
    let state = strict_state();
    for n in 0..4 {
        log_served(&state, Some(ALICE), Some(ALICE), TimeDelta::minutes(20 - n)).await;
        log_served(&state, Some(BOB), Some(ALICE), TimeDelta::minutes(10 - n)).await;
    }
    let api = Api::new(&state).await;

    let outbound = logs(&api, "?direction=outbound", Some(ALICE)).await;
    assert_eq!(outbound.len(), 4);
    assert!(outbound.iter().all(|e| e.direction == Direction::Outbound));
    assert!(outbound.iter().all(|e| e.login.as_deref() == Some(ALICE)));
    let inbound = logs(&api, "?direction=inbound", Some(ALICE)).await;
    assert_eq!(inbound.len(), 4);
    assert!(inbound.iter().all(|e| e.direction == Direction::Inbound));
    assert!(inbound.iter().all(|e| e.login.as_deref() == Some(BOB)));
    assert_eq!(logs(&api, "", Some(ALICE)).await.len(), 8);

    let bobs = logs(&api, "?direction=outbound", Some(BOB)).await;
    assert_eq!(bobs.len(), 4);
    assert!(logs(&api, "?direction=inbound", Some(BOB)).await.is_empty());

    for query in [
        format!("?direction={}", Direction::Inbound.as_str().to_uppercase()),
        "?direction=sideways".to_owned(),
        "?direction=".to_owned(),
    ] {
        let bad = fetch_logs(&api, &query, Some(ALICE)).await;
        assert_eq!(bad.status(), 400, "{query}");
        let error: ApiError = bad.json().await.unwrap();
        assert!(!error.error.is_empty());
    }
}

#[tokio::test]
async fn limit_and_before_page_through_the_union_and_each_side() {
    let state = strict_state();
    for n in 0..9 {
        let age = TimeDelta::minutes(20 - n);
        match n % 3 {
            0 => log_served(&state, Some(ALICE), None, age).await,
            1 => log_served(&state, Some(BOB), Some(ALICE), age).await,
            _ => log_served(&state, Some(BOB), Some(BOB), age).await,
        }
    }
    let api = Api::new(&state).await;
    let all = logs(&api, "", Some(ALICE)).await;
    assert_eq!(all.len(), 6);

    for direction in ["", "&direction=outbound", "&direction=inbound"] {
        let whole = logs(&api, &format!("?limit=1000{direction}"), Some(ALICE)).await;
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let before = cursor.map_or_else(String::new, |id| format!("&before={id}"));
            let page = logs(&api, &format!("?limit=2{direction}{before}"), Some(ALICE)).await;
            let Some(last) = page.last() else { break };
            assert!(page.len() <= 2);
            cursor = Some(last.id);
            seen.extend(page);
        }
        assert_eq!(seen, whole, "paged {direction:?}");
        assert!(whole.windows(2).all(|pair| pair[0].id > pair[1].id));
    }
}

#[tokio::test]
async fn owners_match_the_caller_ignoring_case() {
    let state = strict_state();
    log_served(
        &state,
        Some(BOB),
        Some("Alice@Example.com"),
        TimeDelta::minutes(3),
    )
    .await;
    log_served(
        &state,
        Some(BOB),
        Some("alice@example.org"),
        TimeDelta::minutes(2),
    )
    .await;
    log_served(
        &state,
        Some("ALICE@example.COM"),
        Some("alice@EXAMPLE.com"),
        TimeDelta::minutes(1),
    )
    .await;
    let api = Api::new(&state).await;

    let found = logs(&api, "", Some("ALICE@EXAMPLE.COM")).await;
    assert_eq!(
        sides(&found),
        [Direction::Outbound, Direction::Inbound],
        "the caller's own subscription never makes an inbound row"
    );
    assert_eq!(logs(&api, "", Some(BOB)).await.len(), 2);
}

#[tokio::test]
async fn requests_served_by_a_removed_subscription_stay_inbound_for_its_owner() {
    let state = strict_state();
    two_owners(&state).await;
    log_served(&state, Some(BOB), Some(ALICE), TimeDelta::minutes(2)).await;
    state.with_registry(|registry| {
        registry.remove_account(&registry.control_target(&id("a")).unwrap())
    });
    assert!(
        !state
            .with_registry(|r| r.snapshot(Utc::now()))
            .accounts
            .iter()
            .any(|account| account.report.id == id("a"))
    );
    let api = Api::new(&state).await;

    let inbound = logs(&api, "?direction=inbound", Some(ALICE)).await;
    assert_eq!(inbound.len(), 1);
    assert_eq!(inbound[0].owner.as_deref(), Some(ALICE));
    assert_eq!(inbound[0].account_id, Some(id("a")));
}

#[tokio::test]
async fn a_caller_without_a_login_has_no_inbound_requests() {
    let state = lax_state();
    let mut elsewhere = logged_request(Some(BOB), Some("a"), TimeDelta::minutes(3));
    elsewhere.owner = Some(ALICE.to_owned());
    store_request(&state, elsewhere).await;
    let mut here = logged_request(None, Some("a"), TimeDelta::minutes(2));
    here.consumer = "test-node".to_owned();
    here.owner = Some(ALICE.to_owned());
    store_request(&state, here).await;
    let api = Api::new(&state).await;

    let anonymous = logs(&api, "", None).await;
    assert_eq!(sides(&anonymous), [Direction::Outbound]);
    assert!(logs(&api, "?direction=inbound", None).await.is_empty());
    let alice = logs(&api, "", Some(ALICE)).await;
    assert_eq!(sides(&alice), [Direction::Inbound, Direction::Inbound]);
}

#[tokio::test]
async fn the_overview_shows_the_models_each_subscription_was_seen_to_list() {
    let state = strict_state();
    let restricted = AccountReport {
        models: Some(ModelAllowlist::new(["claude-sonnet-*"]).unwrap()),
        ..claude_account("a")
    };
    apply(&state, report("alice-box", vec![restricted]), Some(ALICE)).await;
    apply(
        &state,
        report("bob-box", vec![claude_account("b")]),
        Some(BOB),
    )
    .await;
    state.with_registry(|r| {
        r.record_models(
            &id("a"),
            ["claude-opus-4", "claude-haiku-4", "claude-sonnet-4"]
                .map(str::to_owned)
                .to_vec(),
        );
    });
    let api = Api::new(&state).await;
    let overview = api.overview(Some(ALICE)).await;
    let a = subscription(&overview, "a");
    assert_eq!(
        a.available_models,
        ["claude-haiku-4", "claude-opus-4", "claude-sonnet-4"],
        "the list is the provider's, before the allowlist"
    );
    assert_eq!(
        a.models,
        Some(ModelAllowlist::new(["claude-sonnet-*"]).unwrap())
    );
    let b = subscription(&overview, "b");
    assert!(b.available_models.is_empty());
    assert_eq!(b.models, None);
}

async fn fetch_usage(api: &Api, query: &str, login: Option<&str>) -> reqwest::Response {
    api.request(reqwest::Method::GET, &format!("{USAGE_PATH}{query}"), login)
        .send()
        .await
        .unwrap()
}

async fn usage(api: &Api, query: &str, login: Option<&str>) -> UsageReport {
    let response = fetch_usage(api, query, login).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

/// Log a request that `login` made with account `account`, which `owner`
/// owns, and end it with `input` input tokens.
async fn log_used(
    state: &AppState,
    login: Option<&str>,
    owner: &str,
    account: &str,
    input: u64,
    age: TimeDelta,
) {
    let mut record = logged_request(login, Some(account), age);
    let request_id = record.id;
    record.owner = Some(owner.to_owned());
    store_request(state, record).await;
    let done = Completion {
        duration_ms: 5,
        responses: 1,
        usage: Some(TokenUsage {
            input,
            ..TokenUsage::default()
        }),
        error: None,
    };
    state
        .db
        .run(move |db| db.complete_request(request_id, &done))
        .await
        .unwrap();
}

#[tokio::test]
async fn usage_rejects_hours_outside_its_range() {
    let state = strict_state();
    let api = Api::new(&state).await;
    for query in ["?hours=0", "?hours=337", "?hours=-1", "?hours=many"] {
        let response = fetch_usage(&api, query, Some(ALICE)).await;
        assert_eq!(response.status(), 400, "{query}");
        let error: ApiError = response.json().await.unwrap();
        assert!(!error.error.is_empty());
    }
    for query in ["?hours=1", "?hours=336"] {
        assert_eq!(fetch_usage(&api, query, Some(ALICE)).await.status(), 200);
    }
}

#[tokio::test]
async fn usage_covers_a_week_by_default() {
    let state = strict_state();
    log_used(&state, Some(ALICE), ALICE, "a", 1, TimeDelta::days(6)).await;
    log_used(&state, Some(ALICE), ALICE, "a", 2, TimeDelta::days(8)).await;
    let api = Api::new(&state).await;

    let before = Utc::now();
    let week = usage(&api, "", Some(ALICE)).await;
    let after = Utc::now();
    assert!(week.since >= before - TimeDelta::days(7));
    assert!(week.since <= after - TimeDelta::days(7));
    assert_eq!(week.rows.len(), 1);
    assert_eq!(week.rows[0].tokens.input, 1);

    let day = usage(&api, "?hours=24", Some(ALICE)).await;
    assert!(day.rows.is_empty());
    let fortnight = usage(&api, "?hours=336", Some(ALICE)).await;
    assert_eq!(fortnight.rows[0].tokens.input, 3);
    assert_eq!(fortnight.rows[0].requests, 2);
}

#[tokio::test]
async fn usage_is_empty_for_a_caller_without_a_login() {
    let state = lax_state();
    log_used(&state, None, ALICE, "a", 1, TimeDelta::hours(1)).await;
    log_used(&state, Some(ALICE), ALICE, "a", 1, TimeDelta::hours(1)).await;
    let api = Api::new(&state).await;
    assert!(usage(&api, "", None).await.rows.is_empty());
    assert_eq!(usage(&api, "", Some(ALICE)).await.rows.len(), 2);
}

#[tokio::test]
async fn usage_shows_only_the_callers_subscriptions_largest_spend_first() {
    let state = strict_state();
    log_used(&state, Some(ALICE), ALICE, "a", 10, TimeDelta::hours(1)).await;
    log_used(&state, Some(BOB), ALICE, "a", 500, TimeDelta::hours(1)).await;
    log_used(&state, Some(ALICE), ALICE, "c", 100, TimeDelta::hours(2)).await;
    log_used(&state, Some(ALICE), BOB, "b", 9999, TimeDelta::hours(1)).await;
    let api = Api::new(&state).await;

    let alice = usage(&api, "", Some("ALICE@example.com")).await;
    let spends: Vec<f64> = alice.rows.iter().map(|row| row.spend).collect();
    assert_eq!(spends, [500.0, 100.0, 10.0]);
    let logins: Vec<_> = alice.rows.iter().map(|row| row.login.as_deref()).collect();
    assert_eq!(logins, [Some(BOB), Some(ALICE), Some(ALICE)]);
    assert!(alice.rows.iter().all(|row| row.account_id.as_str() != "b"));

    let bob = usage(&api, "", Some(BOB)).await;
    assert_eq!(bob.rows.len(), 1);
    assert_eq!(bob.rows[0].spend, 9999.0);
    assert!(
        usage(&api, "", Some("carol@example.com"))
            .await
            .rows
            .is_empty()
    );
}

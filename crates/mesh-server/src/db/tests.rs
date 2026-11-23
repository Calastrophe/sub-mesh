use std::collections::HashMap;

use chrono::TimeDelta;
use mesh_core::api::{Direction, TokenUsage, Transport};
use mesh_core::{AccountId, Provider};

use super::log::LoginCount;
use crate::registry::{Scope, Warmth, WarmthKey, WarmthRow};

use super::*;

fn t0() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

fn account(id: &str) -> AccountReport {
    AccountReport {
        provider: Provider::Claude,
        label: Some("me@example.com".to_owned()),
        updated_at: t0(),
        ..crate::testing::account(id, 0.5)
    }
}

fn report(accounts: Vec<AccountReport>) -> DaemonReport {
    DaemonReport {
        sent_at: t0(),
        ..crate::testing::report("n1", accounts)
    }
}

fn count(db: &Db, table: &str) -> i64 {
    db.conn()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn report_round_trip() {
    let db = Db::open_in_memory().unwrap();
    let original = report(vec![account("claude-a"), account("claude-b")]);
    db.save_report(&original, Some("alice@example.com"), t0())
        .unwrap();

    let loaded = db.load().unwrap();
    assert_eq!(
        loaded.daemons,
        [DaemonRow {
            node: "n1".to_owned(),
            owner: Some("alice@example.com".to_owned()),
            last_seen: t0(),
        }]
    );
    let mut accounts = loaded.accounts;
    accounts.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    assert_eq!(
        accounts,
        [
            ("n1".to_owned(), original.accounts[0].clone()),
            ("n1".to_owned(), original.accounts[1].clone()),
        ]
    );
}

#[test]
fn later_report_replaces_the_nodes_accounts_and_owner() {
    let db = Db::open_in_memory().unwrap();
    db.save_report(
        &report(vec![account("claude-a"), account("claude-b")]),
        Some("alice@example.com"),
        t0(),
    )
    .unwrap();
    let second = report(vec![account("claude-b")]);
    db.save_report(&second, None, t0() + TimeDelta::seconds(5))
        .unwrap();

    let loaded = db.load().unwrap();
    assert_eq!(loaded.daemons.len(), 1);
    assert_eq!(loaded.daemons[0].last_seen, t0() + TimeDelta::seconds(5));
    assert_eq!(loaded.daemons[0].owner, None);
    assert_eq!(loaded.accounts.len(), 1);
    assert_eq!(loaded.accounts[0].1.id.as_str(), "claude-b");
}

fn request_record(id: i64, ts: DateTime<Utc>) -> RequestRecord {
    RequestRecord {
        id,
        ts,
        consumer: "laptop".to_owned(),
        login: None,
        provider: "claude".to_owned(),
        transport: Transport::Http,
        method: "POST".to_owned(),
        path: "v1/messages".to_owned(),
        model: None,
        client: None,
        session: None,
        account_id: Some("claude-a".to_owned()),
        owner: None,
        node: None,
        status: 200,
        latency_ms: 12,
        attempts: 1,
    }
}

fn login(login: &str) -> LogOwner {
    LogOwner::Login(login.to_owned())
}

fn ids(entries: &[mesh_core::api::LogEntry]) -> Vec<i64> {
    entries.iter().map(|entry| entry.id).collect()
}

#[test]
fn prunes_old_requests_only() {
    let db = Db::open_in_memory().unwrap();
    db.record_request(&request_record(1, t0() - TimeDelta::days(31)), &[])
        .unwrap();
    db.record_request(&request_record(2, t0() - TimeDelta::days(1)), &[])
        .unwrap();
    assert_eq!(count(&db, "requests"), 2);
    let deleted = db.prune_requests(t0() - TimeDelta::days(30)).unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(count(&db, "requests"), 1);
}

#[test]
fn pruning_keeps_a_row_at_the_cutoff_and_newer() {
    let db = Db::open_in_memory().unwrap();
    let cutoff = t0() - TimeDelta::days(14);
    db.record_request(&request_record(1, cutoff - TimeDelta::seconds(1)), &[])
        .unwrap();
    db.record_request(&request_record(2, cutoff), &[]).unwrap();
    db.record_request(&request_record(3, t0()), &[]).unwrap();
    assert_eq!(db.prune_requests(cutoff).unwrap(), 1);
    assert_eq!(ids(&db.recent_requests_of_anyone(10).unwrap()), [3, 2]);
}

#[test]
fn ids_keep_growing_after_pruning_every_row_and_restarting() {
    let dir = crate::testing::TempDir::new();
    let path = dir.path().join("mesh.db");
    {
        let db = Db::open(&path).unwrap();
        db.record_request(&request_record(1, t0() - TimeDelta::days(20)), &[])
            .unwrap();
        db.record_request(&request_record(2, t0() - TimeDelta::days(15)), &[])
            .unwrap();
        assert_eq!(db.prune_requests(t0() - TimeDelta::days(14)).unwrap(), 2);
        assert_eq!(count(&db, "requests"), 0);
        assert_eq!(db.last_request_id().unwrap(), 2);
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(count(&db, "requests"), 0);
    assert_eq!(db.last_request_id().unwrap(), 2);
}

#[test]
fn counts_requests_per_login_since_a_time() {
    let db = Db::open_in_memory().unwrap();
    let mut next = 0;
    let mut log = |login: Option<&str>, ts| {
        next += 1;
        let mut record = request_record(next, ts);
        record.login = login.map(str::to_owned);
        db.record_request(&record, &[]).unwrap();
    };
    log(Some("claude-a"), t0());
    log(Some("claude-a"), t0() - TimeDelta::minutes(59));
    log(Some("claude-a"), t0() - TimeDelta::minutes(61));
    log(Some("claude-b"), t0());
    log(None, t0());
    let activity = db.activity(t0() - TimeDelta::hours(1)).unwrap();
    assert_eq!(
        activity.by_login,
        [
            LoginCount {
                login: "claude-a".to_owned(),
                requests: 2
            },
            LoginCount {
                login: "claude-b".to_owned(),
                requests: 1
            },
        ]
    );
}

#[test]
fn counts_requests_per_account_since_a_time() {
    let db = Db::open_in_memory().unwrap();
    let mut next = 0;
    let mut log = |account: Option<&str>, ts| {
        next += 1;
        let mut record = request_record(next, ts);
        record.account_id = account.map(str::to_owned);
        db.record_request(&record, &[]).unwrap();
    };
    log(Some("claude-a"), t0());
    log(Some("claude-a"), t0() - TimeDelta::minutes(59));
    log(Some("claude-a"), t0() - TimeDelta::minutes(61));
    log(Some("claude-b"), t0());
    log(None, t0());
    let activity = db.activity(t0() - TimeDelta::hours(1)).unwrap();
    assert_eq!(
        activity.by_account,
        HashMap::from([("claude-a".to_owned(), 2), ("claude-b".to_owned(), 1)])
    );
}

#[test]
fn lists_a_logins_requests_newest_first() {
    let db = Db::open_in_memory().unwrap();
    for n in 1..=5 {
        let mut record = request_record(n, t0() + TimeDelta::seconds(n));
        record.login = Some("user@example.com".to_owned());
        record.status = 200 + u16::try_from(n).unwrap();
        record.transport = Transport::Websocket;
        record.method = "GET".to_owned();
        record.path = "responses".to_owned();
        record.model = Some("gpt-5".to_owned());
        record.client = Some("codex-cli/1".to_owned());
        record.session = Some("s1".to_owned());
        record.owner = Some("alice@example.com".to_owned());
        record.node = Some("alice-box".to_owned());
        db.record_request(&record, &[]).unwrap();
    }
    let recent = db
        .recent_requests(&login("user@example.com"), None, 3, None)
        .unwrap();
    assert_eq!(ids(&recent), [5, 4, 3]);
    let newest = &recent[0];
    assert_eq!(newest.login.as_deref(), Some("user@example.com"));
    assert_eq!(newest.status, 205);
    assert_eq!(newest.ts, t0() + TimeDelta::seconds(5));
    assert_eq!(newest.provider, Provider::Claude);
    assert_eq!(newest.consumer, "laptop");
    assert_eq!(newest.transport, Transport::Websocket);
    assert_eq!(newest.method, "GET");
    assert_eq!(newest.path, "responses");
    assert_eq!(newest.model.as_deref(), Some("gpt-5"));
    assert_eq!(newest.client.as_deref(), Some("codex-cli/1"));
    assert_eq!(newest.session.as_deref(), Some("s1"));
    assert_eq!(newest.node.as_deref(), Some("alice-box"));
    assert_eq!(newest.owner.as_deref(), Some("alice@example.com"));
    assert_eq!(newest.direction, Direction::Outbound);
    assert_eq!(
        newest.account_id.as_ref().map(AccountId::as_str),
        Some("claude-a")
    );
    assert_eq!((newest.latency_ms, newest.attempts), (12, 1));
    assert_eq!(newest.duration_ms, None);
    assert_eq!(
        (newest.responses, newest.usage, newest.error.clone()),
        (0, None, None)
    );
    let all = db
        .recent_requests(&login("user@example.com"), None, 100, None)
        .unwrap();
    assert_eq!(all.len(), 5);
}

#[test]
fn a_login_sees_only_its_own_requests_ignoring_case() {
    let db = Db::open_in_memory().unwrap();
    let mut next = 0;
    let mut log = |login: Option<&str>, consumer: &str| {
        next += 1;
        let mut record = request_record(next, t0());
        record.login = login.map(str::to_owned);
        record.consumer = consumer.to_owned();
        db.record_request(&record, &[]).unwrap();
    };
    log(Some("Alice@Example.com"), "laptop");
    log(Some("bob@example.com"), "laptop");
    log(Some("alice@example.com"), "phone");
    log(None, "laptop");
    log(Some("alice@example.com.au"), "laptop");
    log(None, "dev-box");

    let alice = db
        .recent_requests(&login("ALICE@example.COM"), None, 10, None)
        .unwrap();
    assert_eq!(ids(&alice), [3, 1]);
    let bob = db
        .recent_requests(&login("bob@example.com"), None, 10, None)
        .unwrap();
    assert_eq!(ids(&bob), [2]);
    let nobody = db
        .recent_requests(&login("carol@example.com"), None, 10, None)
        .unwrap();
    assert!(nobody.is_empty());

    let laptop = db
        .recent_requests(&LogOwner::Node("laptop".to_owned()), None, 10, None)
        .unwrap();
    assert_eq!(
        ids(&laptop),
        [4],
        "a node sees only requests without a login"
    );
    let other = db
        .recent_requests(&LogOwner::Node("dev-box".to_owned()), None, 10, None)
        .unwrap();
    assert_eq!(ids(&other), [6]);
    assert_eq!(db.recent_requests_of_anyone(10).unwrap().len(), 6);
}

/// Log a request that `login` made with a subscription of `owner`.
fn served(db: &Db, id: i64, login: Option<&str>, owner: Option<&str>) {
    let mut record = request_record(id, t0() + TimeDelta::seconds(id));
    record.login = login.map(str::to_owned);
    record.owner = owner.map(str::to_owned);
    db.record_request(&record, &[]).unwrap();
}

fn directions(entries: &[mesh_core::api::LogEntry]) -> Vec<(i64, Direction)> {
    entries
        .iter()
        .map(|entry| (entry.id, entry.direction))
        .collect()
}

#[test]
fn a_log_holds_what_the_login_made_and_what_others_made_with_its_subscriptions() {
    const ALICE: &str = "alice@example.com";
    let db = Db::open_in_memory().unwrap();
    served(&db, 1, Some(ALICE), Some(ALICE));
    served(&db, 2, Some("bob@example.com"), Some(ALICE));
    served(&db, 3, Some(ALICE), Some("bob@example.com"));
    served(&db, 4, None, Some("ALICE@Example.com"));
    served(&db, 5, Some("Bob@example.com"), None);
    served(&db, 6, Some("ALICE@EXAMPLE.COM"), Some("Alice@Example.com"));
    served(
        &db,
        7,
        Some("carol@example.com"),
        Some("alice@example.com.au"),
    );
    served(&db, 8, Some("carol@example.com"), None);

    let both = db.recent_requests(&login(ALICE), None, 10, None).unwrap();
    assert_eq!(
        directions(&both),
        [
            (6, Direction::Outbound),
            (4, Direction::Inbound),
            (3, Direction::Outbound),
            (2, Direction::Inbound),
            (1, Direction::Outbound),
        ],
        "a request made with the caller's own subscription is outbound only"
    );
    let outbound = db
        .recent_requests(&login(ALICE), Some(Direction::Outbound), 10, None)
        .unwrap();
    assert_eq!(ids(&outbound), [6, 3, 1]);
    let inbound = db
        .recent_requests(&login(ALICE), Some(Direction::Inbound), 10, None)
        .unwrap();
    assert_eq!(
        directions(&inbound),
        [(4, Direction::Inbound), (2, Direction::Inbound)]
    );
    assert_eq!(inbound[0].owner.as_deref(), Some("ALICE@Example.com"));
    assert_eq!(inbound[1].login.as_deref(), Some("bob@example.com"));

    let bob = login("bob@example.com");
    let both = db.recent_requests(&bob, None, 10, None).unwrap();
    assert_eq!(
        directions(&both),
        [
            (5, Direction::Outbound),
            (3, Direction::Inbound),
            (2, Direction::Outbound),
        ]
    );
    let nobody = login("dave@example.com");
    assert!(
        db.recent_requests(&nobody, None, 10, None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_node_without_a_login_has_no_inbound_requests() {
    let db = Db::open_in_memory().unwrap();
    served(&db, 1, None, Some("alice@example.com"));
    served(&db, 2, None, None);
    let laptop = LogOwner::Node("laptop".to_owned());
    let both = db.recent_requests(&laptop, None, 10, None).unwrap();
    assert_eq!(
        directions(&both),
        [(2, Direction::Outbound), (1, Direction::Outbound)]
    );
    let inbound = db
        .recent_requests(&laptop, Some(Direction::Inbound), 10, None)
        .unwrap();
    assert!(inbound.is_empty());
}

#[test]
fn limit_and_before_page_through_both_sides_and_each_side() {
    const ALICE: &str = "alice@example.com";
    let db = Db::open_in_memory().unwrap();
    for id in 1..=9 {
        match id % 3 {
            0 => served(&db, id, Some("bob@example.com"), Some(ALICE)),
            1 => served(&db, id, Some(ALICE), None),
            _ => served(&db, id, Some("bob@example.com"), None),
        }
    }
    let owner = login(ALICE);
    let mut paged = Vec::new();
    let mut before = None;
    loop {
        let page = db.recent_requests(&owner, None, 2, before).unwrap();
        let Some(last) = page.last() else { break };
        before = Some(last.id);
        paged.extend(directions(&page));
    }
    let whole = db.recent_requests(&owner, None, 100, None).unwrap();
    assert_eq!(paged, directions(&whole));
    assert_eq!(ids(&whole), [9, 7, 6, 4, 3, 1]);

    for (direction, expected) in [
        (Direction::Outbound, vec![7, 4, 1]),
        (Direction::Inbound, vec![9, 6, 3]),
    ] {
        let first = db
            .recent_requests(&owner, Some(direction), 2, None)
            .unwrap();
        assert_eq!(ids(&first), expected[..2]);
        let rest = db
            .recent_requests(&owner, Some(direction), 2, Some(expected[1]))
            .unwrap();
        assert_eq!(ids(&rest), expected[2..]);
        assert!(first.iter().all(|entry| entry.direction == direction));
    }
}

#[test]
fn rows_keep_their_owner_and_unfinished_ones_are_closed_at_startup() {
    let db = Db::open_in_memory().unwrap();
    for (id, age, finished, error) in [
        (1, 100, true, None),
        (2, 90, false, None),
        (3, 80, false, Some("refused")),
        (4, 5, false, None),
    ] {
        let mut record = request_record(id, t0() - TimeDelta::seconds(age));
        record.owner = Some("alice@example.com".to_owned());
        db.record_request(&record, &[]).unwrap();
        if finished {
            let done = Completion {
                duration_ms: 7,
                ..Completion::default()
            };
            db.complete_request(id, &done).unwrap();
        }
        if let Some(error) = error {
            db.conn()
                .execute(
                    "UPDATE requests SET error = ?2 WHERE id = ?1",
                    params![id, error],
                )
                .unwrap();
        }
    }
    assert_eq!(db.close_unfinished_requests(t0()).unwrap(), 3);

    let rows = db.recent_requests_of_anyone(10).unwrap();
    let row = |id| rows.iter().find(|row| row.id == id).unwrap();
    assert_eq!(row(1).duration_ms, Some(7));
    assert_eq!(row(1).error, None);
    assert_eq!(row(2).duration_ms, Some(90_000));
    assert_eq!(row(2).error.as_deref(), Some("cut off by a server restart"));
    assert_eq!(row(3).duration_ms, Some(80_000));
    assert_eq!(row(3).error.as_deref(), Some("refused"));
    assert_eq!(row(4).duration_ms, Some(5_000));
    assert!(
        rows.iter()
            .all(|row| row.owner.as_deref() == Some("alice@example.com"))
    );
    assert_eq!(db.close_unfinished_requests(t0()).unwrap(), 0);
}

#[test]
fn a_cut_off_row_never_gets_a_negative_duration() {
    let db = Db::open_in_memory().unwrap();
    db.record_request(&request_record(1, t0() + TimeDelta::hours(1)), &[])
        .unwrap();
    assert_eq!(db.close_unfinished_requests(t0()).unwrap(), 1);
    assert_eq!(
        db.recent_requests_of_anyone(1).unwrap()[0].duration_ms,
        Some(0)
    );
}

#[test]
fn before_is_exclusive_and_pages_back() {
    let db = Db::open_in_memory().unwrap();
    for n in 1..=7 {
        let mut record = request_record(n * 10, t0());
        record.login = Some("user@example.com".to_owned());
        db.record_request(&record, &[]).unwrap();
    }
    let owner = login("user@example.com");
    let first = db.recent_requests(&owner, None, 3, None).unwrap();
    assert_eq!(ids(&first), [70, 60, 50]);
    let second = db.recent_requests(&owner, None, 3, Some(50)).unwrap();
    assert_eq!(ids(&second), [40, 30, 20]);
    let third = db.recent_requests(&owner, None, 3, Some(20)).unwrap();
    assert_eq!(ids(&third), [10]);
    assert!(
        db.recent_requests(&owner, None, 3, Some(10))
            .unwrap()
            .is_empty()
    );
    assert!(
        db.recent_requests(&owner, None, 3, Some(-5))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unreadable_request_rows_are_skipped() {
    let db = Db::open_in_memory().unwrap();
    db.record_request(&request_record(1, t0()), &[]).unwrap();
    let mut bad = request_record(2, t0());
    bad.provider = "gemini".to_owned();
    db.record_request(&bad, &[]).unwrap();
    let recent = db.recent_requests_of_anyone(10).unwrap();
    assert_eq!(ids(&recent), [1]);
}

#[test]
fn completing_a_request_stores_its_end() {
    let db = Db::open_in_memory().unwrap();
    db.record_request(&request_record(1, t0()), &[]).unwrap();
    db.record_request(&request_record(2, t0()), &[]).unwrap();
    let usage = TokenUsage {
        input: 100,
        cache_read: 40,
        cache_write: 5,
        output: 20,
        reasoning: 3,
    };
    db.complete_request(
        1,
        &Completion {
            duration_ms: 3456,
            responses: 2,
            usage: Some(usage),
            error: Some("late".to_owned()),
        },
    )
    .unwrap();
    db.complete_request(9, &Completion::default()).unwrap();

    let entries = db.recent_requests_of_anyone(10).unwrap();
    let (second, first) = (&entries[0], &entries[1]);
    assert_eq!(first.duration_ms, Some(3456));
    assert_eq!(first.responses, 2);
    assert_eq!(first.usage, Some(usage));
    assert_eq!(first.error.as_deref(), Some("late"));
    assert_eq!(second.duration_ms, None);
    assert_eq!((second.responses, second.usage), (0, None));
}

#[test]
fn a_late_error_does_not_replace_an_earlier_one() {
    let db = Db::open_in_memory().unwrap();
    db.record_request(&request_record(1, t0()), &[]).unwrap();
    let complete = |error: Option<&str>| {
        let completion = Completion {
            error: error.map(str::to_owned),
            ..Completion::default()
        };
        db.complete_request(1, &completion).unwrap();
    };
    complete(None);
    assert_eq!(db.recent_requests_of_anyone(1).unwrap()[0].error, None);
    complete(Some("first"));
    complete(Some("second"));
    complete(None);
    assert_eq!(
        db.recent_requests_of_anyone(1).unwrap()[0].error.as_deref(),
        Some("first")
    );
}

#[test]
fn zero_usage_is_not_missing_usage() {
    let db = Db::open_in_memory().unwrap();
    db.record_request(&request_record(1, t0()), &[]).unwrap();
    let completion = Completion {
        duration_ms: 5,
        usage: Some(TokenUsage::default()),
        responses: 1,
        error: None,
    };
    db.complete_request(1, &completion).unwrap();
    assert_eq!(
        db.recent_requests_of_anyone(1).unwrap()[0].usage,
        Some(TokenUsage::default())
    );
    db.complete_request(1, &Completion::default()).unwrap();
    assert_eq!(db.recent_requests_of_anyone(1).unwrap()[0].usage, None);
}

#[test]
fn ids_continue_after_the_highest_stored_id() {
    let db = Db::open_in_memory().unwrap();
    assert_eq!(db.last_request_id().unwrap(), 0);
    db.record_request(&request_record(7, t0()), &[]).unwrap();
    db.record_request(&request_record(3, t0()), &[]).unwrap();
    assert_eq!(db.last_request_id().unwrap(), 7);
}

const OLD_REQUESTS: &str = "
    CREATE TABLE requests (
        id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, consumer TEXT NOT NULL,
        login TEXT, provider TEXT NOT NULL, account_id TEXT, node TEXT,
        status INTEGER NOT NULL, latency_ms INTEGER NOT NULL, attempts INTEGER NOT NULL
    );
    INSERT INTO requests (ts, consumer, provider, status, latency_ms, attempts)
        VALUES ('2026-01-01T00:00:00Z', 'laptop', 'claude', 200, 1, 1);
    CREATE TABLE daemons (node TEXT PRIMARY KEY, owner TEXT, last_seen TEXT NOT NULL);
    INSERT INTO daemons VALUES ('n1', NULL, '2026-01-01T00:00:00Z');
";

fn user_version(conn: &rusqlite::Connection) -> i64 {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[test]
fn an_old_request_log_is_replaced_and_other_tables_are_kept() {
    for version in [0, 1, 2] {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(OLD_REQUESTS).unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();

        schema::initialize(&mut conn).unwrap();
        assert_eq!(user_version(&conn), 3, "from version {version}");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        conn.execute_batch(
            "SELECT transport, owner, error, duration_ms, input_tokens FROM requests",
        )
        .unwrap();
        let daemons: i64 = conn
            .query_row("SELECT COUNT(*) FROM daemons", [], |row| row.get(0))
            .unwrap();
        assert_eq!(daemons, 1);
    }
}

#[test]
fn a_current_request_log_survives_a_restart() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::initialize(&mut conn).unwrap();
    let db = Db {
        conn: Arc::new(Mutex::new(conn)),
    };
    db.record_request(&request_record(1, t0()), &[]).unwrap();
    schema::initialize(&mut db.conn()).unwrap();
    assert_eq!(count(&db, "requests"), 1);
    assert_eq!(user_version(&db.conn()), 3);
}

fn warmth_row(scope: Scope, name: &str, account: &str, ttl: i64) -> WarmthRow {
    (
        WarmthKey {
            provider: Provider::Claude,
            scope,
            name: name.to_owned(),
        },
        Warmth {
            account: account.parse().unwrap(),
            last_used: t0() + TimeDelta::microseconds(500_001),
            ttl: TimeDelta::seconds(ttl),
        },
    )
}

#[test]
fn warmth_round_trips_and_prunes_at_the_same_boundary() {
    let db = Db::open_in_memory().unwrap();
    db.save_report(
        &report(vec![account("claude-a"), account("claude-b")]),
        None,
        t0(),
    )
    .unwrap();
    let mut next = 0;
    let mut store = |rows: &[WarmthRow]| {
        next += 1;
        db.record_request(&request_record(next, t0()), rows)
            .unwrap();
    };
    let session = |id, ttl| warmth_row(Scope::Session, "s1", id, ttl);
    let user = warmth_row(Scope::User, "s1", "claude-a", 3600);
    store(&[session("claude-a", 3600), user.clone()]);
    let moved = session("claude-b", 300);
    store(&[
        moved.clone(),
        warmth_row(Scope::Session, "orphan", "missing", 3600),
    ]);

    assert_eq!(count(&db, "requests"), 2);
    let expiry = moved.1.last_used + moved.1.ttl;
    assert_eq!(db.load_warmth(expiry).unwrap(), [moved, user.clone()]);
    assert_eq!(db.prune_warmth(expiry).unwrap(), 1);
    let cold = expiry + TimeDelta::microseconds(1);
    assert_eq!(db.load_warmth(cold).unwrap(), [user]);
    assert_eq!(db.prune_warmth(cold).unwrap(), 1);
    assert_eq!(count(&db, "cache_warmth"), 1);
}

#[tokio::test]
async fn run_executes_off_thread() {
    let db = Db::open_in_memory().unwrap();
    let loaded = db.run(|db| db.load()).await.unwrap();
    assert!(loaded.daemons.is_empty());
}

/// Log a request of `login` served by account `account` of `owner`, and end
/// it with `usage` (none when the provider reported none).
fn used(
    db: &Db,
    id: i64,
    (login, owner): (Option<&str>, &str),
    (account, model): (&str, Option<&str>),
    usage: Option<TokenUsage>,
) {
    let mut record = request_record(id, t0() + TimeDelta::seconds(id));
    record.login = login.map(str::to_owned);
    record.owner = Some(owner.to_owned());
    record.account_id = Some(account.to_owned());
    record.model = model.map(str::to_owned);
    db.record_request(&record, &[]).unwrap();
    let done = Completion {
        duration_ms: 5,
        responses: 1,
        usage,
        error: None,
    };
    db.complete_request(id, &done).unwrap();
}

fn tokens(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input,
        cache_read: 0,
        cache_write: 0,
        output,
        reasoning: 0,
    }
}

#[test]
fn usage_sums_large_token_counts_without_integer_overflow() {
    let db = Db::open_in_memory().unwrap();
    for id in 1..=3 {
        used(
            &db,
            id,
            (Some("alice"), "alice"),
            ("c-a", None),
            Some(tokens(i64::MAX as u64, i64::MAX as u64)),
        );
    }
    let rows = db.usage("alice", t0()).unwrap();
    assert_eq!(rows[0].tokens.input, u64::MAX);
    assert_eq!(rows[0].tokens.output, u64::MAX);
}

#[test]
fn usage_groups_by_account_login_and_model() {
    const ALICE: &str = "alice@example.com";
    const BOB: &str = "bob@example.com";
    let db = Db::open_in_memory().unwrap();
    used(
        &db,
        1,
        (Some(ALICE), ALICE),
        ("a", Some("m1")),
        Some(tokens(10, 1)),
    );
    used(
        &db,
        2,
        (Some(ALICE), ALICE),
        ("a", Some("m1")),
        Some(tokens(20, 2)),
    );
    used(
        &db,
        3,
        (Some(BOB), ALICE),
        ("a", Some("m1")),
        Some(tokens(5, 0)),
    );
    used(
        &db,
        4,
        (Some(ALICE), ALICE),
        ("a", Some("m2")),
        Some(tokens(7, 0)),
    );
    used(
        &db,
        5,
        (Some(ALICE), ALICE),
        ("b", Some("m1")),
        Some(tokens(9, 0)),
    );
    used(&db, 6, (None, ALICE), ("a", None), Some(tokens(3, 0)));

    let rows = db.usage(ALICE, t0()).unwrap();
    assert_eq!(rows.len(), 5);
    let row = |account: &str, login: Option<&str>, model: Option<&str>| {
        rows.iter()
            .find(|row| {
                row.account_id.as_str() == account
                    && row.login.as_deref() == login
                    && row.model.as_deref() == model
            })
            .unwrap_or_else(|| panic!("no row for {account} {login:?} {model:?}"))
    };
    let alice = row("a", Some(ALICE), Some("m1"));
    assert_eq!(alice.requests, 2);
    assert_eq!(alice.responses, 2);
    assert_eq!(alice.tokens, tokens(30, 3));
    assert_eq!(alice.provider, Provider::Claude);
    assert_eq!(alice.last_used, t0() + TimeDelta::seconds(2));
    assert_eq!(row("a", Some(BOB), Some("m1")).tokens, tokens(5, 0));
    assert_eq!(row("a", Some(ALICE), Some("m2")).tokens, tokens(7, 0));
    assert_eq!(row("b", Some(ALICE), Some("m1")).tokens, tokens(9, 0));
    assert_eq!(row("a", None, None).tokens, tokens(3, 0));
}

#[test]
fn usage_counts_requests_without_usage() {
    const ALICE: &str = "alice@example.com";
    let db = Db::open_in_memory().unwrap();
    used(&db, 1, (Some(ALICE), ALICE), ("a", None), None);
    used(
        &db,
        2,
        (Some(ALICE), ALICE),
        ("a", None),
        Some(tokens(4, 1)),
    );
    let mut unfinished = request_record(3, t0() + TimeDelta::seconds(3));
    unfinished.login = Some(ALICE.to_owned());
    unfinished.owner = Some(ALICE.to_owned());
    unfinished.account_id = Some("a".to_owned());
    db.record_request(&unfinished, &[]).unwrap();

    let rows = db.usage(ALICE, t0()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].requests, 3);
    assert_eq!(rows[0].responses, 1);
    assert_eq!(rows[0].tokens, tokens(4, 1));

    let only_empty = Db::open_in_memory().unwrap();
    used(&only_empty, 1, (Some(ALICE), ALICE), ("a", None), None);
    let rows = only_empty.usage(ALICE, t0()).unwrap();
    assert_eq!(rows[0].requests, 1);
    assert_eq!(rows[0].tokens, TokenUsage::default());
    assert_eq!(rows[0].spend, 0.0);
}

#[test]
fn usage_covers_the_period_from_since_on() {
    const ALICE: &str = "alice@example.com";
    let db = Db::open_in_memory().unwrap();
    used(
        &db,
        1,
        (Some(ALICE), ALICE),
        ("a", None),
        Some(tokens(1, 0)),
    );
    used(
        &db,
        10,
        (Some(ALICE), ALICE),
        ("a", None),
        Some(tokens(2, 0)),
    );
    used(
        &db,
        20,
        (Some(ALICE), ALICE),
        ("a", None),
        Some(tokens(4, 0)),
    );

    let cutoff = t0() + TimeDelta::seconds(10);
    let rows = db.usage(ALICE, cutoff).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].requests, 2, "a request at the cutoff counts");
    assert_eq!(rows[0].tokens, tokens(6, 0));
    assert!(
        db.usage(ALICE, t0() + TimeDelta::seconds(21))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn usage_belongs_to_the_owner_ignoring_case() {
    const ALICE: &str = "alice@example.com";
    const BOB: &str = "bob@example.com";
    let db = Db::open_in_memory().unwrap();
    used(
        &db,
        1,
        (Some(ALICE), ALICE),
        ("a", None),
        Some(tokens(1, 0)),
    );
    used(&db, 2, (Some(BOB), ALICE), ("a", None), Some(tokens(2, 0)));
    used(&db, 3, (Some(BOB), BOB), ("b", None), Some(tokens(4, 0)));
    used(&db, 4, (Some(ALICE), BOB), ("b", None), Some(tokens(8, 0)));
    let mut unserved = request_record(5, t0() + TimeDelta::seconds(5));
    unserved.owner = Some(ALICE.to_owned());
    unserved.account_id = None;
    db.record_request(&unserved, &[]).unwrap();

    let alice = db.usage("ALICE@Example.COM", t0()).unwrap();
    let total: u64 = alice.iter().map(|row| row.tokens.input).sum();
    assert_eq!(total, 3, "own and others' requests on the owner's accounts");
    assert!(alice.iter().all(|row| row.account_id.as_str() == "a"));
    assert!(alice.iter().any(|row| row.login.as_deref() == Some(ALICE)));
    assert!(alice.iter().any(|row| row.login.as_deref() == Some(BOB)));

    let bob = db.usage(BOB, t0()).unwrap();
    assert_eq!(bob.iter().map(|row| row.tokens.input).sum::<u64>(), 12);
    assert!(db.usage("carol@example.com", t0()).unwrap().is_empty());
}

#[test]
fn usage_sorts_by_spend_and_prices_the_model() {
    const ALICE: &str = "alice@example.com";
    let db = Db::open_in_memory().unwrap();
    used(
        &db,
        1,
        (Some(ALICE), ALICE),
        ("a", Some("claude-haiku-4")),
        Some(tokens(90, 0)),
    );
    used(
        &db,
        2,
        (Some(ALICE), ALICE),
        ("b", Some("claude-opus-4")),
        Some(tokens(30, 0)),
    );
    used(
        &db,
        3,
        (Some(ALICE), ALICE),
        ("c", Some("claude-sonnet-4")),
        Some(tokens(40, 0)),
    );

    let rows = db.usage(ALICE, t0()).unwrap();
    let order: Vec<&str> = rows.iter().map(|row| row.account_id.as_str()).collect();
    assert_eq!(order, ["b", "c", "a"]);
    assert!(rows[0].spend > rows[1].spend && rows[1].spend > rows[2].spend);
    let expected = mesh_core::spend::spend(Provider::Claude, Some("claude-opus-4"), &tokens(30, 0));
    assert_eq!(rows[0].spend, expected);
}

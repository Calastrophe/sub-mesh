use std::time::Duration;

use chrono::Utc;
use mesh_core::api::{LogEntry, TokenUsage, Transport};

use super::*;

fn record() -> RequestRecord {
    RequestRecord {
        id: 0,
        ts: Utc::now(),
        consumer: "laptop".to_owned(),
        login: Some("alice@example.com".to_owned()),
        provider: "codex".to_owned(),
        transport: Transport::Http,
        method: "POST".to_owned(),
        path: "responses".to_owned(),
        model: None,
        client: None,
        session: None,
        account_id: None,
        owner: None,
        node: None,
        status: 200,
        latency_ms: 1,
        attempts: 1,
    }
}

async fn send(log: &Log) -> Entry {
    let slot = log.reserve().await.unwrap();
    slot.send(record(), Vec::new(), Instant::now())
}

async fn entries(db: &Db, wanted: impl Fn(&[LogEntry]) -> bool) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    for _ in 0..100 {
        entries = db
            .run(|db| db.recent_requests_of_anyone(1000))
            .await
            .unwrap();
        if wanted(&entries) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    entries
}

#[tokio::test]
async fn ids_follow_the_highest_stored_id_and_grow_with_each_request() {
    let db = Db::open_in_memory().unwrap();
    db.record_request(&RequestRecord { id: 41, ..record() }, &[])
        .unwrap();
    let log = Log::start(db.clone()).unwrap();
    let first = send(&log).await;
    let second = send(&log).await;
    assert_eq!((first.id, second.id), (42, 43));
    first.complete(Completion::default());
    second.complete(Completion::default());

    let rows = entries(&db, |rows| {
        rows.len() == 3
            && rows
                .iter()
                .all(|row| row.duration_ms.is_some() || row.id == 41)
    })
    .await;
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    assert_eq!(ids, [43, 42, 41]);
}

#[tokio::test]
async fn ids_keep_growing_after_every_row_is_pruned() {
    let db = Db::open_in_memory().unwrap();
    db.record_request(
        &RequestRecord {
            id: 41,
            ts: Utc::now() - chrono::TimeDelta::days(30),
            ..record()
        },
        &[],
    )
    .unwrap();
    assert_eq!(db.prune_requests(Utc::now()).unwrap(), 1);
    let log = Log::start(db.clone()).unwrap();
    assert_eq!(send(&log).await.id, 42);
}

#[tokio::test]
async fn completing_an_entry_updates_its_row() {
    let db = Db::open_in_memory().unwrap();
    let log = Log::start(db.clone()).unwrap();
    let entry = send(&log).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let usage = TokenUsage {
        input: 5,
        ..TokenUsage::default()
    };
    entry.complete(Completion {
        duration_ms: 1,
        responses: 1,
        usage: Some(usage),
        error: Some("late".to_owned()),
    });
    let rows = entries(&db, |rows| rows.iter().any(|row| row.duration_ms.is_some())).await;
    let row = &rows[0];
    assert!(
        row.duration_ms.unwrap() >= 30,
        "the entry measures the duration itself: {:?}",
        row.duration_ms
    );
    assert_eq!(row.responses, 1);
    assert_eq!(row.usage, Some(usage));
    assert_eq!(row.error.as_deref(), Some("late"));
}

#[tokio::test]
async fn a_dropped_entry_still_gets_its_duration() {
    let db = Db::open_in_memory().unwrap();
    let log = Log::start(db.clone()).unwrap();
    drop(send(&log).await);
    let rows = entries(&db, |rows| rows.iter().any(|row| row.duration_ms.is_some())).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].duration_ms.is_some());
    assert_eq!((rows[0].responses, rows[0].usage), (0, None));
}

#[tokio::test]
async fn many_completions_can_queue_without_waiting() {
    let db = Db::open_in_memory().unwrap();
    let log = Log::start(db.clone()).unwrap();
    let count = MAX_PENDING_REQUESTS / 2;
    let mut pending = Vec::new();
    for _ in 0..count {
        pending.push(send(&log).await);
    }
    drop(pending);
    let rows = entries(&db, |rows| {
        rows.len() == count && rows.iter().all(|row| row.duration_ms.is_some())
    })
    .await;
    assert_eq!(rows.len(), count);
    assert!(rows.iter().all(|row| row.duration_ms.is_some()));
}

#[tokio::test]
async fn starting_closes_the_rows_that_a_stopped_server_left_open() {
    let db = Db::open_in_memory().unwrap();
    let opened = RequestRecord {
        id: 7,
        ts: Utc::now() - chrono::TimeDelta::seconds(120),
        transport: Transport::Websocket,
        ..record()
    };
    db.record_request(&opened, &[]).unwrap();
    db.complete_request(
        7,
        &Completion {
            duration_ms: 3,
            error: None,
            ..Completion::default()
        },
    )
    .unwrap();
    let cut = RequestRecord {
        id: 8,
        ..opened.clone()
    };
    db.record_request(&cut, &[]).unwrap();

    let log = Log::start(db.clone()).unwrap();
    let rows = db.recent_requests_of_anyone(10).unwrap();
    let (cut, done) = (&rows[0], &rows[1]);
    assert_eq!((cut.id, done.id), (8, 7));
    assert!(cut.duration_ms.unwrap() >= 120_000, "{:?}", cut.duration_ms);
    assert_eq!(cut.error.as_deref(), Some("cut off by a server restart"));
    assert_eq!((done.duration_ms, done.error.as_deref()), (Some(3), None));

    let entry = send(&log).await;
    assert_eq!(entry.id, 9);
    drop(entry);
}

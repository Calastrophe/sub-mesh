use chrono::{DateTime, Utc};
use mesh_core::api::{
    AccessRequest, ListGroup, ListKind, MAX_ACCESS_REQUESTS, RequestState, SettingName,
    SettingValue,
};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};

use super::{Db, time_text};

#[derive(Clone, Debug, PartialEq)]
pub struct StoredEntry {
    pub group: ListGroup,
    pub kind: ListKind,
    pub value: String,
    pub added_by: Option<String>,
    pub added_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredValue {
    pub name: SettingName,
    pub value: SettingValue,
    pub set_by: Option<String>,
    pub set_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoredSettings {
    pub entries: Vec<StoredEntry>,
    pub values: Vec<StoredValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestOutcome {
    Existing(AccessRequest),
    Created(AccessRequest),
    Full,
}

fn list_key(group: ListGroup, kind: ListKind) -> String {
    format!("{}.{}", group.as_str(), kind.as_str())
}

fn parse_list(key: &str) -> Option<(ListGroup, ListKind)> {
    let (group, kind) = key.split_once('.')?;
    let group = ListGroup::ALL
        .into_iter()
        .find(|known| known.as_str() == group)?;
    Some((group, kind.parse().ok()?))
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.to_utc())
}

fn parse_state(text: &str) -> Option<RequestState> {
    match text {
        "pending" => Some(RequestState::Pending),
        "denied" => Some(RequestState::Denied),
        _ => None,
    }
}

const REQUEST_COLUMNS: &str = "login, note, state, requested_at, decided_by, decided_at";

fn request_row(row: &Row<'_>) -> rusqlite::Result<Option<AccessRequest>> {
    let state: String = row.get(2)?;
    let requested_at: String = row.get(3)?;
    let decided_at: Option<String> = row.get(5)?;
    let (Some(state), Some(requested_at)) = (parse_state(&state), parse_time(&requested_at)) else {
        return Ok(None);
    };
    Ok(Some(AccessRequest {
        login: row.get(0)?,
        note: row.get(1)?,
        state,
        requested_at,
        decided_by: row.get(4)?,
        decided_at: decided_at.as_deref().and_then(parse_time),
    }))
}

fn find_request(tx: &Transaction<'_>, login: &str) -> rusqlite::Result<Option<AccessRequest>> {
    Ok(tx
        .query_row(
            &format!("SELECT {REQUEST_COLUMNS} FROM access_requests WHERE login = ?1"),
            [login],
            request_row,
        )
        .optional()?
        .flatten())
}

fn insert_entry(tx: &Transaction<'_>, entry: &StoredEntry) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO access_entries (list, value, added_by, added_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            list_key(entry.group, entry.kind),
            entry.value,
            entry.added_by,
            time_text(entry.added_at)
        ],
    )?;
    if (entry.group, entry.kind) == (ListGroup::Consumers, ListKind::Logins) {
        tx.execute(
            "DELETE FROM access_requests WHERE login = ?1",
            [&entry.value],
        )?;
    }
    Ok(())
}

impl Db {
    pub fn load_settings(&self) -> anyhow::Result<StoredSettings> {
        let conn = self.conn();
        let mut stored = StoredSettings::default();
        let mut stmt = conn.prepare(
            "SELECT list, value, added_by, added_at FROM access_entries ORDER BY added_at, value",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let list: String = row.get(0)?;
            let value: String = row.get(1)?;
            let added_at: String = row.get(3)?;
            match (parse_list(&list), parse_time(&added_at)) {
                (Some((group, kind)), Some(added_at)) => stored.entries.push(StoredEntry {
                    group,
                    kind,
                    value,
                    added_by: row.get(2)?,
                    added_at,
                }),
                _ => tracing::warn!(%list, %value, "skipping unreadable access entry"),
            }
        }
        let mut stmt = conn
            .prepare("SELECT name, value, set_by, set_at FROM setting_overrides ORDER BY name")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let value: String = row.get(1)?;
            let set_at: String = row.get(3)?;
            let parsed = name.parse::<SettingName>().ok().zip(
                serde_json::from_str::<SettingValue>(&value)
                    .ok()
                    .zip(parse_time(&set_at)),
            );
            match parsed {
                Some((name, (value, set_at))) => stored.values.push(StoredValue {
                    name,
                    value,
                    set_by: row.get(2)?,
                    set_at,
                }),
                None => tracing::warn!(%name, "skipping unreadable setting override"),
            }
        }
        Ok(stored)
    }

    pub fn add_entry(&self, entry: &StoredEntry) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        insert_entry(&tx, entry)?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_entry(&self, entry: &StoredEntry) -> anyhow::Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM access_entries WHERE list = ?1 AND value = ?2 COLLATE BINARY",
            params![list_key(entry.group, entry.kind), entry.value],
        )? > 0)
    }

    pub fn set_override(&self, value: &StoredValue) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO setting_overrides (name, value, set_by, set_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                value.name.as_str(),
                serde_json::to_string(&value.value)?,
                value.set_by,
                time_text(value.set_at)
            ],
        )?;
        Ok(())
    }

    pub fn remove_override(&self, name: SettingName) -> anyhow::Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM setting_overrides WHERE name = ?1",
            [name.as_str()],
        )? > 0)
    }

    pub fn access_requests(&self) -> anyhow::Result<Vec<AccessRequest>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {REQUEST_COLUMNS} FROM access_requests ORDER BY requested_at, login"
        ))?;
        let rows = stmt.query_map([], request_row)?;
        let mut requests = Vec::new();
        for row in rows {
            requests.extend(row?);
        }
        Ok(requests)
    }

    pub fn access_request(&self, login: &str) -> anyhow::Result<Option<AccessRequest>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        Ok(find_request(&tx, login)?)
    }

    pub fn pending_requests(&self) -> anyhow::Result<u32> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM access_requests WHERE state = 'pending'",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn request_access(
        &self,
        login: &str,
        note: Option<&str>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<RequestOutcome> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = find_request(&tx, login)? {
            return Ok(RequestOutcome::Existing(existing));
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM access_requests WHERE state = 'pending'",
            [],
            |row| row.get(0),
        )?;
        if usize::try_from(count).unwrap_or(usize::MAX) >= MAX_ACCESS_REQUESTS {
            return Ok(RequestOutcome::Full);
        }
        tx.execute(
            "INSERT INTO access_requests (login, note, state, requested_at)
             VALUES (?1, ?2, 'pending', ?3)",
            params![login, note, time_text(now)],
        )?;
        let created = find_request(&tx, login)?;
        tx.commit()?;
        created
            .map(RequestOutcome::Created)
            .ok_or_else(|| anyhow::anyhow!("the new access request did not read back"))
    }

    pub fn approve_request(
        &self,
        login: &str,
        entry: Option<&StoredEntry>,
    ) -> anyhow::Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if find_request(&tx, login)?.is_none() {
            return Ok(false);
        }
        tx.execute("DELETE FROM access_requests WHERE login = ?1", [login])?;
        if let Some(entry) = entry {
            insert_entry(&tx, entry)?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn deny_request(
        &self,
        login: &str,
        by: Option<&str>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        Ok(self.conn().execute(
            "UPDATE access_requests SET state = 'denied', decided_by = ?2, decided_at = ?3
             WHERE login = ?1",
            params![login, by, time_text(now)],
        )? > 0)
    }

    pub fn forget_requests_of(&self, logins: &[String]) -> anyhow::Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut count = 0;
        for login in logins {
            count += tx.execute("DELETE FROM access_requests WHERE login = ?1", [login])?;
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn expire_denied(&self, cutoff: DateTime<Utc>) -> anyhow::Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM access_requests WHERE state = 'denied' AND decided_at < ?1",
            [time_text(cutoff)],
        )?)
    }

    pub fn forget_request(&self, login: &str) -> anyhow::Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM access_requests WHERE login = ?1", [login])?
            > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    fn entry(group: ListGroup, kind: ListKind, value: &str) -> StoredEntry {
        StoredEntry {
            group,
            kind,
            value: value.to_owned(),
            added_by: Some("admin@example.com".to_owned()),
            added_at: t0(),
        }
    }

    #[test]
    fn entries_and_overrides_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let login = entry(ListGroup::Consumers, ListKind::Logins, "bob@example.com");
        let tag = entry(ListGroup::Daemons, ListKind::Tags, "tag:mesh");
        db.add_entry(&login).unwrap();
        db.add_entry(&tag).unwrap();
        db.add_entry(&entry(
            ListGroup::Consumers,
            ListKind::Logins,
            "BOB@example.com",
        ))
        .unwrap();
        let headroom = StoredValue {
            name: SettingName::AffinityMinHeadroom,
            value: SettingValue::Fraction(0.25),
            set_by: None,
            set_at: t0(),
        };
        db.set_override(&headroom).unwrap();
        let stored = db.load_settings().unwrap();
        assert_eq!(stored.entries.len(), 2);
        assert!(stored.entries.contains(&login));
        assert!(stored.entries.contains(&tag));
        assert_eq!(stored.values, [headroom]);

        assert!(db.delete_entry(&login).unwrap());
        assert!(
            !db.delete_entry(&entry(ListGroup::Daemons, ListKind::Logins, "tag:mesh"))
                .unwrap()
        );
        assert!(
            db.remove_override(SettingName::AffinityMinHeadroom)
                .unwrap()
        );
        assert!(
            !db.remove_override(SettingName::AffinityMinHeadroom)
                .unwrap()
        );
        let stored = db.load_settings().unwrap();
        assert_eq!(stored.entries, [tag]);
        assert!(stored.values.is_empty());
    }

    #[test]
    fn a_request_is_made_once_and_decided() {
        let db = Db::open_in_memory().unwrap();
        let RequestOutcome::Created(created) = db
            .request_access("carol@example.com", Some("hi"), t0())
            .unwrap()
        else {
            panic!("no request was created");
        };
        assert_eq!(created.state, RequestState::Pending);
        assert_eq!(
            db.request_access("Carol@example.com", None, t0()).unwrap(),
            RequestOutcome::Existing(created.clone())
        );
        assert_eq!(db.pending_requests().unwrap(), 1);
        assert!(
            db.deny_request("carol@example.com", Some("a"), t0())
                .unwrap()
        );
        let denied = db.access_request("carol@example.com").unwrap().unwrap();
        assert_eq!(denied.state, RequestState::Denied);
        assert_eq!(denied.decided_by.as_deref(), Some("a"));
        assert_eq!(db.pending_requests().unwrap(), 0);

        let approved = entry(ListGroup::Consumers, ListKind::Logins, "carol@example.com");
        assert!(
            db.approve_request("carol@example.com", Some(&approved))
                .unwrap()
        );
        assert!(
            !db.approve_request("carol@example.com", Some(&approved))
                .unwrap()
        );
        assert!(db.access_requests().unwrap().is_empty());
        assert_eq!(db.load_settings().unwrap().entries, [approved]);
    }

    #[test]
    fn requests_are_bounded() {
        let db = Db::open_in_memory().unwrap();
        for index in 0..MAX_ACCESS_REQUESTS {
            let login = format!("user{index}@example.com");
            assert!(matches!(
                db.request_access(&login, None, t0()).unwrap(),
                RequestOutcome::Created(_)
            ));
        }
        assert_eq!(
            db.request_access("late@example.com", None, t0()).unwrap(),
            RequestOutcome::Full
        );
        assert!(db.forget_request("user0@example.com").unwrap());
        assert!(matches!(
            db.request_access("late@example.com", None, t0()).unwrap(),
            RequestOutcome::Created(_)
        ));
    }

    #[test]
    fn tags_differ_by_case_and_logins_do_not() {
        let db = Db::open_in_memory().unwrap();
        for value in ["tag:mesh", "tag:Mesh"] {
            db.add_entry(&entry(ListGroup::Daemons, ListKind::Tags, value))
                .unwrap();
        }
        for value in ["bob@example.com", "BOB@example.com"] {
            db.add_entry(&entry(ListGroup::Daemons, ListKind::Logins, value))
                .unwrap();
        }
        let values: Vec<_> = db
            .load_settings()
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.value)
            .collect();
        assert_eq!(values.len(), 3, "{values:?}");
        assert!(values.contains(&"tag:Mesh".to_owned()));
        assert!(
            !db.delete_entry(&entry(ListGroup::Daemons, ListKind::Tags, "TAG:MESH"))
                .unwrap()
        );
        assert!(
            db.delete_entry(&entry(ListGroup::Daemons, ListKind::Tags, "tag:Mesh"))
                .unwrap()
        );
        assert_eq!(db.load_settings().unwrap().entries.len(), 2);
    }

    #[test]
    fn a_nocase_table_is_rebuilt_with_its_rows() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE access_entries (
                 list     TEXT NOT NULL,
                 value    TEXT NOT NULL COLLATE NOCASE,
                 added_by TEXT,
                 added_at TEXT NOT NULL,
                 PRIMARY KEY (list, value)
             );
             INSERT INTO access_entries VALUES
                 ('daemons.tags', 'tag:mesh', 'a', '2026-01-01T00:00:00Z'),
                 ('consumers.logins', 'Bob@example.com', NULL, '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        let db = Db::init(conn).unwrap();
        assert_eq!(db.load_settings().unwrap().entries.len(), 2);
        db.add_entry(&entry(ListGroup::Daemons, ListKind::Tags, "tag:Mesh"))
            .unwrap();
        db.add_entry(&entry(
            ListGroup::Consumers,
            ListKind::Logins,
            "bob@example.com",
        ))
        .unwrap();
        assert_eq!(db.load_settings().unwrap().entries.len(), 3);
    }

    #[test]
    fn only_pending_requests_fill_the_queue_and_old_denials_expire() {
        let db = Db::open_in_memory().unwrap();
        for index in 0..MAX_ACCESS_REQUESTS {
            let login = format!("user{index}@example.com");
            db.request_access(&login, None, t0()).unwrap();
            assert!(db.deny_request(&login, None, t0()).unwrap());
        }
        assert!(matches!(
            db.request_access("late@example.com", None, t0()).unwrap(),
            RequestOutcome::Created(_)
        ));
        assert_eq!(db.access_requests().unwrap().len(), MAX_ACCESS_REQUESTS + 1);
        let later = t0() + chrono::TimeDelta::days(31);
        assert!(db.deny_request("late@example.com", None, later).unwrap());
        assert_eq!(
            db.expire_denied(later - chrono::TimeDelta::days(30))
                .unwrap(),
            MAX_ACCESS_REQUESTS
        );
        assert_eq!(db.access_requests().unwrap().len(), 1);
        assert_eq!(
            db.forget_requests_of(&["LATE@example.com".to_owned()])
                .unwrap(),
            1
        );
        assert!(db.access_requests().unwrap().is_empty());
    }

    #[test]
    fn adding_a_consumer_drops_its_request() {
        let db = Db::open_in_memory().unwrap();
        db.request_access("dave@example.com", None, t0()).unwrap();
        db.add_entry(&entry(
            ListGroup::Consumers,
            ListKind::Logins,
            "Dave@example.com",
        ))
        .unwrap();
        assert!(db.access_requests().unwrap().is_empty());
    }
}

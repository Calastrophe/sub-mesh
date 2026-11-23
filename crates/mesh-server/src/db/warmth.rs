use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, params};

use super::Db;
use crate::registry::{MAX_KEY_BYTES, Scope, WARMTH_LIMIT, Warmth, WarmthKey, WarmthRow};

pub(super) fn store(conn: &Connection, (key, warmth): &WarmthRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO cache_warmth
             (provider, scope_kind, scope, account_id, last_used_us, ttl_secs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            key.provider.as_str(),
            key.scope.as_str(),
            key.name,
            warmth.account.as_str(),
            warmth.last_used.timestamp_micros(),
            warmth.ttl.num_seconds()
        ],
    )?;
    Ok(())
}

impl Db {
    /// Restore warm entries for known accounts, newest first.
    pub fn load_warmth(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<WarmthRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT w.provider, w.scope_kind, w.scope, w.account_id, w.last_used_us, w.ttl_secs
             FROM cache_warmth w JOIN accounts a ON a.id = w.account_id AND a.provider = w.provider
             WHERE w.last_used_us + w.ttl_secs * 1000000 >= ?1
               AND length(CAST(w.scope AS BLOB)) BETWEEN 1 AND ?2
             ORDER BY w.last_used_us DESC, w.provider, w.scope_kind, w.scope",
        )?;
        let mut rows = stmt.query(params![now.timestamp_micros(), MAX_KEY_BYTES])?;
        let mut loaded = Vec::new();
        while loaded.len() < WARMTH_LIMIT
            && let Some(row) = rows.next()?
        {
            let entry = (|| {
                let key = WarmthKey {
                    provider: row.get_ref(0).ok()?.as_str().ok()?.parse().ok()?,
                    scope: Scope::parse(row.get_ref(1).ok()?.as_str().ok()?)?,
                    name: row.get(2).ok()?,
                };
                let warmth = Warmth {
                    account: row.get_ref(3).ok()?.as_str().ok()?.parse().ok()?,
                    last_used: DateTime::from_timestamp_micros(row.get(4).ok()?)?,
                    ttl: TimeDelta::try_seconds(row.get(5).ok()?)?,
                };
                Some((key, warmth))
            })();
            match entry {
                Some(entry) => loaded.push(entry),
                None => tracing::warn!("skipping unreadable cache warmth row"),
            }
        }
        Ok(loaded)
    }

    /// Delete cold entries and entries for accounts that have gone away.
    pub fn prune_warmth(&self, now: DateTime<Utc>) -> anyhow::Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM cache_warmth
             WHERE last_used_us + ttl_secs * 1000000 < ?1
                OR NOT EXISTS (SELECT 1 FROM accounts a
                               WHERE a.id = account_id AND a.provider = cache_warmth.provider)",
            [now.timestamp_micros()],
        )?)
    }
}

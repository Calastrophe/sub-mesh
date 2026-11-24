//! SQLite persistence: daemons, accounts, a log of relayed requests, and
//! which accounts hold warm prompt caches.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use mesh_core::{AccountReport, DaemonReport};
use rusqlite::{Connection, params};

use crate::registry::DaemonRow;

mod log;
mod schema;
mod settings;
mod warmth;

pub use log::{Activity, Completion, LogOwner, RequestRecord};
pub use settings::{RequestOutcome, StoredEntry, StoredSettings, StoredValue};

fn time_text(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// What the database holds at startup.
#[derive(Debug, Default)]
pub struct Loaded {
    pub daemons: Vec<DaemonRow>,
    /// `(node, report)` pairs.
    pub accounts: Vec<(String, AccountReport)>,
}

/// A shared connection. Calls block, so async code uses [`Db::run`].
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open the database file, creating it and its parent directory.
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        Db::init(conn)
    }

    /// An in-memory database, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Db> {
        Db::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> anyhow::Result<Db> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::initialize(&mut conn).context("cannot initialize schema")?;
        log::register_functions(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run `work` on a blocking thread.
    pub async fn run<T, F>(&self, work: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || work(&db))
            .await
            .context("database task failed")?
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("database mutex poisoned")
    }

    /// Store a daemon report from the machine of `owner`: the daemon row,
    /// and exactly the accounts it reports now.
    pub fn save_report(
        &self,
        report: &DaemonReport,
        owner: Option<&str>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let stamp = time_text(now);
        tx.execute(
            "INSERT INTO daemons (node, owner, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(node) DO UPDATE SET owner = ?2, last_seen = ?3",
            params![report.node, owner, stamp],
        )?;
        tx.execute("DELETE FROM accounts WHERE node = ?1", params![report.node])?;
        for account in &report.accounts {
            tx.execute(
                "INSERT OR REPLACE INTO accounts (id, node, provider, report_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    account.id.as_str(),
                    report.node,
                    account.provider.as_str(),
                    serde_json::to_string(account)?,
                    stamp
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn forget_daemons(&self, nodes: &[String]) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for node in nodes {
            tx.execute("DELETE FROM accounts WHERE node = ?1", [node])?;
            tx.execute("DELETE FROM daemons WHERE node = ?1", [node])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read back every daemon and account. Rows that no longer parse are
    /// skipped with a warning.
    pub fn load(&self) -> anyhow::Result<Loaded> {
        let conn = self.conn();
        let mut loaded = Loaded::default();

        let mut stmt = conn.prepare("SELECT node, owner, last_seen FROM daemons")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (node, owner, last_seen) = row?;
            match DateTime::parse_from_rfc3339(&last_seen) {
                Ok(last_seen) => loaded.daemons.push(DaemonRow {
                    node,
                    owner,
                    last_seen: last_seen.to_utc(),
                }),
                Err(error) => tracing::warn!(%node, %error, "skipping stored daemon"),
            }
        }

        let mut stmt = conn.prepare("SELECT id, node, report_json FROM accounts")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, node, json) = row?;
            match serde_json::from_str::<AccountReport>(&json) {
                Ok(report) => loaded.accounts.push((node, report)),
                Err(error) => tracing::warn!(%id, %error, "skipping stored account"),
            }
        }
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests;

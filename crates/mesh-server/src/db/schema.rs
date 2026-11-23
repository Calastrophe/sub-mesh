use rusqlite::{Connection, TransactionBehavior};

/// The version of the tables, kept in `PRAGMA user_version`.
///
/// The server never reads an older layout. A database from an older version
/// loses its request log, which is disposable. Raise this when the layout of
/// `requests` changes.
const VERSION: i64 = 3;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS daemons (
    node      TEXT PRIMARY KEY,
    owner     TEXT,
    last_seen TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS accounts (
    id          TEXT PRIMARY KEY,
    node        TEXT NOT NULL,
    provider    TEXT NOT NULL,
    report_json TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS requests (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    ts                 TEXT NOT NULL,
    consumer           TEXT NOT NULL,
    login              TEXT,
    provider           TEXT NOT NULL,
    transport          TEXT NOT NULL,
    method             TEXT NOT NULL,
    path               TEXT NOT NULL,
    model              TEXT,
    client             TEXT,
    session            TEXT,
    account_id         TEXT,
    owner              TEXT,
    node               TEXT,
    status             INTEGER NOT NULL,
    error              TEXT,
    latency_ms         INTEGER NOT NULL,
    duration_ms        INTEGER,
    attempts           INTEGER NOT NULL,
    responses          INTEGER NOT NULL DEFAULT 0,
    input_tokens       INTEGER,
    cache_read_tokens  INTEGER,
    cache_write_tokens INTEGER,
    output_tokens      INTEGER,
    reasoning_tokens   INTEGER
);
CREATE INDEX IF NOT EXISTS requests_ts ON requests (ts);
CREATE INDEX IF NOT EXISTS requests_login ON requests (login COLLATE NOCASE, id);
CREATE INDEX IF NOT EXISTS requests_anonymous ON requests (consumer, id) WHERE login IS NULL;
CREATE INDEX IF NOT EXISTS requests_owner ON requests (owner COLLATE NOCASE, id);
CREATE TABLE IF NOT EXISTS cache_warmth (
    provider   TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope      TEXT NOT NULL,
    account_id TEXT NOT NULL,
    last_used_us INTEGER NOT NULL,
    ttl_secs   INTEGER NOT NULL CHECK (ttl_secs BETWEEN 1 AND 4294967295),
    PRIMARY KEY (provider, scope_kind, scope)
);
";

/// Create the tables, serialized even across concurrent starts. A database
/// of an older [`VERSION`] gets a fresh request log.
pub(super) fn initialize(conn: &mut Connection) -> anyhow::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let outdated = version < VERSION;
    if outdated {
        tx.execute_batch("DROP TABLE IF EXISTS requests;")?;
    }
    tx.execute_batch(SCHEMA)?;
    if outdated {
        tx.pragma_update(None, "user_version", VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

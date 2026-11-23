use std::collections::HashMap;

use chrono::{DateTime, Utc};
use mesh_core::api::{Direction, LogEntry, TokenUsage, Transport, UsageRow};
use mesh_core::spend::spend;
use mesh_core::{AccountId, Provider};
use rusqlite::{Row, params};

use super::{Db, time_text, warmth};
use crate::registry::WarmthRow;

/// One relayed request, as logged when its response starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRecord {
    /// Increases with each request. The slot that queues the record sets it.
    pub id: i64,
    pub ts: DateTime<Utc>,
    /// The caller's node name.
    pub consumer: String,
    /// The caller's tailnet login, when known.
    pub login: Option<String>,
    pub provider: String,
    pub transport: Transport,
    pub method: String,
    /// The path below the provider's prefix, without the query.
    pub path: String,
    pub model: Option<String>,
    pub client: Option<String>,
    pub session: Option<String>,
    pub account_id: Option<String>,
    /// The tailnet login that owns the daemon that served the request, when
    /// known. The row keeps it after the account or daemon is gone.
    pub owner: Option<String>,
    pub node: Option<String>,
    pub status: u16,
    pub latency_ms: u64,
    pub attempts: u32,
}

/// What became of a request once its response ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completion {
    pub duration_ms: u64,
    pub responses: u32,
    pub usage: Option<TokenUsage>,
    /// An error found late. It never replaces one that the row has.
    pub error: Option<String>,
}

/// Whose log a read returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogOwner {
    /// The caller has this login. Its log holds the requests it made
    /// (outbound) and the requests that others made with subscriptions it
    /// owns (inbound). Logins compare ignoring ASCII case.
    Login(String),
    /// The caller has no login, which only happens without whois. Its log
    /// holds the requests without a login that came from this node. It owns
    /// no subscriptions, so it has no inbound requests.
    Node(String),
}

impl LogOwner {
    fn key(&self) -> &str {
        match self {
            LogOwner::Login(key) | LogOwner::Node(key) => key,
        }
    }

    /// The condition on `requests` for the rows the caller made, with the
    /// owner's [`key`](LogOwner::key) as `?1`.
    fn outbound(&self) -> &'static str {
        match self {
            LogOwner::Login(_) => "login = ?1 COLLATE NOCASE",
            LogOwner::Node(_) => "login IS NULL AND consumer = ?1",
        }
    }

    /// The condition for the rows that someone else made with the caller's
    /// subscriptions. It excludes the rows of [`LogOwner::outbound`], so the
    /// two sides never share a row.
    fn inbound(&self) -> Option<&'static str> {
        match self {
            LogOwner::Login(_) => {
                Some("owner = ?1 COLLATE NOCASE AND (login IS NULL OR login <> ?1 COLLATE NOCASE)")
            }
            LogOwner::Node(_) => None,
        }
    }
}

/// The rows of one side of a log read.
struct Side {
    /// The condition on `requests`, with the key of the owner as `?1`.
    condition: &'static str,
    outbound: bool,
}

/// How many requests one login made in a period.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginCount {
    pub login: String,
    pub requests: u64,
}

/// Request counts for a period.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    /// Requests per account id.
    pub by_account: HashMap<String, u64>,
    /// Requests per login, busiest first.
    pub by_login: Vec<LoginCount>,
}

const LOG_COLUMNS: &str = "id, ts, login, consumer, provider, transport, method, path, model, \
     client, session, account_id, owner, node, status, error, latency_ms, duration_ms, attempts, \
     responses, input_tokens, cache_read_tokens, cache_write_tokens, output_tokens, \
     reasoning_tokens";

/// The error of a request that no response end completed.
const CUT_OFF: &str = "cut off by a server restart";

fn transport_text(transport: Transport) -> &'static str {
    match transport {
        Transport::Http => "http",
        Transport::Websocket => "websocket",
    }
}

fn parse_transport(text: &str) -> Option<Transport> {
    match text {
        "http" => Some(Transport::Http),
        "websocket" => Some(Transport::Websocket),
        _ => None,
    }
}

fn to_sql(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

fn from_sql(count: Option<i64>) -> u64 {
    count
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0)
}

/// A row of the request log, as stored.
struct StoredRequest {
    id: i64,
    ts: String,
    login: Option<String>,
    consumer: String,
    provider: String,
    transport: String,
    method: String,
    path: String,
    model: Option<String>,
    client: Option<String>,
    session: Option<String>,
    account_id: Option<String>,
    owner: Option<String>,
    node: Option<String>,
    status: i64,
    error: Option<String>,
    latency_ms: i64,
    duration_ms: Option<i64>,
    attempts: i64,
    responses: i64,
    input_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    outbound: bool,
}

/// SQLite integer SUM overflows if a daemon reports very large counts.
struct SumTokens;

impl rusqlite::functions::Aggregate<u64, String> for SumTokens {
    fn init(&self, _: &mut rusqlite::functions::Context<'_>) -> rusqlite::Result<u64> {
        Ok(0)
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        sum: &mut u64,
    ) -> rusqlite::Result<()> {
        *sum = sum.saturating_add(from_sql(ctx.get(0)?));
        Ok(())
    }

    fn finalize(
        &self,
        _: &mut rusqlite::functions::Context<'_>,
        sum: Option<u64>,
    ) -> rusqlite::Result<String> {
        Ok(sum.unwrap_or_default().to_string())
    }
}

pub(super) fn register_functions(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;

    conn.create_aggregate_function(
        "sum_tokens",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        SumTokens,
    )
}

/// The requests of one group of [`Db::usage`], as stored.
struct UsageGroup {
    account_id: String,
    provider: String,
    login: Option<String>,
    model: Option<String>,
    requests: i64,
    last_used: String,
    responses: i64,
    tokens: [String; 5],
}

impl UsageGroup {
    fn into_row(self) -> Option<UsageRow> {
        let provider = self.provider.parse::<Provider>().ok()?;
        let [input, cache_read, cache_write, output, reasoning] =
            self.tokens.map(|count| count.parse().ok());
        let (input, cache_read, cache_write, output, reasoning) =
            (input?, cache_read?, cache_write?, output?, reasoning?);
        let tokens = TokenUsage {
            input,
            cache_read,
            cache_write,
            output,
            reasoning,
        };
        Some(UsageRow {
            account_id: self.account_id.parse::<AccountId>().ok()?,
            provider,
            spend: spend(provider, self.model.as_deref(), &tokens),
            login: self.login,
            model: self.model,
            requests: from_sql(Some(self.requests)),
            responses: from_sql(Some(self.responses)),
            tokens,
            last_used: DateTime::parse_from_rfc3339(&self.last_used).ok()?.to_utc(),
        })
    }
}

impl StoredRequest {
    fn read(row: &Row<'_>) -> rusqlite::Result<StoredRequest> {
        Ok(StoredRequest {
            id: row.get("id")?,
            ts: row.get("ts")?,
            login: row.get("login")?,
            consumer: row.get("consumer")?,
            provider: row.get("provider")?,
            transport: row.get("transport")?,
            method: row.get("method")?,
            path: row.get("path")?,
            model: row.get("model")?,
            client: row.get("client")?,
            session: row.get("session")?,
            account_id: row.get("account_id")?,
            owner: row.get("owner")?,
            node: row.get("node")?,
            status: row.get("status")?,
            error: row.get("error")?,
            latency_ms: row.get("latency_ms")?,
            duration_ms: row.get("duration_ms")?,
            attempts: row.get("attempts")?,
            responses: row.get("responses")?,
            input_tokens: row.get("input_tokens")?,
            cache_read_tokens: row.get("cache_read_tokens")?,
            cache_write_tokens: row.get("cache_write_tokens")?,
            output_tokens: row.get("output_tokens")?,
            reasoning_tokens: row.get("reasoning_tokens")?,
            outbound: row.get("outbound")?,
        })
    }

    fn into_entry(self) -> Option<LogEntry> {
        let usage = self.input_tokens.map(|input| TokenUsage {
            input: from_sql(Some(input)),
            cache_read: from_sql(self.cache_read_tokens),
            cache_write: from_sql(self.cache_write_tokens),
            output: from_sql(self.output_tokens),
            reasoning: from_sql(self.reasoning_tokens),
        });
        Some(LogEntry {
            id: self.id,
            ts: DateTime::parse_from_rfc3339(&self.ts).ok()?.to_utc(),
            direction: if self.outbound {
                Direction::Outbound
            } else {
                Direction::Inbound
            },
            login: self.login,
            consumer: self.consumer,
            provider: self.provider.parse::<Provider>().ok()?,
            transport: parse_transport(&self.transport)?,
            method: self.method,
            path: self.path,
            model: self.model,
            client: self.client,
            session: self.session,
            account_id: self.account_id.and_then(|id| id.parse::<AccountId>().ok()),
            owner: self.owner,
            node: self.node,
            status: u16::try_from(self.status).ok()?,
            error: self.error,
            latency_ms: u64::try_from(self.latency_ms).ok()?,
            duration_ms: self
                .duration_ms
                .and_then(|duration| u64::try_from(duration).ok()),
            attempts: u32::try_from(self.attempts).ok()?,
            responses: u32::try_from(self.responses).ok()?,
            usage,
        })
    }
}

impl Db {
    /// Log a request and store the cache warmth it left, in one transaction.
    pub fn record_request(
        &self,
        record: &RequestRecord,
        warmth: &[WarmthRow],
    ) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO requests
                 (id, ts, consumer, login, provider, transport, method, path, model, client,
                  session, account_id, owner, node, status, latency_ms, attempts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                record.id,
                time_text(record.ts),
                record.consumer,
                record.login,
                record.provider,
                transport_text(record.transport),
                record.method,
                record.path,
                record.model,
                record.client,
                record.session,
                record.account_id,
                record.owner,
                record.node,
                record.status,
                to_sql(record.latency_ms),
                record.attempts
            ],
        )?;
        for row in warmth {
            warmth::store(&tx, row)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record how request `id` ended. A request that is gone, for example
    /// because it was pruned, is left alone.
    pub fn complete_request(&self, id: i64, completion: &Completion) -> anyhow::Result<()> {
        let usage = completion.usage;
        self.conn().execute(
            "UPDATE requests
             SET duration_ms = ?2, responses = ?3, error = COALESCE(error, ?4),
                 input_tokens = ?5, cache_read_tokens = ?6, cache_write_tokens = ?7,
                 output_tokens = ?8, reasoning_tokens = ?9
             WHERE id = ?1",
            params![
                id,
                to_sql(completion.duration_ms),
                completion.responses,
                completion.error,
                usage.map(|usage| to_sql(usage.input)),
                usage.map(|usage| to_sql(usage.cache_read)),
                usage.map(|usage| to_sql(usage.cache_write)),
                usage.map(|usage| to_sql(usage.output)),
                usage.map(|usage| to_sql(usage.reasoning)),
            ],
        )?;
        Ok(())
    }

    /// Close the requests that never saw their response end, such as a
    /// WebSocket that was open when the server stopped. Each gets the time
    /// from its start to `now` as its duration, and the error of a cut-off
    /// when it has none. Returns how many it closed.
    pub fn close_unfinished_requests(&self, now: DateTime<Utc>) -> anyhow::Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let unfinished: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, ts FROM requests WHERE duration_ms IS NULL")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        for (id, ts) in &unfinished {
            let duration = DateTime::parse_from_rfc3339(ts)
                .map_or(0, |ts| (now - ts.to_utc()).num_milliseconds().max(0));
            tx.execute(
                "UPDATE requests SET duration_ms = ?2, error = COALESCE(error, ?3) WHERE id = ?1",
                params![id, duration, CUT_OFF],
            )?;
        }
        tx.commit()?;
        Ok(unfinished.len())
    }

    /// The highest id the request log has used, or `0` when it never held a
    /// row. Pruning cannot lower it: the table keeps its high-water mark in
    /// `sqlite_sequence`, so ids keep growing when every row is gone.
    pub fn last_request_id(&self) -> anyhow::Result<i64> {
        Ok(self.conn().query_row(
            "SELECT MAX(
                 COALESCE((SELECT MAX(id) FROM requests), 0),
                 COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'requests'), 0)
             )",
            [],
            |row| row.get(0),
        )?)
    }

    /// Request counts per account and per login, for requests logged at or
    /// after `since`.
    pub fn activity(&self, since: DateTime<Utc>) -> anyhow::Result<Activity> {
        Ok(Activity {
            by_account: self.account_counts(since)?,
            by_login: self.login_counts(since)?,
        })
    }

    /// Requests per account id logged at or after `since`. Requests that no
    /// account served are left out.
    fn account_counts(&self, since: DateTime<Utc>) -> anyhow::Result<HashMap<String, u64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT account_id, COUNT(*) FROM requests
             WHERE ts >= ?1 AND account_id IS NOT NULL
             GROUP BY account_id",
        )?;
        let rows = stmt.query_map(params![time_text(since)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Requests per login logged at or after `since`, busiest first.
    /// Requests without a login are left out.
    pub fn login_counts(&self, since: DateTime<Utc>) -> anyhow::Result<Vec<LoginCount>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT login, COUNT(*) AS n FROM requests
             WHERE ts >= ?1 AND login IS NOT NULL
             GROUP BY login ORDER BY n DESC, login",
        )?;
        let rows = stmt.query_map(params![time_text(since)], |row| {
            Ok(LoginCount {
                login: row.get(0)?,
                requests: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The newest `limit` requests in the log of `owner`, newest first: the
    /// ones it made and the ones others made with its subscriptions, or only
    /// the ones of `direction`. With `before`, only those with a lower id.
    /// Rows that no longer parse are skipped.
    ///
    /// Each side is a separate select over its own index, merged by id, so
    /// the read stops early however many rows the other side has.
    pub fn recent_requests(
        &self,
        owner: &LogOwner,
        direction: Option<Direction>,
        limit: u32,
        before: Option<i64>,
    ) -> anyhow::Result<Vec<LogEntry>> {
        let mut sides = Vec::new();
        if direction != Some(Direction::Inbound) {
            sides.push(Side {
                condition: owner.outbound(),
                outbound: true,
            });
        }
        if direction != Some(Direction::Outbound)
            && let Some(condition) = owner.inbound()
        {
            sides.push(Side {
                condition,
                outbound: false,
            });
        }
        self.select_requests(&sides, owner.key(), limit, before)
    }

    /// The newest `limit` requests of everyone, newest first, as outbound.
    #[cfg(test)]
    pub fn recent_requests_of_anyone(&self, limit: u32) -> anyhow::Result<Vec<LogEntry>> {
        let anyone = Side {
            condition: "?1 IS NOT NULL",
            outbound: true,
        };
        self.select_requests(&[anyone], "", limit, None)
    }

    fn select_requests(
        &self,
        sides: &[Side],
        key: &str,
        limit: u32,
        before: Option<i64>,
    ) -> anyhow::Result<Vec<LogEntry>> {
        if sides.is_empty() {
            return Ok(Vec::new());
        }
        let selects: Vec<String> = sides
            .iter()
            .map(|side| {
                format!(
                    "SELECT {LOG_COLUMNS}, {} AS outbound FROM requests
                     WHERE {} AND id < ?2",
                    u8::from(side.outbound),
                    side.condition
                )
            })
            .collect();
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "{} ORDER BY id DESC LIMIT ?3",
            selects.join(" UNION ALL ")
        ))?;
        let rows = stmt.query_map(
            params![key, before.unwrap_or(i64::MAX), limit],
            StoredRequest::read,
        )?;
        let mut entries = Vec::new();
        for row in rows {
            match row?.into_entry() {
                Some(entry) => entries.push(entry),
                None => tracing::warn!("skipping unreadable request log row"),
            }
        }
        Ok(entries)
    }

    /// What the requests served by the subscriptions of `owner` used since
    /// `since`, one row per account, provider, login (ignoring ASCII case),
    /// and model, largest spend first. Requests that no account served are
    /// left out. A request without usage counts as a request and adds no
    /// tokens. Rows that no longer parse are skipped.
    pub fn usage(&self, owner: &str, since: DateTime<Utc>) -> anyhow::Result<Vec<UsageRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT account_id, provider, MIN(login), model, COUNT(*), MAX(ts),
                    COALESCE(SUM(CASE WHEN input_tokens IS NULL THEN 0 ELSE responses END), 0),
                    sum_tokens(input_tokens), sum_tokens(cache_read_tokens),
                    sum_tokens(cache_write_tokens), sum_tokens(output_tokens),
                    sum_tokens(reasoning_tokens)
             FROM requests
             WHERE owner = ?1 COLLATE NOCASE AND ts >= ?2 AND account_id IS NOT NULL
             GROUP BY account_id, provider, login COLLATE NOCASE, model
             ORDER BY account_id, provider, login COLLATE NOCASE, model",
        )?;
        let groups = stmt.query_map(params![owner, time_text(since)], |row| {
            Ok(UsageGroup {
                account_id: row.get(0)?,
                provider: row.get(1)?,
                login: row.get(2)?,
                model: row.get(3)?,
                requests: row.get(4)?,
                last_used: row.get(5)?,
                responses: row.get(6)?,
                tokens: [
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ],
            })
        })?;
        let mut rows = Vec::new();
        for group in groups {
            match group?.into_row() {
                Some(row) => rows.push(row),
                None => tracing::warn!("skipping unreadable request log row"),
            }
        }
        rows.sort_by(|a, b| b.spend.total_cmp(&a.spend));
        Ok(rows)
    }

    /// Delete requests logged before `cutoff`. Returns the number deleted. It
    /// never lowers [`Db::last_request_id`].
    pub fn prune_requests(&self, cutoff: DateTime<Utc>) -> anyhow::Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM requests WHERE ts < ?1",
            params![time_text(cutoff)],
        )?)
    }
}

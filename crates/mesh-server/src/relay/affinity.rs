//! What a request says about its prompt cache: which conversation it
//! belongs to, and how long the provider keeps the cache.
//!
//! The conversation key comes from the first of these that is present:
//!
//! 1. A session header, preferring the provider's own client (see [`from_headers`]).
//! 2. The request body: Claude's `metadata.user_id`, which embeds the
//!    session id, or Codex's `prompt_cache_key`.
//!
//! Claude can request an hour with `cache_control`. Codex supplies a lifetime in
//! `prompt_cache_options.ttl` or the Responses API's `prompt_cache_retention`.
//!
//! The same parse reads the model: both providers take it from the top-level
//! `model` field, and the relay sends the request only to accounts that may
//! serve it.
//!
//! Plain JSON bodies up to [`FULL_PROBE_BYTES`] are parsed whole. Larger ones, up
//! to [`MAX_SESSION_SCAN_BYTES`], get a bounded scan on the blocking pool that
//! borrows the key and skips prompt content. Claude's scan reads the
//! `cache_control` of the system prompt, the tools, and the last message only,
//! because an earlier message does not show what the current request asks for.
//! Missing or unreadable hints never prevent forwarding.

use std::time::Duration;

use axum::http::{HeaderMap, header};
use bytes::Bytes;
use chrono::TimeDelta;
use mesh_core::Provider;
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::sync::Semaphore;

use crate::registry::MAX_KEY_BYTES;

/// Largest body that is parsed whole, with every message inspected.
const FULL_PROBE_BYTES: usize = 2 * 1024 * 1024;

/// JSON can encode each ASCII byte as a six-byte Unicode escape, plus the enclosing quotes.
const MAX_ENCODED_KEY_BYTES: usize = 2 + 6 * MAX_KEY_BYTES;

/// Largest body that gets the bounded scan, independent of the configured relay body limit.
const MAX_SESSION_SCAN_BYTES: usize = 64 * 1024 * 1024;

/// At most two large bodies are scanned at once. Other requests proceed immediately.
static LARGE_SCANS: Semaphore = Semaphore::const_new(2);

/// How long forwarding may wait for a queued or running large-body scan.
const LARGE_SCAN_TIMEOUT: Duration = Duration::from_millis(250);

/// How long Claude keeps a cache whose `cache_control` asks for `"1h"`.
const CLAUDE_EXTENDED_TTL: TimeDelta = TimeDelta::hours(1);

/// How long Codex keeps a cache whose request asks for `"24h"`.
const CODEX_EXTENDED_TTL: TimeDelta = TimeDelta::hours(24);

/// The longest model id read from a body, in bytes.
const MAX_MODEL_BYTES: usize = 256;

/// JSON can encode each ASCII byte as a six-byte Unicode escape, plus the enclosing quotes.
const MAX_ENCODED_MODEL_BYTES: usize = 2 + 6 * MAX_MODEL_BYTES;

/// What a request body says about its prompt cache and its model.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BodyHints {
    /// The conversation key.
    pub session: Option<String>,
    /// How long the body asks the provider to keep the cache, when it asks
    /// for more than the default.
    pub ttl: Option<TimeDelta>,
    /// The model the request asks for, which both providers take from the
    /// top-level `model` field.
    pub model: Option<String>,
}

/// The first usable session header, preferring the provider's own client.
pub fn from_headers(provider: Provider, headers: &HeaderMap) -> Option<String> {
    let names = match provider {
        Provider::Claude => [
            "x-claude-code-session-id",
            "session-id",
            "session_id",
            "x-session-id",
            "conversation_id",
        ],
        Provider::Codex => [
            "session-id",
            "session_id",
            "x-session-id",
            "conversation_id",
            "x-claude-code-session-id",
        ],
    };
    names.into_iter().find_map(|name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(usable)
    })
}

/// Inspect a buffered HTTP request. Large scans are optional work for the blocking pool.
pub async fn http_hints(provider: Provider, headers: &HeaderMap, body: &Bytes) -> BodyHints {
    if body.len() <= FULL_PROBE_BYTES {
        return small_body_hints(provider, headers, body);
    }
    if body.len() > MAX_SESSION_SCAN_BYTES || !is_plain_json(headers) {
        return BodyHints::default();
    }
    let Ok(permit) = LARGE_SCANS.try_acquire() else {
        return BodyHints::default();
    };
    let body = body.clone();
    let mut scan = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        large_body_hints(provider, &body)
    });
    match tokio::time::timeout(LARGE_SCAN_TIMEOUT, &mut scan).await {
        Ok(Ok(hints)) => hints,
        Ok(Err(_)) => BodyHints::default(),
        Err(_) => {
            scan.abort();
            BodyHints::default()
        }
    }
}

/// Read the key and the lifetime of a large body while skipping prompt content.
fn large_body_hints(provider: Provider, body: &[u8]) -> BodyHints {
    if body.trim_ascii_start().first() != Some(&b'{') {
        return BodyHints::default();
    }
    match provider {
        Provider::Claude => {
            serde_json::from_slice(body).map(|probe: LargeClaudeProbe| probe.hints())
        }
        Provider::Codex => serde_json::from_slice(body).map(|probe: CodexProbe| probe.hints()),
    }
    .unwrap_or_default()
}

fn small_body_hints(provider: Provider, headers: &HeaderMap, body: &[u8]) -> BodyHints {
    if !is_plain_json(headers) {
        return BodyHints::default();
    }
    match provider {
        Provider::Claude => serde_json::from_slice(body).map(|probe: ClaudeProbe| probe.hints()),
        Provider::Codex => serde_json::from_slice(body).map(|probe: CodexProbe| probe.hints()),
    }
    .unwrap_or_default()
}

/// The fields of a Claude request that matter. Serde skips every other
/// field without building it, and a field of an unexpected type says
/// nothing. The model stays raw so that its length is checked before it is
/// decoded.
#[derive(Deserialize)]
struct ClaudeProbe<'a> {
    #[serde(borrow)]
    model: Option<&'a RawValue>,
    metadata: Option<Value>,
    system: Option<Value>,
    tools: Option<Value>,
    messages: Option<Value>,
}

impl ClaudeProbe<'_> {
    fn hints(self) -> BodyHints {
        BodyHints {
            model: self.model.and_then(encoded_model),
            session: self
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("user_id"))
                .and_then(Value::as_str)
                .and_then(usable),
            ttl: claude_ttl(
                self.system.as_ref(),
                self.tools.as_ref(),
                blocks(self.messages.as_ref()),
            ),
        }
    }
}

/// The fields of a large Claude request that matter. The key and the
/// messages stay raw, so that only the last message is ever built.
#[derive(Deserialize)]
struct LargeClaudeProbe<'a> {
    #[serde(borrow)]
    model: Option<&'a RawValue>,
    #[serde(borrow)]
    metadata: Option<&'a RawValue>,
    system: Option<Value>,
    tools: Option<Value>,
    #[serde(borrow)]
    messages: Option<Vec<&'a RawValue>>,
}

impl LargeClaudeProbe<'_> {
    fn hints(self) -> BodyHints {
        #[derive(Deserialize)]
        struct Metadata<'a> {
            #[serde(borrow)]
            user_id: Option<&'a RawValue>,
        }

        let session = self
            .metadata
            .map(|metadata| metadata.get())
            .filter(|metadata| metadata.starts_with('{'))
            .and_then(|metadata| serde_json::from_str::<Metadata>(metadata).ok())
            .and_then(|metadata| metadata.user_id)
            .and_then(encoded_key);
        let last = self
            .messages
            .as_deref()
            .and_then(<[_]>::last)
            .and_then(|message| serde_json::from_str::<Value>(message.get()).ok());
        BodyHints {
            session,
            ttl: claude_ttl(self.system.as_ref(), self.tools.as_ref(), last.as_ref()),
            model: self.model.and_then(encoded_model),
        }
    }
}

/// The lifetime a Claude request asks for: an hour, when the system prompt,
/// the tools, or the given messages carry a block that says so.
fn claude_ttl<'a>(
    system: Option<&Value>,
    tools: Option<&Value>,
    messages: impl IntoIterator<Item = &'a Value>,
) -> Option<TimeDelta> {
    let in_messages = messages
        .into_iter()
        .flat_map(|message| blocks(message.get("content")))
        .any(|block| {
            asks_for_an_hour(block)
                || (block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && blocks(block.get("content")).any(asks_for_an_hour))
        });
    (in_messages || blocks(system).chain(blocks(tools)).any(asks_for_an_hour))
        .then_some(CLAUDE_EXTENDED_TTL)
}

fn blocks(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value.and_then(Value::as_array).into_iter().flatten()
}

fn asks_for_an_hour(block: &Value) -> bool {
    block
        .get("cache_control")
        .and_then(|control| control.get("ttl"))
        .and_then(Value::as_str)
        == Some("1h")
}

/// The fields of a Codex request that matter. The key stays raw so that its
/// length is checked before it is decoded.
#[derive(Deserialize)]
struct CodexProbe<'a> {
    #[serde(borrow)]
    model: Option<&'a RawValue>,
    #[serde(borrow)]
    prompt_cache_key: Option<&'a RawValue>,
    prompt_cache_options: Option<Value>,
    prompt_cache_retention: Option<Value>,
}

impl CodexProbe<'_> {
    fn hints(self) -> BodyHints {
        let ttl = self
            .prompt_cache_options
            .as_ref()
            .and_then(|options| options.get("ttl"))
            .and_then(Value::as_str);
        let ttl = match ttl {
            Some("30m") => Some(TimeDelta::minutes(30)),
            Some("12h") => Some(TimeDelta::hours(12)),
            _ if self.prompt_cache_retention.as_ref().and_then(Value::as_str) == Some("24h") => {
                Some(CODEX_EXTENDED_TTL)
            }
            _ => None,
        };
        BodyHints {
            session: self.prompt_cache_key.and_then(encoded_key),
            ttl,
            model: self.model.and_then(encoded_model),
        }
    }
}

/// The key in a raw JSON string, unless its encoding is too long to be usable.
fn encoded_key(raw: &RawValue) -> Option<String> {
    let raw = raw.get();
    if raw.len() > MAX_ENCODED_KEY_BYTES {
        return None;
    }
    usable(&serde_json::from_str::<String>(raw).ok()?)
}

/// The model id in a raw JSON string, unless its encoding is too long, or it
/// is empty, to be a model id. It is kept as the request has it.
fn encoded_model(raw: &RawValue) -> Option<String> {
    let raw = raw.get();
    if raw.len() > MAX_ENCODED_MODEL_BYTES {
        return None;
    }
    serde_json::from_str::<String>(raw)
        .ok()
        .filter(|model| !model.is_empty() && model.len() <= MAX_MODEL_BYTES)
}

/// Whether the body may be JSON that can be read as it is: not compressed,
/// and not declared as another type. A missing type is tried anyway.
fn is_plain_json(headers: &HeaderMap) -> bool {
    let plain = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .all(|value| {
            value
                .as_bytes()
                .trim_ascii()
                .eq_ignore_ascii_case(b"identity")
        });
    let json = headers.get_all(header::CONTENT_TYPE).iter().all(|value| {
        let Ok(value) = value.to_str() else {
            return false;
        };
        let media = value.split(';').next().unwrap_or("").trim();
        media.eq_ignore_ascii_case("application/json")
            || media.to_ascii_lowercase().ends_with("+json")
    });
    plain && json
}

/// A trimmed key, if it is not empty and not too long.
fn usable(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= MAX_KEY_BYTES).then(|| value.to_owned())
}

#[cfg(test)]
mod tests;

//! Watches a response body on its way to the consumer, then completes the
//! request's log row.
//!
//! The tracker never holds a body back and never keeps one whole. What it
//! reads depends on the response:
//!
//! - An error response (`4xx` or `5xx`): its first [`FAILURE_BYTES`], for the
//!   message of the error.
//! - A successful answer of a model endpoint, as an event stream: the data
//!   lines, one at a time, for the token usage. A line over
//!   [`MAX_LINE_BYTES`] is skipped.
//! - A successful answer of a model endpoint, as one JSON document: its last
//!   [`TAIL_BYTES`], for the token usage.
//! - A successful answer of a model endpoint with no `content-type`, which
//!   ChatGPT sends for its event streams: the first few bytes, until they
//!   show an event stream or a JSON document. Then it reads the body as one
//!   of those.
//! - Anything else, and any body with a `content-encoding`, which the
//!   tracker cannot read: nothing, so the row gets only its duration.
//!
//! The row completes when the body ends, fails, or is dropped.

use std::mem;

use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use mesh_core::Provider;

use super::usage::{Tally, error_message};
use crate::db::Completion;
use crate::log::Entry;

/// How much of an error body is kept.
const FAILURE_BYTES: usize = 4 * 1024;

/// The longest event line that is read. A Codex `response.completed` carries
/// the whole response, so the limit is generous.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// How much of the end of a JSON document is kept.
const TAIL_BYTES: usize = 16 * 1024;

/// The largest line buffer kept between lines.
const RETAINED_LINE_BYTES: usize = 64 * 1024;

/// Follows one response body for the log row of its request.
pub(super) struct Tracker {
    entry: Option<Entry>,
    scan: Scan,
}

impl Tracker {
    pub(super) fn new(
        entry: Entry,
        provider: Provider,
        path: &str,
        response: &Response,
    ) -> Tracker {
        Tracker {
            entry: Some(entry),
            scan: Scan::choose(provider, path, response.status(), response.headers()),
        }
    }

    pub(super) fn observe(&mut self, chunk: &[u8]) {
        self.scan.observe(chunk);
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            entry.complete(mem::take(&mut self.scan).completion());
        }
    }
}

/// What to read of a body.
#[derive(Default)]
pub(super) enum Scan {
    #[default]
    Nothing,
    Failure(Vec<u8>),
    /// A successful answer of a model endpoint with no `content-type`. The
    /// first bytes of the body decide what it is. See [`Sniff`].
    Sniff(Sniff),
    Events {
        lines: DataLines,
        tally: Tally,
    },
    Document {
        tail: Tail,
        tally: Tally,
    },
}

impl Scan {
    fn events(provider: Provider) -> Scan {
        Scan::Events {
            lines: DataLines::default(),
            tally: Tally::new(provider),
        }
    }

    fn document(provider: Provider) -> Scan {
        Scan::Document {
            tail: Tail::default(),
            tally: Tally::new(provider),
        }
    }

    /// Choose from the response head. `path` is the request's path below the
    /// provider's prefix.
    pub(super) fn choose(
        provider: Provider,
        path: &str,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Scan {
        if is_encoded(headers) {
            return Scan::Nothing;
        }
        if status.as_u16() >= 400 {
            return Scan::Failure(Vec::new());
        }
        if !status.is_success() || !is_model_endpoint(provider, path) {
            return Scan::Nothing;
        }
        let Some(kind) = headers.get(CONTENT_TYPE) else {
            return Scan::Sniff(Sniff::new(provider));
        };
        let kind = kind.to_str().unwrap_or_default().to_ascii_lowercase();
        if kind.contains("event-stream") {
            Scan::events(provider)
        } else if kind.contains("json") {
            Scan::document(provider)
        } else {
            Scan::Nothing
        }
    }

    pub(super) fn observe(&mut self, chunk: &[u8]) {
        match self {
            Scan::Nothing => {}
            Scan::Failure(head) => {
                let room = FAILURE_BYTES.saturating_sub(head.len());
                head.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Scan::Sniff(sniff) => {
                if let Some((mut scan, used)) = sniff.feed(chunk) {
                    scan.observe(&chunk[used..]);
                    *self = scan;
                }
            }
            Scan::Events { lines, tally } => lines.push(chunk, &mut |data| tally.event(data)),
            Scan::Document { tail, .. } => tail.push(chunk),
        }
    }

    /// What the body held, once it has ended.
    pub(super) fn completion(self) -> Completion {
        match self {
            Scan::Nothing | Scan::Sniff(_) => Completion::default(),
            Scan::Failure(body) => Completion {
                error: error_message(&body),
                ..Completion::default()
            },
            Scan::Events {
                mut lines,
                mut tally,
            } => {
                lines.finish(&mut |data| tally.event(data));
                tally.completion()
            }
            Scan::Document { tail, mut tally } => {
                tally.document(&tail.bytes);
                tally.completion()
            }
        }
    }
}

/// What the first bytes of a body without a `content-type` say it is.
///
/// ChatGPT answers its event streams with no `content-type` at all. A body
/// that starts, after whitespace, with `event:`, `data:` or `:` is an event
/// stream, and one that starts with `{` is a JSON document. Anything else is
/// read as nothing. The sniff keeps at most the first few bytes, until they
/// decide, and then hands them to the scan that it chose.
pub(super) struct Sniff {
    provider: Provider,
    head: Vec<u8>,
}

enum Kind {
    Events,
    Document,
    Other,
}

impl Sniff {
    fn new(provider: Provider) -> Sniff {
        Sniff {
            provider,
            head: Vec::new(),
        }
    }

    /// Take the next bytes of the body. Once they decide, returns the scan
    /// to read the body with, which has seen the bytes the sniff kept, and
    /// how many bytes of `chunk` the sniff used.
    fn feed(&mut self, chunk: &[u8]) -> Option<(Scan, usize)> {
        for (index, &byte) in chunk.iter().enumerate() {
            if self.head.is_empty() && byte.is_ascii_whitespace() {
                continue;
            }
            self.head.push(byte);
            let Some(kind) = classify(&self.head) else {
                continue;
            };
            let mut scan = match kind {
                Kind::Events => Scan::events(self.provider),
                Kind::Document => Scan::document(self.provider),
                Kind::Other => Scan::Nothing,
            };
            scan.observe(&self.head);
            return Some((scan, index + 1));
        }
        None
    }
}

/// What `head`, the first bytes of a body after its whitespace, are. `None`
/// while more bytes could still change the answer.
fn classify(head: &[u8]) -> Option<Kind> {
    const FIELDS: [&[u8]; 2] = [b"event:", b"data:"];
    match head {
        [b'{', ..] => Some(Kind::Document),
        [b':', ..] => Some(Kind::Events),
        _ if FIELDS.iter().any(|field| head.starts_with(field)) => Some(Kind::Events),
        _ if FIELDS.iter().any(|field| field.starts_with(head)) => None,
        _ => Some(Kind::Other),
    }
}

fn is_encoded(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_ENCODING)
        .is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
}

/// The endpoints whose answers report token usage.
fn is_model_endpoint(provider: Provider, path: &str) -> bool {
    match provider {
        Provider::Codex => matches!(path, "responses" | "responses/compact"),
        Provider::Claude => path == "v1/messages",
    }
}

/// Splits an event stream into the data of its events, a line at a time.
#[derive(Default)]
pub(super) struct DataLines {
    line: Vec<u8>,
    overflowed: bool,
}

impl DataLines {
    /// Take the next bytes of the stream and pass each complete data line to
    /// `emit`.
    pub(super) fn push(&mut self, mut chunk: &[u8], emit: &mut impl FnMut(&str)) {
        while let Some(end) = chunk.iter().position(|&byte| byte == b'\n') {
            self.extend(&chunk[..end]);
            self.end_line(emit);
            chunk = &chunk[end + 1..];
        }
        self.extend(chunk);
    }

    /// The stream ended: the last line may have no line break.
    pub(super) fn finish(&mut self, emit: &mut impl FnMut(&str)) {
        self.end_line(emit);
    }

    fn extend(&mut self, bytes: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.line.len() + bytes.len() > MAX_LINE_BYTES {
            self.overflowed = true;
            self.line = Vec::new();
        } else {
            self.line.extend_from_slice(bytes);
        }
    }

    fn end_line(&mut self, emit: &mut impl FnMut(&str)) {
        if !self.overflowed
            && let Some(data) = data_field(&self.line)
        {
            emit(data);
        }
        self.overflowed = false;
        self.line.clear();
        if self.line.capacity() > RETAINED_LINE_BYTES {
            self.line = Vec::new();
        }
    }
}

/// The value of a `data:` line, without its line break.
fn data_field(line: &[u8]) -> Option<&str> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let value = line.strip_prefix(b"data:")?;
    let value = value.strip_prefix(b" ").unwrap_or(value);
    std::str::from_utf8(value).ok()
}

/// The end of a body, at least the last [`TAIL_BYTES`] of it.
#[derive(Default)]
pub(super) struct Tail {
    bytes: Vec<u8>,
}

impl Tail {
    pub(super) fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= TAIL_BYTES {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&chunk[chunk.len() - TAIL_BYTES..]);
            return;
        }
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > 2 * TAIL_BYTES {
            let excess = self.bytes.len() - TAIL_BYTES;
            self.bytes.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests;

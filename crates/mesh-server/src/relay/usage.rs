//! The tokens and errors in what a provider sends back, read as the
//! messages stream past.
//!
//! A [`Tally`] takes one JSON message at a time: a server-sent event's data
//! or a WebSocket text frame. It parses a message only when the message
//! carries a `"usage"` member or is an `error` event, so the many small
//! content deltas cost one substring search each.
//!
//! Codex reports its usage in the `response.completed` event, and in the
//! `response.incomplete` and `response.failed` events when they have one.
//! Claude starts a stream with `message_start`, which holds the input tokens,
//! and ends it with `message_delta`, whose `output_tokens` is the total.
//! A response counts once its usage is final. A response that ends early
//! counts for nothing, so the usage is always that of the responses counted.

use mesh_core::Provider;
use mesh_core::api::TokenUsage;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{MAX_ERROR_CHARS, clip};
use crate::db::Completion;

/// What the responses of one request used, summed.
#[derive(Debug)]
pub(super) struct Tally {
    provider: Provider,
    responses: u32,
    usage: TokenUsage,
    started: Option<TokenUsage>,
    error: Option<String>,
}

impl Tally {
    pub(super) fn new(provider: Provider) -> Tally {
        Tally {
            provider,
            responses: 0,
            usage: TokenUsage::default(),
            started: None,
            error: None,
        }
    }

    /// Take one message: the data of a server-sent event or a WebSocket
    /// text frame. A message that is not JSON is ignored.
    pub(super) fn event(&mut self, text: &str) {
        if !text.contains(r#""usage""#) && !text.contains(r#""error""#) {
            return;
        }
        match self.provider {
            Provider::Codex => self.codex_event(text),
            Provider::Claude => self.claude_event(text),
        }
    }

    /// Take the end of a JSON body that is no event stream. Both providers
    /// write the top-level `usage` near the end, so the last `"usage"`
    /// member of the tail is the one.
    pub(super) fn document(&mut self, tail: &[u8]) {
        let usage = match self.provider {
            Provider::Codex => last_usage::<CodexUsage>(tail).map(CodexUsage::tokens),
            Provider::Claude => last_usage::<ClaudeUsage>(tail).map(ClaudeUsage::tokens),
        };
        if let Some(usage) = usage {
            self.add(usage);
        }
    }

    /// What was found, without a duration.
    pub(super) fn completion(&self) -> Completion {
        Completion {
            duration_ms: 0,
            responses: self.responses,
            usage: (self.responses > 0).then_some(self.usage),
            error: self.error.clone(),
        }
    }

    fn codex_event(&mut self, text: &str) {
        let Ok(event) = serde_json::from_str::<CodexEvent>(text) else {
            return;
        };
        match event.kind.as_str() {
            "response.completed" | "response.incomplete" | "response.failed" => {
                let Some(response) = event.response else {
                    return;
                };
                if let Some(usage) = response.usage {
                    self.add(usage.tokens());
                }
                self.note_error(response.error.and_then(ErrorField::into_message));
            }
            "error" => {
                let message = event.error.and_then(ErrorField::into_message);
                self.note_error(message.or(event.message));
            }
            _ => {}
        }
    }

    fn claude_event(&mut self, text: &str) {
        let Ok(event) = serde_json::from_str::<ClaudeEvent>(text) else {
            return;
        };
        match event.kind.as_str() {
            "message_start" => {
                self.started = event
                    .message
                    .and_then(|message| message.usage)
                    .map(ClaudeUsage::tokens);
            }
            "message_delta" => {
                let mut usage = self.started.take().unwrap_or_default();
                if let Some(output) = event.usage.and_then(|usage| usage.output_tokens) {
                    usage.output = output;
                }
                self.add(usage);
            }
            "error" => self.note_error(event.error.and_then(ErrorField::into_message)),
            _ => {}
        }
    }

    fn add(&mut self, usage: TokenUsage) {
        self.responses = self.responses.saturating_add(1);
        let total = &mut self.usage;
        total.input = total.input.saturating_add(usage.input);
        total.cache_read = total.cache_read.saturating_add(usage.cache_read);
        total.cache_write = total.cache_write.saturating_add(usage.cache_write);
        total.output = total.output.saturating_add(usage.output);
        total.reasoning = total.reasoning.saturating_add(usage.reasoning);
    }

    /// Keep the first error of the request.
    fn note_error(&mut self, message: Option<String>) {
        if self.error.is_none() {
            self.error = message
                .map(|message| clip(message.trim(), MAX_ERROR_CHARS))
                .filter(|message| !message.is_empty());
        }
    }
}

/// The message of an error response from its first bytes, which may stop
/// in the middle of the JSON. JSON that has no message, and any other text,
/// is the message itself.
pub(super) fn error_message(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let found = serde_json::from_str::<ErrorBody>(&text)
        .ok()
        .and_then(|body| {
            let message = body.error.and_then(ErrorField::into_message);
            message.or(body.message).or(body.detail)
        });
    let message = found.unwrap_or_else(|| text.clone().into_owned());
    let message = message.trim();
    (!message.is_empty()).then(|| clip(message, MAX_ERROR_CHARS))
}

/// The value of the last `"usage"` member in `tail`, when it parses as `T`.
fn last_usage<T: DeserializeOwned>(tail: &[u8]) -> Option<T> {
    const KEY: &[u8] = br#""usage""#;
    let at = tail.windows(KEY.len()).rposition(|window| window == KEY)?;
    let value = tail[at + KEY.len()..]
        .trim_ascii_start()
        .strip_prefix(b":")?;
    serde_json::Deserializer::from_slice(value)
        .into_iter::<T>()
        .next()?
        .ok()
}

/// An `error` member: an object with a message, or the message itself.
#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorField {
    Detail { message: Option<String> },
    Text(String),
}

impl ErrorField {
    fn into_message(self) -> Option<String> {
        match self {
            ErrorField::Detail { message } => message,
            ErrorField::Text(text) => Some(text),
        }
    }
}

/// The JSON of an error response. OpenAI and Anthropic use `error`; the
/// ChatGPT backend also uses `detail`.
#[derive(Deserialize)]
struct ErrorBody {
    error: Option<ErrorField>,
    message: Option<String>,
    detail: Option<String>,
}

#[derive(Deserialize)]
struct CodexEvent {
    #[serde(rename = "type", default)]
    kind: String,
    response: Option<CodexResponse>,
    error: Option<ErrorField>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct CodexResponse {
    usage: Option<CodexUsage>,
    error: Option<ErrorField>,
}

#[derive(Deserialize)]
struct CodexUsage {
    input_tokens: Option<u64>,
    input_tokens_details: Option<InputDetails>,
    output_tokens: Option<u64>,
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Deserialize)]
struct InputDetails {
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct OutputDetails {
    reasoning_tokens: Option<u64>,
}

impl CodexUsage {
    /// Codex counts the cached tokens in `input_tokens` and the reasoning
    /// tokens in `output_tokens`.
    fn tokens(self) -> TokenUsage {
        TokenUsage {
            input: self.input_tokens.unwrap_or(0),
            cache_read: self
                .input_tokens_details
                .and_then(|details| details.cached_tokens)
                .unwrap_or(0),
            cache_write: 0,
            output: self.output_tokens.unwrap_or(0),
            reasoning: self
                .output_tokens_details
                .and_then(|details| details.reasoning_tokens)
                .unwrap_or(0),
        }
    }
}

#[derive(Deserialize)]
struct ClaudeEvent {
    #[serde(rename = "type", default)]
    kind: String,
    message: Option<ClaudeMessage>,
    usage: Option<ClaudeUsage>,
    error: Option<ErrorField>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl ClaudeUsage {
    /// Claude reports the cached tokens apart from `input_tokens`.
    fn tokens(self) -> TokenUsage {
        let written = self.cache_creation_input_tokens.unwrap_or(0);
        let read = self.cache_read_input_tokens.unwrap_or(0);
        TokenUsage {
            input: self
                .input_tokens
                .unwrap_or(0)
                .saturating_add(written)
                .saturating_add(read),
            cache_read: read,
            cache_write: written,
            output: self.output_tokens.unwrap_or(0),
            reasoning: 0,
        }
    }
}

#[cfg(test)]
mod tests;

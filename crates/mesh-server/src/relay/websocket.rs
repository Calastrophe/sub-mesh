//! Consumer WebSockets, routed by the model of their first message.
//!
//! The handshake does not name a model: Codex sends it in each
//! `response.create` message. So the server accepts the upgrade at once,
//! waits for the client's first message, and reads its `model`, session key
//! and cache lifetime the way it reads an HTTP body. It then picks the
//! account as for HTTP, opens the socket through that account's daemon,
//! sends the first message, and relays both ways.
//!
//! Until the first message has gone upstream, nothing has run, so every
//! failed open may try the next account. Once the upgrade is accepted, a
//! refusal can no longer be an HTTP status: the client gets an `error` event
//! with the status, error and headers of that refusal, as Codex reads them
//! from upstream, and the socket closes. Codex stops on `404`
//! `model_not_found` and retries other statuses, falling back to HTTP when
//! its retries run out.
//!
//! The server logs the socket when it opens and again when it closes, with
//! the tokens of every response that completed on it. It reads them from the
//! upstream text frames that it relays anyway.
//!
//! The upstream handshake headers no longer reach the client. Codex reads
//! three of them: `openai-model` and `x-codex-turn-state`, which upstream
//! does not send on this handshake, and `x-models-etag`, which mesh
//! replaces with its own catalog. Upstream also repeats `x-models-etag` in a
//! `codex.response.metadata` event per response, and the server removes it
//! there so that Codex does not fetch the catalog again after each turn.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::SinkExt;
use mesh_core::Provider;
use mesh_core::api::Transport;
use mesh_core::protocol::{DENIED_HEADER, MODEL_HEADER};
use mesh_relay::{FrameHooks, MeshHeaders, UpstreamSocket};
use serde_json::{Map, Value, json};

use super::http::MODELS_ETAG;
use super::usage::Tally;
use super::{Job, Run, affinity, headroom_hint, refused_for_model, relay_target};
use crate::AppState;
use crate::peer::Peer;
use crate::registry::{Candidate, InFlightGuard};

/// How long the server waits for the client's first message after the
/// upgrade. Codex sends it at once.
const FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest refusal body that is turned into an `error` event.
const REFUSAL_BODY_LIMIT: usize = 1024 * 1024;

/// The start of the upstream event that carries response headers. Upstream
/// writes `type` first. An event written otherwise keeps its
/// `x-models-etag`, which costs Codex only a catalog fetch.
const METADATA_EVENT_START: &str = r#"{"type":"codex.response.metadata""#;

/// Accept the upgrade, then route the socket by its first message.
pub(super) fn relay(
    state: Arc<AppState>,
    provider: Provider,
    peer: Peer,
    session: Option<String>,
    job: &Job<'_>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let method = job.method.clone();
    let headers = job.headers.clone();
    let rest = job.rest.to_owned();
    let query = job.query.map(str::to_owned);
    upgrade.on_upgrade(move |client| async move {
        let job = Job {
            method,
            headers,
            rest: &rest,
            query: query.as_deref(),
        };
        let run = Run::new(&state, provider, peer, session, Transport::Websocket, &job);
        serve(run, &job, client).await;
    })
}

/// Route and relay one accepted socket.
async fn serve(mut run: Run<'_>, job: &Job<'_>, mut client: WebSocket) {
    let Some(first) = first_message(&mut client, FIRST_MESSAGE_TIMEOUT).await else {
        return;
    };
    run.started = Instant::now();
    if let ws::Message::Text(text) = &first {
        let body = Bytes::from(text.clone());
        run.apply(affinity::http_hints(run.provider, &HeaderMap::new(), &body).await);
    }
    let (candidate, mut socket, guard) = match open(&mut run, job).await {
        Ok(opened) => opened,
        Err(refusal) => return refuse(client, *refusal).await,
    };
    if let Err(error) = socket.send(first).await {
        tracing::warn!(node = %candidate.node, %error, "relay first message failed");
        let response = run
            .finish(Some(&candidate), error.into_response(), None)
            .await;
        return refuse(client, response).await;
    }
    let hint = headroom_hint(socket.response_headers());
    let entry = run
        .log(Some(&candidate), StatusCode::SWITCHING_PROTOCOLS, hint)
        .await;
    let tally = Arc::new(Mutex::new(Tally::new(run.provider)));
    socket
        .relay(client, hooks(run.provider, tally.clone()))
        .await;
    if let Some(entry) = entry {
        entry.complete(tally.lock().expect("tally mutex poisoned").completion());
    }
    drop(guard);
}

/// The client's first data message. `None` when the client closes, fails,
/// or sends nothing within `limit`, and then nothing is logged: no request
/// was made.
async fn first_message(client: &mut WebSocket, limit: Duration) -> Option<ws::Message> {
    let read = async {
        while let Some(Ok(message)) = client.recv().await {
            match message {
                ws::Message::Text(_) | ws::Message::Binary(_) => return Some(message),
                ws::Message::Ping(_) | ws::Message::Pong(_) => {}
                ws::Message::Close(_) => return None,
            }
        }
        None
    };
    match tokio::time::timeout(limit, read).await {
        Ok(message) => message,
        Err(_) => {
            tracing::debug!("WebSocket client sent no message in time");
            None
        }
    }
}

/// Open the socket on the first candidate that accepts it.
///
/// No message has gone upstream yet, so any failed open moves on to the
/// next candidate. A daemon that refuses only the model is not rested, and
/// when every candidate refuses that way, the answer is the provider's
/// "model not found". An upstream `429` rests the account and is the answer
/// when no other account serves. Any other refusal is the answer at once.
///
/// The error is the logged response to turn into an `error` event.
async fn open(
    run: &mut Run<'_>,
    job: &Job<'_>,
) -> Result<(Candidate, UpstreamSocket, InFlightGuard), Box<Response>> {
    let candidates = run.candidates("websocket");
    if candidates.is_empty() && run.model_unserved() {
        return Err(Box::new(run.model_not_found().await));
    }
    let total = candidates.len();
    let mut model_refusals = 0;
    let mut limited = None;
    let headers = handshake_headers(job, run.model.as_deref());
    let mut candidates = candidates.into_iter();
    while let Some((candidate, client, guard)) = run.next_attempt(&mut candidates) {
        let target = relay_target(&candidate, job.rest, job.query);
        let open = client.connect_websocket(&target, headers.clone());
        let error = match tokio::time::timeout(Duration::from_secs(30), open).await {
            Ok(Ok(socket)) => return Ok((candidate, socket, guard)),
            Ok(Err(error)) => error,
            Err(_) => {
                run.daemon_failed(&candidate, "WebSocket handshake timed out");
                continue;
            }
        };
        let mesh_relay::Error::Rejected {
            status,
            headers: refused,
            ..
        } = &error
        else {
            run.daemon_failed(&candidate, &error);
            continue;
        };
        if refused_for_model(*status, refused) {
            tracing::info!(account = %candidate.id, "daemon does not allow the model");
            model_refusals += 1;
        } else if *status == StatusCode::TOO_MANY_REQUESTS && refused.contains_key(DENIED_HEADER) {
            run.denied(&candidate);
        } else if *status == StatusCode::TOO_MANY_REQUESTS {
            run.upstream_limited(&candidate, refused);
            limited = Some((candidate, refusal_response(error)));
        } else {
            let response = refusal_response(error);
            return Err(Box::new(run.finish(Some(&candidate), response, None).await));
        }
    }
    if let Some((candidate, response)) = limited {
        return Err(Box::new(run.finish(Some(&candidate), response, None).await));
    }
    if model_refusals > 0 && model_refusals == total {
        return Err(Box::new(run.model_not_found().await));
    }
    Err(Box::new(run.exhausted().await))
}

/// The client's handshake headers, plus the model of its first message for
/// the daemon's allowlist check.
fn handshake_headers(job: &Job<'_>, model: Option<&str>) -> HeaderMap {
    let mut headers = job.headers.clone();
    if let Some(value) = model.and_then(|model| HeaderValue::from_str(model).ok()) {
        headers.insert(HeaderName::from_static(MODEL_HEADER), value);
    }
    headers
}

/// A refused handshake as the response the client would have had, without
/// internal headers.
fn refusal_response(error: mesh_relay::Error) -> Response {
    let mut response = error.into_response();
    *response.headers_mut() =
        mesh_relay::forward_response_headers(response.headers(), MeshHeaders::Strip);
    response
}

/// Send `refusal` to the client as an `error` event and close the socket.
async fn refuse(mut client: WebSocket, refusal: Response) {
    let event = error_event(refusal).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        if client.send(ws::Message::Text(event.into())).await.is_ok() {
            let _ = client.close().await;
        }
    })
    .await;
}

/// An HTTP refusal as the `error` event that Codex reads from upstream:
/// the status, the body's `error` object, and the headers, which carry
/// `retry-after` and the usage limit details.
async fn error_event(refusal: Response) -> String {
    let (parts, body) = refusal.into_parts();
    let body = axum::body::to_bytes(body, REFUSAL_BODY_LIMIT)
        .await
        .unwrap_or_default();
    let error = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|mut body| body.get_mut("error").map(Value::take))
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"message": String::from_utf8_lossy(&body).trim()}));
    let headers: Map<String, Value> = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some((name.as_str().to_owned(), Value::from(value)))
        })
        .collect();
    json!({
        "type": "error",
        "status": parts.status.as_u16(),
        "error": error,
        "headers": headers,
    })
    .to_string()
}

/// The hooks of a socket: each upstream text frame goes to `tally`, and a
/// Codex frame loses its models etag.
fn hooks(provider: Provider, tally: Arc<Mutex<Tally>>) -> FrameHooks {
    FrameHooks::default().on_upstream_text(move |text| {
        tally.lock().expect("tally mutex poisoned").event(text);
        match provider {
            Provider::Codex => without_models_etag(text),
            Provider::Claude => None,
        }
    })
}

/// A `codex.response.metadata` event without its `x-models-etag` header,
/// or `None` to keep the event as it is.
fn without_models_etag(text: &str) -> Option<String> {
    if !text.starts_with(METADATA_EVENT_START) {
        return None;
    }
    let mut event = serde_json::from_str::<Map<String, Value>>(text).ok()?;
    event
        .get_mut("headers")?
        .as_object_mut()?
        .remove(MODELS_ETAG)?;
    Some(Value::Object(event).to_string())
}

#[cfg(test)]
mod tests;

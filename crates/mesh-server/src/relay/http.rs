use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use mesh_core::Provider;
use mesh_core::protocol::DENIED_HEADER;
use mesh_relay::MeshHeaders;

use super::stream::guard_response;
use super::{Job, Run, headroom_hint, refused_for_model, relay_target};
use crate::registry::{Candidate, InFlightGuard};

/// The header on a Codex response that names the model catalog the response
/// was made with. Codex fetches the catalog again when it differs from the
/// `ETag` of the catalog it holds, and mesh makes its own catalog.
pub(super) const MODELS_ETAG: &str = "x-models-etag";

pub(super) async fn read_body(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
    deadline: Duration,
) -> Result<Bytes, (StatusCode, &'static str)> {
    const TOO_LARGE: (StatusCode, &str) =
        (StatusCode::PAYLOAD_TOO_LARGE, "request body too large\n");
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|len| len > limit as u64) {
        return Err(TOO_LARGE);
    }
    tokio::time::timeout(deadline, axum::body::to_bytes(body, limit))
        .await
        .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "request body timed out\n"))?
        .map_err(|error| {
            if error.to_string().contains("length limit") {
                TOO_LARGE
            } else {
                (StatusCode::BAD_REQUEST, "cannot read request body\n")
            }
        })
}

/// Try the candidates in order until one serves the request.
///
/// A candidate is skipped, and its daemon rested, when it has no open
/// tunnel or when [`mesh_relay::Error::never_dispatched`] says the send failed before the request
/// reached the daemon. Any other send failure ends the request with a `502`
/// and is never retried: the request may have begun on that daemon.
///
/// A daemon that refuses only because its account does not allow the model
/// is not rested. When every candidate refuses that way, the answer is the
/// provider's "model not found" `404`.
pub(super) async fn relay(run: &mut Run<'_>, job: &Job<'_>, body: Bytes) -> Response {
    let mut limited = None;
    let candidates = run.candidates("http");
    if candidates.is_empty() && run.model_unserved() {
        return run.model_not_found().await;
    }
    let total = candidates.len();
    let mut model_refusals = 0;
    let mut candidates = candidates.into_iter();
    let mut headers = job.headers.clone();
    if run.provider == Provider::Claude {
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    while let Some((candidate, client, guard)) = run.next_attempt(&mut candidates) {
        let target = relay_target(&candidate, job.rest, job.query);
        let send = client.send(job.method.clone(), &target, headers.clone(), body.clone());
        let Ok(sent) = tokio::time::timeout(Duration::from_secs(120), send).await else {
            let response =
                (StatusCode::GATEWAY_TIMEOUT, "upstream response timed out\n").into_response();
            return run.finish(Some(&candidate), response, None).await;
        };
        match sent {
            Ok(response) if refused_for_model(response.status(), response.headers()) => {
                tracing::info!(account = %candidate.id, "daemon does not allow the model");
                model_refusals += 1;
            }
            Ok(response)
                if response.status() == StatusCode::TOO_MANY_REQUESTS
                    && response.headers().contains_key(DENIED_HEADER) =>
            {
                run.denied(&candidate);
            }
            Ok(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
                run.upstream_limited(&candidate, response.headers());
                limited = Some((candidate, response, guard));
            }
            Ok(response) => {
                return finish(run, &candidate, response, guard).await;
            }
            Err(error) if error.never_dispatched() => {
                run.daemon_failed(&candidate, &error);
            }
            Err(error) => {
                tracing::warn!(node = %candidate.node, %error, "relay request failed");
                return run
                    .finish(Some(&candidate), error.into_response(), None)
                    .await;
            }
        }
    }
    if let Some((candidate, response, guard)) = limited {
        return finish(run, &candidate, response, guard).await;
    }
    if model_refusals > 0 && model_refusals == total {
        return run.model_not_found().await;
    }
    run.exhausted().await
}

async fn finish(
    run: &Run<'_>,
    candidate: &Candidate,
    response: Response,
    guard: InFlightGuard,
) -> Response {
    let hint = if response.status() == StatusCode::TOO_MANY_REQUESTS {
        None
    } else {
        headroom_hint(response.headers())
    };
    let response = run.finish(Some(candidate), response, hint).await;
    let mut response = mesh_relay::forward_response(response, MeshHeaders::Strip, HeaderMap::new());
    if run.provider == Provider::Codex {
        response.headers_mut().remove(MODELS_ETAG);
    }
    guard_response(response, guard)
}

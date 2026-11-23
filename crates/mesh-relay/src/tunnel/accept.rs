use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::header::{CONNECTION, CONTENT_TYPE, UPGRADE};
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use hyper::upgrade::OnUpgrade;
use mesh_core::protocol::{NODE_HEADER, TUNNEL_PROTOCOL, valid_node_name};

use crate::Error;
use crate::headers;

/// A valid tunnel upgrade request, ready to be answered.
///
/// The server sends `response` (the `101 Switching Protocols` answer) back
/// from its handler and awaits `upgrade` in a spawned task, usually with
/// [`crate::tunnel::Client::from_upgrade`]. `upgrade` resolves only
/// after `response` has been sent.
#[derive(Debug)]
#[non_exhaustive]
pub struct Accept {
    /// The daemon's node name from [`NODE_HEADER`].
    pub node: String,
    /// The `101` response to return from the handler.
    pub response: Response,
    /// The upgraded connection, once `response` is sent.
    pub upgrade: OnUpgrade,
}

/// Validate a tunnel upgrade on an upgrade-capable server connection.
/// The caller must verify the daemon's identity before sending the response.
///
/// A refusal is [`Error::Rejected`], which renders as the response to
/// send: `405` for a wrong method, `426` for a missing or wrong upgrade
/// token, `400` for a missing or invalid node name (see
/// [`valid_node_name`]), and `500` when the connection cannot be upgraded.
#[allow(clippy::result_large_err)]
pub fn accept<B>(mut request: Request<B>) -> Result<Accept, Error> {
    if request.method() != Method::GET {
        return Err(refuse(
            StatusCode::METHOD_NOT_ALLOWED,
            "tunnel upgrade must be GET",
        ));
    }
    let headers = request.headers();
    if !headers::has_token(headers, CONNECTION, "upgrade")
        || !headers::has_token(headers, UPGRADE, TUNNEL_PROTOCOL)
    {
        return Err(refuse(
            StatusCode::UPGRADE_REQUIRED,
            "tunnel requires Upgrade: mesh-tunnel/1",
        ));
    }
    let node = headers
        .get(NODE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|node| !node.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| refuse(StatusCode::BAD_REQUEST, "missing x-mesh-node header"))?;
    if !valid_node_name(&node) {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "invalid x-mesh-node header",
        ));
    }
    let upgrade = request
        .extensions_mut()
        .remove::<OnUpgrade>()
        .ok_or_else(|| {
            refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "connection cannot be upgraded",
            )
        })?;

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("upgrade"));
    response
        .headers_mut()
        .insert(UPGRADE, HeaderValue::from_static(TUNNEL_PROTOCOL));
    Ok(Accept {
        node,
        response,
        upgrade,
    })
}

fn refuse(status: StatusCode, message: &'static str) -> Error {
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Error::Rejected {
        status,
        headers,
        body: Bytes::from(format!("{message}\n")),
    }
}

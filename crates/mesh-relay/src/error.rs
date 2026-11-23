use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::response::Parts;
use http::{HeaderMap, StatusCode};

use crate::headers::{self, MeshHeaders};
use crate::http::response_with_body;

/// Errors while relaying.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("upstream request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    /// A WebSocket handshake or tunnel upgrade was refused.
    #[error("upstream refused the WebSocket handshake with {status}")]
    Rejected {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
    },
    /// The tunnel connection failed or is closed.
    #[error("tunnel error: {0}")]
    Tunnel(#[from] hyper::Error),
    /// Network I/O failed, for example while dialing the server.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Whether the request certainly never reached the peer, so sending it
    /// again cannot run it twice.
    ///
    /// True only when hyper cancelled the request before it reached the
    /// HTTP/2 connection. Any other failure, including one whose dispatch is
    /// unclear, is false.
    #[must_use]
    pub fn never_dispatched(&self) -> bool {
        matches!(self, Self::Tunnel(error) if error.is_canceled())
    }

    pub(crate) fn from_refusal(parts: Parts, body: Bytes) -> Self {
        Self::Rejected {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }
}

impl IntoResponse for Error {
    /// `Rejected` keeps the upstream status, headers, and body; everything
    /// else becomes `502 Bad Gateway` with a short text body.
    fn into_response(self) -> Response {
        match self {
            Error::Rejected {
                status,
                headers,
                body,
            } => response_with_body(
                status,
                headers::forward_response_headers(&headers, MeshHeaders::Keep),
                Body::from(body),
            ),
            other => (StatusCode::BAD_GATEWAY, other.to_string()).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_cancelled_tunnel_send_never_dispatched() {
        assert!(!Error::Other("bad path".to_owned()).never_dispatched());
        assert!(!Error::Io(std::io::Error::other("reset")).never_dispatched());
        assert!(!Error::WebSocket("x".to_owned()).never_dispatched());
    }
}

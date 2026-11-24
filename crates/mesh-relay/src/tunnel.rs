//! Reverse tunnel between `mesh-server` and `mesh-daemon`.
//!
//! The daemon dials out over HTTP/1.1, in plain text or TLS ([`Dial`]).
//! The upgrade is an ordinary HTTP/1.1 `Upgrade`, so a reverse proxy that
//! passes WebSockets through carries it too. Both peers verify identity
//! before completing the upgrade ([`accept`]). The server then
//! becomes the HTTP/2 client ([`Client`]) and the daemon serves its
//! router ([`serve`]). WebSockets use extended `CONNECT` (RFC 8441).
//!
//! See [`mesh_core::protocol`] for the wire protocol.

mod accept;
mod client;
mod dial;
mod serve;

#[cfg(test)]
mod tests;

use std::time::Duration;

pub use accept::{Accept, accept};
pub use client::Client;
pub use dial::{Dial, ServerIo, dial};
pub use serve::{serve, with_connect_info};

/// Interval between HTTP/2 keepalive pings on both ends of a tunnel.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// How long a tunnel end waits for a keepalive ping to be answered before it
/// closes the tunnel.
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Time limit for each dial step: the TCP connect and the upgrade exchange.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Synthetic `:authority` of every request on a tunnel. HTTP/2 requires an
/// authority and a scheme, so tunnel requests use `http://tunnel/...`.
pub const TUNNEL_AUTHORITY: &str = "tunnel";

const REJECTED_BODY_LIMIT: usize = 1024 * 1024;

async fn rejected(response: http::Response<hyper::body::Incoming>) -> crate::Error {
    use http_body_util::{BodyExt, Limited};

    let (parts, body) = response.into_parts();
    match Limited::new(body, REJECTED_BODY_LIMIT).collect().await {
        Ok(body) => crate::Error::from_refusal(parts, body.to_bytes()),
        Err(err) => crate::Error::Other(format!("cannot read refusal body: {err}")),
    }
}

//! Paths and headers shared by `mesh-server` and `mesh-daemon`.
//!
//! The daemon checks the server's tailnet identity, then sends `GET`
//! [`TUNNEL_PATH`] with `Connection: upgrade`, `Upgrade:` [`TUNNEL_PROTOCOL`],
//! and [`NODE_HEADER`]. The server checks the caller against its `[daemons]`
//! policy using tailnet whois before returning `101 Switching Protocols`.
//! After upgrade, the server becomes the HTTP/2 client and sends each relay
//! or control call on its own stream. WebSockets use extended `CONNECT`
//! (RFC 8441). The daemon sends reports separately and redials with backoff
//! after a disconnect. The server considers it online while the tunnel is up.
//!
//! The server routes `/<provider>/<upstream path>` to
//! `/relay/<account id>/<upstream path>` on the selected daemon. The daemon
//! replaces auth headers and streams the upstream response back, with
//! [`HEADROOM_HEADER`] updating the server between reports. A daemon refusal
//! (`429` with [`DENIED_HEADER`]) lets the server try the next account. A
//! refusal with the value [`DENIED_MODEL`] means only that the account does
//! not allow the requested model, so the server tries the next account
//! without a cooldown.
//!
//! A consumer WebSocket is routed by the `model` of its first client
//! message. The server names that model in [`MODEL_HEADER`] on its `CONNECT`
//! to the daemon, so that the daemon can refuse it the same way before it
//! dials upstream. The daemon checks the `model` of every later
//! `response.create` message itself.
//! The server strips internal `x-mesh-*` headers from consumer requests and
//! responses. The daemon keeps these headers out of upstream requests.

use std::fmt;
use std::str::FromStr;

use crate::Admission;

/// Server endpoint where daemons `POST` a JSON [`crate::DaemonReport`].
pub const REPORT_PATH: &str = "/mesh/v1/report";

/// Server endpoint where a daemon opens its tunnel (see the module docs).
pub const TUNNEL_PATH: &str = "/mesh/v1/tunnel";

/// `Upgrade` token for the HTTP/2 tunnel.
pub const TUNNEL_PROTOCOL: &str = "mesh-tunnel/1";

/// Request header on the tunnel upgrade naming the daemon's node. It must
/// match the `node` of the daemon's reports.
pub const NODE_HEADER: &str = "x-mesh-node";

/// Prefix of the daemon relay route: `/relay/{account_id}/{*path}`.
pub const RELAY_PREFIX: &str = "/relay";

/// Prefix of every internal mesh header.
pub const HEADER_PREFIX: &str = "x-mesh-";

/// Response header from the daemon with the account's headroom after the
/// request, as a decimal fraction, or `deny` when it no longer admits.
pub const HEADROOM_HEADER: &str = "x-mesh-headroom";

/// Response header on a daemon `429` refusal. The value is the
/// [`crate::DenyReason`] kind, for example `reserve`.
pub const DENIED_HEADER: &str = "x-mesh-denied";

/// [`DENIED_HEADER`] value when the account's model allowlist does not allow
/// the requested model. The account itself still admits traffic.
pub const DENIED_MODEL: &str = "model";

/// Request header on a WebSocket relay naming the model of the consumer's
/// first message, which the server replays upstream once the socket is open.
pub const MODEL_HEADER: &str = "x-mesh-model";

/// Default port of the server's consumer and report API.
pub const DEFAULT_SERVER_PORT: u16 = 7070;

/// Default port of the daemon's local admin API (loopback only).
pub const DEFAULT_ADMIN_PORT: u16 = 7072;

/// Prefix of the daemon control API, served only on the tunnel, so only the
/// server the daemon dialed can call it. The server uses it to apply changes the
/// account owner makes in the dashboard.
///
/// | Method   | Path                                     | Body                 |
/// |----------|------------------------------------------|----------------------|
/// | `PUT`    | `/control/v1/accounts/{id}/reserve`      | [`crate::api::SetReserve`] |
/// | `PUT`    | `/control/v1/accounts/{id}/models`       | [`crate::api::SetModels`]  |
/// | `POST`   | `/control/v1/accounts/{id}/pause`        |                      |
/// | `POST`   | `/control/v1/accounts/{id}/resume`       |                      |
/// | `DELETE` | `/control/v1/accounts/{id}`              |                      |
///
/// Each call carries [`ON_BEHALF_HEADER`]. Success returns the account's new
/// [`crate::AccountReport`], or `204 No Content` for `DELETE`. Errors return
/// [`crate::ApiError`].
pub const CONTROL_PREFIX: &str = "/control/v1";

/// Request header on control calls naming the tailnet login the server acts
/// for. The daemon refuses the call unless it is the daemon machine's owner
/// (or listed in its `owners` config).
pub const ON_BEHALF_HEADER: &str = "x-mesh-on-behalf-of";

/// The daemon's view of an account after a request, as carried in
/// [`HEADROOM_HEADER`]: the headroom with four decimals, or `deny`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HeadroomHint {
    /// The account no longer admits traffic.
    Deny,
    /// The room left before the reserve, from 0.0 to 1.0.
    Value(f64),
}

impl From<&Admission> for HeadroomHint {
    fn from(admission: &Admission) -> Self {
        admission.headroom().map_or(Self::Deny, Self::Value)
    }
}

impl fmt::Display for HeadroomHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deny => f.write_str("deny"),
            Self::Value(headroom) => write!(f, "{headroom:.4}"),
        }
    }
}

/// Error returned when a header value is not a [`HeadroomHint`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected a finite headroom or \"deny\"")]
pub struct ParseHeadroomError;

impl FromStr for HeadroomHint {
    type Err = ParseHeadroomError;

    /// Reads `deny` in any case, or a finite number clamped to `0.0..=1.0`.
    /// Surrounding whitespace is ignored.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("deny") {
            return Ok(Self::Deny);
        }
        s.parse::<f64>()
            .ok()
            .filter(|headroom| headroom.is_finite())
            .map(|headroom| Self::Value(headroom.clamp(0.0, 1.0)))
            .ok_or(ParseHeadroomError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DenyReason;

    #[test]
    fn formats_headroom() {
        assert_eq!(HeadroomHint::Value(0.421).to_string(), "0.4210");
        assert_eq!(HeadroomHint::Value(1.0).to_string(), "1.0000");
        assert_eq!(HeadroomHint::Deny.to_string(), "deny");
        assert_eq!(
            HeadroomHint::from(&Admission::Admit { headroom: 0.5 }).to_string(),
            "0.5000"
        );
        let paused = Admission::Deny {
            reason: DenyReason::Paused,
        };
        assert_eq!(HeadroomHint::from(&paused), HeadroomHint::Deny);
    }

    #[test]
    fn parses_headroom() {
        assert_eq!("deny".parse(), Ok(HeadroomHint::Deny));
        assert_eq!(" DENY ".parse(), Ok(HeadroomHint::Deny));
        assert_eq!(" 0.25 ".parse(), Ok(HeadroomHint::Value(0.25)));
        assert_eq!("7".parse(), Ok(HeadroomHint::Value(1.0)));
        assert_eq!("-1".parse(), Ok(HeadroomHint::Value(0.0)));
        assert_eq!("nan".parse::<HeadroomHint>(), Err(ParseHeadroomError));
        assert_eq!("inf".parse::<HeadroomHint>(), Err(ParseHeadroomError));
        assert_eq!("soon".parse::<HeadroomHint>(), Err(ParseHeadroomError));
    }
}

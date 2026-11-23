//! Request passthrough for mesh.
//!
//! The server relays consumer requests to a daemon, which relays them
//! upstream. Request bodies are buffered for retries. HTTP response bodies
//! are streamed as they arrive.
//!
//! The server reaches each daemon over a reverse [`tunnel`] that the daemon
//! dials.

mod error;
mod headers;
mod http;
pub mod shutdown;
pub mod tailscale;
pub mod tunnel;
mod websocket;

pub use error::Error;
pub use headers::{MeshHeaders, forward_request_headers, forward_response_headers};
pub use http::{forward_response, into_response, send};
pub use websocket::{
    ClientText, FrameHooks, UpstreamSocket, is_websocket_request, is_websocket_upgrade,
};

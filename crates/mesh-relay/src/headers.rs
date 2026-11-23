use std::collections::HashSet;

use http::header::{CONNECTION, CONTENT_LENGTH, HOST};
use http::{HeaderMap, HeaderName};
use mesh_core::protocol::HEADER_PREFIX;

/// Whether to keep internal `x-mesh-*` headers when copying headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshHeaders {
    Keep,
    Strip,
}

/// Copy request headers for forwarding: drops hop-by-hop headers
/// (`connection`, `keep-alive`, `te`, `trailer`, `transfer-encoding`,
/// `upgrade`, `proxy-*`, and any named in `connection`), `host`, and
/// `content-length`. Keeps the remaining values in map iteration order, including
/// `user-agent`, `accept-encoding`, and provider headers. Drops `x-mesh-*`
/// when `mesh` is [`MeshHeaders::Strip`].
pub fn forward_request_headers(headers: &HeaderMap, mesh: MeshHeaders) -> HeaderMap {
    copy_headers(headers, mesh, |name| name != HOST && name != CONTENT_LENGTH)
}

/// Copy response headers for forwarding: drops hop-by-hop headers and
/// `content-length` (the body is streamed), and `x-mesh-*` when `mesh` is
/// [`MeshHeaders::Strip`].
pub fn forward_response_headers(headers: &HeaderMap, mesh: MeshHeaders) -> HeaderMap {
    copy_headers(headers, mesh, |name| name != CONTENT_LENGTH)
}

/// Hop-by-hop headers (RFC 9110 section 7.6.1), plus `proxy-*`.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection" | "keep-alive" | "te" | "trailer" | "transfer-encoding" | "upgrade"
    ) || name.as_str().starts_with("proxy-")
}

/// Header names listed in the `connection` header.
fn connection_listed(headers: &HeaderMap) -> HashSet<HeaderName> {
    comma_separated(headers, CONNECTION)
        .filter_map(|token| HeaderName::from_bytes(token).ok())
        .collect()
}

fn comma_separated(headers: &HeaderMap, name: HeaderName) -> impl Iterator<Item = &[u8]> {
    headers
        .get_all(name)
        .into_iter()
        .flat_map(|value| value.as_bytes().split(|&byte| byte == b','))
        .map(<[u8]>::trim_ascii)
}

pub(super) fn has_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    comma_separated(headers, name).any(|value| value.eq_ignore_ascii_case(token.as_bytes()))
}

pub(super) fn copy_headers(
    headers: &HeaderMap,
    mesh: MeshHeaders,
    keep: impl Fn(&HeaderName) -> bool,
) -> HeaderMap {
    let listed = connection_listed(headers);
    let mut out = HeaderMap::with_capacity(headers.keys_len());
    for (name, value) in headers {
        let drop = is_hop_by_hop(name)
            || !keep(name)
            || listed.contains(name)
            || (mesh == MeshHeaders::Strip && name.as_str().starts_with(HEADER_PREFIX));
        if !drop {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

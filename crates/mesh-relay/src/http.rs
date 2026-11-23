use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::header::CONTENT_LENGTH;
use http::{HeaderMap, Method, StatusCode};
use url::Url;

use crate::Error;
use crate::headers::{self, MeshHeaders};

/// Send one buffered request with caller-prepared headers.
///
/// For passthrough, configure `client` without default headers, a user
/// agent, redirects, or a total timeout. Disable automatic decompression
/// to preserve content encoding. Reqwest supplies protocol headers such
/// as the request body's content length.
#[allow(clippy::result_large_err)]
pub async fn send(
    client: &reqwest::Client,
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Bytes,
) -> Result<reqwest::Response, Error> {
    let bodiless = body.is_empty() && matches!(method, Method::GET | Method::HEAD);
    let mut request = client.request(method, url).headers(headers);
    if !bodiless {
        request = request.body(body);
    }
    request.send().await.map_err(Error::from)
}

/// Relay a reqwest response using [`forward_response`]'s header rules.
/// Streams the body as data chunks.
pub fn into_response(upstream: reqwest::Response, mesh: MeshHeaders, extra: HeaderMap) -> Response {
    let status = upstream.status();
    let headers = response_headers(upstream.headers(), mesh, extra);
    response_with_body(status, headers, Body::from_stream(upstream.bytes_stream()))
}

/// Relay a tunnel response with its status and streaming body. Apply
/// [`crate::forward_response_headers`] and replace matching headers with
/// `extra`. Relay the data chunks. Start with fresh HTTP version and extension fields.
pub fn forward_response(upstream: Response, mesh: MeshHeaders, extra: HeaderMap) -> Response {
    let (parts, body) = upstream.into_parts();
    response_with_body(
        parts.status,
        response_headers(&parts.headers, mesh, extra),
        Body::from_stream(body.into_data_stream()),
    )
}

pub(super) fn response_with_body(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn response_headers(upstream: &HeaderMap, mesh: MeshHeaders, extra: HeaderMap) -> HeaderMap {
    let mut headers = headers::copy_headers(upstream, mesh, |name| {
        name != CONTENT_LENGTH && !extra.contains_key(name)
    });
    headers.extend(extra);
    headers
}

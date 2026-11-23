//! Protection against requests that another website makes through the
//! user's browser.
//!
//! The server knows callers by their tailnet address, which a browser
//! attaches to every request, including one that a page on another website
//! makes. A form post or `fetch` with `no-cors` can reach `POST .../pause`
//! without a preflight. Browsers name the page's origin on such requests, so
//! the server refuses changes whose origin is not the server itself.
//! Callers that are not browsers send no origin and are not affected.

use axum::extract::Request;
use axum::http::{HeaderMap, Method, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::failure::ApiFailure;

/// Middleware: refuse changes that a page of another origin made.
pub async fn same_origin_changes(request: Request, next: Next) -> Response {
    if is_cross_origin_change(request.method(), request.uri(), request.headers()) {
        return ApiFailure::forbidden("cross-origin changes are refused").into_response();
    }
    next.run(request).await
}

/// Whether the request changes state and a browser says that it did not
/// come from a page of this server.
fn is_cross_origin_change(method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return false;
    }
    let mut fetch_sites = headers.get_all("sec-fetch-site").iter();
    let fetch_site = match (fetch_sites.next(), fetch_sites.next()) {
        (None, None) => None,
        (Some(value), None) if matches!(value.as_bytes(), b"same-origin" | b"none") => {
            Some(value.as_bytes())
        }
        _ => return true,
    };
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return false;
    };
    if origins.next().is_some() {
        return true;
    }
    let Some((scheme, origin_host)) = origin.to_str().ok().and_then(|s| s.split_once("://")) else {
        return true;
    };
    if scheme != "http" && !(scheme == "https" && fetch_site == Some(b"same-origin")) {
        return true;
    }
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = match (hosts.next(), hosts.next()) {
        (None, None) => uri.authority().map(|host| host.as_str()),
        (Some(value), None) => value.to_str().ok(),
        _ => return true,
    };
    !host.is_some_and(|host| !host.is_empty() && origin_host.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn blocked(method: &Method, headers: &HeaderMap) -> bool {
        is_cross_origin_change(method, &Uri::from_static("/api/v1"), headers)
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn requests_that_only_read_are_never_refused() {
        let hostile = headers(&[("origin", "http://evil.example"), ("host", "mesh:7070")]);
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!blocked(&method, &hostile));
        }
    }

    #[test]
    fn clients_that_are_not_browsers_send_no_origin() {
        let plain = headers(&[("host", "mesh:7070")]);
        for method in [Method::POST, Method::PUT, Method::DELETE] {
            assert!(!blocked(&method, &plain));
        }
    }

    #[test]
    fn the_dashboard_may_change_things() {
        let same = headers(&[
            ("origin", "http://mesh:7070"),
            ("host", "mesh:7070"),
            ("sec-fetch-site", "same-origin"),
        ]);
        assert!(!blocked(&Method::POST, &same));
        let old = headers(&[("origin", "http://Mesh.example"), ("host", "mesh.example")]);
        assert!(!blocked(&Method::DELETE, &old));
        let typed = headers(&[("sec-fetch-site", "none"), ("host", "mesh:7070")]);
        assert!(!blocked(&Method::POST, &typed));
    }

    #[test]
    fn ambiguous_origin_headers_are_refused() {
        let mut origin = headers(&[("origin", "http://mesh"), ("host", "mesh")]);
        origin.append("origin", HeaderValue::from_static("http://elsewhere"));
        assert!(blocked(&Method::POST, &origin));

        let mut fetch = headers(&[("sec-fetch-site", "same-origin")]);
        fetch.append("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(blocked(&Method::POST, &fetch));

        let mut malformed = HeaderMap::new();
        malformed.insert("sec-fetch-site", HeaderValue::from_bytes(&[0xff]).unwrap());
        assert!(blocked(&Method::POST, &malformed));
    }

    #[test]
    fn another_website_may_not() {
        let evil = headers(&[("origin", "http://evil.example"), ("host", "mesh:7070")]);
        assert!(blocked(&Method::POST, &evil));
        let other_port = headers(&[("origin", "http://mesh:8080"), ("host", "mesh:7070")]);
        assert!(blocked(&Method::PUT, &other_port));
        for origin in ["null", "garbage", "https://mesh:7070", "ftp://mesh:7070"] {
            let odd = headers(&[("origin", origin), ("host", "mesh:7070")]);
            assert!(blocked(&Method::POST, &odd), "{origin}");
        }
        for site in ["cross-site", "same-site"] {
            let fetch = headers(&[("sec-fetch-site", site), ("host", "mesh:7070")]);
            assert!(blocked(&Method::POST, &fetch), "{site}");
        }
        let no_host = headers(&[("origin", "http://mesh:7070")]);
        assert!(blocked(&Method::POST, &no_host));
    }
}

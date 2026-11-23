use http::header::AUTHORIZATION;
use http::{HeaderMap, HeaderValue};

/// Clear consumer credentials and install a bearer token that is redacted in header logs.
pub(crate) fn replace_bearer(headers: &mut HeaderMap, access_token: &str) {
    headers.remove(AUTHORIZATION);
    headers.remove("x-api-key");
    match HeaderValue::from_str(&format!("Bearer {access_token}")) {
        Ok(mut value) => {
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        Err(_) => {
            tracing::warn!("access token is not a valid header value; sending no credentials")
        }
    }
}

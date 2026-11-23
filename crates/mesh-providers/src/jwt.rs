//! Reading claims from a JWT payload. The signature is not checked: the
//! tokens come straight from the provider's token endpoint over TLS.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde_json::Value;

/// Decode the payload of `header.payload.signature` as JSON.
pub(crate) fn payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let (_header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The `exp` claim of `token`, when it is a JWT that has one.
pub(crate) fn expiry(token: &str) -> Option<DateTime<Utc>> {
    let exp = payload(token)?.get("exp")?.as_i64()?;
    DateTime::from_timestamp(exp, 0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build an unsigned JWT with `claims` as its payload.
    pub(crate) fn make_jwt(claims: &Value) -> String {
        let encode = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
        format!(
            "{}.{}.sig",
            encode(&serde_json::json!({"alg": "none"})),
            encode(claims)
        )
    }

    #[test]
    fn decodes_payload_and_expiry() {
        let token = make_jwt(&serde_json::json!({"exp": 1_700_000_000, "email": "a@b.c"}));
        assert_eq!(payload(&token).unwrap()["email"], "a@b.c");
        assert_eq!(expiry(&token), DateTime::from_timestamp(1_700_000_000, 0));
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert!(payload("not-a-jwt").is_none());
        assert!(payload("a.b").is_none());
        assert!(payload("a.b.c.d").is_none());
        assert!(payload("a.!!!.c").is_none());
        assert!(expiry(&make_jwt(&serde_json::json!({"no": "exp"}))).is_none());
    }
}

//! Claude (Pro/Max subscription): OAuth settings, auth headers, rate-limit
//! headers, and the `api/oauth/usage` response.

use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};
use mesh_core::Window;
use serde_json::Value;

use crate::oauth::{Encoding, Identity, OAuthApp, TokenResponse};
use crate::{Credentials, usage};

const SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

pub(crate) const APP: OAuthApp = OAuthApp {
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    scope: SCOPES,
    refresh_scope: Some(SCOPES),
    callback_path: "/callback",
    extra_authorize_params: &[("code", "true")],
    exchange_encoding: Encoding::Json,
    exchange_sends_state: true,
};

const BETA_HEADER: HeaderName = HeaderName::from_static("anthropic-beta");

/// Anthropic only accepts an OAuth bearer token on requests that carry this beta.
pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Keep the consumer's betas and add the beta required for OAuth.
pub(crate) fn apply_auth(credentials: &Credentials, headers: &mut HeaderMap) {
    crate::auth::replace_bearer(headers, &credentials.access_token);

    let mut betas: Vec<&str> = headers
        .get_all(BETA_HEADER)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|beta| !beta.is_empty())
        .collect();
    if !betas.contains(&OAUTH_BETA) {
        betas.push(OAUTH_BETA);
        if let Ok(value) = HeaderValue::from_str(&betas.join(",")) {
            headers.insert(BETA_HEADER, value);
        }
    }
}

/// The account email from the token response.
pub(crate) fn identity(tokens: &TokenResponse) -> Identity {
    Identity {
        chatgpt_account_id: None,
        email: tokens
            .account
            .as_ref()
            .and_then(|account| account.email_address.clone())
            .filter(|email| !email.is_empty()),
    }
}

const HEADER_PREFIX: &str = "anthropic-ratelimit-unified-";

const WEEK_MINUTES: u32 = 7 * 24 * 60;

/// The two account-wide windows: header key, window id, and length in minutes.
/// Per-model windows (`7d_<family>`) are ignored.
const WINDOWS: [(&str, &str, u32); 2] = [
    ("5h", "five_hour", 5 * 60),
    ("7d", "seven_day", WEEK_MINUTES),
];

/// Read `anthropic-ratelimit-unified-{5h,7d}-{utilization,reset,status}`.
/// Utilization is a fraction and reset an epoch in seconds.
pub(crate) fn parse_rate_limit_headers(headers: &HeaderMap) -> Vec<Window> {
    let header = |name: String| headers.get(name)?.to_str().ok().map(str::trim);

    let mut windows = Vec::new();
    for (key, id, minutes) in WINDOWS {
        let status_limited =
            header(format!("{HEADER_PREFIX}{key}-status")).is_some_and(is_rejected);
        let utilization = header(format!("{HEADER_PREFIX}{key}-utilization"))
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite());
        if utilization.is_none() && !status_limited {
            continue;
        }
        let used = utilization.unwrap_or(1.0).clamp(0.0, 1.0);
        let resets_at = header(format!("{HEADER_PREFIX}{key}-reset")).and_then(parse_time);
        windows.push(Window {
            id: id.to_owned(),
            duration_minutes: Some(minutes),
            used,
            resets_at,
            limited: status_limited || used >= 1.0,
        });
    }

    let overall_rejected = header(format!("{HEADER_PREFIX}status")).is_some_and(is_rejected);
    if overall_rejected && !windows.iter().any(|window| window.limited) {
        let claim = header(format!("{HEADER_PREFIX}representative-claim"));
        let named = windows
            .iter()
            .any(|window| Some(window.id.as_str()) == claim);
        for window in &mut windows {
            window.limited = !named || Some(window.id.as_str()) == claim;
        }
    }
    windows
}

fn is_rejected(status: &str) -> bool {
    status.eq_ignore_ascii_case("rejected") || status.eq_ignore_ascii_case("limited")
}

/// Epoch seconds, or an RFC 3339 time.
fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    match value.parse::<i64>() {
        Ok(seconds) => DateTime::from_timestamp(seconds, 0),
        Err(_) => DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|time| time.with_timezone(&Utc)),
    }
}

/// Read the usage body: `five_hour` and `seven_day` are each `null` or
/// `{"utilization": <percent 0-100>, "resets_at": <ISO-8601 or null>}`.
/// Per-model windows such as `seven_day_opus` are ignored.
pub(crate) fn parse_usage(body: &Value) -> Vec<Window> {
    WINDOWS
        .iter()
        .filter_map(|(_, id, minutes)| {
            let window = body.get(*id)?;
            let utilization = window
                .get("utilization")?
                .as_f64()
                .filter(|value| value.is_finite())?;
            let resets_at = window
                .get("resets_at")
                .and_then(Value::as_str)
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                .map(|time| time.with_timezone(&Utc));
            Some(usage::window_from_percent(
                (*id).to_owned(),
                Some(*minutes),
                utilization,
                resets_at,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_headers as headers;
    use serde_json::json;

    #[test]
    fn parses_unified_headers() {
        let windows = parse_rate_limit_headers(&headers(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.25"),
            ("anthropic-ratelimit-unified-5h-reset", "1700001000"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.9"),
            ("anthropic-ratelimit-unified-7d-reset", "1700500000"),
            ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-7d_opus-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_opus-utilization", "1.0"),
            ("anthropic-ratelimit-unified-7d_sonnet-utilization", "0.5"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.0"),
        ]));
        let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["five_hour", "seven_day"]);
        assert_eq!(windows[0].duration_minutes, Some(300));
        assert_eq!(windows[0].used, 0.25);
        assert_eq!(
            windows[0].resets_at,
            DateTime::from_timestamp(1_700_001_000, 0)
        );
        assert!(!windows[0].limited);
        assert_eq!(windows[1].duration_minutes, Some(10080));
        assert!(
            !windows[1].limited,
            "allowed_warning and per-model rejection are not a limit"
        );
    }

    #[test]
    fn overall_rejection_blames_the_claimed_window() {
        let windows = parse_rate_limit_headers(&headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "seven_day",
            ),
            ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.95"),
        ]));
        assert!(!windows[0].limited);
        assert!(windows[1].limited);

        let unnamed = parse_rate_limit_headers(&headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
        ]));
        assert!(unnamed[0].limited);
    }

    #[test]
    fn no_headers_no_windows() {
        assert!(parse_rate_limit_headers(&HeaderMap::new()).is_empty());
        let only_overall = headers(&[("anthropic-ratelimit-unified-status", "rejected")]);
        assert!(parse_rate_limit_headers(&only_overall).is_empty());
    }

    #[test]
    fn parses_the_usage_body() {
        let body = json!({
            "five_hour": {"utilization": 37.0, "resets_at": "2025-11-20T05:00:00.123456+00:00"},
            "seven_day": {"utilization": 100, "resets_at": null},
            "seven_day_opus": {"utilization": 99.0, "resets_at": null},
            "seven_day_sonnet": {"utilization": 12.5, "resets_at": "2025-11-24T10:00:00Z"},
            "extra_usage": {"utilization": 3.0}
        });
        let windows = parse_usage(&body);
        let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["five_hour", "seven_day"]);
        assert!((windows[0].used - 0.37).abs() < 1e-9);
        assert_eq!(windows[0].duration_minutes, Some(300));
        assert_eq!(
            windows[0].resets_at,
            DateTime::parse_from_rfc3339("2025-11-20T05:00:00.123456Z")
                .ok()
                .map(|t| t.with_timezone(&Utc))
        );
        assert!(windows[1].limited);
        assert_eq!(windows[1].resets_at, None);
        assert_eq!(windows[1].duration_minutes, Some(10080));
        assert!(parse_usage(&json!({})).is_empty());
    }

    #[test]
    fn beta_header_is_merged_not_replaced() {
        let creds = Credentials {
            access_token: "tok".to_owned(),
            refresh_token: None,
            expires_at: None,
            chatgpt_account_id: None,
            email: None,
        };
        let mut map = headers(&[(
            "anthropic-beta",
            "claude-code-20250219, interleaved-thinking-2025-05-14",
        )]);
        apply_auth(&creds, &mut map);
        assert_eq!(
            map.get("anthropic-beta").unwrap(),
            "claude-code-20250219,interleaved-thinking-2025-05-14,oauth-2025-04-20"
        );
        apply_auth(&creds, &mut map);
        assert_eq!(map.get_all("anthropic-beta").iter().count(), 1);
        assert_eq!(
            map.get("anthropic-beta")
                .unwrap()
                .to_str()
                .unwrap()
                .matches("oauth-2025-04-20")
                .count(),
            1
        );
    }
}

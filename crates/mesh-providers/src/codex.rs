//! Codex (ChatGPT subscription): OAuth settings, auth headers, rate-limit
//! headers, and the `wham/usage` response.
//!
//! Header names and the usage shape follow openai/codex
//! (`codex-api/src/rate_limits.rs`, `backend-client`).

use chrono::{DateTime, TimeDelta, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};
use mesh_core::Window;
use serde_json::Value;

use crate::oauth::{Encoding, Identity, OAuthApp, TokenResponse};
use crate::{Credentials, jwt, usage};

pub(crate) const APP: OAuthApp = OAuthApp {
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    scope: "openid email profile offline_access",
    refresh_scope: None,
    callback_path: "/auth/callback",
    extra_authorize_params: &[
        ("prompt", "login"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
    ],
    exchange_encoding: Encoding::Form,
    exchange_sends_state: false,
};

const ACCOUNT_ID_HEADER: HeaderName = HeaderName::from_static("chatgpt-account-id");

/// Replace the consumer's credentials with this account's.
pub(crate) fn apply_auth(credentials: &Credentials, headers: &mut HeaderMap) {
    crate::auth::replace_bearer(headers, &credentials.access_token);
    headers.remove(ACCOUNT_ID_HEADER);
    if let Some(account_id) = &credentials.chatgpt_account_id {
        match HeaderValue::from_str(account_id) {
            Ok(value) => {
                headers.insert(ACCOUNT_ID_HEADER, value);
            }
            Err(_) => tracing::warn!("ChatGPT account id is not a valid header value"),
        }
    }
}

/// The ChatGPT account id and email from the `id_token`.
pub(crate) fn identity(tokens: &TokenResponse) -> Identity {
    let Some(claims) = tokens.id_token.as_deref().and_then(jwt::payload) else {
        return Identity::default();
    };
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Identity {
        chatgpt_account_id: text(
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id")),
        ),
        email: text(claims.get("email")).or_else(|| {
            text(
                claims
                    .get("https://api.openai.com/profile")
                    .and_then(|profile| profile.get("email")),
            )
        }),
    }
}

/// Read `x-codex-primary-*` and `x-codex-secondary-*`. Additional per-model
/// limits (`x-codex-<limit>-primary-*`) are ignored, and so is a window of
/// zero minutes: plans without a second window send the secondary headers
/// with `window-minutes: 0`.
pub(crate) fn parse_rate_limit_headers(headers: &HeaderMap, now: DateTime<Utc>) -> Vec<Window> {
    let mut windows = Vec::new();
    for which in ["primary", "secondary"] {
        let header = |suffix: &str| {
            headers
                .get(format!("x-codex-{which}-{suffix}"))?
                .to_str()
                .ok()
        };
        let Some(used_percent) = header("used-percent")
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        let minutes = header("window-minutes").and_then(|value| value.parse::<i64>().ok());
        if minutes == Some(0) {
            continue;
        }
        let minutes = minutes.filter(|minutes| *minutes > 0);
        let resets_at = reset_time(
            now,
            header("reset-at").and_then(|value| value.parse().ok()),
            header("reset-after-seconds").and_then(|value| value.parse().ok()),
        );
        if used_percent == 0.0 && minutes.is_none() && resets_at.is_none() {
            continue;
        }
        windows.push(usage::window_from_percent(
            which.to_owned(),
            minutes.and_then(|m| u32::try_from(m).ok()),
            used_percent,
            resets_at,
        ));
    }
    windows
}

/// Read the usage response's account-wide `rate_limit`, in snake_case or
/// camelCase. `additional_rate_limits` is ignored, and so is a window of
/// zero seconds.
pub(crate) fn parse_usage(body: &Value, now: DateTime<Utc>) -> Vec<Window> {
    let mut windows = Vec::new();
    let Some(details) = field(body, &["rate_limit", "rateLimit"]) else {
        return windows;
    };
    for (which, names) in [
        ("primary", ["primary_window", "primaryWindow"]),
        ("secondary", ["secondary_window", "secondaryWindow"]),
    ] {
        let Some(snapshot) = field(details, &names).or_else(|| field(details, &[which])) else {
            continue;
        };
        let Some(used_percent) = field(snapshot, &["used_percent", "usedPercent"]).and_then(as_f64)
        else {
            continue;
        };
        let seconds =
            field(snapshot, &["limit_window_seconds", "limitWindowSeconds"]).and_then(as_i64);
        if seconds == Some(0) {
            continue;
        }
        let minutes = seconds
            .filter(|seconds| *seconds > 0)
            .map(|seconds| seconds / 60 + i64::from(seconds % 60 != 0))
            .or_else(|| {
                field(snapshot, &["window_minutes", "windowMinutes"])
                    .and_then(as_i64)
                    .filter(|minutes| *minutes > 0)
            });
        let resets_at = reset_time(
            now,
            field(snapshot, &["reset_at", "resetAt"]).and_then(as_i64),
            field(snapshot, &["reset_after_seconds", "resetAfterSeconds"]).and_then(as_i64),
        );
        windows.push(usage::window_from_percent(
            which.to_owned(),
            minutes.and_then(|m| u32::try_from(m).ok()),
            used_percent,
            resets_at,
        ));
    }

    let reached = field(details, &["limit_reached", "limitReached"]).and_then(Value::as_bool)
        == Some(true)
        || field(details, &["allowed"]).and_then(Value::as_bool) == Some(false);
    if reached && !windows.iter().any(|window| window.limited) {
        for window in &mut windows {
            window.limited = true;
        }
    }
    windows
}

fn reset_time(
    now: DateTime<Utc>,
    epoch_seconds: Option<i64>,
    after_seconds: Option<i64>,
) -> Option<DateTime<Utc>> {
    epoch_seconds
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .or_else(|| {
            let seconds = after_seconds.filter(|seconds| *seconds >= 0)?;
            now.checked_add_signed(TimeDelta::try_seconds(seconds)?)
        })
}

/// The first of `names` that is present and not null.
fn field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|name| value.get(*name).filter(|value| !value.is_null()))
}

fn as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|number| number.is_finite())
}

fn as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_headers as headers;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn ignores_unrepresentable_durations_and_uses_non_null_aliases() {
        let windows = parse_usage(
            &serde_json::json!({
                "rate_limit": null,
                "rateLimit": {
                    "primaryWindow": {
                        "usedPercent": 50,
                        "limitWindowSeconds": i64::MAX,
                        "resetAfterSeconds": i64::MAX
                    }
                }
            }),
            now(),
        );
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used, 0.5);
        assert_eq!(windows[0].duration_minutes, None);
        assert_eq!(windows[0].resets_at, None);
    }

    #[test]
    fn parses_the_two_header_windows_and_ignores_additional_families() {
        let windows = parse_rate_limit_headers(
            &headers(&[
                ("x-codex-primary-used-percent", "42.5"),
                ("x-codex-primary-window-minutes", "300"),
                ("x-codex-primary-reset-at", "1700001000"),
                ("x-codex-primary-reset-after-seconds", "2000"),
                ("x-codex-secondary-used-percent", "100"),
                ("x-codex-secondary-window-minutes", "10080"),
                ("x-codex-secondary-reset-after-seconds", "3600"),
                ("x-codex-bengalfox-primary-used-percent", "10"),
                ("x-codex-bengalfox-primary-window-minutes", "60"),
                ("x-codex-other-limit-primary-used-percent", "5"),
                ("x-codex-other-limit-primary-reset-at", "1700002000"),
            ]),
            now(),
        );
        let find = |id: &str| {
            windows
                .iter()
                .find(|w| w.id == id)
                .unwrap_or_else(|| panic!("{id} in {windows:?}"))
        };

        let primary = find("primary");
        assert_eq!(primary.duration_minutes, Some(300));
        assert!((primary.used - 0.425).abs() < 1e-9);
        assert_eq!(
            primary.resets_at,
            DateTime::from_timestamp(1_700_001_000, 0)
        );
        assert!(!primary.limited);

        let secondary = find("secondary");
        assert_eq!(secondary.duration_minutes, Some(10080));
        assert_eq!(secondary.resets_at, Some(now() + TimeDelta::seconds(3600)));
        assert!(secondary.limited);

        assert_eq!(windows.len(), 2);
    }

    #[test]
    fn a_zero_minute_window_is_no_window() {
        let windows = parse_rate_limit_headers(
            &headers(&[
                ("x-codex-primary-used-percent", "59"),
                ("x-codex-primary-window-minutes", "10080"),
                ("x-codex-primary-reset-after-seconds", "460672"),
                ("x-codex-secondary-used-percent", "0"),
                ("x-codex-secondary-window-minutes", "0"),
                ("x-codex-secondary-reset-after-seconds", "0"),
                ("x-codex-secondary-reset-at", ""),
            ]),
            now(),
        );
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "primary");
        let body = json!({"rate_limit": {
            "primary_window": {"used_percent": 59, "limit_window_seconds": 604_800, "reset_after_seconds": 10},
            "secondary_window": {"used_percent": 0, "limit_window_seconds": 0, "reset_after_seconds": 0}
        }});
        let windows = parse_usage(&body, now());
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "primary");
    }

    #[test]
    fn ignores_missing_or_empty_headers() {
        assert!(parse_rate_limit_headers(&HeaderMap::new(), now()).is_empty());
        let empty = headers(&[("x-codex-primary-used-percent", "0")]);
        assert!(parse_rate_limit_headers(&empty, now()).is_empty());
        let junk = headers(&[("x-codex-primary-used-percent", "lots")]);
        assert!(parse_rate_limit_headers(&junk, now()).is_empty());
    }

    #[test]
    fn parses_the_usage_body_in_both_cases_and_ignores_additional_limits() {
        let snake = json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 25, "limit_window_seconds": 18000,
                                   "reset_after_seconds": 600, "reset_at": 1_700_000_600},
                "secondary_window": {"used_percent": 60, "limit_window_seconds": 604_800,
                                     "reset_after_seconds": 90_000, "reset_at": 1_700_090_000}
            },
            "additional_rate_limits": [{
                "limit_name": "GPT-5.3-Codex-Spark",
                "metered_feature": "codex_bengalfox",
                "rate_limit": {"allowed": false, "limit_reached": true,
                               "primary_window": {"used_percent": 7, "limit_window_seconds": 3600,
                                                  "reset_after_seconds": 5, "reset_at": 1_700_000_005},
                               "secondary_window": null}
            }]
        });
        let camel = json!({
            "rateLimit": {
                "primaryWindow": {"usedPercent": 25, "limitWindowSeconds": 18000, "resetAt": 1_700_000_600},
                "secondaryWindow": {"usedPercent": 60, "limitWindowSeconds": 604_800, "resetAfterSeconds": 90_000}
            },
            "additionalRateLimits": [{
                "meteredFeature": "codex_bengalfox",
                "rateLimit": {"primaryWindow": {"usedPercent": 7, "limitWindowSeconds": 3600, "resetAt": 1_700_000_005}}
            }]
        });

        for body in [snake, camel] {
            let windows = parse_usage(&body, now());
            let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
            assert_eq!(ids, ["primary", "secondary"]);
            assert_eq!(windows[0].duration_minutes, Some(300));
            assert!((windows[0].used - 0.25).abs() < 1e-9);
            assert_eq!(
                windows[0].resets_at,
                DateTime::from_timestamp(1_700_000_600, 0)
            );
            assert_eq!(windows[1].duration_minutes, Some(10080));
            assert_eq!(
                windows[1].resets_at,
                Some(now() + TimeDelta::seconds(90_000))
            );
            assert!(windows.iter().all(|w| !w.limited));
        }
    }

    #[test]
    fn limit_reached_marks_windows_limited() {
        let full = json!({"rate_limit": {
            "allowed": false, "limit_reached": true,
            "primary_window": {"used_percent": 100, "limit_window_seconds": 18000, "reset_at": 1_700_000_600},
            "secondary_window": {"used_percent": 30, "limit_window_seconds": 604_800, "reset_at": 1_700_090_000}
        }});
        let windows = parse_usage(&full, now());
        assert!(windows[0].limited);
        assert!(!windows[1].limited, "only the full window is limited");

        let odd = json!({"rate_limit": {
            "allowed": false,
            "primary_window": {"used_percent": 50, "limit_window_seconds": 18000, "reset_at": 1_700_000_600}
        }});
        assert!(parse_usage(&odd, now())[0].limited);
    }

    #[test]
    fn empty_usage_bodies_give_no_windows() {
        assert!(parse_usage(&json!({}), now()).is_empty());
        assert!(
            parse_usage(
                &json!({"rate_limit": null, "additional_rate_limits": null}),
                now()
            )
            .is_empty()
        );
    }
}

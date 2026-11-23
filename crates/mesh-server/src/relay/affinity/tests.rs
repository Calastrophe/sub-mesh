use axum::http::{HeaderMap, HeaderValue};
use chrono::TimeDelta;
use mesh_core::Provider;

use super::*;

fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(*name, HeaderValue::from_static(value));
    }
    map
}

fn json_headers() -> HeaderMap {
    headers(&[("content-type", "application/json")])
}

fn key(provider: Provider, headers: &HeaderMap, body: &[u8]) -> Option<String> {
    small_body_hints(provider, headers, body).session
}

fn claude_ttl(body: &str) -> Option<TimeDelta> {
    small_body_hints(Provider::Claude, &json_headers(), body.as_bytes()).ttl
}

fn codex_ttl(body: &str) -> Option<TimeDelta> {
    small_body_hints(Provider::Codex, &json_headers(), body.as_bytes()).ttl
}

const CLAUDE_USER: &str = "user_9f2c_account_1b2d_session_77aa";

fn claude_body(user_id: &str) -> String {
    format!(
        r#"{{"model":"claude-sonnet","max_tokens":10,"messages":[{{"role":"user","content":"hi"}}],"metadata":{{"user_id":"{user_id}"}}}}"#
    )
}

fn codex_body(key: &str) -> String {
    format!(r#"{{"model":"gpt","input":[],"prompt_cache_key":"{key}","stream":true}}"#)
}

#[test]
fn header_keys_prefer_earlier_headers() {
    let map = headers(&[
        ("x-claude-code-session-id", "d"),
        ("conversation_id", "c"),
        ("x-session-id", "b"),
    ]);
    assert_eq!(from_headers(Provider::Codex, &map).as_deref(), Some("b"));
    assert_eq!(
        from_headers(
            Provider::Codex,
            &headers(&[("session_id", "a"), ("x-session-id", "b")])
        )
        .as_deref(),
        Some("a")
    );
    assert_eq!(
        from_headers(Provider::Codex, &headers(&[("x-other", "z")])),
        None
    );
    assert_eq!(
        from_headers(
            Provider::Codex,
            &headers(&[("session_id", " "), ("x-session-id", "b")])
        )
        .as_deref(),
        Some("b")
    );
}

#[test]
fn each_provider_prefers_its_client_session_header() {
    let map = headers(&[
        ("session_id", "generic"),
        ("session-id", "codex"),
        ("x-claude-code-session-id", "claude"),
    ]);
    assert_eq!(
        from_headers(Provider::Codex, &map).as_deref(),
        Some("codex")
    );
    assert_eq!(
        from_headers(Provider::Claude, &map).as_deref(),
        Some("claude")
    );
}

#[test]
fn header_keys_are_trimmed_and_bounded() {
    assert_eq!(
        from_headers(Provider::Codex, &headers(&[("session_id", " abc ")])).as_deref(),
        Some("abc")
    );
    let long = "x".repeat(MAX_KEY_BYTES + 1);
    let mut map = HeaderMap::new();
    map.insert("session_id", HeaderValue::from_str(&long).unwrap());
    assert_eq!(from_headers(Provider::Codex, &map), None);
    let longest = "x".repeat(MAX_KEY_BYTES);
    map.insert("session_id", HeaderValue::from_str(&longest).unwrap());
    assert_eq!(
        from_headers(Provider::Codex, &map).as_deref(),
        Some(longest.as_str())
    );
}

#[test]
fn claude_key_is_the_whole_metadata_user_id() {
    let body = claude_body(CLAUDE_USER);
    assert_eq!(
        key(Provider::Claude, &json_headers(), body.as_bytes()).as_deref(),
        Some(CLAUDE_USER)
    );
}

#[test]
fn codex_key_is_the_prompt_cache_key() {
    let body = codex_body("conv-123");
    assert_eq!(
        key(Provider::Codex, &json_headers(), body.as_bytes()).as_deref(),
        Some("conv-123")
    );
}

#[test]
fn each_provider_reads_only_its_own_field() {
    let claude = claude_body(CLAUDE_USER);
    let codex = codex_body("conv-123");
    assert_eq!(
        key(Provider::Codex, &json_headers(), claude.as_bytes()),
        None
    );
    assert_eq!(
        key(Provider::Claude, &json_headers(), codex.as_bytes()),
        None
    );
}

#[test]
fn a_missing_content_type_is_tried_and_other_types_are_not() {
    let body = codex_body("k");
    assert!(key(Provider::Codex, &HeaderMap::new(), body.as_bytes()).is_some());
    for content_type in [
        "application/json; charset=utf-8",
        "Application/JSON",
        "application/vnd.api+json",
    ] {
        let mut map = HeaderMap::new();
        map.insert("content-type", HeaderValue::from_static(content_type));
        assert!(
            key(Provider::Codex, &map, body.as_bytes()).is_some(),
            "{content_type}"
        );
    }
    for content_type in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "application/jsonx",
    ] {
        let mut map = HeaderMap::new();
        map.insert("content-type", HeaderValue::from_static(content_type));
        assert_eq!(
            key(Provider::Codex, &map, body.as_bytes()),
            None,
            "{content_type}"
        );
    }
}

#[test]
fn compressed_bodies_are_not_parsed() {
    let body = codex_body("k");
    let mut map = json_headers();
    map.insert("content-encoding", HeaderValue::from_static("zstd"));
    assert_eq!(key(Provider::Codex, &map, body.as_bytes()), None);
    map.insert("content-encoding", HeaderValue::from_static("identity"));
    assert_eq!(
        key(Provider::Codex, &map, body.as_bytes()).as_deref(),
        Some("k")
    );
}

#[tokio::test]
async fn a_large_body_supplies_a_late_key_and_lifetime_without_decoding_prompt_text() {
    let prompt = "p".repeat(FULL_PROBE_BYTES);
    let user = r#"{"session_id":"77aa"}"#;
    let quoted_user = serde_json::to_string(user).unwrap();
    for (provider, body, expected, ttl) in [
        (
            Provider::Claude,
            format!(
                r#"{{"messages":[{{"content":"{prompt}"}}],"metadata":{{"user_id":{quoted_user}}},"system":[{ONE_HOUR_BLOCK}]}}"#
            ),
            user,
            TimeDelta::hours(1),
        ),
        (
            Provider::Codex,
            format!(
                r#"{{"input":"{prompt}","prompt_cache_key":"conv","prompt_cache_retention":"24h"}}"#
            ),
            "conv",
            TimeDelta::hours(24),
        ),
    ] {
        let body = Bytes::from(body);
        let hints = http_hints(provider, &json_headers(), &body).await;
        assert_eq!(hints.session.as_deref(), Some(expected));
        assert_eq!(hints.ttl, Some(ttl));
        let mut with_header = json_headers();
        with_header.insert("session_id", HeaderValue::from_static("from-header"));
        assert_eq!(
            http_hints(provider, &with_header, &body).await.ttl,
            Some(ttl)
        );
        for map in [
            headers(&[("content-type", "text/plain")]),
            headers(&[
                ("content-encoding", "identity"),
                ("content-encoding", "gzip"),
            ]),
        ] {
            assert_eq!(
                http_hints(provider, &map, &body).await,
                BodyHints::default()
            );
        }
    }
}

#[tokio::test]
async fn a_body_over_the_scan_limit_gets_no_hints() {
    let body = Bytes::from(format!(
        r#"{{"input":"{}","prompt_cache_key":"conv","prompt_cache_retention":"24h"}}"#,
        "p".repeat(MAX_SESSION_SCAN_BYTES)
    ));
    assert_eq!(
        http_hints(Provider::Codex, &json_headers(), &body).await,
        BodyHints::default()
    );
}

fn large_claude(messages: &str, system: &str) -> BodyHints {
    let prompt = "p".repeat(FULL_PROBE_BYTES);
    let body = format!(
        r#"{{"system":{system},"messages":{messages},"metadata":{{"user_id":"u1"}},"pad":"{prompt}"}}"#
    );
    large_body_hints(Provider::Claude, body.as_bytes())
}

fn hour_message(role: &str) -> String {
    format!(r#"{{"role":"{role}","content":[{ONE_HOUR_BLOCK}]}}"#)
}

const PLAIN_MESSAGE: &str = r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#;

#[test]
fn a_large_claude_body_reads_an_hour_from_the_system_prompt() {
    let hints = large_claude(
        &format!("[{PLAIN_MESSAGE}]"),
        &format!("[{ONE_HOUR_BLOCK}]"),
    );
    assert_eq!(hints.ttl, Some(TimeDelta::hours(1)));
    assert_eq!(hints.session.as_deref(), Some("u1"));
}

#[test]
fn a_large_claude_body_reads_an_hour_from_the_tools() {
    let body = format!(
        r#"{{"tools":[{ONE_HOUR_BLOCK}],"messages":[{PLAIN_MESSAGE}],"pad":"{}"}}"#,
        "p".repeat(FULL_PROBE_BYTES)
    );
    assert_eq!(
        large_body_hints(Provider::Claude, body.as_bytes()).ttl,
        Some(TimeDelta::hours(1))
    );
}

#[test]
fn a_large_claude_body_reads_an_hour_from_the_last_message_only() {
    let last = large_claude(&format!("[{PLAIN_MESSAGE},{}]", hour_message("user")), "[]");
    assert_eq!(last.ttl, Some(TimeDelta::hours(1)));
    let nested = format!(
        r#"{{"role":"user","content":[{{"type":"tool_result","content":[{ONE_HOUR_BLOCK}]}}]}}"#
    );
    assert_eq!(
        large_claude(&format!("[{PLAIN_MESSAGE},{nested}]"), "[]").ttl,
        Some(TimeDelta::hours(1))
    );
    let earlier = large_claude(&format!("[{},{PLAIN_MESSAGE}]", hour_message("user")), "[]");
    assert_eq!(earlier.ttl, None);
    assert_eq!(earlier.session.as_deref(), Some("u1"));
}

#[test]
fn a_large_claude_body_without_messages_or_a_system_prompt_says_only_the_key() {
    for body in [
        r#"{"metadata":{"user_id":"u1"}}"#,
        r#"{"metadata":{"user_id":"u1"},"messages":[],"system":null,"tools":null}"#,
    ] {
        assert_eq!(
            large_body_hints(Provider::Claude, body.as_bytes()),
            BodyHints {
                session: Some("u1".to_owned()),
                ttl: None,
                model: None,
            }
        );
    }
}

#[test]
fn a_large_codex_body_reads_the_cache_option_lifetime() {
    let body = format!(
        r#"{{"input":"{}","prompt_cache_options":{{"ttl":"12h"}},"prompt_cache_key":"conv"}}"#,
        "p".repeat(FULL_PROBE_BYTES)
    );
    assert_eq!(
        large_body_hints(Provider::Codex, body.as_bytes()),
        BodyHints {
            session: Some("conv".to_owned()),
            ttl: Some(TimeDelta::hours(12)),
            model: None,
        }
    );
}

#[test]
fn a_queued_large_body_scan_times_out_without_hints() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (release, hold) = tokio::sync::oneshot::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || hold.blocking_recv());
        let body = Bytes::from(format!(
            r#"{{"input":"{}","prompt_cache_key":"conv"}}"#,
            "p".repeat(FULL_PROBE_BYTES)
        ));
        let headers = json_headers();
        let hints = tokio::time::timeout(
            Duration::from_secs(5),
            http_hints(Provider::Codex, &headers, &body),
        )
        .await
        .expect("forwarding must resume while the only blocking thread is occupied");
        assert_eq!(hints, BodyHints::default());
        release.send(()).unwrap();
        blocker.await.unwrap().unwrap();
    });
}

#[test]
fn a_large_body_scan_requires_a_valid_key_at_the_provider_path() {
    for body in [
        r#"{"messages":[{"metadata":{"user_id":"decoy"}}],"input":{"prompt_cache_key":"decoy"}}"#,
        r#"{"metadata":{"user_id":{}},"prompt_cache_key":["k"]}"#,
        r#"{"metadata":["k"]}"#,
        r#"["k"]"#,
        r#"{"metadata":{"user_id":"k"},"prompt_cache_key":"k"} trailing"#,
    ] {
        for provider in Provider::ALL {
            assert_eq!(
                large_body_hints(provider, body.as_bytes()),
                BodyHints::default()
            );
        }
    }
    let long = codex_body(&"x".repeat(MAX_KEY_BYTES + 1));
    assert_eq!(
        large_body_hints(Provider::Codex, long.as_bytes()),
        BodyHints {
            model: Some("gpt".to_owned()),
            ..BodyHints::default()
        }
    );
    let escaped = "\\u0061".repeat(MAX_KEY_BYTES);
    let longest = codex_body(&escaped);
    assert_eq!(
        large_body_hints(Provider::Codex, longest.as_bytes())
            .session
            .as_deref(),
        Some("a".repeat(MAX_KEY_BYTES).as_str())
    );
    let too_long = codex_body(&"\\u0061".repeat(MAX_KEY_BYTES + 1));
    assert_eq!(
        large_body_hints(Provider::Codex, too_long.as_bytes()).session,
        None
    );
}

#[test]
fn garbage_bodies_say_nothing_and_never_fail() {
    let garbage: [&[u8]; 20] = [
        b"",
        b"not json at all",
        b"{",
        b"{\"metadata\":{\"user_id\":\"abc\"",
        b"[1,2,3]",
        b"\"just a string\"",
        b"null",
        &[0xff, 0xfe, 0x00, 0x01],
        b"{\"metadata\":\"user_x\"}",
        b"{\"metadata\":null,\"prompt_cache_key\":null}",
        b"{\"metadata\":{\"user_id\":42},\"prompt_cache_key\":[\"a\"]}",
        b"{\"metadata\":{\"user_id\":\"   \"},\"prompt_cache_key\":\"\"}",
        b"{\"system\":7,\"tools\":\"x\",\"messages\":null}",
        b"{\"messages\":[null,1,\"a\",[[]],{\"cache_control\":null}]}",
        b"{\"system\":[{\"cache_control\":\"1h\"}]}",
        b"{\"system\":[{\"cache_control\":{\"ttl\":3600}}]}",
        b"{\"system\":[{\"cache_control\":{\"ttl\":[\"1h\"]}}]",
        b"{\"prompt_cache_retention\":24}",
        b"{\"prompt_cache_retention\":[\"24h\"],\"prompt_cache_key\":{}}",
        b"{\"prompt_cache_retention\":\"24h\"} trailing",
    ];
    for body in garbage {
        for provider in Provider::ALL {
            assert_eq!(
                small_body_hints(provider, &json_headers(), body),
                BodyHints::default(),
                "{provider}: {:?}",
                String::from_utf8_lossy(body)
            );
        }
    }
}

#[test]
fn a_bad_field_does_not_hide_the_other_provider_field() {
    let body = br#"{"metadata":"x","prompt_cache_key":"k"}"#;
    assert_eq!(
        key(Provider::Codex, &json_headers(), body).as_deref(),
        Some("k")
    );
    let body = br#"{"prompt_cache_key":7,"metadata":{"user_id":"u"}}"#;
    assert_eq!(
        key(Provider::Claude, &json_headers(), body).as_deref(),
        Some("u")
    );
}

#[test]
fn body_keys_are_trimmed_and_bounded() {
    assert_eq!(
        key(
            Provider::Codex,
            &json_headers(),
            br#"{"prompt_cache_key":"  k  "}"#
        )
        .as_deref(),
        Some("k")
    );
    let body = codex_body(&"k".repeat(MAX_KEY_BYTES + 1));
    assert_eq!(key(Provider::Codex, &json_headers(), body.as_bytes()), None);
}

#[test]
fn a_key_from_unicode_text_is_kept_whole() {
    let body = codex_body("sessión-日本");
    assert_eq!(
        key(Provider::Codex, &json_headers(), body.as_bytes()).as_deref(),
        Some("sessión-日本")
    );
}

const ONE_HOUR_BLOCK: &str =
    r#"{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}"#;

#[test]
fn claude_asks_for_an_hour_in_the_system_prompt_the_tools_or_a_message() {
    for body in [
        format!(r#"{{"system":[{ONE_HOUR_BLOCK}],"messages":[]}}"#),
        format!(r#"{{"tools":[{ONE_HOUR_BLOCK}],"messages":[]}}"#),
        format!(r#"{{"messages":[{{"role":"user","content":[{ONE_HOUR_BLOCK}]}}]}}"#),
        format!(
            r#"{{"messages":[{{"role":"user","content":[{{"type":"tool_result","content":[{ONE_HOUR_BLOCK}]}}]}}]}}"#
        ),
    ] {
        assert_eq!(claude_ttl(&body), Some(TimeDelta::hours(1)), "{body}");
    }
}

#[test]
fn claude_asks_for_nothing_without_an_hour_cache_control() {
    for body in [
        "{}".to_owned(),
        claude_body(CLAUDE_USER),
        r#"{"system":[{"cache_control":{"type":"ephemeral"}}]}"#.to_owned(),
        r#"{"system":[{"cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#.to_owned(),
        r#"{"messages":[{"role":"user","content":"\"ttl\":\"1h\" cache_control"}]}"#.to_owned(),
        r#"{"cache_control":{"ttl":"1h"}}"#.to_owned(),
        r#"{"tools":[{"input_schema":{"properties":{"cache_control":{"ttl":"1h"}}}}]}"#.to_owned(),
    ] {
        assert_eq!(claude_ttl(&body), None, "{body}");
    }
}

#[test]
fn codex_asks_for_a_day_with_24h_retention_only() {
    let body =
        |retention: &str| format!(r#"{{"model":"gpt","prompt_cache_retention":{retention}}}"#);
    assert_eq!(codex_ttl(&body(r#""24h""#)), Some(TimeDelta::hours(24)));
    for retention in [r#""in_memory""#, r#""1h""#, "null", "24"] {
        assert_eq!(codex_ttl(&body(retention)), None, "{retention}");
    }
    assert_eq!(codex_ttl(&codex_body("k")), None);
}

#[test]
fn codex_reads_the_supported_cache_option_lifetimes() {
    for (ttl, expected) in [
        ("30m", Some(TimeDelta::minutes(30))),
        ("12h", Some(TimeDelta::hours(12))),
        ("24h", None),
        ("999999999999999999999h", None),
    ] {
        let body =
            format!(r#"{{"prompt_cache_key":"conv","prompt_cache_options":{{"ttl":"{ttl}"}}}}"#);
        let hints = small_body_hints(Provider::Codex, &json_headers(), body.as_bytes());
        assert_eq!(hints.session.as_deref(), Some("conv"));
        assert_eq!(hints.ttl, expected, "{ttl}");
        assert_eq!(claude_ttl(&body), None);
    }
}

#[test]
fn codex_prefers_a_recognized_cache_option_and_falls_back_to_retention() {
    let body = |options: &str| {
        format!(r#"{{"prompt_cache_options":{options},"prompt_cache_retention":"24h"}}"#)
    };
    assert_eq!(
        codex_ttl(&body(r#"{"ttl":"12h"}"#)),
        Some(TimeDelta::hours(12))
    );
    for options in [
        "null",
        "true",
        "{}",
        r#"{"ttl":12}"#,
        r#"{"ttl":"unknown"}"#,
    ] {
        assert_eq!(codex_ttl(&body(options)), Some(TimeDelta::hours(24)));
    }
}

#[test]
fn a_provider_ignores_the_other_providers_retention() {
    let claude = format!(r#"{{"system":[{ONE_HOUR_BLOCK}]}}"#);
    let codex = r#"{"prompt_cache_retention":"24h"}"#;
    assert_eq!(
        small_body_hints(Provider::Codex, &json_headers(), claude.as_bytes()),
        BodyHints::default()
    );
    assert_eq!(
        small_body_hints(Provider::Claude, &json_headers(), codex.as_bytes()),
        BodyHints::default()
    );
}

#[test]
fn one_parse_gives_the_key_and_the_retention() {
    let claude =
        format!(r#"{{"metadata":{{"user_id":"{CLAUDE_USER}"}},"system":[{ONE_HOUR_BLOCK}]}}"#);
    assert_eq!(
        small_body_hints(Provider::Claude, &json_headers(), claude.as_bytes()),
        BodyHints {
            session: Some(CLAUDE_USER.to_owned()),
            ttl: Some(TimeDelta::hours(1)),
            model: None,
        }
    );
    let codex = r#"{"prompt_cache_key":"conv","prompt_cache_retention":"24h"}"#;
    assert_eq!(
        small_body_hints(Provider::Codex, &json_headers(), codex.as_bytes()),
        BodyHints {
            session: Some("conv".to_owned()),
            ttl: Some(TimeDelta::hours(24)),
            model: None,
        }
    );
}

fn model(provider: Provider, body: &str) -> Option<String> {
    small_body_hints(provider, &json_headers(), body.as_bytes()).model
}

#[test]
fn one_parse_gives_the_model_with_the_key_and_the_retention() {
    assert_eq!(
        small_body_hints(
            Provider::Claude,
            &json_headers(),
            claude_body(CLAUDE_USER).as_bytes()
        ),
        BodyHints {
            session: Some(CLAUDE_USER.to_owned()),
            ttl: None,
            model: Some("claude-sonnet".to_owned()),
        }
    );
    let codex = r#"{"model":"gpt-5","prompt_cache_key":"conv","prompt_cache_retention":"24h"}"#;
    assert_eq!(
        small_body_hints(Provider::Codex, &json_headers(), codex.as_bytes()),
        BodyHints {
            session: Some("conv".to_owned()),
            ttl: Some(TimeDelta::hours(24)),
            model: Some("gpt-5".to_owned()),
        }
    );
}

#[test]
fn the_model_is_kept_as_the_request_has_it() {
    for provider in Provider::ALL {
        assert_eq!(
            model(provider, r#"{"model":"Claude-Sonnet-4"}"#).as_deref(),
            Some("Claude-Sonnet-4")
        );
        assert_eq!(
            model(provider, r#"{"model":"gpt-5"}"#).as_deref(),
            Some("gpt-5")
        );
        assert_eq!(
            model(provider, r#"{"input":{"model":"nested"}}"#),
            None,
            "only the top-level field counts"
        );
    }
}

#[test]
fn a_model_that_is_not_a_model_id_says_nothing_and_hides_nothing() {
    let long = "m".repeat(MAX_MODEL_BYTES + 1);
    let escaped_long = "\\u006d".repeat(MAX_MODEL_BYTES + 1);
    let longest = "m".repeat(MAX_MODEL_BYTES);
    for provider in Provider::ALL {
        for value in [
            "7",
            "null",
            "true",
            r#"["gpt"]"#,
            r#"{"id":"gpt"}"#,
            r#""""#,
            &format!(r#""{long}""#),
            &format!(r#""{escaped_long}""#),
        ] {
            let body = format!(
                r#"{{"model":{value},"prompt_cache_key":"k","metadata":{{"user_id":"u"}}}}"#
            );
            let hints = small_body_hints(provider, &json_headers(), body.as_bytes());
            assert_eq!(hints.model, None, "{provider}: {value}");
            assert!(hints.session.is_some(), "{provider}: {value}");
        }
        assert_eq!(
            model(provider, &format!(r#"{{"model":"{longest}"}}"#)).as_deref(),
            Some(longest.as_str())
        );
    }
}

#[test]
fn compressed_or_foreign_bodies_say_no_model() {
    let body = codex_body("k");
    let mut compressed = json_headers();
    compressed.insert("content-encoding", HeaderValue::from_static("zstd"));
    for headers in [compressed, headers(&[("content-type", "text/plain")])] {
        assert_eq!(
            small_body_hints(Provider::Codex, &headers, body.as_bytes()).model,
            None
        );
    }
}

#[test]
fn a_large_body_supplies_a_late_model_without_decoding_prompt_text() {
    let prompt = "p".repeat(FULL_PROBE_BYTES);
    for (provider, body, expected) in [
        (
            Provider::Claude,
            format!(r#"{{"messages":[{{"content":"{prompt}"}}],"model":"claude-opus-4"}}"#),
            "claude-opus-4",
        ),
        (
            Provider::Codex,
            format!(r#"{{"input":"{prompt}","model":"gpt-5-codex"}}"#),
            "gpt-5-codex",
        ),
    ] {
        let hints = large_body_hints(provider, body.as_bytes());
        assert_eq!(hints.model.as_deref(), Some(expected), "{provider}");
    }
    let long = "m".repeat(MAX_MODEL_BYTES + 1);
    for provider in Provider::ALL {
        let body = format!(r#"{{"model":"{long}","pad":"{prompt}"}}"#);
        assert_eq!(large_body_hints(provider, body.as_bytes()).model, None);
    }
}

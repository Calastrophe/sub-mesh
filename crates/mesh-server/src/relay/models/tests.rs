use axum::http::HeaderValue;

use super::*;

fn kept<'a>(entries: &'a [(String, &RawValue)]) -> Vec<(&'a str, &'a RawValue)> {
    entries
        .iter()
        .map(|(id, raw)| (id.as_str(), *raw))
        .collect()
}

fn listed(provider: Provider, body: &str) -> Vec<String> {
    entries(provider, body.as_bytes())
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

#[test]
fn cache_enforces_the_total_byte_budget_and_rejects_old_replacements() {
    let lists = ModelLists::default();
    let key = (Provider::Codex, None);
    let other = (Provider::Codex, Some("client_version=2".to_owned()));
    let catalog = Cached {
        body: Bytes::from(vec![0; MAX_CATALOG_BYTES]),
        etag: None,
        revision: 2,
        made: Instant::now(),
    };
    lists.keep(key.clone(), catalog.clone());
    lists.keep(other.clone(), catalog.clone());
    lists.keep(
        key.clone(),
        Cached {
            revision: 1,
            ..catalog
        },
    );
    assert!(lists.fresh(&key, 2).is_some());
    assert!(lists.fresh(&other, 2).is_none());
}

#[test]
fn only_a_get_of_the_exact_list_path_is_a_list() {
    assert!(is_list(Provider::Codex, &Method::GET, "/models"));
    assert!(is_list(Provider::Claude, &Method::GET, "/v1/models"));
    assert!(!is_list(Provider::Codex, &Method::POST, "/models"));
    assert!(!is_list(Provider::Codex, &Method::HEAD, "/models"));
    assert!(!is_list(Provider::Codex, &Method::GET, "/v1/models"));
    assert!(!is_list(Provider::Claude, &Method::GET, "/models"));
    for rest in [
        "",
        "/models/",
        "/models/gpt-5",
        "/v1/models/x",
        "/responses",
    ] {
        for provider in Provider::ALL {
            assert!(!is_list(provider, &Method::GET, rest), "{provider} {rest}");
        }
    }
}

#[test]
fn entries_are_keyed_by_slug_for_codex_and_by_id_for_claude() {
    let codex = r#"{"models":[{"slug":"a","id":"x"},{"slug":"b"}]}"#;
    assert_eq!(listed(Provider::Codex, codex), ["a", "b"]);
    assert!(entries(Provider::Claude, codex.as_bytes()).is_none());
    let claude = r#"{"data":[{"id":"a","slug":"x"},{"id":"b"}],"has_more":false}"#;
    assert_eq!(listed(Provider::Claude, claude), ["a", "b"]);
    assert!(entries(Provider::Codex, claude.as_bytes()).is_none());
}

#[test]
fn an_entry_without_a_usable_key_is_left_out() {
    let body = r#"{"models":[{"slug":"a"},{},{"slug":""},{"slug":7},{"slug":null},"b",{"name":"c"},{"slug":"d"}]}"#;
    assert_eq!(listed(Provider::Codex, body), ["a", "d"]);
}

#[test]
fn an_answer_that_is_not_a_list_is_unreadable() {
    for body in [
        "",
        "not json",
        "[]",
        "{}",
        r#"{"models":{}}"#,
        r#"{"models":"a"}"#,
        r#"{"models":[]} trailing"#,
    ] {
        assert!(
            entries(Provider::Codex, body.as_bytes()).is_none(),
            "{body}"
        );
    }
    assert_eq!(
        listed(Provider::Codex, r#"{"models":[]}"#),
        Vec::<String>::new()
    );
}

#[test]
fn a_codex_list_keeps_every_entry_as_it_was_sent() {
    let first = "{ \"slug\":\"a\",\n  \"base_instructions\":\"line\\nbreak \\u00e9\",\"n\":1.50 }";
    let second = r#"{"slug":"b","priority":2}"#;
    let body = format!(r#"{{"models":[{first}, {second}],"other":true}}"#);
    let parsed = entries(Provider::Codex, body.as_bytes()).unwrap();
    assert_eq!(
        String::from_utf8(render(Provider::Codex, &kept(&parsed))).unwrap(),
        format!(r#"{{"models":[{first},{second}]}}"#)
    );
}

#[test]
fn a_claude_list_is_a_page_without_more() {
    let body = r#"{"data":[{"type":"model","id":"claude-a","display_name":"A"},{"type":"model","id":"claude-\"b"}],"has_more":true,"first_id":"x","last_id":"y"}"#;
    let parsed = entries(Provider::Claude, body.as_bytes()).unwrap();
    let rendered = render(Provider::Claude, &kept(&parsed));
    let text = String::from_utf8(rendered.clone()).unwrap();
    assert_eq!(
        text,
        r#"{"data":[{"type":"model","id":"claude-a","display_name":"A"},{"type":"model","id":"claude-\"b"}],"has_more":false,"first_id":"claude-a","last_id":"claude-\"b"}"#
    );
    let value: serde_json::Value = serde_json::from_slice(&rendered).unwrap();
    assert_eq!(value["last_id"], "claude-\"b");
}

#[test]
fn an_empty_list_still_has_its_envelope() {
    assert_eq!(render(Provider::Codex, &[]), br#"{"models":[]}"#);
    assert_eq!(
        render(Provider::Claude, &[]),
        br#"{"data":[],"has_more":false,"first_id":null,"last_id":null}"#
    );
}

#[test]
fn the_etag_is_a_quoted_tag_of_the_content() {
    let tag = etag(b"{\"models\":[]}");
    assert_eq!(tag, etag(b"{\"models\":[]}"));
    assert_ne!(tag, etag(b"{\"models\":[{}]}"));
    let text = tag.to_str().unwrap();
    assert!(text.starts_with('"') && text.ends_with('"'), "{text}");
    assert_eq!(text.len(), 18);
}

#[test]
fn accounts_are_asked_for_the_whole_list_in_the_clear() {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("accept-encoding", "gzip, zstd"),
        ("if-none-match", "\"x\""),
        ("if-match", "\"x\""),
        ("if-modified-since", "Wed, 21 Oct 2026 07:28:00 GMT"),
        ("if-unmodified-since", "Wed, 21 Oct 2026 07:28:00 GMT"),
        ("if-range", "\"x\""),
        ("range", "bytes=0-9"),
        ("user-agent", "codex"),
        ("originator", "codex_cli_rs"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    let asked = list_headers(&headers);
    assert_eq!(asked["accept-encoding"], "identity");
    assert_eq!(asked["user-agent"], "codex");
    assert_eq!(asked["originator"], "codex_cli_rs");
    assert_eq!(asked.len(), 3);
}

fn list(body: &'static str, revision: u64, made: Instant) -> Cached {
    Cached {
        body: Bytes::from_static(body.as_bytes()),
        etag: None,
        revision,
        made,
    }
}

fn key(query: &str) -> ListKey {
    (Provider::Codex, Some(query.to_owned()))
}

#[test]
fn a_kept_list_serves_its_query_until_it_ages_or_goes_stale() {
    let lists = ModelLists::new(DEADLINE, Duration::from_millis(80));
    lists.keep(key("a"), list("one", 3, Instant::now()));
    let fresh = |query, revision| lists.fresh(&key(query), revision).map(|list| list.body);
    assert_eq!(fresh("a", 3), Some(Bytes::from_static(b"one")));
    assert_eq!(fresh("b", 3), None);
    assert_eq!(fresh("a", 4), None);
    assert_eq!(
        lists
            .fresh(&(Provider::Claude, Some("a".to_owned())), 3)
            .map(|list| list.body),
        None
    );
    assert_eq!(
        lists
            .fresh(&(Provider::Codex, None), 3)
            .map(|list| list.body),
        None
    );
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(fresh("a", 3), None);
}

#[test]
fn the_kept_lists_are_bounded() {
    let lists = ModelLists::default();
    for n in 0..MAX_CACHED + 10 {
        lists.keep(key(&n.to_string()), list("x", 0, Instant::now()));
    }
    let kept = lists.cached.lock().unwrap().len();
    assert_eq!(kept, MAX_CACHED);
    lists.keep(key("0"), list("again", 0, Instant::now()));
    assert_eq!(
        lists.fresh(&key("0"), 0).map(|list| list.body),
        Some(Bytes::from_static(b"again")),
        "a query that is kept may be replaced when the table is full"
    );
    assert!(
        lists
            .fresh(&key(&format!("{}", MAX_CACHED + 5)), 0)
            .is_none()
    );
}

#[test]
fn old_lists_make_room_for_new_ones() {
    let lists = ModelLists::new(DEADLINE, Duration::from_millis(20));
    for n in 0..MAX_CACHED {
        lists.keep(key(&n.to_string()), list("x", 0, Instant::now()));
    }
    std::thread::sleep(Duration::from_millis(40));
    lists.keep(key("new"), list("y", 0, Instant::now()));
    assert_eq!(lists.cached.lock().unwrap().len(), 1);
    assert!(lists.fresh(&key("new"), 0).is_some());
}

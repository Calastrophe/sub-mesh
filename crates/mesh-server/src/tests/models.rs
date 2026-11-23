//! Model lists answered by the server, and routing of requests by model.

use mesh_core::protocol::DENIED_MODEL;
use mesh_core::{ApiError, DenyReason, ModelAllowlist};
use serde_json::{Value, json};

use super::*;
use crate::relay::ModelLists;
use crate::testing::state_with_lists;

fn allowlist(patterns: &[&str]) -> ModelAllowlist {
    ModelAllowlist::new(patterns.iter().copied()).unwrap()
}

pub(super) fn codex(id: &str, models: Option<&[&str]>) -> AccountReport {
    AccountReport {
        models: models.map(allowlist),
        ..account(id, 0.7)
    }
}

fn claude(id: &str, models: Option<&[&str]>) -> AccountReport {
    AccountReport {
        provider: Provider::Claude,
        ..codex(id, models)
    }
}

fn list_path(provider: Provider, id: &str) -> String {
    match provider {
        Provider::Codex => format!("/relay/{id}/models"),
        Provider::Claude => format!("/relay/{id}/v1/models"),
    }
}

fn on_list(stub: &Stub, provider: Provider, id: &str, reply: Reply) {
    stub.on(Method::GET, &list_path(provider, id), reply);
}

fn json_reply(body: String) -> Reply {
    Reply::status(200)
        .header("content-type", "application/json")
        .body(body)
}

/// A Codex catalog of the entries, each as raw JSON text.
fn catalog(entries: &[&str]) -> Reply {
    json_reply(format!(r#"{{"models":[{}]}}"#, entries.join(",")))
}

/// A Codex catalog of entries that have these slugs.
fn slugs(slugs: &[&str]) -> Reply {
    let entries: Vec<String> = slugs
        .iter()
        .map(|slug| format!(r#"{{"slug":"{slug}","display_name":"{slug}"}}"#))
        .collect();
    catalog(&entries.iter().map(String::as_str).collect::<Vec<_>>())
}

/// A Claude page of entries that have these ids.
fn page(ids: &[&str]) -> Reply {
    let entries: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"type":"model","id":"{id}","display_name":"{id}"}}"#))
        .collect();
    json_reply(format!(
        r#"{{"data":[{}],"has_more":false,"first_id":"x","last_id":"y"}}"#,
        entries.join(",")
    ))
}

fn config() -> Config {
    Config {
        insecure_skip_whois: true,
        web_dir: None,
        ..Config::default()
    }
}

/// A state with `stub` as the daemon of every account.
async fn list_state(
    stub: &Arc<Stub>,
    accounts: Vec<AccountReport>,
    lists: ModelLists,
) -> Arc<AppState> {
    let state = state_with_lists(config(), lists);
    FakeDaemon::connect(&state, "node", stub.router()).await;
    apply(&state, report("node", accounts), None).await;
    state
}

async fn get(addr: SocketAddr, path: &str) -> reqwest::Response {
    reqwest::get(format!("http://{addr}{path}")).await.unwrap()
}

fn available(state: &AppState, id: &str) -> Vec<String> {
    state
        .with_registry(|r| r.view(&id.parse().unwrap(), Utc::now()))
        .unwrap()
        .available_models
}

pub(super) async fn logged(state: &AppState, count: usize) -> Vec<mesh_core::api::LogEntry> {
    let mut records = Vec::new();
    for _ in 0..50 {
        records = state
            .db
            .run(|db| db.recent_requests_of_anyone(20))
            .await
            .unwrap();
        if records.len() >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    records
}

#[tokio::test]
async fn codex_models_merge_by_slug_after_each_accounts_allowlist() {
    let stub = Stub::new();
    let first = "{ \"slug\":\"m1\",\n \"priority\": 2,\"base_instructions\":\"a\\nb\" }";
    let shared_a = r#"{"slug":"shared","from":"a"}"#;
    on_list(&stub, Provider::Codex, "a", catalog(&[first, shared_a]));
    on_list(
        &stub,
        Provider::Codex,
        "b",
        catalog(&[
            r#"{"slug":"shared","from":"b"}"#,
            r#"{"slug":"m3"}"#,
            r#"{"slug":"m4"}"#,
        ]),
    );
    let state = list_state(
        &stub,
        vec![codex("a", None), codex("b", Some(&["shared", "M3"]))],
        ModelLists::default(),
    )
    .await;
    let addr = serve(state.clone()).await;
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/codex/models?client_version=0.171.0"))
        .header("accept-encoding", "gzip, zstd")
        .header("if-none-match", "\"old\"")
        .header("x-mesh-internal", "secret")
        .header("user-agent", "codex-test")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    assert_eq!(
        response.text().await.unwrap(),
        format!(r#"{{"models":[{first},{shared_a},{{"slug":"m3"}}]}}"#)
    );

    let mut seen = stub.seen();
    seen.sort_by(|a, b| a.path.cmp(&b.path));
    let paths: Vec<_> = seen.iter().map(|seen| seen.path.as_str()).collect();
    assert_eq!(paths, ["/relay/a/models", "/relay/b/models"]);
    for seen in &seen {
        assert_eq!(seen.method, Method::GET);
        assert_eq!(seen.query.as_deref(), Some("client_version=0.171.0"));
        assert_eq!(seen.headers["accept-encoding"], "identity");
        assert_eq!(seen.headers["user-agent"], "codex-test");
        assert!(!seen.headers.contains_key("if-none-match"));
        assert!(!seen.headers.contains_key("x-mesh-internal"));
        assert!(seen.body.is_empty());
    }
    assert_eq!(available(&state, "a"), ["m1", "shared"]);
    assert_eq!(
        available(&state, "b"),
        ["m3", "m4", "shared"],
        "the recorded ids are the whole list, before the allowlist"
    );
}

#[tokio::test]
async fn claude_models_merge_by_id_into_one_page() {
    let stub = Stub::new();
    on_list(
        &stub,
        Provider::Claude,
        "a",
        page(&["claude-sonnet-4", "claude-shared"]),
    );
    on_list(
        &stub,
        Provider::Claude,
        "b",
        page(&["claude-shared", "claude-haiku-4", "claude-opus-4"]),
    );
    let state = list_state(
        &stub,
        vec![claude("a", None), claude("b", Some(&["claude-*-4"]))],
        ModelLists::default(),
    )
    .await;
    let addr = serve(state.clone()).await;
    let response = get(addr, "/claude/v1/models?limit=1000").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert!(!response.headers().contains_key("etag"));
    let page: Value = response.json().await.unwrap();
    let ids: Vec<_> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "claude-sonnet-4",
            "claude-shared",
            "claude-haiku-4",
            "claude-opus-4"
        ]
    );
    assert_eq!(page["data"][0]["type"], "model");
    assert_eq!(page["has_more"], false);
    assert_eq!(page["first_id"], "claude-sonnet-4");
    assert_eq!(page["last_id"], "claude-opus-4");
    for seen in stub.seen() {
        assert_eq!(seen.query.as_deref(), Some("limit=1000"));
    }
    assert_eq!(
        available(&state, "b"),
        ["claude-haiku-4", "claude-opus-4", "claude-shared"]
    );
}

#[tokio::test]
async fn a_list_that_every_allowlist_empties_is_still_an_answer() {
    let stub = Stub::new();
    on_list(&stub, Provider::Codex, "a", slugs(&["m1"]));
    on_list(&stub, Provider::Claude, "c", page(&["claude-a"]));
    let state = list_state(
        &stub,
        vec![codex("a", Some(&["other"])), claude("c", Some(&["other"]))],
        ModelLists::default(),
    )
    .await;
    let addr = serve(state).await;
    let response = get(addr, "/codex/models?client_version=1").await;
    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key("etag"));
    assert_eq!(response.text().await.unwrap(), r#"{"models":[]}"#);
    let response = get(addr, "/claude/v1/models").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"data":[],"has_more":false,"first_id":null,"last_id":null}"#
    );
}

#[tokio::test]
async fn the_etag_follows_the_content() {
    let stub = Stub::new();
    on_list(&stub, Provider::Codex, "a", slugs(&["m1"]));
    let state = list_state(&stub, vec![codex("a", None)], ModelLists::default()).await;
    let addr = serve(state).await;
    let tag = |response: &reqwest::Response| response.headers()["etag"].clone();
    let one = get(addr, "/codex/models?client_version=1").await;
    let same = get(addr, "/codex/models?client_version=2").await;
    assert_eq!(tag(&one), tag(&same));
    on_list(&stub, Provider::Codex, "a", slugs(&["m1", "m2"]));
    let more = get(addr, "/codex/models?client_version=3").await;
    assert_ne!(tag(&one), tag(&more));
}

#[tokio::test]
async fn only_online_admitting_accounts_are_asked() {
    let stub = Stub::new();
    for id in ["a", "b", "c", "d"] {
        on_list(&stub, Provider::Codex, id, slugs(&[&format!("m-{id}")]));
    }
    let mut paused = codex("b", None);
    paused.paused = true;
    paused.admission = Admission::Deny {
        reason: DenyReason::Paused,
    };
    let state = list_state(
        &stub,
        vec![codex("a", None), paused, codex("c", None), codex("d", None)],
        ModelLists::default(),
    )
    .await;
    state.with_registry(|r| {
        r.cooldown_account(&"c".parse().unwrap(), Utc::now() + TimeDelta::minutes(1))
    });
    let other = Stub::new();
    let _node = FakeDaemon::connect(&state, "other", other.router()).await;
    apply(&state, report("other", vec![claude("x", None)]), None).await;

    let addr = serve(state).await;
    let response = get(addr, "/codex/models?client_version=1").await;
    assert_eq!(response.status(), 200);
    let value: Value = response.json().await.unwrap();
    let slugs: Vec<_> = value["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["m-a", "m-d"]);
    let paths: Vec<_> = stub.seen().into_iter().map(|seen| seen.path).collect();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&list_path(Provider::Codex, "a")));
    assert!(paths.contains(&list_path(Provider::Codex, "d")));
    assert!(other.seen().is_empty(), "another provider is not asked");
}

#[tokio::test]
async fn the_answers_that_arrive_in_time_are_merged() {
    let stub = Stub::new();
    on_list(&stub, Provider::Codex, "a", Reply::status(500).body("boom"));
    on_list(&stub, Provider::Codex, "b", slugs(&["m-b"]));
    on_list(
        &stub,
        Provider::Codex,
        "c",
        slugs(&["m-c"]).delay(Duration::from_secs(5)),
    );
    on_list(
        &stub,
        Provider::Codex,
        "d",
        Reply::status(200).body("<html>"),
    );
    let lists = ModelLists::new(Duration::from_millis(400), Duration::from_secs(60));
    let state = list_state(
        &stub,
        vec![
            codex("a", None),
            codex("b", None),
            codex("c", None),
            codex("d", None),
        ],
        lists,
    )
    .await;
    let addr = serve(state.clone()).await;
    let started = std::time::Instant::now();
    let response = get(addr, "/codex/models?client_version=1").await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(response.status(), 200);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["models"].as_array().unwrap().len(), 1);
    assert_eq!(value["models"][0]["slug"], "m-b");
    wait_for_in_flight(&state, "c", 0).await;
    assert!(available(&state, "c").is_empty());
    assert_eq!(available(&state, "b"), ["m-b"]);
    let records = logged(&state, 1).await;
    assert_eq!(records[0].status, 200);
    assert_eq!(records[0].account_id, None);
    assert_eq!(records[0].attempts, 4);
}

#[tokio::test]
async fn no_answer_is_a_503_with_an_api_error_and_is_not_kept() {
    let stub = Stub::new();
    on_list(&stub, Provider::Codex, "a", Reply::status(500));
    on_list(
        &stub,
        Provider::Codex,
        "b",
        Reply::status(200).body("not json"),
    );
    let lists = ModelLists::new(Duration::from_millis(300), Duration::from_secs(60));
    let state = list_state(&stub, vec![codex("a", None), codex("b", None)], lists).await;
    let addr = serve(state).await;
    let response = get(addr, "/codex/models?client_version=1").await;
    assert_eq!(response.status(), 503);
    let error: ApiError = response.json().await.unwrap();
    assert!(error.error.contains("no account"), "{}", error.error);

    on_list(&stub, Provider::Codex, "a", slugs(&["m1"]));
    let response = get(addr, "/codex/models?client_version=1").await;
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn late_answers_alone_are_a_503() {
    let stub = Stub::new();
    on_list(
        &stub,
        Provider::Claude,
        "a",
        page(&["claude-a"]).delay(Duration::from_secs(5)),
    );
    let lists = ModelLists::new(Duration::from_millis(200), Duration::from_secs(60));
    let state = list_state(&stub, vec![claude("a", None)], lists).await;
    let addr = serve(state).await;
    let response = get(addr, "/claude/v1/models?limit=1000").await;
    assert_eq!(response.status(), 503);
    let error: ApiError = response.json().await.unwrap();
    assert!(error.error.contains("answered"), "{}", error.error);
}

#[tokio::test]
async fn without_an_account_the_list_is_a_503() {
    let state = state();
    let addr = serve(state.clone()).await;
    for path in ["/codex/models?client_version=1", "/claude/v1/models"] {
        let response = get(addr, path).await;
        assert_eq!(response.status(), 503, "{path}");
        let error: ApiError = response.json().await.unwrap();
        assert_eq!(error.error, "mesh: no account has headroom");
    }
}

#[tokio::test]
async fn a_merged_list_is_kept_per_provider_and_query() {
    let stub = Stub::new();
    on_list(&stub, Provider::Codex, "a", slugs(&["m1"]));
    on_list(&stub, Provider::Claude, "c", page(&["claude-a"]));
    let state = list_state(
        &stub,
        vec![codex("a", None), claude("c", None)],
        ModelLists::default(),
    )
    .await;
    let addr = serve(state.clone()).await;
    let calls = |provider, id| stub.calls(&list_path(provider, id));

    let first = get(addr, "/codex/models?client_version=1").await;
    let etag = first.headers()["etag"].clone();
    let body = first.text().await.unwrap();
    let again = get(addr, "/codex/models?client_version=1").await;
    assert_eq!(again.headers()["etag"], etag);
    assert_eq!(again.text().await.unwrap(), body);
    assert_eq!(calls(Provider::Codex, "a"), 1);

    get(addr, "/codex/models?client_version=2").await;
    get(addr, "/codex/models").await;
    assert_eq!(calls(Provider::Codex, "a"), 3);
    get(addr, "/claude/v1/models?limit=1000").await;
    get(addr, "/claude/v1/models?limit=1000").await;
    assert_eq!(calls(Provider::Claude, "c"), 1);
    assert_eq!(calls(Provider::Codex, "a"), 3);

    let records = logged(&state, 6).await;
    assert_eq!(records.len(), 6);
    assert_eq!(
        records.iter().filter(|record| record.attempts == 0).count(),
        2,
        "kept lists ask no account"
    );
    assert!(records.iter().all(|record| record.status == 200));
}

#[tokio::test]
async fn a_kept_list_expires() {
    let stub = Stub::new();
    on_list(&stub, Provider::Codex, "a", slugs(&["m1"]));
    let lists = ModelLists::new(Duration::from_secs(2), Duration::from_millis(150));
    let state = list_state(&stub, vec![codex("a", None)], lists).await;
    let addr = serve(state).await;
    get(addr, "/codex/models?client_version=1").await;
    get(addr, "/codex/models?client_version=1").await;
    assert_eq!(stub.calls(&list_path(Provider::Codex, "a")), 1);
    tokio::time::sleep(Duration::from_millis(250)).await;
    get(addr, "/codex/models?client_version=1").await;
    assert_eq!(stub.calls(&list_path(Provider::Codex, "a")), 2);
}

#[tokio::test]
async fn a_changed_allowlist_makes_a_kept_list_stale() {
    let stub = Stub::new();
    on_list(&stub, Provider::Codex, "a", slugs(&["m1", "m2"]));
    let state = list_state(&stub, vec![codex("a", None)], ModelLists::default()).await;
    let addr = serve(state.clone()).await;
    let slugs_of = |value: Value| -> Vec<String> {
        value["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["slug"].as_str().unwrap().to_owned())
            .collect()
    };
    let path = "/codex/models?client_version=1";
    assert_eq!(
        slugs_of(get(addr, path).await.json().await.unwrap()),
        ["m1", "m2"]
    );

    apply(
        &state,
        report("node", vec![codex("a", Some(&["m2"]))]),
        None,
    )
    .await;
    assert_eq!(
        slugs_of(get(addr, path).await.json().await.unwrap()),
        ["m2"]
    );
    assert_eq!(stub.calls(&list_path(Provider::Codex, "a")), 2);
    assert_eq!(
        slugs_of(get(addr, path).await.json().await.unwrap()),
        ["m2"]
    );
    assert_eq!(stub.calls(&list_path(Provider::Codex, "a")), 2);
}

#[tokio::test]
async fn other_requests_on_the_list_paths_are_relayed_as_usual() {
    let stub = Stub::new();
    stub.on(
        Method::POST,
        "/relay/a/models",
        Reply::status(200).body("posted"),
    );
    stub.on(
        Method::GET,
        "/relay/a/models/gpt-5",
        Reply::status(200).body("one"),
    );
    stub.on(
        Method::GET,
        "/relay/a/v1/models",
        Reply::status(200).body("unused"),
    );
    stub.on(
        Method::GET,
        "/relay/c/v1/models/claude-a",
        Reply::status(200).body("claude one"),
    );
    stub.on(Method::HEAD, "/relay/a/models", Reply::status(200));
    let state = list_state(
        &stub,
        vec![codex("a", None), claude("c", None)],
        ModelLists::default(),
    )
    .await;
    let addr = serve(state).await;
    let client = reqwest::Client::new();

    let posted = client
        .post(format!("http://{addr}/codex/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(posted.text().await.unwrap(), "posted");
    assert_eq!(
        get(addr, "/codex/models/gpt-5").await.text().await.unwrap(),
        "one"
    );
    assert_eq!(
        get(addr, "/claude/v1/models/claude-a")
            .await
            .text()
            .await
            .unwrap(),
        "claude one"
    );
    let head = client
        .head(format!("http://{addr}/codex/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    let methods: Vec<_> = stub.seen().into_iter().map(|seen| seen.method).collect();
    assert_eq!(
        methods,
        [Method::POST, Method::GET, Method::GET, Method::HEAD]
    );
}

#[tokio::test]
async fn a_peer_outside_the_consumer_policy_cannot_list_models() {
    let stub = Stub::new();
    on_list(&stub, Provider::Codex, "a", slugs(&["m1"]));
    let state = state_with_lists(
        Config {
            web_dir: None,
            ..Config::default()
        },
        ModelLists::default(),
    );
    FakeDaemon::connect(&state, "node", stub.router()).await;
    apply(&state, report("node", vec![codex("a", None)]), None).await;
    let addr = serve(state).await;
    assert_eq!(
        get(addr, "/codex/models?client_version=1").await.status(),
        403
    );
    assert_eq!(get(addr, "/claude/v1/models").await.status(), 403);
    assert!(stub.seen().is_empty());
}

fn model_body(model: &str) -> String {
    format!(r#"{{"model":"{model}","input":[]}}"#)
}

async fn post_model(addr: SocketAddr, model: &str) -> reqwest::Response {
    codex_request(addr)
        .header("content-type", "application/json")
        .body(model_body(model))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_request_goes_only_to_accounts_that_allow_its_model() {
    let stub = relays(vec![
        ("a", Reply::status(200).body("from a")),
        ("b", Reply::status(200).body("from b")),
    ]);
    let state = relay_state(
        stub.router(),
        vec![
            codex("a", Some(&["gpt-5-codex"])),
            codex("b", Some(&["gpt-5"])),
        ],
    )
    .await;
    let addr = serve(state).await;
    assert_eq!(
        post_model(addr, "gpt-5").await.text().await.unwrap(),
        "from b"
    );
    assert_eq!(
        post_model(addr, "GPT-5-Codex").await.text().await.unwrap(),
        "from a"
    );
    assert_eq!(
        post_model(addr, "gpt-5").await.text().await.unwrap(),
        "from b"
    );
    expect_calls(&stub, &[("a", 1), ("b", 2)]);
}

#[tokio::test]
async fn a_request_prefers_accounts_whose_listed_models_name_its_model() {
    let stub = relays(vec![
        ("a", Reply::status(200).body("from a")),
        ("b", Reply::status(200).body("from b")),
    ]);
    let state = relay_state(stub.router(), vec![codex("a", None), codex("b", None)]).await;
    state.with_registry(|r| {
        r.record_models(&"a".parse().unwrap(), vec!["gpt-5-codex".to_owned()]);
        r.record_models(&"b".parse().unwrap(), vec!["gpt-5".to_owned()]);
    });
    let addr = serve(state).await;
    assert_eq!(
        post_model(addr, "GPT-5").await.text().await.unwrap(),
        "from b"
    );
    assert_eq!(
        post_model(addr, "GPT-5-CODEX").await.text().await.unwrap(),
        "from a"
    );
    expect_calls(&stub, &[("a", 1), ("b", 1)]);
    let unlisted = post_model(addr, "gpt-5-alias").await;
    assert_eq!(
        unlisted.status(),
        200,
        "a model no account listed still relays"
    );
}

async fn expect_not_found(response: reqwest::Response, provider: Provider, model: &str) {
    assert_eq!(response.status(), 404);
    assert_eq!(response.headers()["content-type"], "application/json");
    let body: Value = response.json().await.unwrap();
    let message = match provider {
        Provider::Codex => {
            assert_eq!(body["error"]["type"], "invalid_request_error");
            assert_eq!(body["error"]["code"], "model_not_found");
            &body["error"]["message"]
        }
        Provider::Claude => {
            assert_eq!(body["type"], "error");
            assert_eq!(body["error"]["type"], "not_found_error");
            &body["error"]["message"]
        }
    };
    assert!(message.as_str().unwrap().contains(model), "{message}");
}

#[tokio::test]
async fn a_model_no_account_allows_is_a_404_before_any_daemon_is_tried() {
    let stub = Stub::new();
    let state = state();
    FakeDaemon::connect(&state, "node", stub.router()).await;
    apply(
        &state,
        report(
            "node",
            vec![
                codex("a", Some(&["gpt-5-codex"])),
                codex("b", Some(&["gpt-5-mini"])),
                claude("c", Some(&["claude-sonnet-*"])),
            ],
        ),
        None,
    )
    .await;
    let addr = serve(state.clone()).await;
    expect_not_found(post_model(addr, "gpt-5").await, Provider::Codex, "gpt-5").await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/claude/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-4","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    expect_not_found(response, Provider::Claude, "claude-opus-4").await;
    assert!(stub.seen().is_empty());

    let records = logged(&state, 2).await;
    assert!(records.iter().all(|record| {
        record.status == 404 && record.attempts == 0 && record.account_id.is_none()
    }));
}

#[tokio::test]
async fn an_account_that_only_lacks_room_keeps_the_503() {
    let stub = relays(vec![("a", Reply::status(200)), ("b", Reply::status(200))]);
    let state = relay_state(
        stub.router(),
        vec![
            codex("a", Some(&["gpt-5"])),
            codex("b", Some(&["gpt-5-mini"])),
        ],
    )
    .await;
    state.with_registry(|r| {
        r.cooldown_account(&"a".parse().unwrap(), Utc::now() + TimeDelta::minutes(1))
    });
    let addr = serve(state.clone()).await;
    let response = post_model(addr, "gpt-5").await;
    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "mesh_unavailable");

    let mut paused = codex("a", Some(&["gpt-5"]));
    paused.admission = Admission::Deny {
        reason: DenyReason::Paused,
    };
    apply(&state, report("node", vec![paused, codex("b", None)]), None).await;
    assert_eq!(post_model(addr, "gpt-5").await.status(), 200);
    assert_eq!(post_model(addr, "other").await.status(), 200);
    assert!(stub.seen().len() == 2);
}

#[tokio::test]
async fn a_model_without_any_account_is_the_usual_503() {
    let state = state();
    let addr = serve(state).await;
    let response = post_model(addr, "gpt-5").await;
    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "mesh: no account has headroom");
}

fn denied_model() -> Reply {
    Reply::status(429).header(DENIED_HEADER, DENIED_MODEL)
}

fn cooldown_of(state: &AppState, id: &str) -> Option<chrono::DateTime<Utc>> {
    state
        .with_registry(|r| r.view(&id.parse().unwrap(), Utc::now()))
        .unwrap()
        .cooldown_until
}

#[tokio::test]
async fn a_daemon_model_refusal_moves_on_without_a_cooldown() {
    let stub = relays(vec![
        ("a", denied_model()),
        ("b", Reply::status(200).body("from b")),
    ]);
    let state = relay_state(stub.router(), vec![codex("a", None), codex("b", None)]).await;
    let addr = serve(state.clone()).await;
    assert_eq!(
        post_model(addr, "gpt-5").await.text().await.unwrap(),
        "from b"
    );
    expect_calls(&stub, &[("a", 1), ("b", 1)]);
    assert_eq!(cooldown_of(&state, "a"), None);
    let view = state
        .with_registry(|r| r.view(&"a".parse().unwrap(), Utc::now()))
        .unwrap();
    assert!(view.admission.is_admit());

    let candidates = state.with_registry(|r| {
        r.candidates(
            Provider::Codex,
            &Caller::default(),
            Some("gpt-5"),
            Utc::now(),
        )
    });
    assert_eq!(
        candidates.len(),
        2,
        "the refusing account is still a candidate"
    );
}

#[tokio::test]
async fn a_refusal_for_another_reason_still_rests_the_account() {
    let stub = relays(vec![
        ("a", denied_model()),
        ("b", denied()),
        ("c", Reply::status(200).body("from c")),
    ]);
    let state = relay_state(
        stub.router(),
        vec![codex("a", None), codex("b", None), codex("c", None)],
    )
    .await;
    let addr = serve(state.clone()).await;
    assert_eq!(
        post_model(addr, "gpt-5").await.text().await.unwrap(),
        "from c"
    );
    assert_eq!(cooldown_of(&state, "a"), None);
    assert!(cooldown_of(&state, "b").is_some());
}

#[tokio::test]
async fn when_every_daemon_refuses_the_model_the_answer_is_a_404() {
    let stub = relays(vec![("a", denied_model()), ("b", denied_model())]);
    let state = relay_state(stub.router(), vec![codex("a", None), codex("b", None)]).await;
    let addr = serve(state.clone()).await;
    expect_not_found(post_model(addr, "gpt-5").await, Provider::Codex, "gpt-5").await;
    expect_calls(&stub, &[("a", 1), ("b", 1)]);
    assert_eq!(cooldown_of(&state, "a"), None);
    assert_eq!(cooldown_of(&state, "b"), None);
    let records = logged(&state, 1).await;
    assert_eq!((records[0].status, records[0].attempts), (404, 2));
}

#[tokio::test]
async fn a_model_the_server_cannot_read_still_gets_a_404_when_the_daemons_refuse_it() {
    let stub = relays(vec![("a", denied_model())]);
    let state = relay_state(stub.router(), vec![codex("a", None)]).await;
    let addr = serve(state).await;
    let response = codex_request(addr)
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .body(ZSTD_EMPTY_OBJECT)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "model_not_found");
    expect_calls(&stub, &[("a", 1)]);
}

#[tokio::test]
async fn a_claude_model_refusal_is_a_not_found_error() {
    let stub = Stub::new();
    stub.on(Method::POST, "/relay/c/v1/messages", denied_model());
    let state = state();
    FakeDaemon::connect(&state, "node", stub.router()).await;
    apply(&state, report("node", vec![claude("c", None)]), None).await;
    let addr = serve(state).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/claude/v1/messages"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-4","messages":[]}"#)
        .send()
        .await
        .unwrap();
    expect_not_found(response, Provider::Claude, "claude-opus-4").await;
    assert_eq!(stub.calls("/relay/c/v1/messages"), 1);
}

#[tokio::test]
async fn a_refusal_for_the_model_and_one_for_room_is_a_503() {
    let stub = relays(vec![("a", denied_model()), ("b", denied())]);
    let state = relay_state(stub.router(), vec![codex("a", None), codex("b", None)]).await;
    let addr = serve(state.clone()).await;
    assert_eq!(post_model(addr, "gpt-5").await.status(), 503);
    assert_eq!(cooldown_of(&state, "a"), None);
    assert!(cooldown_of(&state, "b").is_some());
}

#[tokio::test]
async fn the_codex_models_etag_is_not_relayed() {
    let stub = relays(vec![(
        "a",
        Reply::status(200)
            .header("x-models-etag", "\"upstream\"")
            .header("x-other", "kept")
            .body("served"),
    )]);
    stub.on(
        Method::POST,
        "/relay/c/v1/messages",
        Reply::status(200).header("x-models-etag", "\"claude\""),
    );
    let state = state();
    FakeDaemon::connect(&state, "node", stub.router()).await;
    apply(
        &state,
        report("node", vec![codex("a", None), claude("c", None)]),
        None,
    )
    .await;
    let addr = serve(state).await;
    let response = send_codex(addr, "x").await;
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("x-models-etag"));
    assert_eq!(response.headers()["x-other"], "kept");
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/claude/v1/messages"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-models-etag"], "\"claude\"");
}

#[tokio::test]
async fn a_model_that_is_not_a_string_does_not_stop_a_request() {
    let stub = relays(vec![("a", Reply::status(200).body("served"))]);
    let state = relay_state(stub.router(), vec![codex("a", Some(&["gpt-5*"]))]).await;
    let addr = serve(state).await;
    assert_eq!(
        post_model(addr, "gpt-5-codex").await.text().await.unwrap(),
        "served"
    );
    let response = codex_request(addr)
        .header("content-type", "application/json")
        .body(json!({"input": [], "model": 5}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "served");
}

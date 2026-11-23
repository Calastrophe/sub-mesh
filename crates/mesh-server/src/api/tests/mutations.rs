use super::*;
use crate::registry::Caller;
use crate::testing::Seen;

/// The account `a` as the daemon reports it after a change.
fn changed(f: impl FnOnce(&mut AccountReport)) -> AccountReport {
    let mut report = claude_account("a");
    f(&mut report);
    report
}

fn paused_report() -> AccountReport {
    changed(|report| {
        report.paused = true;
        report.admission = Admission::Deny {
            reason: DenyReason::Paused,
        };
    })
}

async fn api_for(stub: &Arc<Stub>) -> (Arc<AppState>, Api) {
    let state = strict_state();
    connect_alice(&state, stub).await;
    two_owners(&state).await;
    let api = Api::new(&state).await;
    (state, api)
}

const PAUSE: &str = "/control/v1/accounts/a/pause";
const RESUME: &str = "/control/v1/accounts/a/resume";
const RESERVE: &str = "/control/v1/accounts/a/reserve";
const MODELS: &str = "/control/v1/accounts/a/models";
const ACCOUNT: &str = "/control/v1/accounts/a";

fn on_behalf_of(seen: &Seen) -> &str {
    seen.headers[ON_BEHALF_HEADER].to_str().unwrap()
}

#[tokio::test]
async fn setting_a_reserve_calls_the_daemon_and_applies_the_answer_at_once() {
    let stub = Stub::new();
    let reserve = ReserveConfig {
        default: 0.5,
        five_hour: Some(0.6),
        ..ReserveConfig::default()
    };
    let answer = changed(|report| report.reserve = reserve.clone());
    let body = SetReserve {
        reserve: Some(reserve.clone()),
    };
    stub.on(reqwest::Method::PUT, RESERVE, Reply::json(200, &answer));
    let (state, api) = api_for(&stub).await;

    let response = api
        .request(reqwest::Method::PUT, &reserve_path(&id("a")), Some(ALICE))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let updated: Subscription = response.json().await.unwrap();
    assert_eq!(updated.id, id("a"));
    assert_eq!(updated.reserve, reserve);
    assert!(updated.can_manage);
    assert_eq!(updated.owner.as_deref(), Some(ALICE));
    let view = state
        .with_registry(|r| r.view(&id("a"), Utc::now()))
        .unwrap();
    assert_eq!(view.report.reserve, reserve);
    let overview = api.overview(Some(ALICE)).await;
    assert_eq!(subscription(&overview, "a").reserve, reserve);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(on_behalf_of(&seen[0]), ALICE);
    assert_eq!(seen[0].headers["content-type"], "application/json");
    let sent: SetReserve = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(sent, body);
}

#[tokio::test]
async fn clearing_a_reserve_sends_null() {
    let stub = Stub::new();
    stub.on(
        reqwest::Method::PUT,
        RESERVE,
        Reply::json(200, &claude_account("a")),
    );
    let (_state, api) = api_for(&stub).await;
    let response = api
        .request(reqwest::Method::PUT, &reserve_path(&id("a")), Some(ALICE))
        .json(&json!({"reserve": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(sent, json!({"reserve": null}));
}

#[tokio::test]
async fn pause_and_resume_update_the_registry_at_once() {
    let stub = Stub::new();
    stub.on(
        reqwest::Method::POST,
        PAUSE,
        Reply::json(200, &paused_report()),
    );
    stub.on(
        reqwest::Method::POST,
        RESUME,
        Reply::json(200, &claude_account("a")),
    );
    let (state, api) = api_for(&stub).await;
    let candidates = || {
        state
            .with_registry(|r| r.candidates(Provider::Claude, &Caller::default(), None, Utc::now()))
            .into_iter()
            .map(|c| c.id.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(candidates(), ["a"]);

    let paused: Subscription = api
        .request(reqwest::Method::POST, &pause_path(&id("a")), Some(ALICE))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(paused.paused);
    assert_eq!(paused.score, None);
    assert!(!paused.admission.is_admit());
    assert!(candidates().is_empty());

    let resumed: Subscription = api
        .request(reqwest::Method::POST, &resume_path(&id("a")), Some(ALICE))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!resumed.paused);
    assert!(resumed.score.is_some());
    assert_eq!(candidates(), ["a"]);
    let seen = stub.seen();
    let paths: Vec<_> = seen.iter().map(|seen| seen.path.as_str()).collect();
    assert_eq!(paths, [PAUSE, RESUME]);
    assert!(seen.iter().all(|seen| on_behalf_of(seen) == ALICE));
}

#[tokio::test]
async fn deleting_removes_the_subscription_at_once() {
    let stub = Stub::new();
    stub.on(reqwest::Method::DELETE, ACCOUNT, Reply::status(204));
    let (state, api) = api_for(&stub).await;
    let response = api
        .request(
            reqwest::Method::DELETE,
            &subscription_path(&id("a")),
            Some(ALICE),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert!(response.text().await.unwrap().is_empty());
    assert!(
        state
            .with_registry(|r| r.view(&id("a"), Utc::now()))
            .is_none()
    );
    let overview = api.overview(Some(ALICE)).await;
    assert_eq!(overview.subscriptions.len(), 1);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, reqwest::Method::DELETE);
    assert_eq!(on_behalf_of(&seen[0]), ALICE);
}

#[tokio::test]
async fn an_unknown_subscription_is_a_404_and_calls_no_daemon() {
    let stub = Stub::new();
    let (_state, api) = api_for(&stub).await;
    for (verb, suffix) in [
        (reqwest::Method::PUT, "/reserve"),
        (reqwest::Method::PUT, "/models"),
        (reqwest::Method::POST, "/pause"),
        (reqwest::Method::POST, "/resume"),
        (reqwest::Method::DELETE, ""),
    ] {
        for name in ["nope", "not%20an%20id", "..%2Fb"] {
            let response = api
                .request(
                    verb.clone(),
                    &format!("/api/v1/subscriptions/{name}{suffix}"),
                    Some(ALICE),
                )
                .json(&json!({"reserve": null}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 404, "{verb} {name}{suffix}");
            let error: ApiError = response.json().await.unwrap();
            assert_eq!(error.error, "unknown subscription");
        }
    }
    assert!(stub.seen().is_empty());
}

#[tokio::test]
async fn only_the_owner_may_change_a_subscription() {
    let stub = Stub::new();
    let (state, api) = api_for(&stub).await;
    let reserve = json!({"reserve": {"default": 0.9}});
    for login in [Some(BOB), None, Some("alice@example.org")] {
        for (verb, path_) in [
            (reqwest::Method::PUT, reserve_path(&id("a"))),
            (reqwest::Method::PUT, models_path(&id("a"))),
            (reqwest::Method::POST, pause_path(&id("a"))),
            (reqwest::Method::POST, resume_path(&id("a"))),
            (reqwest::Method::DELETE, subscription_path(&id("a"))),
        ] {
            let response = api
                .request(verb.clone(), &path_, login)
                .json(&reserve)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403, "{login:?} {verb} {path_}");
            let error: ApiError = response.json().await.unwrap();
            assert!(error.error.contains("owner"), "{}", error.error);
        }
    }
    assert!(stub.seen().is_empty());
    assert!(
        state
            .with_registry(|r| r.view(&id("a"), Utc::now()))
            .is_some()
    );
}

#[tokio::test]
async fn a_subscription_without_a_known_owner_cannot_be_changed() {
    let state = strict_state();
    apply(&state, report("ownerless", vec![claude_account("o")]), None).await;
    let api = Api::new(&state).await;
    let response = api
        .request(reqwest::Method::POST, &pause_path(&id("o")), Some(ALICE))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn without_whois_the_daemon_is_told_to_act_for_the_owner() {
    let stub = Stub::new();
    stub.on(
        reqwest::Method::POST,
        PAUSE,
        Reply::json(200, &paused_report()),
    );
    let state = lax_state();
    connect_alice(&state, &stub).await;
    two_owners(&state).await;
    let api = Api::new(&state).await;
    let response = api
        .request(reqwest::Method::POST, &pause_path(&id("a")), None)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(on_behalf_of(&seen[0]), ALICE);
}

#[tokio::test]
async fn an_ownerless_development_daemon_gets_a_nonempty_login() {
    let stub = Stub::new();
    stub.on(
        reqwest::Method::POST,
        PAUSE,
        Reply::json(200, &paused_report()),
    );
    let state = lax_state();
    FakeDaemon::connect(&state, "dev", stub.router()).await;
    apply(&state, report("dev", vec![claude_account("a")]), None).await;
    let api = Api::new(&state).await;
    let response = api
        .request(reqwest::Method::POST, &pause_path(&id("a")), None)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(on_behalf_of(&seen[0]), "mesh-development");
}

#[tokio::test]
async fn deleting_requires_a_204_from_the_daemon() {
    let stub = Stub::new();
    stub.on(reqwest::Method::DELETE, ACCOUNT, Reply::status(200));
    let (state, api) = api_for(&stub).await;
    let response = api
        .request(
            reqwest::Method::DELETE,
            &subscription_path(&id("a")),
            Some(ALICE),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(state.with_registry(|registry| registry.view(&id("a"), Utc::now()).is_some()));
}

#[tokio::test]
async fn a_daemon_without_a_tunnel_is_a_502_and_changes_nothing() {
    let state = strict_state();
    two_owners(&state).await;
    let api = Api::new(&state).await;
    let response = api
        .request(reqwest::Method::POST, &pause_path(&id("b")), Some(BOB))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let error: ApiError = response.json().await.unwrap();
    assert!(error.error.contains("bob-box"), "{}", error.error);
    let view = state
        .with_registry(|r| r.view(&id("b"), Utc::now()))
        .unwrap();
    assert!(!view.report.paused);
}

#[tokio::test]
async fn a_daemon_4xx_passes_through_with_its_status_and_message() {
    for (status, answer) in [
        (403, "only the owner"),
        (404, "no such account"),
        (400, "bad reserve"),
    ] {
        let stub = Stub::new();
        stub.on(
            reqwest::Method::POST,
            PAUSE,
            Reply::json(status, &json!({"error": answer})),
        );
        let (state, api) = api_for(&stub).await;
        let response = api
            .request(reqwest::Method::POST, &pause_path(&id("a")), Some(ALICE))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        let error: ApiError = response.json().await.unwrap();
        assert_eq!(error.error, answer);
        let view = state
            .with_registry(|r| r.view(&id("a"), Utc::now()))
            .unwrap();
        assert!(!view.report.paused);
        assert_eq!(stub.seen().len(), 1);
    }
}

#[tokio::test]
async fn a_daemon_4xx_without_an_api_error_body_still_gives_one() {
    let stub = Stub::new();
    stub.on(
        reqwest::Method::DELETE,
        ACCOUNT,
        Reply::status(409).body("busy\n"),
    );
    let (_state, api) = api_for(&stub).await;
    let response = api
        .request(
            reqwest::Method::DELETE,
            &subscription_path(&id("a")),
            Some(ALICE),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let error: ApiError = response.json().await.unwrap();
    assert_eq!(error.error, "busy");
}

#[tokio::test]
async fn a_failing_or_confused_daemon_is_a_502() {
    let cases = [
        ("500", Reply::status(500).body("boom")),
        (
            "redirect",
            Reply::status(302).header("location", "http://elsewhere/"),
        ),
        ("not json", Reply::status(200).body("<html>")),
        ("no body", Reply::status(204)),
        ("other account", Reply::json(200, &claude_account("b"))),
        (
            "other provider",
            Reply::json(200, &changed(|report| report.provider = Provider::Codex)),
        ),
        (
            "oversized answer",
            Reply::status(200).body("x".repeat(2 * 1024 * 1024 + 1)),
        ),
    ];
    for (name, reply) in cases {
        let stub = Stub::new();
        stub.on(reqwest::Method::POST, PAUSE, reply);
        let (state, api) = api_for(&stub).await;
        let response = api
            .request(reqwest::Method::POST, &pause_path(&id("a")), Some(ALICE))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 502, "{name}");
        let error: ApiError = response.json().await.unwrap();
        assert!(error.error.contains("alice-box"), "{name}: {}", error.error);
        let view = state
            .with_registry(|r| r.view(&id("a"), Utc::now()))
            .unwrap();
        assert!(!view.report.paused, "{name}");
    }
}

#[tokio::test]
async fn an_invalid_reserve_body_is_a_400() {
    let stub = Stub::new();
    let (_state, api) = api_for(&stub).await;
    for body in [
        "not json",
        "{\"reserve\": 7}",
        "{\"reserve\": {\"default\": \"high\"}}",
    ] {
        let response = api
            .request(reqwest::Method::PUT, &reserve_path(&id("a")), Some(ALICE))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{body}");
        let error: ApiError = response.json().await.unwrap();
        assert!(error.error.contains("invalid reserve"), "{}", error.error);
    }
    assert!(stub.seen().is_empty());
}

#[tokio::test]
async fn a_slow_daemon_does_not_block_other_calls() {
    let stub = Stub::new();
    stub.on(
        reqwest::Method::POST,
        PAUSE,
        Reply::json(200, &paused_report()).delay(Duration::from_millis(800)),
    );
    let (_state, api) = api_for(&stub).await;
    let slow = api
        .request(reqwest::Method::POST, &pause_path(&id("a")), Some(ALICE))
        .send();
    let quick = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        let overview = api.overview(Some(ALICE)).await;
        (overview, started.elapsed())
    };
    let (slow, (overview, elapsed)) = tokio::join!(slow, quick);
    assert_eq!(slow.unwrap().status(), 200);
    assert_eq!(overview.subscriptions.len(), 2);
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
}

#[tokio::test]
async fn a_page_of_another_website_cannot_change_a_subscription() {
    let stub = Stub::new();
    let (state, api) = api_for(&stub).await;
    let response = api
        .request(reqwest::Method::POST, &pause_path(&id("a")), Some(ALICE))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let error: ApiError = response.json().await.unwrap();
    assert!(error.error.contains("cross-origin"), "{}", error.error);
    assert!(stub.seen().is_empty());
    assert!(
        state
            .with_registry(|r| r.view(&id("a"), Utc::now()))
            .is_some()
    );

    stub.on(
        reqwest::Method::POST,
        PAUSE,
        Reply::json(200, &paused_report()),
    );
    let response = api
        .request(reqwest::Method::POST, &pause_path(&id("a")), Some(ALICE))
        .header("origin", format!("http://{}", api.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response = api
        .request(reqwest::Method::GET, OVERVIEW_PATH, Some(ALICE))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(stub.seen().len(), 1);
}

fn allowlist(patterns: &[&str]) -> ModelAllowlist {
    ModelAllowlist::new(patterns.iter().copied()).unwrap()
}

#[tokio::test]
async fn setting_models_calls_the_daemon_and_applies_the_answer_at_once() {
    let stub = Stub::new();
    let models = allowlist(&["claude-sonnet-*", "claude-haiku-4"]);
    let answer = changed(|report| report.models = Some(models.clone()));
    let body = SetModels {
        models: Some(models.clone()),
    };
    stub.on(reqwest::Method::PUT, MODELS, Reply::json(200, &answer));
    let (state, api) = api_for(&stub).await;
    let before = state.with_registry(|r| r.models_revision());

    let response = api
        .request(reqwest::Method::PUT, &models_path(&id("a")), Some(ALICE))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let updated: Subscription = response.json().await.unwrap();
    assert_eq!(updated.id, id("a"));
    assert_eq!(updated.models, Some(models.clone()));
    assert!(updated.can_manage);
    let view = state
        .with_registry(|r| r.view(&id("a"), Utc::now()))
        .unwrap();
    assert_eq!(view.report.models, Some(models.clone()));
    assert!(state.with_registry(|r| r.models_revision()) > before);
    let overview = api.overview(Some(ALICE)).await;
    assert_eq!(subscription(&overview, "a").models, Some(models));
    assert_eq!(subscription(&overview, "b").models, None);

    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, reqwest::Method::PUT);
    assert_eq!(seen[0].path, MODELS);
    assert_eq!(on_behalf_of(&seen[0]), ALICE);
    assert_eq!(seen[0].headers["content-type"], "application/json");
    let sent: SetModels = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(sent, body);
}

#[tokio::test]
async fn clearing_models_sends_null_and_follows_the_daemon_answer() {
    let stub = Stub::new();
    stub.on(
        reqwest::Method::PUT,
        MODELS,
        Reply::json(200, &claude_account("a")),
    );
    let (state, api) = api_for(&stub).await;
    state.with_registry(|r| {
        let target = r.control_target(&id("a")).unwrap();
        let restricted = changed(|report| report.models = Some(allowlist(&["claude-opus-4"])));
        assert!(r.replace_account(&target, restricted));
    });
    let response = api
        .request(reqwest::Method::PUT, &models_path(&id("a")), Some(ALICE))
        .json(&json!({"models": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let updated: Subscription = response.json().await.unwrap();
    assert_eq!(updated.models, None);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(sent, json!({"models": null}));
}

#[tokio::test]
async fn an_invalid_models_body_is_a_400() {
    let stub = Stub::new();
    let (_state, api) = api_for(&stub).await;
    for body in [
        "not json",
        "{\"models\": 7}",
        "{\"models\": []}",
        "{\"models\": [\"two words\"]}",
        "{\"models\": [\"\"]}",
        "{\"models\": [1]}",
    ] {
        let response = api
            .request(reqwest::Method::PUT, &models_path(&id("a")), Some(ALICE))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{body}");
        let error: ApiError = response.json().await.unwrap();
        assert!(error.error.contains("invalid models"), "{}", error.error);
    }
    assert!(stub.seen().is_empty());
}

#[tokio::test]
async fn a_daemon_4xx_for_models_passes_through() {
    let stub = Stub::new();
    stub.on(
        reqwest::Method::PUT,
        MODELS,
        Reply::json(400, &json!({"error": "bad allowlist"})),
    );
    let (state, api) = api_for(&stub).await;
    let response = api
        .request(reqwest::Method::PUT, &models_path(&id("a")), Some(ALICE))
        .json(&SetModels {
            models: Some(allowlist(&["claude-opus-4"])),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let error: ApiError = response.json().await.unwrap();
    assert_eq!(error.error, "bad allowlist");
    let view = state
        .with_registry(|r| r.view(&id("a"), Utc::now()))
        .unwrap();
    assert_eq!(view.report.models, None);
}

#[tokio::test]
async fn a_page_of_another_website_cannot_change_models() {
    let stub = Stub::new();
    let (_state, api) = api_for(&stub).await;
    let response = api
        .request(reqwest::Method::PUT, &models_path(&id("a")), Some(ALICE))
        .header("origin", "http://evil.example")
        .json(&json!({"models": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert!(stub.seen().is_empty());
}

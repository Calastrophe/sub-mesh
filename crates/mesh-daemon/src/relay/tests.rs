use http::{StatusCode, Uri};
use mesh_core::protocol::{DENIED_HEADER, DENIED_MODEL, HEADROOM_HEADER};
use mesh_core::{Admission, DenyReason, Window};

use super::{
    BodyModel, denied_model_response, denied_response, deny_kind, headroom_headers, mark_limited,
    request_model, split_relay_uri, to_ws_url,
};

fn window(id: &str, used: f64) -> Window {
    Window {
        id: id.to_owned(),
        duration_minutes: None,
        used,
        resets_at: None,
        limited: false,
    }
}

#[test]
fn splits_relay_paths() {
    let uri: Uri = "/relay/claude-1/v1/messages?beta=true".parse().unwrap();
    let (id, path) = split_relay_uri(&uri).unwrap();
    assert_eq!(id, "claude-1");
    assert_eq!(path, "v1/messages?beta=true");

    let uri: Uri = "/relay/claude-1/a%2Fb".parse().unwrap();
    assert_eq!(split_relay_uri(&uri).unwrap().1, "a%2Fb");

    for bad in [
        "/relay/claude-1",
        "/relay/claude-1/",
        "/relay//x",
        "/other/a/b",
    ] {
        let uri: Uri = bad.parse().unwrap();
        assert!(split_relay_uri(&uri).is_none(), "{bad}");
    }
}

#[test]
fn converts_scheme_to_websocket() {
    let url = to_ws_url("https://api.example.com/v1/realtime?x=1".parse().unwrap()).unwrap();
    assert_eq!(url.as_str(), "wss://api.example.com/v1/realtime?x=1");
    let url = to_ws_url("http://localhost:1/x".parse().unwrap()).unwrap();
    assert_eq!(url.scheme(), "ws");
    assert!(to_ws_url("ftp://x/".parse().unwrap()).is_err());
}

#[test]
fn headroom_headers_carry_the_admission() {
    let headers = headroom_headers(&Admission::Admit { headroom: 0.5 });
    assert_eq!(headers[HEADROOM_HEADER], "0.5000");
    let deny = Admission::Deny {
        reason: DenyReason::Paused,
    };
    assert_eq!(headroom_headers(&deny)[HEADROOM_HEADER], "deny");
}

#[test]
fn denial_kinds_and_response() {
    let reserve = DenyReason::Reserve {
        window: "five_hour".to_owned(),
    };
    assert_eq!(deny_kind(&DenyReason::Paused), "paused");
    assert_eq!(deny_kind(&DenyReason::AuthError), "auth_error");
    assert_eq!(deny_kind(&reserve), "reserve");
    assert_eq!(
        deny_kind(&DenyReason::Limited {
            window: "x".to_owned()
        }),
        "limited"
    );
    let response = denied_response(&reserve);
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[DENIED_HEADER], "reserve");
}

#[test]
fn reads_the_top_level_model_only() {
    let model = |body: &str| match request_model(body.as_bytes()) {
        BodyModel::Named(model) => Some(model.into_owned()),
        _ => None,
    };
    assert_eq!(
        model(r#"{"model": "claude-sonnet-5-5", "messages": [{"model": "x"}]}"#).as_deref(),
        Some("claude-sonnet-5-5")
    );
    assert_eq!(
        model(r#"{"messages": [{"model": "inner"}], "model": "gpt-6"}"#).as_deref(),
        Some("gpt-6")
    );
    assert_eq!(
        model(r#"{"model": "gpt\u002d6"}"#).as_deref(),
        Some("gpt-6")
    );
    for absent in [
        "",
        "request body",
        "[]",
        "{}",
        r#"{"model": null}"#,
        r#"{"nested": {"model": "x"}}"#,
    ] {
        assert_eq!(
            request_model(absent.as_bytes()),
            BodyModel::Absent,
            "{absent}"
        );
    }
    for unreadable in [
        r#"{"model": 5}"#,
        r#"{"model": {"name": "x"}}"#,
        r#"{"model": "x""#,
        r#"{"model": "gpt-6", "model": "gpt-6.2-sol"}"#,
    ] {
        assert_eq!(
            request_model(unreadable.as_bytes()),
            BodyModel::Unreadable,
            "{unreadable}"
        );
    }
}

#[tokio::test]
async fn model_refusal_is_a_429_that_names_the_model_only() {
    let response = denied_model_response("claude-opus-5-5");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[DENIED_HEADER], DENIED_MODEL);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["model"], "claude-opus-5-5");
    assert!(body["error"].as_str().unwrap().contains("claude-opus-5-5"));
    assert!(body.get("reason").is_none());
}

#[test]
fn marks_full_windows_limited() {
    let mut windows = vec![window("a", 1.0), window("b", 0.4)];
    mark_limited(&mut windows);
    assert!(windows[0].limited && !windows[1].limited);

    let mut windows = vec![window("a", 0.2), window("b", 0.7)];
    mark_limited(&mut windows);
    assert!(!windows[0].limited && windows[1].limited);

    mark_limited(&mut []);
}

#[tokio::test]
async fn deletion_stops_a_relay_still_uploading() {
    use std::convert::Infallible;
    use std::future;
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::ws::WebSocketUpgrade;
    use axum::extract::{FromRequestParts, Request, State};
    use bytes::Bytes;
    use mesh_core::Provider;
    use tokio::sync::oneshot;

    use super::relay;
    use crate::state::tests::{stored, test_state};

    let state = test_state("relay-delete-upload");
    let account = state.insert_account(stored("codex-1", Provider::Codex));
    let (upload_started, wait_for_upload) = oneshot::channel();
    let stream = futures_util::stream::once(async move {
        upload_started.send(()).unwrap();
        future::pending::<Result<Bytes, Infallible>>().await
    });
    let request = Request::post("/relay/codex-1/v1/messages")
        .body(Body::from_stream(stream))
        .unwrap();
    let (mut parts, body) = request.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &()).await;
    let request = Request::from_parts(parts, body);
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        let (response, ()) = tokio::join!(relay(State(state.clone()), ws, request), async {
            wait_for_upload.await.unwrap();
            state.delete_account(&account.id).await.unwrap();
        },);
        response
    })
    .await
    .expect("relay should finish without waiting for the rest of the upload");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(state.daemon_report().accounts.is_empty());
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[test]
fn reads_the_model_of_a_response_create_message_only() {
    use std::borrow::Cow;

    use super::frames::{Requested, requested_model};

    assert_eq!(
        requested_model(r#"{"type":"response.create","model":"gpt-6","input":[]}"#),
        Requested::Model(Cow::Borrowed("gpt-6"))
    );
    assert_eq!(
        requested_model(r#" {"input":[{"model":"x"}],"model":"gpt-6","type":"response.create"}"#),
        Requested::Model(Cow::Owned("gpt-6".to_owned())),
        "order and escapes do not hide the model"
    );
    assert_eq!(
        requested_model(r#"{"type":"response.interrupt","response_id":"resp_1"}"#),
        Requested::Nothing
    );
    for unreadable in [
        r#"{"type":"response.create","input":[]}"#,
        r#"{"type":"other","model":"gpt-6"}"#,
        "response.create gpt-6",
        "",
        r#"{"type":"response.create","model":"gpt-6","model":"o3"}"#,
        r#"{"type":"response.create","model":5}"#,
        r#"{"type":"response.create","model":"gpt-6""#,
    ] {
        assert_eq!(
            requested_model(unreadable),
            Requested::Unreadable,
            "{unreadable}"
        );
    }
}

#[test]
fn a_disallowed_model_is_answered_and_others_pass() {
    use mesh_core::{ModelAllowlist, Provider};
    use mesh_relay::ClientText;

    use super::frames::check;
    use crate::state::tests::{stored, test_state};

    let state = test_state("relay-frame-check");
    let account = state.insert_account(stored("codex-1", Provider::Codex));
    let create = |model: &str| format!(r#"{{"type":"response.create","model":"{model}"}}"#);
    let duplicate = r#"{"type":"response.create","model":"gpt-6","model":"o3"}"#;
    assert_eq!(
        check(&state, &account, duplicate),
        ClientText::Forward,
        "without an allowlist nothing is parsed"
    );

    state
        .set_account_models(&account.id, Some(ModelAllowlist::new(["gpt-*"]).unwrap()))
        .unwrap();
    for allowed in [
        create("GPT-6"),
        r#"{"type":"response.interrupt"}"#.to_owned(),
    ] {
        assert_eq!(check(&state, &account, &allowed), ClientText::Forward);
    }
    let ClientText::Answer(event) = check(&state, &account, &create("o3")) else {
        panic!("a disallowed model is answered");
    };
    let event: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["status"], 404);
    assert_eq!(event["error"]["code"], "model_not_found");
    assert!(event["error"]["message"].as_str().unwrap().contains("o3"));

    let ClientText::Answer(event) = check(&state, &account, duplicate) else {
        panic!("an unreadable message is answered");
    };
    let event: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(event["status"], 400);
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

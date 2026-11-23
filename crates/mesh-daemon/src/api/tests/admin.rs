use axum::response::IntoResponse;
use mesh_core::{
    AccountId, Admission, ApiError, DenyReason, ModelAllowlist, Provider, ReserveConfig,
};

use axum::Json;
use axum::extract::{Path, State};
use http::StatusCode;
use mesh_core::admin::{SetModels, SetReserve};

use crate::api::{
    AdminReply, admin_delete, admin_pause, admin_resume, admin_set_models, admin_set_reserve,
};
use crate::state::tests::{stored, test_state};

fn failure_status(result: AdminReply) -> StatusCode {
    result.expect_err("expected an error").0
}

#[tokio::test]
async fn pause_resume_and_status() {
    let state = test_state("admin-pause");
    state.insert_account(stored("claude-1", Provider::Claude));

    let Json(status) = admin_pause(State(state.clone()), Path("claude-1".into()))
        .await
        .unwrap();
    assert!(status.accounts[0].paused);
    assert_eq!(
        status.accounts[0].report.admission,
        Admission::Deny {
            reason: DenyReason::Paused
        }
    );
    assert_eq!(status.node, "node");

    let Json(status) = admin_resume(State(state.clone()), Path("claude-1".into()))
        .await
        .unwrap();
    assert!(!status.accounts[0].paused);
    assert!(status.accounts[0].report.admission.is_admit());
    assert_eq!(
        status.accounts[0].report.label.as_deref(),
        Some("me@example.com")
    );

    let missing = admin_pause(State(state.clone()), Path("nope".into())).await;
    assert_eq!(failure_status(missing), StatusCode::NOT_FOUND);
    let invalid = admin_pause(State(state.clone()), Path("a b".into())).await;
    assert_eq!(failure_status(invalid), StatusCode::BAD_REQUEST);
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn reserve_updates() {
    let state = test_state("admin-reserve");
    state.insert_account(stored("codex-1", Provider::Codex));
    let id: AccountId = "codex-1".parse().unwrap();
    let reserve = ReserveConfig {
        default: 0.6,
        ..ReserveConfig::default()
    };

    let body = SetReserve {
        account: None,
        reserve: Some(reserve.clone()),
    };
    let Json(status) = admin_set_reserve(State(state.clone()), Json(body))
        .await
        .unwrap();
    assert_eq!(status.default_reserve, reserve);
    assert_eq!(status.accounts[0].reserve, None);

    let body = SetReserve {
        account: Some(id.clone()),
        reserve: Some(ReserveConfig::default()),
    };
    let Json(status) = admin_set_reserve(State(state.clone()), Json(body))
        .await
        .unwrap();
    assert_eq!(status.accounts[0].reserve, Some(ReserveConfig::default()));

    let body = SetReserve {
        account: Some(id),
        reserve: None,
    };
    let Json(status) = admin_set_reserve(State(state.clone()), Json(body))
        .await
        .unwrap();
    assert_eq!(status.accounts[0].reserve, None);

    let clear_default = SetReserve {
        account: None,
        reserve: None,
    };
    let result = admin_set_reserve(State(state.clone()), Json(clear_default)).await;
    assert_eq!(failure_status(result), StatusCode::BAD_REQUEST);

    let unknown = SetReserve {
        account: Some("zzz".parse().unwrap()),
        reserve: Some(ReserveConfig::default()),
    };
    let result = admin_set_reserve(State(state.clone()), Json(unknown)).await;
    assert_eq!(failure_status(result), StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn models_update() {
    let state = test_state("admin-models");
    state.insert_account(stored("codex-1", Provider::Codex));
    let id: AccountId = "codex-1".parse().unwrap();
    let own = ModelAllowlist::new(["gpt-*", "o3"]).unwrap();

    let body = SetModels {
        account: id.clone(),
        models: Some(own.clone()),
    };
    let Json(status) = admin_set_models(State(state.clone()), Json(body))
        .await
        .unwrap();
    assert_eq!(status.accounts[0].models, Some(own.clone()));
    assert_eq!(status.accounts[0].report.models, Some(own));

    let body = SetModels {
        account: id,
        models: None,
    };
    let Json(status) = admin_set_models(State(state.clone()), Json(body))
        .await
        .unwrap();
    assert_eq!(status.accounts[0].models, None);
    assert_eq!(status.accounts[0].report.models, None);

    let unknown = SetModels {
        account: "zzz".parse().unwrap(),
        models: None,
    };
    let result = admin_set_models(State(state.clone()), Json(unknown)).await;
    assert_eq!(failure_status(result), StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn delete_unknown_returns_admin_error() {
    let state = test_state("admin-delete");
    let response = admin_delete(State(state.clone()), Path("nope".into()))
        .await
        .unwrap_err()
        .into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let body: ApiError = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.error, "no account nope");
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[test]
fn admin_rejects_browser_requests_and_non_loopback_hosts() {
    use http::HeaderMap;

    for (host, browser_header, allowed) in [
        ("127.0.0.1:7072", None, true),
        ("[::1]:7072", None, true),
        ("localhost:7072", None, true),
        ("example.test:7072", None, false),
        ("127.0.0.1:7072", Some("origin"), false),
        ("127.0.0.1:7072", Some("sec-fetch-site"), false),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("host", host.parse().unwrap());
        if let Some(name) = browser_header {
            headers.insert(name, "test".parse().unwrap());
        }
        assert_eq!(crate::api::admin_headers_allowed(&headers), allowed);
    }
}

const TOKEN: &str = "t0ken-t0ken-t0ken-t0ken-t0ken-t0ken-abc";

async fn serve_admin(name: &str) -> (String, std::sync::Arc<crate::state::AppState>) {
    let state = test_state(name);
    state.insert_account(stored("claude-1", Provider::Claude));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = crate::api::admin_router(state.clone(), TOKEN.to_owned());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state)
}

#[tokio::test]
async fn admin_routes_need_the_token() {
    use mesh_core::admin::{RELOAD_PATH, STATUS_PATH, pause_path};

    let (url, state) = serve_admin("admin-token").await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let id: AccountId = "claude-1".parse().unwrap();
    let routes = [
        (http::Method::GET, STATUS_PATH.to_owned()),
        (http::Method::POST, RELOAD_PATH.to_owned()),
        (http::Method::POST, pause_path(&id)),
        (http::Method::DELETE, mesh_core::admin::account_path(&id)),
    ];
    let wrong = [
        None,
        Some(String::new()),
        Some("Bearer".to_owned()),
        Some("Bearer ".to_owned()),
        Some(format!("Bearer {TOKEN}x")),
        Some(format!("Bearer {}", &TOKEN[1..])),
        Some(format!("bearer {TOKEN}")),
        Some(format!("Basic {TOKEN}")),
        Some(TOKEN.to_owned()),
    ];
    for (method, path) in &routes {
        for header in &wrong {
            let mut request = client.request(method.clone(), format!("{url}{path}"));
            if let Some(header) = header {
                request = request.header(http::header::AUTHORIZATION, header);
            }
            let response = request.send().await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path} {header:?}"
            );
            let body: ApiError = response.json().await.unwrap();
            assert!(!body.error.contains(TOKEN), "{}", body.error);
        }
    }
    assert!(
        state.account(&id).is_some(),
        "rejected calls change nothing"
    );

    let response = client
        .get(format!("{url}{STATUS_PATH}"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = client
        .post(format!("{url}{}", pause_path(&id)))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let status: mesh_core::admin::AdminStatus = response.json().await.unwrap();
    assert!(status.accounts[0].paused);

    let browser = client
        .get(format!("{url}{STATUS_PATH}"))
        .bearer_auth(TOKEN)
        .header(http::header::ORIGIN, "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(browser.status(), StatusCode::FORBIDDEN);
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[test]
fn token_is_created_once_in_the_data_dir() {
    let state = test_state("admin-token-file");
    let path = state.settings.admin_token_path();
    assert_eq!(path.file_name().unwrap(), mesh_core::admin::TOKEN_FILE);
    assert_eq!(path.parent().unwrap(), state.settings.data_dir);
    let token = mesh_providers::read_or_create_secret(&path).unwrap();
    assert_eq!(mesh_providers::read_or_create_secret(&path).unwrap(), token);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

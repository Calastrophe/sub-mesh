use std::time::Duration;

use http::{Method, StatusCode};
use mesh_core::api::{SetModels, SetReserve};
use mesh_core::protocol::{CONTROL_PREFIX, ON_BEHALF_HEADER};
use mesh_core::{
    AccountReport, Admission, ApiError, DenyReason, ModelAllowlist, Provider, ReserveConfig,
};
use mesh_providers::TokenStore;
use std::sync::Arc;

use crate::state::AppState;
use crate::tunnel::testing::{Call, Reply, TestServer, Tunnel, daemon_state};

const OWNER: &str = "alice@example.com";

struct Control {
    state: Arc<AppState>,
    store: TokenStore,
    tunnel: Tunnel,
}

impl Control {
    async fn start(name: &str, extra: &str) -> Self {
        let server = TestServer::start().await;
        let state = daemon_state(name, &server.url, extra, "", true);
        state.set_owner(Some(OWNER.to_owned()));
        let (tunnel, store) = Tunnel::with_account(&state, server, Provider::Codex).await;
        Self {
            state,
            store,
            tunnel,
        }
    }

    fn call(&self, method: Method, path: &str, login: Option<&str>) -> Call {
        let call = self
            .tunnel
            .request(method, &format!("{CONTROL_PREFIX}/accounts/{path}"));
        match login {
            Some(login) => call.header(ON_BEHALF_HEADER, login),
            None => call,
        }
    }

    async fn reserve(&self, id: &str, body: &SetReserve, login: Option<&str>) -> Reply {
        self.call(Method::PUT, &format!("{id}/reserve"), login)
            .json(body)
            .send()
            .await
    }

    async fn models(&self, id: &str, body: &SetModels, login: Option<&str>) -> Reply {
        self.call(Method::PUT, &format!("{id}/models"), login)
            .json(body)
            .send()
            .await
    }

    async fn post(&self, path: &str, login: Option<&str>) -> Reply {
        self.call(Method::POST, path, login).send().await
    }

    /// True when a prompt report was requested since the last check.
    async fn report_requested(&self) -> bool {
        tokio::time::timeout(Duration::from_millis(100), self.state.wait_changed())
            .await
            .is_ok()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state.settings.data_dir);
    }
}

async fn error_of(response: Reply) -> String {
    response.json::<ApiError>().await.error
}

#[tokio::test]
async fn pause_and_resume_return_the_report() {
    let control = Control::start("control-pause", "").await;
    control.report_requested().await;

    let response = control.post("codex-1/pause", Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let report: AccountReport = response.json().await;
    assert!(report.paused);
    assert_eq!(
        report.admission,
        Admission::Deny {
            reason: DenyReason::Paused
        }
    );
    assert!(control.report_requested().await);

    let report: AccountReport = control
        .post("codex-1/resume", Some(OWNER))
        .await
        .json()
        .await;
    assert!(!report.paused);
    assert!(report.admission.is_admit());
    assert!(control.report_requested().await);
}

#[tokio::test]
async fn reserve_is_set_and_cleared() {
    let control = Control::start("control-reserve", "").await;
    let own = ReserveConfig {
        default: 0.7,
        ..ReserveConfig::default()
    };
    let body = SetReserve {
        reserve: Some(own.clone()),
    };
    let response = control.reserve("codex-1", &body, Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let report: AccountReport = response.json().await;
    assert_eq!(report.reserve, own);

    let clear = SetReserve { reserve: None };
    let report: AccountReport = control
        .reserve("codex-1", &clear, Some(OWNER))
        .await
        .json()
        .await;
    assert_eq!(report.reserve, control.state.settings.reserve);

    let invalid = SetReserve {
        reserve: Some(ReserveConfig {
            default: 1.5,
            ..ReserveConfig::default()
        }),
    };
    let response = control.reserve("codex-1", &invalid, Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(error_of(response).await.contains("outside"));

    let response = control
        .call(Method::PUT, "codex-1/reserve", Some(OWNER))
        .body("not json")
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn models_are_set_and_cleared() {
    let control = Control::start("control-models", "").await;
    control.report_requested().await;
    let own = ModelAllowlist::new(["gpt-*"]).unwrap();
    let body = SetModels {
        models: Some(own.clone()),
    };
    let response = control.models("codex-1", &body, Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let report: AccountReport = response.json().await;
    assert_eq!(report.models, Some(own.clone()));
    assert!(control.report_requested().await);
    assert_eq!(control.state.daemon_report().accounts[0].models, Some(own));

    let clear = SetModels { models: None };
    let report: AccountReport = control
        .models("codex-1", &clear, Some(OWNER))
        .await
        .json()
        .await;
    assert_eq!(report.models, None);

    let response = control
        .call(Method::PUT, "codex-1/models", Some(OWNER))
        .body(r#"{"models": []}"#)
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = control
        .call(Method::PUT, "codex-1/models", Some(OWNER))
        .body(r#"{"models": ["bad pattern"]}"#)
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = control.models("nope", &clear, Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn models_need_the_owner() {
    let control = Control::start("control-models-owner", "").await;
    let body = SetModels {
        models: Some(ModelAllowlist::new(["gpt-*"]).unwrap()),
    };
    for login in [None, Some("bob@example.com")] {
        let response = control.models("codex-1", &body, login).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{login:?}");
    }
    assert_eq!(control.state.daemon_report().accounts[0].models, None);
}

#[tokio::test]
async fn delete_removes_account_and_token_file() {
    let control = Control::start("control-delete", "").await;
    control.report_requested().await;
    let response = control
        .call(Method::DELETE, "codex-1", Some(OWNER))
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(control.state.accounts().is_empty());
    assert!(control.store.load_all().unwrap().is_empty());
    assert!(control.report_requested().await);
    control.state.reload().await.unwrap();
    assert!(control.state.daemon_report().accounts.is_empty());

    let again = control
        .call(Method::DELETE, "codex-1", Some(OWNER))
        .send()
        .await;
    assert_eq!(again.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_or_invalid_account() {
    let control = Control::start("control-unknown", "").await;
    let response = control.post("nope/pause", Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(error_of(response).await.contains("nope"));

    let body = SetReserve { reserve: None };
    let response = control.reserve("nope", &body, Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = control.post("a%20b/pause", Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn wrong_or_missing_owner_is_forbidden() {
    let control = Control::start("control-forbidden", "").await;
    for login in [None, Some(""), Some("   "), Some("bob@example.com")] {
        let response = control.post("codex-1/pause", login).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{login:?}");
        assert!(!error_of(response).await.is_empty());
    }
    let body = SetReserve { reserve: None };
    let response = control
        .reserve("codex-1", &body, Some("bob@example.com"))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = control.call(Method::DELETE, "codex-1", None).send().await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = control.post("nope/pause", Some("bob@example.com")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(control.state.accounts().len(), 1);
    assert!(!control.state.daemon_report().accounts[0].paused);
}

#[tokio::test]
async fn owner_match_ignores_case_and_extra_owners_work() {
    let control = Control::start("control-owners", r#"owners = ["Bob@Example.com"]"#).await;
    for login in ["ALICE@example.COM", "bob@example.com"] {
        let response = control.post("codex-1/pause", Some(login)).await;
        assert_eq!(response.status(), StatusCode::OK, "{login}");
    }
    let response = control
        .post("codex-1/pause", Some("carol@example.com"))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unknown_owner_is_refused_unless_insecure() {
    let control = Control::start("control-no-owner", "").await;
    control.state.set_owner(None);
    let response = control.post("codex-1/pause", Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    drop(control);

    let insecure = Control::start("control-insecure", "insecure_skip_whois = true").await;
    insecure.state.set_owner(None);
    let response = insecure
        .post("codex-1/pause", Some("anyone@example.com"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = insecure.post("codex-1/resume", None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    insecure.state.set_owner(Some(OWNER.to_owned()));
    let response = insecure
        .post("codex-1/resume", Some("anyone@example.com"))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = insecure.post("codex-1/resume", Some(OWNER)).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn control_authorizes_before_reading_the_body() {
    let control = Control::start("control-body-auth", "").await;
    let response = control
        .call(Method::PUT, "codex-1/reserve", None)
        .body(vec![0; 3 * 1024 * 1024])
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_of(response).await, "missing on-behalf-of login");

    let response = control
        .call(Method::POST, "codex-1/pause", Some(OWNER))
        .header(ON_BEHALF_HEADER, "bob@example.com")
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_of(response).await, "multiple on-behalf-of logins");
}

#[tokio::test]
async fn control_and_relay_dispatch_only_under_their_own_prefix() {
    let control = Control::start("control-routing", "").await;
    let response = control
        .tunnel
        .request(
            Method::POST,
            &format!("/relay/missing{CONTROL_PREFIX}/accounts/codex-1/pause"),
        )
        .header(ON_BEHALF_HEADER, OWNER)
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.text().await, "unknown account\n");

    let response = control
        .tunnel
        .request(
            Method::POST,
            &format!("{CONTROL_PREFIX}/relay/codex-1/v1/messages"),
        )
        .header(ON_BEHALF_HEADER, OWNER)
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!control.state.daemon_report().accounts[0].paused);
}

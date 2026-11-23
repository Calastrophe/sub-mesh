use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use chrono::{TimeDelta, Utc};
use mesh_core::api::{
    Direction, LOGS_PATH, LogEntry, OVERVIEW_PATH, Overview, SetModels, SetReserve, Subscription,
    TokenUsage, Transport, USAGE_PATH, UsageReport, models_path, pause_path, reserve_path,
    resume_path, subscription_path,
};
use mesh_core::protocol::ON_BEHALF_HEADER;
use mesh_core::{
    AccountId, AccountReport, Admission, ApiError, DenyReason, ModelAllowlist, Provider,
    ReserveConfig,
};
use serde_json::json;

use super::routes;
use crate::config::Config;
use crate::db::{Completion, RequestRecord as LoggedRequest};
use crate::peer::Peer;
use crate::testing::{FakeDaemon, Reply, Stub, account, apply, report, serve_app, state_exact};
use crate::{AppState, Arc, SocketAddr};

const LOGIN_HEADER: &str = "x-test-login";
const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";

/// Whois cannot be faked over loopback, so the tests take the caller's login
/// from a header, as the real middleware would take it from whois.
async fn identify(mut request: Request, next: Next) -> Response {
    let login = request
        .headers()
        .get(LOGIN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    request.extensions_mut().insert(Peer {
        node: "test-node".to_owned(),
        login,
    });
    next.run(request).await
}

async fn serve_api(state: Arc<AppState>) -> SocketAddr {
    serve_app(
        routes()
            .layer(middleware::from_fn(identify))
            .with_state(state),
    )
    .await
}

/// A server that checks logins, like production (`insecure_skip_whois` off).
fn strict_state() -> Arc<AppState> {
    state_exact(Config {
        web_dir: None,
        ..Config::default()
    })
}

/// A server in development mode: no whois, so no logins.
fn lax_state() -> Arc<AppState> {
    state_exact(Config {
        insecure_skip_whois: true,
        web_dir: None,
        ..Config::default()
    })
}

struct Api {
    addr: SocketAddr,
}

impl Api {
    async fn new(state: &Arc<AppState>) -> Api {
        Api {
            addr: serve_api(state.clone()).await,
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        login: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let request = reqwest::Client::new().request(method, format!("http://{}{path}", self.addr));
        match login {
            Some(login) => request.header(LOGIN_HEADER, login),
            None => request,
        }
    }

    async fn overview(&self, login: Option<&str>) -> Overview {
        let response = self
            .request(reqwest::Method::GET, OVERVIEW_PATH, login)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }
}

fn id(name: &str) -> AccountId {
    name.parse().unwrap()
}

fn claude_account(name: &str) -> AccountReport {
    AccountReport {
        provider: Provider::Claude,
        label: Some(format!("{name}@mail.example")),
        ..account(name, 0.7)
    }
}

/// Alice's daemon `alice-box` has account `a`, and Bob's `bob-box` has
/// account `b`. Neither has a tunnel unless the caller connects one first.
async fn two_owners(state: &Arc<AppState>) {
    apply(
        state,
        report("alice-box", vec![claude_account("a")]),
        Some(ALICE),
    )
    .await;
    apply(
        state,
        report("bob-box", vec![claude_account("b")]),
        Some(BOB),
    )
    .await;
}

/// Connect `alice-box` to `state` with `stub` as its control API. The
/// tunnel stays up until the test ends.
async fn connect_alice(state: &Arc<AppState>, stub: &Arc<Stub>) {
    let addr = serve_app(
        axum::Router::new()
            .route(
                mesh_core::protocol::TUNNEL_PATH,
                axum::routing::get(
                    |axum::extract::State(state): axum::extract::State<Arc<AppState>>,
                     request: Request| async {
                        crate::tunnel::connect(state, Some(ALICE.to_owned()), request)
                    },
                ),
            )
            .with_state(state.clone()),
    )
    .await;
    FakeDaemon::dial(state, addr, "alice-box", stub.router()).await;
}

fn subscription<'a>(overview: &'a Overview, name: &str) -> &'a Subscription {
    overview
        .subscriptions
        .iter()
        .find(|subscription| subscription.id.as_str() == name)
        .unwrap_or_else(|| panic!("no subscription {name}"))
}

/// The request that `log_request` logs next. Each state has its own
/// database, so the ids only have to grow.
static NEXT_REQUEST: AtomicI64 = AtomicI64::new(1);

fn logged_request(login: Option<&str>, account: Option<&str>, age: TimeDelta) -> LoggedRequest {
    LoggedRequest {
        id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
        ts: Utc::now() - age,
        consumer: "laptop".to_owned(),
        login: login.map(str::to_owned),
        provider: "claude".to_owned(),
        transport: Transport::Http,
        method: "POST".to_owned(),
        path: "v1/messages".to_owned(),
        model: None,
        client: None,
        session: None,
        account_id: account.map(str::to_owned),
        owner: None,
        node: None,
        status: 200,
        latency_ms: 10,
        attempts: 1,
    }
}

async fn store_request(state: &AppState, record: LoggedRequest) {
    state
        .db
        .run(move |db| db.record_request(&record, &[]))
        .await
        .unwrap();
}

async fn log_request(state: &AppState, login: Option<&str>, account: Option<&str>, age: TimeDelta) {
    store_request(state, logged_request(login, account, age)).await;
}

/// Log a request that `login` made with account `a`, which `owner` owns.
async fn log_served(state: &AppState, login: Option<&str>, owner: Option<&str>, age: TimeDelta) {
    let mut record = logged_request(login, Some("a"), age);
    record.owner = owner.map(str::to_owned);
    store_request(state, record).await;
}

mod mutations;
mod reads;

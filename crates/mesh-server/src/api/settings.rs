use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use chrono::Utc;
use mesh_core::api::{ListGroup, ListKind, SetSetting, SettingName, SettingsView};

use super::failure::ApiFailure;
use crate::AppState;
use crate::peer::Peer;
use crate::settings::{self, Change, Refusal};

pub struct Admin(pub Peer);

impl FromRequestParts<Arc<AppState>> for Admin {
    type Rejection = ApiFailure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<Peer>()
            .cloned()
            .ok_or_else(|| ApiFailure::forbidden("the caller is not identified"))?;
        if state.peers.is_admin(&peer) {
            Ok(Admin(peer))
        } else {
            Err(ApiFailure::forbidden("only admins can manage settings"))
        }
    }
}

fn refusal(refusal: Refusal) -> ApiFailure {
    match refusal {
        Refusal::Invalid(message) => ApiFailure::bad_request(message),
        Refusal::Missing(message) => ApiFailure::not_found(message),
        Refusal::Fixed(message) => ApiFailure::new(StatusCode::CONFLICT, message),
    }
}

async fn run(
    state: &Arc<AppState>,
    admin: &Peer,
    change: Option<Change>,
) -> Result<Json<SettingsView>, ApiFailure> {
    if let Some(change) = &change {
        tracing::info!(admin = ?admin.login, ?change, "settings change");
    }
    let shared = state.clone();
    let by = admin.login.clone();
    let outcome = state
        .db
        .run(move |db| {
            let mut current = shared.settings.lock();
            let result = match change {
                Some(change) => settings::apply(
                    &shared.config,
                    db,
                    &mut current,
                    change,
                    by.as_deref(),
                    Utc::now(),
                )?,
                None => Ok(false),
            };
            if result == Ok(true) {
                shared.install(&current.effective);
            }
            let requests = db.access_requests()?;
            Ok(result.map(|_| settings::view(&shared.config, &current, requests)))
        })
        .await
        .map_err(|error| {
            tracing::warn!(%error, "settings change failed");
            ApiFailure::internal("cannot store the settings")
        })?;
    outcome.map(Json).map_err(refusal)
}

fn kind(kind: &str) -> Result<ListKind, ApiFailure> {
    kind.parse().map_err(ApiFailure::not_found)
}

fn setting(name: &str) -> Result<SettingName, ApiFailure> {
    name.parse().map_err(ApiFailure::not_found)
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
) -> Result<Json<SettingsView>, ApiFailure> {
    run(&state, &admin, None).await
}

async fn entry(
    state: &Arc<AppState>,
    admin: &Peer,
    group: ListGroup,
    list: &str,
    value: String,
    add: bool,
) -> Result<Json<SettingsView>, ApiFailure> {
    let list = kind(list)?;
    let change = if add {
        Change::Add(group, list, value)
    } else {
        Change::Remove(group, list, value)
    };
    run(state, admin, Some(change)).await
}

pub async fn add_consumer(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path((list, value)): Path<(String, String)>,
) -> Result<Json<SettingsView>, ApiFailure> {
    entry(&state, &admin, ListGroup::Consumers, &list, value, true).await
}

pub async fn remove_consumer(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path((list, value)): Path<(String, String)>,
) -> Result<Json<SettingsView>, ApiFailure> {
    entry(&state, &admin, ListGroup::Consumers, &list, value, false).await
}

pub async fn add_daemon(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path((list, value)): Path<(String, String)>,
) -> Result<Json<SettingsView>, ApiFailure> {
    entry(&state, &admin, ListGroup::Daemons, &list, value, true).await
}

pub async fn remove_daemon(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path((list, value)): Path<(String, String)>,
) -> Result<Json<SettingsView>, ApiFailure> {
    entry(&state, &admin, ListGroup::Daemons, &list, value, false).await
}

pub async fn set_value(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<SettingsView>, ApiFailure> {
    let name = setting(&name)?;
    let body: SetSetting = serde_json::from_slice(&body)
        .map_err(|error| ApiFailure::bad_request(format!("invalid setting: {error}")))?;
    run(&state, &admin, Some(Change::Set(name, body.value))).await
}

pub async fn reset_value(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path(name): Path<String>,
) -> Result<Json<SettingsView>, ApiFailure> {
    let name = setting(&name)?;
    run(&state, &admin, Some(Change::Reset(name))).await
}

pub async fn approve(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path(login): Path<String>,
) -> Result<Json<SettingsView>, ApiFailure> {
    run(&state, &admin, Some(Change::Approve(login))).await
}

pub async fn deny(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path(login): Path<String>,
) -> Result<Json<SettingsView>, ApiFailure> {
    run(&state, &admin, Some(Change::Deny(login))).await
}

pub async fn forget(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Path(login): Path<String>,
) -> Result<Json<SettingsView>, ApiFailure> {
    run(&state, &admin, Some(Change::Forget(login))).await
}

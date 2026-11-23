use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use mesh_core::AccountId;
use mesh_core::api::{SetModels, SetReserve, Subscription};

use super::command::{Command, Outcome};
use super::failure::ApiFailure;
use super::overview;
use crate::AppState;
use crate::peer::Peer;
use crate::registry::ControlTarget;

/// `PUT /api/v1/subscriptions/{id}/reserve`
pub async fn set_reserve(
    State(state): State<Arc<AppState>>,
    Extension(peer): Extension<Peer>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Subscription>, ApiFailure> {
    let target = manageable(&state, &peer, &id)?;
    let body: SetReserve = serde_json::from_slice(&body)
        .map_err(|error| ApiFailure::bad_request(format!("invalid reserve: {error}")))?;
    update(&state, &peer, &target, Command::SetReserve(body)).await
}

/// `PUT /api/v1/subscriptions/{id}/models`
pub async fn set_models(
    State(state): State<Arc<AppState>>,
    Extension(peer): Extension<Peer>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Subscription>, ApiFailure> {
    let target = manageable(&state, &peer, &id)?;
    let body: SetModels = serde_json::from_slice(&body)
        .map_err(|error| ApiFailure::bad_request(format!("invalid models: {error}")))?;
    update(&state, &peer, &target, Command::SetModels(body)).await
}

/// `POST /api/v1/subscriptions/{id}/pause`
pub async fn pause(
    State(state): State<Arc<AppState>>,
    Extension(peer): Extension<Peer>,
    Path(id): Path<String>,
) -> Result<Json<Subscription>, ApiFailure> {
    let target = manageable(&state, &peer, &id)?;
    update(&state, &peer, &target, Command::Pause).await
}

/// `POST /api/v1/subscriptions/{id}/resume`
pub async fn resume(
    State(state): State<Arc<AppState>>,
    Extension(peer): Extension<Peer>,
    Path(id): Path<String>,
) -> Result<Json<Subscription>, ApiFailure> {
    let target = manageable(&state, &peer, &id)?;
    update(&state, &peer, &target, Command::Resume).await
}

/// `DELETE /api/v1/subscriptions/{id}`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Extension(peer): Extension<Peer>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let target = manageable(&state, &peer, &id)?;
    Command::Delete
        .send(&state, &target, on_behalf_of(&state, &peer, &target))
        .await?;
    if !state.with_registry(|registry| registry.remove_account(&target)) {
        return Err(target_changed());
    }
    tracing::info!(account = %target.id, login = ?peer.login, "subscription deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// The account's daemon, when the caller may change the account.
/// A name that cannot be an account id is as unknown as any other.
fn manageable(state: &AppState, peer: &Peer, id: &str) -> Result<ControlTarget, ApiFailure> {
    let unknown = || ApiFailure::not_found("unknown subscription");
    let id: AccountId = id.parse().map_err(|_| unknown())?;
    let target = state
        .with_registry(|registry| registry.control_target(&id))
        .ok_or_else(unknown)?;
    if !can_manage(
        peer.login.as_deref(),
        target.owner.as_deref(),
        state.config.insecure_skip_whois,
    ) {
        return Err(ApiFailure::forbidden(
            "only the owner of a subscription may change it",
        ));
    }
    Ok(target)
}

/// The login the daemon is told to act for: the caller's. Without whois
/// (development only) the caller has no login, so the daemon is told the
/// owner's, which the server has already decided the caller may act as.
fn on_behalf_of<'a>(
    state: &AppState,
    peer: &'a Peer,
    target: &'a ControlTarget,
) -> Option<&'a str> {
    peer.login.as_deref().or(target.owner.as_deref()).or(state
        .config
        .insecure_skip_whois
        .then_some("mesh-development"))
}

/// Send an update to the daemon, apply its answer to the registry at once,
/// and return the subscription as it now stands.
async fn update(
    state: &AppState,
    peer: &Peer,
    target: &ControlTarget,
    command: Command,
) -> Result<Json<Subscription>, ApiFailure> {
    let outcome = command
        .send(state, target, on_behalf_of(state, peer, target))
        .await?;
    let Outcome::Report(report) = outcome else {
        return Err(ApiFailure::internal("the daemon removed the account"));
    };
    let now = Utc::now();
    let view = state.with_registry(|registry| {
        if registry.replace_account(target, *report) {
            registry.view(&target.id, now)
        } else {
            None
        }
    });
    let view = view.ok_or_else(target_changed)?;
    let activity = overview::activity(state, now).await;
    Ok(Json(overview::subscription(view, &activity, true)))
}

fn target_changed() -> ApiFailure {
    ApiFailure::new(
        StatusCode::CONFLICT,
        "subscription changed while the command was in progress",
    )
}

/// Whether `caller` may change a subscription that belongs to `owner`.
///
/// Logins compare ignoring ASCII case. A caller or an owner that is not
/// known never matches. The exception is `insecure_skip_whois`, which has
/// no identities at all: a caller with no login may then change everything.
pub(super) fn can_manage(
    caller: Option<&str>,
    owner: Option<&str>,
    insecure_skip_whois: bool,
) -> bool {
    match (caller, owner) {
        (Some(caller), Some(owner)) => caller.eq_ignore_ascii_case(owner),
        (None, _) => insecure_skip_whois,
        (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::can_manage;

    #[test]
    fn only_owners_and_anonymous_development_callers_can_manage() {
        let alice = Some("a@example.com");
        let bob = Some("b@example.com");
        for insecure in [false, true] {
            assert!(can_manage(Some("A@Example.com"), alice, insecure));
            assert!(!can_manage(bob, alice, insecure));
            assert!(!can_manage(alice, None, insecure));
            assert_eq!(can_manage(None, alice, insecure), insecure);
            assert_eq!(can_manage(None, None, insecure), insecure);
        }
    }
}

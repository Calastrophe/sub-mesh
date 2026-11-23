//! Model checks on the client messages of a relayed Codex WebSocket.
//!
//! Codex asks for a model in each `response.create` message, and one
//! connection may switch models between them. The server routes a socket by
//! its first message only, so the daemon checks every `response.create`
//! against the account's allowlist as it passes. A refused message never
//! goes upstream: the client gets the error event that makes Codex stop
//! without retrying, and the connection stays open.

use std::borrow::Cow;
use std::sync::Arc;

use chrono::Utc;
use mesh_core::{Admission, Provider, model_allowed};
use mesh_relay::{ClientText, FrameHooks};
use serde::Deserialize;
use serde_json::json;

use crate::state::{AccountState, AppState};

/// The `type` of the client message that asks for a model.
const RESPONSE_CREATE: &str = "response.create";

/// The hooks for a socket of `account`: a model check on Codex client
/// messages, and nothing for other providers.
pub(super) fn hooks(state: &Arc<AppState>, account: &Arc<AccountState>) -> FrameHooks {
    if account.provider != Provider::Codex {
        return FrameHooks::default();
    }
    let state = state.clone();
    let account = account.clone();
    FrameHooks::default().on_client_text(move |text| {
        match state.current_admission(&account, Utc::now()) {
            Some(Admission::Admit { .. }) => check(&state, &account, text),
            Some(Admission::Deny { reason }) => ClientText::Answer(
                json!({
                    "type": "error",
                    "status": 429,
                    "error": {"message": format!("account refused: {}", super::deny_kind(&reason))},
                })
                .to_string(),
            ),
            None => ClientText::Answer(
                json!({
                    "type": "error",
                    "status": 404,
                    "error": {"message": "unknown account"},
                })
                .to_string(),
            ),
        }
    })
}

/// Forward `text` unless it is a `response.create` for a model that the
/// account does not allow. Without an allowlist nothing is parsed.
pub(super) fn check(state: &AppState, account: &AccountState, text: &str) -> ClientText {
    let Some(allowlist) = state.effective_models(account) else {
        return ClientText::Forward;
    };
    match requested_model(text) {
        Requested::Nothing => ClientText::Forward,
        Requested::Model(model) if model_allowed(Some(&allowlist), &model) => ClientText::Forward,
        Requested::Model(model) => {
            tracing::info!(account = %account.id, %model, "WebSocket model not allowed");
            ClientText::Answer(model_not_found_event(&model))
        }
        Requested::Unreadable => {
            tracing::info!(account = %account.id, "unreadable WebSocket message refused");
            ClientText::Answer(unreadable_event())
        }
    }
}

/// What a client message asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Requested<'a> {
    /// An interrupt that cannot start a model request.
    Nothing,
    /// A `response.create` for this model.
    Model(Cow<'a, str>),
    /// A JSON object whose `type` and `model` cannot be read, for example
    /// because a key repeats. Upstream may read it differently, so it is
    /// refused rather than let through unchecked.
    Unreadable,
}

/// Read the model of a Codex client message. An interrupt needs no model.
pub(crate) fn requested_model(text: &str) -> Requested<'_> {
    #[derive(Deserialize)]
    struct Probe<'a> {
        #[serde(rename = "type", borrow)]
        kind: Option<Cow<'a, str>>,
        #[serde(borrow)]
        model: Option<Cow<'a, str>>,
    }

    if !text.trim_ascii_start().starts_with('{') {
        return Requested::Unreadable;
    }
    match serde_json::from_str::<Probe<'_>>(text) {
        Ok(Probe {
            kind: Some(kind),
            model: Some(model),
        }) if kind == RESPONSE_CREATE => Requested::Model(model),
        Ok(Probe {
            kind: Some(kind), ..
        }) if kind == "response.interrupt" => Requested::Nothing,
        _ => Requested::Unreadable,
    }
}

/// The error event for a model that the account does not allow. Codex reads
/// `model_not_found` as a request it must not retry.
pub(crate) fn model_not_found_event(model: &str) -> String {
    json!({
        "type": "error",
        "status": 404,
        "error": {
            "type": "invalid_request_error",
            "code": "model_not_found",
            "message": format!("account refused: model {model:?} is not allowed"),
        },
    })
    .to_string()
}

/// The error event for a client message whose model cannot be read.
fn unreadable_event() -> String {
    json!({
        "type": "error",
        "status": 400,
        "error": {
            "type": "invalid_request_error",
            "message": "mesh: cannot read the type and model of the message",
        },
    })
    .to_string()
}

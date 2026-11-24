use chrono::Utc;
use leptos::ev::SubmitEvent;
use leptos::prelude::*;
use leptos::task::spawn_local;
use mesh_core::api::{MAX_ACCESS_NOTE, RequestState};

use crate::api;
use crate::components::Logo;
use crate::format::ago;
use crate::mock;
use crate::state::{ToastKind, use_store};

#[component]
pub fn AccessPage() -> impl IntoView {
    let store = use_store();
    let note = RwSignal::new(String::new());
    let sending = RwSignal::new(false);
    let login = move || {
        store.session.with(|session| {
            session
                .as_ref()
                .and_then(|session| session.login.clone())
                .unwrap_or_default()
        })
    };
    let node = move || {
        store
            .session
            .with(|session| session.as_ref().map(|session| session.node.clone()))
            .unwrap_or_default()
    };
    let request = move || {
        store
            .session
            .with(|session| session.as_ref().and_then(|session| session.request.clone()))
    };
    let submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        if sending.get_untracked() {
            return;
        }
        sending.set(true);
        let text = note.get_untracked();
        let text = (!text.trim().is_empty()).then_some(text);
        spawn_local(async move {
            let result = api::request_access(text).await;
            if sending.try_set(false).is_some() {
                return;
            }
            match result {
                Ok(session) => store.set_session(session),
                Err(error) => store.toast(ToastKind::Error, error.to_string()),
            }
        });
    };
    let body = move || match request() {
        None => view! {
            <form class="access-form" on:submit=submit>
                <label class="field">
                    <span>"Note"</span>
                    <textarea
                        class="model-patterns"
                        rows="3"
                        maxlength=MAX_ACCESS_NOTE.to_string()
                        placeholder="Optional"
                        prop:value=move || note.get()
                        on:input=move |ev| note.set(event_target_value(&ev))
                    ></textarea>
                </label>
                <button class="btn btn-primary" type="submit" prop:disabled=move || sending.get()>
                    "Request access"
                </button>
            </form>
        }
        .into_any(),
        Some(request) => {
            let (class, text) = match request.state {
                RequestState::Pending => ("badge badge-paused", "Pending"),
                RequestState::Denied => ("badge badge-denied", "Denied"),
            };
            view! {
                <div class="access-state">
                    <span class=class>{text}</span>
                    <span class="muted">{format!("Requested {}", ago(Utc::now(), request.requested_at))}</span>
                </div>
                {request.note.map(|note| view! { <p class="access-note">{note}</p> })}
            }
            .into_any()
        }
    };
    view! {
        <div class="access">
            {mock::enabled().then(|| view! { <span class="mock-pill">"mock data"</span> })}
            <div class="access-card">
                <div class="brand">
                    <Logo />
                    <div class="brand-text">
                        <strong>"sub-mesh"</strong>
                    </div>
                </div>
                <h1>"Request access"</h1>
                <div class="chip">
                    <span class="avatar">
                        {move || login().chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()}
                    </span>
                    <span class="chip-login">{login}</span>
                    <span class="chip-node">{node}</span>
                </div>
                {body}
            </div>
        </div>
    }
}

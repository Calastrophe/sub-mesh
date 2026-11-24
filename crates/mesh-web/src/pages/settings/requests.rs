use chrono::Utc;
use leptos::prelude::*;
use mesh_core::api::{AccessRequest, RequestState};

use crate::api::{self, Decision};
use crate::components::Table;
use crate::format::{ago, display_label};
use crate::state::use_store;

#[component]
pub fn RequestsCard() -> impl IntoView {
    let store = use_store();
    let requests = move || {
        store.settings.with(|view| {
            view.as_ref()
                .map(|view| view.requests.clone())
                .unwrap_or_default()
        })
    };
    let pending = move || {
        store.settings.with(|view| {
            view.as_ref()
                .map_or(0, mesh_core::api::SettingsView::pending)
        })
    };
    let rows = move || {
        let show = store.show_emails.get();
        requests()
            .into_iter()
            .map(|request| request_row(request, show))
            .collect_view()
    };
    view! {
        <section class="group">
            <div class="group-title">
                "Access requests" <span class="count-pill">{pending}</span>
            </div>
            <Show
                when=move || store.settings.with(|view| view.as_ref().is_some_and(|view| !view.requests.is_empty()))
                fallback=|| view! { <p class="muted settings-empty">"No requests."</p> }
            >
                <Table headers=&["User", "Note", "Requested", "State"] numeric_headers=&[""] class="table-settings">
                    {rows}
                </Table>
            </Show>
        </section>
    }
}

fn request_row(request: AccessRequest, show: bool) -> impl IntoView {
    let store = use_store();
    let now = Utc::now();
    let login = StoredValue::new(request.login.clone());
    let name = display_label(&request.login, show);
    let act = move |decision: Decision| {
        let login = login.get_value();
        let shown = display_label(&login, store.show_emails.get_untracked());
        let message = match decision {
            Decision::Approve => format!("Approved {shown}"),
            Decision::Deny => format!("Denied {shown}"),
            Decision::Forget => format!("Cleared the request of {shown}"),
        };
        store.change_settings(message, api::decide(login, decision), |_| {});
    };
    let busy = move || store.settings_busy.get();
    let (class, state) = match request.state {
        RequestState::Pending => ("badge badge-paused", "pending"),
        RequestState::Denied => ("badge badge-denied", "denied"),
    };
    let decided = request
        .decided_at
        .map(|at| format!(" · {}", ago(now, at)))
        .unwrap_or_default();
    let second = match request.state {
        RequestState::Pending => view! {
            <button class="btn btn-sm" prop:disabled=busy on:click=move |_| act(Decision::Deny)>"Deny"</button>
        }
        .into_any(),
        RequestState::Denied => view! {
            <button class="btn btn-ghost btn-sm" prop:disabled=busy on:click=move |_| act(Decision::Forget)>"Clear"</button>
        }
        .into_any(),
    };
    view! {
        <tr>
            <td class="mono">{name}</td>
            <td class="request-note">{request.note.unwrap_or_else(|| "—".to_owned())}</td>
            <td class="nowrap muted">{ago(now, request.requested_at)}</td>
            <td class="nowrap">
                <span class=class>{state}</span>
                <span class="muted">{decided}</span>
            </td>
            <td class="num">
                <div class="row-actions">
                    <button class="btn btn-primary btn-sm" prop:disabled=busy on:click=move |_| act(Decision::Approve)>
                        "Approve"
                    </button>
                    {second}
                </div>
            </td>
        </tr>
    }
}

use leptos::prelude::*;
use mesh_core::api::ListGroup;

use crate::components::{EmailToggle, PageHeader};
use crate::state::use_store;

mod lists;
mod requests;
mod server;
mod tuning;
mod values;

use lists::AccessListCard;
use requests::RequestsCard;
use server::ServerCard;
use tuning::TuningCard;

#[component]
pub fn SettingsPage() -> impl IntoView {
    let store = use_store();
    let admin = Memo::new(move |_| store.is_admin());
    let loaded = Memo::new(move |_| store.settings.with(Option::is_some));
    let body = move || {
        if !admin.get() {
            return view! { <div class="empty"><p>"Admins only."</p></div> }.into_any();
        }
        if !loaded.get() {
            if store.settings_failed() {
                return view! {
                    <div class="empty">
                        <p>"Could not load the settings."</p>
                        <button class="btn" on:click=move |_| store.refresh_settings()>"Retry"</button>
                    </div>
                }
                .into_any();
            }
            return view! { <div class="empty"><p>"Loading…"</p></div> }.into_any();
        }
        view! {
            <RequestsCard />
            <AccessListCard group=ListGroup::Consumers />
            <AccessListCard group=ListGroup::Daemons />
            <TuningCard />
            <ServerCard />
        }
        .into_any()
    };
    view! {
        <PageHeader title="Settings"><EmailToggle /></PageHeader>
        {body}
    }
}

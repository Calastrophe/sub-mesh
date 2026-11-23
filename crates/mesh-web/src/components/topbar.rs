use leptos::prelude::*;

use crate::format::display_label;
use crate::mock;
use crate::state::use_store;

/// The strip above every page: mock-mode notice and the caller's chip.
#[component]
pub fn TopBar() -> impl IntoView {
    let store = use_store();
    let chip = move || {
        let show = store.show_emails.get();
        store.overview.with(|overview| {
            overview.as_ref().map(|overview| {
                let login = overview
                    .me
                    .login
                    .as_deref()
                    .map_or_else(|| "dev".to_owned(), |login| display_label(login, show));
                let initial = login
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_default();
                let node = overview.me.node.clone();
                view! {
                    <div class="chip">
                        <span class="avatar">{initial}</span>
                        <span class="chip-login">{login}</span>
                        <span class="chip-node">{node}</span>
                    </div>
                }
            })
        })
    };
    view! {
        <div class="topbar">
            {mock::enabled().then(|| view! { <span class="mock-pill">"mock data"</span> })} {chip}
        </div>
    }
}

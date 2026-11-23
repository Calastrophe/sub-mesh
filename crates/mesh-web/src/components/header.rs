use leptos::prelude::*;

use crate::state::use_store;

/// A page title with optional actions on the right.
#[component]
pub fn PageHeader(
    title: &'static str,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! {
        <header class="page-head">
            <div>
                <h1>{title}</h1>
            </div>
            <div class="page-actions">{children.map(|children| children())}</div>
        </header>
    }
}

/// Toggle the visibility of login and subscription labels.
#[component]
pub fn EmailToggle() -> impl IntoView {
    let show = use_store().show_emails;
    view! {
        <button
            class="btn"
            class:active=move || show.get()
            aria-pressed=move || show.get().to_string()
            on:click=move |_| show.update(|show| *show = !*show)
        >
            {move || if show.get() { "Hide emails" } else { "Show emails" }}
        </button>
    }
}

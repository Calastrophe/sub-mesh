use leptos::prelude::*;

/// The mesh mark: three linked nodes.
#[component]
pub fn Logo() -> impl IntoView {
    view! {
        <svg class="logo" width="32" height="32" viewBox="0 0 32 32" aria-hidden="true">
            <rect width="32" height="32" rx="8" class="logo-bg"></rect>
            <path d="M8 22L16 8l8 14z" class="logo-lines"></path>
            <circle cx="16" cy="8" r="3" class="logo-node"></circle>
            <circle cx="8" cy="22" r="3" class="logo-node"></circle>
            <circle cx="24" cy="22" r="3" class="logo-node"></circle>
        </svg>
    }
}

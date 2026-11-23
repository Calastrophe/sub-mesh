use leptos::prelude::*;

use crate::state::{ToastKind, use_store};

/// Error and confirmation toasts, bottom right.
#[component]
pub fn Toasts() -> impl IntoView {
    let store = use_store();
    view! {
        <div class="toasts" aria-live="polite">
            <For
                each=move || store.toasts.get()
                key=|toast| toast.id
                children=move |toast| {
                    let class = match toast.kind {
                        ToastKind::Info => "toast",
                        ToastKind::Error => "toast toast-error",
                    };
                    view! {
                        <div class=class role="status">
                            <span>{toast.text}</span>
                            <button
                                class="toast-close"
                                aria-label="Dismiss"
                                on:click=move |_| store.dismiss(toast.id)
                            >
                                "×"
                            </button>
                        </div>
                    }
                }
            />
        </div>
    }
}

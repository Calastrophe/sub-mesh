use leptos::prelude::*;

use crate::state::use_store;

/// Shows its children once the first overview has arrived, and a loading or
/// error message until then.
#[component]
pub fn Loaded(children: ChildrenFn) -> impl IntoView {
    let store = use_store();
    view! {
        <Show
            when=move || store.overview.with(Option::is_some)
            fallback=move || {
                view! {
                    <div class="empty">
                        <p>{move || store.overview_status()}</p>
                        <Show when=move || store.overview_failed()>
                            <button class="btn" on:click=move |_| store.refresh()>"Retry"</button>
                        </Show>
                    </div>
                }
            }
        >
            {children()}
        </Show>
    }
}

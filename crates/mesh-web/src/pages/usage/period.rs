use leptos::prelude::*;

use crate::state::use_store;
use crate::usage::PERIODS;

/// A segmented control that picks how far back the usage reaches. The
/// server sums, so a change loads the report again.
#[component]
pub fn PeriodFilter() -> impl IntoView {
    let store = use_store();
    view! {
        <div class="segmented" role="group" aria-label="Period">
            {PERIODS
                .into_iter()
                .map(|period| {
                    view! {
                        <button
                            class="segmented-btn"
                            class:active=move || store.usage_hours() == period.hours
                            aria-pressed=move || (store.usage_hours() == period.hours).to_string()
                            on:click=move |_| store.set_usage_hours(period.hours)
                        >
                            {period.label}
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

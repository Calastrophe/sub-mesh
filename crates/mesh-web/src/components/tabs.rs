use leptos::prelude::*;
use mesh_core::Provider;

use crate::summary::Filter;

const TABS: [(&str, Filter); 3] = [
    ("All", Filter::All),
    ("Claude", Filter::Only(Provider::Claude)),
    ("Codex", Filter::Only(Provider::Codex)),
];

/// Tabs that pick which providers a list shows, each with the number of
/// items `count` reports for it.
#[component]
pub fn ProviderTabs(
    filter: RwSignal<Filter>,
    #[prop(into)] count: Callback<Filter, usize>,
) -> impl IntoView {
    view! {
        <nav class="tabs" aria-label="Provider filter">
            {TABS
                .into_iter()
                .map(|(label, tab)| {
                    view! {
                        <button
                            class="tab"
                            class:active=move || filter.get() == tab
                            aria-pressed=move || filter.get() == tab
                            on:click=move |_| filter.set(tab)
                        >
                            {match tab {
                                Filter::All => None,
                                Filter::Only(provider) => {
                                    Some(view! { <span class=format!("pdot pdot-{}", provider.as_str())></span> })
                                }
                            }} <span>{label}</span>
                            <span class="count-pill">{move || count.run(tab)}</span>
                        </button>
                    }
                })
                .collect_view()}
        </nav>
    }
}

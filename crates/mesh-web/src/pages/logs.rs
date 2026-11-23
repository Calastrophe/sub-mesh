mod direction;
mod row;

use std::collections::HashSet;

use leptos::prelude::*;
use mesh_core::api::Direction;

use self::direction::DirectionFilter;
use self::row::LogRow;
use crate::components::{EmailToggle, Glyph, Icon, PageHeader, ProviderTabs, Table};
use crate::logs;
use crate::state::use_store;
use crate::summary::Filter;

/// The signed-in user's requests and those others made with their
/// subscriptions, newest first, with filters and a button to page back.
#[component]
pub fn LogsPage() -> impl IntoView {
    let store = use_store();
    let loaded = store.logs;
    let loading = store.logs_loading;
    let paging = store.logs_paging;
    let failed = move || store.logs_error.get() && !loading.get();
    let expanded = RwSignal::new(HashSet::<i64>::new());
    let matching = move || {
        let (filter, errors_only) = (store.log_filter.get(), store.errors_only.get());
        loaded.with(|entries| {
            entries.as_ref().map(|entries| {
                entries
                    .iter()
                    .filter(|entry| logs::shown(entry, filter, errors_only))
                    .count()
            })
        })
    };
    let rows = move || {
        let (filter, errors_only) = (store.log_filter.get(), store.errors_only.get());
        let entries = loaded.read();
        let entries = entries.as_ref()?;
        Some(
            entries
                .iter()
                .filter(|entry| logs::shown(entry, filter, errors_only))
                .map(|entry| view! { <LogRow entry=entry.clone() expanded=expanded /> })
                .collect_view(),
        )
    };
    view! {
        <PageHeader title="Logs">
            <EmailToggle />
            <button
                class="btn btn-primary"
                class:spinning=move || loading.get()
                prop:disabled=move || loading.get()
                on:click=move |_| {
                    store.refresh_logs();
                    store.refresh();
                }
            >
                <Icon glyph=Glyph::Refresh size=15 />
                "Refresh"
            </button>
        </PageHeader>
        <div class="toolbar">
            <ProviderTabs
                filter=store.log_filter
                count=move |filter: Filter| {
                    loaded
                        .with(|entries| {
                            entries
                                .as_ref()
                                .map_or(
                                    0,
                                    |entries| {
                                        entries.iter().filter(|entry| filter.matches(entry.provider)).count()
                                    },
                                )
                        })
                }
            />
            <div class="toolbar-filters">
                <DirectionFilter />
                <button
                    class="toggle"
                    class:active=move || store.errors_only.get()
                    aria-pressed=move || store.errors_only.get().to_string()
                    on:click=move |_| store.errors_only.update(|only| *only = !*only)
                >
                    "Errors only"
                </button>
            </div>
        </div>
        <Table
            class="table-logs"
            headers=&["Time", "Model", "Request", "Status", "Subscription"]
            numeric_headers=&["Latency", "Duration", "Tokens"]
            footer=Children::to_children(move || view! {
                <Show when=move || loaded.with(|entries| entries.as_ref().is_some_and(Vec::is_empty))>
                    <p class="empty">
                        {move || match store.log_direction() {
                            None => "No requests logged yet.",
                            Some(Direction::Outbound) => "You have not made any requests yet.",
                            Some(Direction::Inbound) => "No one has used your subscriptions yet.",
                        }}
                    </p>
                </Show>
                <Show when=move || {
                    loaded.with(|entries| entries.as_ref().is_some_and(|entries| !entries.is_empty()))
                        && matching() == Some(0)
                }>
                    <p class="empty">"No loaded request matches the filters."</p>
                </Show>
                <Show when=move || loaded.with(Option::is_none) && failed()>
                    <div class="empty">
                        <p>"Could not load your requests."</p>
                        <button class="btn" on:click=move |_| store.refresh_logs()>"Retry"</button>
                    </div>
                </Show>
            })
        >
            {rows}
        </Table>
        <Show when=move || store.logs_more.get() && loaded.with(Option::is_some)>
            <div class="log-more">
                <button
                    class="btn"
                    prop:disabled=move || paging.get()
                    on:click=move |_| store.load_older_logs()
                >
                    {move || if paging.get() { "Loading…" } else { "Load older" }}
                </button>
            </div>
        </Show>
    }
}

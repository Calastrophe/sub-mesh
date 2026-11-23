mod card;
mod period;
mod table;

use std::collections::HashSet;

use leptos::prelude::*;
use mesh_core::Provider;

use self::card::UsageCard;
use self::period::PeriodFilter;
use self::table::UsageTable;
use crate::components::{EmailToggle, Glyph, Icon, Loaded, PageHeader, ProviderTabs};
use crate::state::{Store, use_store};
use crate::summary::{Filter, provider_name};
use crate::usage;

/// How a user is named on this page: `you` for the caller, otherwise the
/// login, masked unless the dashboard shows emails.
fn user_text(store: Store, login: Option<&str>) -> String {
    let show_full = store.show_emails.get();
    store.overview.with(|overview| {
        let me = overview.as_ref().and_then(|o| o.me.login.as_deref());
        usage::user_label(login, me, show_full)
    })
}

/// The line that stands in for the cards when nothing used the
/// subscriptions the filter shows.
fn empty_text(filter: Filter, hours: u32) -> String {
    let whose = match filter {
        Filter::All => "your subscriptions".to_owned(),
        Filter::Only(provider) => format!("your {} subscriptions", provider_name(provider)),
    };
    format!(
        "Nothing used {whose} in the last {}.",
        usage::period_name(hours)
    )
}

fn content(store: Store, expanded: RwSignal<HashSet<(Provider, Option<String>)>>) -> AnyView {
    let filter = store.usage_filter.get();
    let hours = store.usage_hours();
    let shown = store.usage.with(|report| {
        report.as_ref().map(|report| {
            filter
                .providers()
                .filter_map(|provider| usage::provider_usage(&report.rows, provider))
                .collect::<Vec<_>>()
        })
    });
    match shown {
        None if store.usage_error.get() && !store.usage_loading.get() => view! {
            <div class="empty">
                <p>"Could not load usage."</p>
                <button class="btn" on:click=move |_| store.refresh_usage()>"Retry"</button>
            </div>
        }
        .into_any(),
        None => view! {
            <div class="empty">
                <p>"Loading…"</p>
            </div>
        }
        .into_any(),
        Some(shown) if shown.is_empty() => {
            view! { <p class="empty">{empty_text(filter, hours)}</p> }.into_any()
        }
        Some(shown) => {
            let cards = shown
                .iter()
                .map(|usage| view! { <UsageCard usage=usage.clone() /> })
                .collect_view();
            let tables = shown
                .into_iter()
                .map(|usage| {
                    let provider = usage.provider;
                    view! {
                        <section class="group">
                            <h2 class="group-title">
                                <span class=format!("pdot pdot-{}", provider.as_str())></span>
                                {provider_name(provider)}
                            </h2>
                            <UsageTable usage=usage expanded=expanded />
                        </section>
                    }
                })
                .collect_view();
            view! {
                <div class="cards">{cards}</div>
                {tables}
            }
            .into_any()
        }
    }
}

/// What the mesh used of the signed-in user's subscriptions, per user.
#[component]
pub fn UsagePage() -> impl IntoView {
    let store = use_store();
    let loading = store.usage_loading;
    let expanded = RwSignal::new(HashSet::<(Provider, Option<String>)>::new());
    view! {
        <PageHeader title="Usage">
            <EmailToggle />
            <button
                class="btn btn-primary"
                class:spinning=move || loading.get()
                prop:disabled=move || loading.get()
                on:click=move |_| {
                    store.refresh_usage();
                    store.refresh();
                }
            >
                <Icon glyph=Glyph::Refresh size=15 />
                "Refresh"
            </button>
        </PageHeader>
        <Loaded>
            <div class="toolbar">
                <ProviderTabs
                    filter=store.usage_filter
                    count=move |filter: Filter| {
                        store
                            .usage
                            .with(|report| {
                                report.as_ref().map_or(0, |report| usage::user_count(&report.rows, filter))
                            })
                    }
                />
                <div class="toolbar-filters">
                    <PeriodFilter />
                </div>
            </div>
            {move || content(store, expanded)}
        </Loaded>
    }
}

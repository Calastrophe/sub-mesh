mod bar;
mod card;
mod menu;
mod models;
mod reserve;
mod row;
mod status;

use leptos::prelude::*;
use mesh_core::Provider;

use crate::components::{EmailToggle, Glyph, Icon, Loaded, PageHeader, ProviderTabs};
use crate::state::use_store;
use crate::summary::{Filter, provider_name, sorted_ids};

use self::card::ProviderCard;
use self::row::SubscriptionRow;

#[component]
fn ProviderSection(provider: Provider) -> impl IntoView {
    let store = use_store();
    let ids = Memo::new(move |_| {
        store.overview.with(|overview| {
            overview
                .as_ref()
                .map(|overview| sorted_ids(&overview.subscriptions, provider))
                .unwrap_or_default()
        })
    });
    view! {
        <section class="group">
            <h2 class="group-title">
                {provider_name(provider)} <span class="count-pill">{move || ids.with(Vec::len)}</span>
            </h2>
            <div class="sub-list">
                <For
                    each=move || ids.get()
                    key=|id| id.clone()
                    children=move |id| view! { <SubscriptionRow id=id /> }
                />
                <Show when=move || ids.with(Vec::is_empty)>
                    <p class="empty">"No subscriptions yet."</p>
                </Show>
            </div>
        </section>
    }
}

/// Usage of every subscription, by provider.
#[component]
pub fn QuotaPage() -> impl IntoView {
    let store = use_store();
    let providers = move || store.filter.get().providers().collect::<Vec<_>>();
    view! {
        <PageHeader title="Quota">
            <EmailToggle />
            <button
                class="btn btn-primary"
                class:spinning=move || store.loading.get()
                prop:disabled=move || store.loading.get()
                on:click=move |_| store.refresh()
            >
                <Icon glyph=Glyph::Refresh size=15 />
                "Refresh"
            </button>
        </PageHeader>
        <Loaded>
            <ProviderTabs
                filter=store.filter
                count=move |filter: Filter| {
                    store
                        .overview
                        .with(|o| o.as_ref().map_or(0, |o| filter.count(&o.subscriptions)))
                }
            />
            <div class="cards">
                <For
                    each=providers
                    key=|provider| *provider
                    children=|provider| view! { <ProviderCard provider=provider /> }
                />
            </div>
            <For
                each=providers
                key=|provider| *provider
                children=|provider| view! { <ProviderSection provider=provider /> }
            />
        </Loaded>
    }
}

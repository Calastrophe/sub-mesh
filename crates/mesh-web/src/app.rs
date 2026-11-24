use std::time::Duration;

use leptos::ev;
use leptos::prelude::*;

use crate::components::{Sidebar, Toasts, TopBar};
use crate::pages::{
    AccessPage, DaemonsPage, LogsPage, Page, QuotaPage, SettingsPage, UsagePage, UsersPage,
};
use crate::state::{Store, ToastKind, use_store};

/// How often the data reloads by itself.
const AUTO_REFRESH: Duration = Duration::from_secs(15);

fn current_page() -> Page {
    let hash = window().location().hash().unwrap_or_default();
    Page::from_hash(&hash)
}

/// The dashboard shell: sidebar, top bar, the current page and toasts.
#[component]
pub fn App() -> impl IntoView {
    let store = Store::new();
    provide_context(store);

    store.refresh_session();
    match set_interval_with_handle(move || store.refresh_session(), AUTO_REFRESH) {
        Ok(interval) => on_cleanup(move || interval.clear()),
        Err(_) => store.toast(ToastKind::Error, "Could not start auto-refresh.".to_owned()),
    }
    let consumer = Memo::new(move |_| {
        store
            .session
            .with(|session| session.as_ref().map(|session| session.consumer))
    });

    view! {
        {move || match consumer.get() {
            Some(true) => view! { <Dashboard /> }.into_any(),
            Some(false) => view! { <AccessPage /> }.into_any(),
            None => view! { <Starting /> }.into_any(),
        }}
        <Toasts />
    }
}

#[component]
fn Starting() -> impl IntoView {
    let store = use_store();
    view! {
        <div class="empty">
            <p>
                {move || {
                    if store.session_error.get() { "Could not reach the server." } else { "Loading…" }
                }}
            </p>
            <Show when=move || store.session_error.get()>
                <button class="btn" on:click=move |_| store.refresh_session()>"Retry"</button>
            </Show>
        </div>
    }
}

fn refresh_page(store: Store, page: Page) {
    match page {
        Page::Logs => store.refresh_logs(),
        Page::Usage => store.refresh_usage(),
        Page::Settings => store.refresh_settings(),
        _ => {}
    }
}

#[component]
fn Dashboard() -> impl IntoView {
    let store = use_store();

    let page = RwSignal::new(current_page());
    let navigation = window_event_listener(ev::hashchange, move |_| page.set(current_page()));
    on_cleanup(move || navigation.remove());
    let narrow = window()
        .match_media("(max-width: 820px)")
        .ok()
        .flatten()
        .is_some_and(|query| query.matches());
    let collapsed = RwSignal::new(narrow);

    store.refresh();
    Effect::new(move |_| {
        let page = page.get();
        let admin = store.is_admin();
        if page != Page::Settings || admin {
            refresh_page(store, page);
        }
    });
    match set_interval_with_handle(
        move || {
            store.refresh();
            refresh_page(store, page.get_untracked());
        },
        AUTO_REFRESH,
    ) {
        Ok(interval) => on_cleanup(move || interval.clear()),
        Err(_) => store.toast(ToastKind::Error, "Could not start auto-refresh.".to_owned()),
    }

    view! {
        <div class="shell" class:collapsed=move || collapsed.get()>
            <Sidebar page=page collapsed=collapsed />
            <div class="main">
                <TopBar />
                <main class="page">
                    {move || match page.get() {
                        Page::Quota => view! { <QuotaPage /> }.into_any(),
                        Page::Usage => view! { <UsagePage /> }.into_any(),
                        Page::Logs => view! { <LogsPage /> }.into_any(),
                        Page::Daemons => view! { <DaemonsPage /> }.into_any(),
                        Page::Users => view! { <UsersPage /> }.into_any(),
                        Page::Settings => view! { <SettingsPage /> }.into_any(),
                    }}
                </main>
            </div>
        </div>
    }
}

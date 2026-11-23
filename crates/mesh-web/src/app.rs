use std::time::Duration;

use leptos::ev;
use leptos::prelude::*;

use crate::components::{Sidebar, Toasts, TopBar};
use crate::pages::{DaemonsPage, LogsPage, Page, QuotaPage, UsagePage, UsersPage};
use crate::state::{Store, ToastKind};

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
    Effect::new(move |_| match page.get() {
        Page::Logs => store.refresh_logs(),
        Page::Usage => store.refresh_usage(),
        _ => {}
    });
    match set_interval_with_handle(
        move || {
            store.refresh();
            match page.get_untracked() {
                Page::Logs => store.refresh_logs(),
                Page::Usage => store.refresh_usage(),
                _ => {}
            }
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
                    }}
                </main>
            </div>
            <Toasts />
        </div>
    }
}

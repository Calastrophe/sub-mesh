use leptos::prelude::*;

use crate::components::{EmailToggle, Loaded, PageHeader, Table};
use crate::format::display_label;
use crate::state::use_store;

/// Tailnet users who own subscriptions or made requests lately.
#[component]
pub fn UsersPage() -> impl IntoView {
    let store = use_store();
    let rows = move || {
        let show = store.show_emails.get();
        store.overview.with(|overview| {
            let overview = overview.as_ref()?;
            Some(
                overview
                    .users
                    .iter()
                    .map(|user| {
                        let is_me = overview.me.login.as_deref().is_some_and(|login| login.eq_ignore_ascii_case(&user.login));
                        view! {
                            <tr>
                                <td>
                                    {display_label(&user.login, show)}
                                    {is_me.then(|| view! { <span class="badge badge-me">"you"</span> })}
                                </td>
                                <td class="num">{user.subscriptions}</td>
                                <td class="num">{user.requests_last_hour}</td>
                            </tr>
                        }
                    })
                    .collect_view(),
            )
        })
    };
    view! {
        <PageHeader title="Users"><EmailToggle /></PageHeader>
        <Loaded>
            <Table headers=&["User"] numeric_headers=&["Subscriptions", "Requests, last hour"]>{rows}</Table>
        </Loaded>
    }
}

use chrono::Local;
use leptos::prelude::*;

use crate::components::{EmailToggle, Loaded, PageHeader, Table};
use crate::format::{ago, clock};
use crate::logs::login_text;
use crate::state::use_store;

/// The daemons the server knows about.
#[component]
pub fn DaemonsPage() -> impl IntoView {
    let store = use_store();
    let rows = move || {
        let show = store.show_emails.get();
        store.overview.with(|overview| {
            let overview = overview.as_ref()?;
            Some(
                overview
                    .daemons
                    .iter()
                    .map(|daemon| {
                        let subs = overview
                            .subscriptions
                            .iter()
                            .filter(|s| s.node == daemon.node)
                            .count();
                        let (class, text) = if daemon.online {
                            ("badge badge-online", "online")
                        } else {
                            ("badge badge-offline", "offline")
                        };
                        view! {
                            <tr>
                                <td class="mono">{daemon.node.clone()}</td>
                                <td>{login_text(daemon.owner.as_deref(), show)}</td>
                                <td class="nowrap muted">
                                    {daemon
                                        .connected_since
                                        .map(|since| ago(overview.now, since))
                                        .unwrap_or_else(|| "—".to_owned())}
                                </td>
                                <td>
                                    <span class=class>{text}</span>
                                </td>
                                <td class="nowrap">
                                    {ago(overview.now, daemon.last_seen)}
                                    <span class="muted">{format!(" · {}", clock(daemon.last_seen, &Local))}</span>
                                </td>
                                <td class="num">{subs}</td>
                            </tr>
                        }
                    })
                    .collect_view(),
            )
        })
    };
    view! {
        <PageHeader title="Daemons"><EmailToggle /></PageHeader>
        <Loaded>
            <Table headers=&["Node", "Owner", "Tunnel up", "Status", "Last seen"] numeric_headers=&["Subscriptions"]>{rows}</Table>
        </Loaded>
    }
}

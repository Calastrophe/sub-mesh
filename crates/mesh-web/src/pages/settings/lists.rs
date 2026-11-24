use chrono::Utc;
use leptos::ev::SubmitEvent;
use leptos::prelude::*;
use mesh_core::api::{EntrySource, ListEntry, ListGroup, ListKind};

use crate::api;
use crate::components::{Glyph, Icon, Table};
use crate::format::{ago, display_label};
use crate::state::use_store;

fn title(group: ListGroup) -> &'static str {
    match group {
        ListGroup::Consumers => "Consumers",
        ListGroup::Daemons => "Daemons",
    }
}

fn source_badge(source: EntrySource) -> impl IntoView {
    let (class, text) = match source {
        EntrySource::Config => ("badge", "config"),
        EntrySource::Admins => ("badge badge-me", "admin"),
        EntrySource::Web => ("badge badge-cooldown", "web"),
    };
    view! { <span class=class>{text}</span> }
}

#[component]
pub fn AccessListCard(group: ListGroup) -> impl IntoView {
    let store = use_store();
    let rows = move || {
        let show = store.show_emails.get();
        let now = store.now().unwrap_or_else(Utc::now);
        store.settings.with(|view| {
            let list = view.as_ref()?.list(group);
            let own = list.own_login.then(|| {
                view! {
                    <tr>
                        <td class="muted">"this server's login"</td>
                        <td>"login"</td>
                        <td>{source_badge(EntrySource::Config)}</td>
                        <td class="muted">"—"</td>
                        <td class="num"></td>
                    </tr>
                }
            });
            let entries = list
                .logins
                .iter()
                .map(|entry| (ListKind::Logins, entry.clone()))
                .chain(
                    list.tags
                        .iter()
                        .map(|entry| (ListKind::Tags, entry.clone())),
                )
                .map(|(kind, entry)| entry_row(group, kind, entry, show, now))
                .collect_view();
            Some(view! { {own} {entries} })
        })
    };
    let count = move || {
        store.settings.with(|view| {
            view.as_ref().map_or(0, |view| {
                let list = view.list(group);
                list.logins.len() + list.tags.len() + usize::from(list.own_login)
            })
        })
    };
    view! {
        <section class="group">
            <div class="group-title">
                {title(group)} <span class="count-pill">{count}</span>
            </div>
            <Table
                headers=&["Entry", "Kind", "Source", "Added"]
                numeric_headers=&[""]
                class="table-settings"
                footer=Children::to_children(move || view! { <AddEntry group=group /> })
            >
                {rows}
            </Table>
        </section>
    }
}

fn entry_row(
    group: ListGroup,
    kind: ListKind,
    entry: ListEntry,
    show: bool,
    now: chrono::DateTime<Utc>,
) -> impl IntoView {
    let store = use_store();
    let text = match kind {
        ListKind::Logins => display_label(&entry.value, show),
        ListKind::Tags => entry.value.clone(),
    };
    let added = match (&entry.added_at, &entry.added_by) {
        (Some(at), Some(by)) => format!("{} · {}", ago(now, *at), display_label(by, show)),
        (Some(at), None) => ago(now, *at),
        _ => "—".to_owned(),
    };
    let removable = entry.source == EntrySource::Web;
    let remove_label = format!("Remove {text}");
    let value = entry.value.clone();
    let remove = move |_| {
        let value = value.clone();
        let shown = match kind {
            ListKind::Logins => display_label(&value, store.show_emails.get_untracked()),
            ListKind::Tags => value.clone(),
        };
        store.change_settings(
            format!("Removed {shown} from {}", title(group).to_lowercase()),
            api::set_entry(group, kind, value, false),
            |_| {},
        );
    };
    view! {
        <tr>
            <td class="mono">{text}</td>
            <td>{if kind == ListKind::Logins { "login" } else { "tag" }}</td>
            <td>{source_badge(entry.source)}</td>
            <td class="nowrap muted">{added}</td>
            <td class="num">
                {removable
                    .then(|| {
                        view! {
                            <button
                                class="icon-btn"
                                aria-label=remove_label
                                title="Remove"
                                prop:disabled=move || store.settings_busy.get()
                                on:click=remove.clone()
                            >
                                <Icon glyph=Glyph::Trash size=15 />
                            </button>
                        }
                    })}
            </td>
        </tr>
    }
}

#[component]
fn AddEntry(group: ListGroup) -> impl IntoView {
    let store = use_store();
    let kind = RwSignal::new(ListKind::Logins);
    let text = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let add = move |ev: SubmitEvent| {
        ev.prevent_default();
        let list = kind.get_untracked();
        let value = match list.check(&text.get_untracked()) {
            Ok(value) => value,
            Err(message) => {
                error.set(Some(message));
                return;
            }
        };
        let shown = match list {
            ListKind::Logins => display_label(&value, store.show_emails.get_untracked()),
            ListKind::Tags => value.clone(),
        };
        store.change_settings(
            format!("Added {shown} to {}", title(group).to_lowercase()),
            api::set_entry(group, list, value, true),
            move |ok| {
                if ok {
                    text.set(String::new());
                }
            },
        );
    };
    let choice = move |label: &'static str, target: ListKind| {
        view! {
            <button
                class="segmented-btn"
                type="button"
                class:active=move || kind.get() == target
                aria-pressed=move || (kind.get() == target).to_string()
                on:click=move |_| {
                    kind.set(target);
                    error.set(None);
                }
            >
                {label}
            </button>
        }
    };
    view! {
        <form class="add-entry field" on:submit=add>
            <div class="field-input">
                <div class="segmented">{choice("Login", ListKind::Logins)} {choice("Tag", ListKind::Tags)}</div>
                <input
                    class="entry-input"
                    type="text"
                    aria-label="New entry"
                    autocomplete="off"
                    spellcheck="false"
                    placeholder=move || match kind.get() {
                        ListKind::Logins => "user@github",
                        ListKind::Tags => "tag:name",
                    }
                    prop:value=move || text.get()
                    on:input=move |ev| {
                        text.set(event_target_value(&ev));
                        error.set(None);
                    }
                />
                <button
                    class="btn btn-sm"
                    type="submit"
                    prop:disabled=move || store.settings_busy.get() || text.get().trim().is_empty()
                >
                    "Add"
                </button>
            </div>
            {move || error.get().map(|message| view! { <p class="editor-error" role="alert">{message}</p> })}
        </form>
    }
}

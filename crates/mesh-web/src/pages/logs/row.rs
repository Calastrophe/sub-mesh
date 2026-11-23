use std::collections::HashSet;

use chrono::{Local, Utc};
use leptos::prelude::*;
use mesh_core::AccountId;
use mesh_core::api::{LogEntry, TokenUsage, Transport};

use super::direction::DirectionIcon;
use crate::components::{Glyph, Icon};
use crate::format::{self, clock_seconds, full_time, label_text, thousands};
use crate::logs;
use crate::state::{Store, use_store};
use crate::summary::provider_name;

/// Columns of the log table, for the detail row to span.
const COLUMNS: &str = "8";

/// The subscription's label, masked unless `show_full`, or its id when the
/// overview does not list it. `None` when no subscription served the request.
fn subscription_text(store: Store, id: Option<&AccountId>, show_full: bool) -> Option<String> {
    let id = id?;
    Some(
        store
            .with_subscription(id, |sub| label_text(sub, show_full))
            .unwrap_or_else(|| id.to_string()),
    )
}

/// A login, masked unless the dashboard shows emails, for a field that
/// follows the setting.
fn login_field(store: Store, login: Option<String>) -> impl Fn() -> String {
    move || logs::login_text(login.as_deref(), store.show_emails.get())
}

fn or_dash(text: Option<&str>) -> String {
    text.unwrap_or("—").to_owned()
}

/// How long the request lasted, or for a request that has not ended, the
/// time since it started, muted.
fn duration_cell(entry: &LogEntry) -> impl IntoView + use<> {
    match entry.duration_ms {
        Some(millis) => view! { {format::elapsed(millis)} }.into_any(),
        None => {
            let so_far = (Utc::now() - entry.ts)
                .num_milliseconds()
                .max(0)
                .cast_unsigned();
            view! { <span class="muted">{format::elapsed(so_far)}</span> }.into_any()
        }
    }
}

fn tokens_cell(entry: &LogEntry) -> impl IntoView + use<> {
    entry.usage.map(|usage| logs::tokens_text(&usage))
}

fn field(label: &'static str, value: impl IntoView + 'static) -> impl IntoView {
    view! {
        <div class="kv">
            <dt>{label}</dt>
            <dd>{value}</dd>
        </div>
    }
}

fn token_fields(usage: &TokenUsage) -> impl IntoView + use<> {
    let count = |tokens: u64| {
        if tokens == 0 {
            "—".to_owned()
        } else {
            thousands(tokens)
        }
    };
    view! {
        {field("Input tokens", thousands(usage.input))}
        {field("Cache read", count(usage.cache_read))}
        {field("Cache write", count(usage.cache_write))}
        {field("Output tokens", thousands(usage.output))}
        {field("Reasoning", count(usage.reasoning))}
    }
}

/// Every field of an entry, shown under its row.
fn detail(entry: &LogEntry, store: Store) -> impl IntoView + use<> {
    let account = entry.account_id.clone();
    let subscription = move || {
        let text = subscription_text(store, account.as_ref(), store.show_emails.get())?;
        Some(match account.as_ref() {
            Some(id) if id.as_str() != text => format!("{text} ({id})"),
            _ => text,
        })
    };
    let session = entry.session.clone();
    view! {
        <dl class="kv-grid">
            {field("Time", full_time(entry.ts, &Local))}
            {field(
                "Direction",
                view! {
                    <span class="dir-text">
                        <DirectionIcon direction=entry.direction size=13 />
                        {logs::direction_name(entry.direction)}
                    </span>
                },
            )}
            {field("Request id", entry.id.to_string())}
            {field("Request", format!("{} · {}", logs::request_text(entry), logs::transport_name(entry.transport)))}
            {field("Provider", provider_name(entry.provider))}
            {field("Model", or_dash(entry.model.as_deref()))}
            {field("Client", or_dash(entry.client.as_deref()))}
            {field("User", login_field(store, entry.login.clone()))}
            {field("Consumer node", entry.consumer.clone())}
            {field("Owner", login_field(store, entry.owner.clone()))}
            {field(
                "Session",
                view! { <span class="mono wrap">{or_dash(session.as_deref())}</span> },
            )}
            {field(
                "Subscription",
                move || subscription().unwrap_or_else(|| "—".to_owned()),
            )}
            {field("Serving node", or_dash(entry.node.as_deref()))}
            {field("Attempts", entry.attempts.to_string())}
            {field("Responses", entry.responses.to_string())}
            {field("Latency", format::latency(entry.latency_ms))}
            {field("Duration", duration_cell(entry))}
            {field("Status", entry.status.to_string())}
            {match entry.usage.as_ref() {
                Some(usage) => token_fields(usage).into_any(),
                None => field("Tokens", "None reported").into_any(),
            }}
        </dl>
        {entry
            .error
            .clone()
            .map(|error| {
                view! {
                    <div class="log-error-full" role="alert">
                        <span class="muted">"Error"</span>
                        <p>{error}</p>
                    </div>
                }
            })}
    }
}

/// One log entry as a table row, and below it, when open, every field.
#[component]
pub fn LogRow(entry: LogEntry, expanded: RwSignal<HashSet<i64>>) -> impl IntoView {
    let store = use_store();
    let id = entry.id;
    let open = Memo::new(move |_| expanded.with(|open| open.contains(&id)));
    let toggle = move |_| {
        expanded.update(|open| {
            if !open.remove(&id) {
                open.insert(id);
            }
        });
    };
    let account = entry.account_id.clone();
    let level = logs::status_level(&entry);
    let websocket = entry.transport == Transport::Websocket;
    let detail_id = format!("log-{id}");
    let model = or_dash(entry.model.as_deref());
    let error = entry.error.clone();
    let summary = view! {
        <tr
            class="log-row"
            class:open=move || open.get()
            class:has-error=error.is_some()
            on:click=toggle
        >
            <td class="nowrap">
                <div class="row-lead">
                    <DirectionIcon direction=entry.direction />
                    <button
                        class="row-toggle"
                        aria-expanded=move || open.get().to_string()
                        aria-controls=detail_id.clone()
                    >
                        <Icon glyph=Glyph::Chevron size=12 />
                        <span>{clock_seconds(entry.ts, &Local)}</span>
                    </button>
                </div>
            </td>
            <td class="mono cut">
                <span class=format!("pdot pdot-{}", entry.provider.as_str())></span>
                {model.clone()}
            </td>
            <td class="mono cut">
                {logs::request_text(&entry)}
                {websocket.then(|| view! { <span class="tag">"WS"</span> })}
            </td>
            <td class="nowrap">
                <span class=format!("status {}", level.class())>{entry.status}</span>
            </td>
            <td class="mono cut">
                {move || {
                    subscription_text(store, account.as_ref(), store.show_emails.get())
                        .unwrap_or_else(|| "—".to_owned())
                }}
            </td>
            <td class="num nowrap">{format::latency(entry.latency_ms)}</td>
            <td class="num nowrap">{duration_cell(&entry)}</td>
            <td class="num nowrap">{tokens_cell(&entry)}</td>
        </tr>
    };
    let error_row = error.map(|error| {
        view! {
            <tr class="log-errline" on:click=toggle>
                <td colspan=COLUMNS>
                    <div class="log-error">{error.clone()}</div>
                </td>
            </tr>
        }
    });
    let entry = StoredValue::new(entry);
    view! {
        {summary}
        {error_row}
        <Show when=move || open.get()>
            <tr class="log-detail" id=detail_id.clone()>
                <td colspan=COLUMNS>
                    <div class="log-detail-body">{entry.with_value(|entry| detail(entry, store))}</div>
                </td>
            </tr>
        </Show>
    }
}

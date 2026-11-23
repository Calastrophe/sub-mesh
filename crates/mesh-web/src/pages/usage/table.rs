use std::collections::HashSet;

use chrono::{DateTime, Utc};
use leptos::prelude::*;
use mesh_core::api::UsageRow;
use mesh_core::{AccountId, Provider};

use super::user_text;
use crate::components::{Glyph, Icon};
use crate::format::{ago, label_text};
use crate::logs::compact;
use crate::state::{Store, use_store};
use crate::usage::{self, ProviderUsage, UserUsage};

type Open = RwSignal<HashSet<(Provider, Option<String>)>>;

/// The column names, in order. The numbers are right-aligned.
const HEADERS: [&str; 8] = [
    "User",
    "Requests",
    "Input",
    "Output",
    "Cached",
    "Share",
    "Top model",
    "Last used",
];

fn model_text(model: Option<&str>) -> String {
    model.unwrap_or("—").to_owned()
}

fn subscription_text(store: Store, id: &AccountId) -> String {
    let show_full = store.show_emails.get();
    store
        .with_subscription(id, |sub| label_text(sub, show_full))
        .unwrap_or_else(|| id.to_string())
}

fn last_used(store: Store, at: DateTime<Utc>) -> String {
    ago(store.now().unwrap_or_else(Utc::now), at)
}

/// One of a user's subscription and model pairs, under their row.
#[component]
fn PartRow(row: UsageRow, total_spend: f64) -> impl IntoView {
    let store = use_store();
    let share = if total_spend > 0.0 {
        row.spend / total_spend
    } else {
        0.0
    };
    let account = row.account_id.clone();
    view! {
        <tr class="usage-part">
            <td class="cut mono part-name">{move || subscription_text(store, &account)}</td>
            <td class="num">{compact(row.requests)}</td>
            <td class="num">{compact(row.tokens.input)}</td>
            <td class="num">{compact(row.tokens.output)}</td>
            <td class="num">{usage::cached_text(&row.tokens)}</td>
            <td class="num">{usage::share_text(share)}</td>
            <td class="mono cut">{model_text(row.model.as_deref())}</td>
            <td class="num nowrap">{move || last_used(store, row.last_used)}</td>
        </tr>
    }
}

/// A user's row, and below it, when open, one row for each subscription and
/// model they used.
#[component]
fn UserRows(
    provider: Provider,
    total_spend: f64,
    rank: usize,
    user: UserUsage,
    expanded: Open,
) -> impl IntoView {
    let store = use_store();
    let key = (provider, user.login.clone());
    let open = Memo::new({
        let key = key.clone();
        move |_| expanded.with(|open| open.contains(&key))
    });
    let toggle = move |_| {
        expanded.update(|open| {
            if !open.remove(&key) {
                open.insert(key.clone());
            }
        });
    };
    let login = user.login.clone();
    let share = if total_spend > 0.0 {
        user.spend / total_spend
    } else {
        0.0
    };
    let summary = view! {
        <tr class="usage-row" class:open=move || open.get() on:click=toggle>
            <td class="cut">
                <button class="row-toggle" aria-expanded=move || open.get().to_string()>
                    <Icon glyph=Glyph::Chevron size=12 />
                    <span class=format!("swatch {}", usage::share_class(rank))></span>
                    <span class="cut">{move || user_text(store, login.as_deref())}</span>
                </button>
            </td>
            <td class="num">{compact(user.requests)}</td>
            <td class="num">{compact(user.tokens.input)}</td>
            <td class="num">{compact(user.tokens.output)}</td>
            <td class="num">{usage::cached_text(&user.tokens)}</td>
            <td class="num">{usage::share_text(share)}</td>
            <td class="mono cut">{model_text(user.top_model.as_deref())}</td>
            <td class="num nowrap">{move || last_used(store, user.last_used)}</td>
        </tr>
    };
    let rows = StoredValue::new(user.rows);
    view! {
        {summary}
        <Show when=move || open.get()>
            {rows
                .with_value(|rows| {
                    rows.iter()
                        .map(|row| view! { <PartRow row=row.clone() total_spend=total_spend /> })
                        .collect_view()
                })}
        </Show>
    }
}

/// The users of one provider, largest spend first. Opening a user lists
/// what they used by subscription and model.
#[component]
pub fn UsageTable(usage: ProviderUsage, expanded: Open) -> impl IntoView {
    let provider = usage.provider;
    let total_spend = usage.spend;
    let rows = usage
        .users
        .into_iter()
        .enumerate()
        .map(|(rank, user)| {
            view! { <UserRows provider=provider total_spend=total_spend rank=rank user=user expanded=expanded /> }
        })
        .collect_view();
    view! {
        <div class="table-wrap">
            <table class="table table-usage">
                <thead>
                    <tr>
                        {HEADERS
                            .iter()
                            .enumerate()
                            .map(|(index, &name)| {
                                let numeric = matches!(index, 1..=5 | 7);
                                view! { <th class:num=numeric>{name}</th> }
                            })
                            .collect_view()}
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        </div>
    }
}

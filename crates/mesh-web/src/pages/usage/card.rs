use leptos::prelude::*;

use super::user_text;
use crate::format;
use crate::logs::compact;
use crate::state::use_store;
use crate::summary::provider_name;
use crate::usage::{self, ProviderUsage, Slice};

fn requests_text(count: u64) -> String {
    match count {
        1 => "1 request".to_owned(),
        count => format!("{} requests", format::thousands(count)),
    }
}

/// One part of the share bar, as wide as its share of the spend.
#[component]
fn Segment(slice: Slice) -> impl IntoView {
    view! { <span class=format!("share-seg {}", slice.class) style:flex-grow=format!("{:.5}", slice.share)></span> }
}

/// One entry of the legend: the part's color, who it is and the percent.
#[component]
fn LegendItem(slice: Slice) -> impl IntoView {
    let store = use_store();
    let name = move || match slice.others {
        0 => user_text(store, slice.login.as_deref()),
        count => usage::others_label(count),
    };
    view! {
        <li>
            <span class=format!("swatch {}", slice.class)></span>
            <span class="legend-name">{name}</span>
            <span class="pct">{usage::share_text(slice.share)}</span>
        </li>
    }
}

/// What the mesh used of one provider's subscriptions: the tokens, and how
/// the spend splits between users.
#[component]
pub fn UsageCard(usage: ProviderUsage) -> impl IntoView {
    let slices = usage::slices(&usage);
    let tokens = usage.tokens;
    let total = tokens.input.saturating_add(tokens.output);
    let segments = slices
        .iter()
        .cloned()
        .map(|slice| view! { <Segment slice=slice /> })
        .collect_view();
    let legend = slices
        .into_iter()
        .map(|slice| view! { <LegendItem slice=slice /> })
        .collect_view();
    view! {
        <article class="card">
            <div class="card-head">
                <span class=format!("pdot pdot-{}", usage.provider.as_str())></span>
                <strong>{provider_name(usage.provider)}</strong>
                <span class="muted card-count">{requests_text(usage.requests)}</span>
            </div>
            <div class="card-label">"Tokens used"</div>
            <div class="card-big">
                <span class="big">{compact(total)}</span>
                <span class="of">
                    {format!("{} input · {} output", compact(tokens.input), compact(tokens.output))}
                </span>
            </div>
            <div class="share-bar" aria-hidden="true">{segments}</div>
            <ul class="share-legend" aria-label="Share of spend">{legend}</ul>
            <div class="card-note">
                <span class="muted">"Input from cache"</span>
                <strong>{usage::cached_text(&tokens)}</strong>
            </div>
        </article>
    }
}

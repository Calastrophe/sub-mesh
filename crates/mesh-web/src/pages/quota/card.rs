use chrono::Local;
use leptos::prelude::*;
use mesh_core::Provider;

use super::bar::SegmentedBar;
use crate::format;
use crate::state::use_store;
use crate::summary::{Aggregate, provider_name, summarize};

fn plural(count: usize) -> String {
    match count {
        1 => "1 subscription".to_owned(),
        n => format!("{n} subscriptions"),
    }
}

fn used_text(aggregate: &Aggregate) -> String {
    if aggregate.has_data() {
        format::percent(aggregate.used_sum)
    } else {
        "--".to_owned()
    }
}

/// The aggregate of a provider's weekly window.
#[component]
pub fn ProviderCard(provider: Provider) -> impl IntoView {
    let store = use_store();
    move || {
        let (now, summary) = store.overview.with(|overview| {
            let overview = overview.as_ref()?;
            let now = overview.now;
            Some((now, summarize(provider, &overview.subscriptions, now)))
        })?;
        let weekly = summary.weekly;
        let weekly_used = used_text(&weekly);
        let weekly_reset = format::reset_text(now, weekly.earliest_reset, &Local);
        Some(view! {
            <article class="card">
                <div class="card-head">
                    <span class=format!("pdot pdot-{}", provider.as_str())></span>
                    <strong>{provider_name(provider)}</strong>
                    <span class="muted card-count">{plural(summary.subscriptions)}</span>
                </div>
                <div class="card-label">"Weekly limit"</div>
                <div class="card-big">
                    <span class="big">{weekly_used}</span>
                    <span class="of">{format!("of {}00%", summary.subscriptions.max(1))}</span>
                </div>
                <SegmentedBar segments=weekly.segments />
                <div class="card-reset">{weekly_reset}</div>
            </article>
        })
    }
}

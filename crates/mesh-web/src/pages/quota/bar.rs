use chrono::{DateTime, Local, Utc};
use leptos::prelude::*;
use mesh_core::{Window, WindowClass};

use crate::format;
use crate::level::Level;

/// A thin usage bar with an optional tick. `used` is a fraction.
#[component]
pub fn Bar(used: f64, tick: Option<f64>) -> impl IntoView {
    let width = used.clamp(0.0, 1.0) * 100.0;
    view! {
        <div class="bar" role="img" aria-label=match tick {
            Some(cutoff) => format!("{} used, {} mesh cutoff", format::percent(used), format::percent(cutoff)),
            None => format!("{} used", format::percent(used)),
        }>
            <div
                class=format!("bar-fill {}", Level::from_used(used).class())
                style:width=format!("{width}%")
            ></div>
            {tick
                .map(|at| {
                    view! {
                        <span
                            class="bar-tick"
                            style:left=format!("{}%", at.clamp(0.0, 1.0) * 100.0)
                        ></span>
                    }
                })}
        </div>
    }
}

/// One usage window of a subscription: name, used percent, a bar with the
/// reserve cutoff marked, and the reset time.
pub fn window_bar(window: &Window, reserve: f64, now: DateTime<Utc>) -> impl IntoView + use<> {
    let used = window.used_at(now);
    let limited = window.limited_at(now);
    let tick = (reserve > 0.0).then_some(1.0 - reserve);
    view! {
        <div class="win">
            <div class="win-head">
                <span class="win-name">{format::window_label(window)}</span>
                {limited.then(|| view! { <span class="win-limited">"limited"</span> })}
                <span class="win-pct">{format::percent(used)}</span>
            </div>
            <Bar used=used tick=tick />
            <div class="win-foot">{format::reset_text(now, window.resets_at, &Local)}</div>
        </div>
    }
}

/// The cell of a window the subscription does not report: blank for a plan
/// without a 5-hour limit, and a note for a weekly limit not yet known.
pub fn no_window(class: WindowClass) -> impl IntoView {
    let text = match class {
        WindowClass::FiveHour => None,
        WindowClass::Weekly => Some("No weekly data"),
    };
    view! {
        <div class="win win-none">
            {text.map(|text| view! {
                <div class="win-head">
                    <span class="win-name">{text}</span>
                </div>
            })}
        </div>
    }
}

/// One segment per subscription, each filled and colored by its usage. A
/// subscription without the window is an empty segment.
#[component]
pub fn SegmentedBar(segments: Vec<Option<f64>>) -> impl IntoView {
    view! {
        <div class="seg-bar">
            {segments
                .into_iter()
                .map(|used| {
                    let fill = used
                        .map(|used| {
                            view! {
                                <span
                                    class=format!("seg-fill {}", Level::from_used(used).class())
                                    style:width=format!("{}%", used.clamp(0.0, 1.0) * 100.0)
                                ></span>
                            }
                        });
                    view! { <span class="seg">{fill}</span> }
                })
                .collect_view()}
        </div>
    }
}

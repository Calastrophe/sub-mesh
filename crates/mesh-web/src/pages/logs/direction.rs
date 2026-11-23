use leptos::prelude::*;
use mesh_core::api::Direction;

use crate::components::{Glyph, Icon};
use crate::logs;
use crate::state::use_store;

const CHOICES: [(&str, Option<Direction>); 3] = [
    ("All", None),
    ("Outbound", Some(Direction::Outbound)),
    ("Inbound", Some(Direction::Inbound)),
];

/// The arrow that marks which way a request went: up and out for the
/// caller's own, down and in for others using the caller's subscriptions.
#[component]
pub fn DirectionIcon(direction: Direction, #[prop(default = 14)] size: u32) -> impl IntoView {
    let (class, glyph) = match direction {
        Direction::Outbound => ("dir dir-out", Glyph::ArrowUpRight),
        Direction::Inbound => ("dir dir-in", Glyph::ArrowDownLeft),
    };
    view! {
        <span class=class role="img" aria-label=logs::direction_name(direction)>
            <Icon glyph=glyph size=size />
        </span>
    }
}

/// A segmented control that picks which direction the log shows. The server
/// filters, so a change loads the newest page again.
#[component]
pub fn DirectionFilter() -> impl IntoView {
    let store = use_store();
    view! {
        <div class="segmented" role="group" aria-label="Direction filter">
            {CHOICES
                .into_iter()
                .map(|(label, choice)| {
                    view! {
                        <button
                            class="segmented-btn"
                            class:active=move || store.log_direction() == choice
                            aria-pressed=move || (store.log_direction() == choice).to_string()
                            on:click=move |_| store.set_log_direction(choice)
                        >
                            {choice.map(|direction| view! { <DirectionIcon direction=direction size=13 /> })}
                            <span>{label}</span>
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

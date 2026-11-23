use chrono::{DateTime, Utc};
use leptos::prelude::*;
use mesh_core::api::Subscription;
use mesh_core::{AccountId, WindowClass};

use super::bar::{no_window, window_bar};
use super::menu::ActionsMenu;
use super::models::{self, ModelsEditor};
use super::reserve::ReserveEditor;
use super::status::badges;
use crate::format::{self, label_text};
use crate::state::use_store;
use crate::summary::window_of;

/// The inline editor open below a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Editor {
    Reserve,
    Models,
}

/// The allowed models of a row: "All models" or the patterns as chips, and
/// how many of the provider's listed models they allow.
fn models_line(sub: &Subscription) -> impl IntoView + use<> {
    let count = models::form::count_text(sub.models.as_ref(), &sub.available_models);
    let chips = match &sub.models {
        None => view! { <span class="model-chip model-chip-all">"All models"</span> }.into_any(),
        Some(list) => {
            let (shown, hidden) = models::form::split_chips(list.patterns());
            let shown = shown
                .iter()
                .map(|pattern| view! { <span class="model-chip">{pattern.clone()}</span> })
                .collect_view();
            let more = (hidden > 0).then(
                || view! { <span class="model-chip model-chip-more">{format!("+{hidden}")}</span> },
            );
            view! { {shown} {more} }.into_any()
        }
    };
    view! {
        <div class="sub-models">
            {chips} {count.map(|count| view! { <span class="sub-models-count">{count}</span> })}
        </div>
    }
}

/// The grid cells of a row: name and badges, the windows, and the
/// balancer's priority as dots with the in-flight count. `five_hour` is
/// whether the provider has a 5-hour limit at all; if not, the row shows only
/// the weekly window.
fn row_cells(
    sub: &Subscription,
    now: DateTime<Utc>,
    show_full: bool,
    best: Option<f64>,
    five_hour: bool,
) -> impl IntoView + use<> {
    let name = label_text(sub, show_full);
    let owner = sub
        .owner
        .as_deref()
        .map(|owner| format::display_label(owner, show_full));
    let badges = badges(sub, now);
    let classes: &[WindowClass] = if five_hour {
        &[WindowClass::FiveHour, WindowClass::Weekly]
    } else {
        &[WindowClass::Weekly]
    };
    let windows = classes
        .iter()
        .map(|&class| match window_of(&sub.windows, class) {
            Some(window) => window_bar(window, sub.reserve.reserve_for(window), now).into_any(),
            None => no_window(class).into_any(),
        })
        .collect_view();
    let filled = format::priority_dots(sub.score, best);
    let dots = (0..format::PRIORITY_DOTS)
        .map(|dot| view! { <span class:on=dot < filled></span> })
        .collect_view();
    view! {
        <div class="sub-id">
            <div class="sub-name">{name}</div>
            <div class="sub-meta">
                {owner.map(|owner| view! { <span>{owner}</span><span class="sep">"·"</span> })}
                <span>{sub.node.clone()}</span>
            </div>
            <div class="badges">
                {badges
                    .into_iter()
                    .map(|badge| {
                        view! { <span class=format!("badge {}", badge.kind.class())>{badge.text}</span> }
                    })
                    .collect_view()}
            </div>
            {models_line(sub)}
        </div>
        <div class="sub-windows" class:weekly-only=!five_hour>{windows}</div>
        <div class="sub-score">
            <div class="priority-dots">{dots}</div>
            <div class="muted">{format!("{} in flight", sub.in_flight)}</div>
        </div>
    }
}

/// One subscription: info, windows, score, and for owners an actions menu
/// with an inline reserve or models editor below.
#[component]
pub fn SubscriptionRow(id: AccountId) -> impl IntoView {
    let store = use_store();
    let id = StoredValue::new(id);
    let subscription =
        move || id.with_value(|id| store.with_subscription(id, |sub| (sub.can_manage, sub.paused)));
    let can_manage = move || subscription().is_some_and(|(can_manage, _)| can_manage);
    let paused = Signal::derive(move || subscription().is_some_and(|(_, paused)| paused));
    let name = Signal::derive(move || {
        let show_full = store.show_emails.get();
        id.with_value(|id| store.with_subscription(id, |sub| label_text(sub, show_full)))
            .unwrap_or_default()
    });
    let editing = RwSignal::new(None::<Editor>);
    let open_editor = Memo::new(move |_| editing.get().filter(|_| can_manage()));
    view! {
        <div class="sub">
            <div class="sub-row">
                {move || {
                    let now = store.now()?;
                    let show_full = store.show_emails.get();
                    id.with_value(|id| {
                        let provider = store.with_subscription(id, |sub| sub.provider)?;
                        let best = store.best_score(provider);
                        let five_hour = store.shows_five_hour(provider);
                        store.with_subscription(id, |sub| row_cells(sub, now, show_full, best, five_hour))
                    })
                }}
                <div class="sub-actions">
                    <Show when=can_manage>
                        <ActionsMenu id=id.get_value() paused=paused name=name editing=editing />
                    </Show>
                </div>
            </div>
            {move || {
                let editor = open_editor.get()?;
                untrack(|| {
                    let on_close = Callback::new(move |()| {
                        let _ = editing.try_set(None);
                    });
                    id.with_value(|id| {
                        store.with_subscription(id, |sub| match editor {
                            Editor::Reserve => {
                                view! {
                                    <ReserveEditor
                                        id=id.clone()
                                        initial=sub.reserve.clone()
                                        five_hour=store.shows_five_hour(sub.provider)
                                        on_close=on_close
                                    />
                                }
                                    .into_any()
                            }
                            Editor::Models => {
                                view! {
                                    <ModelsEditor
                                        id=id.clone()
                                        initial=sub.models.clone()
                                        available=sub.available_models.clone()
                                        on_close=on_close
                                    />
                                }
                                    .into_any()
                            }
                        })
                    })
                })
            }}
        </div>
    }
}

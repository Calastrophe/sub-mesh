use leptos::ev::SubmitEvent;
use leptos::prelude::*;
use mesh_core::api::{SettingKind, SettingValue, Tunable};

use super::values::{label, parse, shown, to_text, unit};
use crate::api;
use crate::state::use_store;

#[component]
pub fn TuningCard() -> impl IntoView {
    let store = use_store();
    let tunables = move || {
        store.settings.with(|view| {
            view.as_ref()
                .map(|view| view.values.clone())
                .unwrap_or_default()
        })
    };
    let proxy_only = Memo::new(move |_| {
        store.settings.with(|view| {
            view.as_ref()
                .and_then(|view| view.fixed.trusted_proxy.as_ref())
                .is_some_and(|proxy| proxy.only)
        })
    });
    view! {
        <section class="group">
            <div class="group-title">"Tuning"</div>
            <div class="settings-list">
                <For
                    each=tunables
                    key=|tunable| format!("{tunable:?}")
                    children=move |tunable| view! { <TunableRow tunable=tunable proxy_only=proxy_only /> }
                />
            </div>
        </section>
    }
}

#[component]
fn TunableRow(tunable: Tunable, proxy_only: Memo<bool>) -> impl IntoView {
    let store = use_store();
    let name = tunable.name;
    let kind = name.kind();
    let current = tunable.value;
    let text = RwSignal::new(to_text(kind, current));
    let error = RwSignal::new(None::<String>);
    let busy = move || store.settings_busy.get();
    let send = move |value: Option<SettingValue>| {
        let message = match value {
            Some(value) => format!("{} set to {}", label(name), shown(kind, value)),
            None => format!("{} reset to config", label(name)),
        };
        store.change_settings(message, api::set_value(name, value), |_| {});
    };
    let save = move |ev: SubmitEvent| {
        ev.prevent_default();
        match parse(name, &text.get_untracked(), current) {
            Ok(value) => send(Some(value)),
            Err(message) => error.set(Some(message)),
        }
    };
    let dirty = move || text.get().trim() != to_text(kind, current);
    let edit = move |ev| {
        text.set(event_target_value(&ev));
        error.set(None);
    };

    let control = match kind {
        SettingKind::Flag => {
            let on = current.as_flag();
            let locked = move || !on && proxy_only.get();
            view! {
                <button
                    class="toggle"
                    class:active=on
                    type="button"
                    aria-pressed=on.to_string()
                    title=move || locked().then_some("trusted_proxy.only")
                    prop:disabled=move || busy() || locked()
                    on:click=move |_| send(Some(SettingValue::Flag(!on)))
                >
                    {if on { "On" } else { "Off" }}
                </button>
            }
            .into_any()
        }
        _ => {
            let slider = (kind == SettingKind::Fraction).then(|| {
                view! {
                    <input
                        type="range"
                        class="slider"
                        min="0"
                        max="100"
                        step="0.5"
                        aria-label=label(name)
                        prop:disabled=busy
                        prop:value=move || text.get()
                        on:input=edit
                    />
                }
            });
            view! {
                <form class="field-input" on:submit=save>
                    {slider}
                    <input
                        type="number"
                        min="0"
                        step="any"
                        aria-label=label(name)
                        prop:disabled=busy
                        prop:value=move || text.get()
                        on:input=edit
                    />
                    <span class="unit">{unit(kind)}</span>
                    <button
                        class="btn btn-primary btn-sm"
                        type="submit"
                        class:invisible=move || !dirty()
                        prop:disabled=move || busy() || !dirty()
                    >
                        "Save"
                    </button>
                </form>
            }
            .into_any()
        }
    };

    view! {
        <div class="setting-row field">
            <div class="setting-name">
                <span class="setting-label">{label(name)}</span>
                <span class="mono muted">{name.as_str()}</span>
            </div>
            {control}
            <div class="setting-source">
                {if tunable.overridden {
                    view! {
                        <span class="badge badge-cooldown">"override"</span>
                        <span class="muted">{format!("config {}", shown(kind, tunable.config))}</span>
                        <button
                            class="btn btn-ghost btn-sm"
                            type="button"
                            prop:disabled=busy
                            on:click=move |_| send(None)
                        >
                            "Reset to config"
                        </button>
                    }
                        .into_any()
                } else {
                    view! { <span class="badge">"config"</span> }.into_any()
                }}
            </div>
            {move || error.get().map(|message| view! { <p class="editor-error" role="alert">{message}</p> })}
        </div>
    }
}

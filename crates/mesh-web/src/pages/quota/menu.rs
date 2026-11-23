use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos::wasm_bindgen::JsCast;
use mesh_core::AccountId;

use super::row::Editor;
use crate::components::{Glyph, Icon};
use crate::state::{Action, use_store};

/// The ⋯ menu of a subscription the caller owns: pause or resume, edit the
/// reserve or the allowed models, remove with confirmation.
#[component]
pub fn ActionsMenu(
    id: AccountId,
    #[prop(into)] paused: Signal<bool>,
    #[prop(into)] name: Signal<String>,
    editing: RwSignal<Option<Editor>>,
) -> impl IntoView {
    let store = use_store();
    let id = StoredValue::new(id);
    let open = RwSignal::new(false);
    let confirming = RwSignal::new(false);
    let busy = move || id.with_value(|id| store.is_applying(id));
    let wrapper = NodeRef::<html::Div>::new();
    let trigger = NodeRef::<html::Button>::new();
    let close = move || {
        open.set(false);
        confirming.set(false);
    };
    let close_and_focus = move || {
        close();
        if let Some(button) = trigger.get_untracked() {
            let _ = button.focus();
        }
    };
    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        let pointer = window_event_listener(ev::pointerdown, move |event| {
            let target = event
                .target()
                .and_then(|target| target.dyn_into::<web_sys::Node>().ok());
            if let Some(element) = wrapper.get_untracked()
                && !element.contains(target.as_ref())
            {
                close();
            }
        });
        let keyboard = window_event_listener(ev::keydown, move |event| {
            if event.key() == "Escape" {
                event.prevent_default();
                close_and_focus();
            }
        });
        on_cleanup(move || {
            pointer.remove();
            keyboard.remove();
        });
    });
    let run = move |action: Action| {
        close();
        store.apply(id.get_value(), action, |_| {});
    };
    view! {
        <div
            class="menu-wrap"
            node_ref=wrapper
            on:focusout=move |event| {
                if let Some(target) = event
                    .related_target()
                    .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                    && let Some(element) = wrapper.get_untracked()
                    && !element.contains(Some(&target))
                {
                    close();
                }
            }
        >
            <button
                class="icon-btn"
                aria-label="Subscription actions"
                node_ref=trigger
                aria-expanded=move || open.get()
                prop:disabled=move || busy() || editing.get().is_some()
                on:click=move |_| {
                    if open.get_untracked() {
                        close();
                    } else {
                        open.set(true);
                    }
                }
            >
                <Icon glyph=Glyph::More size=18 />
            </button>
            <Show when=move || open.get()>
                <div class="menu">
                    <Show
                        when=move || !confirming.get()
                        fallback=move || {
                            view! {
                                <div class="menu-confirm">
                                    <p>
                                        "Remove " <strong>{move || name.get()}</strong>
                                        "? Its saved tokens are deleted."
                                    </p>
                                    <div class="menu-confirm-buttons">
                                        <button class="btn" autofocus on:click=move |_| close_and_focus()>
                                            "Cancel"
                                        </button>
                                        <button
                                            class="btn btn-danger"
                                            on:click=move |_| run(Action::Remove)
                                        >
                                            "Remove"
                                        </button>
                                    </div>
                                </div>
                            }
                        }
                    >
                        <button
                            class="menu-item"
                            on:click=move |_| {
                                run(Action::SetPaused(!paused.get_untracked()));
                            }
                        >
                            {move || {
                                let (glyph, label) = if paused.get() {
                                    (Glyph::Play, "Resume")
                                } else {
                                    (Glyph::Pause, "Pause")
                                };
                                view! { <Icon glyph=glyph /> {label} }
                            }}
                        </button>
                        <button
                            class="menu-item"
                            on:click=move |_| {
                                close();
                                editing.set(Some(Editor::Reserve));
                            }
                        >
                            <Icon glyph=Glyph::Sliders />
                            "Edit reserve"
                        </button>
                        <button
                            class="menu-item"
                            on:click=move |_| {
                                close();
                                editing.set(Some(Editor::Models));
                            }
                        >
                            <Icon glyph=Glyph::Cube />
                            "Edit models"
                        </button>
                        <button
                            class="menu-item menu-danger"
                            on:click=move |_| confirming.set(true)
                        >
                            <Icon glyph=Glyph::Trash />
                            "Remove"
                        </button>
                    </Show>
                </div>
            </Show>
        </div>
    }
}

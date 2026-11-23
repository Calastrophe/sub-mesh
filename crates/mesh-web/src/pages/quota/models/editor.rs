use leptos::ev::SubmitEvent;
use leptos::prelude::*;
use mesh_core::{AccountId, ModelAllowlist};

use super::form::{self, Coverage};
use crate::components::{Glyph, Icon};
use crate::state::{Action, use_store};

/// How to write patterns, shown beside the field on hover or focus.
const HELP: &str = "One pattern per line. * matches anything.";

/// An inline form for a subscription's model allowlist. Patterns go one per
/// line, and `*` matches any run of characters. When the provider has listed
/// its models, a checklist shows which ones the patterns allow and adds or
/// removes exact ids. "Reset to default" clears the subscription's own list so
/// the daemon default applies again.
#[component]
pub fn ModelsEditor(
    id: AccountId,
    initial: Option<ModelAllowlist>,
    available: Vec<String>,
    on_close: Callback<()>,
) -> impl IntoView {
    let store = use_store();
    let text = RwSignal::new(form::patterns_text(initial.as_ref()));
    let error = RwSignal::new(None::<String>);
    let id = StoredValue::new(id);
    let busy = move || id.with_value(|id| store.is_applying(id));
    let available = StoredValue::new(available);

    let submit = move |models: Option<ModelAllowlist>| {
        if untrack(busy) {
            return;
        }
        error.set(None);
        store.apply(id.get_value(), Action::SetModels(models), move |ok| {
            if ok {
                let _ = on_close.try_run(());
            }
        });
    };
    let save = move |ev: SubmitEvent| {
        ev.prevent_default();
        match form::parse(&text.get_untracked()) {
            Ok(models) => submit(Some(models)),
            Err(message) => error.set(Some(message)),
        }
    };

    let checklist = move || {
        let total = available.with_value(Vec::len);
        (total > 0).then(|| {
            let allowed = move || available.with_value(|models| form::draft_count(&text.get(), models));
            let items = available
                .get_value()
                .into_iter()
                .map(|model| {
                    let model = StoredValue::new(model);
                    let coverage = move || model.with_value(|model| form::coverage(&text.get(), model));
                    view! {
                        <li>
                            <label class="model-option">
                                <input
                                    type="checkbox"
                                    prop:checked=move || coverage() != Coverage::Missing
                                    prop:disabled=move || busy() || coverage() == Coverage::Wildcard
                                    on:change=move |ev| {
                                        let checked = event_target_checked(&ev);
                                        model.with_value(|model| {
                                            text.update(|text| {
                                                *text = if checked {
                                                    form::with_exact(text, model)
                                                } else {
                                                    form::without_exact(text, model)
                                                };
                                            });
                                        });
                                        error.set(None);
                                    }
                                />
                                <span class="model-id">{model.get_value()}</span>
                                {move || {
                                    (coverage() == Coverage::Wildcard)
                                        .then(|| view! { <span class="model-note">"by pattern"</span> })
                                }}
                            </label>
                        </li>
                    }
                })
                .collect_view();
            view! {
                <div class="model-check">
                    <div class="model-check-head">
                        <span>"Models the provider lists"</span>
                        <span class="muted">{move || format!("{} of {total} allowed", allowed())}</span>
                    </div>
                    <ul class="model-list">{items}</ul>
                </div>
            }
        })
    };

    view! {
        <form class="editor" on:submit=save>
            <label class="field">
                <span class="field-label">
                    "Allowed models"
                    <span class="help" tabindex="0" aria-label=HELP>
                        <Icon glyph=Glyph::Info size=14 />
                        <span class="help-tip" role="tooltip">{HELP}</span>
                    </span>
                </span>
                <textarea
                    class="model-patterns"
                    rows="4"
                    spellcheck="false"
                    autocapitalize="off"
                    autofocus
                    placeholder="claude-sonnet-*"
                    prop:disabled=busy
                    prop:value=move || text.get()
                    on:input=move |ev| {
                        text.set(event_target_value(&ev));
                        error.set(None);
                    }
                ></textarea>
            </label>
            {checklist}
            {move || error.get().map(|message| view! { <p class="editor-error" role="alert">{message}</p> })}
            <div class="editor-buttons">
                <button class="btn btn-primary" type="submit" prop:disabled=busy>
                    "Save"
                </button>
                <button
                    class="btn"
                    type="button"
                    prop:disabled=busy
                    on:click=move |_| submit(None)
                >
                    "Reset to default"
                </button>
                <button class="btn btn-ghost" type="button" prop:disabled=busy on:click=move |_| on_close.run(())>
                    "Cancel"
                </button>
            </div>
        </form>
    }
}

use leptos::prelude::*;

use super::icon::{Glyph, Icon};
use super::logo::Logo;
use crate::pages::Page;

const SECTIONS: [(&str, &[Page]); 2] = [
    ("Overview", &[Page::Quota, Page::Usage, Page::Logs]),
    ("Mesh", &[Page::Daemons, Page::Users]),
];

fn glyph(page: Page) -> Glyph {
    match page {
        Page::Quota => Glyph::Gauge,
        Page::Usage => Glyph::Chart,
        Page::Logs => Glyph::List,
        Page::Daemons => Glyph::Server,
        Page::Users => Glyph::Users,
    }
}

/// The left navigation, collapsible to icons only.
#[component]
pub fn Sidebar(page: RwSignal<Page>, collapsed: RwSignal<bool>) -> impl IntoView {
    view! {
        <aside class="sidebar">
            <div class="brand">
                <Logo />
                <div class="brand-text">
                    <strong>"sub-mesh"</strong>
                </div>
            </div>
            <nav aria-label="Dashboard">
                {SECTIONS
                    .iter()
                    .map(|&(heading, pages)| {
                        view! {
                            <div class="nav-section">
                                <h6>{heading}</h6>
                                {pages
                                    .iter()
                                    .map(|&target| {
                                        view! {
                                            <a
                                                class="nav-item"
                                                class:active=move || page.get() == target
                                                href=format!("#{}", target.slug())
                                                aria-label=target.title()
                                                aria-current=move || (page.get() == target).then_some("page")
                                            >
                                                <Icon glyph=glyph(target) size=18 />
                                                <span class="nav-label">{target.title()}</span>
                                            </a>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                    })
                    .collect_view()}
            </nav>
            <button
                class="collapse-btn"
                aria-expanded=move || !collapsed.get()
                aria-label=move || {
                    if collapsed.get() { "Expand sidebar" } else { "Collapse sidebar" }
                }
                on:click=move |_| collapsed.update(|c| *c = !*c)
            >
                <Icon glyph=Glyph::Chevron size=14 />
            </button>
        </aside>
    }
}

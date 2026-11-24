use leptos::prelude::*;

/// The icons the dashboard uses (24x24, stroked).
#[derive(Clone, Copy)]
pub enum Glyph {
    Gauge,
    Chart,
    List,
    Server,
    Users,
    Refresh,
    More,
    Chevron,
    Pause,
    Play,
    Sliders,
    Cube,
    Trash,
    ArrowUpRight,
    ArrowDownLeft,
    Info,
    Settings,
}

impl Glyph {
    fn markup(self) -> &'static str {
        match self {
            Self::Gauge => r#"<path d="M12 14l4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/>"#,
            Self::Chart => r#"<path d="M3 3v18h18"/><path d="M8 17v-5M13 17V8M18 17v-8"/>"#,
            Self::List => r#"<path d="M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01"/>"#,
            Self::Server => {
                r#"<rect x="3" y="3" width="18" height="7" rx="2"/><rect x="3" y="14" width="18" height="7" rx="2"/><path d="M7 6.5h.01M7 17.5h.01"/>"#
            }
            Self::Users => {
                r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75"/>"#
            }
            Self::Refresh => r#"<path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/>"#,
            Self::More => {
                r#"<circle cx="5" cy="12" r="1.3"/><circle cx="12" cy="12" r="1.3"/><circle cx="19" cy="12" r="1.3"/>"#
            }
            Self::Chevron => r#"<path d="M15 18l-6-6 6-6"/>"#,
            Self::Pause => {
                r#"<rect x="6" y="5" width="4" height="14" rx="1"/><rect x="14" y="5" width="4" height="14" rx="1"/>"#
            }
            Self::Play => r#"<path d="M7 4l13 8-13 8z"/>"#,
            Self::Sliders => {
                r#"<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>"#
            }
            Self::Cube => {
                r#"<path d="M21 8l-9-5-9 5v8l9 5 9-5z"/><path d="M3 8l9 5 9-5M12 13v8"/>"#
            }
            Self::Trash => r#"<path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6"/>"#,
            Self::ArrowUpRight => r#"<path d="M7 17L17 7M8 7h9v9"/>"#,
            Self::ArrowDownLeft => r#"<path d="M17 7L7 17M16 17H7V8"/>"#,
            Self::Settings => {
                r#"<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>"#
            }
            Self::Info => {
                r#"<circle cx="12" cy="12" r="9.5"/><path d="M12 11v5.5"/><circle cx="12" cy="7.6" r="1.25" fill="currentColor" stroke="none"/>"#
            }
        }
    }
}

#[component]
pub fn Icon(glyph: Glyph, #[prop(default = 16)] size: u32) -> impl IntoView {
    view! {
        <svg
            class="icon"
            width=size
            height=size
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
            inner_html=glyph.markup()
        ></svg>
    }
}

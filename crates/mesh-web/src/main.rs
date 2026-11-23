//! The mesh dashboard: a client-side-rendered Leptos app built with trunk.
//!
//! Open it with `?mock` to run on built-in sample data.

mod api;
mod app;
mod components;
mod format;
mod level;
mod logs;
mod mock;
mod pages;
mod state;
mod summary;
mod usage;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}

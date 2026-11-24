//! Reusable view pieces.

mod header;
mod icon;
mod loaded;
mod logo;
mod number;
mod sidebar;
mod table;
mod tabs;
mod toasts;
mod topbar;

pub use header::{EmailToggle, PageHeader};
pub use icon::{Glyph, Icon};
pub use loaded::Loaded;
pub use logo::Logo;
pub use number::typed_number;
pub use sidebar::Sidebar;
pub use table::Table;
pub use tabs::ProviderTabs;
pub use toasts::Toasts;
pub use topbar::TopBar;

//! Shared types for mesh: providers, accounts, usage windows, the reserve
//! rule, and the wire protocol between `mesh-daemon` and `mesh-server`.

mod account;
pub mod admin;
mod admission;
mod allowlist;
pub mod api;
mod error;
pub mod protocol;
mod provider;
mod report;
mod reserve;
mod score;
pub mod spend;
mod window;

pub use account::{AccountId, ParseAccountIdError};
pub use admission::{Admission, DenyReason, admission};
pub use allowlist::{
    MAX_PATTERN_LEN, MAX_PATTERNS, ModelAllowlist, ModelAllowlistError, model_allowed,
};
pub use error::ApiError;
pub use provider::Provider;
pub use report::{AccountReport, DaemonReport};
pub use reserve::ReserveConfig;
pub use score::Score;
pub use window::{Window, WindowClass};

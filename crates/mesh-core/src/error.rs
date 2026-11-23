use serde::{Deserialize, Serialize};

/// Error body of every JSON API: the management API, the daemon's admin API,
/// and the control API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// Human-readable error message.
    pub error: String,
}

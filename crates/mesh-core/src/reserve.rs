use serde::{Deserialize, Serialize};

use crate::window::{Window, WindowClass};

/// How much of each window the owner keeps for themselves.
///
/// A reserve of 0.3 means the mesh may use a window until it is 70% used.
/// The class setting wins over `default`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReserveConfig {
    /// Reserve fraction for a window without a class setting.
    pub default: f64,
    /// Override for the 5-hour window.
    pub five_hour: Option<f64>,
    /// Override for the weekly window.
    pub weekly: Option<f64>,
}

impl Default for ReserveConfig {
    fn default() -> Self {
        Self {
            default: 0.3,
            five_hour: None,
            weekly: None,
        }
    }
}

impl ReserveConfig {
    /// The reserve fraction for `window`, clamped to `0.0..=1.0`.
    ///
    /// An override or default of `NaN` is returned unchanged.
    #[must_use]
    pub fn reserve_for(&self, window: &Window) -> f64 {
        let class_reserve = match window.class() {
            WindowClass::FiveHour => self.five_hour,
            WindowClass::Weekly => self.weekly,
        };
        class_reserve.unwrap_or(self.default).clamp(0.0, 1.0)
    }
}

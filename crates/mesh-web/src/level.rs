//! Usage levels that drive bar and badge colors.

/// How worrying a usage figure is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Green: under 70%.
    Ok,
    /// Amber: 70% up to 90%.
    Warn,
    /// Red: 90% and above.
    Hot,
}

impl Level {
    /// The level of a used fraction. `NaN` counts as fine.
    pub fn from_used(used: f64) -> Self {
        if used >= 0.9 {
            Self::Hot
        } else if used >= 0.7 {
            Self::Warn
        } else {
            Self::Ok
        }
    }

    /// The level of an HTTP status: success and redirects are fine, client
    /// errors warn, everything else is hot.
    pub fn from_status(status: u16) -> Self {
        match status {
            ..400 => Self::Ok,
            400..500 => Self::Warn,
            _ => Self::Hot,
        }
    }

    /// The CSS class that colors this level.
    pub fn class(self) -> &'static str {
        match self {
            Self::Ok => "lvl-ok",
            Self::Warn => "lvl-warn",
            Self::Hot => "lvl-hot",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds() {
        assert_eq!(Level::from_used(0.0), Level::Ok);
        assert_eq!(Level::from_used(0.699), Level::Ok);
        assert_eq!(Level::from_used(0.7), Level::Warn);
        assert_eq!(Level::from_used(0.899), Level::Warn);
        assert_eq!(Level::from_used(0.9), Level::Hot);
        assert_eq!(Level::from_used(1.0), Level::Hot);
        assert_eq!(Level::from_used(f64::NAN), Level::Ok);
    }

    #[test]
    fn status_levels() {
        assert_eq!(Level::from_status(200), Level::Ok);
        assert_eq!(Level::from_status(429), Level::Warn);
        assert_eq!(Level::from_status(502), Level::Hot);
    }
}

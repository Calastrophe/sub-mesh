use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An upstream subscription provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// OpenAI Codex through a ChatGPT subscription.
    Codex,
    /// Anthropic Claude through a Claude Pro/Max subscription.
    Claude,
}

impl Provider {
    /// All supported providers.
    pub const ALL: [Self; 2] = [Self::Codex, Self::Claude];

    /// The lowercase name used in paths, config, and storage.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string is not a known provider name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown provider {0:?} (expected \"codex\" or \"claude\")")]
pub struct ParseProviderError(String);

impl FromStr for Provider {
    type Err = ParseProviderError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.as_str() == s)
            .ok_or_else(|| ParseProviderError(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_str() {
        for provider in Provider::ALL {
            assert_eq!(provider.as_str().parse::<Provider>(), Ok(provider));
        }
        assert!("gemini".parse::<Provider>().is_err());
    }
}

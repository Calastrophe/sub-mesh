use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Identifier for one logged-in subscription account, unique across the mesh.
///
/// Made of 1–64 ASCII letters, digits, `-`, and `_`, so it is safe in URL paths
/// and file names. Login generates IDs as `<provider>-<random hex>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountId(String);

impl AccountId {
    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for AccountId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for AccountId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// Error returned when an account ID has an invalid length or character.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid account id {0:?}: use 1-64 ASCII letters, digits, '-' or '_'")]
pub struct ParseAccountIdError(String);

impl FromStr for AccountId {
    type Err = ParseAccountIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.to_owned())
    }
}

impl TryFrom<String> for AccountId {
    type Error = ParseAccountIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = (1..=64).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if valid {
            Ok(Self(value))
        } else {
            Err(ParseAccountIdError(value))
        }
    }
}

impl From<AccountId> for String {
    fn from(id: AccountId) -> Self {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_ids() {
        assert!("claude-1a2b".parse::<AccountId>().is_ok());
        for bad in ["", "../x", "a/b", "a b", &"x".repeat(65)] {
            assert!(
                bad.parse::<AccountId>().is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn deserialize_validates() {
        assert!(serde_json::from_str::<AccountId>("\"ok_1\"").is_ok());
        assert!(serde_json::from_str::<AccountId>("\"no/slash\"").is_err());
    }
}

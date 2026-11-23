use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Longest pattern accepted in a [`ModelAllowlist`].
pub const MAX_PATTERN_LEN: usize = 128;

/// Most patterns accepted in one [`ModelAllowlist`].
pub const MAX_PATTERNS: usize = 64;

/// The models of one account that the mesh may use.
///
/// Each pattern is a model id, where `*` matches any run of characters, so
/// `claude-sonnet-*` allows every Sonnet model. Matching ignores ASCII case.
/// Where an allowlist is optional, `None` means every model is allowed. An
/// allowlist always holds at least one pattern; to stop all traffic, pause
/// the account instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct ModelAllowlist(Vec<String>);

/// Why a list of patterns is not a valid [`ModelAllowlist`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelAllowlistError {
    #[error("the model allowlist is empty; pause the account to stop all traffic")]
    Empty,
    #[error("the model allowlist has more than {MAX_PATTERNS} patterns")]
    TooMany,
    #[error(
        "model pattern {0:?} is empty, longer than {MAX_PATTERN_LEN} bytes, or has whitespace or control characters"
    )]
    BadPattern(String),
}

impl ModelAllowlist {
    /// Build an allowlist, trimming each pattern and dropping duplicates.
    pub fn new<I, S>(patterns: I) -> Result<Self, ModelAllowlistError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut list: Vec<String> = Vec::new();
        for pattern in patterns {
            let pattern = pattern.into().trim().to_owned();
            let valid = !pattern.is_empty()
                && pattern.len() <= MAX_PATTERN_LEN
                && !pattern.chars().any(|c| c.is_whitespace() || c.is_control());
            if !valid {
                return Err(ModelAllowlistError::BadPattern(pattern));
            }
            if !list.iter().any(|held| held.eq_ignore_ascii_case(&pattern)) {
                if list.len() == MAX_PATTERNS {
                    return Err(ModelAllowlistError::TooMany);
                }
                list.push(pattern);
            }
        }
        if list.is_empty() {
            return Err(ModelAllowlistError::Empty);
        }
        Ok(Self(list))
    }

    /// The patterns, in the order given.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.0
    }

    /// Whether `model` matches any pattern.
    #[must_use]
    pub fn allows(&self, model: &str) -> bool {
        let model = ascii_lowercase(model);
        self.0
            .iter()
            .any(|pattern| matches(&ascii_lowercase(pattern), &model))
    }
}

/// Whether `model` is allowed by `allowlist`, where `None` allows every model.
#[must_use]
pub fn model_allowed(allowlist: Option<&ModelAllowlist>, model: &str) -> bool {
    allowlist.is_none_or(|list| list.allows(model))
}

impl TryFrom<Vec<String>> for ModelAllowlist {
    type Error = ModelAllowlistError;

    fn try_from(patterns: Vec<String>) -> Result<Self, Self::Error> {
        Self::new(patterns)
    }
}

impl From<ModelAllowlist> for Vec<String> {
    fn from(list: ModelAllowlist) -> Self {
        list.0
    }
}

impl fmt::Display for ModelAllowlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(", "))
    }
}

fn ascii_lowercase(text: &str) -> Cow<'_, str> {
    if text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(text.to_ascii_lowercase())
    } else {
        Cow::Borrowed(text)
    }
}

/// Match the anchored ends first, then find each interior literal in order.
fn matches(pattern: &str, model: &str) -> bool {
    let Some((prefix, rest)) = pattern.split_once('*') else {
        return model == pattern;
    };
    let (middle, suffix) = rest.rsplit_once('*').unwrap_or(("", rest));
    let Some(mut remaining) = model
        .strip_prefix(prefix)
        .and_then(|model| model.strip_suffix(suffix))
    else {
        return false;
    };

    for literal in middle.split('*').filter(|literal| !literal.is_empty()) {
        let Some(index) = remaining.find(literal) else {
            return false;
        };
        remaining = &remaining[index + literal.len()..];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(patterns: &[&str]) -> ModelAllowlist {
        ModelAllowlist::new(patterns.iter().copied()).unwrap()
    }

    #[test]
    fn exact_and_wildcard_patterns_match_ignoring_case() {
        let allow = list(&["claude-sonnet-*", "GPT-6.2-SOL", "*-mini"]);
        assert!(allow.allows("claude-sonnet-5-5"));
        assert!(allow.allows("Claude-Sonnet-4-5-20250929"));
        assert!(allow.allows("gpt-6.2-sol"));
        assert!(allow.allows("gpt-5-mini"));
        assert!(!allow.allows("claude-opus-5-5"));
        assert!(!allow.allows("gpt-6.2-sol-pro"));
        assert!(!allow.allows("claude-sonnet"));
    }

    #[test]
    fn star_alone_and_inner_stars_match() {
        assert!(list(&["*"]).allows("anything"));
        assert!(list(&["*"]).allows(""));
        assert!(list(&["gpt-*-sol"]).allows("gpt-6.2-sol"));
        assert!(list(&["a*b*c"]).allows("axxbyyc"));
        assert!(!list(&["a*b*c"]).allows("axxbyy"));
    }

    #[test]
    fn glob_literals_cannot_overlap_anchored_ends() {
        let allow = list(&["ab**B*bc", "*XYZ", "é*ß"]);
        assert!(allow.allows("abbbc"));
        assert!(!allow.allows("abbc"));
        assert!(allow.allows("XYZxyz"));
        assert!(allow.allows("é🌱ß"));
        assert!(!allow.allows("É🌱ß"));
    }

    #[test]
    fn none_allows_every_model() {
        assert!(model_allowed(None, "claude-opus-5-5"));
        assert!(!model_allowed(
            Some(&list(&["claude-haiku-*"])),
            "claude-opus-5-5"
        ));
    }

    #[test]
    fn new_trims_dedupes_and_validates() {
        assert_eq!(
            list(&[" gpt-6.2-sol ", "GPT-6.2-SOL", "claude-*"]).patterns(),
            ["gpt-6.2-sol", "claude-*"]
        );
        assert_eq!(
            ModelAllowlist::new(Vec::<String>::new()),
            Err(ModelAllowlistError::Empty)
        );
        assert!(matches!(
            ModelAllowlist::new(["bad model"]),
            Err(ModelAllowlistError::BadPattern(_))
        ));
        assert!(matches!(
            ModelAllowlist::new([""]),
            Err(ModelAllowlistError::BadPattern(_))
        ));
        let too_many: Vec<String> = (0..=MAX_PATTERNS).map(|i| format!("m{i}")).collect();
        assert_eq!(
            ModelAllowlist::new(too_many),
            Err(ModelAllowlistError::TooMany)
        );
    }

    #[test]
    fn serde_is_a_plain_list_and_validates() {
        let allow = list(&["claude-*", "gpt-6.2-sol"]);
        let json = serde_json::to_string(&allow).unwrap();
        assert_eq!(json, r#"["claude-*","gpt-6.2-sol"]"#);
        assert_eq!(
            serde_json::from_str::<ModelAllowlist>(&json).unwrap(),
            allow
        );
        assert!(serde_json::from_str::<ModelAllowlist>("[]").is_err());
    }
}

//! Conversion between a [`ModelAllowlist`] and the pattern text of the
//! models editor, plus the counts and labels shown on a subscription row.

use mesh_core::{ModelAllowlist, ModelAllowlistError, model_allowed};

/// Patterns shown as chips on a row before the rest collapse into "+N".
pub const CHIP_LIMIT: usize = 3;

/// How the patterns being edited cover one model id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// No pattern allows the model.
    Missing,
    /// A pattern equal to the id allows it.
    Exact,
    /// Only a wildcard pattern allows it.
    Wildcard,
}

/// The editor text for an allowlist: one pattern per line. No allowlist is
/// empty text.
pub fn patterns_text(models: Option<&ModelAllowlist>) -> String {
    models
        .map(|list| list.patterns().join("\n"))
        .unwrap_or_default()
}

/// Patterns in editor text, split on whitespace and commas.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|token| !token.is_empty())
}

/// Read the editor text back into an allowlist.
pub fn parse(text: &str) -> Result<ModelAllowlist, String> {
    ModelAllowlist::new(tokens(text)).map_err(|error| match error {
        ModelAllowlistError::Empty => {
            "Enter at least one pattern, or reset to the daemon default".to_owned()
        }
        other => other.to_string(),
    })
}

/// Whether the valid patterns in `text` allow `model`. Text without a valid
/// pattern allows nothing, so a list being drafted starts from a blank slate.
fn draft_allows(text: &str, model: &str) -> bool {
    tokens(text).any(|token| ModelAllowlist::new([token]).is_ok_and(|list| list.allows(model)))
}

fn has_exact(text: &str, model: &str) -> bool {
    tokens(text).any(|token| token.eq_ignore_ascii_case(model))
}

/// How the patterns in `text` cover `model`.
pub fn coverage(text: &str, model: &str) -> Coverage {
    if tokens(text).any(|token| {
        token.contains('*') && ModelAllowlist::new([token]).is_ok_and(|list| list.allows(model))
    }) {
        Coverage::Wildcard
    } else if has_exact(text, model) {
        Coverage::Exact
    } else {
        Coverage::Missing
    }
}

/// `text` with `model` added as an exact pattern on its own line.
pub fn with_exact(text: &str, model: &str) -> String {
    if has_exact(text, model) {
        return text.to_owned();
    }
    let kept = text.trim_end();
    if kept.is_empty() {
        model.to_owned()
    } else {
        format!("{kept}\n{model}")
    }
}

/// `text` without any pattern equal to `model`, one pattern per line.
pub fn without_exact(text: &str, model: &str) -> String {
    tokens(text)
        .filter(|token| !token.eq_ignore_ascii_case(model))
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many of `available` the patterns in `text` allow.
pub fn draft_count(text: &str, available: &[String]) -> usize {
    available
        .iter()
        .filter(|model| draft_allows(text, model))
        .count()
}

/// How many of `available` the allowlist in effect allows.
pub fn allowed_count(models: Option<&ModelAllowlist>, available: &[String]) -> usize {
    available
        .iter()
        .filter(|model| model_allowed(models, model))
        .count()
}

/// The count shown on a row, such as "8 of 13 models". Empty when the
/// provider has not listed models yet.
pub fn count_text(models: Option<&ModelAllowlist>, available: &[String]) -> Option<String> {
    let total = available.len();
    if total == 0 {
        return None;
    }
    let noun = if total == 1 { "model" } else { "models" };
    Some(match models {
        None => format!("{total} {noun}"),
        Some(_) => format!("{} of {total} {noun}", allowed_count(models, available)),
    })
}

/// The patterns shown as chips and how many more are hidden.
pub fn split_chips(patterns: &[String]) -> (&[String], usize) {
    let shown = patterns.len().min(CHIP_LIMIT);
    (&patterns[..shown], patterns.len() - shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(patterns: &[&str]) -> ModelAllowlist {
        ModelAllowlist::new(patterns.iter().copied()).unwrap()
    }

    fn ids(models: &[&str]) -> Vec<String> {
        models.iter().map(|&model| model.to_owned()).collect()
    }

    #[test]
    fn shows_one_pattern_per_line() {
        assert_eq!(patterns_text(None), "");
        assert_eq!(
            patterns_text(Some(&list(&["claude-sonnet-*", "gpt-6-luna"]))),
            "claude-sonnet-*\ngpt-6-luna"
        );
    }

    #[test]
    fn parses_lines_commas_and_duplicates() {
        let parsed = parse(" claude-sonnet-*,\nclaude-haiku-5-5\n\nCLAUDE-HAIKU-5-5 ").unwrap();
        assert_eq!(parsed, list(&["claude-sonnet-*", "claude-haiku-5-5"]));
        let text = patterns_text(Some(&parsed));
        assert_eq!(parse(&text).unwrap(), parsed);
    }

    #[test]
    fn rejects_bad_text() {
        assert!(parse("").unwrap_err().contains("at least one pattern"));
        assert!(parse(" \n, ").is_err());
        let too_long = "a".repeat(mesh_core::MAX_PATTERN_LEN + 1);
        assert!(parse(&too_long).is_err());
        let many = (0..=mesh_core::MAX_PATTERNS)
            .map(|n| format!("model-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse(&many).is_err());
    }

    #[test]
    fn covers_models_exactly_or_by_wildcard() {
        let text = "claude-sonnet-*\nclaude-haiku-5-5";
        assert_eq!(coverage(text, "claude-sonnet-5-5"), Coverage::Wildcard);
        assert_eq!(
            coverage("claude-sonnet-* claude-sonnet-5-5", "claude-sonnet-5-5"),
            Coverage::Wildcard
        );
        assert_eq!(coverage(text, "Claude-Haiku-5-5"), Coverage::Exact);
        assert_eq!(coverage(text, "claude-opus-5-5"), Coverage::Missing);
        assert_eq!(coverage("", "claude-opus-5-5"), Coverage::Missing);
    }

    #[test]
    fn adds_and_removes_exact_patterns() {
        assert_eq!(with_exact("", "gpt-6-luna"), "gpt-6-luna");
        assert_eq!(with_exact("gpt-6.*\n", "gpt-6-luna"), "gpt-6.*\ngpt-6-luna");
        assert_eq!(with_exact("GPT-6-LUNA", "gpt-6-luna"), "GPT-6-LUNA");
        assert_eq!(
            without_exact("gpt-6.*\ngpt-6-luna", "GPT-6-Luna"),
            "gpt-6.*"
        );
        assert_eq!(without_exact("gpt-6-luna", "gpt-6-luna"), "");
    }

    #[test]
    fn counts_allowed_models() {
        let available = ids(&["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]);
        let restricted = list(&["claude-sonnet-*", "claude-haiku-5-5"]);
        assert_eq!(allowed_count(Some(&restricted), &available), 2);
        assert_eq!(allowed_count(None, &available), 3);
        assert_eq!(draft_count("claude-sonnet-*", &available), 1);
        assert_eq!(draft_count("", &available), 0);
        assert_eq!(draft_count("claude-sonnet-*\nbad\u{7}", &available), 1);
    }

    #[test]
    fn labels_the_count_for_a_row() {
        let available = ids(&["gpt-6.2-sol", "gpt-6.1-sol", "gpt-6-luna"]);
        let restricted = list(&["gpt-6.*-sol"]);
        assert_eq!(
            count_text(Some(&restricted), &available).as_deref(),
            Some("2 of 3 models")
        );
        assert_eq!(count_text(None, &available).as_deref(), Some("3 models"));
        assert_eq!(
            count_text(None, &ids(&["gpt-6-luna"])).as_deref(),
            Some("1 model")
        );
        assert_eq!(count_text(Some(&restricted), &[]), None);
    }

    #[test]
    fn collapses_extra_chips() {
        let patterns = ids(&["a", "b", "c", "d", "e"]);
        let (shown, hidden) = split_chips(&patterns);
        assert_eq!((shown.len(), hidden), (CHIP_LIMIT, 2));
        let (shown, hidden) = split_chips(&patterns[..2]);
        assert_eq!((shown.len(), hidden), (2, 0));
    }
}

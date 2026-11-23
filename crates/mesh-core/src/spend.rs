//! Price-weighted token counts, to compare how much of a subscription each
//! user spent.
//!
//! A subscription does not bill per token, but its limits follow the
//! providers' list prices closely: an output token costs several input
//! tokens, a cached input token a tenth of one, and a large model more than
//! a small one. Spend weighs each token by those relative prices, in units
//! of one uncached input token of the provider's reference model. Spends
//! compare only within one provider.

use crate::api::TokenUsage;
use crate::provider::Provider;

/// The price of each kind of token, relative to an uncached input token.
struct Weights {
    cache_read: f64,
    cache_write: f64,
    output: f64,
}

const CLAUDE: Weights = Weights {
    cache_read: 0.1,
    cache_write: 1.25,
    output: 5.0,
};

const CODEX: Weights = Weights {
    cache_read: 0.1,
    cache_write: 1.0,
    output: 8.0,
};

/// The price of a model relative to the provider's reference model, from
/// the model family in its name. Unknown models count as the reference.
fn model_factor(provider: Provider, model: Option<&str>) -> f64 {
    let model = model.unwrap_or_default().to_ascii_lowercase();
    match provider {
        Provider::Claude if model.contains("opus") => 5.0 / 3.0,
        Provider::Claude if model.contains("haiku") => 1.0 / 3.0,
        Provider::Codex if model.contains("luna") || model.contains("mini") => 0.2,
        Provider::Claude | Provider::Codex => 1.0,
    }
}

/// The price-weighted size of `usage`, spent on `model`.
#[must_use]
pub fn spend(provider: Provider, model: Option<&str>, usage: &TokenUsage) -> f64 {
    let weights = match provider {
        Provider::Claude => &CLAUDE,
        Provider::Codex => &CODEX,
    };
    let uncached = usage
        .input
        .saturating_sub(usage.cache_read)
        .saturating_sub(usage.cache_write);
    #[allow(clippy::cast_precision_loss)]
    let tokens = uncached as f64
        + usage.cache_read as f64 * weights.cache_read
        + usage.cache_write as f64 * weights.cache_write
        + usage.output as f64 * weights.output;
    tokens * model_factor(provider, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cache_read: u64, cache_write: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input,
            cache_read,
            cache_write,
            output,
            reasoning: 0,
        }
    }

    #[test]
    fn weighs_each_kind_of_token() {
        let spent = spend(
            Provider::Claude,
            Some("claude-sonnet-5-5"),
            &usage(1_200, 1_000, 100, 10),
        );
        assert!((spent - (100.0 + 100.0 + 125.0 + 50.0)).abs() < 1e-9);
        let spent = spend(
            Provider::Codex,
            Some("gpt-6.1-sol"),
            &usage(1_000, 500, 0, 10),
        );
        assert!((spent - (500.0 + 50.0 + 80.0)).abs() < 1e-9);
    }

    #[test]
    fn larger_models_spend_more() {
        let tokens = usage(1_000, 0, 0, 100);
        let opus = spend(Provider::Claude, Some("claude-opus-5-5"), &tokens);
        let sonnet = spend(Provider::Claude, Some("claude-sonnet-5-5"), &tokens);
        let haiku = spend(Provider::Claude, Some("claude-haiku-5-5"), &tokens);
        assert!(opus > sonnet && sonnet > haiku);
        let luna = spend(Provider::Codex, Some("gpt-6-luna"), &tokens);
        let sol = spend(Provider::Codex, Some("gpt-6.1-sol"), &tokens);
        assert!(sol > luna);
        assert!((spend(Provider::Codex, None, &tokens) - sol).abs() < 1e-9);
    }

    #[test]
    fn inconsistent_counts_never_go_negative() {
        assert!(spend(Provider::Claude, None, &usage(10, 50, 50, 0)) >= 0.0);
    }
}

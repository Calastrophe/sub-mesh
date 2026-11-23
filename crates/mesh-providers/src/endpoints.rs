//! The URLs each provider flow talks to, and the rule that maps a consumer
//! path onto an upstream URL.

use std::sync::LazyLock;

use mesh_core::Provider;
use url::Url;

use crate::ProviderError;

static CODEX_UPSTREAM: LazyLock<Url> =
    LazyLock::new(|| parse("https://chatgpt.com/backend-api/codex/"));
static CLAUDE_UPSTREAM: LazyLock<Url> = LazyLock::new(|| parse("https://api.anthropic.com/"));

fn parse(url: &str) -> Url {
    Url::parse(url).expect("built-in URL is valid")
}

/// The base URL that consumer paths are relative to for `provider`.
pub(crate) fn upstream_base(provider: Provider) -> &'static Url {
    match provider {
        Provider::Codex => &CODEX_UPSTREAM,
        Provider::Claude => &CLAUDE_UPSTREAM,
    }
}

/// Every URL the login, refresh, usage, and relay code talks to.
///
/// [`Endpoints::new`] gives the real provider endpoints. Tests and local
/// mocks change the public fields to point at a different server.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Endpoints {
    pub provider: Provider,
    /// Where the user's browser goes to approve the login.
    pub authorize_url: Url,
    /// OAuth token endpoint, for code exchange and refresh.
    pub token_url: Url,
    /// Usage endpoint polled by `poll_usage`.
    pub usage_url: Url,
    /// Base that consumer paths are joined onto. See [`crate::upstream_base`].
    pub upstream_base: Url,
    /// Port of the local login callback server. The provider's OAuth client
    /// only allows its fixed port; `0` picks any free port (tests).
    pub callback_port: u16,
}

impl Endpoints {
    /// The real endpoints of `provider`.
    pub fn new(provider: Provider) -> Self {
        let (authorize, token, usage, port) = match provider {
            Provider::Codex => (
                "https://auth.openai.com/oauth/authorize",
                "https://auth.openai.com/oauth/token",
                "https://chatgpt.com/backend-api/wham/usage",
                1455,
            ),
            Provider::Claude => (
                "https://claude.ai/oauth/authorize",
                "https://platform.claude.com/v1/oauth/token",
                "https://api.anthropic.com/api/oauth/usage",
                54545,
            ),
        };
        Endpoints {
            provider,
            authorize_url: parse(authorize),
            token_url: parse(token),
            usage_url: parse(usage),
            upstream_base: upstream_base(provider).clone(),
            callback_port: port,
        }
    }

    /// Join a consumer path and query onto [`Endpoints::upstream_base`].
    /// See [`crate::upstream_url`].
    pub fn upstream_url(&self, path_and_query: &str) -> Result<Url, ProviderError> {
        join_upstream(&self.upstream_base, path_and_query)
    }
}

/// Join `path_and_query` onto `base`, refusing anything that could leave
/// the base: absolute URLs, `//host`, backslashes, `.`/`..` segments (also
/// percent-encoded), encoded separators, fragments, and control characters.
pub(crate) fn join_upstream(base: &Url, path_and_query: &str) -> Result<Url, ProviderError> {
    let refuse = |why: &str| ProviderError::Other(format!("refusing upstream path: {why}"));

    if path_and_query
        .chars()
        .any(|c| c.is_control() || c == ' ' || c == '\\' || c == '#')
    {
        return Err(refuse(
            "it has a space, control character, backslash, or fragment",
        ));
    }
    let relative = path_and_query.strip_prefix('/').unwrap_or(path_and_query);
    if relative.starts_with('/') {
        return Err(refuse("it starts with //"));
    }
    let path = relative.split_once('?').map_or(relative, |(path, _)| path);
    if path
        .split('/')
        .next()
        .is_some_and(|first| first.contains(':'))
    {
        return Err(refuse("it looks like an absolute URL"));
    }
    for segment in path.split('/') {
        let lower = segment.to_ascii_lowercase();
        if lower.contains("%2f") || lower.contains("%5c") {
            return Err(refuse("it has an encoded path separator"));
        }
        let decoded = lower.replace("%2e", ".");
        if decoded == "." || decoded == ".." {
            return Err(refuse("it has a . or .. segment"));
        }
    }

    let mut base = base.clone();
    if !base.path().ends_with('/') {
        let with_slash = format!("{}/", base.path());
        base.set_path(&with_slash);
    }
    let joined = base
        .join(relative)
        .map_err(|error| refuse(&error.to_string()))?;
    let stays_inside = joined.origin() == base.origin() && joined.path().starts_with(base.path());
    if !stays_inside {
        return Err(refuse("it leaves the upstream base"));
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_under_a_base_prefix_without_a_trailing_slash() {
        let base = Url::parse("https://example.test/prefix").unwrap();
        let url = join_upstream(&base, "a/b").unwrap();
        assert_eq!(url.as_str(), "https://example.test/prefix/a/b");
    }
}

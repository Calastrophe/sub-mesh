//! The interactive OAuth login: PKCE, the browser URL, the local callback
//! (or a pasted code), and the code exchange.

use std::time::Duration;

use chrono::Utc;
use mesh_core::{AccountId, Provider};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use url::Url;

use self::callback::CallbackServer;
use self::pkce::Pkce;
use crate::oauth::{self, CodeExchange};
use crate::{Endpoints, LoginOptions, ProviderError, StoredAccount};

mod callback;
mod pkce;

/// How long a login waits for the browser (or a pasted code).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Run the whole login. Prints the URL and instructions on stderr.
pub(crate) async fn run(
    client: &reqwest::Client,
    options: &LoginOptions,
    endpoints: &Endpoints,
) -> Result<StoredAccount, ProviderError> {
    let mut flow = LoginFlow::begin(endpoints, options.manual).await?;
    let provider = endpoints.provider;
    eprintln!(
        "Open this URL in a browser to log in to {provider}:\n\n  {}\n",
        flow.authorize_url()
    );

    let code = if options.manual {
        eprintln!(
            "After you approve, the browser goes to a localhost address that does not load.\n\
             Copy the whole URL from the address bar (or just the code) and paste it here:"
        );
        flow.read_code(&mut BufReader::new(tokio::io::stdin()), LOGIN_TIMEOUT)
            .await?
    } else {
        eprintln!(
            "Waiting up to {} minutes for the browser to return to {} ...",
            LOGIN_TIMEOUT.as_secs() / 60,
            flow.redirect_uri
        );
        flow.wait_for_callback(LOGIN_TIMEOUT).await?
    };
    flow.complete(client, &code).await
}

/// One login in progress.
struct LoginFlow<'endpoints> {
    endpoints: &'endpoints Endpoints,
    pkce: Pkce,
    state: String,
    redirect_uri: String,
    authorize_url: Url,
    server: Option<CallbackServer>,
}

impl<'endpoints> LoginFlow<'endpoints> {
    /// Generate PKCE and state and, unless `manual`, start the callback server.
    async fn begin(endpoints: &'endpoints Endpoints, manual: bool) -> Result<Self, ProviderError> {
        let app = oauth::app(endpoints.provider);
        let pkce = Pkce::generate();
        let state = pkce::random_token();
        let (server, port) = if manual {
            (None, endpoints.callback_port)
        } else {
            let server =
                CallbackServer::bind(endpoints.callback_port, app.callback_path, state.clone())
                    .await?;
            let port = server.port();
            (Some(server), port)
        };
        let redirect_uri = format!("http://localhost:{port}{}", app.callback_path);
        let authorize_url = oauth::authorize_url(endpoints, &redirect_uri, &pkce.challenge, &state);
        Ok(LoginFlow {
            endpoints,
            pkce,
            state,
            redirect_uri,
            authorize_url,
            server,
        })
    }

    fn authorize_url(&self) -> &Url {
        &self.authorize_url
    }

    /// Wait for the browser to return to the local callback.
    async fn wait_for_callback(&mut self, limit: Duration) -> Result<String, ProviderError> {
        let server = self.server.take().ok_or_else(|| {
            ProviderError::Other("the callback server is not running in manual mode".to_owned())
        })?;
        server.wait_for_code(limit).await
    }

    /// Read a pasted redirect URL or code from `input`.
    async fn read_code<R>(&self, input: &mut R, limit: Duration) -> Result<String, ProviderError>
    where
        R: AsyncBufRead + Unpin,
    {
        let mut line = String::new();
        let read = tokio::time::timeout(limit, input.read_line(&mut line))
            .await
            .map_err(|_| {
                ProviderError::Other(format!(
                    "timed out after {} seconds waiting for the pasted code",
                    limit.as_secs()
                ))
            })??;
        if read == 0 {
            return Err(ProviderError::Other("no code was pasted".to_owned()));
        }
        parse_pasted(&line, &self.state)
    }

    /// Exchange `code` for tokens and build the new account.
    async fn complete(
        &self,
        client: &reqwest::Client,
        code: &str,
    ) -> Result<StoredAccount, ProviderError> {
        let provider = self.endpoints.provider;
        let exchange = CodeExchange {
            redirect_uri: &self.redirect_uri,
            verifier: &self.pkce.verifier,
            state: &self.state,
            code,
        };
        let tokens = oauth::exchange_code(client, self.endpoints, &exchange).await?;
        let credentials = tokens.into_credentials(provider, Utc::now())?;
        if provider == Provider::Codex && credentials.chatgpt_account_id.is_none() {
            tracing::warn!(
                "the Codex id_token has no ChatGPT account id; requests may be rejected"
            );
        }
        Ok(StoredAccount {
            id: new_account_id(provider),
            provider,
            label: credentials.email.clone(),
            credentials,
        })
    }
}

/// `<provider>-<8 random hex digits>`.
fn new_account_id(provider: Provider) -> AccountId {
    format!("{provider}-{:08x}", rand::random::<u32>())
        .parse()
        .expect("provider name and hex digits form a valid account id")
}

/// Accept a redirect URL, a query string, `code#state`, or a bare code.
fn parse_pasted(input: &str, expected_state: &str) -> Result<String, ProviderError> {
    let input = input.trim();
    let params = if input.starts_with("http://") || input.starts_with("https://") {
        let url = Url::parse(input)
            .map_err(|error| ProviderError::Other(format!("the pasted URL is invalid: {error}")))?;
        RedirectParams::parse(url.query().unwrap_or_default())
    } else if input.starts_with('?')
        || ["code=", "state=", "error="]
            .iter()
            .any(|key| input.starts_with(key))
    {
        RedirectParams::parse(input.strip_prefix('?').unwrap_or(input))
    } else {
        let (code, state) = match input.split_once('#') {
            Some((code, state)) => (code, state),
            None => (input, expected_state),
        };
        Some(RedirectParams {
            code: Some(code.to_owned()),
            state: Some(state.to_owned()),
            error: None,
        })
    };
    let Some(RedirectParams { code, state, error }) = params else {
        return Err(ProviderError::Other(
            "duplicate parameters in the pasted redirect".to_owned(),
        ));
    };
    if state.as_deref() != Some(expected_state) {
        return Err(ProviderError::Other(
            "the pasted redirect has missing or mismatched OAuth state (state mismatch)".to_owned(),
        ));
    }
    if let Some(reason) = error {
        return Err(ProviderError::Other(format!(
            "login was not completed: {reason}"
        )));
    }
    code.filter(|code| !code.is_empty())
        .ok_or_else(|| ProviderError::Other("no authorization code was pasted".to_owned()))
}

#[derive(Default)]
struct RedirectParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<&'static str>,
}

impl RedirectParams {
    /// Reject ambiguous redirects. Only known error codes can appear in diagnostics.
    fn parse(query: &str) -> Option<Self> {
        let mut params = Self::default();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let duplicate = match key.as_ref() {
                "code" => params.code.replace(value.into_owned()).is_some(),
                "state" => params.state.replace(value.into_owned()).is_some(),
                "error" => {
                    let reason = match value.as_ref() {
                        "access_denied" => "access_denied",
                        _ => "authorization failed",
                    };
                    params.error.replace(reason).is_some()
                }
                _ => false,
            };
            if duplicate {
                return None;
            }
        }
        Some(params)
    }
}

#[cfg(test)]
mod tests;

//! One usage poller per account.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mesh_providers::ProviderError;

use crate::state::{AccountState, AppState};

/// Longest wait after the usage endpoint rate limits us.
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// The wait after a rate-limited poll: doubles each time, up to 15 minutes
/// (never below `base`).
pub fn next_backoff(current: Option<Duration>, base: Duration) -> Duration {
    let doubled = current.unwrap_or(base).saturating_mul(2);
    doubled.min(MAX_BACKOFF.max(base))
}

enum Outcome {
    Done,
    RateLimited,
}

pub async fn run(state: Arc<AppState>, account: Arc<AccountState>) {
    let mut stop = account.subscribe_stop();
    let base = state.settings.poll_interval;
    let mut backoff = None;
    loop {
        if *stop.borrow() {
            return;
        }
        let delay = match poll_once(&state, &account, &mut stop).await {
            Outcome::Done => {
                backoff = None;
                base
            }
            Outcome::RateLimited => {
                let delay = next_backoff(backoff, base);
                backoff = Some(delay);
                tracing::warn!(account = %account.id, ?delay, "usage endpoint rate limited, backing off");
                delay
            }
        };
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = stop.wait_for(|stopped| *stopped) => return,
        }
    }
}

async fn poll_once(
    state: &Arc<AppState>,
    account: &Arc<AccountState>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> Outcome {
    if account.inner().auth_error.is_some() {
        return Outcome::Done;
    }
    let endpoints = state.settings.endpoints(account.provider);
    let mut rejected = None;
    let result = loop {
        let credentials = match state.fresh_credentials(account, rejected.as_deref()).await {
            Ok(credentials) => credentials,
            Err(err) => {
                record_error(state, account, format!("token refresh failed: {err}"));
                return Outcome::Done;
            }
        };
        let result = tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return Outcome::Done,
            result = mesh_providers::poll_usage_with(
                &state.upstream, &credentials, endpoints, Utc::now()
            ) => result,
        };
        if matches!(result, Err(ProviderError::Status { status: 401, .. })) && rejected.is_none() {
            rejected = Some(credentials.access_token);
        } else {
            break result;
        }
    };
    match result {
        Ok(usage) => {
            let now = Utc::now();
            let _ = state.mutate(account, |inner| {
                inner.merge(usage.windows, now);
                inner.last_poll_error = None;
            });
            Outcome::Done
        }
        Err(ProviderError::Status { status: 429, .. }) => {
            record_error(
                state,
                account,
                "usage endpoint rate limited (429)".to_owned(),
            );
            Outcome::RateLimited
        }
        Err(err) => {
            tracing::warn!(account = %account.id, "usage poll failed: {err}");
            record_error(state, account, err.to_string());
            Outcome::Done
        }
    }
}

fn record_error(state: &AppState, account: &AccountState, message: String) {
    let _ = state.mutate(account, |inner| inner.last_poll_error = Some(message));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_cap() {
        let base = Duration::from_secs(90);
        let first = next_backoff(None, base);
        assert_eq!(first, Duration::from_secs(180));
        let second = next_backoff(Some(first), base);
        assert_eq!(second, Duration::from_secs(360));
        assert_eq!(
            next_backoff(Some(Duration::from_secs(600)), base),
            MAX_BACKOFF
        );
        assert_eq!(next_backoff(Some(MAX_BACKOFF), base), MAX_BACKOFF);
    }

    #[test]
    fn backoff_never_below_base() {
        let base = Duration::from_secs(3600);
        assert_eq!(next_backoff(None, base), Duration::from_secs(3600));
    }
}

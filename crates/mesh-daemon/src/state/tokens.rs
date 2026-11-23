use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use mesh_providers::{Credentials, ProviderError, StoredAccount};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{AccountState, AppState};

const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

impl AppState {
    /// Return fresh credentials, coalescing refreshes for the same account.
    /// A rejected access token forces refresh only while it is still current.
    pub async fn fresh_credentials(
        self: &Arc<Self>,
        account: &Arc<AccountState>,
        rejected: Option<&str>,
    ) -> Result<Credentials, ProviderError> {
        let refresh = account.refresh.clone().lock_owned().await;
        let access = self.token_store_access.clone().read_owned().await;
        if !self
            .account(&account.id)
            .is_some_and(|current| Arc::ptr_eq(&current, account))
        {
            return Err(ProviderError::Other("account was removed".to_owned()));
        }
        let credentials = {
            let inner = account.inner();
            if let Some(message) = &inner.auth_error {
                return Err(ProviderError::Auth(message.clone()));
            }
            inner.stored.credentials.clone()
        };
        if rejected != Some(credentials.access_token.as_str())
            && !credentials.needs_refresh(Utc::now())
        {
            return Ok(credentials);
        }

        let (send, receive) = oneshot::channel();
        {
            let mut tasks = self.tasks.lock().expect("background tasks lock poisoned");
            let tasks = tasks
                .as_mut()
                .ok_or_else(|| ProviderError::Other("daemon is shutting down".to_owned()))?;
            Self::reap_tasks(tasks);
            let state = self.clone();
            let account = account.clone();
            tasks.spawn(async move {
                let (_refresh, _access) = (refresh, access);
                let result = state.refresh_and_save(&account, credentials).await;
                let _ = send.send(result);
            });
        }
        receive
            .await
            .map_err(|_| ProviderError::Other("token refresh task stopped".to_owned()))?
    }

    async fn refresh_and_save(
        &self,
        account: &AccountState,
        mut credentials: Credentials,
    ) -> Result<Credentials, ProviderError> {
        let deadline = Instant::now() + REFRESH_TIMEOUT;
        let mut result = self.refresh(account, &mut credentials, deadline).await;
        if matches!(result, Err(ProviderError::Auth(_)))
            && let Some(stored) = self.rotated_account(account, &credentials)
        {
            credentials = stored.credentials.clone();
            account.inner().stored = stored;
            result = self.refresh(account, &mut credentials, deadline).await;
        }

        match result {
            Ok(()) => {
                let (stored, _) = self.mutate(account, |inner| {
                    inner.stored.credentials = credentials.clone();
                    inner.auth_error = None;
                    inner.stored.clone()
                });
                if let Err(err) = self.store.save(&stored) {
                    tracing::error!(account = %account.id, "cannot save refreshed tokens: {err}");
                }
                Ok(credentials)
            }
            Err(ProviderError::Auth(message)) => {
                tracing::warn!(account = %account.id, "login rejected: {message}");
                let _ = self.mutate(account, |inner| inner.auth_error = Some(message.clone()));
                Err(ProviderError::Auth(message))
            }
            Err(err) => Err(err),
        }
    }

    async fn refresh(
        &self,
        account: &AccountState,
        credentials: &mut Credentials,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        let endpoints = self.settings.endpoints(account.provider);
        tokio::time::timeout_at(
            deadline,
            mesh_providers::refresh_with(&self.upstream, credentials, endpoints),
        )
        .await
        .map_err(|_| ProviderError::Other("token refresh timed out".to_owned()))?
    }

    fn rotated_account(
        &self,
        account: &AccountState,
        credentials: &Credentials,
    ) -> Option<StoredAccount> {
        match self.store.load(&account.id) {
            Ok(Some(stored))
                if stored.provider == account.provider
                    && stored.credentials.refresh_token != credentials.refresh_token =>
            {
                Some(stored)
            }
            Ok(_) => None,
            Err(ProviderError::Json(err)) => {
                tracing::warn!(
                    account = %account.id,
                    line = err.line(),
                    column = err.column(),
                    "invalid token file JSON"
                );
                None
            }
            Err(err) => {
                tracing::warn!(account = %account.id, "cannot re-read tokens: {err}");
                None
            }
        }
    }
}

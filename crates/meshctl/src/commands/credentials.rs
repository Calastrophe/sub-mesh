use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use mesh_core::{AccountId, Provider};
use mesh_providers::LoginOptions;

use super::{AppContext, print_json};
use crate::client;
use crate::render;

pub(super) async fn login(ctx: &AppContext, provider: Provider, manual: bool) -> Result<()> {
    let store = ctx.store();
    let options = LoginOptions { manual };
    let account = mesh_providers::login(provider, &ctx.http, &options)
        .await
        .with_context(|| format!("{provider} login failed"))?;
    store.save(&account).with_context(|| {
        format!(
            "cannot save account {} in {}",
            account.id,
            store.dir().display()
        )
    })?;

    let reloaded = match ctx.admin.reload().await {
        Ok(_) => true,
        Err(error) if client::is_not_running(&error) => {
            eprintln!("note: {error}. The daemon will load the account when it starts.");
            false
        }
        Err(error) => {
            return Err(error.context(format!(
                "account {} was saved, but the daemon did not reload",
                account.id
            )));
        }
    };

    let label = account.label.as_deref().unwrap_or("(no label)");
    if ctx.json {
        print_json(&serde_json::json!({
            "id": account.id,
            "provider": account.provider,
            "label": account.label,
        }))
    } else {
        println!("logged in: {} ({} {label})", account.id, account.provider);
        if reloaded {
            println!("mesh-daemon reloaded its accounts");
        }
        Ok(())
    }
}

pub(super) async fn probe(ctx: &AppContext, id: &AccountId) -> Result<()> {
    let store = ctx.store();
    let mut account = store
        .load_all()
        .with_context(|| format!("cannot read accounts in {}", store.dir().display()))?
        .into_iter()
        .find(|account| &account.id == id)
        .ok_or_else(|| anyhow!("no saved account {id} in {}", store.dir().display()))?;

    if account.credentials.needs_refresh(Utc::now()) {
        eprintln!(
            "refreshing the expiring access token for {id}. If the provider rotates the refresh \
             token, mesh-daemon may briefly hold a stale copy before reloading."
        );
        mesh_providers::refresh(account.provider, &ctx.http, &mut account.credentials)
            .await
            .context("token refresh failed")?;
        store.save(&account).with_context(|| {
            format!(
                "cannot save refreshed tokens for {id} in {}",
                store.dir().display()
            )
        })?;
        if let Err(error) = ctx.admin.reload().await
            && !client::is_not_running(&error)
        {
            eprintln!(
                "warning: refreshed tokens for {id} were saved, but mesh-daemon did not reload: \
                 {error:#}. Restart mesh-daemon to load the saved tokens."
            );
        }
    }

    let now = Utc::now();
    let usage = mesh_providers::poll_usage(account.provider, &ctx.http, &account.credentials, now)
        .await
        .context("usage request failed")?;

    if ctx.json {
        return print_json(&usage);
    }
    println!("raw usage response:");
    println!("{}", serde_json::to_string_pretty(&usage.raw)?);
    println!();
    if usage.windows.is_empty() {
        println!("parsed windows: none");
    } else {
        println!("parsed windows:");
        print!("{}", render::window_table(&usage.windows, None, now, "  "));
    }
    Ok(())
}

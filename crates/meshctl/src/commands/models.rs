use anyhow::{Context, Result};
use clap::Subcommand;
use mesh_core::admin::{AdminAccount, AdminStatus, SetModels};
use mesh_core::{AccountId, ModelAllowlist, Provider};
use serde::Serialize;

use super::reserve::find_account;
use super::{AppContext, print_result};
use crate::render::{format_default_models, format_models};

#[derive(Subcommand)]
pub(crate) enum ModelsCommand {
    /// Show the default model allowlists, or an account's allowlist.
    Show { id: Option<AccountId> },
    /// Set the models an account may use. A `*` in a pattern matches any text.
    Set {
        id: AccountId,
        /// Model ids or patterns such as `claude-sonnet-*`, matched ignoring case.
        #[arg(required = true, value_name = "PATTERN")]
        patterns: Vec<String>,
    },
    /// Remove an account's own allowlist so the default applies again.
    Clear { id: AccountId },
}

/// What `models show ID` prints with `--json`.
#[derive(Serialize)]
struct AccountModels<'a> {
    account: &'a AccountId,
    /// The allowlist in effect; `None` allows every model.
    models: Option<&'a ModelAllowlist>,
    /// Whether the account set the allowlist itself, rather than inheriting
    /// the default.
    own: bool,
}

pub(super) async fn run(ctx: &AppContext, command: ModelsCommand) -> Result<()> {
    let (body, message) = match command {
        ModelsCommand::Show { id } => return show(ctx, id.as_ref()).await,
        ModelsCommand::Set { id, patterns } => {
            let models = ModelAllowlist::new(patterns).context("invalid model allowlist")?;
            let message = format!("{id} models: {}", format_models(Some(&models)));
            let body = SetModels {
                account: id,
                models: Some(models),
            };
            (body, message)
        }
        ModelsCommand::Clear { id } => {
            let message = format!("{id} now uses the default models");
            let body = SetModels {
                account: id,
                models: None,
            };
            (body, message)
        }
    };
    let status = ctx.admin.set_models(&body).await?;
    print_result(ctx, &status, || println!("{message}"))
}

async fn show(ctx: &AppContext, id: Option<&AccountId>) -> Result<()> {
    let status = ctx.admin.status().await?;
    let Some(id) = id else {
        return print_result(ctx, &status.default_models, || {
            print!("{}", format_default_models(&status.default_models));
        });
    };

    let account = find_account(&status, id)?;
    let view = AccountModels {
        account: id,
        models: account.report.models.as_ref(),
        own: account.models.is_some(),
    };
    print_result(ctx, &view, || {
        println!("{id} {}", describe(&status, account));
    })
}

/// A sentence ending with the models an account may use, and where they
/// come from.
fn describe(status: &AdminStatus, account: &AdminAccount) -> String {
    let models = format_models(account.report.models.as_ref());
    if account.models.is_some() {
        format!("has its own models: {models}")
    } else if has_default(status, account.report.provider) {
        format!("has no models of its own; the default applies: {models}")
    } else {
        format!("has no models of its own and no default is set: {models}")
    }
}

fn has_default(status: &AdminStatus, provider: Provider) -> bool {
    status.default_models.iter().any(|(p, _)| *p == provider)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("meshctl").chain(args.iter().copied()))
    }

    #[test]
    fn parses_show_set_and_clear() {
        let Some(Command::Models(ModelsCommand::Show { id })) =
            parse(&["models", "show"]).unwrap().command
        else {
            panic!("expected models show");
        };
        assert!(id.is_none());

        let Some(Command::Models(ModelsCommand::Show { id })) =
            parse(&["models", "show", "claude-1"]).unwrap().command
        else {
            panic!("expected models show");
        };
        assert_eq!(id.unwrap().as_str(), "claude-1");

        let Some(Command::Models(ModelsCommand::Set { id, patterns })) = parse(&[
            "models",
            "set",
            "claude-1",
            "claude-sonnet-*",
            "claude-haiku-*",
        ])
        .unwrap()
        .command
        else {
            panic!("expected models set");
        };
        assert_eq!(id.as_str(), "claude-1");
        assert_eq!(patterns, ["claude-sonnet-*", "claude-haiku-*"]);

        let Some(Command::Models(ModelsCommand::Clear { id })) =
            parse(&["models", "clear", "claude-1"]).unwrap().command
        else {
            panic!("expected models clear");
        };
        assert_eq!(id.as_str(), "claude-1");
    }

    #[test]
    fn set_needs_a_pattern_and_clear_needs_an_account() {
        assert!(parse(&["models", "set", "claude-1"]).is_err());
        assert!(parse(&["models", "set"]).is_err());
        assert!(parse(&["models", "clear"]).is_err());
    }

    #[test]
    fn describes_where_the_models_come_from() {
        let list = ModelAllowlist::new(["claude-sonnet-*"]).unwrap();
        let mut status = crate::render::tests::sample_status();
        assert_eq!(
            describe(&status, &status.accounts[0]),
            "has no models of its own and no default is set: all"
        );

        status.default_models = vec![(Provider::Claude, list.clone())];
        status.accounts[0].report.models = Some(list.clone());
        assert_eq!(
            describe(&status, &status.accounts[0]),
            "has no models of its own; the default applies: claude-sonnet-*"
        );

        status.accounts[0].models = Some(list);
        assert_eq!(
            describe(&status, &status.accounts[0]),
            "has its own models: claude-sonnet-*"
        );
    }

    #[test]
    fn set_rejects_bad_patterns_before_calling_the_daemon() {
        assert!(ModelAllowlist::new(["bad pattern"]).is_err());
        assert!(ModelAllowlist::new([""]).is_err());
    }
}

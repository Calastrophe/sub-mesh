use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use clap::{Args, Subcommand};
use mesh_core::admin::{AdminAccount, AdminStatus, SetReserve};
use mesh_core::{AccountId, ReserveConfig};

use super::{AppContext, print_result};
use crate::render::{effective_reserve, format_reserve, window_table};

#[derive(Subcommand)]
pub(crate) enum ReserveCommand {
    /// Show the default reserve, or an account's reserve.
    Show { id: Option<AccountId> },
    /// Change the default reserve or one account's reserve.
    Set {
        /// Change this account's own reserve instead of the default.
        #[arg(long, value_name = "ID")]
        account: Option<AccountId>,
        #[command(flatten)]
        changes: ReserveChanges,
    },
    /// Remove an account's own reserve so the default applies again.
    Clear {
        #[arg(long, value_name = "ID")]
        account: AccountId,
    },
}

/// Parse a reserve value written as a fraction (`0.3`) or a percentage
/// (`30%`). The result must be within `0..=1`.
fn parse_fraction(text: &str) -> Result<f64, String> {
    let text = text.trim();
    let (number, divisor) = match text.strip_suffix('%') {
        Some(number) => (number.trim_end(), 100.0),
        None => (text, 1.0),
    };
    let value: f64 = number.parse().map_err(|_| {
        format!("{text:?} is not a number; use a fraction like 0.3 or a percentage like 30%")
    })?;
    let fraction = value / divisor;
    if (0.0..=1.0).contains(&fraction) {
        Ok(fraction)
    } else {
        Err(format!(
            "{text:?} is out of range; a reserve must be between 0 and 1 (0% to 100%)"
        ))
    }
}

#[derive(Args, Default)]
pub(crate) struct ReserveChanges {
    /// New default reserve for a window without a 5-hour or weekly setting.
    #[arg(long, value_name = "FRACTION", value_parser = parse_fraction)]
    default: Option<f64>,
    /// New reserve for the 5-hour window.
    #[arg(long, value_name = "FRACTION", value_parser = parse_fraction)]
    five_hour: Option<f64>,
    /// New reserve for the weekly window.
    #[arg(long, value_name = "FRACTION", value_parser = parse_fraction)]
    weekly: Option<f64>,
}

impl ReserveChanges {
    fn is_empty(&self) -> bool {
        self.default.is_none() && self.five_hour.is_none() && self.weekly.is_none()
    }

    fn apply(self, mut base: ReserveConfig) -> ReserveConfig {
        base.default = self.default.unwrap_or(base.default);
        base.five_hour = self.five_hour.or(base.five_hour);
        base.weekly = self.weekly.or(base.weekly);
        base
    }
}

pub(super) fn find_account<'status>(
    status: &'status AdminStatus,
    id: &AccountId,
) -> Result<&'status AdminAccount> {
    status
        .accounts
        .iter()
        .find(|account| &account.report.id == id)
        .ok_or_else(|| anyhow!("no account {id}; run `meshctl status` to list accounts"))
}

pub(super) async fn run(ctx: &AppContext, command: ReserveCommand) -> Result<()> {
    let (body, message) = match command {
        ReserveCommand::Show { id } => return show(ctx, id.as_ref()).await,
        ReserveCommand::Set { account, changes } => {
            if changes.is_empty() {
                bail!("nothing to change; pass --default, --five-hour, or --weekly");
            }
            let status = ctx.admin.status().await?;
            let base = match &account {
                Some(id) => effective_reserve(find_account(&status, id)?, &status.default_reserve),
                None => &status.default_reserve,
            };
            let reserve = changes.apply(base.clone());
            let target = account.as_ref().map_or("default", AccountId::as_str);
            let message = format!("{target} reserve: {}", format_reserve(&reserve));
            let body = SetReserve {
                account,
                reserve: Some(reserve),
            };
            (body, message)
        }
        ReserveCommand::Clear { account } => {
            let message = format!("{account} now uses the default reserve");
            let body = SetReserve {
                account: Some(account),
                reserve: None,
            };
            (body, message)
        }
    };
    let status = ctx.admin.set_reserve(&body).await?;
    print_result(ctx, &status, || println!("{message}"))
}

async fn show(ctx: &AppContext, id: Option<&AccountId>) -> Result<()> {
    let status = ctx.admin.status().await?;
    let Some(id) = id else {
        return print_result(ctx, &status.default_reserve, || {
            println!(
                "default reserve: {}",
                format_reserve(&status.default_reserve)
            );
        });
    };

    let account = find_account(&status, id)?;
    print_result(ctx, &account.reserve, || {
        let reserve = effective_reserve(account, &status.default_reserve);
        let message = if account.reserve.is_some() {
            "has its own reserve:"
        } else {
            "has no reserve of its own; the default applies:"
        };
        println!("{id} {message} {}", format_reserve(reserve));
        if !account.report.windows.is_empty() {
            println!("effective reserve per window:");
            print!(
                "{}",
                window_table(&account.report.windows, Some(reserve), Utc::now(), "  ")
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fractions_and_percentages() {
        assert_eq!(parse_fraction("0.3"), Ok(0.3));
        assert_eq!(parse_fraction("30%"), Ok(0.3));
        assert_eq!(parse_fraction(" 12.5 % "), Ok(0.125));
        assert_eq!(parse_fraction("0"), Ok(0.0));
        assert_eq!(parse_fraction("1"), Ok(1.0));
        assert_eq!(parse_fraction("100%"), Ok(1.0));
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
            "", "abc", "-0.1", "1.5", "150%", "-5%", "NaN", "inf", "30%%", "30",
        ] {
            assert!(parse_fraction(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn apply_overrides_only_given_fields() {
        let base = ReserveConfig {
            default: 0.3,
            five_hour: Some(0.1),
            weekly: None,
        };
        let changes = ReserveChanges {
            default: Some(0.4),
            weekly: Some(0.2),
            ..ReserveChanges::default()
        };
        let merged = changes.apply(base);
        assert_eq!(merged.default, 0.4);
        assert_eq!(merged.five_hour, Some(0.1));
        assert_eq!(merged.weekly, Some(0.2));
    }

    #[test]
    fn detects_empty_changes() {
        assert!(ReserveChanges::default().is_empty());
    }
}

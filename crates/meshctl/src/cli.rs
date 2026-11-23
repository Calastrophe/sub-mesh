use std::path::PathBuf;

use clap::{Parser, Subcommand};
use mesh_core::{AccountId, Provider};

use crate::commands::models::ModelsCommand;
use crate::commands::reserve::ReserveCommand;

#[derive(Parser)]
#[command(version, about = "Control the local mesh daemon")]
pub(crate) struct Cli {
    /// Base URL of the daemon's admin API.
    #[arg(long, global = true, env = "MESH_ADMIN", value_name = "URL")]
    pub(crate) admin: Option<String>,

    /// Data directory; accounts are stored in <DATA_DIR>/accounts and the daemon's admin token in <DATA_DIR>/admin-token.
    #[arg(long, global = true, env = "MESH_DATA_DIR", value_name = "PATH")]
    pub(crate) data_dir: Option<PathBuf>,

    /// Print command results as JSON.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Log in to a provider and save the account.
    Login {
        provider: Provider,
        /// Paste the redirect URL or code instead of using a browser callback.
        #[arg(long)]
        manual: bool,
    },
    /// Show the daemon's state (the default command).
    Status,
    /// Stop sharing an account.
    Pause { id: AccountId },
    /// Resume sharing an account.
    Resume { id: AccountId },
    /// Remove an account and its saved tokens.
    Remove {
        id: AccountId,
        /// Do not ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Show or change reserves.
    #[command(subcommand)]
    Reserve(ReserveCommand),
    /// Show or change which models an account may use.
    #[command(subcommand)]
    Models(ModelsCommand),
    /// Check usage directly with saved tokens. Reload the daemon if tokens are refreshed.
    Probe { id: AccountId },
}

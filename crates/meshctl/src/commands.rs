mod credentials;
pub(crate) mod models;
pub(crate) mod reserve;

use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use mesh_core::admin;
use mesh_core::protocol::DEFAULT_ADMIN_PORT;
use mesh_providers::TokenStore;
use reqwest::{Client, Method};
use serde::Serialize;
use url::Url;

use crate::cli::{Cli, Command};
use crate::client::{self, AdminClient};
use crate::render;

struct AppContext {
    admin: AdminClient,
    http: Client,
    data_dir: PathBuf,
    json: bool,
}

impl AppContext {
    fn new(admin: Option<String>, data_dir: Option<PathBuf>, json: bool) -> Result<Self> {
        let admin_url = admin.unwrap_or_else(|| format!("http://127.0.0.1:{DEFAULT_ADMIN_PORT}"));
        let admin_url =
            Url::parse(&admin_url).with_context(|| format!("invalid admin URL {admin_url:?}"))?;
        let data_dir = match data_dir {
            Some(dir) => dir,
            None => mesh_providers::default_data_dir()
                .ok_or_else(|| anyhow!("cannot find the data directory; pass --data-dir"))?,
        };
        Ok(Self {
            admin: AdminClient::new(admin_url.as_str(), data_dir.join(admin::TOKEN_FILE))?,
            http: client::http_client()?,
            data_dir,
            json,
        })
    }

    fn store(&self) -> TokenStore {
        TokenStore::new(self.data_dir.join("accounts"))
    }
}

pub(crate) async fn run(cli: Cli) -> Result<()> {
    let Cli {
        admin,
        data_dir,
        json,
        command,
    } = cli;
    let ctx = AppContext::new(admin, data_dir, json)?;
    let (method, path, message) = match command.unwrap_or(Command::Status) {
        Command::Login { provider, manual } => {
            return credentials::login(&ctx, provider, manual).await;
        }
        Command::Status => {
            let status = ctx.admin.status().await?;
            return print_result(&ctx, &status, || {
                print!("{}", render::render_status(&status, Utc::now()));
            });
        }
        Command::Pause { id } => (Method::POST, admin::pause_path(&id), format!("paused {id}")),
        Command::Resume { id } => (
            Method::POST,
            admin::resume_path(&id),
            format!("resumed {id}"),
        ),
        Command::Remove { id, yes } => {
            if !yes && !confirm(&format!("Remove account {id} and delete its saved tokens?"))? {
                return print_result(&ctx, &serde_json::json!({ "cancelled": true }), || {
                    println!("cancelled");
                });
            }
            (
                Method::DELETE,
                admin::account_path(&id),
                format!("removed {id}"),
            )
        }
        Command::Reserve(command) => return reserve::run(&ctx, command).await,
        Command::Models(command) => return models::run(&ctx, command).await,
        Command::Probe { id } => return credentials::probe(&ctx, &id).await,
    };
    let status = ctx.admin.call(method, &path).await?;
    print_result(&ctx, &status, || println!("{message}"))
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    use std::io::Write as _;

    let mut stdout = io::stdout().lock();
    let json = serde_json::to_string_pretty(value).context("cannot serialize command result")?;
    writeln!(stdout, "{json}").context("cannot write command result")
}

fn print_result(ctx: &AppContext, value: &impl Serialize, print_text: impl FnOnce()) -> Result<()> {
    if ctx.json {
        print_json(value)
    } else {
        print_text();
        Ok(())
    }
}

/// Ask a yes/no question on the terminal. Anything but `y` or `yes` is no.
fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead as _, Write as _};

    {
        let mut stderr = io::stderr().lock();
        write!(stderr, "{question} [y/N] ").context("cannot write confirmation prompt")?;
        stderr.flush().context("cannot flush confirmation prompt")?;
    }
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("cannot read confirmation")?;
    let answer = answer.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;

use account::rest::SiteClient;
use account::seed::create_missing;
use account::stock::stock;
use config::load_kit;

mod account;
mod bot;
mod brain;
mod config;
mod metrics;
mod probes;
mod run;
#[cfg(test)]
pub mod testing;
mod wire;
mod world;

#[derive(Parser)]
#[command(
    name = "loadtest",
    about = "Logs bot players into a running game server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Seed {
        #[arg(long)]
        site: String,
        #[arg(long)]
        email: String,
        #[arg(long, env = "LOADTEST_PASSWORD")]
        password: String,
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        #[arg(long, default_value_t = 50)]
        count: usize,
        #[arg(long, default_value = "Loadbot")]
        prefix: String,
        #[arg(long, default_value = "kit.yaml")]
        kit: String,
        #[arg(long)]
        restock: bool,
    },
    Run {
        #[arg(long)]
        site: String,
        #[arg(long)]
        email: String,
        #[arg(long, env = "LOADTEST_PASSWORD")]
        password: String,
        #[arg(long, default_value = "127.0.0.1:5555")]
        server: String,
        #[arg(long)]
        items: String,
        #[arg(long, default_value = "../server/assets/areas.yaml")]
        areas: String,
        #[arg(long, default_value = "run.yaml")]
        config: String,
        #[arg(long, default_value_t = 60)]
        ramp_secs: u64,
        #[arg(long, default_value_t = 600)]
        duration_secs: u64,
        #[arg(long)]
        max_bots: Option<usize>,
        #[arg(long, default_value = "Loadbot")]
        prefix: String,
        #[arg(long, default_value = "run.json")]
        out: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Seed {
            site,
            email,
            password,
            database_url,
            count,
            prefix,
            kit,
            restock,
        } => {
            let kit = load_kit(&kit)?;

            let site = SiteClient::new(&site)?;
            let session = site.authenticate(&email, &password).await?;
            let characters = site.characters(&session).await?;

            let created = if restock {
                Vec::new()
            } else {
                create_missing(
                    &site,
                    &session,
                    &characters,
                    &prefix,
                    count,
                    kit.character.vocation,
                )
                .await?
            };

            let characters = site.characters(&session).await?;
            let pool = PgPoolOptions::new().connect(&database_url).await?;

            println!("created {} character(s)", created.len());

            let bot_prefix = format!("{prefix} ");
            let mut stocked = 0;
            for character in characters
                .iter()
                .filter(|c| c.name.starts_with(&bot_prefix))
            {
                stock(&pool, character.id, &kit)
                    .await
                    .with_context(|| format!("stocking {}", character.name))?;
                stocked += 1;
            }

            println!("stocked {stocked} character(s)");
            Ok(())
        }
        Command::Run {
            site,
            email,
            password,
            server,
            items,
            areas,
            config,
            ramp_secs,
            duration_secs,
            max_bots,
            prefix,
            out,
        } => {
            run::run(run::RunArgs {
                site,
                email,
                password,
                server,
                items,
                areas,
                config,
                ramp: std::time::Duration::from_secs(ramp_secs),
                duration: std::time::Duration::from_secs(duration_secs),
                max_bots,
                prefix,
                out,
            })
            .await
        }
    }
}

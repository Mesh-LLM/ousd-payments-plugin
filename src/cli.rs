use crate::{config::Config, runtime, tempo::keys};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "tempo",
    version,
    about = "OUSD payments and refundable identity bonds on Tempo"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Run the Mesh plugin (the default).
    Serve,
    /// Set up the local wallet before the plugin runs. Everything else is a
    /// control operation on the running plugin.
    Wallet {
        #[command(subcommand)]
        command: WalletCommand,
    },
}
#[derive(Subcommand)]
enum WalletCommand {
    /// Initialize an encrypted wallet for the configured network. No transaction.
    Init,
    /// Read the persisted public address without unlocking or network access.
    Address,
}

/// Run the plugin or an explicit local wallet setup command.
pub async fn run() -> Result<()> {
    if std::env::args().any(|arg| arg == "--manifest") {
        println!(
            "{}",
            mesh_llm_plugin::package_manifest_json(&runtime::manifest())?
        );
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => runtime::run().await,
        Command::Wallet { command } => {
            let config = Config::load_from(cli.config)?;
            let settings = config.wallet;
            let dir = config.data_dir;
            let value = match command {
                // Without a configured password file the wallet gets a random
                // one in its state directory, as when `fund` creates it.
                WalletCommand::Init => {
                    let password = match settings.password_file {
                        Some(path) => path,
                        None => {
                            keys::secure_dir(&dir)?;
                            let path = keys::generated_password(&dir);
                            keys::create_password(&path)?;
                            path
                        }
                    };
                    serde_json::to_value(keys::initialize(&dir, &password, settings.network)?)?
                }
                WalletCommand::Address => {
                    serde_json::to_value(keys::load_profile(&dir, settings.network)?)?
                }
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}

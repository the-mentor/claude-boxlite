mod attach;
mod boxopts;
mod commands;
mod config;
mod env;
mod naming;
mod proto;
mod secrets;
mod sidecar;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cbox", about = "Drive BoxLite micro-VMs for this repo")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a box and attach this terminal to it.
    Up {
        name: Option<String>,
        #[arg(short, long)]
        force: bool,
        #[arg(short = 'c', long = "cwd")]
        cwd_mount: bool,
        #[arg(short = 'v', long = "volume")]
        volumes: Vec<String>,
        #[arg(short = 'e', long = "env")]
        env_flags: Vec<String>,
        #[arg(short = 'i', long, default_value = "claude-boxlite-custom")]
        image: String,
        #[arg(long)]
        config: Option<PathBuf>,
        /// NAME=ENV_VAR@host[,host...]
        #[arg(long = "secret")]
        secret_flags: Vec<String>,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Print the box name that would be used here, and why.
    Name { name: Option<String> },
    /// Stop and remove a box.
    Down { name: Option<String> },
    /// List boxes across every per-name home.
    List {
        /// Include stopped boxes.
        #[arg(short, long)]
        all: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Name { name } => {
            let cwd = std::env::current_dir().context("cannot read current directory")?;
            let resolved = naming::resolve(name.as_deref(), &cwd);
            println!("{}  (derived: {})", resolved.name, resolved.source.describe());
        }
        Commands::Up {
            name, force, cwd_mount, volumes, env_flags, image, config, secret_flags, cmd,
        } => {
            commands::up::run(commands::up::UpArgs {
                name, force, cwd_mount, volumes, env_flags, image, config, secret_flags, cmd,
            })
            .await?;
        }
        Commands::Down { name } => commands::down::run(name).await?,
        Commands::List { all } => commands::list::run(all).await?,
    }
    Ok(())
}

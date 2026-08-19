mod attach;
mod boxopts;
mod client;
mod commands;
mod config;
mod env;
mod envfile;
mod naming;
mod proto;
mod secrets;
mod server;
mod sidecar;
#[cfg(test)]
mod test_env_lock;

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
        /// KEY=VALUE file to load before resolving credentials. Defaults to
        /// $CBOX_ENV_FILE, then ~/.config/cbox/env.
        #[arg(long = "env-file")]
        env_file: Option<PathBuf>,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Print the box name that would be used here, and why.
    Name { name: Option<String> },
    /// Open a session in a running box.
    Exec {
        name: Option<String>,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
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
            name, force, cwd_mount, volumes, env_flags, image, config, secret_flags, env_file, cmd,
        } => {
            commands::up::run(commands::up::UpArgs {
                name, force, cwd_mount, volumes, env_flags, image, config, secret_flags, env_file,
                cmd,
            })
            .await?;
        }
        Commands::Exec { name, cmd } => commands::exec::run(name, cmd).await?,
        Commands::Down { name } => commands::down::run(name).await?,
        Commands::List { all } => commands::list::run(all).await?,
    }
    Ok(())
}

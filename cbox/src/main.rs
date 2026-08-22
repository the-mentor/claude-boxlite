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
mod stdin_reader;
#[cfg(test)]
mod test_env_lock;
mod terminal_guard;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cbox",
    about = "Drive BoxLite micro-VMs for this repo",
    // `version` with no value takes CARGO_PKG_VERSION, so `-V` tracks
    // Cargo.toml automatically rather than needing a hand-edited string.
    version
)]
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
        /// Container rootfs disk size in GB. The COW overlay is sparse and
        /// grows with actual usage; the virtual size is max(this, base image
        /// size), so smaller values are ignored. Defaults to 10GB, which
        /// gives headroom for in-box docker pull/apt/npm/build caches.
        #[arg(long = "disk-size")]
        disk_size: Option<u64>,
        /// Let the box outlive this session so `cbox exec` can reach it
        /// later. Without this, closing the terminal lets boxlite's own
        /// watchdog stop the VM -- the disk and box record survive, and a
        /// later `cbox up` resumes it (a cold boot, not a suspend/resume).
        /// Mirrors `boxlite run`'s `-d`.
        #[arg(short = 'd', long = "detach")]
        detach: bool,
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
            disk_size, detach,
        } => {
            commands::up::run(commands::up::UpArgs {
                name, force, cwd_mount, volumes, env_flags, image, config, secret_flags, env_file,
                cmd, disk_size_gb: disk_size, detach,
            })
            .await?;
        }
        Commands::Exec { name, cmd } => commands::exec::run(name, cmd).await?,
        Commands::Down { name } => commands::down::run(name).await?,
        Commands::List { all } => commands::list::run(all).await?,
    }
    Ok(())
}

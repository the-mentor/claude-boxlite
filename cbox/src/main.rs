mod config;
mod naming;

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
    /// Print the box name that would be used here, and why.
    Name {
        /// Explicit box name (overrides derivation).
        name: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Name { name } => {
            let cwd = std::env::current_dir().context("cannot read current directory")?;
            let resolved = naming::resolve(name.as_deref(), &cwd);
            println!("{}  (derived: {})", resolved.name, resolved.source.describe());
        }
    }
    Ok(())
}

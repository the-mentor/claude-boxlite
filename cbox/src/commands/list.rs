//! `cbox list` — every box across every per-name home.
//!
//! Homes are per box name, so listing means walking them all. The box matching
//! the current directory is marked, since the derived default is otherwise
//! invisible.

use anyhow::{Context, Result};
use boxlite::{BoxStatus, BoxliteOptions, BoxliteRuntime};

use crate::{config, naming};

pub async fn run(all: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read current directory")?;
    let here = naming::resolve(None, &cwd).name;

    let root = config::box_home("_").parent().unwrap().to_path_buf();
    let Ok(entries) = std::fs::read_dir(&root) else {
        println!("cbox: no boxes yet");
        return Ok(());
    };

    let mut found = false;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(String::from) else { continue };

        let runtime = match BoxliteRuntime::new(BoxliteOptions {
            home_dir: entry.path(),
            image_registries: vec![],
        }) {
            Ok(r) => r,
            // A home locked by a running `cbox up` cannot be opened here —
            // the common case being a second terminal listing while the
            // first is still attached. Report it and move on rather than
            // aborting the whole listing.
            Err(_) => {
                println!("{:<28} (in use)", name);
                found = true;
                continue;
            }
        };

        for info in runtime.list_info().await.unwrap_or_default() {
            if !all && info.status == BoxStatus::Stopped {
                continue;
            }
            let marker = if name == here { " <- here" } else { "" };
            println!(
                "{:<28} {:<12}{}",
                info.name.unwrap_or_else(|| name.clone()),
                info.status,
                marker
            );
            found = true;
        }
    }

    if !found {
        println!("cbox: no boxes yet");
    }
    Ok(())
}

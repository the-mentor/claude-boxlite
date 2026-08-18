//! `cbox exec` — open a session in a running box.

use anyhow::{Context, Result};
use boxlite::{BoxliteOptions, BoxliteRuntime};

use crate::{attach, client, config, naming};

pub async fn run(name: Option<String>, cmd: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read current directory")?;
    let resolved = naming::resolve(name.as_deref(), &cwd);
    let home = config::box_home(&resolved.name);
    let cmd = if cmd.is_empty() {
        vec!["claude".to_string(), "--continue".to_string()]
    } else {
        cmd
    };

    match client::route(&home).await? {
        client::Route::Socket(stream) => {
            let code = client::proxy(stream, &cmd).await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        client::Route::OwnRuntime => {
            let runtime = BoxliteRuntime::new(BoxliteOptions {
                home_dir: home,
                image_registries: vec![],
            })
            .context("failed to open the BoxLite runtime")?;

            let litebox = runtime
                .get(&resolved.name)
                .await?
                .with_context(|| format!("no box named {}", resolved.name))?;
            attach::attach(&litebox, &cmd).await
        }
    }
}

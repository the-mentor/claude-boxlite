//! `cbox up` — create a box and attach the terminal to it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use boxlite::{BoxCommand, BoxliteOptions, BoxliteRuntime, LiteBox};

use crate::{attach, boxopts, config, env, naming, secrets};

pub struct UpArgs {
    pub name: Option<String>,
    pub force: bool,
    pub cwd_mount: bool,
    pub volumes: Vec<String>,
    pub env_flags: Vec<String>,
    pub image: String,
    pub config: Option<PathBuf>,
    pub secret_flags: Vec<String>,
    pub cmd: Vec<String>,
}

pub async fn run(args: UpArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read current directory")?;
    let resolved = naming::resolve(args.name.as_deref(), &cwd);
    let name = resolved.name;

    // Secrets first: their source variables must be withheld from passthrough.
    let mut specs = Vec::new();
    if std::env::var("GH_TOKEN").is_ok() || std::env::var("GITHUB_TOKEN").is_ok() {
        specs.push(secrets::github_spec());
    }
    for flag in &args.secret_flags {
        specs.push(secrets::parse_secret_flag(flag)?);
    }
    let built = secrets::build(&specs)?;
    let has_github = specs.iter().any(|s| s.name == "gh");

    let passthrough: Vec<String> =
        env::DEFAULT_PASSTHROUGH.iter().map(|s| s.to_string()).collect();
    let mut plain = env::compose(&args.env_flags, &passthrough, &built.source_vars)?;
    plain.extend(built.env.clone());
    plain.push((
        "TERM".into(),
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
    ));
    plain.push(("BOX_NAME".into(), name.clone()));

    let home = config::box_home(&name);
    std::fs::create_dir_all(&home)
        .with_context(|| format!("cannot create box home {}", home.display()))?;

    let registries = config::resolve_config_path(args.config.as_deref())
        .map(|p| config::load_registries(&p))
        .unwrap_or_default();

    let runtime = BoxliteRuntime::new(BoxliteOptions {
        home_dir: home,
        image_registries: registries,
    })
    .context("failed to open the BoxLite runtime")?;

    if args.force {
        let _ = runtime.remove(&name, true).await;
    }

    let flags = boxopts::UpFlags {
        image: args.image,
        cwd_mount: args.cwd_mount,
        volumes: args.volumes,
        cmd: if args.cmd.is_empty() { vec!["claude".into()] } else { args.cmd },
        invocation_dir: cwd,
    };
    let options = boxopts::build(&flags, built.secrets, plain)?;

    println!("cbox: starting {name} ({})", resolved.source.describe());
    let litebox = runtime
        .create(options, Some(name.clone()))
        .await
        .context("failed to create the box")?;
    litebox.start().await.context("failed to start the box")?;

    if has_github {
        run_git_bootstrap(&litebox).await;
    }

    attach::attach(&litebox, &flags.cmd).await
}

/// Point git at the pre-encoded secret so GitHub operations authenticate.
///
/// This step is entirely best-effort: it must never take `cbox up` down with
/// it. Two things can go wrong — the image might not have git at all, or the
/// bootstrap script itself might fail — and both are handled by warning to
/// stderr and moving on, never by propagating an error. A box that skipped
/// the bootstrap 401s on its first git operation against GitHub, which is
/// visible immediately and named by the warning below, and that outcome is
/// strictly better than killing the whole session over an inessential
/// configuration step.
async fn run_git_bootstrap(litebox: &LiteBox) {
    let has_git = match litebox
        .exec(BoxCommand::new("sh").args(["-lc", "command -v git >/dev/null 2>&1"]))
        .await
    {
        Ok(probe) => match probe.wait().await {
            Ok(result) => result.success(),
            Err(e) => {
                eprintln!(
                    "cbox: warning: could not check for git in the box ({e}); \
                     skipping GitHub credential bootstrap. git operations against \
                     GitHub will fail to authenticate until this is configured manually."
                );
                return;
            }
        },
        Err(e) => {
            eprintln!(
                "cbox: warning: could not check for git in the box ({e}); \
                 skipping GitHub credential bootstrap. git operations against \
                 GitHub will fail to authenticate until this is configured manually."
            );
            return;
        }
    };

    if !has_git {
        eprintln!(
            "cbox: warning: git not found in the box; skipping GitHub credential bootstrap. \
             git operations against GitHub will fail to authenticate."
        );
        return;
    }

    let script = secrets::git_bootstrap_script();
    match litebox.exec(BoxCommand::new("sh").args(["-lc", &script])).await {
        Ok(e) => match e.wait().await {
            Ok(result) if result.success() => {}
            Ok(result) => eprintln!(
                "cbox: warning: git credential bootstrap exited with code {}; \
                 git operations against GitHub will fail to authenticate.",
                result.exit_code
            ),
            Err(e) => eprintln!(
                "cbox: warning: git credential bootstrap failed ({e}); \
                 git operations against GitHub will fail to authenticate."
            ),
        },
        Err(e) => eprintln!(
            "cbox: warning: git credential bootstrap failed to start ({e}); \
             git operations against GitHub will fail to authenticate."
        ),
    }
}

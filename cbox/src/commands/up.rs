//! `cbox up` — create a box and attach the terminal to it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use boxlite::{BoxCommand, BoxliteOptions, BoxliteRuntime, LiteBox};

use crate::{attach, boxopts, config, env, envfile, naming, secrets, sidecar};

pub struct UpArgs {
    pub name: Option<String>,
    pub force: bool,
    pub cwd_mount: bool,
    pub volumes: Vec<String>,
    pub env_flags: Vec<String>,
    pub image: String,
    pub config: Option<PathBuf>,
    pub secret_flags: Vec<String>,
    pub env_file: Option<PathBuf>,
    pub cmd: Vec<String>,
    pub disk_size_gb: Option<u64>,
}

pub async fn run(args: UpArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read current directory")?;
    let resolved = naming::resolve(args.name.as_deref(), &cwd);
    let name = resolved.name;

    // Load the env file, if any, before anything below reads the process
    // environment: passthrough selection, secret source lookup, and GitHub
    // token detection all need to see whatever it provides. A variable
    // already set in the real environment wins — this only fills gaps.
    let env_file_path = envfile::resolve_path(args.env_file.as_deref());
    if let Some(path) = &env_file_path {
        envfile::apply(path);
    }

    // Secrets first: their source variables must be withheld from passthrough.
    let mut specs = Vec::new();
    if secrets::has_github_token() {
        specs.push(secrets::github_spec());
    }
    for flag in &args.secret_flags {
        specs.push(secrets::parse_secret_flag(flag)?);
    }
    let built = secrets::build(&specs)?;
    let has_github = specs.iter().any(|s| s.name == "gh");

    let passthrough = env::passthrough_vars();
    let mut plain = env::compose(&args.env_flags, &passthrough, &built.source_vars)?;
    plain.extend(built.env.clone());
    plain.push((
        "TERM".into(),
        std::env::var("TERM").unwrap_or_else(|_| attach::DEFAULT_TERM.into()),
    ));
    plain.push(("BOX_NAME".into(), name.clone()));

    let home = config::box_home(&name);
    std::fs::create_dir_all(&home)
        .with_context(|| format!("cannot create box home {}", home.display()))?;

    let registries = config::resolve_config_path(args.config.as_deref())
        .map(|p| config::load_registries(&p))
        .unwrap_or_default();

    let runtime = BoxliteRuntime::new(BoxliteOptions {
        home_dir: home.clone(),
        image_registries: registries,
    })
    .context("failed to open the BoxLite runtime")?;

    if args.force {
        let _ = runtime.remove(&name, true).await;
    }

    let cmd = if args.cmd.is_empty() { vec!["claude".into()] } else { args.cmd };

    // The one line that turns a multi-hour "why did Claude just exit"
    // diagnosis into something visible immediately: warn, don't fail — the
    // user launching a non-`claude` command, or one that authenticates some
    // other way, is not this function's business.
    if cmd.first().map(String::as_str) == Some("claude") && !env::any_anthropic_credential_set() {
        let looked = match &env_file_path {
            Some(p) => format!("cbox looked for an env file at {}", p.display()),
            None => "cbox could not determine an env file location (no $HOME)".to_string(),
        };
        eprintln!(
            "cbox: warning: no Anthropic credential found (checked ANTHROPIC_API_KEY, \
             ANTHROPIC_AUTH_TOKEN, ANTHROPIC_BASE_URL, CLAUDE_CODE_OAUTH_TOKEN). \
             Claude will not be able to authenticate. {looked} \
             (override with --env-file or $CBOX_ENV_FILE)."
        );
    }

    let flags = boxopts::UpFlags {
        image: args.image,
        cwd_mount: args.cwd_mount,
        volumes: args.volumes,
        cmd,
        invocation_dir: cwd,
        disk_size_gb: args.disk_size_gb,
    };
    let options = boxopts::build(&flags, built.secrets, plain)?;

    println!("cbox: starting {name} ({})", resolved.source.describe());
    let litebox = runtime
        .create(options, Some(name.clone()))
        .await
        .context("failed to create the box")?;

    // Best-effort, like the git bootstrap below: `cbox list` losing the
    // origin column for this one box is far better than `cbox up` failing
    // over a metadata write.
    if let Err(e) = sidecar::write(&home, &flags.invocation_dir) {
        eprintln!(
            "cbox: warning: could not record this box's origin ({e}); \
             `cbox list` won't show a directory for it."
        );
    }

    litebox.start().await.context("failed to start the box")?;

    if has_github {
        run_git_bootstrap(&litebox).await;
    }

    let litebox = Arc::new(litebox);
    let home_for_socket = config::box_home(&name);
    let server = tokio::spawn({
        let litebox = Arc::clone(&litebox);
        let home = home_for_socket.clone();
        async move {
            if let Err(e) = crate::server::serve(litebox, home).await {
                eprintln!("cbox: control socket stopped: {e}");
            }
        }
    });

    let result = attach::attach(&litebox, &flags.cmd).await;

    // Clean shutdown unlinks the socket. A SIGKILL cannot, which is why the
    // client also handles a stale socket.
    server.abort();
    let _ = std::fs::remove_file(crate::server::socket_path(&home_for_socket));
    // `up`'s own exit status doesn't reflect the attached command's exit code
    // today (only `cbox exec` propagates that, via `commands/exec.rs`) — this
    // task didn't touch that, so keep dropping it here rather than changing
    // what `cbox up` reports to the shell.
    result.map(|_code| ())
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

//! `cbox up` — create a box and attach the terminal to it.

use std::os::unix::fs::PermissionsExt;
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
    /// Let the box outlive this process. See `boxopts::build`'s `detach`
    /// comment for the full reasoning; default is `false`, matching what
    /// the pre-cbox justfile actually passed to `boxlite run`.
    pub detach: bool,
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

    // Computed before `built.secrets` is moved into `boxopts::build` below.
    // Never the values themselves -- see `sidecar::hash_secret_value` for
    // why a hash is enough and adequate here.
    let secret_hashes: std::collections::BTreeMap<String, u64> = built
        .secrets
        .iter()
        .map(|s| (s.name.clone(), sidecar::hash_secret_value(&s.value)))
        .collect();

    let passthrough = env::passthrough_vars();
    let mut plain = env::compose(&args.env_flags, &passthrough, &built.source_vars)?;
    plain.extend(built.env.clone());
    plain.push((
        "TERM".into(),
        std::env::var("TERM").unwrap_or_else(|_| attach::DEFAULT_TERM.into()),
    ));
    plain.push(("BOX_NAME".into(), name.clone()));

    let home = config::box_home(&name);
    secure_box_home(&home)?;

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
            Some(loc) => format!("cbox looked for an env file at {}", loc.path.display()),
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
        detach: args.detach,
    };
    let options = boxopts::build(&flags, built.secrets, plain)?;

    println!("cbox: starting {name} ({})", resolved.source.describe());
    // get_or_create rather than create: with detach: false the common case
    // is exactly a name collision -- the box from a previous session is
    // sitting there Stopped, and that must be resumed, not rejected. `-f`
    // above already removed any existing box under this name, so on that
    // path this always creates fresh.
    let (litebox, created) = runtime
        .get_or_create(options, Some(name.clone()))
        .await
        .context("failed to create or reuse the box")?;

    if created {
        // Best-effort, like the git bootstrap below: `cbox list` losing the
        // origin column for this one box is far better than `cbox up`
        // failing over a metadata write.
        if let Err(e) = sidecar::write(&home, &flags.invocation_dir, &secret_hashes) {
            eprintln!(
                "cbox: warning: could not record this box's origin ({e}); \
                 `cbox list` won't show a directory for it."
            );
        }
    } else {
        // `get_or_create`'s own doc: "the provided options are ignored (no
        // config drift validation)". So the reused box keeps whatever
        // credentials, mounts, and disk size it had when first created,
        // silently -- unless this says so, that's invisible until something
        // fails (e.g. a rotated token 401ing), which is precisely the
        // failure class this whole project exists to prevent.
        println!(
            "cbox: reusing existing box {name}; its configuration (credentials, mounts, \
             disk size) dates from when it was first created. Run with -f/--force to \
             recreate it with today's settings instead."
        );
        if let Some(existing) = sidecar::read(&home) {
            let changed = sidecar::changed_secrets(&existing.secret_hashes, &secret_hashes);
            if !changed.is_empty() {
                // A warning, not a refusal: the whole point of reuse is to
                // resume a box that's otherwise fine, and most reuses won't
                // have rotated anything. Refusing here would force -f (a
                // full recreate) onto every rotation, including ones that
                // don't matter for this session (e.g. a secret this
                // invocation doesn't even use). Naming the stale secret and
                // the fix is what turns this from an invisible 401 later
                // into an actionable line now.
                eprintln!(
                    "cbox: warning: {} in your environment no longer match(es) what this box \
                     was created with -- it will keep substituting the OLD value(s) until you \
                     run with -f/--force to recreate it.",
                    changed.join(", ")
                );
            }
        }
    }

    // Idempotent on an already-Running box (the SDK's own doc on `start()`),
    // so this is correct whether the box above was just created, resumed
    // from Stopped, or was already Running.
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

/// Create the box's home directory if needed, and ensure it is owner-only.
///
/// The control socket `server::serve` binds inside this directory grants any
/// local process arbitrary TTY exec into a box that substitutes a real
/// credential (e.g. a GitHub token) into outbound HTTPS -- so this directory
/// must never be traversable by anyone but the owner. `create_dir_all` alone
/// yields 0755 and no-ops on an already-existing directory, so the
/// permission is set unconditionally afterward, which also tightens a home
/// left over-permissive by an earlier run. macOS does not reliably enforce a
/// Unix-domain socket's own mode on `connect()`, so this directory
/// permission -- not the socket's own mode, set separately in
/// `server::serve` -- is the one that actually gates access.
fn secure_box_home(home: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(home)
        .with_context(|| format!("cannot create box home {}", home.display()))?;
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict permissions on box home {}", home.display()))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::secure_box_home;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_freshly_created_box_home_is_owner_only() {
        let dir = std::env::temp_dir()
            .join(format!("cbox-up-test-home-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        secure_box_home(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "fresh box home must be owner-only, got {mode:o}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pre_existing_over_permissive_box_home_is_tightened() {
        // Guards against a home left over-permissive by an earlier run --
        // this is the case `create_dir_all` alone silently leaves open,
        // since it no-ops on an already-existing directory.
        let dir = std::env::temp_dir()
            .join(format!("cbox-up-test-home-loose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        secure_box_home(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "a loosely-permissioned existing home must be tightened, got {mode:o}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

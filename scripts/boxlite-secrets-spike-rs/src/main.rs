//! Validate BoxLite's host-side secret injection against this repo's custom image.
//!
//! A Rust port of `scripts/boxlite-secrets-spike.py`, running the same checks
//! against the same box. The Python version answered the product question (see
//! its docstring for the findings); this one exists to answer a different one:
//! whether the app that replaces `just up` should be written in Rust or Python.
//!
//! `boxlite run` cannot express secrets at all — a CLI runtime is configured from
//! --config, whose struct carries only `image_registries` — so shipping secret
//! substitution means writing a program against the SDK either way. The only
//! question is which SDK.
//!
//! == What the port actually changed ==
//!
//! Every workaround in the Python spike was a *binding* artifact, not a BoxLite
//! one, and none of them survived the port:
//!
//!   Python                                     Rust
//!   ------------------------------------------ ---------------------------------
//!   pump thread crashed: greenlet cannot        `tokio::select!`, no fiber bridge
//!     switch across OS threads
//!   `wait()` returns a PyO3 Future, not a       `wait().await` is a normal async
//!     coroutine, so `create_task` rejected it     fn returning `ExecResult`
//!   interactive TTY needed `box._box` and       `LiteBox::exec` and `Execution`
//!     `box._sync_helper._sync()` — private        are the public API
//!   REST mode needed `object.__new__` plus      `BoxliteRuntime::rest(opts)`
//!     hand-populating five private fields
//!   stdout silently stalled after the guest     `ExecStdout` is a `Stream` over
//!     exited — no exception, no EOF — so          an mpsc receiver, so it yields
//!     exiting the shell needed a 0.3s poll        `None` on exit. Real EOF, and
//!     plus an empty-write liveness probe          the probe deletes itself
//!
//! That last row is the one to weigh. In Python, detecting that the user typed
//! `exit` took a select loop, a timeout, and a probe whose payload had to be
//! empty because the box's PTY echoes anything else. Here it is `None` from a
//! stream, which is what the underlying channel meant all along.
//!
//! The offline logic is covered by `cargo test` rather than a `--self-check`
//! flag, which is the other half of the argument: the parsing helpers below are
//! unit-tested, and everything else is checked by the compiler before it runs.
//!
//! == Running ==
//!
//! ```text
//! GH_TOKEN=ghp_... ANTHROPIC_API_KEY=sk-ant-... cargo run --release
//! cargo run --release -- --interactive
//! cargo run --release -- --interactive -- bash
//! cargo run --release -- --git-remote https://github.com/you/private.git
//! ```
//!
//! Checks whose credential is unset are skipped, not failed. The box is removed
//! on exit unless `--keep` is passed. Needs the custom image already built and
//! pushed (`just build`).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use boxlite::{
    BoxCommand, BoxOptions, BoxliteOptions, BoxliteRuntime, ImageRegistry, LiteBox, RootfsSpec,
    Secret,
};
use clap::Parser;
use futures::{Stream, StreamExt};
use tokio::io::AsyncReadExt as _;

/// The guest-side token BoxLite swaps for the real value at egress.
///
/// The Rust `Secret` takes this explicitly (the Python binding derived it), so
/// the format is ours to pick here — but it has to keep matching the binding's,
/// or the same box would present different placeholders depending on which SDK
/// launched it.
fn placeholder(name: &str) -> String {
    format!("<BOXLITE_SECRET:{name}>")
}

/// The Basic-auth username GitHub expects when the password is a PAT.
const GH_BASIC_USER: &str = "x-access-token";

#[derive(Parser)]
#[command(about = "Validate BoxLite secret injection against the custom image")]
struct Cli {
    /// Image to boot (default: the custom layer).
    #[arg(long, default_value = "claude-boxlite-custom")]
    image: String,

    /// BOXLITE_HOME for this spike.
    #[arg(long)]
    home: Option<PathBuf>,

    /// BoxLite registry config (default: this repo's registries.local.json).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Optional private HTTPS repo URL to exercise `git ls-remote`.
    #[arg(long)]
    git_remote: Option<String>,

    /// Leave the box running for manual poking.
    #[arg(long)]
    keep: bool,

    /// After the checks, attach this terminal to the box and drive it by hand.
    #[arg(long)]
    interactive: bool,

    /// With --interactive, what to run in the box (default: claude); prefix with --.
    #[arg(last = true)]
    cmd: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    Pass,
    Fail,
    Skip,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Skip => "SKIP",
        }
    }
}

#[derive(Default)]
struct Report {
    rows: Vec<(String, Verdict)>,
}

impl Report {
    fn add(&mut self, label: &str, verdict: Verdict, detail: &str) {
        println!("  [{}] {label}: {detail}", verdict.tag());
        let _ = std::io::stdout().flush();
        self.rows.push((label.to_string(), verdict));
    }

    /// Print the tally and return the process exit code.
    fn summarise(&self) -> i32 {
        let passed = self.rows.iter().filter(|(_, v)| *v == Verdict::Pass).count();
        let failed: Vec<&str> = self
            .rows
            .iter()
            .filter(|(_, v)| *v == Verdict::Fail)
            .map(|(l, _)| l.as_str())
            .collect();
        println!("\n{passed}/{} passed", self.rows.len());
        if failed.is_empty() {
            return 0;
        }
        eprintln!("failing: {}", failed.join(", "));
        1
    }
}

/// Map this repo's BoxLite --config JSON onto `ImageRegistry` values.
///
/// The SDK takes registries as constructed values rather than a file path, so
/// the same registries.local.json the justfile passes to `boxlite run` has to be
/// translated here. Auth is flattened: the file nests it under "auth",
/// `ImageRegistry` takes a builder call.
///
/// A missing or malformed file yields no registries rather than an error — the
/// Python spike behaves the same way, and the box still boots against docker.io.
fn load_registries(path: &Path) -> Vec<ImageRegistry> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return vec![];
    };
    let Some(entries) = doc.get("image_registries").and_then(|v| v.as_array()) else {
        return vec![];
    };

    entries
        .iter()
        .filter_map(|entry| {
            let host = entry.get("host")?.as_str()?;
            let http = entry.get("transport").and_then(|t| t.as_str()) == Some("http");
            let mut registry = if http {
                ImageRegistry::http(host)
            } else {
                ImageRegistry::https(host)
            };
            registry = registry
                .with_skip_verify(bool_at(entry, "skip_verify"))
                .with_search(bool_at(entry, "search"));

            let auth = entry.get("auth");
            let user = auth.and_then(|a| a.get("username")).and_then(|v| v.as_str());
            let pass = auth.and_then(|a| a.get("password")).and_then(|v| v.as_str());
            if let (Some(user), Some(pass)) = (user, pass) {
                registry = registry.with_basic_auth(user, pass);
            }
            Some(registry)
        })
        .collect()
}

fn bool_at(entry: &serde_json::Value, key: &str) -> bool {
    entry.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Collect an stdout/stderr stream into one string.
///
/// Both streams are `Stream<Item = String>` over an mpsc receiver, so this ends
/// on real EOF when the guest process exits — no timeout, no sentinel.
async fn drain<S: Stream<Item = String> + Unpin>(stream: Option<S>) -> String {
    let Some(mut stream) = stream else {
        return String::new();
    };
    let mut lines = Vec::new();
    while let Some(line) = stream.next().await {
        lines.push(line.trim_end_matches('\n').to_string());
    }
    lines.join("\n")
}

/// Run a shell snippet in the box; return (exit code, stdout, stderr).
///
/// Both streams are drained before `wait()`: the iterators are fed by the live
/// execution, so waiting first can leave output unread.
async fn run(litebox: &LiteBox, script: &str) -> Result<(Option<i32>, String, String)> {
    let mut exec = litebox
        .exec(BoxCommand::new("sh").args(["-lc", script]))
        .await
        .context("exec failed")?;
    let out = drain(exec.stdout()).await;
    let mut err = drain(exec.stderr()).await;

    let code = match exec.wait().await {
        Ok(result) => Some(result.code()),
        Err(e) => {
            err = format!("{err}\nwait() failed: {e}").trim().to_string();
            None
        }
    };
    Ok((code, out.trim().to_string(), err.trim().to_string()))
}

/// curl exit codes worth naming.
///
/// Anything in the TLS family means the guest does not trust BoxLite's MITM CA,
/// which is a different verdict from a rejected credential: it points at a
/// fixable base-image change rather than a dead end.
fn curl_tls_error(rc: &str) -> Option<&'static str> {
    match rc {
        "35" => Some("TLS handshake"),
        "51" => Some("cert/host mismatch"),
        "60" => Some("CA not trusted"),
        "77" => Some("CA bundle unreadable"),
        _ => None,
    }
}

/// Split curl's `%{http_code}` from the echoed `rc=N`.
///
/// The two can land in separate stream lines, so whitespace is collapsed before
/// splitting. Returns ("", "?") shaped values rather than failing, so a garbled
/// response still reports as a FAIL with its raw text instead of panicking.
fn split_status_rc(out: &str) -> (String, String) {
    let joined = out.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.split_once("rc=") {
        Some((status, rest)) => (
            status.trim().to_string(),
            rest.split_whitespace().next().unwrap_or("?").to_string(),
        ),
        None => (joined.trim().to_string(), "?".to_string()),
    }
}

/// Issue one authenticated request from inside the box and classify it.
///
/// Reports curl's exit code and the HTTP status separately so a TLS failure
/// (needs a CA) is never mistaken for a 401 (substitution did not happen).
async fn probe_http(
    litebox: &LiteBox,
    report: &mut Report,
    label: &str,
    url: &str,
    header: &str,
    note: &str,
) -> Result<()> {
    let script = format!(
        "curl -sS -o /dev/null -w '%{{http_code}}' -H '{header}' '{url}' 2>/tmp/curl.err; \
         echo \" rc=$?\"; cat /tmp/curl.err"
    );
    let (_, out, err) = run(litebox, &script).await?;
    let (status, rc) = split_status_rc(&out);

    if let Some(kind) = curl_tls_error(&rc) {
        report.add(
            label,
            Verdict::Fail,
            &format!("TLS error {rc} ({kind}) — guest does not trust the MITM CA"),
        );
    } else if status.starts_with('2') {
        report.add(
            label,
            Verdict::Pass,
            &format!("HTTP {status} — substitution happened{note}"),
        );
    } else if status == "401" || status == "403" {
        report.add(
            label,
            Verdict::Fail,
            &format!("HTTP {status} — placeholder reached the server unsubstituted"),
        );
    } else {
        let status = if status.is_empty() { "?" } else { &status };
        let tail: String = err.chars().take(160).collect();
        report.add(label, Verdict::Fail, &format!("HTTP {status} rc={rc} {tail}"));
    }
    Ok(())
}

/// Attach the local terminal to a TTY exec inside the box, in this process.
///
/// In-process is not a preference, it is the only thing that can work: the
/// substituting proxy is host-side and belongs to the runtime that created the
/// box with `secrets`. Handing the terminal to `boxlite exec` would open a
/// second runtime with no secrets, sending the literal placeholder.
async fn attach(litebox: &LiteBox, cmd: &[String]) -> Result<()> {
    println!(
        "\nboxlite-secrets-spike: attaching. What this is here to settle:\n  \
         - `gh api user` works                  -> substitution reaches an exec'd process\n  \
         - claude reaches the API               -> Node trusts the MITM CA\n  \
         - claude fails on certificates         -> base/Dockerfile must install the CA\n  \
         - `git push` to a private HTTPS remote -> the extraHeader path substitutes too\n\
         Exit the shell/agent to return here; the box is then removed as usual.\n"
    );

    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    let mut command = BoxCommand::new(&cmd[0]).tty(true).env("TERM", term);
    if cmd.len() > 1 {
        command = command.args(&cmd[1..]);
    }

    let mut exec = litebox.exec(command).await.context("interactive exec failed")?;
    // crossterm reports (cols, rows); resize_tty takes (rows, cols).
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    exec.resize_tty(rows as u32, cols as u32).await?;

    // Take both streams before sharing `exec`: these need &mut, everything
    // afterwards (resize_tty) needs only &self.
    let out_stream = exec.stdout().context("no stdout on the interactive exec")?;
    let stdin_writer = exec.stdin().context("no stdin on the interactive exec")?;
    let exec = Arc::new(exec);

    crossterm::terminal::enable_raw_mode()?;
    let result = pump(exec, out_stream, stdin_writer).await;
    crossterm::terminal::disable_raw_mode()?;
    result
}

/// Shuttle bytes between the local terminal and the box until the guest exits.
///
/// The loop ends on `None` from the stdout stream. That is a real EOF — the
/// stream is an mpsc receiver whose sender is dropped when the guest process
/// dies — which is exactly what the Python spike could not observe, and why it
/// needed a timed liveness probe instead.
async fn pump(
    exec: Arc<boxlite::Execution>,
    mut out_stream: impl Stream<Item = String> + Unpin,
    mut stdin_writer: boxlite::ExecStdin,
) -> Result<()> {
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 4096];

    loop {
        tokio::select! {
            chunk = out_stream.next() => match chunk {
                Some(text) => {
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                }
                None => break, // guest exited
            },
            read = stdin.read(&mut buf) => {
                let n = read?;
                if n == 0 {
                    break; // local stdin closed
                }
                if stdin_writer.write(&buf[..n]).await.is_err() {
                    break; // guest is gone
                }
            }
            _ = sigwinch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let _ = exec.resize_tty(rows as u32, cols as u32).await;
                }
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let gh_token = std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
        .filter(|s| !s.is_empty());
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());

    if gh_token.is_none() && anthropic_key.is_none() {
        anyhow::bail!(
            "set GH_TOKEN and/or ANTHROPIC_API_KEY — nothing to test otherwise"
        );
    }

    // Each secret is scoped to the hosts it may be substituted for. A host absent
    // from this list must never receive the real value; that scoping is the whole
    // security property, so keep these lists as narrow as the check requires.
    let mut secrets: Vec<Secret> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();

    if let Some(token) = &gh_token {
        secrets.push(Secret {
            name: "gh".into(),
            hosts: vec!["github.com".into(), "api.github.com".into()],
            placeholder: placeholder("gh"),
            value: token.clone(),
        });
        env.push(("GH_TOKEN".into(), placeholder("gh")));

        // A second secret for git, because git cannot use the first one. git only
        // authenticates to github.com with Basic, i.e. base64(user:token) — and
        // base64 hides the placeholder from the proxy's literal string match, so
        // the raw-token secret above can never reach a git request. Measured on
        // the host: Bearer gets 401 from git-upload-pack, Basic gets 200.
        //
        // The trick is to move the base64 to the host side: store the already
        // encoded credential as the value, and put the placeholder where the
        // encoded blob belongs. git's http.extraHeader is passed through verbatim
        // (unlike helper-supplied credentials, which git encodes itself), so the
        // placeholder reaches the wire intact for the proxy to match.
        secrets.push(Secret {
            name: "gh_basic".into(),
            hosts: vec!["github.com".into()],
            placeholder: placeholder("gh_basic"),
            value: BASE64.encode(format!("{GH_BASIC_USER}:{token}")),
        });
    }

    if let Some(key) = &anthropic_key {
        secrets.push(Secret {
            name: "anthropic".into(),
            hosts: vec!["api.anthropic.com".into()],
            placeholder: placeholder("anthropic"),
            value: key.clone(),
        });
        env.push(("ANTHROPIC_API_KEY".into(), placeholder("anthropic")));
    }

    // Always route Claude through the agentgateway — the gateway injects the real
    // key via backendAuth (agentgateway/config.yaml /api route), so the box never
    // needs the real value. BoxLite substitution for api.anthropic.com is not the
    // intended production path for Anthropic auth.
    env.push((
        "ANTHROPIC_BASE_URL".into(),
        "http://host.boxlite.internal:15002/api".into(),
    ));
    let env_names: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();

    println!(
        "boxlite-secrets-spike: booting {} with {} secret(s)\n",
        cli.image,
        secrets.len()
    );

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("cannot locate the repo root from CARGO_MANIFEST_DIR")?
        .to_path_buf();
    let config = cli
        .config
        .clone()
        .unwrap_or_else(|| repo_root.join("registries.local.json"));

    let mut runtime_opts = BoxliteOptions {
        image_registries: load_registries(&config),
        ..Default::default()
    };
    if let Some(home) = &cli.home {
        runtime_opts.home_dir = home.clone();
    }
    let runtime = BoxliteRuntime::new(runtime_opts).context("failed to open the BoxLite runtime")?;

    let litebox = runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image(cli.image.clone()),
                env,
                secrets,
                auto_remove: !cli.keep,
                // The image sets no ENTRYPOINT/CMD, so it inherits node:26's
                // `node`, which exits immediately without a TTY and takes the box
                // down with it. Every check here runs via exec, so the main
                // process just has to stay alive.
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                working_dir: Some("/workspace".into()),
                ..Default::default()
            },
            Some("boxlite-secrets-spike-rs".into()),
        )
        .await
        .context("failed to create the box")?;
    litebox.start().await.context("failed to start the box")?;

    let code = checks(&litebox, &cli, &gh_token, &anthropic_key, &env_names).await?;

    if cli.interactive {
        // Before teardown on purpose: the substituting proxy belongs to this
        // runtime, so the session has to live inside its lifetime too.
        let cmd = if cli.cmd.is_empty() {
            vec!["claude".to_string()]
        } else {
            cli.cmd.clone()
        };
        if let Err(e) = attach(&litebox, &cmd).await {
            eprintln!("\nboxlite-secrets-spike: interactive attach failed: {e:?}");
        }
    }

    if !cli.keep {
        let _ = litebox.stop().await;
    } else {
        println!("\nbox kept: {}", litebox.id());
    }

    std::process::exit(code);
}

/// Run every credential check and return the process exit code.
async fn checks(
    litebox: &LiteBox,
    cli: &Cli,
    gh_token: &Option<String>,
    anthropic_key: &Option<String>,
    env_names: &[String],
) -> Result<i32> {
    let mut report = Report::default();

    // 1. Custody: whatever the guest can read must be the placeholder. This is
    //    the check that would catch the feature silently degrading to plain
    //    env-var passthrough, which would look identical from every probe below.
    for (var, name) in [("GH_TOKEN", "gh"), ("ANTHROPIC_API_KEY", "anthropic")] {
        if !env_names.iter().any(|n| n == var) {
            continue;
        }
        let (_, out, _) = run(litebox, &format!("printenv {var} || true")).await?;
        let real = if name == "gh" { gh_token } else { anthropic_key };
        let label = format!("custody/{var}");

        if out == placeholder(name) {
            report.add(&label, Verdict::Pass, "guest sees only the placeholder");
        } else if real.as_ref().is_some_and(|r| out.contains(r.as_str())) {
            report.add(
                &label,
                Verdict::Fail,
                "REAL VALUE PRESENT IN GUEST — no custody gain",
            );
        } else {
            let seen: String = out.chars().take(40).collect();
            report.add(&label, Verdict::Fail, &format!("unexpected value {seen:?}"));
        }
    }

    // 2. Substitution for GitHub (Authorization header); gateway reachability for
    //    Anthropic (the gateway injects the real key, the box sends a dummy).
    if gh_token.is_some() {
        probe_http(
            litebox,
            &mut report,
            "github/authorization-header",
            "https://api.github.com/user",
            &format!("Authorization: Bearer {}", placeholder("gh")),
            "",
        )
        .await?;
    }

    if anthropic_key.is_some() {
        // The gateway's /api route has no incoming auth check — it accepts any
        // x-api-key value and rewrites it via backendAuth before forwarding. This
        // confirms the gateway is up and holds a valid key; it says nothing about
        // BoxLite substitution, which is not the Anthropic path.
        let (_, out, _) = run(
            litebox,
            "curl -sS -o /dev/null -w '%{http_code}' \
             http://host.boxlite.internal:15002/api/v1/models \
             -H 'x-api-key: dummy' 2>/dev/null || true",
        )
        .await?;
        let status = out.split_whitespace().next().unwrap_or("").to_string();
        let ok = status.starts_with('2');
        report.add(
            "anthropic/gateway-reachable",
            if ok { Verdict::Pass } else { Verdict::Fail },
            &if ok {
                format!("HTTP {status} — gateway up, key injected by gateway")
            } else {
                let status = if status.is_empty() { "?" } else { &status };
                format!("HTTP {status} — gateway down or missing ANTHROPIC_API_KEY")
            },
        );
    }

    // 3. The real clients, not just curl. gh and git each build their own TLS
    //    stack and their own header, so a curl PASS does not imply these pass.
    if gh_token.is_some() {
        let (_, out, err) = run(litebox, "gh api user --jq .login 2>&1 || true").await?;
        let ok = !out.is_empty() && !out.to_lowercase().contains("error");
        let detail = if out.is_empty() { &err } else { &out };
        let detail: String = detail.chars().take(120).collect();
        report.add(
            "github/gh-cli",
            if ok { Verdict::Pass } else { Verdict::Fail },
            if detail.is_empty() { "no output" } else { &detail },
        );

        // Point git at the pre-encoded secret and blank the credential helper, so
        // the only credential in play is the substitutable header. Without the
        // blanking, git falls back to the helper on a 401 and sends base64'd
        // garbage, which muddies the verdict.
        run(
            litebox,
            &format!(
                "git config --global http.https://github.com/.extraHeader \
                 'Authorization: Basic {}' && \
                 git config --global credential.helper '' && \
                 git config --global user.email box@boxlite.local && \
                 git config --global user.name boxlite",
                placeholder("gh_basic")
            ),
        )
        .await?;

        match &cli.git_remote {
            Some(remote) => {
                let (_, out, err) = run(
                    litebox,
                    &format!("git ls-remote '{remote}' HEAD 2>&1 | head -1 || true"),
                )
                .await?;
                let lowered = out.to_lowercase();
                let ok = !out.is_empty()
                    && !lowered.contains("fatal")
                    && !lowered.contains("denied");
                let detail: String = if out.is_empty() { &err } else { &out }
                    .chars()
                    .take(120)
                    .collect();
                report.add(
                    "github/git-ls-remote",
                    if ok { Verdict::Pass } else { Verdict::Fail },
                    &detail,
                );
            }
            None => report.add(
                "github/git-ls-remote",
                Verdict::Skip,
                "pass --git-remote <private-https-url> to exercise",
            ),
        }
    }

    Ok(report.summarise())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_matches_the_python_binding_format() {
        assert_eq!(placeholder("gh"), "<BOXLITE_SECRET:gh>");
        assert_eq!(placeholder("gh_basic"), "<BOXLITE_SECRET:gh_basic>");
    }

    /// The whole git fix rests on this blob decoding to what GitHub expects, and
    /// it is the one value here that is never eyeballed in the output.
    #[test]
    fn git_basic_blob_decodes_to_the_credential_github_wants() {
        let encoded = BASE64.encode(format!("{GH_BASIC_USER}:ghp_example"));
        let decoded = String::from_utf8(BASE64.decode(&encoded).unwrap()).unwrap();
        assert_eq!(decoded, "x-access-token:ghp_example");
        // The placeholder must NOT survive encoding — that is precisely why the
        // helper-supplied path cannot work and this one can.
        assert!(!encoded.contains("x-access-token"));
    }

    #[test]
    fn status_and_rc_split_across_every_stream_shape_seen_live() {
        assert_eq!(split_status_rc("200 rc=0"), ("200".into(), "0".into()));
        assert_eq!(split_status_rc("200\n rc=0"), ("200".into(), "0".into()));
        assert_eq!(split_status_rc("\nrc=60"), ("".into(), "60".into()));
        assert_eq!(split_status_rc(""), ("".into(), "?".into()));
    }

    #[test]
    fn tls_exit_codes_are_named_and_others_are_not() {
        assert_eq!(curl_tls_error("60"), Some("CA not trusted"));
        assert_eq!(curl_tls_error("0"), None);
        assert_eq!(curl_tls_error("?"), None);
    }

    #[test]
    fn registries_parse_transport_and_flattened_auth() {
        let dir = std::env::temp_dir().join("boxlite-spike-rs-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registries.json");
        std::fs::write(
            &path,
            r#"{"image_registries":[
                 {"host":"localhost:5551","transport":"http","skip_verify":true,"search":true},
                 {"host":"docker.io","transport":"https"},
                 {"host":"ecr.example.com","auth":{"username":"AWS","password":"pw"}}
               ]}"#,
        )
        .unwrap();
        assert_eq!(load_registries(&path).len(), 3);

        // Malformed and missing files degrade to "no registries", never a panic.
        std::fs::write(&path, "not json").unwrap();
        assert!(load_registries(&path).is_empty());
        assert!(load_registries(Path::new("/nonexistent/registries.json")).is_empty());
    }

    #[test]
    fn drain_joins_lines_and_tolerates_an_absent_stream() {
        let none: Option<futures::stream::Empty<String>> = None;
        assert_eq!(futures::executor::block_on(drain(none)), "");

        let some = futures::stream::iter(vec!["a".to_string(), "b\n".to_string()]);
        assert_eq!(futures::executor::block_on(drain(Some(some))), "a\nb");
    }
}

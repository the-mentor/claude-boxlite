# Driving BoxLite through its SDK instead of its CLI

This document evaluates replacing the `boxlite` CLI invocations in `justfile` with BoxLite's
SDK, and records what that would and would not buy. It is a **proposal, not a description of
what this repo does today** — everything here is unimplemented. For the box side as it
currently stands see `docs/design/general.md`; for the host-side gateway see
`docs/design/agentgateway.md`.

Claims are tagged as **verified** (checked against the installed `boxlite 0.9.7` CLI, the
`boxlite==0.9.7` Python package, or a live run of `scripts/boxlite-secrets-spike.py`) or
**open**. Treat the split as load-bearing.

`scripts/boxlite-secrets-spike.py` has now been run against a live box with a real `GH_TOKEN`,
and the question that could have sunk the whole idea did not: **substitution works, and the
guest needed no CA change.** See "What the spike established" below for exactly how far that
result reaches — it covers `curl` and `gh`, and *not* Claude Code's own Node TLS stack.

## The forcing function: `secrets` is SDK-only

BoxLite can substitute a real credential into outbound HTTPS at the host boundary, so the
value never enters the guest. The guest holds a placeholder (`<BOXLITE_SECRET:name>`); a
host-side MITM proxy swaps in the real value on requests to named hosts only.

**Verified.** `boxlite.Secret(name, value, hosts=..., placeholder=None)` exists in the
0.9.7 Python package, and its docstring states the substitution happens host-side with the
secret never entering the guest VM. `secrets` is a field on `BoxOptions` — the per-box create
options — alongside `env`, `volumes`, `network`, and `ports`.

**Verified.** The CLI cannot reach it. `boxlite run --help` at 0.9.7 exposes `--allow-net`,
`--network`, `-v`, `-p`, `-e`, and `--config`, but no secret flag. And because `secrets` sits
on `BoxOptions` rather than `BoxliteOptions` (the struct behind `--config`, whose field is
`image_registries`), the config file this repo already passes cannot carry it either.

That asymmetry is the entire argument for the SDK. Every other benefit below is available
some other way or is a convenience; this one is not reachable from the CLI at all.

## What it would change about the credential model

Two things, of very different value.

**It retires the `GH_TOKEN` passthrough.** `docs/design/general.md` currently states that
`GH_TOKEN`/`GITHUB_TOKEN` must reach the box because "the box runs `gh` and `git push` itself
— no proxy can do that on its behalf." A host-side HTTPS MITM *can* do it on its behalf:
`git push` over HTTPS and every `gh` call are ordinary HTTPS requests carrying the token in a
header. Scoping a `Secret` to `github.com` and `api.github.com` leaves the box holding only a
placeholder. This is the one credential this repo knowingly gives up today, and it is the
main reason to do this work.

Note the shape of the dependency: this only holds because `custom/Dockerfile` routes git
HTTPS through `gh auth git-credential`. A box that pushed over SSH would be outside the
MITM's reach entirely, and the property would silently not apply.

**It demotes the gateway from credential boundary to observability.** The `/api` keyed route
exists so the box can send a dummy `ANTHROPIC_AUTH_TOKEN` while the real key stays host-side.
A `Secret` scoped to `api.anthropic.com` achieves the same custody with no gateway in the
path — and with it, the `hostOverride`/`backendTLS` pairing, the
`AGENTGATEWAY_ANTHROPIC_UPSTREAM_HOST`-versus-`ANTHROPIC_BASE_URL` naming hazard, and the
`llm_vars` three-way conditional all stop being load-bearing for custody.

This is worth less than it looks. agentgateway is still wanted for MCP multiplexing and for
the Logs/Analytics/Costs telemetry, and both survive unchanged. So this removes a *reason* the
gateway must be correct, not the gateway. Do not treat it as a deletion.

## What `--allow-net` does and does not fix

**Verified.** `--allow-net` exists on the CLI today, needs no SDK, and takes repeatable
hosts/IPs with `*.example.com` wildcards and CIDRs; everything else is DNS-sinkholed.

What it gives is **egress containment**: an agent that has been prompt-injected, or a poisoned
dependency, cannot reach an arbitrary destination to send this repo's source anywhere. Given
the box runs an autonomous agent over the workspace with a GitHub token, that is the most
valuable single control available here.

What it does **not** give is any improvement on the problem `agentgateway.md`'s ports section
describes. `host.boxlite.internal` resolves to `192.168.127.254`, and BoxLite's own
documentation states a non-empty allow-list must include that address (or a covering CIDR)
for the alias to resolve at all. The box needs the alias for its baked MCP config. The
allow-list has no port syntax — it matches hosts, IPs, wildcards, and CIDRs. So allowing the
alias for MCP restores reachability to *every* published host port at the same time.

The split is host-versus-internet, not port-versus-port. **The ports policy in
`agentgateway.md` stays exactly as load-bearing as it is written**, and `--allow-net` must not
be described as having relaxed it.

Its real cost is `WebFetch`/`WebSearch`, which run inside the box against arbitrary domains
and stop working under any allow-list. That makes this a per-box choice rather than a global
default — an opt-in flag on `just up`, not a change to the baseline.

## The runtime-lock win

`docs/design/general.md` documents per-box `BOXLITE_HOME` as a workaround for BoxLite taking
an exclusive lock on the whole home directory for as long as a CLI process is attached, and
records that `just exec <name>` therefore cannot run while `just up <name>` is attached. It
names a shared `boxlite serve` daemon as the fix and parks it on boxlite-ai/boxlite#942 (the
REST API does not forward `-v`/`-c` bind mounts).

An in-process SDK runtime resolves this without waiting for that issue: one long-lived
`Boxlite` instance holds one lock and manages many boxes, and `volumes` is a native
`BoxOptions` field rather than something that has to survive a REST round-trip. Whether the
per-box home split can then be collapsed entirely is **open** — it depends on whether the lock
is per-home or per-runtime-process, which the spike does not currently test.

## Facts established against 0.9.7

Checked by introspecting the installed package and CLI, not from documentation prose:

- `init_default(Options(home_dir=..., image_registries=[...]))` accepts this repo's registry
  configuration. Registries are constructor arguments, so `registries.local.json` has to be
  parsed and mapped by the caller — `ImageRegistry` takes `username`/`password` directly rather
  than the file's nested `auth` block.
- **Use `SyncBoxlite`, not `Boxlite`.** `Boxlite` is the native async runtime; its
  `create()`/`exec()` return coroutines. The sync wrapper needs the `[sync]` extra (greenlet) or
  the name is silently absent from the module. `init_default()` returns `None` — it only
  registers options for the global runtime; the runtime itself comes from `default()`, and its
  dispatcher fiber must be running before `create()`/`exec()`, which is what using it as a
  context manager does. This is the one place the SDK is meaningfully less obvious than the CLI.
- `BoxOptions.env` takes a list of `(key, value)` tuples, not a dict.
- **A box needs a foreground process.** This repo's image sets no `ENTRYPOINT`/`CMD`, so it
  inherits `node:26`'s `node`, which exits immediately without a TTY and takes the box down with
  it. Under `boxlite run` the CLI supplies the command; through the SDK the caller must
  (`cmd=["sleep", "infinity"]` for an exec-only box). A port of `just up` has to pass `claude`
  here explicitly.
- `Box.exec(command, args=None, env=None, tty=False, user=None, timeout_secs=None, cwd=None)`
  returns an `Execution` exposing `stdout`, `stderr`, `stdin`, `wait`, `kill`, `signal`, and
  **`resize_tty`**. That last one matters more than it looks: it means interactive TTY
  attachment and terminal resize are first-class in the SDK, which was the main risk to
  preserving the interactive Claude TUI that `just up` exists to provide.
- On the sync wrapper, `stdout`/`stderr` are **methods** returning iterators of already-decoded,
  newline-stripped lines, and return `None` when unavailable. Both must be drained before
  `wait()`, since the iterators are fed by the live execution. Line boundaries follow read
  chunks, not newlines, so a single `printf` without a trailing newline can arrive split across
  two "lines" — do not parse a stream's shape as if it were a shell here-doc.
- `ExecResult` exposes only `error_message` — there is no documented exit-code accessor, so
  the spike reads stdout and treats a missing exit code as normal rather than as failure.
- `SecurityOptions` offers `development()`/`standard()`/`maximum()` presets covering jailer,
  seccomp, and rlimits. The CLI collapses all of this to `--security enable|disable`, so the
  SDK is strictly more expressive here. Not a motivation on its own; worth knowing.

## What the spike established

Live run, `claude-boxlite-custom`, one `Secret` scoped to `github.com`/`api.github.com`, 4/4
checks passing:

- **Custody holds. Verified.** `printenv GH_TOKEN` inside the guest returns
  `<BOXLITE_SECRET:gh>` and nothing else. This is the check that would have caught the feature
  degrading to plain env-var passthrough, which looks identical to success from every network
  probe.
- **`Authorization` substitution works. Verified.** `curl -H "Authorization: Bearer
  <BOXLITE_SECRET:gh>" https://api.github.com/user` returns HTTP 200 from inside the box.
- **It works for a real client, not just curl. Verified.** `gh api user --jq .login` resolves
  the account. `gh` builds its own TLS stack and its own header, so the curl pass did not imply
  this one.
- **No guest CA change is needed — for OpenSSL and Go clients. Verified, with a limit.** Zero
  TLS errors from either `curl` or `gh`, so BoxLite's intercepting CA is already trusted by the
  system store in this image and `base/Dockerfile` needs no change for them. **This does not
  extend to Claude Code.** Node ships its own bundled CA store and ignores the system store, so
  `NODE_EXTRA_CA_CERTS` is still an open requirement, and it is now the most likely remaining
  blocker. The spike does not test it because that path needs the Anthropic secret.

So the question that could have sunk the idea is answered favourably for the credential that
motivated the work — `GH_TOKEN`. The Anthropic half is untested.

## Open questions

- **Does substitution cover `x-api-key`?** Claude Code sends `x-api-key`, not `Authorization`.
  The `Authorization` path is now verified; this one is not, and the docstring only says "HTTP
  headers" generically. Half-coverage would mean the feature only retires half the credentials.
  Run the spike with `ANTHROPIC_API_KEY` set to settle it — the check is already written.
- **Does Node trust the intercepting CA?** Per above: untested, and it decides whether
  `base/Dockerfile` and `NODE_EXTRA_CA_CERTS` have to change. Falls out of the `x-api-key` run.
- **Does `git push` over HTTPS work?** Only `git ls-remote` is wired into the spike, and that
  check skipped for want of a `--git-remote` URL. `git` shells out to `gh auth
  git-credential` per `custom/Dockerfile`, so the credential arrives as a placeholder that the
  MITM must substitute on a request `git` built — a different code path from `gh`'s own API
  calls. This is the claim `general.md`'s passthrough retirement rests on, and it is unverified.
- **Does a non-listed host receive the placeholder rather than the secret?** Untested, and the
  hardest to test safely — a naive probe against a third-party echo service would exfiltrate
  the credential if the scoping is broken, which is precisely the case being tested. Doing this
  properly needs a local HTTPS endpoint whose certificate the box already trusts; a plain HTTP
  listener would not be intercepted and would produce a false pass.
- **Does the SDK path still need the `clean-cache` sqlite workaround?** The tag→digest cache
  described in `general.md` is a property of the runtime, not the CLI, so probably yes — but an
  in-process runtime may expose invalidation properly and let that workaround be deleted.

## Language

BoxLite ships Python, Node, Go, Rust, and C SDKs. The engine is Rust, so the `boxlite` crate is
the native API and the others are bindings that may lag it.

The spike is Python because the two questions that decide the project are language-independent
and Python answers them fastest. Choosing the implementation language before the spike passes
would be committing to a port that might not be worth doing.

If it does pass, the choice is between Rust (native API, no binding lag, matches agentgateway
if the two are ever folded together) and Go (single static binary, drops cleanly into the
existing `bin/cb` install pattern). Neither is obviously right, and the decision should be made
against whatever the spike reveals about TTY handling, which is where the real work sits.

## Sequencing

1. ~~Run the spike.~~ Done for `GH_TOKEN`: substitution works and no CA change was needed. Next
   is the same spike with `ANTHROPIC_API_KEY` set (settles `x-api-key` and Node's CA trust in one
   run) and with `--git-remote` pointed at a private HTTPS repo (settles `git push`).
2. Independently of the spike, `--allow-net` can land on `just up`/`up-dev` as an opt-in flag.
   It needs no SDK and is the only item here that is cheap today.
3. Only then pick a language and port the `run`/`exec` paths, keeping `justfile` as the front
   door so the change is invisible to anyone using `just up` or `cb up`.

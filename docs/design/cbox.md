# cbox: replacing micro-VM management with a Rust binary

This document specifies `cbox`, a Rust program that takes over every part of this repo that
talks to BoxLite. It is a **design, not a description of what exists** — nothing here is
implemented.

Claims are tagged **verified** (measured against `boxlite 0.9.7`, the CLI, or a live box),
**read from source** (asserted by the crate's own code but not exercised), or **open**. The
split is load-bearing: two of the open items can still change the design.

**Where the evidence lives.** Every **verified** claim below states its measurement inline, so
this document stands on its own and does not need to be read alongside anything else. The
spikes it credits — `scripts/boxlite-secrets-spike.py` and `scripts/boxlite-secrets-spike-rs`
— are provenance for those measurements and live on
`claude/sdk-language-mitm-secrets-k9o5m4`; neither branch needs to land before the other.
`docs/design/boxlite-sdk.md` was copied across from there because it is the direct predecessor
and is referenced as an argument rather than as a citation.

The one place that ordering matters is implementation, not review: `attach.rs` is described
below as lifted from the Rust spike, and that code is on the other branch. Either it has
landed by then or the implementation cherry-picks that single file.

Predecessor: `docs/design/boxlite-sdk.md` evaluated driving BoxLite through its SDK and ended
by parking the language choice — "pick a language and port the `run`/`exec` paths, keeping
`justfile` as the front door." This document is that step. For the box side as it stands see
`docs/design/general.md`; for the host-side gateway see `docs/design/agentgateway.md`.

## Why this exists

**`secrets` is unreachable from the CLI.** BoxLite can substitute a real credential into
outbound HTTPS at the host boundary so the value never enters the guest. The guest holds a
placeholder; a per-box MITM proxy swaps in the real value for named hosts only.

**Verified.** `secrets` is a field on `BoxOptions` — the per-box create options. `boxlite run
--help` at 0.9.7 exposes `--allow-net`, `--network`, `-v`, `-p`, `-e`, `--config`, and no
secret flag. Because `secrets` sits on `BoxOptions` rather than `BoxliteOptions` (the struct
behind `--config`, whose only field is `image_registries`), the config file this repo already
passes cannot carry it either.

That asymmetry is the whole argument. Everything else cbox does is available some other way;
this is not. Today `docs/design/general.md` records that `GH_TOKEN` must reach the box because
"the box runs `gh` and `git push` itself — no proxy can do that on its behalf." A host-side
MITM can, and cbox is how this repo gets to use it.

**Language.** Rust, decided by porting the Python spike (`scripts/boxlite-secrets-spike.py`)
to `scripts/boxlite-secrets-spike-rs` and comparing. Every workaround in the Python version
turned out to be a binding artifact and none survived the port: greenlet's thread affinity
crashed the I/O pump, `wait()` returned a PyO3 `Future` rather than a coroutine, interactive
TTY required reaching into `box._box` and `box._sync_helper._sync()`, and REST mode required
`object.__new__` plus hand-populating five private fields. The decisive one is exit detection:
`ExecStdout` is a `Stream` over an mpsc receiver, so it yields `None` when the guest dies. The
Python binding lost that EOF, forcing a 0.3s poll and an empty-write liveness probe; in Rust
the probe deletes itself.

Costs found by building rather than reasoning: the crate compiles `boxlite-shared` from
source, whose `build.rs` requires **`protoc >= 3.12`** for host/guest gRPC codegen, so
contributors need `protoc` and a Rust toolchain. First build ~4 minutes, incremental 1-2s.
`cargo audit` over the resulting 411 crates reports **zero vulnerabilities** and six
unmaintained warnings, all reached through `qcow2-rs 0.1.6` and all identical in the Python
path — the wheel links the same code, just unauditably.

## Scope

cbox owns everything that talks to BoxLite. The justfile keeps everything that talks to
Docker. They share only the image tag and `registries.local.json`.

| cbox | justfile |
| --- | --- |
| `up`, `exec`, `down`, `list` | `build-base`, `build-image`, `build` |
| `logs`, `inspect`, `stats`, `cp` | `registry-up`, `registry-down`, `registry-login` |
| `clean-cache` | `gateway-up`, `gateway-down`, `gateway-logs` |
| per-box `BOXLITE_HOME` layout | `gateway-generate-ui-password` |
| `registries.local.json` parsing | `install`, `install-boxlite`, `uninstall` |

`clean-cache` moves because it performs surgery on BoxLite's own sqlite index. Whether it
survives at all is **open** — see Open questions.

## Front door

The justfile stays the front door. `just up` and `cb up` keep working and forward to cbox, so
this change is invisible to anyone using the repo today, which is what `boxlite-sdk.md`'s
sequencing asked for.

The forwarding recipe must `cd` first:

```make
up *args:
    cd {{invocation_directory()}} && \
      cbox up --config {{justfile_directory()}}/registries.local.json {{args}}
```

`just` sets the working directory to the justfile's own directory. Without the `cd`, cbox's
cwd would always be the repo root and every derived box name would resolve to
`claude-boxlite` regardless of where the user was standing — silently defeating the naming
design below. The `cd` makes cbox's cwd genuinely the user's, which then requires passing
`--config` explicitly since it is no longer relative.

## CLI surface

```
cbox up   [name] [-f] [-c] [-v host:box] [-e KEY[=VALUE]] [-i image]
                 [--cpus N] [--memory MiB] [-u user] [-p [host:]box[/proto]]
                 [--secret NAME=ENV_VAR@hosts] [--allow-net HOST]... [--network disabled]
                 [-- cmd...]
cbox exec [name] [-- cmd...]
cbox down [name]
cbox list [-a]
cbox logs [name] [-f] [-n N]
cbox inspect [name]
cbox stats [name] [-s]
cbox cp   <src> <dst>
cbox name
cbox clean-cache
```

## Box naming

The default box name is derived, so that in normal use the name disappears from the UX
entirely: `cbox exec` in a directory finds that directory's box without being told.

Precedence:

1. explicit positional argument — `cbox up myname`
2. `CBOX_NAME` environment variable
3. basename of `git rev-parse --show-toplevel`
4. basename of the current directory
5. the literal `claude-box`

Level 3 is the interesting one: it resolves to the same box from anywhere inside a repo, so
`cbox exec` works from a subdirectory. Using the cwd basename directly would target a
*different* box from `base/` than from the repo root, which is exactly the confusion
derivation is meant to remove.

Level 2 gives per-project pinning for free. The justfile already does `set dotenv-load`, so
`CBOX_NAME=whatever` in a repo's gitignored `.env` pins that repo's box with no new file
format.

**Sanitization**, since derived names become filesystem paths at `boxes/<name>/`: anything
outside `[A-Za-z0-9._-]` becomes `-`, consecutive dashes collapse, leading and trailing dashes
are trimmed, the result truncates at 64 characters, and an empty result falls through to
`claude-box`. This is pure logic and gets direct unit tests — a repo named `foo.bar/baz` is
the kind of input that breaks a naive implementation.

**`cbox name`** prints the name that would be used here and which rule produced it. A derived
default is magic, and magic has to be inspectable. `cbox list` marks the box matching the
current directory.

**Cost.** Each box name gets its own `BOXLITE_HOME` and therefore its own image cache — this
is why naming granularity is a disk decision. Deriving per *repo* keeps that bounded; deriving
per repo-and-branch was rejected because a week of feature branches is many gigabytes of
duplicated image cache, and the process model below does not collapse homes.

Migration is explicitly out of scope. Existing `claude-box` boxes keep working; they are
simply no longer the default.

## Lifecycle and process model

### Detach is mandatory

**Read from source.** `BoxOptions::detach` defaults to false, and its documentation says the
box "stops when the runtime that created it is dropped." Under that default, exiting `cbox up`
would destroy the box — a regression against today's behavior, where `just down` exists
precisely because the box outlives `just up`.

cbox therefore sets **`detach: true`**. `auto_remove: true` is then correct and harmless: with
`detach: true` a dropped runtime no longer stops the box, so removal happens only on an
explicit `cbox down`, which is exactly today's `stop` + `rm`.

The consequence to accept: a crashed or SIGKILL'd `cbox up` leaves a box running with no
owner. That is already true today, and `cbox list -a` plus `cbox down` remain the recovery.

### `up` serves `exec`

BoxLite takes an exclusive lock on an entire `BOXLITE_HOME` for as long as a runtime is
attached, which is why `general.md` documents that `just exec <name>` cannot run while `just
up <name>` is attached. Per-box homes let *different* boxes run concurrently but cannot fix
this, because the lock is on the one home the box lives in.

cbox fixes it without a daemon. `cbox up` already holds a long-lived runtime; it additionally
listens on a unix socket at `<home>/cbox.sock`. `cbox exec` connects to it when present.

```
cbox up claude-boxlite          # holds runtime + lock, listens on cbox.sock
cbox exec claude-boxlite        # connects to the socket -> works immediately
cbox exec other-box             # no socket -> opens its own runtime
```

A daemon was considered and rejected. It would additionally collapse the per-box image cache
duplication, which is a real disk win, but it costs daemon lifecycle, stale-daemon and
version-skew handling, socket permissions, and "where did my box go after reboot" — too much
machinery for a dev tool, and `up` is a process that already exists and already holds
everything needed.

### Connect-or-fallback

The failure modes are the design here, and all three must be handled explicitly:

- **socket absent** — open our own runtime and attach to the detached box
- **socket present, `ECONNREFUSED`** — stale, left by a crashed `up`; unlink it and fall back
- **socket present and accepting** — proxy over it

`up` unlinks its socket on clean exit, but SIGKILL cannot. The stale path is therefore
load-bearing rather than defensive: without it, one crashed `up` bricks `exec` for that box
until someone deletes a file by hand.

### Secrets survive the creating process

**Read from source, not verified.** `vmm/controller/shim.rs` carries the comment "Pass port
mappings to subprocess (shim creates gvproxy)", and secrets reach the shim through its config
pipe. The MITM proxy is therefore created by the per-box shim, not by the runtime process, and
lives as long as the box does.

If that holds, `cbox exec` gets substitution for free even in the fallback path, because the
proxy is already running alongside the VM. If it does not, the fallback path silently loses
substitution and only the socket path works — which would make the socket mandatory rather
than an optimization. **This must be verified before implementation**: boot a box with
secrets, exit the creating process, exec from a second process, and check `gh api user`.

## Credential model

### GitHub moves to secrets

`GH_TOKEN`/`GITHUB_TOKEN` leave the passthrough list. This requires **two** secrets, because
`gh` and `git` want the credential in incompatible shapes.

**Verified.** `gh` sends `Authorization: Bearer <placeholder>` to `api.github.com`; the
placeholder is on the wire and the proxy substitutes it.

**Verified.** `git` does not work that way. Its credential helper hands over a token and git
builds `Authorization: Basic base64(user:token)` itself — base64 hides the placeholder from a
literal string matcher, so the raw-token secret can never reach a git request. Forcing a
Bearer header via `http.extraHeader` also fails: measured on the host with a real token,
`github.com`'s `git-upload-pack` returns **401 for Bearer and 200 for Basic**. git to GitHub
is Basic or nothing.

**Verified.** The fix is to move the base64 to the host side — store the *already encoded*
credential as the secret's value and put the placeholder where the encoded blob belongs, using
`http.extraHeader`, which git passes through verbatim:

```
Secret { name: "gh",       hosts: ["api.github.com"],  value: <raw token> }
Secret { name: "gh_basic", hosts: ["github.com"],
         value: base64("x-access-token:" + <raw token>) }

git config --global http.https://github.com/.extraHeader \
    "Authorization: Basic <BOXLITE_SECRET:gh_basic>"
git config --global credential.helper ''
```

Blanking the helper is required, not tidiness: without it git falls back to it on a 401 and
sends base64'd garbage. cbox applies both settings when it creates a box with GitHub secrets.

**Why cbox applies these at runtime rather than baking them into `custom/Dockerfile`.** The
`extraHeader` value is a fixed placeholder, so it looks like static image configuration — and
the image is already where the related git credential config lives. But baking it
unconditionally breaks the case with no GitHub secret: the box would still send
`Authorization: Basic <BOXLITE_SECRET:gh_basic>` with nothing to substitute it, and GitHub
returns 401 even for an anonymous clone of a public repo, which works today. Something at
runtime has to know whether a GitHub secret is actually in play, and that is cbox.

The cost of that choice is an assumption: cbox is running `git config` inside the guest, so it
assumes the image has git. It applies the bootstrap **non-fatally** for exactly that reason —
probe for git first, warn and skip if absent, and never fail `cbox up` over it. A box that
skipped the bootstrap will 401 on its first git operation against GitHub, which is visible
immediately and named by the warning; aborting the whole session over an inessential
configuration step would be worse.

**Required follow-up on the image.** `custom/Dockerfile` currently sets
`credential.https://github.com.helper` to `gh auth git-credential`. Under the two-secret model
that line is not merely redundant, it is harmful: on a 401 git retries through that helper and
sends base64'd placeholder garbage — precisely the confusing failure the `credential.helper ''`
blanking exists to prevent. cbox blanks it per-box, so cbox-launched boxes are correct, but the
line should be deleted from the image for anyone using it directly.

**Verified end to end** from a box holding only placeholders: `gh api user`, `git clone` of a
private repo, and commit plus `git push -u` of a new branch. Custody holds throughout —
`printenv GH_TOKEN` in the guest returns the placeholder and nothing else, which is the check
that would catch the feature degrading to plain passthrough.

**Verified.** Substitution is HTTPS-only. A plain-HTTP request carrying the placeholder
arrives unmodified, because there is no TLS interception to rewrite through.

Note the dependency this rests on: `custom/Dockerfile` routes git HTTPS through `gh auth
git-credential`, and the `extraHeader` approach replaces that. A box that pushed over SSH
would be outside the MITM's reach entirely and the property would silently not apply.

### Anthropic stays on the gateway

`ANTHROPIC_BASE_URL` continues to point at the gateway's `/api` route, which injects the real
key via `backendAuth` and keeps it host-side. The gateway remains the credential boundary for
Anthropic, plus MCP multiplexing and telemetry.

Moving Anthropic to a `Secret` was considered and deferred because it rests on two unverified
things (see Open questions): whether substitution covers the `x-api-key` header Claude Code
actually sends, and whether Node trusts BoxLite's MITM CA. Today's Anthropic traffic goes over
plain HTTP to `host.boxlite.internal`, which bypasses the MITM entirely — so the fact that
Claude Code works today says nothing about that path.

### Generic secrets and passthrough

Nothing about the mechanism is GitHub-specific. GitHub is a built-in *preset* — one entry in a
general table, shipped because cbox already knows its hosts and its base64 quirk — and any
other credential uses the same machinery:

```
--secret NAME=ENV_VAR@host[,host...]
```

The flag names the environment variable holding the value; the value itself never enters
`argv`. This is deliberate and follows a norm the repo already set, in
`gateway-generate-ui-password`: "never passed as an argument — that would land in both shell
history and `ps` output." `ps` is readable by any local process.

Hosts are mandatory. An unscoped secret would be substituted on requests to any host, which
inverts the property the feature exists to provide. That check belongs at the point where the
SDK's `Secret` is constructed, not only where flags are parsed — the invariant is the security
property itself, and it should not rest on every future caller remembering to validate first.

Values continue to come from the environment, populated by a gitignored `.env` exactly as
today. The passthrough list becomes configuration rather than the hardcoded `passthrough_vars`
string, so any variable can be forwarded without editing code.

### How the three environment mechanisms compose

cbox has three ways a variable reaches a box, and they must not be confused:

| Mechanism | Source of value | What the guest sees |
| --- | --- | --- |
| passthrough list | host env, if set | the real value |
| `-e KEY=VALUE` | the flag itself | the real value |
| `-e KEY` | host env, if set | the real value |
| `--secret NAME=VAR@hosts` | host env var `VAR` | a **placeholder** |

`-e KEY=VALUE` is today's justfile flag, unchanged. `-e KEY` (no `=`) is added as the ad-hoc
form of the passthrough list — it forwards `$KEY` from the host, mirroring `docker run -e`,
and costs nothing to support.

Precedence, most specific first: `-e` beats the passthrough list for the same key, so a flag
always overrides `.env`. An unset variable is skipped rather than passed empty, which is
today's behavior and matters because an empty `GH_TOKEN` is worse than an absent one.

**A variable that backs a secret is never also passed through as a plain value.** If
`GH_TOKEN` is a secret's source, the guest's `GH_TOKEN` is set to that secret's placeholder
and the real value is withheld — that is the entire custody property. cbox rejects a
configuration that both names a variable as a secret source and lists it for plain
passthrough, rather than silently picking one; a silent choice here is the difference between
a credential staying host-side and not, and it would look identical from every network probe.

`-e` is not a secret mechanism. Its value is written in `argv` and is visible in `ps` and
shell history, which is fine for `BOX_NAME` or `ANTHROPIC_BASE_URL` and wrong for a
credential.

## Configuration

cbox parses `registries.local.json` itself and maps it to `Vec<ImageRegistry>`; the SDK takes
constructed values, not a file path. The file's nested `auth` block flattens onto
`ImageRegistry::with_basic_auth`. Format and location are unchanged, so `just build`,
`registry-login.py`, and the gitignore all keep working untouched. cbox also inherits the
first-run bootstrap that copies `registries.json` to `registries.local.json`.

Resolution order: `--config` (what the justfile passes), then `$CBOX_REGISTRIES`, then
`~/.config/cbox/registries.json` for direct use outside the repo. A missing file yields no
registries rather than an error — the box still boots against `docker.io`.

## New capabilities

These exist in the BoxLite CLI today and the justfile has never used them.

**Egress allow-list.** `--allow-net HOST` (repeatable; exact host, `*.example.com`, IP, or
CIDR) restricts egress and DNS-sinkholes everything else; `--network disabled` removes the
interface entirely. This is the strongest security control available here — the box runs an
autonomous agent over your source with a GitHub token, and an allow-list is what stops a
prompt-injected agent or a poisoned dependency from shipping that source anywhere.

Two constraints make it opt-in rather than a default. It costs `WebFetch` and `WebSearch`,
which run inside the box against arbitrary domains. And any non-empty allow-list **must**
include `192.168.127.254` (`host.boxlite.internal`) or the baked MCP config cannot resolve —
which, since the list has no port syntax, restores reachability to every published host port
at once. **The ports policy in `agentgateway.md` stays exactly as load-bearing as written**,
and `--allow-net` must not be described as having relaxed it.

**Observability.** `cbox logs [-f] [-n]`, `list -a`, `inspect`, `stats [-s]`. These close a
real gap created by `detach: true`: a box that fails to start otherwise leaves no output and
no way to see it, and stopped boxes are invisible without `-a`.

**Resource limits.** `--cpus`, `--memory`, `-u/--user` map directly onto `BoxOptions`.

**File transfer and ports.** `cbox cp` and `-p/--publish`. Neither has a driving need today —
`-v`/`-c` already covers getting the workspace in, and the gateway is host-side and reached
via `host.boxlite.internal` rather than a published box port — but both are thin pass-throughs
and were requested.

Snapshots, `clone_box`, and archive export/import are SDK-only and deliberately excluded.
Fast-forking a prepared box is interesting for agent workflows but nothing needs it yet.

## Internal architecture

`cbox/` at the repo root, alongside `base/`, `custom/`, and `agentgateway/` — the repo already
uses one top-level directory per component.

| Module | Responsibility |
| --- | --- |
| `main.rs` | clap surface and dispatch only |
| `naming.rs` | the precedence chain and sanitization |
| `config.rs` | `registries.local.json` → `ImageRegistry`; per-box home layout |
| `env.rs` | `-e` parsing, passthrough collection, secret-conflict rejection |
| `secrets.rs` | preset table, secret construction, git `extraHeader` bootstrap |
| `boxopts.rs` | assemble `BoxOptions` from flags, config, and secrets |
| `attach.rs` | TTY attach loop, lifted from the Rust spike |
| `proto.rs` | socket frame codec |
| `server.rs` | `up`'s listener |
| `client.rs` | `exec`'s connect-or-fallback |
| `commands/` | one file per verb |

Everything except `attach.rs`, `server.rs`, and `commands/` is testable without booting a VM.

### Socket protocol

Deliberately small: length-prefixed frames, `u8` tag plus `u32` length plus payload. Control
frames are JSON — `serde_json` is already a dependency — and data frames are raw bytes.
Explicitly not `bincode`, which the dependency audit flagged as unmaintained.

| Tag | Frame | Direction |
| --- | --- | --- |
| 0 | `Exec {cmd, env, rows, cols}` | client → server |
| 1 | `Stdin` raw bytes | client → server |
| 2 | `Resize {rows, cols}` | client → server |
| 3 | `Stdout` raw bytes | server → client |
| 4 | `Exit {code}` | server → client |

The `Exec` frame carries no `args` or `tty` field, and both omissions are deliberate. `cmd`
holds the full argv — the server takes `cmd[0]` as the program and `cmd[1..]` as its arguments —
so a separate `args` field would be redundant. And every session over this socket is an
interactive attach by construction, so the server always allocates a TTY; a field that only ever
holds one value is not a field.

`env` **is** carried, and it is not the box's environment. The box's environment is fixed at
creation and exec'd processes inherit it. What cannot be inherited is the *client's terminal
identity*, because `cbox exec` runs in a different terminal from the `cbox up` that owns the
socket. Reading `TERM` on the server side would report whichever terminal started the box.
`justfile:17-19` records why that matters concretely: Claude Code uses `TERM_PROGRAM` to decide
whether to enable the Kitty keyboard protocol, "which is what lets a terminal tell Shift+Enter
apart from plain Enter." Answering that question with the wrong terminal's variables silently
breaks Shift+Enter in an exec session, so the client sends its own and the server applies them
to the command.

## Testing

`cargo test` covers the pure logic, which is most of the risk that is not in the VM:

- name derivation and sanitization across each precedence level, plus adversarial inputs
  (`foo.bar/baz`, empty, over-length, all-punctuation)
- `registries.local.json` parsing: transport selection, flattened auth, malformed and missing
  files degrading to empty rather than panicking
- secret construction: that the `gh_basic` blob decodes to `x-access-token:<token>`, and that
  the placeholder does *not* survive encoding — the asymmetry the whole git fix rests on
- `--secret NAME=ENV_VAR@hosts` parsing, including rejection of a missing host list
- frame codec round-trips, including partial reads and oversized frames

Not unit-testable and verified by running: TTY attach, the socket path, and substitution
itself. These need a booted box and real credentials, so they cannot be `cargo test`. The two
spikes remain the integration harness — they already run these checks against a real box, and
`scripts/boxlite-secrets-spike-rs` shares the attach implementation.

One integration check is load-bearing enough to name explicitly, because an open question
depends on it and its failure mode is silent:

```
cbox up <name>                 # box created with secrets; exit the session
cbox exec <name> -- gh api user --jq .login
```

Run with the socket path disabled so `exec` is forced through connect-or-fallback into its own
runtime. Success means secrets live with the box's shim and the fallback is sound. Failure
means substitution belongs to the creating process, and the fallback must be deleted rather
than fixed — `exec` would have to require the socket and fail loudly when it is absent, since
a box that has quietly lost substitution is indistinguishable from a working one until a
request gets rejected.

## Open questions

These can still change the design.

- **Do secrets survive the creating process?** Read from source as yes (the shim creates
  gvproxy). If no, the socket path becomes mandatory rather than an optimization, because
  `exec`'s fallback would silently lose substitution — and *silently* is the problem: a box
  that has lost substitution looks identical to a working one until a request is actually
  rejected. Checked during implementation as an integration test (see Testing) rather than as
  a gate beforehand. The cost of being wrong is bounded: it deletes the fallback path, it does
  not change the rest of the architecture.
- **Does the SDK replace `clean-cache`'s sqlite surgery?** `runtime.images()` exists; whether
  it exposes tag→digest invalidation is unchecked. If not, the `DELETE FROM image_index`
  workaround ports as-is and the design should say so plainly rather than pretend otherwise.
- **Does substitution cover `x-api-key`?** Decides whether Anthropic can ever leave the
  gateway. Only `Authorization` is verified.
- **Does Node trust the MITM CA?** Node ships its own CA store and ignores the system store.
  Decides whether `NODE_EXTRA_CA_CERTS` and a `base/Dockerfile` change are needed for any
  Anthropic-over-MITM path.
- **Does a non-listed host receive the placeholder rather than the secret?** The scoping
  property is the security model and is untested. Testing it naively would exfiltrate the
  credential if scoping is broken — precisely the case under test — so it needs a local HTTPS
  endpoint the box already trusts. A plain-HTTP listener produces a false pass, since
  substitution is HTTPS-only.

## Sequencing

1. Land `cbox up`/`exec`/`down`/`list` with GitHub secrets and derived naming, forwarded from
   the justfile. This is the milestone that retires the `GH_TOKEN` passthrough. Land the
   secrets-survival integration check with it — `exec` cannot be called done until the
   fallback path is known to keep substitution.
3. Add observability, then `--allow-net`, then resource limits, then `cp`/`--publish`.
4. Settle `clean-cache` — port the sqlite workaround or replace it, and record which.
5. Revisit Anthropic only if the `x-api-key` and Node CA questions both resolve favorably.

# claude-boxlite

Build and run [Claude Code](https://github.com/anthropics/claude-code) inside a
[BoxLite](https://boxliteai.com) microVM, with an MCP config baked in that points Claude
Code at a host-side [agentgateway](https://agentgateway.dev). One `just` command builds the
image and boots the box.

This repo covers both halves: the **box side** (building the image, running the VM) and the
**host side** (`agentgateway/`, run with `just gateway-up`). The box's baked MCP config points
at `http://host.boxlite.internal:3000/mcp`, which the gateway serves. The gateway can also
broker Anthropic traffic — with an API key it holds the key host-side so the VM never sees it.

## How it works

- **Two-layer image.** `base/` builds `claude-boxlite-base` (Debian + Node 20 + Claude
  Code) — slow, rebuilt rarely. `custom/` layers `claude-boxlite-custom` on top, baking
  `custom/claude.json` in as `/root/.claude.json` (theme, onboarding, and a user-scoped
  `agentgateway` MCP server). Nothing is baked into `/workspace`, so mounting a host
  directory there clobbers no config.
- **Image handoff via a local registry.** BoxLite does not read Docker's local image
  store, so the custom image is pushed to a local `registry:2` (managed by docker compose
  under `local-development/registry/`) and BoxLite pulls it from there. BoxLite is actually
  pointed at `registries.local.json` (gitignored), auto-created from the tracked
  `registries.json` template the first time you run `just up`/`up-dev`, so it's safe to add
  authenticated registries (e.g. ECR via `just registry-login`, see below) locally without
  ever touching the tracked file.
- **Credentials.** Secrets are read from a gitignored `.env` and passed to the box at run
  time via env-var injection (BoxLite's official credential mechanism) — never baked into
  an image. A known set of vars is forwarded when set (see `passthrough_vars` in the
  `justfile`): Claude auth is not a flat list but one mutually exclusive set picked by
  `llm_vars` based on what `.env` contains — subscription, direct API key, or gateway-keyed
  API key (see **The host-side gateway** below for exactly which vars each set includes),
  plus `ANTHROPIC_MODEL` always optional on top — GitHub (`GH_TOKEN`/`GITHUB_TOKEN`), and git
  identity (`GIT_AUTHOR_*` / `GIT_COMMITTER_*`). Unset vars are skipped.
- **GitHub.** Setting `GH_TOKEN` (or `GITHUB_TOKEN`) authenticates the `gh` CLI
  automatically; git is preconfigured to use gh's credential helper, so `git clone`/`push`
  over HTTPS work too. Commit identity comes from the `GIT_AUTHOR_*` / `GIT_COMMITTER_*`
  vars.

## Prerequisites

- [`docker`](https://docs.docker.com/get-docker/) with `docker compose`
- [`boxlite`](https://boxliteai.com) CLI — install the latest into `~/bin` with
  `just install-boxlite`, or pin a version with `just install-boxlite v0.9.7` (see below)
- [`just`](https://github.com/casey/just)

## Setup

Install the `boxlite` CLI if you don't already have it — this downloads the release tarball
directly from [GitHub releases](https://github.com/boxlite-ai/boxlite/releases) (no piped
install script), verifies its sha256 checksum, and installs into `~/bin` by default:

```bash
just install-boxlite                    # latest release, installed to ~/bin
just install-boxlite v0.9.7             # pin a specific version
just install-boxlite "" /usr/local/bin  # install to a different directory
```

Copy the env template and set your credentials:

```bash
cp .env.example .env
# Claude auth — pick ONE:
#   subscription: `claude setup-token`, then CLAUDE_CODE_OAUTH_TOKEN=...
#                 optionally ANTHROPIC_BASE_URL=http://host.boxlite.internal:3001/claude
#   API key:      ANTHROPIC_API_KEY=... plus
#                 ANTHROPIC_BASE_URL=http://host.boxlite.internal:3001/api
#                 and ANTHROPIC_AUTH_TOKEN=unused (value unchecked; the gateway
#                 attaches the real key, so it never enters the box)
# Optional GitHub: set GH_TOKEN=... (a PAT) — used by the box AND the gateway's github MCP target
# Optional git identity: GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL
```

> **Upgrading:** if your `.env` already sets `ANTHROPIC_BASE_URL` for some other proxy, add
> `ANTHROPIC_AUTH_TOKEN=...` to it. Setting `ANTHROPIC_BASE_URL` now means "the credential
> lives outside the box", so `ANTHROPIC_API_KEY` is no longer forwarded — without an auth
> token the box would reach your proxy with no credential at all.

Copy the registries template the same way (or let `just up`/`up-dev` create it for you on first
run):

```bash
cp registries.json registries.local.json
```

`registries.local.json` is gitignored — it's the file BoxLite's `--config` actually reads, so
it's where `just registry-login` (see below) writes credentials for authenticated registries
like ECR, without ever touching the tracked `registries.json`.

## Usage

```bash
just up-dev            # build images (base + custom, pushed to the local registry) then boot the box
just up                # boot the box without rebuilding (images must already be built)
just build             # start the local registry, build base + custom images, push custom
just shell             # open a session in the running box
just list              # list running boxes
just down              # stop and remove the box
just gateway-up        # start the host-side agentgateway (MCP + Anthropic routes)
just gateway-down      # stop it
just gateway-logs      # follow its logs
```

Use `just up-dev` the first time (or after changing the image); use `just up` for a fast
boot once the images are built. Both run Claude Code interactively inside the box, so they
need a valid `CLAUDE_CODE_OAUTH_TOKEN` in `.env`. The `agentgateway` MCP server is
configured user-scoped in `/root/.claude.json`, so Claude Code points at the host gateway
in any project — including a mounted host directory.

### The host-side gateway

`just gateway-up` runs agentgateway from `agentgateway/docker-compose.yml`. It binds
`127.0.0.1` only — the box still reaches it because `host.boxlite.internal` resolves to the
host loopback proxy, so nothing is exposed to your network.

| Bind | Serves |
|---|---|
| `:3000/mcp` | multiplexed MCP tools (`github` live, proxied to a sibling `github-mcp` container — not GitHub's remote endpoint; others commented in `agentgateway/config.yaml`) |
| `:3001/claude` | Anthropic passthrough — your subscription OAuth token goes upstream untouched |
| `:3001/api` | Anthropic-Messages-API keyed — the gateway attaches `ANTHROPIC_API_KEY`, which stays on the host; upstream defaults to `api.anthropic.com` but is configurable via `AGENTGATEWAY_ANTHROPIC_UPSTREAM_HOST` (e.g. for a LiteLLM key) |
| `:15000/ui` | admin UI |

(`:3000` and `:3001` are two separately named gateways in `agentgateway/config.yaml`'s
`gateways:` map — `mcp-gateway` and `llm-gateway` — not one gateway with two binds.) `:3000`
also allows CORS from the admin UI's tool playground (`127.0.0.1:15000`) so it can call the
MCP endpoint directly from browser JavaScript; the box itself talks to it server-to-server and
is unaffected.

It is long-lived and restarts with Docker; `just up`/`up-dev` do not start it. If
`ANTHROPIC_BASE_URL` points at it and it is not running, the box will fail to reach Anthropic
— `just gateway-logs` is the first thing to check.

The `/mcp` half works in every auth mode. Only the keyed mode keeps a credential off the VM:
in subscription mode Claude Code must hold the OAuth token to send it, so that mode buys
observability and a single egress point, not credential custody.

If `ANTHROPIC_API_KEY` isn't actually an Anthropic key — e.g. a LiteLLM key — set
`AGENTGATEWAY_ANTHROPIC_UPSTREAM_HOST` in `.env` to the bare host it should be sent to instead (no scheme,
no path). This only changes where the `/api` route forwards to; it's unrelated to
`ANTHROPIC_BASE_URL`, which is where the box itself sends traffic (always the gateway, in
this mode).

**What actually stays off the box.** "The gateway keeps credentials host-side" is about one
credential, not all of them:

| Credential | Reaches the box? | Why |
|---|---|---|
| `ANTHROPIC_API_KEY` | No | the box never calls Anthropic directly — the gateway does it on the box's behalf, so the key has no reason to be there |
| `GH_TOKEN` / `GITHUB_TOKEN` | Yes | the box runs `gh` and `git push` itself, and no proxy can do that for it |

**Troubleshooting**

| Symptom | Cause |
|---|---|
| `/mcp` connects but lists no tools | `GH_TOKEN` unset or expired — the `github-mcp` container's GitHub API calls 401, visible in `just gateway-logs` |
| 401 from Anthropic | your `ANTHROPIC_BASE_URL` path and your credential disagree: `/claude` needs the OAuth token, `/api` needs the gateway to have `ANTHROPIC_API_KEY` |
| 401 in subscription mode with the right path | `ANTHROPIC_AUTH_TOKEN` is set and shadowing the OAuth token — unset it |
| 400 `Extra inputs are not permitted` | beta headers the backend rejects; set `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` and add it to `passthrough_vars` |
| Box can't reach Anthropic at all | the gateway isn't running — `just gateway-up` |

`up`, `up-dev`, `shell`, and `down` take an optional box name (default `claude-box`), so you
can run several boxes side by side. `up`/`up-dev` also accept `-f`/`--force` to replace an
existing box of the same name (without it, a name collision errors out):

```bash
just up-dev my-box     # build + boot a box named "my-box"
just up my-box -f      # re-boot it, replacing the running box
just up --cwd          # boot with the host current directory mounted at /workspace
just shell my-box      # open a session in it
just down my-box       # tear it down
```

`up`/`up-dev` also accept `-c`/`--cwd` (mount the host current directory onto `/workspace`),
`-v host:box` (mount an arbitrary host folder, repeatable), and `-e KEY=VALUE` (inject an
extra environment variable into the box, repeatable):

```bash
just up -e test=1 -e test2=2   # boot with test=1 and test2=2 set in the box
```

Other recipes: `just registry-up` / `just registry-down` manage the local registry
directly; `just gateway-up` / `just gateway-down` / `just gateway-logs` manage the host-side
agentgateway (see below); `just --list` shows everything.

### Running from anywhere

`just` only finds a justfile in the current (or a parent) directory, so by default these
commands only work from inside this repo. To run them from any directory, install the
`cb` wrapper onto your `PATH`:

```bash
just install    # symlinks bin/cb into ~/bin (pass a dir to override)
cb up-dev       # now works from anywhere
```

`just uninstall` removes the symlink. The wrapper just runs `just --justfile
/path/to/this/repo/justfile "$@"`, so it behaves identically to running `just` from inside
the repo, including recipes' relative paths (e.g. `registries.json`). `-c`/`--cwd` uses
`just`'s `invocation_directory()` rather than `$PWD` so it mounts the directory you actually
ran the command from, not the repo's own directory.

## Windows

`just` recipes run under `sh`. On Windows, install Git Bash and add
`set windows-shell := ["bash", "-cu"]` near the top of the `justfile`, or run under WSL.

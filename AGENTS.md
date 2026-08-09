# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## What this repo builds

A two-layer Docker image that runs Claude Code inside a [BoxLite](https://boxliteai.com)
microVM, with an MCP config baked in pointing at a host-side
[agentgateway](https://agentgateway.dev) (`http://host.boxlite.internal:3000/mcp`). This repo
covers both halves: the box side (building the image, booting the VM) and the host side
(`agentgateway/`, a docker compose service started with `just gateway-up`), which serves MCP
on `:3000` and two Anthropic routes on `:3001`.

## Commands

```bash
just up-dev            # build images (base + custom, pushed to local registry), then boot the box
just up                # boot the box without rebuilding (images must already be built)
just build              # start the local registry, build base + custom images, push custom
just exec               # open a session in the running box (alias: just shell)
just list               # list running boxes across every box name (see below), forwarding args to `boxlite list`
just down               # stop and remove the box
just gateway-up/down/logs # manage the host-side agentgateway
just registry-up/down   # manage the local docker-compose registry directly
just registry-login     # log in to an authenticated registry (e.g. ECR), see below
just install/uninstall  # symlink the cb wrapper onto PATH (see below)
```

`up`/`up-dev` take an optional box name (default `claude-box`) and flags: `-f`/`--force`
(replace an existing box of the same name), `-c`/`--cwd` (mount host cwd onto `/workspace`),
`-v host:box` (mount an arbitrary host folder, repeatable), `-e KEY=VALUE` (inject an extra
environment variable into the box, repeatable, appended to `envflags` alongside
`passthrough_vars`), and `-- <cmd>` (override the executable launched in the box; defaults to
`claude`, e.g. `just up -- bash`). `exec` takes the same optional box name and `-- <cmd>`
override (e.g. `just exec -- bash`) to exec something other than `claude` in the running box.

`just` only looks for a justfile in the current or a parent directory, so these recipes only
work from inside the repo by default. `just install` symlinks `bin/cb` — a
wrapper that runs `just --justfile <repo>/justfile "$@"` — onto `PATH` (default
`~/bin`, override with `just install <dir>`), so `cb up-dev` works from
anywhere. `just` sets the working directory to the justfile's own directory when invoked with
`--justfile`, so recipes' relative paths (`registries.local.json`,
`local-development/registry/docker-compose.yml`) resolve correctly either way. `just
uninstall` removes the symlink.

There is no test suite or linter in this repo; verification is building the images and
booting a box (`just up-dev`).

## Running multiple boxes at once

Each box name gets its own `BOXLITE_HOME`
(`${BOXLITE_HOME:-$HOME/.boxlite}/boxes/<name>`), passed to every `boxlite` invocation via
`--home`. BoxLite takes an exclusive filesystem lock on the whole `BOXLITE_HOME` directory
for as long as a `boxlite run`/`exec` process is attached to it — not just on the one box —
so two boxes sharing a home can't run concurrently (`Failed to acquire runtime lock ...
Another BoxliteRuntime is already using directory`). Splitting the home per box name is what
lets `just up box-a` and `just up box-b` run at the same time from separate terminals. This
also means `just exec <name>` only succeeds once the `just up <name>` session for that same
box has exited — both commands open their own local runtime and take the same per-home lock,
so a box can only be attached from one CLI process at a time; that's a limitation of
BoxLite's CLI process model, not something specific to this repo. `just list` and `just
clean-cache` iterate over every `boxes/*` home to cover all box names. A shared `boxlite
serve` daemon would sidestep the per-process lock entirely (many boxes, one runtime, one
lock), but its REST API doesn't yet forward `-v`/`-c` bind mounts into the box
(boxlite-ai/boxlite#942), which this repo depends on for mounting the host workspace — so
that's not viable until upstream lands it.

## Architecture

- **Two-layer image.** `base/Dockerfile` builds `claude-boxlite-base` (Debian + Node + Claude
  Code + `gh` + `uv`) — slow, rebuilt rarely. `custom/Dockerfile` layers
  `claude-boxlite-custom` on top: bakes `custom/claude.json` in as `/root/.claude.json`
  (theme, onboarding-complete flag, user-scoped `agentgateway` MCP server) and installs the
  plugin marketplaces/plugins listed in its `MARKETPLACES`/`PLUGINS` build args. Nothing is
  baked into `/workspace`, so mounting a host directory there clobbers no config.
- **Image handoff via a local registry.** BoxLite does not read Docker's local image store, so
  `just build-image` pushes the custom image to a local `registry:2` (started by
  `local-development/registry/docker-compose.yml`) and BoxLite pulls from there. BoxLite is
  pointed at `registries.local.json` (gitignored) via `--config`, not the tracked
  `registries.json` directly — `just up`/`up-dev` copy the latter to the former on first run
  (if it doesn't already exist) so it's safe to add per-machine registry credentials to the
  local copy without touching the tracked template.
  `just clean-cache` drops BoxLite's cached tag→digest row for the custom image (and sweeps
  orphaned blobs) so a rebuilt `:latest` is actually re-pulled instead of served from cache —
  see the comment above the `clean-cache` recipe in the `justfile` for why disk-images are
  deliberately left alone.
- **Credentials via env-var passthrough, not baked images.** `justfile`'s `passthrough_vars`
  lists the vars forwarded into the box when set (from a gitignored `.env`, loaded via
  `set dotenv-load`): Claude auth is one mutually exclusive set picked by `llm_vars` based on
  what `.env` contains — subscription, direct API key, or gateway-keyed API key (see
  **Host-side gateway** below for exactly which vars each set includes), plus
  `ANTHROPIC_MODEL` always optional on top — GitHub (`GH_TOKEN`/`GITHUB_TOKEN`), git identity
  (`GIT_AUTHOR_*`/`GIT_COMMITTER_*`), and terminal identity (`TERM_PROGRAM` and friends — see
  below). Add a var to `passthrough_vars` to make it available in the box.
- **Terminal identity passthrough.** `TERM` is forwarded unconditionally (hardcoded on the
  `boxlite run`/`exec` invocations, defaulting to `xterm-256color`) rather than living in
  `passthrough_vars` — that list only forwards a var when the host already has it set, with no
  way to fall back to a default, and `TERM` needs one so the box always renders in color even
  when the host's `TERM` is unset (headless invocations, some IDE terminals). `TERM` alone is
  often the same generic value (`xterm-256color`) across unrelated terminal emulators, though, so
  it can't identify the terminal on its own. Claude Code
  additionally reads `TERM_PROGRAM` (and related vars like `KITTY_WINDOW_ID`,
  `WEZTERM_EXECUTABLE`, `ITERM_SESSION_ID`) to identify the actual terminal emulator, which
  decides e.g. whether to enable the Kitty keyboard protocol that lets a terminal distinguish
  Shift+Enter from plain Enter. `passthrough_vars` forwards this group so behavior inside the
  box matches running `claude` directly on the host in the same terminal. `just up`/`just exec` also always
  inject `BOX_NAME`, set to the box name being booted/attached to (independent of
  `passthrough_vars`, since it's not a host env var), so a session can tell which box it's
  running in.
- **Host-side gateway.** `agentgateway/docker-compose.yml` runs two services, not one:
  `agentgateway` itself, and a sibling `github-mcp` container (GitHub's official
  `github-mcp-server` image, run in HTTP mode with no published host port — only agentgateway
  reaches it, over the compose network). `config.yaml`'s `github` MCP target proxies there
  instead of GitHub's remote MCP endpoint, which is unreachable from this Docker host (TLS
  handshake failure, reproduced independently of agentgateway); `github-mcp-server` enforces
  its own OAuth layer on every HTTP-mode request including `initialize`, so agentgateway
  injects `Authorization: Bearer $GH_TOKEN` via a `requestHeaderModifier` on that target.
  `config.yaml` declares two named gateways under its top-level `gateways:` map (the older
  `binds:`/`mcp.port` shape is deprecated) — `mcp-gateway` on `:3000`, which is what the box's
  baked `/root/.claude.json` already points at, and `llm-gateway` on `:3001` for the two
  Anthropic routes — both loopback-bound. The admin UI (`:15000`) binds loopback
  container-internally (`ADMIN_ADDR`) but is NOT published in `docker-compose.yml` by default:
  every running box reaches host loopback via `host.boxlite.internal`, so publishing it would
  expose its unauthenticated `/config_dump` (which contains real credential values, since
  agentgateway expands environment variables to raw text before parsing) and `/quitquitquit`
  to every box, not just the host. It's commented out with a warning to that effect and should
  only be published temporarily for local debugging while no untrusted box is running. Loopback
  alone is not the security boundary here — a box sits inside it — publishing on loopback is
  what actually gates reachability from a box. The `/api` route carries `backendAuth` and
  attaches `$ANTHROPIC_API_KEY` host-side (its upstream is configurable via
  `AGENTGATEWAY_ANTHROPIC_UPSTREAM_HOST`, `host:port` with no scheme, defaulting to
  `api.anthropic.com:443`, for routing a non-Anthropic key such as a LiteLLM deployment to its
  actual provider), so that key is deliberately absent from `passthrough_vars` in that mode;
  the `/claude` route has no credential block at all, which is what makes it forward a
  subscription's OAuth token untouched. `llm_vars` in the `justfile` picks which credential
  vars reach the box based on what `.env` sets — the mode is data, not a flag. When
  `ANTHROPIC_BASE_URL` points at the gateway's keyed `/api` route, `ANTHROPIC_API_KEY` itself
  never reaches the box — only `GH_TOKEN`/`GITHUB_TOKEN` does, because the box runs `gh` and
  `git push` itself and no proxy can do that for it. With neither `CLAUDE_CODE_OAUTH_TOKEN` nor
  `ANTHROPIC_BASE_URL` set, `llm_vars` forwards `ANTHROPIC_API_KEY` straight into the box
  instead — the gateway isn't involved in that mode, so the guarantee doesn't apply.
- **GitHub auth.** `custom/Dockerfile` configures git's `credential.https://github.com.helper`
  to `gh auth git-credential`, so an injected `GH_TOKEN`/`GITHUB_TOKEN` authenticates both the
  `gh` CLI and `git clone`/`push` over HTTPS with no separate login step.
- **Authenticated registries (e.g. ECR).** BoxLite reads registry credentials from a config
  file, not a Docker-style credential store, so there's no `docker login` equivalent built in.
  `scripts/registry-login.py` fills that gap: piped a password on stdin (mirroring
  `docker login --username ... --password-stdin ...`), it adds or updates the `--registry
  <host>`'s `auth` entry in `registries.local.json` (gitignored — never commit live
  credentials) instead of the tracked `registries.json`, which `just up`/`up-dev` already pass
  as `--config` (see above).
  Because BoxLite caches pulled images by tag→digest and never re-hits the registry for a
  cached tag, a short-lived credential (ECR tokens last 12h) only needs to be fresh at pull
  time — re-run `just registry-login` before pulls that will actually hit the registry (first
  pull, a new tag, or after `just clean-cache`).

## Commit and PR titles

Prefix every commit message and pull request title with a
[Conventional Commits](https://www.conventionalcommits.org/) type, e.g. `feat:`, `fix:`,
`docs:`, `chore:`, `refactor:`. This is what "semantic versioning" naming means in this repo —
the prefix communicates the kind of change, it does not require bumping a version number by
hand.

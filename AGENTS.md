# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## What this repo builds

A two-layer Docker image that runs Claude Code inside a [BoxLite](https://boxliteai.com)
microVM, with an MCP config baked in pointing at a host-side
[agentgateway](https://agentgateway.dev) (`http://host.boxlite.internal:3000/mcp`). This repo
only covers the box side (building the image, booting the VM) — the agentgateway itself runs
separately on the host.

## Commands

```bash
just up-dev            # build images (base + custom, pushed to local registry), then boot the box
just up                # boot the box without rebuilding (images must already be built)
just build              # start the local registry, build base + custom images, push custom
just shell              # open a session in the running box
just down               # stop and remove the box
just registry-up/down   # manage the local docker-compose registry directly
just registry-login     # log in to an authenticated registry (e.g. ECR), see below
just install/uninstall  # symlink the claude-boxlite wrapper onto PATH (see below)
```

`up`/`up-dev` take an optional box name (default `claude-box`) and flags: `-f`/`--force`
(replace an existing box of the same name), `-c`/`--cwd` (mount host cwd onto `/workspace`),
`-v host:box` (mount an arbitrary host folder, repeatable), `-e KEY=VALUE` (inject an extra
environment variable into the box, repeatable, appended to `envflags` alongside
`passthrough_vars`), and `-- <cmd>` (override the executable launched in the box; defaults to
`claude`, e.g. `just up -- bash`). `shell` takes the same optional box name and `-- <cmd>`
override (e.g. `just shell -- bash`) to exec something other than `claude` in the running box.

`just` only looks for a justfile in the current or a parent directory, so these recipes only
work from inside the repo by default. `just install` symlinks `bin/claude-boxlite` — a
wrapper that runs `just --justfile <repo>/justfile "$@"` — onto `PATH` (default
`~/bin`, override with `just install <dir>`), so `claude-boxlite up-dev` works from
anywhere. `just` sets the working directory to the justfile's own directory when invoked with
`--justfile`, so recipes' relative paths (`registries.local.json`,
`local-development/registry/docker-compose.yml`) resolve correctly either way. `just
uninstall` removes the symlink.

There is no test suite or linter in this repo; verification is building the images and
booting a box (`just up-dev`).

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
  `set dotenv-load`): Claude auth (`CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY`, plus
  optional `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL`), GitHub
  (`GH_TOKEN`/`GITHUB_TOKEN`), and git identity (`GIT_AUTHOR_*`/`GIT_COMMITTER_*`). Add a var
  to `passthrough_vars` to make it available in the box.
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

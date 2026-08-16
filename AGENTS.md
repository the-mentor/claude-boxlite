# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## What this repo builds

A two-layer Docker image that runs Claude Code inside a [BoxLite](https://boxliteai.com)
microVM, with an MCP config baked in pointing at a host-side
[agentgateway](https://agentgateway.dev) (`http://host.boxlite.internal:15003/mcp`). This repo
covers both halves: the box side (building the image, booting the VM) and the host side
(`agentgateway/`, a docker compose service started with `just gateway-up`), which serves MCP
on `:15003` and two Anthropic routes on `:15002`.

## Commands

```bash
just up-dev            # build images (base + custom, pushed to local registry), then boot the box
just up                # boot the box without rebuilding (images must already be built)
just build              # start the local registry, build base + custom images, push custom
just build --no-cache   # same, bypassing the Docker layer cache (see below)
just exec               # open a session in the running box (alias: just shell)
just list               # list running boxes across every box name (see below), forwarding args to `boxlite list`
just down               # stop and remove the box
just gateway-up/down/logs # manage the host-side agentgateway
just gateway-generate-ui-password # change the admin UI's default credentials, see below
just registry-up/down   # manage the local docker-compose registry directly
just registry-login     # log in to an authenticated registry (e.g. ECR), see below
just install/uninstall  # symlink the cb wrapper onto PATH (see below)
```

`up`/`up-dev` take an optional box name (default `claude-box`) and flags: `-f`/`--force`
(replace an existing box of the same name), `-c`/`--cwd` (mount host cwd onto `/workspace`),
`-v host:box` (mount an arbitrary host folder, repeatable), `-e KEY=VALUE` (inject an extra
environment variable into the box, repeatable, appended to `envflags` alongside
`passthrough_vars`), `-i`/`--image` (override the image path passed to `boxlite run`; defaults
to `custom_tag`, i.e. `claude-boxlite-custom`), and `-- <cmd>` (override the executable
launched in the box; defaults to `claude`, e.g. `just up -- bash`). `exec` takes the same optional box name and `-- <cmd>`
override (e.g. `just exec -- bash`) to exec something other than `claude --continue` (its
default) in the running box.

`build`, `build-image`, and `build-base` are variadic: everything after the recipe name is
forwarded verbatim to `docker build`, and `build`/`build-image` also pass it down to the
recipes they depend on, so `just build --no-cache` rebuilds both layers cache-free (`--pull`,
`--progress=plain` and friends work the same way). `--no-cache` is the one that matters in
practice: the version-fetching `RUN` steps (`npm install -g @anthropic-ai/claude-code`, the
oh-my-posh installer, `apt upgrade`) have fixed command text, so Docker keeps replaying their
cached layers no matter how stale they get.

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

`--home` — this is what lets `just up box-a` and `just up box-b` run at the same time from
separate terminals; see `docs/design/general.md` for why BoxLite requires it. `just list` and
`just clean-cache` iterate over every `boxes/*` home to cover all box names.

`just exec <name>` only succeeds once the `just up <name>` session for that same box has
exited. Both commands open their own local BoxLite runtime and take the same per-home lock, so
running `exec` while `up` is still attached fails with `Failed to acquire runtime lock ...
Another BoxliteRuntime is already using directory`. That's expected, not a bug — exit (or
Ctrl-C) the `up` session first.

## Architecture

- **Box side.** The two-layer image, the local-registry image handoff (and the `clean-cache`
  tag→digest gotcha), environment-variable credential passthrough, GitHub auth wiring, and
  authenticated-registry (ECR) support are all documented in `docs/design/general.md` — read
  it before touching the Dockerfiles, the `justfile`'s image/registry recipes, or
  `passthrough_vars`.
- **Host-side gateway.** `agentgateway/docker-compose.yml` runs `agentgateway` itself — serving
  MCP on `:15003` (what the box's baked `/root/.claude.json` points at) and two Anthropic routes
  on `:15002` — plus a sibling `github-mcp` container with no published host port, and an admin
  UI on `:15000`. Both the credential model (which vars reach the box vs. the gateway, and
  which of the two Anthropic routes is keyed vs. passthrough) and the port model (every
  published port is reachable from any running box, not just from the host, via
  `host.boxlite.internal`) have real hazards if changed without care. Read
  `docs/design/agentgateway.md` before editing `agentgateway/config.yaml` or its
  `docker-compose.yml`.

## Commit and PR titles

Prefix every commit message and pull request title with a
[Conventional Commits](https://www.conventionalcommits.org/) type, e.g. `feat:`, `fix:`,
`docs:`, `chore:`, `refactor:`. This is what "semantic versioning" naming means in this repo —
the prefix communicates the kind of change, it does not require bumping a version number by
hand.

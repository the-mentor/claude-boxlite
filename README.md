# claude-boxlite

Build and run [Claude Code](https://github.com/anthropics/claude-code) inside a
[BoxLite](https://boxliteai.com) microVM, with an MCP config baked in that points Claude
Code at a host-side [agentgateway](https://agentgateway.dev). One `just` command builds the
image and boots the box.

This repo covers the **box side**: building the image and running the VM. Standing up the
agentgateway itself is out of scope — the baked config points at
`http://host.boxlite.internal:3000/mcp`, ready for a gateway you run separately.

## How it works

- **Two-layer image.** `base/` builds `claude-boxlite-base` (Debian + Node 20 + Claude
  Code) — slow, rebuilt rarely. `custom/` layers `claude-boxlite-custom` on top, baking
  `custom/mcp.json` in as `/workspace/.mcp.json`.
- **Image handoff via a local registry.** BoxLite does not read Docker's local image
  store, so the custom image is pushed to a local `registry:2` (managed by docker compose
  under `local-development/registry/`) and BoxLite pulls it from there. `registries.json`
  tells BoxLite to use that registry over plain HTTP.
- **Credentials.** Claude Code's OAuth token is read from a gitignored `.env` and passed
  to the box at run time — never baked into an image.

## Prerequisites

- [`docker`](https://docs.docker.com/get-docker/) with `docker compose`
- [`boxlite`](https://boxliteai.com) CLI
- [`just`](https://github.com/casey/just)

## Setup

Copy the env template and set your Claude Code OAuth token:

```bash
cp .env.example .env
# get a token with:  claude setup-token
# then edit .env and set CLAUDE_CODE_OAUTH_TOKEN=...
```

## Usage

```bash
just up-dev            # build images (base + custom, pushed to the local registry) then boot the box
just up                # boot the box without rebuilding (images must already be built)
just build             # start the local registry, build base + custom images, push custom
just shell             # open a session in the running box
just down              # stop and remove the box
```

Use `just up-dev` the first time (or after changing the image); use `just up` for a fast
boot once the images are built. Both run Claude Code interactively inside the box, so they
need a valid `CLAUDE_CODE_OAUTH_TOKEN` in `.env`. Once inside, `.mcp.json` is already
present at `/workspace/.mcp.json`, pointing Claude Code at the host gateway.

`up`, `up-dev`, `shell`, and `down` take an optional box name (default `claude-box`), so you
can run several boxes side by side. `up`/`up-dev` also accept `-f`/`--force` to replace an
existing box of the same name (without it, a name collision errors out):

```bash
just up-dev my-box     # build + boot a box named "my-box"
just up my-box -f      # re-boot it, replacing the running box
just shell my-box      # open a session in it
just down my-box       # tear it down
```

Other recipes: `just registry-up` / `just registry-down` manage the local registry
directly; `just --list` shows everything.

## Windows

`just` recipes run under `sh`. On Windows, install Git Bash and add
`set windows-shell := ["bash", "-cu"]` near the top of the `justfile`, or run under WSL.

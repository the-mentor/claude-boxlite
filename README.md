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
  `justfile`): Claude auth (`CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY`, plus optional
  `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_BASE_URL` / `ANTHROPIC_MODEL`), GitHub
  (`GH_TOKEN`/`GITHUB_TOKEN`), and git identity (`GIT_AUTHOR_*` / `GIT_COMMITTER_*`). Unset
  vars are skipped.
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
#   subscription: get a token with `claude setup-token`, set CLAUDE_CODE_OAUTH_TOKEN=...
#   API key:      set ANTHROPIC_API_KEY=... (optionally ANTHROPIC_BASE_URL=... for a gateway)
# Optional GitHub: set GH_TOKEN=... (a PAT) to enable gh + git over HTTPS
# Optional git identity: GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL
```

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
just down              # stop and remove the box
```

Use `just up-dev` the first time (or after changing the image); use `just up` for a fast
boot once the images are built. Both run Claude Code interactively inside the box, so they
need a valid `CLAUDE_CODE_OAUTH_TOKEN` in `.env`. The `agentgateway` MCP server is
configured user-scoped in `/root/.claude.json`, so Claude Code points at the host gateway
in any project — including a mounted host directory.

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
directly; `just --list` shows everything.

### Running from anywhere

`just` only finds a justfile in the current (or a parent) directory, so by default these
commands only work from inside this repo. To run them from any directory, install the
`claude-boxlite` wrapper onto your `PATH`:

```bash
just install            # symlinks bin/claude-boxlite into ~/bin (pass a dir to override)
claude-boxlite up-dev   # now works from anywhere
```

`just uninstall` removes the symlink. The wrapper just runs `just --justfile
/path/to/this/repo/justfile "$@"`, so it behaves identically to running `just` from inside
the repo, including recipes' relative paths (e.g. `registries.json`). `-c`/`--cwd` uses
`just`'s `invocation_directory()` rather than `$PWD` so it mounts the directory you actually
ran the command from, not the repo's own directory.

## Windows

`just` recipes run under `sh`. On Windows, install Git Bash and add
`set windows-shell := ["bash", "-cu"]` near the top of the `justfile`, or run under WSL.

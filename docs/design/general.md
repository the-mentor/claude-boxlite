# Box-side design

This document explains the design of the box side of this repo — the Dockerfiles, the
justfile's image and registry handling, and the credential wiring — and the reasoning behind
its non-obvious choices, for whoever next needs to change it. For day-to-day usage (commands,
flags) see `AGENTS.md`; for user-facing setup see `README.md`. This is not a change log — it
doesn't track who did what or when, only what the setup is and why it has to be that way. For
the host-side agentgateway service, see `docs/design/agentgateway.md`.

## Two-layer image

`base/Dockerfile` builds `claude-boxlite-base` (Debian + Node + Claude Code + `gh` + `uv`) and
`custom/Dockerfile` layers `claude-boxlite-custom` on top of it. The split exists because the
two layers change at very different rates: the base layer's contents — the OS, the language
runtime, the CLIs — are slow to build and rarely need to change, while the custom layer is
where per-repo configuration lives and gets rebuilt often. Keeping them separate means a config
tweak (a new plugin, a `claude.json` change) rebuilds only the fast layer.

The custom layer bakes `custom/claude.json` in as `/root/.claude.json` — the theme, the
onboarding-complete flag, and the user-scoped `agentgateway` MCP server entry — and installs
whatever plugin marketplaces and plugins are listed in its `MARKETPLACES`/`PLUGINS` build args.
Nothing is baked into `/workspace` itself, which is a deliberate choice: it means mounting a
host directory there (`-c`/`-v`) never clobbers baked-in config, because there's no config
sitting at that path to clobber.

## Image handoff through a local registry

BoxLite does not read Docker's local image store — a `docker build` alone doesn't make an
image visible to a `boxlite run`. `just build-image` bridges that gap by pushing the custom
image to a local `registry:2` (started by `local-development/registry/docker-compose.yml`),
and BoxLite pulls from there instead. BoxLite is pointed at `registries.local.json`
(gitignored) via `--config` rather than the tracked `registries.json` directly: `just
up`/`up-dev` copy the latter to the former on first run only, so it's safe to add
per-machine registry credentials to the local copy without ever touching the tracked
template.

BoxLite also caches pulled images by tag, and that cache is the trap: once `claude-boxlite-custom:latest` has been pulled once, BoxLite will keep serving the cached digest for
that tag even after a rebuild pushes a new one — there's no `boxlite rmi` to invalidate it.
`just clean-cache` (run automatically at the end of `build-image`) works around this by
deleting the cached tag→digest row for the custom image directly from BoxLite's own sqlite
index, plus sweeping any blob files (manifests/configs/layers/extracted) that no longer have a
referencing row, so the next `boxlite run` is forced to re-pull. Disk-images are deliberately
left out of that sweep: BoxLite's index doesn't record which image a given disk-image belongs
to, so an orphaned one can't be told apart from a live one without risking a costly, or
outright breaking, re-pull.

## Credentials: environment-variable passthrough, not baked images

Baking credentials into the image was never on the table — an image is meant to be rebuilt,
pushed, and pulled by anyone with registry access, so anything baked in would leak to whoever
gets the image. Instead, `cbox` (the Rust binary the `justfile`'s `up`/`exec`/`down`/`list`
recipes wrap) forwards a fixed list of plain, non-secret variables — Claude auth (a subscription
OAuth token, a keyed-gateway auth token, or a raw API key, depending on what's set; see
`docs/design/agentgateway.md`), an always-optional `ANTHROPIC_MODEL`, git identity
(`GIT_AUTHOR_*`/`GIT_COMMITTER_*`), and terminal-identity variables — plus `BOX_NAME`, set to the
box name being booted or attached to. None of it lives in the image; it's composed at box-create
time by `env::passthrough_vars()` and `env::compose()` in `cbox/src/env.rs`. Adding a new
unconditional passthrough variable is a one-line change there.

GitHub auth (`GH_TOKEN`/`GITHUB_TOKEN`) is deliberately **not** on that list — the whole point of
`cbox`'s secrets model, covered in `docs/design/cbox.md`, is that a token like this never
reaches the guest as a plain value at all, only as a `<BOXLITE_SECRET:...>` placeholder that a
host-side proxy substitutes on the wire. Read `docs/design/cbox.md` before touching
`cbox/src/secrets.rs`, `cbox/src/env.rs`, or `custom/Dockerfile`'s git configuration.

## Authenticated registries (e.g. ECR)

BoxLite reads registry credentials from its own config file (`registries.local.json`), not
from a Docker-style credential store, so there's no `docker login` equivalent built into
BoxLite itself. `scripts/registry-login.py` fills that gap: piped a password on stdin
(mirroring `docker login --username ... --password-stdin ...`), it adds or updates the
`--registry <host>`'s `auth` entry in `registries.local.json` — never the tracked
`registries.json`, so live credentials are never at risk of being committed.

Because BoxLite caches pulled images by tag and never re-hits the registry for a tag it already
has cached, a short-lived credential only needs to be fresh at the moment of an actual pull —
not continuously. ECR tokens last 12 hours, so `just registry-login` only needs to be re-run
before a pull that will really hit the registry: the first pull, a new tag, or right after
`just clean-cache` has forced a re-pull of an existing tag.

## Running multiple boxes: per-box `BOXLITE_HOME` and its lock

Each box name gets its own `BOXLITE_HOME`
(`${BOXLITE_HOME:-$HOME/.boxlite}/boxes/<name>`), passed to every `boxlite` invocation via
`--home`. This split exists because of a BoxLite behavior that isn't optional: BoxLite takes an
exclusive filesystem lock on the *entire* `BOXLITE_HOME` directory for as long as a `boxlite
run`/`exec` process is attached to it, not just a lock scoped to the one box inside it. Two
boxes sharing a home therefore can't run concurrently — the second `boxlite run` fails with
`Failed to acquire runtime lock ... Another BoxliteRuntime is already using directory`. Giving
each box name its own home sidesteps the shared lock entirely, which is what lets `just up
box-a` and `just up box-b` run at the same time from separate terminals — the underlying lock
is on the directory, not on the box name, so there's no way to make two boxes coexist inside
one home.

That same lock is also why `just shell <name>` only succeeds once the `just up <name>` session
for that box has exited: `shell` opens `boxlite exec`, which is its own local runtime process
and takes the identical per-home lock that `run` already holds. This is a limitation of
BoxLite's CLI process model — every invocation is its own runtime — not something specific to
this repo's layout, and per-box homes don't fix it, because it's a lock on the one home a given
box actually lives in.

A shared `boxlite serve` daemon would sidestep the per-process lock differently: many boxes,
one long-lived runtime, one lock held by the daemon instead of by each transient CLI
invocation. That would let `shell` and `up` for the *same* box coexist, which per-box homes
cannot do. It isn't viable yet, though — its REST API doesn't currently forward `-v`/`-c` bind
mounts into the box (boxlite-ai/boxlite#942), and this repo depends on those mounts to get the
host workspace into `/workspace`. Until that lands upstream, per-box homes are the only
available way to run more than one box at a time.

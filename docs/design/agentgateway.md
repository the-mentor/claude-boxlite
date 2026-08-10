# Host-side agentgateway

This document explains the design of `agentgateway/` and the reasoning behind its
non-obvious choices, for whoever next needs to change it. For day-to-day usage (starting it,
the admin UI, troubleshooting) see `README.md`. This is not a change log — it doesn't track
who did what or when, only what the config is and why it has to be that way.

## Shape

agentgateway (`cr.agentgateway.dev/agentgateway`, pinned at `v1.4.1`) runs as a long-lived
Docker Compose service on the host, alongside two sibling containers:

- **`mcp-gateway`** (port 15003) — serves `/mcp` and `/sse`, multiplexing MCP tool targets.
  One target is live (`github`, proxied to the sibling `github-mcp` container); three more
  ship disabled.
- **`llm-gateway`** (port 15002) — two Anthropic-Messages-API routes, `/claude` (subscription
  passthrough) and `/api` (keyed), described below.
- **`ui-gateway`** (port 15000) — the admin UI: config viewer, MCP tool playground, and (at
  `v1.4.1`) Logs, Analytics, Costs, Models, Providers, Guardrails, Keys, and Policies pages —
  behind HTTP basic auth.
- **`github-mcp`** — GitHub's official MCP server image, run as a sibling compose service
  with no published host port, reachable only from `agentgateway` over the compose network.
- **`jaeger`** (port 16686) — OTLP-gRPC trace backend for `config.tracing`; only its
  read-only trace-viewer UI is published, not the `4317` collection port `agentgateway`
  reaches it on over the compose network. See Telemetry below.

Each of the three is a separate named entry under `config.yaml`'s top-level `gateways:` map,
not one gateway with three binds — a name collision between a gateway and a top-level block
(`mcp:`, `ui:`) is why they're `mcp-gateway`/`llm-gateway`/`ui-gateway` rather than
`mcp`/`llm`/`ui`.

The box's baked MCP config (`/root/.claude.json`, built into the custom image) points at
`http://host.boxlite.internal:15003/mcp` and needs no change regardless of what else happens
here — that URL is a promise this config keeps, not a value read from it.

## The credential model

The two LLM routes exist because the two Anthropic auth styles need genuinely different
upstream handling, not because of an aesthetic preference for symmetry:

- **`/claude` (passthrough).** No `backendAuth` block at all. Claude Code's own OAuth token
  and its `anthropic-beta` header travel upstream untouched. The absence of a credential
  block *is* what makes this passthrough rather than keyed auth — there's no flag to flip.
- **`/api` (keyed).** The gateway attaches `ANTHROPIC_API_KEY` via `policies.backendAuth.key`
  on the route. The box sends a dummy `ANTHROPIC_AUTH_TOKEN` and never sees the real key.

This asymmetry is the entire point of the gateway's keyed mode, and it is enforced by exactly
one thing: the `llm_vars` conditional in `justfile` (lines 15–21), which decides which
credential-shaped variables get forwarded into the box based on what's set in `.env`. Nothing
in the schema enforces it — `config.yaml` validates cleanly whether or not `/claude` carries
a `backendAuth` block, and a future edit could add one without any validator objecting. The
guarantee that `/claude` has no credential and no host override is a property of this specific
file's contents, checked only by reading it (or by the live asymmetry test below) — there is
no schema rule that would catch a regression.

`llm_vars` picks one of three mutually exclusive sets based on what's in `.env`:

| `.env` state | Vars forwarded to the box | Box's `ANTHROPIC_BASE_URL` |
|---|---|---|
| `CLAUDE_CODE_OAUTH_TOKEN` set | `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_BASE_URL` | unset, or `…:15002/claude` |
| `CLAUDE_CODE_OAUTH_TOKEN` unset, `ANTHROPIC_BASE_URL` set | `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL` | `…:15002/api` |
| neither set | `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` | unset (direct to Anthropic) |

Read as four real-world modes: subscription-direct, subscription-through-gateway,
keyed-through-gateway, and keyed-direct. `ANTHROPIC_API_KEY` never appears in the same branch
as a gateway `ANTHROPIC_BASE_URL` — that's the credential-custody claim, and it was observed
by actually running the conditional against a real `.env` for all four rows, not assumed from
reading the code.

Be honest about what this does and doesn't cover: `GH_TOKEN`/`GITHUB_TOKEN` deliberately
*still* reach the box (they're in `passthrough_vars` unconditionally), because the box runs
`gh` and `git push` itself — no proxy can do that on its behalf. One credential left the VM
here, not all of them. The gateway also holds a copy of `GH_TOKEN` (injected into the
`github` MCP target's `Authorization` header), so the same token exists in two places by
design.

## Ports, and why loopback is not the security boundary

Every port `docker-compose.yml` publishes binds `127.0.0.1` only. That keeps them off the
LAN, but it does **not** keep them off the box: `host.boxlite.internal` resolves to the host
loopback proxy, so every running box reaches any port published here exactly as if it were
the host itself. "Loopback-only" narrows the audience to "this machine plus every box booted
from it," not to "the host alone."

This was a real, confirmed defect, not a hypothetical hardening exercise: agentgateway's
built-in admin API (port 15001) was briefly published on 127.0.0.1, and a box was
independently confirmed able to `curl` its `/config_dump` endpoint and read back real
credential values, live. That endpoint's blast radius is bigger than one endpoint:
`admin_router` merges the **entire UI router** into the admin server
(`management/admin.rs:197-198`), so `:15001` exposes not just `/config_dump`,
`/debug/pprof/*`, `/logging`, and an unauthenticated `POST /quitquitquit`, but the complete UI
API too — including `POST /api/config` (a live config **write**) and `/api/logs/*` — all with
no authentication in front of any of it. Publishing it at all, loopback or not, hands every box
a way to read secrets out of the running config, rewrite that config, and pull request logs —
this reasoning is the justification for the whole ports policy in this file, not just the
`/config_dump` credential leak that was first observed. It is never published in this repo;
`ADMIN_ADDR` still binds it inside the container for anyone who wants to reach it deliberately
(e.g. `docker exec` a curl, or uncomment the port for a debugging session with no untrusted box
running) — and setting it isn't only about deliberate reachability: `adminAddr`'s own schema
default is `localhost:15000` (`config.rs:306-310`), the same port `ui-gateway` binds
(`config.yaml:37-39`), so leaving `ADMIN_ADDR` unset wouldn't just make the admin API
unreachable from outside the container, it would also collide with the admin UI's own bind.

The admin UI (port 15000) is different: it serves the full UI — config viewer, tool
playground, and the Logs/Analytics/Costs/Models/Providers/Guardrails/Keys/Policies pages
listed under Shape above — but sits behind `basicAuth` with `mode: strict` (see below), so
it's safe to publish even though every box can reach it too — a box without the password just
gets a 401.

The lesson generalizes: before adding or uncommenting a port in `docker-compose.yml`, ask
whether the thing behind it authenticates its own requests. Binding `127.0.0.1` answers "is
this reachable from the LAN," not "is this reachable from a box."

## Constraints the config must obey

These aren't stylistic preferences; each one crash-loops or silently breaks the gateway if
violated.

| Constraint | Consequence if violated |
|---|---|
| Environment expansion is raw-text across the **entire file, including comments** — the loader substitutes every `$word`-shaped token it finds, anywhere, and aborts if any has no matching env var. | A comment that mentions a shell variable by its `$NAME` form (even in prose, even describing something unrelated) crash-loops the gateway on startup, before any port opens. Exactly three real references exist in `config.yaml` today: `$GH_TOKEN`, `$ANTHROPIC_API_KEY`, `$AGENTGATEWAY_ANTHROPIC_UPSTREAM_HOST`. Spell variable names out plainly in comments (`GH_TOKEN`, not `$GH_TOKEN`). |
| `hostOverride` takes `host:port` only — no scheme, no path. | A bare hostname with no port crash-loops the gateway; `hostOverride` replaces the authority, not the whole URL, and doesn't imply a scheme either (see `backendTLS` below). |
| `htpasswd` must be a `{file: ...}` reference, never an inline hash string. | Apache-generated htpasswd hashes contain `$`-prefixed segments (one for `apr1`, one for `bcrypt`). An inline hash hits the same raw-text expansion rule above and crash-loops the gateway trying to resolve a segment as an environment variable. |
| `basicAuth` needs `mode: strict`. | The schema's default mode is `optional`, which lets a request through with *no* credentials at all — silently defeating the point of putting auth in front of the admin UI. |

## Decisions worth recording

**Why the GitHub MCP server runs as a sibling container, not the remote endpoint or a stdio
target.** GitHub's remote MCP endpoint (`api.githubcopilot.com`) is unreachable from this
Docker host — confirmed with a bare `curl` and an unrelated container, zero agentgateway
involvement, so it's a network condition of the environment, not a config bug that retrying
the target shape would fix. A stdio target is also out: the gateway container has neither
`node` nor `uv` installed, so it can't spawn a stdio MCP server itself. Running GitHub's
official server image as a sibling compose service, reached over the compose network in
plain HTTP, sidesteps both problems — no TLS needed for an in-network hop, and the image
brings its own runtime.

**Why the sibling doesn't hold its own PAT.** The natural-seeming design — give `github-mcp`
a `GITHUB_PERSONAL_ACCESS_TOKEN` and let it authenticate to GitHub itself — doesn't work in
this server's HTTP mode: `github-mcp-server` enforces its own OAuth layer on *every* request,
including `initialize`, and ignores `GITHUB_PERSONAL_ACCESS_TOKEN` entirely when running in
HTTP mode (that env var is a stdio-mode credential only). A raw request straight to
`github-mcp` with no `Authorization` header gets a 401 with a `Www-Authenticate: Bearer`
challenge regardless of what that env var holds. So the token has to arrive as a
caller-supplied Bearer header, which means agentgateway has to inject it —
`policies.requestHeaderModifier.set` on the `github` MCP target sets `Authorization: Bearer
$GH_TOKEN`, sourced from agentgateway's own environment. This was verified live: a GitHub
tool call (`get_me`, `list_releases`) succeeded end-to-end with the sibling holding no
credential of its own — the client-supplied Bearer alone authenticates.

**Why the keyed route's upstream is configurable, and why that variable isn't
`ANTHROPIC_BASE_URL`.** `AnthropicProvider` in the pinned schema accepts only a `model` field
(`additionalProperties: false`) — there's no way to point the provider itself at a different
host. The override lives one level up, on `LocalNamedAIProvider.hostOverride` on the `/api`
backend, driven by `AGENTGATEWAY_ANTHROPIC_UPSTREAM_HOST`. That name is deliberately not
`ANTHROPIC_BASE_URL`: `ANTHROPIC_BASE_URL` is the variable that tells the *box* to send its
traffic to the gateway. Reusing it here would conflate "where the box sends requests" with
"where the gateway forwards them," and a value meant for one would silently redirect the
other — letting the box bypass the gateway entirely, or pointing the gateway at itself.

**Why `gateways:`/`routes:` rather than `binds:`/`mcp.port`.** `binds` and top-level
`mcp.port` are both documented as deprecated in the pinned v1.4.1 schema, and an older draft
of this config using them triggered the admin UI's legacy-config warning. `gateways` is a map
of listener configs (port/protocol/tls); `mcp:` and each `routes[]` entry attach to a named
gateway via their own `gateways:` field. The migration is a pure structural rename — the same
two ports (15003, 15002) are declared the same way underneath, confirmed to be a true runtime
no-op (the box's baked MCP URL kept working with zero changes, and the legacy-config warning
disappeared).

**Why the three extra MCP targets ship disabled.** `terraform`, `playwright`, and `fetch` are
all stdio servers, and — same constraint as GitHub above — this container has no `node`/`uv`
to run them directly. Enabling any of them means standing up its own sibling compose service
first (images and notes are in the comments above each), which is a per-user choice with real
image-pull cost, not something to default on.

## Telemetry

Three independent pieces sit behind the `config:` block in `agentgateway/config.yaml`
(`config.tracing`, `config.logging.database`, `config.modelCatalog`), plus one that isn't
configured here at all (`config.statsAddr`) — and the split between them is exactly what let
the admin UI's Logs/Analytics/Costs pages exist and sit visibly empty before that block was
added.

**Traces are export-only.** There is no traces page anywhere in the v1.4.1 admin UI — no
`Traces.tsx`, no trace API under `ui/src/api/` — so a trace never renders inside agentgateway's
own UI, configured or not. `config.tracing` only controls where spans are *exported*: OTLP over
gRPC (`otlpProtocol: grpc`) to `otlpEndpoint: http://jaeger:4317`, the new `jaeger` sibling
service (see Shape above), whose own UI at `127.0.0.1:16686` is where traces are actually
viewed. `randomSampling: true` is load-bearing, not decorative — Claude Code's requests carry
no incoming trace context, so without it agentgateway never starts a span on its own, and the
endpoint sits configured but silent, no error either way. The one place a trace does surface
inside agentgateway's own UI is indirect: the Logs page's rows carry `trace_id`/`span_id`
(`telemetry/log_store.rs:385-386`) as a join key into Jaeger, not a rendered trace.

**Tokens and cost are UI-visible, but only through the request-log DB, and cost additionally
needs the catalog** — two independent failure modes, not one. `config.logging.database.url`
(now `/var/lib/agentgateway/requests.db`, SQLite, on the new `agentgateway-logs` named volume)
is what the Logs/Analytics pages read tokens, duration, and (when priced) cost from at all;
without it those pages have no rows regardless of what `config.modelCatalog` says. Database
configured, catalog missing: requests log with real token counts and a blank cost column.
Catalog configured, database missing: requests get priced, but nowhere the UI can display them
— cost calculation and cost *display* are that separable. `config.modelCatalog` now points at
the tracked `agentgateway/model-costs.json` (`file: /etc/agentgateway/model-costs.json`,
mounted `:ro`), but `Catalog::resolve` is a bare exact-match lookup on the model id Claude Code
sends, with no date-suffix stripping — a model missing from that file still gets its tokens
counted, just with cost stuck null.

**Prometheus metrics are always collected and currently unreachable.**
`gen_ai_client_token_usage` and `gen_ai_client_cost` are registered unconditionally
(`telemetry/metrics.rs:205-207`) — there is no `config:` switch that turns metrics collection
off; `config.metrics` only supports `remove`/`fields.add`, for pruning or annotating series
that already exist, not gating whether they're collected in the first place. They serve on
`config.statsAddr`, which defaults to `0.0.0.0:15020`, and `docker-compose.yml` does not
publish that port, so nothing outside the container can scrape them today. If a Grafana view
is ever wanted, publishing `:15020` is the whole fix — and it's a smaller ask than it looks
next to the `:15001` lesson above: unlike the admin API, `:15020` is read-only and carries no
credentials, so it's far less dangerous to expose, though it is still one more port every
running box would be able to reach (see Ports above).

**Do not use the UI's "Refresh base costs" button.** With `modelCatalog` configured
declaratively, as it now is, that button is unnecessary — and using it anyway works against
this setup, not with it: it tries to write `base-costs.json` into the config file's parent
directory and persist `config.modelCatalog` back into `config.yaml` itself
(`ui.rs:676-695`, `BASE_COSTS_FILE` at `ui.rs:30`), but `config.yaml` is mounted `:ro`
(`./config.yaml:/config.yaml:ro` in `docker-compose.yml`), so the part of that write which
matters — persisting back into `config.yaml` — has nowhere to land. Leave the button alone
rather than relying on that failure as a safety net.

## Facts established against the schema

These were non-obvious enough, and costly enough to re-derive, that they're worth stating
plainly. All checked against the schema pinned to the `v1.4.1` image tag
(`https://raw.githubusercontent.com/agentgateway/agentgateway/v1.4.1/schema/config.json`):

- `AnthropicProvider` accepts only `model` (`additionalProperties: false`) — there is no
  `baseUrl` on the provider itself, which is why the upstream override for `/api` lives on
  `LocalNamedAIProvider.hostOverride` instead.
- `backendTLS` sits on backend policies (`LocalBackendPolicies`), a sibling of `backendAuth`
  and `ai`. `hostOverride` only replaces `host:port`; it does not imply a scheme.
  `backendTLS: {}` (empty object = TLS with default settings) is required on `/api` whenever
  the overridden upstream speaks TLS — which every real one does, default or LiteLLM-style.
  Without it, agentgateway sends plain HTTP to the override host and a TLS-only upstream's
  own frontend (nginx, etc.) answers with a bare 400. This was a real failure caught live,
  not a theoretical gap.
- `promptGuard` is available at route level (`routes[].policies.ai.promptGuard`), not only
  nested under `llm.models[]` — relevant if guardrails are ever added, since both LLM routes
  here use the explicit `routes:` form rather than the `llm:` shorthand.
- A gateway-attached `mcp:` block serves `/mcp` and `/sse` automatically; no separate route
  declaration is needed to expose it, which is why the box's baked `.../mcp` URL needed no
  change across the `binds:`→`gateways:` migration.
- `policies.ai.routes` and `policies.backendAuth` are each valid in two schema locations —
  route-level and backend-nested — because `LocalBackendPolicies` is the same type reused in
  both spots. This config puts `ai.routes` backend-nested and `backendAuth` at route level,
  matching agentgateway's own published Claude Code integration examples; both placements
  validate, so this is a style choice, not a requirement.

## Open and deferred items

State these as risks to revisit, not TODOs to feel bad about:

- **`path: /` on the `github` MCP target works only because that server version isn't
  path-selective.** `github-mcp-server` v1.8.0 answers identically on `/` and `/mcp`
  (root-mounted, confirmed by direct comparison). A future server version that tightens its
  routing could make `path: /` stop matching MCP requests with no warning from this config —
  if GitHub tool calls start failing after a `github-mcp` image bump, check this first.
- **`pathPrefix` segment-boundary semantics are undocumented.** `/claude` and `/api` don't
  overlap, so this has never mattered in practice, but the schema doesn't document how a
  prefix match handles partial-segment collisions (e.g. `/api` vs. a hypothetical
  `/apikeys`). A third route with an overlapping prefix is the scenario that would surface
  this — worth checking prefix boundaries explicitly before adding one.
- **The `hostOverride`/full-URL question for the three disabled MCP targets is unconfirmed.**
  Their commented-out form uses a single `host: http://...` string with an embedded scheme
  and path, which is *not* the form proven to work for `github` (explicit `host`/`port`/`path`
  fields). If one of these is enabled, don't assume the single-string form parses — give it
  the same explicit three-field treatment `github` uses, and validate against the schema
  before trusting it.
- **Jaeger v1 (`jaegertracing/all-in-one`) is EOL.** `1.76.0`, the version `jaeger` is pinned
  to, is its last release (EOL 2025-12-31). Migrating to Jaeger v2 (`jaegertracing/jaeger`)
  isn't a drop-in tag bump — it replaces the single `COLLECTOR_OTLP_ENABLED` env var with an
  OTel-Collector-style YAML config. Revisit this service if `1.76.0` ever stops being pullable.

## Verifying a change hasn't broken it

The one guarantee worth re-checking after any change to the LLM routes or credential wiring
is the live asymmetry: with the gateway in keyed mode, a shell inside a box should show no
`ANTHROPIC_API_KEY` in its environment at all, and a request to `/claude` with no credential
attached should fail authentication upstream while the equivalent request to `/api` completes
with no credential supplied by the caller. If `/claude` ever completes without a credential,
or `/api` ever fails when a real key is configured, something has cross-wired the two routes
— re-read the `/claude` backend block for a `backendAuth` or `hostOverride` that shouldn't be
there.

There's no automated test suite for this — verification means actually running the gateway
and probing it. A reusable manual test plan (host- and box-side checks, in order) lives
alongside this project's working notes and isn't tracked in git; treat this section as the
one property from it worth re-deriving from first principles if that plan isn't at hand.

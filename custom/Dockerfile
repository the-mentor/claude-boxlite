FROM claude-boxlite-base

RUN apt-get update \
    && apt-get install -y --no-install-recommends awscli \
    && rm -rf /var/lib/apt/lists/*

# Route git HTTPS auth for github.com through gh's credential helper, so an
# injected GH_TOKEN/GITHUB_TOKEN enables git clone/push with no login step.
RUN git config --system credential.'https://github.com'.helper '!gh auth git-credential'

# Mark first-run onboarding complete so the interactive TUI uses
# CLAUDE_CODE_OAUTH_TOKEN instead of prompting to pick a login method.
COPY claude.json /root/.claude.json

# statusLine is read from ~/.claude/settings.json, NOT ~/.claude.json — putting it
# in claude.json is silently ignored. Copied before the plugin install below, which
# merges enabledPlugins/extraKnownMarketplaces into this same file.
COPY settings.json /root/.claude/settings.json

# Marketplaces (comma-separated GitHub repos) and plugins (comma-separated
# plugin@marketplace ids) to bake in. Append to add more, or override at build:
#   docker build --build-arg PLUGINS="a@m,b@m" custom/
ARG MARKETPLACES="anthropics/claude-plugins-official,dietrichGebert/ponytail"
ARG PLUGINS="superpowers@claude-plugins-official,ponytail@ponytail"
RUN set -eu; IFS=,; \
    for m in $MARKETPLACES; do claude plugin marketplace add "$m"; done; \
    for p in $PLUGINS; do claude plugin install --scope user "$p"; done

# PreToolUse Bash guard: every Bash command is checked against host-side
# policy through the gateway's MCP endpoint before it runs. See
# docs/design/bash-guard.md.
#
# Off by default because it depends on something `just up` does not start: the
# host-side gateway. With no gateway running, the MCP server the hook names is
# not connected, and Claude Code reports a (non-blocking) hook error on every
# single Bash call. A box booted without `just gateway-up` should behave
# exactly as it did before this existed. Turn it on at build time:
#   just build --build-arg BASH_GUARD_HOOK=on
# (docker warns that base/ does not consume the arg — harmless, the justfile
# forwards build args to both layers.)
#
# Merged after the plugin install above, which rewrites this same file: last
# writer wins, so this has to run second. `jq -s '.[0] * .[1]'` is a recursive
# merge, so it adds `hooks` without disturbing statusLine or enabledPlugins.
ARG BASH_GUARD_HOOK=off
COPY bash-guard-hook.json /tmp/bash-guard-hook.json
RUN set -eu; \
    if [ "$BASH_GUARD_HOOK" = "on" ]; then \
      jq -s '.[0] * .[1]' /root/.claude/settings.json /tmp/bash-guard-hook.json \
        > /tmp/settings.merged.json; \
      mv /tmp/settings.merged.json /root/.claude/settings.json; \
      echo "bash-guard PreToolUse hook: enabled"; \
    else \
      echo "bash-guard PreToolUse hook: disabled (build with --build-arg BASH_GUARD_HOOK=on)"; \
    fi; \
    rm -f /tmp/bash-guard-hook.json

# copy oh-my-posh statusline config
COPY mojada-claude.omp.json /root/mojada-claude.omp.json

WORKDIR /workspace

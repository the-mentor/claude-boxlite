FROM claude-boxlite-base

# The base image ends as USER sandbox-user; package installs need root.
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends awscli \
    && rm -rf /var/lib/apt/lists/*
# iTerm2 integration, wired up as hooks in settings.json below.
COPY --chmod=0755 iterm2-notify.sh /usr/local/bin/iterm2-notify
USER sandbox-user

# Mark first-run onboarding complete so the interactive TUI uses
# CLAUDE_CODE_OAUTH_TOKEN instead of prompting to pick a login method.
COPY --chown=sandbox-user:sandbox-user claude.json /home/sandbox-user/.claude.json

# statusLine is read from ~/.claude/settings.json, NOT ~/.claude.json — putting it
# in claude.json is silently ignored. Copied before the plugin install below, which
# merges enabledPlugins/extraKnownMarketplaces into this same file.
COPY --chown=sandbox-user:sandbox-user settings.json /home/sandbox-user/.claude/settings.json

# Marketplaces (comma-separated GitHub repos) and plugins (comma-separated
# plugin@marketplace ids) to bake in. Append to add more, or override at build:
#   docker build --build-arg PLUGINS="a@m,b@m" custom/
ARG MARKETPLACES="anthropics/claude-plugins-official,dietrichGebert/ponytail"
ARG PLUGINS="superpowers@claude-plugins-official,ponytail@ponytail"
RUN set -eu; IFS=,; \
    for m in $MARKETPLACES; do claude plugin marketplace add "$m"; done; \
    for p in $PLUGINS; do claude plugin install --scope user "$p"; done

# copy oh-my-posh statusline config
COPY --chown=sandbox-user:sandbox-user mojada-claude.omp.json /home/sandbox-user/mojada-claude.omp.json

WORKDIR /workspace

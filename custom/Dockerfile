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

COPY mojada-claude.omp.json /root/mojada-claude.omp.json

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

WORKDIR /workspace

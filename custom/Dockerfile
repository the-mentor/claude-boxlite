FROM claude-boxlite-base

# Mark first-run onboarding complete so the interactive TUI uses
# CLAUDE_CODE_OAUTH_TOKEN instead of prompting to pick a login method.
COPY claude.json /root/.claude.json

COPY mcp.json /workspace/.mcp.json

WORKDIR /workspace

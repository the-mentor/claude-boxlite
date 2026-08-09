#!/bin/sh
# Pre-approve ANTHROPIC_API_KEY in ~/.claude.json so the "Detected a custom
# API key" interactive prompt is skipped on every fresh boot of the ephemeral box.
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    python3 - <<'EOF' 2>/dev/null || true
import json, os
key = os.environ.get('ANTHROPIC_API_KEY', '')
if key:
    suffix = key[-20:]
    cfg = '/root/.claude.json'
    with open(cfg) as f:
        c = json.load(f)
    r = c.setdefault('customApiKeyResponses', {})
    a = r.setdefault('approved', [])
    if suffix not in a:
        a.append(suffix)
    with open(cfg, 'w') as f:
        json.dump(c, f, indent=2)
EOF
fi
exec claude-real "$@"

#!/usr/bin/env python3
"""Ask the running gateway what it would decide about a Bash command.

    just guard-check 'rm -rf /'

This is the end-to-end check: it walks the exact path the box's PreToolUse
hook walks — MCP over HTTP to agentgateway on :15003, through the `bash-guard`
target, to the policy server — and prints the verdict Claude Code would act
on. Failing here means the wiring is broken, not the policy; use
`just guard-test` to check the policy on its own, with no gateway involved.

Stdlib only, so it runs on the host with no virtualenv. Speaks enough of MCP's
streamable HTTP transport to handle both reply shapes agentgateway may use
(a plain JSON body, or a one-event text/event-stream).
"""

import json
import os
import sys
import urllib.error
import urllib.request

URL = os.environ.get("BASH_GUARD_GATEWAY_URL", "http://127.0.0.1:15003/mcp")
TOOL = os.environ.get("BASH_GUARD_TOOL", "check_bash_command")
PROTOCOL_VERSION = "2025-06-18"


class GatewayError(RuntimeError):
    pass


def post(message, session_id=None):
    """One JSON-RPC round trip. Returns (parsed_response_or_None, session_id)."""
    request = urllib.request.Request(
        URL,
        data=json.dumps(message).encode(),
        headers={
            "Content-Type": "application/json",
            # Both are required by the streamable HTTP transport: the server
            # picks which one it answers with.
            "Accept": "application/json, text/event-stream",
            **({"Mcp-Session-Id": session_id} if session_id else {}),
            **({"MCP-Protocol-Version": PROTOCOL_VERSION} if session_id else {}),
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            body = response.read().decode("utf-8", "replace")
            returned_session = response.headers.get("Mcp-Session-Id") or session_id
            content_type = response.headers.get("Content-Type", "")
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", "replace")[:500]
        raise GatewayError(f"HTTP {exc.code} from {URL}: {detail}") from exc
    except urllib.error.URLError as exc:
        raise GatewayError(
            f"cannot reach {URL} ({exc.reason}). Is the gateway up? `just gateway-up`"
        ) from exc

    if not body.strip():
        return None, returned_session

    if "text/event-stream" in content_type:
        # Pull the first `data:` payload out of the SSE frame.
        for line in body.splitlines():
            if line.startswith("data:"):
                return json.loads(line[5:].strip()), returned_session
        raise GatewayError(f"no data frame in event stream: {body[:300]}")

    return json.loads(body), returned_session


def check(command):
    response, session = post(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "guard-check", "version": "1.0.0"},
            },
        }
    )
    if not response or "result" not in response:
        raise GatewayError(f"initialize failed: {response}")

    post({"jsonrpc": "2.0", "method": "notifications/initialized"}, session)

    listed, _ = post({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}, session)
    names = [tool["name"] for tool in (listed or {}).get("result", {}).get("tools", [])]
    if TOOL not in names:
        raise GatewayError(
            f"the gateway does not expose {TOOL}. Tools it does expose: "
            f"{', '.join(names) or '(none)'}. Check the bash-guard target in "
            "agentgateway/config.yaml and that bash-guard-mcp is running."
        )

    called, _ = post(
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": TOOL,
                "arguments": {"command": command, "cwd": os.getcwd(), "session_id": "guard-check"},
            },
        },
        session,
    )
    if not called or "result" not in called:
        raise GatewayError(f"tools/call failed: {called}")
    result = called["result"]
    if result.get("isError"):
        raise GatewayError(f"the guard tool returned an error: {result}")
    return result["content"][0]["text"]


def main():
    # Two ways in. Arguments are the convenient form when calling this script
    # directly; stdin is what `just guard-check` uses, because just
    # interpolates recipe arguments as raw text into a shell line and the
    # strings this tool exists to examine are exactly the ones that would
    # execute on the host if they slipped out of their quotes.
    if len(sys.argv) > 1:
        command = " ".join(sys.argv[1:])
    elif not sys.stdin.isatty():
        command = sys.stdin.read().strip("\n")
    else:
        print(
            "usage: echo '<bash command>' | just guard-check\n"
            "   or: python3 agentgateway/bash-guard/ask-gateway.py '<bash command>'",
            file=sys.stderr,
        )
        return 2

    if not command.strip():
        print("guard-check: no command given", file=sys.stderr)
        return 2
    try:
        text = check(command)
    except GatewayError as exc:
        print(f"guard-check: {exc}", file=sys.stderr)
        return 1

    print(f"command:  {command}")
    if not text.lstrip().startswith("{"):
        # Plain text is the server's "no opinion" reply; Claude Code would let
        # its normal permission flow decide.
        print("decision: undecided (normal permission flow applies)")
        print(f"detail:   {text}")
        return 0

    output = json.loads(text)["hookSpecificOutput"]
    print(f"decision: {output['permissionDecision']}")
    print(f"reason:   {output['permissionDecisionReason']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

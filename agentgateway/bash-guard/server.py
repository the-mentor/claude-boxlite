#!/usr/bin/env python3
"""Bash-guard MCP server: the policy engine behind the box's PreToolUse hook.

Exposes one MCP tool, `check_bash_command`, over streamable HTTP. The box's
Claude Code calls it through agentgateway (never directly — this server
publishes no host port) via an `mcp_tool` PreToolUse hook, and Claude Code
reads the tool's text output as the hook's decision.

Return contract, which is Claude Code's hook-output contract verbatim:

  - A verdict of allow/ask/deny returns a JSON object whose
    `hookSpecificOutput.permissionDecision` Claude Code honours directly.
  - A verdict of `undecided` returns PLAIN TEXT, deliberately not starting
    with `{`. Claude Code parses stdout as JSON only when the first
    non-whitespace character is `{`, so plain text reads as "this hook has no
    opinion" and the normal permission flow (settings allowlist, then the
    user prompt) applies unchanged. Returning `allow` instead would
    auto-approve every unmatched command, which is strictly weaker than
    stock Claude Code.

Stdlib only, on purpose: this runs as a bind-mounted script inside a stock
`python:3.13-slim` image, so it needs no image build and no registry push.
See docs/design/bash-guard.md.
"""

import json
import os
import re
import sys
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Protocol versions this server will echo back on initialize. A client asking
# for something outside this set gets PREFERRED instead, which is what the MCP
# spec prescribes for version negotiation.
SUPPORTED_PROTOCOL_VERSIONS = ("2025-06-18", "2025-03-26", "2024-11-05")
PREFERRED_PROTOCOL_VERSION = "2025-06-18"

SERVER_NAME = "bash-guard"
SERVER_VERSION = "1.0.0"
TOOL_NAME = "check_bash_command"

PORT = int(os.environ.get("BASH_GUARD_PORT", "9091"))
RULES_PATH = os.environ.get("BASH_GUARD_RULES", "/etc/bash-guard/rules.json")

DECISIONS = ("allow", "ask", "deny", "undecided")

# Used only when the rules file is unreadable or invalid at startup. Refusing
# to start would be worse than useless: an unreachable guard makes Claude
# Code's mcp_tool hook fail open silently, so a broken rules file would
# disable the guard rather than tighten it. Serving a minimal deny list keeps
# the loudest cases covered while the real file is fixed.
FALLBACK_RULES = {
    "version": 1,
    "default": {"decision": "undecided"},
    "rules": [
        {
            "id": "fallback-rm-root-target",
            "decision": "deny",
            "pattern": r"""\brm\b[^;&|]*\s['"]?(/|/\*|~|~/|~/\*|\$HOME|\$HOME/\*|/workspace|/workspace/\*)['"]?\s*(;|&|\||$)""",
            "reason": "bash-guard is running on its built-in fallback rules "
            "(the rules file failed to load) and blocks deleting a filesystem "
            "root, the home directory, or the whole /workspace mount.",
        }
    ],
}


def log(message):
    """Audit line on stderr, which is where `just gateway-logs` reads from."""
    print(f"[bash-guard] {message}", file=sys.stderr, flush=True)


class Rules:
    """The rule set, reloaded from disk whenever the file's mtime changes.

    Reloading in place matters because rules.json is bind-mounted from the
    repo: editing a rule on the host takes effect on the next tool call, with
    no container restart and no dropped MCP session.
    """

    def __init__(self, path):
        self._path = path
        self._lock = threading.Lock()
        self._mtime = None
        self._compiled = []
        self._default = {"decision": "undecided", "reason": ""}
        self._source = "none"
        if not self._reload(force=True):
            self._install(FALLBACK_RULES, source="built-in fallback")

    # -- loading ---------------------------------------------------------

    def _reload(self, force=False):
        """Load the file if it changed. Returns True when rules are in place."""
        try:
            mtime = os.stat(self._path).st_mtime
        except OSError as exc:
            if force:
                log(f"cannot stat rules file {self._path}: {exc}")
            return False
        if not force and mtime == self._mtime:
            return True
        try:
            with open(self._path, encoding="utf-8") as handle:
                raw = json.load(handle)
            self._install(raw, source=self._path)
            self._mtime = mtime
            return True
        except (OSError, ValueError) as exc:
            # Keep serving the last known-good rules rather than dropping to
            # "no opinion" the moment someone saves a typo.
            log(f"rules file {self._path} failed to load ({exc}); keeping previous rules")
            self._mtime = mtime
            return bool(self._compiled) or self._default["decision"] != "undecided"

    def _install(self, raw, source):
        if not isinstance(raw, dict):
            raise ValueError("rules file must be a JSON object")
        default = raw.get("default") or {}
        if not isinstance(default, dict):
            raise ValueError("`default` must be an object")
        default_decision = default.get("decision", "undecided")
        if default_decision not in DECISIONS:
            raise ValueError(f"`default.decision` must be one of {DECISIONS}")

        compiled = []
        for index, rule in enumerate(raw.get("rules") or []):
            if not isinstance(rule, dict):
                raise ValueError(f"rule {index} must be an object")
            decision = rule.get("decision")
            if decision not in DECISIONS:
                raise ValueError(f"rule {index} has invalid decision {decision!r}")
            pattern = rule.get("pattern")
            if not isinstance(pattern, str) or not pattern:
                raise ValueError(f"rule {index} needs a non-empty `pattern`")
            flags = re.IGNORECASE if rule.get("ignoreCase") else 0
            compiled.append(
                {
                    "id": rule.get("id") or f"rule-{index}",
                    "decision": decision,
                    "reason": rule.get("reason") or "",
                    "regex": re.compile(pattern, flags),
                }
            )

        with self._lock:
            self._compiled = compiled
            self._default = {
                "decision": default_decision,
                "reason": default.get("reason") or "",
            }
            self._source = source
        log(f"loaded {len(compiled)} rules from {source} (default: {default_decision})")

    # -- evaluation ------------------------------------------------------

    def evaluate(self, command):
        """First matching rule wins; falls back to the configured default."""
        self._reload()
        with self._lock:
            compiled = self._compiled
            default = dict(self._default)
        for rule in compiled:
            if rule["regex"].search(command):
                return {
                    "decision": rule["decision"],
                    "reason": rule["reason"],
                    "rule": rule["id"],
                }
        return {"decision": default["decision"], "reason": default["reason"], "rule": None}


RULES = Rules(RULES_PATH)


def truncate(text, limit=400):
    text = " ".join(text.split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def check_bash_command(arguments):
    """Evaluate one Bash command and render Claude Code's hook output."""
    command = arguments.get("command")
    if not isinstance(command, str) or not command.strip():
        # No command to judge. Stay silent rather than block: a malformed hook
        # payload is a wiring bug, not a policy violation.
        return "bash-guard: no command supplied, no policy decision"

    verdict = RULES.evaluate(command)
    decision = verdict["decision"]
    rule_id = verdict["rule"] or "default"
    session = arguments.get("session_id") or "-"
    cwd = arguments.get("cwd") or "-"
    log(f"{decision} [{rule_id}] session={session} cwd={cwd} command={truncate(command, 200)}")

    if decision == "undecided":
        # Plain text on purpose — see the module docstring.
        return f"bash-guard: no matching policy rule ({rule_id}), deferring to normal permissions"

    reason = verdict["reason"] or f"bash-guard rule {rule_id} returned {decision}."
    return json.dumps(
        {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": f"{reason} (bash-guard rule: {rule_id})",
            }
        }
    )


TOOL_DEFINITION = {
    "name": TOOL_NAME,
    "title": "Check a Bash command against gateway policy",
    "description": (
        "Policy check for a Bash command, wired to Claude Code's PreToolUse hook. "
        "Returns the hook's decision: JSON carrying an allow/ask/deny "
        "permissionDecision when a policy rule matches, or plain text when no rule "
        "matches and the normal permission flow should apply. Intended for the hook, "
        "not for direct use."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The Bash command about to run.",
            },
            "cwd": {
                "type": "string",
                "description": "Working directory the command would run in.",
            },
            "session_id": {
                "type": "string",
                "description": "Claude Code session id, recorded in the audit log.",
            },
        },
        "required": ["command"],
    },
}


def handle_message(message):
    """Dispatch one JSON-RPC message. Returns None for notifications."""
    if not isinstance(message, dict):
        return error_response(None, -32600, "Invalid Request")

    method = message.get("method")
    message_id = message.get("id")
    params = message.get("params") or {}

    # No id means a notification: process for effect, answer nothing.
    if message_id is None:
        return None

    if method == "initialize":
        requested = params.get("protocolVersion")
        version = (
            requested
            if requested in SUPPORTED_PROTOCOL_VERSIONS
            else PREFERRED_PROTOCOL_VERSION
        )
        return result_response(
            message_id,
            {
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            },
        )

    if method == "ping":
        return result_response(message_id, {})

    if method == "tools/list":
        return result_response(message_id, {"tools": [TOOL_DEFINITION]})

    if method in ("resources/list", "prompts/list"):
        # agentgateway probes these while multiplexing targets; an empty list
        # is friendlier than a method-not-found error in its logs.
        key = "resources" if method.startswith("resources") else "prompts"
        return result_response(message_id, {key: []})

    if method == "tools/call":
        name = params.get("name")
        if name != TOOL_NAME:
            return error_response(message_id, -32602, f"Unknown tool: {name}")
        arguments = params.get("arguments") or {}
        if not isinstance(arguments, dict):
            return error_response(message_id, -32602, "`arguments` must be an object")
        try:
            text = check_bash_command(arguments)
        except Exception as exc:  # noqa: BLE001 - never take the server down
            log(f"tool call failed: {exc!r}")
            # isError makes Claude Code treat the hook as a non-blocking error,
            # so the command proceeds under normal permissions.
            return result_response(
                message_id,
                {
                    "content": [{"type": "text", "text": f"bash-guard error: {exc}"}],
                    "isError": True,
                },
            )
        return result_response(
            message_id,
            {"content": [{"type": "text", "text": text}], "isError": False},
        )

    return error_response(message_id, -32601, f"Method not found: {method}")


def result_response(message_id, result):
    return {"jsonrpc": "2.0", "id": message_id, "result": result}


def error_response(message_id, code, message):
    return {"jsonrpc": "2.0", "id": message_id, "error": {"code": code, "message": message}}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = f"{SERVER_NAME}/{SERVER_VERSION}"

    def log_message(self, fmt, *args):  # noqa: A003 - BaseHTTPRequestHandler API
        """Silence per-request access logs; decisions are logged instead."""

    # -- helpers ---------------------------------------------------------

    def _send(self, status, body=None, extra_headers=None):
        payload = b"" if body is None else json.dumps(body).encode("utf-8")
        self.send_response(status)
        if payload:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        for key, value in (extra_headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        if payload:
            self.wfile.write(payload)

    def _is_mcp_path(self):
        return self.path.split("?", 1)[0].rstrip("/") in ("", "/mcp")

    # -- verbs -----------------------------------------------------------

    def do_GET(self):
        # Health probe for compose; anything else is the SSE upgrade this
        # server does not offer, and 405 is the spec's way to say so.
        if self.path.split("?", 1)[0].rstrip("/") == "/healthz":
            self._send(200, {"status": "ok", "rules": RULES_PATH})
            return
        self._send(405, {"error": "This server does not offer a server-initiated stream"})

    def do_DELETE(self):
        # Session teardown. Sessions carry no state here, so acknowledge.
        self._send(204)

    def do_POST(self):
        if not self._is_mcp_path():
            self._send(404, {"error": f"Unknown path: {self.path}"})
            return

        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        raw = self.rfile.read(length) if length else b""

        try:
            message = json.loads(raw.decode("utf-8")) if raw else None
        except (ValueError, UnicodeDecodeError):
            self._send(400, error_response(None, -32700, "Parse error"))
            return

        headers = {}
        if isinstance(message, dict) and message.get("method") == "initialize":
            headers["Mcp-Session-Id"] = uuid.uuid4().hex

        # Tolerate JSON-RPC batches even though MCP dropped them: an older
        # client batching initialize+notification should still work.
        if isinstance(message, list):
            responses = [r for r in (handle_message(m) for m in message) if r is not None]
            if not responses:
                self._send(202)
                return
            self._send(200, responses, headers)
            return

        response = handle_message(message)
        if response is None:
            # Notification: 202 with no body, as streamable HTTP requires.
            self._send(202)
            return
        self._send(200, response, headers)


def main():
    log(f"listening on 0.0.0.0:{PORT} (MCP at /mcp), rules from {RULES_PATH}")
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()


if __name__ == "__main__":
    main()

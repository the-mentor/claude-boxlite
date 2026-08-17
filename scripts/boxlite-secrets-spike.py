#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10,<3.14"
# dependencies = ["boxlite[sync]==0.9.7"]
# ///
"""Validate BoxLite's host-side secret injection against this repo's custom image.

BoxLite's `Secret` substitutes a real credential into outbound HTTPS requests at
the host boundary, so the value never enters the guest. That would let the box
run `gh`/`git push` and reach the Anthropic API without ever holding a token —
retiring the GH_TOKEN passthrough documented in docs/design/general.md and the
gateway's role as the credential boundary (docs/design/agentgateway.md).

This script answers the three questions that decide whether that is viable,
before any SDK port is attempted:

  1. Does substitution actually happen?    (a placeholder-only box authenticates)
  2. Does the guest need BoxLite's CA?     (TLS failures vs. clean 200s)
  3. Does the real value stay on the host? (the guest env holds only a placeholder)

Run it from the repo root with whichever credentials you want to exercise:

    GH_TOKEN=ghp_... ANTHROPIC_API_KEY=sk-ant-... scripts/boxlite-secrets-spike.py

Checks whose credential is unset are skipped, not failed. Nothing here writes to
the repo, and the box is removed on exit unless --keep is passed.

With --interactive the automated checks run first and then the terminal is handed
to the box, so the parts no scripted probe covers can be driven by hand — Claude
Code's own Node TLS stack, the interactive TUI, `git push`:

    ANTHROPIC_API_KEY=sk-ant-... scripts/boxlite-secrets-spike.py --interactive
    scripts/boxlite-secrets-spike.py --interactive -- bash

The session runs inside this process, on a `box.exec(..., tty=True)`, because that
is the only place it can run: the substituting proxy is host-side and belongs to
the runtime that created the box with `secrets=`. Handing the terminal to
`boxlite exec` would not work — it opens its own runtime, and a CLI runtime is
configured from --config, which carries only `image_registries`. The CLI cannot
express secrets in a runtime at all, so the session would send the literal
placeholder and collect a 401.

That attach is written against three shapes this repo has not observed (whether
`stdout()` under tty=True yields raw chunks or newline-stripped lines, whether
`stdin` is an attribute or a method, and `resize_tty`'s argument order). boxlite
ships macOS-arm64 wheels only, so they cannot be introspected from a Linux
checkout. Run --probe-api on a host where the package installs to replace the
guesses with facts; --self-check covers only the logic that needs no box.

Dependencies are declared inline (PEP 723) and resolved by `uv run`, which the
shebang invokes — there is no environment to create and nothing to install. The
boxlite pin matches the CLI version this repo's claims were verified against, so
bump both together or the spike stops testing what you actually run.

Needs `uv` on the host (the base image already installs it, but this runs
host-side) and the custom image already built and pushed (`just build`). Running
it under a bare `python3` also works if `boxlite` is importable there.
"""
import argparse
import json
import os
import sys
from pathlib import Path

try:
    # SyncBoxlite, not Boxlite: the latter is the native async runtime, whose
    # create()/exec() return coroutines. The sync wrapper needs the [sync] extra
    # (greenlet) — without it the import below silently lacks SyncBoxlite.
    from boxlite import BoxOptions, ImageRegistry, Options, Secret, SyncBoxlite
except ImportError:  # deferred to main() so --help works without the dependency
    BoxOptions = ImageRegistry = Options = Secret = SyncBoxlite = None

REPO_ROOT = Path(__file__).resolve().parent.parent

# The guest-side token BoxLite swaps for the real value at egress. Shape is set
# by the SDK ("<BOXLITE_SECRET:name>"), not by us — see boxlite.Secret's docstring.
PLACEHOLDER = "<BOXLITE_SECRET:{name}>"


def placeholder(name):
    return PLACEHOLDER.format(name=name)


def load_registries(config_path):
    """Map this repo's BoxLite --config JSON onto ImageRegistry objects.

    The SDK takes registries as constructor arguments rather than a file path,
    so the same registries.local.json the justfile passes to `boxlite run` has
    to be translated here. Auth is flattened: the file nests it under "auth",
    ImageRegistry takes username/password directly.
    """
    try:
        raw = Path(config_path).read_text().strip()
    except FileNotFoundError:
        return []
    entries = json.loads(raw).get("image_registries", []) if raw else []

    registries = []
    for entry in entries:
        auth = entry.get("auth") or {}
        registries.append(
            ImageRegistry(
                host=entry["host"],
                transport=entry.get("transport", "https"),
                skip_verify=entry.get("skip_verify", False),
                search=entry.get("search", False),
                username=auth.get("username"),
                password=auth.get("password"),
            )
        )
    return registries


def drain(stream):
    """Collect a SyncExecStdout/SyncExecStderr line iterator into one string.

    These are methods returning iterators of already-decoded, newline-stripped
    lines (SyncExecution.stdout()/.stderr()), and they return None when the
    stream is unavailable.
    """
    return "\n".join(line.rstrip("\n") for line in stream) if stream is not None else ""


def run(box, script):
    """Run a shell snippet in the box; return (exit_code, stdout, stderr).

    exit_code is None when the runtime does not surface one — callers key off
    stdout instead, which every check below is written to make sufficient.

    Both streams are drained before wait(): the iterators are fed by the live
    execution, so waiting first can leave output unread.
    """
    execution = box.exec("sh", ["-lc", script])
    out, err = drain(execution.stdout()), drain(execution.stderr())
    code = None
    try:
        result = execution.wait()
        code = getattr(result, "exit_code", result if isinstance(result, int) else None)
        message = getattr(result, "error_message", None)
        if message:
            err = f"{err}\n{message}".strip()
    except Exception as exc:  # noqa: BLE001 - a wait() failure is a finding, not a crash
        err = f"{err}\nwait() raised: {exc}".strip()
    return code, out.strip(), err.strip()


# curl exit codes worth naming: anything in the TLS family means the guest does
# not trust BoxLite's MITM CA, which is a different verdict from a rejected
# credential and points at a fixable base-image change rather than a dead end.
CURL_TLS_ERRORS = {35: "TLS handshake", 51: "cert/host mismatch", 60: "CA not trusted", 77: "CA bundle unreadable"}


def probe_http(box, label, url, header, note=""):
    """Issue one authenticated request from inside the box and classify it.

    Prints curl's exit code and HTTP status separately so a TLS failure (needs a
    CA) is never mistaken for a 401 (substitution did not happen).
    """
    script = (
        f'curl -sS -o /dev/null -w "%{{http_code}}" '
        f'-H {header!r} {url!r} 2>/tmp/curl.err; '
        f'echo " rc=$?"; cat /tmp/curl.err'
    )
    _, out, err = run(box, script)
    # curl's %{http_code} and the echoed rc can land in separate stream lines, so
    # collapse whitespace before splitting them apart.
    status, _, rest = " ".join(out.split()).partition("rc=")
    status = status.strip()
    rc = rest.split()[0] if rest.strip() else "?"

    if rc in {str(k) for k in CURL_TLS_ERRORS}:
        return report(label, "FAIL", f"TLS error {rc} ({CURL_TLS_ERRORS[int(rc)]}) — guest does not trust the MITM CA")
    if status.startswith("2"):
        return report(label, "PASS", f"HTTP {status} — substitution happened{note}")
    if status in {"401", "403"}:
        return report(label, "FAIL", f"HTTP {status} — placeholder reached the server unsubstituted")
    return report(label, "FAIL", f"HTTP {status or '?'} rc={rc} {err[:160]}")


RESULTS = []


def report(label, verdict, detail):
    RESULTS.append((label, verdict, detail))
    icon = {"PASS": "PASS", "FAIL": "FAIL", "SKIP": "SKIP"}[verdict]
    print(f"  [{icon}] {label}: {detail}", flush=True)
    return verdict == "PASS"


def build_parser():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--image", default="claude-boxlite-custom", help="image to boot (default: the custom layer)")
    parser.add_argument("--home", default=os.environ.get("BOXLITE_HOME"), help="BOXLITE_HOME for this spike")
    parser.add_argument(
        "--config",
        default=str(REPO_ROOT / "registries.local.json"),
        help="BoxLite registry config (default: this repo's registries.local.json)",
    )
    parser.add_argument("--git-remote", help="optional private HTTPS repo URL to exercise `git ls-remote`")
    parser.add_argument("--keep", action="store_true", help="leave the box running for manual poking")
    parser.add_argument(
        "--interactive",
        action="store_true",
        help="after the checks, attach this terminal to the box and drive it by hand",
    )
    parser.add_argument(
        "cmd",
        nargs="*",
        help="with --interactive, what to run in the box (default: claude); prefix with --",
    )
    parser.add_argument(
        "--self-check", action="store_true", help="assert the offline logic and exit; boots nothing"
    )
    parser.add_argument(
        "--probe-api",
        action="store_true",
        help="dump the SDK's real exec/TTY signatures and exit; boots nothing",
    )
    return parser


def main():
    args = build_parser().parse_args()

    if args.self_check:
        return self_check()

    if Secret is None:
        sys.exit(
            "boxlite-secrets-spike: boxlite is not importable. Run this script directly "
            "(./scripts/boxlite-secrets-spike.py) so uv resolves the inline dependency, "
            "or `uv run scripts/boxlite-secrets-spike.py`."
        )

    if args.probe_api:
        return probe_api()

    gh_token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    anthropic_key = os.environ.get("ANTHROPIC_API_KEY")
    if not gh_token and not anthropic_key:
        sys.exit("boxlite-secrets-spike: set GH_TOKEN and/or ANTHROPIC_API_KEY — nothing to test otherwise")

    # Each secret is scoped to the hosts it may be substituted for. A host absent
    # from this list must never receive the real value; that scoping is the whole
    # security property, so keep these lists as narrow as the check requires.
    # BoxOptions.env takes a list of (key, value) tuples, not a dict.
    secrets, env = [], []
    if gh_token:
        secrets.append(Secret(name="gh", value=gh_token, hosts=["github.com", "api.github.com"]))
        env.append(("GH_TOKEN", placeholder("gh")))
    if anthropic_key:
        secrets.append(Secret(name="anthropic", value=anthropic_key, hosts=["api.anthropic.com"]))
        env.append(("ANTHROPIC_API_KEY", placeholder("anthropic")))
    env_names = {name for name, _ in env}

    print(f"boxlite-secrets-spike: booting {args.image} with {len(secrets)} secret(s)\n", flush=True)

    # init_default() only registers the options for the global runtime; it returns
    # None. The runtime itself comes from default(), and its dispatcher fiber has
    # to be running before create()/exec(), which is what the context manager does.
    SyncBoxlite.init_default(
        Options(home_dir=args.home, image_registries=load_registries(args.config))
    )
    with SyncBoxlite.default() as runtime:
        box = None
        try:
            box = runtime.create(
                BoxOptions(
                    image=args.image,
                    env=env,
                    secrets=secrets,
                    auto_remove=not args.keep,
                    # The image sets no ENTRYPOINT/CMD, so it inherits node:26's
                    # `node`, which exits immediately without a TTY and takes the
                    # box down with it. Every check here runs via exec, so the main
                    # process just has to stay alive.
                    cmd=["sleep", "infinity"],
                ),
                name="boxlite-secrets-spike",
            )
            box.start()

            # 1. Custody: whatever the guest can read must be the placeholder. This is
            #    the check that would catch the feature silently degrading to plain
            #    env-var passthrough, which would look identical from every probe below.
            for var, name in (("GH_TOKEN", "gh"), ("ANTHROPIC_API_KEY", "anthropic")):
                if var not in env_names:
                    continue
                _, out, _ = run(box, f'printenv {var} || true')
                real = gh_token if name == "gh" else anthropic_key
                if out == placeholder(name):
                    report(f"custody/{var}", "PASS", "guest sees only the placeholder")
                elif real and real in out:
                    report(f"custody/{var}", "FAIL", "REAL VALUE PRESENT IN GUEST — no custody gain")
                else:
                    report(f"custody/{var}", "FAIL", f"unexpected value {out[:40]!r}")

            # 2. Substitution, one header style per credential. Claude Code sends
            #    x-api-key; gh and git send Authorization — both must work or the
            #    feature only covers half this repo's traffic.
            if gh_token:
                probe_http(box, "github/authorization-header", "https://api.github.com/user",
                           f"Authorization: Bearer {placeholder('gh')}")
            if anthropic_key:
                probe_http(box, "anthropic/x-api-key-header", "https://api.anthropic.com/v1/models",
                           f"x-api-key: {placeholder('anthropic')}", note=" (no tokens billed)")

            # 3. The real clients, not just curl. gh and git each build their own TLS
            #    stack and their own header, so a curl PASS does not imply these pass.
            if gh_token:
                _, out, err = run(box, "gh api user --jq .login 2>&1 || true")
                report("github/gh-cli", "PASS" if out and "error" not in out.lower() else "FAIL",
                       out[:120] if out else err[:120] or "no output")

                if args.git_remote:
                    _, out, err = run(box, f"git ls-remote {args.git_remote!r} HEAD 2>&1 | head -1 || true")
                    ok = out and "fatal" not in out.lower() and "denied" not in out.lower()
                    report("github/git-ls-remote", "PASS" if ok else "FAIL", (out or err)[:120])
                else:
                    report("github/git-ls-remote", "SKIP", "pass --git-remote <private-https-url> to exercise")

            summarise()
            if args.interactive:
                # Inside the runtime block on purpose: the substituting proxy belongs
                # to this runtime, so the session has to live here too.
                attach(box, args.cmd or ["claude"])

        finally:
            if box is not None and not args.keep:
                try:
                    runtime.remove(box.id, force=True)
                except Exception as exc:  # noqa: BLE001 - cleanup failure must not mask results
                    print(f"\nboxlite-secrets-spike: cleanup failed ({exc}); remove the box by hand", file=sys.stderr)

    return 1 if [v for _, v, _ in RESULTS if v == "FAIL"] else 0


def summarise():
    failed = [label for label, verdict, _ in RESULTS if verdict == "FAIL"]
    print(f"\nboxlite-secrets-spike: {len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed", flush=True)
    if failed:
        print("failing: " + ", ".join(failed), file=sys.stderr)


def self_check():
    """Offline assertions for the logic that needs no box. Boots nothing.

    The interactive path itself cannot be checked here — it needs KVM, the built
    image, and real credentials — so this covers the argument plumbing around it.
    """
    parser = build_parser()
    args = parser.parse_args(["--interactive", "--", "claude", "--continue"])
    assert args.interactive and args.cmd == ["claude", "--continue"], args
    assert (parser.parse_args(["--interactive"]).cmd or ["claude"]) == ["claude"]
    assert parser.parse_args([]).interactive is False

    # The status/rc split, against every stream shape the live run produced.
    for out, want_status, want_rc in (("200 rc=0", "200", "0"), ("200\n rc=0", "200", "0"), ("\nrc=60", "", "60")):
        status, _, rest = " ".join(out.split()).partition("rc=")
        assert status.strip() == want_status and rest.split()[0] == want_rc, out

    assert drain(None) == "" and drain(["a", "b\n"]) == "a\nb"
    print("boxlite-secrets-spike: self-check OK")
    return 0


def probe_api():
    """Print the real signatures of the interactive surface, then exit.

    The interactive attach below is written against three shapes this repo has not
    observed: whether `stdout()` under tty=True yields raw chunks or the same
    newline-stripped lines the non-TTY path gives, whether `stdin` is an attribute
    or a method, and what argument order `resize_tty` takes. boxlite ships
    macOS-arm64 wheels only, so none of it is introspectable from a Linux checkout.
    Run this on a host where the package installs and the guesses become facts.
    """
    import inspect

    for name in ("SyncBox", "SyncExecution", "Execution", "BoxOptions"):
        obj = getattr(__import__("boxlite"), name, None)
        print(f"\n=== {name}: {obj!r}")
        for member in [m for m in dir(obj or ()) if not m.startswith("_")]:
            try:
                sig = str(inspect.signature(getattr(obj, member)))
            except (TypeError, ValueError):
                sig = "  (not callable)"
            print(f"    {member}{sig}")
    return 0


def attach(box, cmd):
    """Attach the local terminal to a TTY exec inside the box, in this process.

    In-process is not a preference, it is the only thing that can work. The
    substituting proxy is host-side and belongs to the runtime that created the box
    with `secrets=`. `boxlite exec` opens its own runtime, and a CLI runtime is
    configured from --config, whose struct carries only `image_registries` — so the
    CLI cannot express secrets in a runtime at all. Handing the terminal to it would
    send the literal placeholder and collect a 401.

    Unverified: see probe_api(). If the TUI renders as garbled full lines, then
    stdout() is line-oriented even under tty=True and the port needs the async API.
    That outcome is a finding, not a bug in this function.
    """
    import signal
    import termios
    import threading
    import tty as ttylib

    if not sys.stdin.isatty():
        sys.exit("boxlite-secrets-spike: --interactive needs a TTY on stdin")

    print(
        "\nboxlite-secrets-spike: attaching. What this is here to settle:\n"
        "  - `gh api user` works                 -> substitution reaches an exec'd process, not just the box's main one\n"
        "  - claude reaches the API              -> Node trusts the MITM CA; no NODE_EXTRA_CA_CERTS needed\n"
        "  - claude fails on certificates        -> base/Dockerfile must install the CA and point Node at it\n"
        "  - `git push` to a private HTTPS remote -> the gh-credential-helper path substitutes too\n"
        "  - the TUI is garbled                  -> the sync stream is line-oriented; the port needs the async API\n"
        "Exit the shell/agent to return here; the box is then removed as usual.\n",
        flush=True,
    )

    execution = box.exec(
        cmd[0], cmd[1:], tty=True, env=[("TERM", os.environ.get("TERM") or "xterm-256color")]
    )

    def resize(*_):
        size = os.get_terminal_size()
        execution.resize_tty(size.lines, size.columns)

    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    out = sys.stdout.buffer

    def pump():
        stream = execution.stdout()
        for chunk in stream or ():
            out.write(chunk if isinstance(chunk, bytes) else chunk.encode())
            out.flush()

    try:
        resize()
        signal.signal(signal.SIGWINCH, resize)
        ttylib.setraw(fd)
        threading.Thread(target=pump, daemon=True).start()
        # stdin may be an attribute or a method, and may want bytes or str; one
        # probe_api() run replaces this with whichever it is.
        writer = execution.stdin() if callable(execution.stdin) else execution.stdin
        while True:
            data = os.read(fd, 1024)
            if not data:
                break
            try:
                writer.write(data)
            except TypeError:
                writer.write(data.decode("utf-8", "replace"))
        execution.wait()
    except Exception as exc:  # noqa: BLE001 - a shape mismatch is the finding
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)
        print(
            f"\nboxlite-secrets-spike: interactive attach failed: {exc!r}\n"
            "Run --probe-api on this host and paste the output; the attach is written\n"
            "against unverified shapes for stdin/stdout/resize_tty.",
            file=sys.stderr,
        )
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


if __name__ == "__main__":
    sys.exit(main())

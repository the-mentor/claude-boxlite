#!/bin/sh
# TTY smoke test for cbox's interactive pump (attach.rs / client.rs / server.rs).
#
# Not a `cargo test`: it needs a real booted box and a real pty, so it is not
# run by `cargo test` or any CI job. This was deferred on the premise that no
# controlling TTY exists in an automated harness -- that premise is false:
# `script(1)` allocates a real pty for the process it wraps regardless of
# whether *its own* stdin is a terminal, which is exactly what
# `cbox/tests/secrets_survive.md` already relies on to drive a real
# `cbox exec` session to completion from this same kind of harness.
#
# Run by hand after building the cbox binary and having an image available:
#
#   just build-cbox   # or: (cd cbox && cargo build --release)
#   ./cbox/tests/tty_smoke.sh [image-name]
#
# `image-name` defaults to `claude-boxlite-custom`. The script skips itself
# (exit 0, printing why) rather than failing when a precondition isn't met:
# no cbox binary, no such image, no `script(1)`, or a `script(1)` that
# doesn't understand the macOS `-q outfile cmd...` invocation this was
# written against (Linux's util-linux `script` takes different flags; this
# has only been exercised on macOS).
#
# What this proves that no `cargo test` in this crate covers:
#   - stdin/stdout actually round-trip over a real pty, not just an
#     in-process mpsc channel or a Cursor<Vec<u8>> (proto.rs's tests use the
#     latter).
#   - a guest's real exit code reaches the calling shell's $? through
#     `cbox exec` end to end (client.rs -> commands/exec.rs's exit_status())
#     -- this is exactly the shape Critical 1 (i32::MIN truncating to exit 0)
#     took.
#   - `TerminalGuard` actually emits its restoration sequences on the normal
#     exit path (`\x1b[?2004l` disables bracketed paste, `\x1b[>4;0m` resets
#     modifyOtherKeys) -- not just on the panic-unwind path
#     `terminal_guard.rs`'s own unit tests cover.
#   - termios itself is restored: `stty -g` on the *calling* terminal, if
#     there is one, is byte-identical before and after. (Under a harness with
#     no controlling terminal at all, `stty -g` itself fails the same way
#     before and after, which trivially satisfies "byte-identical" -- the
#     assertion only bites when run by a human at a real terminal.)
#
# Isolation: uses its own scratch BOXLITE_HOME under mktemp -- never touches
# ~/.boxlite or any box running there, and never inherits a BOXLITE_HOME from
# the calling environment. Cleans up (kills the box's shim process and the
# scratch home) on exit, success or failure.

set -u

image="${1:-claude-boxlite-custom}"
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
cbox_bin="$repo_root/cbox/target/release/cbox"
[ -x "$cbox_bin" ] || cbox_bin="$repo_root/cbox/target/debug/cbox"

skip() {
    echo "tty_smoke: SKIP - $1"
    exit 0
}

fail() {
    # No explicit cleanup call here: `trap cleanup EXIT` below already fires
    # on this `exit 1`, and calling it twice would just be redundant.
    echo "tty_smoke: FAIL - $1"
    exit 1
}

[ -x "$cbox_bin" ] || skip "no cbox binary at cbox/target/{release,debug}/cbox -- run 'just build-cbox' first"
command -v script >/dev/null 2>&1 || skip "no script(1) on PATH"
if ! docker image inspect "$image" >/dev/null 2>&1; then
    skip "image '$image' not found locally (docker image inspect failed) -- build it first"
fi
case "$(uname -s)" in
    Darwin) ;;
    *) skip "only exercised against macOS's script(1) (BSD -q flag); this is $(uname -s)" ;;
esac

# A short, fixed-prefix path, not `mktemp -d`'s output: on macOS `mktemp -d`
# lands under /var/folders/.../T/<random>, and this path nested three levels
# deeper (boxes/<name>/cbox.sock) then overflows sockaddr_un's ~104-byte
# sun_path limit -- observed live while writing this script ("cannot bind
# ...cbox.sock"). $$ keeps concurrent runs from colliding without needing a
# long random component.
home="/tmp/cbox-tty-smoke-$$"
rm -rf "$home"
mkdir -p "$home"
up_typescript=$(mktemp)
exec_typescript=$(mktemp)
zero_exit_typescript=$(mktemp)
shim_pid=""
up_pid=""

cleanup() {
    # Best-effort and idempotent: this runs on every exit path, including
    # after an assertion failure, so nothing here may itself abort the
    # script. Kills rather than waits for a graceful `cbox down` -- tearing
    # a real microVM down cleanly can take a long time (observed, separately
    # from anything this test checks), and a smoke test has no business
    # blocking on that.
    if [ -n "$up_pid" ]; then
        kill -9 "$up_pid" 2>/dev/null
    fi
    # The box's shim process (the host-side proxy that substitutes secret
    # placeholders) is spawned detached and outlives `cbox up` exiting, so
    # killing the `up` process above does not stop it. Find it by the
    # scratch home's own path, which is unique to this run, rather than by
    # name -- never touches a shim for any other box.
    for pid in $(pgrep -f "boxlite-shim" 2>/dev/null); do
        if ps -o command= -p "$pid" 2>/dev/null | grep -qF "$home"; then
            kill -9 "$pid" 2>/dev/null
        fi
    done
    rm -rf "$home" "$up_typescript" "$exec_typescript" "$zero_exit_typescript"
}
trap cleanup EXIT INT TERM

echo "tty_smoke: booting a box under scratch BOXLITE_HOME=$home"
(
    BOXLITE_HOME="$home" script -q "$up_typescript" \
        "$cbox_bin" up -f --image "$image" -- sh -c 'sleep 300'
) &
up_pid=$!

sock="$home/boxes/claude-boxlite/cbox.sock"
waited=0
while [ ! -S "$sock" ]; do
    if ! kill -0 "$up_pid" 2>/dev/null; then
        fail "'cbox up' exited before its control socket appeared -- see $up_typescript"
    fi
    waited=$((waited + 2))
    if [ "$waited" -ge 90 ]; then
        fail "control socket did not appear within 90s"
    fi
    sleep 2
done
echo "tty_smoke: control socket is up after ~${waited}s"

stty_before=""
if stty -g >/dev/null 2>&1; then
    stty_before=$(stty -g)
fi

echo "tty_smoke: running cbox exec -- sh -c 'printf READY; exit 7'"
BOXLITE_HOME="$home" script -q "$exec_typescript" \
    "$cbox_bin" exec -- sh -c 'printf READY; exit 7'
exec_status=$?

stty_after=""
if stty -g >/dev/null 2>&1; then
    stty_after=$(stty -g)
fi

# A gap this smoke test itself had until now: every check above uses a
# nonzero exit code (7), which `commands/exec.rs` has always routed through
# an explicit `std::process::exit` -- bypassing, by accident of this test's
# own choice of exit code, the hang this repo hit live: `tokio::io::stdin()`
# backs its reads with an uncancellable blocking `read(2)` on tokio's own
# blocking-thread pool (tokio's own docs on `stdin()` say so verbatim), so
# any return through `#[tokio::main]` that goes by way of a *zero* exit
# code -- which `up` always did, and `exec` did too whenever the guest
# happened to exit 0 -- hung the whole process until a stray keypress
# finally satisfied that read. Fixed at the root in `stdin_reader.rs` (a
# dedicated, genuinely cancel-safe reader thread, replacing
# `tokio::io::stdin()` in both `attach.rs` and `client.rs`), but a fix at
# the root is exactly the kind of thing worth pinning here: this check
# would have hung indefinitely against the old code, with no keypress ever
# coming from an automated run.
echo "tty_smoke: running cbox exec -- sh -c 'printf READY0; exit 0' (must return without a keypress)"
zero_exit_start=$(date +%s)
BOXLITE_HOME="$home" script -q "$zero_exit_typescript" \
    "$cbox_bin" exec -- sh -c 'printf READY0; exit 0'
zero_exit_status=$?
zero_exit_elapsed=$(($(date +%s) - zero_exit_start))

failures=0

if grep -q "READY" "$exec_typescript"; then
    echo "tty_smoke: PASS - stdout round-tripped (found READY)"
else
    echo "tty_smoke: FAIL - READY not found in captured output"
    failures=$((failures + 1))
fi

if [ "$exec_status" -eq 7 ]; then
    echo "tty_smoke: PASS - exit code 7 propagated through cbox exec"
else
    echo "tty_smoke: FAIL - expected exit code 7, got $exec_status (this is the exact shape Critical 1 was: a failed/signal-derived session truncating to 0)"
    failures=$((failures + 1))
fi

# The check that actually would have caught the runtime-shutdown hang: it
# only reaches this line at all if the *previous* `script -q ... cbox
# exec ...` line above already returned on its own -- a hang there blocks
# the whole script before this point is ever reached. The elapsed-time
# assertion below is the belt-and-suspenders part: a generous bound, well
# above any real box-boot time, that would only be exceeded by something
# waiting on a keypress that an automated run never sends.
if [ "$zero_exit_status" -eq 0 ] && grep -q "READY0" "$zero_exit_typescript"; then
    echo "tty_smoke: PASS - a zero-exit-code session round-tripped stdout and returned control"
else
    echo "tty_smoke: FAIL - zero-exit-code session misbehaved (status=$zero_exit_status) -- see $zero_exit_typescript"
    failures=$((failures + 1))
fi
if [ "$zero_exit_elapsed" -le 30 ]; then
    echo "tty_smoke: PASS - returned in ${zero_exit_elapsed}s with no keypress (the tokio::io::stdin() shutdown hang would not have returned at all)"
else
    echo "tty_smoke: FAIL - took ${zero_exit_elapsed}s to return -- suspiciously long for a session with no work left to do"
    failures=$((failures + 1))
fi

esc=$(printf '\033')
if grep -qF "${esc}[?2004l" "$exec_typescript"; then
    echo "tty_smoke: PASS - found ESC[?2004l (bracketed paste disabled by TerminalGuard)"
else
    echo "tty_smoke: FAIL - ESC[?2004l not found -- TerminalGuard may not have run"
    failures=$((failures + 1))
fi

if grep -qF "${esc}[>4;0m" "$exec_typescript"; then
    echo "tty_smoke: PASS - found ESC[>4;0m (modifyOtherKeys reset by TerminalGuard)"
else
    echo "tty_smoke: FAIL - ESC[>4;0m not found -- TerminalGuard may not have run"
    failures=$((failures + 1))
fi

if [ -z "$stty_before" ] && [ -z "$stty_after" ]; then
    echo "tty_smoke: PASS (trivially) - no controlling terminal to compare stty -g against"
elif [ "$stty_before" = "$stty_after" ]; then
    echo "tty_smoke: PASS - stty -g byte-identical before and after"
else
    echo "tty_smoke: FAIL - stty -g changed:"
    echo "  before: $stty_before"
    echo "  after:  $stty_after"
    failures=$((failures + 1))
fi

if [ "$failures" -eq 0 ]; then
    echo "tty_smoke: all checks passed"
    exit 0
else
    echo "tty_smoke: $failures check(s) failed"
    exit 1
fi

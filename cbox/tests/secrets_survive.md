# Do secrets survive the creating process?

Settles the open question in `docs/design/cbox.md` ("Secrets survive the creating process"):
whether the host-side MITM proxy that substitutes real credentials for `<BOXLITE_SECRET:...>`
placeholders lives as long as the box (per-box shim, per `vmm/controller/shim.rs`'s "shim
creates gvproxy" comment) or only as long as the process that ran `cbox up`. This matters
because `cbox exec` falls back to opening its own `BoxliteRuntime` (`Route::OwnRuntime` in
`cbox/src/client.rs`) whenever no control socket is present — including against a box whose
creating `cbox up` process has already exited. If the proxy died with that process, the
fallback would silently stop substituting credentials.

This is a manual/integration check, not a `cargo test` — it needs a real booted box and a real
GitHub token. Not run in CI.

## Isolation

Run against a throwaway `BOXLITE_HOME` so the check never touches the real boxes under
`~/.boxlite` (`achenroth`, `claude-boxlite`, `do-terraform-dc`, `Ti4CQivAitI5`):

```bash
export BOXLITE_HOME=/tmp/cbox-verify-12
```

The box name itself is still derived from the repo (`claude-boxlite`, via git-root naming in
`cbox/src/naming.rs`) — isolation comes entirely from the separate `BOXLITE_HOME` root, which
`cbox/src/config.rs`'s `box_home` joins with the name.

The `GH_TOKEN` used is whatever `~/.config/cbox/env` (this repo's gitignored `.env`, symlinked
in) provides; `cbox up` loads it automatically via `envfile::apply` before resolving secrets.
No token value is echoed anywhere below or in this file.

## Environment note

Every `cbox` invocation below ran under a harness with no controlling TTY. `cbox up` and the
`cbox exec` fallback path both go through `attach::attach`, which calls
`crossterm::terminal::enable_raw_mode()` — this fails with `Device not configured (os error 6)`
(ENXIO) with no controlling TTY, which is why `cbox up` below exits with an error right after
starting the box (expected, and irrelevant to what's under test: the box and the guest process
inside it are already running by that point). For step 3, to actually observe the exec
session's output despite the same raw-mode requirement, the command was wrapped in macOS's
`script(1)` to allocate a pty:

```bash
script -q /tmp/cbox-verify-12/exec_output.txt env BOXLITE_HOME=/tmp/cbox-verify-12 \
  ./cbox/target/debug/cbox exec -- sh -lc '...'
```

`script` itself did not exit cleanly afterward (it kept waiting on stdin with nothing to close
it) even though the wrapped `cbox exec` process had already finished and printed its output —
harmless, but the leftover `script`/`cbox exec` processes were killed by hand during cleanup;
no box-related process was affected.

## Commands run and observed output

### 1. Create a box with GitHub secrets, then let the creating process exit

```bash
$ BOXLITE_HOME=/tmp/cbox-verify-12 ./cbox/target/debug/cbox up -c --config registries.local.json -- bash
cbox: starting claude-boxlite (git repo root)
Error: Device not configured (os error 6)
```

Expected failure at raw-mode setup (see Environment note above). The box was created and
started before that point:

```bash
$ BOXLITE_HOME=/tmp/cbox-verify-12 ./cbox/target/debug/cbox list
NAME                 ID        STATUS     IMAGE                    CREATED          ORIGIN
claude-boxlite       55BRKMKA  running    claude-boxlite-custom    2026-08-19 07:36 .../claude-boxlite  <- here
```

### 2. Confirm the control socket is gone

```bash
$ ls -la /tmp/cbox-verify-12/boxes/claude-boxlite/cbox.sock
ls: /tmp/cbox-verify-12/boxes/claude-boxlite/cbox.sock: No such file or directory
```

Confirmed absent: `cbox up`'s unlink-on-exit ran even though it exited via the raw-mode error,
so `cbox exec` below is forced down `Route::OwnRuntime` — the connect attempt in
`client::route` gets `NotFound` and never touches the socket path at all.

### 3. Exec from a fresh process (forced through the `OwnRuntime` fallback) and check substitution

```bash
$ script -q /tmp/cbox-verify-12/exec_output2.txt env BOXLITE_HOME=/tmp/cbox-verify-12 \
    ./cbox/target/debug/cbox exec -- sh -lc \
    'printf "LOGIN="; gh api user --jq .login 2>&1; printf "GHTOKEN="; printenv GH_TOKEN; echo DONE_MARKER'
```

Captured output (terminal escape sequences from `TerminalGuard`'s restore-on-drop elided; the
GitHub login value is redacted here — it identifies whose token was used and isn't needed to
answer the question, only that the call succeeded and returned a non-empty login):

```
LOGIN=<redacted-non-empty-login>
GHTOKEN=<BOXLITE_SECRET:gh>
DONE_MARKER
```

- `gh api user --jq .login` **succeeded**, returning a real, non-empty GitHub login — proving
  the host-side MITM proxy substituted the real credential for the placeholder on this request,
  from a process (`cbox exec`, the fallback path) that did not create the box.
- `printenv GH_TOKEN` printed `<BOXLITE_SECRET:gh>` — proving the guest process itself only ever
  held the placeholder; the real value never entered the guest's environment.

## Verdict

**Secrets survive the creating process.** The design document's "read from source, not
verified" reading was correct: the MITM proxy is created by the per-box shim and lives as long
as the box, not as long as the `cbox up` process. `cbox exec`'s `Route::OwnRuntime` fallback
gets credential substitution for free, exactly as `docs/design/cbox.md`'s "Secrets survive the
creating process" section predicted.

No code changes follow from this result. This document itself is the "note it as verified and
continue" step named there and in the Open Questions section.

## Cleanup

```bash
$ BOXLITE_HOME=/tmp/cbox-verify-12 ./cbox/target/debug/cbox down
cbox: removed claude-boxlite
$ rm -rf /tmp/cbox-verify-12
```

Confirmed no `boxlite-shim` process remained for the test box, and no processes or boxes under
the real `~/.boxlite` were touched at any point.

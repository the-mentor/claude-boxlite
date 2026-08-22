//! `cbox exec`'s connect-or-fallback.
//!
//! Three cases, all load-bearing:
//!   - socket absent            -> open our own runtime against the detached box
//!   - socket present, but connect fails -> stale (a crashed `up`'s dead
//!     socket, ECONNREFUSED; or a stray non-socket file, ENOTSOCK); unlink,
//!     fall back
//!   - socket present and accepting       -> proxy over it
//!
//! Without the stale case, one SIGKILL'd `up` bricks exec for that box until
//! someone deletes a file by hand.
//!
//! A fourth, non-load-bearing case: a genuine permission error connecting to
//! the socket also falls back rather than hard-failing this command, but
//! warns loudly first — silently reaching `OwnRuntime` here can otherwise
//! surface downstream as the "Failed to acquire runtime lock" dead end this
//! whole module exists to eliminate, with no hint that the real cause was the
//! socket, not the lock.

use std::io::Write as _;
use std::path::Path;

use anyhow::Result;
use tokio::net::UnixStream;

use crate::proto::{ExecRequest, Frame, read_frame, write_frame};
use crate::server::socket_path;

pub enum Route {
    Socket(UnixStream),
    OwnRuntime,
}

pub async fn route(home: &Path) -> Result<Route> {
    let path = socket_path(home);
    // Connect directly rather than stat-then-connect: a preceding
    // `path.exists()` would swallow *any* stat failure into "false" —
    // indistinguishable from genuine absence — and it would also be a
    // TOCTOU race against whatever `connect` observes a moment later. The
    // connect attempt itself reports ENOENT as `NotFound`, so it already
    // covers the "absent" case without a separate check.
    match UnixStream::connect(&path).await {
        Ok(stream) => Ok(Route::Socket(stream)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Route::OwnRuntime),
        // A permissions problem (on the socket file itself, or on a
        // directory component of the path) is not "no one is home" — it's a
        // real, actionable failure, and silently falling back here is the
        // worst outcome this module can produce: if `up` really is attached
        // and holding the runtime lock, `OwnRuntime` below will fail with
        // "Failed to acquire runtime lock", the exact dead end this routing
        // exists to eliminate, with nothing pointing back at the real cause.
        // So: still fall back (this command shouldn't hard-fail over it),
        // but say so loudly first.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "cbox: warning: permission denied connecting to control socket {} ({e}); \
                 falling back to opening our own runtime. If `cbox up` is still attached to \
                 this box, that fallback will itself fail to acquire the runtime lock — the \
                 socket permission above is the real problem, not the lock.",
                path.display()
            );
            Ok(Route::OwnRuntime)
        }
        // Everything else means the file at this path is not a socket
        // someone is actively listening on. A SIGKILL'd `up` leaves a real
        // dead socket, which fails with ECONNREFUSED; a stray plain file or
        // directory (e.g. left by a `touch`, or a half-written file from
        // some other failure) fails even earlier, with ENOTSOCK — which
        // `std::io::ErrorKind` has no stable, matchable name for and
        // reports as its catch-all `Other`/`Uncategorized` kind, so it can't
        // be singled out by kind() the way ConnectionRefused/NotFound/
        // PermissionDenied can. Both are "no one is home" from the client's
        // point of view, and an exhaustive enumeration of every OS error
        // that can mean that is exactly the kind of file someone has to
        // delete by hand later — the failure mode this routing exists to
        // remove. So: unlink whatever is there and fall back.
        Err(e) => {
            if let Err(unlink_err) = std::fs::remove_file(&path) {
                // `remove_file` can't remove a directory (a stray directory
                // at the socket path fails connect the same ENOTSOCK way a
                // plain file does) — surface that rather than pretending the
                // unlink succeeded, since the next `cbox exec` will just hit
                // this exact spot again.
                eprintln!(
                    "cbox: warning: found a stale control socket at {} ({e}) but could not \
                     remove it ({unlink_err}); falling back to our own runtime anyway, but \
                     `cbox exec` will keep hitting this until the file is removed by hand",
                    path.display()
                );
            }
            Ok(Route::OwnRuntime)
        }
    }
}

/// Terminal-identity variables to forward with the Exec frame. `TERM` always
/// gets a value (defaulting to `xterm-256color`, mirroring `attach::attach`
/// and `up`'s own injection) because the server deliberately does not read
/// its own `TERM` — it's the wrong terminal, the one that ran `cbox up`.
/// Everything else here is skipped when unset rather than sent empty.
///
/// Deliberately narrow: this is terminal identity only, not general
/// environment or credentials. The box's environment is fixed at creation
/// and exec'd processes inherit it; this field must not become a second path
/// for those values.
const TERMINAL_IDENTITY_VARS: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "COLORTERM",
    "KITTY_WINDOW_ID",
    "WEZTERM_EXECUTABLE",
    "ITERM_SESSION_ID",
    "WT_SESSION",
    "VTE_VERSION",
];

fn terminal_env() -> Vec<(String, String)> {
    let mut out = vec![(
        "TERM".to_string(),
        std::env::var("TERM").unwrap_or_else(|_| crate::attach::DEFAULT_TERM.into()),
    )];
    for key in TERMINAL_IDENTITY_VARS {
        if let Ok(val) = std::env::var(key) {
            if !val.is_empty() {
                out.push((key.to_string(), val));
            }
        }
    }
    out
}

/// Drive an interactive session over the socket. Mirrors `attach::pump`, but
/// the far side is `up`'s listener rather than the box directly.
pub async fn proxy(stream: UnixStream, cmd: &[String]) -> Result<i32> {
    let (mut reader, mut writer) = stream.into_split();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    write_frame(
        &mut writer,
        &Frame::Exec(ExecRequest { cmd: cmd.to_vec(), env: terminal_env(), rows, cols }),
    )
    .await?;

    // See `attach.rs` for why this needs to be a guard rather than a
    // `disable_raw_mode()` call after `proxy_loop`: the guest inside the box
    // can leave terminal modes (bracketed paste, modifyOtherKeys, alternate
    // screen, mouse capture, cursor visibility) set via escape sequences that
    // termios-level raw mode restoration doesn't touch, and a manual call
    // after the pump is skipped entirely on a panic or an early `?`.
    let _terminal_guard = crate::terminal_guard::TerminalGuard::enable()?;
    proxy_loop(&mut reader, &mut writer).await
}

async fn proxy_loop(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<i32> {
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    // Not `tokio::io::stdin()` -- see `stdin_reader` for why: mirrors
    // `attach::pump`'s own reasoning and fix for the exact same hang.
    let mut stdin = crate::stdin_reader::StdinReader::spawn();

    loop {
        tokio::select! {
            frame = read_frame(reader) => match frame? {
                Some(Frame::Stdout(bytes)) => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                Some(Frame::Exit { code }) => {
                    if code == crate::proto::EXIT_CODE_SESSION_FAILED {
                        eprintln!(
                            "cbox: exec session ended before it produced an exit status \
                             (the server-side session failed before the guest process could \
                             be waited on)"
                        );
                    }
                    return Ok(code);
                }
                Some(_) => {}
                None => {
                    // `server.rs`'s `handle` guarantees a `Frame::Exit` is
                    // written before the connection closes on every path, so
                    // reaching a clean close without one first is itself an
                    // anomaly (e.g. the server process died) rather than a
                    // normal end of session -- it must not read as success.
                    eprintln!(
                        "cbox: control socket closed without an exit status; \
                         the guest session may still be running detached"
                    );
                    return Ok(1);
                }
            },
            chunk = stdin.read() => {
                let chunk = chunk?;
                if chunk.is_empty() {
                    eprintln!(
                        "cbox: local stdin closed; the guest session may still be running detached"
                    );
                    return Ok(1);
                }
                write_frame(writer, &Frame::Stdin(chunk)).await?;
            }
            _ = sigwinch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    write_frame(writer, &Frame::Resize { rows, cols }).await?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_socket_routes_to_own_runtime() {
        let dir = std::env::temp_dir()
            .join(format!("cbox-client-test-absent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = socket_path(&dir);
        assert!(!path.exists());

        assert!(matches!(route(&dir).await.unwrap(), Route::OwnRuntime));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_plain_file_at_the_socket_path_is_treated_as_stale_and_unlinked() {
        let dir = std::env::temp_dir()
            .join(format!("cbox-client-test-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = socket_path(&dir);
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(path.exists());

        assert!(matches!(route(&dir).await.unwrap(), Route::OwnRuntime));
        assert!(!path.exists(), "stale socket file must be unlinked");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_directory_at_the_socket_path_still_falls_back_even_though_it_cant_be_unlinked() {
        // `remove_file` can never remove a directory (EPERM/EISDIR, not the
        // ENOENT/ENOTSOCK cases the rest of this module expects), so a
        // directory left at the socket path is the one stale-shape that
        // `route()`'s unlink can't actually clear. The command must still
        // fall back rather than propagating that failure -- the unlink
        // failure only needs to be visible (checked by hand against
        // `--nocapture`, since capturing `eprintln!` output needs fd-level
        // tricks this crate's stdlib-only, no-new-deps constraint rules out),
        // not fatal.
        let dir = std::env::temp_dir()
            .join(format!("cbox-client-test-stale-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = socket_path(&dir);
        std::fs::create_dir_all(&path).unwrap();

        assert!(matches!(route(&dir).await.unwrap(), Route::OwnRuntime));
        assert!(path.exists(), "a directory can't be unlinked by remove_file; it stays put");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_live_listener_routes_to_the_socket() {
        let dir = std::env::temp_dir()
            .join(format!("cbox-client-test-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = socket_path(&dir);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        assert!(matches!(route(&dir).await.unwrap(), Route::Socket(_)));

        drop(listener);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_dead_socket_reproduces_econnrefused_and_is_unlinked() {
        // The scenario `route()`'s module doc names as the primary reason it
        // exists: a SIGKILL'd `up` leaves a real bound socket file with
        // nothing accepting on it, which fails with genuine ECONNREFUSED
        // (distinct from the plain-file/ENOTSOCK case above). Previously this
        // was verified only via an ad-hoc live session that no longer exists;
        // pinned here as a real bind-then-drop rather than a plain file, so
        // it actually exercises the ConnectionRefused arm, not the
        // catch-all one.
        let dir =
            std::env::temp_dir().join(format!("cbox-client-test-dead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = socket_path(&dir);
        {
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            drop(listener); // closes the fd but leaves the socket file behind
        }
        assert!(path.exists(), "bind must leave a real file at the socket path");

        assert!(matches!(route(&dir).await.unwrap(), Route::OwnRuntime));
        assert!(!path.exists(), "dead socket file must be unlinked");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_permission_denied_connect_falls_back_without_unlinking() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("cbox-client-test-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = socket_path(&dir);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        // Deny traversal into `dir` so connecting to the socket inside it
        // fails with EACCES/PermissionDenied rather than reaching it -- this
        // is the case `route()` must warn loudly about instead of silently
        // falling back the same way it does for a merely stale socket.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let probe = UnixStream::connect(&path).await;
        let is_permission_denied =
            matches!(&probe, Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied);

        if is_permission_denied {
            assert!(matches!(route(&dir).await.unwrap(), Route::OwnRuntime));
        }

        // Restore before cleanup regardless of the branch above, or
        // `remove_dir_all` below can't traverse into `dir` either.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(listener);
        std::fs::remove_dir_all(&dir).unwrap();

        if !is_permission_denied {
            eprintln!(
                "skipping permission-denied assertion: this environment does not enforce \
                 directory permission bits (e.g. running as root)"
            );
        }
    }

    #[test]
    fn terminal_env_always_carries_term_and_skips_unset_vars() {
        let _lock = crate::test_env_lock::lock();
        let _term = crate::test_env_lock::EnvVarGuard::remove("TERM");
        let _identity: Vec<_> = TERMINAL_IDENTITY_VARS
            .iter()
            .map(|&key| crate::test_env_lock::EnvVarGuard::remove(key))
            .collect();

        let env = terminal_env();
        assert_eq!(env, vec![("TERM".to_string(), "xterm-256color".to_string())]);
    }

    #[test]
    fn terminal_env_forwards_set_identity_vars_and_preserves_real_term() {
        let _lock = crate::test_env_lock::lock();
        let _term = crate::test_env_lock::EnvVarGuard::set("TERM", "screen-256color");
        let _term_program = crate::test_env_lock::EnvVarGuard::set("TERM_PROGRAM", "WezTerm");
        let _cleared: Vec<_> = [
            "TERM_PROGRAM_VERSION",
            "COLORTERM",
            "KITTY_WINDOW_ID",
            "WEZTERM_EXECUTABLE",
            "ITERM_SESSION_ID",
            "WT_SESSION",
            "VTE_VERSION",
        ]
        .iter()
        .map(|&key| crate::test_env_lock::EnvVarGuard::remove(key))
        .collect();

        let env = terminal_env();
        assert_eq!(
            env,
            vec![
                ("TERM".to_string(), "screen-256color".to_string()),
                ("TERM_PROGRAM".to_string(), "WezTerm".to_string()),
            ]
        );
    }
}

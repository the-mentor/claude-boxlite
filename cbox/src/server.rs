//! `cbox up`'s control socket.
//!
//! BoxLite locks the whole BOXLITE_HOME while a runtime is attached, so a
//! second process cannot open one for the same box. `up` already holds a
//! runtime; serving exec requests over a socket is what lets `cbox exec` work
//! while `up` is attached, without introducing a daemon.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use boxlite::{BoxCommand, LiteBox};
use futures::StreamExt;
use tokio::net::{UnixListener, UnixStream};

use crate::proto::{Frame, read_frame, write_frame};

pub fn socket_path(home: &Path) -> PathBuf {
    home.join("cbox.sock")
}

/// Bind the control socket and restrict it to the owner. Split out from
/// `serve` so the permission it sets is unit-testable without also running
/// the (infinite) accept loop.
fn bind(path: &Path) -> Result<UnixListener> {
    // A leftover socket from a SIGKILL'd `up` would block bind.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("cannot bind {}", path.display()))?;
    // Load-bearing on Linux, best-effort on macOS: Darwin does not reliably
    // enforce a Unix-domain socket's own mode on `connect()`, so the real
    // access control is the box home directory's 0700 mode set in
    // `commands::up::run` -- this is done in addition, not instead, since
    // Linux does enforce it.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot restrict permissions on {}", path.display()))?;
    Ok(listener)
}

/// Accept connections until cancelled. Each connection is one exec session.
pub async fn serve(litebox: Arc<LiteBox>, home: PathBuf) -> Result<()> {
    let path = socket_path(&home);
    let listener = bind(&path)?;

    loop {
        // A Unix-domain listener's `accept()` errors (e.g. a transient
        // EMFILE/ENFILE under fd exhaustion, or an aborted incoming
        // connection) are per-attempt, not evidence the listener itself is
        // broken — unlike `bind` above, nothing here reflects back to the
        // fixed, already-created socket file. Ending the loop on the first
        // one would permanently disable `cbox exec` for the rest of this
        // `up` session over what is normally a self-correcting condition, so
        // log and keep accepting. The brief sleep avoids a hot spin loop if
        // the underlying condition (e.g. fd exhaustion) does not clear
        // immediately.
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                eprintln!("cbox: accept failed, continuing to serve: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let litebox = Arc::clone(&litebox);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, litebox).await {
                eprintln!("cbox: exec session ended: {e}");
            }
        });
    }
}

/// An empty `cmd` would panic at `req.cmd[0]` below — reject it up front with
/// a normal error instead, since `cmd` is client-controlled input arriving
/// over the socket, not a value this process constructed itself.
fn validate_cmd(cmd: &[String]) -> Result<()> {
    if cmd.is_empty() {
        anyhow::bail!("Exec frame had an empty cmd");
    }
    Ok(())
}

/// Run one exec session, then guarantee the client sees an `Exit` frame no
/// matter how the session ended.
///
/// `handle_session`'s own success path already writes a real `Frame::Exit`
/// (from `exec.wait()`) before returning `Ok`. Every early-return path inside
/// it (`?` on setup, `bail!` on a malformed first frame, an empty `cmd`, a
/// missing stdout/stdin) skips straight past that write. Left alone, the
/// connection just closes — which `proxy_loop` on the client (`client.rs`)
/// reads as a clean EOF and reports as exit code 0, turning a failure to even
/// start the exec into a reported success. Catching every `Err` here once,
/// rather than adding a frame write at each early-return site, is what
/// guarantees that.
async fn handle(stream: UnixStream, litebox: Arc<LiteBox>) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let result = handle_session(&mut reader, &mut writer, litebox).await;
    if result.is_err() {
        let _ =
            write_frame(&mut writer, &Frame::Exit { code: crate::proto::EXIT_CODE_SESSION_FAILED })
                .await;
    }
    result
}

async fn handle_session(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    litebox: Arc<LiteBox>,
) -> Result<()> {
    let Some(Frame::Exec(req)) = read_frame(reader).await? else {
        anyhow::bail!("first frame was not Exec");
    };

    // `cmd` arrives from a client over the socket, not from a trusted
    // in-process caller — an empty `cmd` must fail cleanly here rather than
    // let `req.cmd[0]` below panic, which would surface as a raw backtrace
    // on `up`'s stderr instead of a normal "exec session ended" message.
    validate_cmd(&req.cmd)?;

    // The client (a different terminal running `cbox exec`) sent its own
    // environment — including TERM — in `req.env`. The server process is the
    // terminal that ran `cbox up`; reading TERM from its own environment
    // would answer with the wrong terminal's identity and silently break
    // things like the Kitty keyboard protocol negotiation in the client's
    // terminal.
    let mut command = BoxCommand::new(&req.cmd[0]).tty(true);
    for (key, val) in &req.env {
        command = command.env(key, val);
    }
    if req.cmd.len() > 1 {
        command = command.args(&req.cmd[1..]);
    }

    let mut exec = litebox.exec(command).await.context("exec failed")?;

    // Invariant from here on: once `litebox.exec()` has handed back a live
    // guest process, no path may drop `exec` without a best-effort kill().
    // `Execution` has no `Drop` impl, so an early `?`/return on any of the
    // setup calls below would otherwise orphan the process we just spawned
    // for the remaining life of the box — the same class of leak the
    // disconnect handling below (`client_gone`) closes, just triggered by a
    // setup failure instead of a lost client.
    if let Err(e) = exec.resize_tty(req.rows as u32, req.cols as u32).await {
        let _ = exec.kill().await;
        return Err(e).context("resize_tty failed");
    }
    let mut out = match exec.stdout() {
        Some(out) => out,
        None => {
            let _ = exec.kill().await;
            anyhow::bail!("no stdout");
        }
    };
    let mut stdin_writer = match exec.stdin() {
        Some(stdin_writer) => stdin_writer,
        None => {
            let _ = exec.kill().await;
            anyhow::bail!("no stdin");
        }
    };
    let exec = Arc::new(exec);

    // Whether the loop ended because the guest process's own stdout closed
    // (nothing to clean up — it already exited) or because the *client*
    // went away while the guest was still running. Unlike attach.rs, where
    // the "client" is the local terminal and its process exiting ends the
    // session by definition, here a disconnected remote client leaves the
    // guest-side exec running with nothing left to consume its output — it
    // must be killed explicitly or it leaks for the rest of the box's life.
    let mut client_gone = false;

    loop {
        tokio::select! {
            chunk = out.next() => match chunk {
                Some(text) => {
                    if write_frame(writer, &Frame::Stdout(text.into_bytes())).await.is_err() {
                        // The client is no longer reachable.
                        client_gone = true;
                        break;
                    }
                }
                None => break, // guest process's stdout closed on its own
            },
            frame = read_frame(reader) => match frame {
                Ok(Some(Frame::Stdin(bytes))) => {
                    if stdin_writer.write(&bytes).await.is_err() {
                        // The guest's stdin pipe is gone (e.g. it already
                        // exited). Same treatment as every other client_gone
                        // path below: without it, `client_gone` stays false,
                        // the kill() after this loop never runs, and
                        // `exec.wait()` afterwards can block forever on a
                        // guest that is (or may still be) running.
                        client_gone = true;
                        break;
                    }
                }
                Ok(Some(Frame::Resize { rows, cols })) => {
                    let _ = exec.resize_tty(rows as u32, cols as u32).await;
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    // Client disconnected cleanly.
                    client_gone = true;
                    break;
                }
                Err(_) => {
                    // Truncated frame — the connection dropped mid-write.
                    client_gone = true;
                    break;
                }
            },
        }
    }

    if client_gone {
        // Best-effort: the session is already over, so a failed kill must
        // not turn into an error here.
        let _ = exec.kill().await;
    }

    // `unwrap_or(0)` here would report a failed `wait()` as a clean exit
    // 0 -- the same class of bug as the truncation `EXIT_CODE_SESSION_FAILED`
    // exists to prevent on the wire. Fall back to that same sentinel instead,
    // so a `wait()` failure reaches the client as a real, non-zero-looking
    // failure rather than silent success.
    let code = exec.wait().await.map(|r| r.code()).unwrap_or(crate::proto::EXIT_CODE_SESSION_FAILED);
    let _ = write_frame(writer, &Frame::Exit { code }).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_lives_inside_the_box_home() {
        let p = socket_path(Path::new("/tmp/boxes/demo"));
        assert_eq!(p, PathBuf::from("/tmp/boxes/demo/cbox.sock"));
    }

    #[tokio::test]
    async fn the_socket_is_created_with_owner_only_permissions() {
        // Any local process that can reach this socket gets arbitrary TTY
        // exec into a box that substitutes a real credential into outbound
        // HTTPS -- this is the load-bearing assertion for Critical 3.
        let dir = std::env::temp_dir()
            .join(format!("cbox-server-test-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = socket_path(&dir);

        let _listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be owner-only, got {mode:o}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn bind_tightens_permissions_on_a_pre_existing_stale_socket() {
        // `bind` unlinks-and-rebinds rather than reusing whatever is there,
        // so a stale socket left with looser permissions by an earlier run
        // must still end up owner-only after this runs.
        let dir = std::env::temp_dir()
            .join(format!("cbox-server-test-stale-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = socket_path(&dir);
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        let _listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "re-bound socket must be owner-only, got {mode:o}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_cmd_is_rejected_instead_of_panicking_at_index_zero() {
        // Guards the `req.cmd[0]` indexing in `handle()`: a client-supplied
        // empty `cmd` must be caught here rather than reach that index and
        // panic on a client-controlled input.
        assert!(validate_cmd(&[]).is_err());
    }

    #[test]
    fn a_non_empty_cmd_passes_validation() {
        assert!(validate_cmd(&["sleep".to_string()]).is_ok());
    }
}

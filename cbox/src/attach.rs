//! Attach the local terminal to a TTY exec inside the box.
//!
//! The loop ends on `None` from the stdout stream, which is a real EOF: the
//! stream is an mpsc receiver whose sender drops when the guest process dies.
//! The Python binding lost that signal and needed a timed liveness probe;
//! here the probe is unnecessary.

use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use boxlite::{BoxCommand, LiteBox};
use futures::{Stream, StreamExt};

/// Default `TERM` when the invoking shell doesn't set one. Shared with
/// `client.rs` (which sends it in the `Exec` frame) and `commands/up.rs`
/// (which bakes it into the box's own environment) so the fallback value
/// can't drift between the three call sites.
pub const DEFAULT_TERM: &str = "xterm-256color";

pub async fn attach(litebox: &LiteBox, cmd: &[String]) -> Result<i32> {
    let term = std::env::var("TERM").unwrap_or_else(|_| DEFAULT_TERM.into());
    let mut command = BoxCommand::new(&cmd[0]).tty(true).env("TERM", term);
    if cmd.len() > 1 {
        command = command.args(&cmd[1..]);
    }

    let mut exec = litebox.exec(command).await.context("interactive exec failed")?;

    // Invariant from here on: once `litebox.exec()` has handed back a live
    // guest process, no path may drop `exec` without a best-effort kill().
    // `Execution` has no `Drop` impl, so an early `?` on any of the setup
    // calls below would otherwise orphan the guest process for good --
    // `server.rs`'s `handle_session` had this exact leak and fixed it the
    // same way.
    // crossterm reports (cols, rows); resize_tty takes (rows, cols).
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if let Err(e) = exec.resize_tty(rows as u32, cols as u32).await {
        let _ = exec.kill().await;
        return Err(e).context("resize_tty failed");
    }

    // Take both streams before sharing `exec`: these need &mut, everything
    // afterwards (resize_tty) needs only &self.
    let out_stream = match exec.stdout() {
        Some(s) => s,
        None => {
            let _ = exec.kill().await;
            anyhow::bail!("no stdout on the interactive exec");
        }
    };
    let stdin_writer = match exec.stdin() {
        Some(s) => s,
        None => {
            let _ = exec.kill().await;
            anyhow::bail!("no stdin on the interactive exec");
        }
    };
    let exec = Arc::new(exec);

    // Guards raw mode plus every guest-set terminal escape mode (bracketed
    // paste, modifyOtherKeys, alternate screen, mouse capture, cursor
    // visibility) and restores them all on drop -- clean return, an early
    // `?` below, or a panic unwinding through `pump`. A plain
    // `disable_raw_mode()` call after `pump` (the previous approach) only
    // undoes termios state and is skipped entirely on a panic.
    let _terminal_guard = crate::terminal_guard::TerminalGuard::enable()?;
    pump(exec, out_stream, stdin_writer).await
}

async fn pump(
    exec: Arc<boxlite::Execution>,
    mut out_stream: impl Stream<Item = String> + Unpin,
    mut stdin_writer: boxlite::ExecStdin,
) -> Result<i32> {
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    // Not `tokio::io::stdin()` -- see `stdin_reader` for why: that handle's
    // read cannot be cancelled, and this loop exits promptly on the guest's
    // stdout EOF without ever cancelling the read it's racing against,
    // which otherwise hangs process shutdown until a stray keypress.
    let mut stdin = crate::stdin_reader::StdinReader::spawn();

    // Only the guest-exited path (`out_stream` returning `None`) may call
    // `exec.wait()` below. Boxes are created with `detach: true` precisely so
    // they survive their terminal going away, so if the loop instead ends
    // because local stdin closed (or a write to the guest's stdin failed),
    // the guest may still be running — waiting on it here could hang this
    // command forever.
    //
    // This is *not* the same treatment `server.rs`'s `client_gone` flag gets,
    // despite the similar-looking split: `server.rs` kills the guest exec
    // before waiting on it, because a disconnected remote client leaves no
    // one to consume the guest's output. Here the "client" going away is the
    // local terminal exiting or losing its stdin, which is the normal,
    // intentional way to leave a detached box running unattended — so this
    // path does not kill the guest, and does not wait on it either.
    //
    // `out_stream` returning `None` is the only exit that's a genuine,
    // silent success: it's a direct wrapper around a tokio mpsc receiver
    // (`boxlite`'s `ExecStdout::poll_next` is a bare `self.receiver.poll_recv`),
    // and tokio's mpsc guarantees a receiver yields every buffered message
    // before it ever returns `None` -- so nothing the guest wrote is lost by
    // the time this loop stops reading it. The other two exits below abandon
    // the session for a local-side reason instead, which is exactly what a
    // silent `Ok(0)` used to hide from the user.
    enum EndReason {
        GuestExited,
        LocalStdinClosed,
        StdinWriteFailed,
    }
    let end_reason;

    loop {
        tokio::select! {
            chunk = out_stream.next() => match chunk {
                Some(text) => {
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                }
                None => { end_reason = EndReason::GuestExited; break; }
            },
            chunk = stdin.read() => {
                let chunk = chunk?;
                if chunk.is_empty() {
                    end_reason = EndReason::LocalStdinClosed;
                    break;
                }
                if stdin_writer.write(&chunk).await.is_err() {
                    end_reason = EndReason::StdinWriteFailed;
                    break;
                }
            }
            _ = sigwinch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let _ = exec.resize_tty(rows as u32, cols as u32).await;
                }
            }
        }
    }

    match end_reason {
        EndReason::GuestExited => {
            let result = exec.wait().await.context("waiting on the exec after the guest exited")?;
            Ok(result.code())
        }
        EndReason::LocalStdinClosed => {
            eprintln!("cbox: local stdin closed; the guest session may still be running detached");
            Ok(1)
        }
        EndReason::StdinWriteFailed => {
            eprintln!("cbox: failed to write to the guest's stdin; the guest session may still be running detached");
            Ok(1)
        }
    }
}

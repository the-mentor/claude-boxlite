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
use tokio::io::AsyncReadExt as _;

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
    // crossterm reports (cols, rows); resize_tty takes (rows, cols).
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    exec.resize_tty(rows as u32, cols as u32).await?;

    // Take both streams before sharing `exec`: these need &mut, everything
    // afterwards (resize_tty) needs only &self.
    let out_stream = exec.stdout().context("no stdout on the interactive exec")?;
    let stdin_writer = exec.stdin().context("no stdin on the interactive exec")?;
    let exec = Arc::new(exec);

    crossterm::terminal::enable_raw_mode()?;
    let result = pump(exec, out_stream, stdin_writer).await;
    crossterm::terminal::disable_raw_mode()?;
    result
}

async fn pump(
    exec: Arc<boxlite::Execution>,
    mut out_stream: impl Stream<Item = String> + Unpin,
    mut stdin_writer: boxlite::ExecStdin,
) -> Result<i32> {
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 4096];

    // Only the guest-exited path (`out_stream` returning `None`) may call
    // `exec.wait()` below. Boxes are created with `detach: true` precisely so
    // they survive their terminal going away, so if the loop instead ends
    // because local stdin closed (or a write to the guest's stdin failed),
    // the guest may still be running — waiting on it here could hang this
    // command forever. Mirrors `server.rs`'s `client_gone` flag, which draws
    // the same distinction for the same reason on the other end of the pipe.
    let mut guest_exited = false;

    loop {
        tokio::select! {
            chunk = out_stream.next() => match chunk {
                Some(text) => {
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                }
                None => { guest_exited = true; break; } // guest exited
            },
            read = stdin.read(&mut buf) => {
                let n = read?;
                if n == 0 {
                    break; // local stdin closed; the guest may still be running
                }
                if stdin_writer.write(&buf[..n]).await.is_err() {
                    break; // write failed; the guest's exit status isn't ours to wait on here
                }
            }
            _ = sigwinch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let _ = exec.resize_tty(rows as u32, cols as u32).await;
                }
            }
        }
    }

    if guest_exited {
        let result = exec.wait().await.context("waiting on the exec after the guest exited")?;
        Ok(result.code())
    } else {
        Ok(0)
    }
}

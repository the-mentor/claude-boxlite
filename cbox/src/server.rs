//! `cbox up`'s control socket.
//!
//! BoxLite locks the whole BOXLITE_HOME while a runtime is attached, so a
//! second process cannot open one for the same box. `up` already holds a
//! runtime; serving exec requests over a socket is what lets `cbox exec` work
//! while `up` is attached, without introducing a daemon.

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

/// Accept connections until cancelled. Each connection is one exec session.
pub async fn serve(litebox: Arc<LiteBox>, home: PathBuf) -> Result<()> {
    let path = socket_path(&home);
    // A leftover socket from a SIGKILL'd `up` would block bind.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot bind {}", path.display()))?;

    loop {
        let (stream, _) = listener.accept().await?;
        let litebox = Arc::clone(&litebox);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, litebox).await {
                eprintln!("cbox: exec session ended: {e}");
            }
        });
    }
}

async fn handle(stream: UnixStream, litebox: Arc<LiteBox>) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    let Some(Frame::Exec(req)) = read_frame(&mut reader).await? else {
        anyhow::bail!("first frame was not Exec");
    };

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
    exec.resize_tty(req.rows as u32, req.cols as u32).await?;
    let mut out = exec.stdout().context("no stdout")?;
    let mut stdin_writer = exec.stdin().context("no stdin")?;
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
                    if write_frame(&mut writer, &Frame::Stdout(text.into_bytes())).await.is_err() {
                        // The client is no longer reachable.
                        client_gone = true;
                        break;
                    }
                }
                None => break, // guest process's stdout closed on its own
            },
            frame = read_frame(&mut reader) => match frame {
                Ok(Some(Frame::Stdin(bytes))) => {
                    if stdin_writer.write(&bytes).await.is_err() {
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

    let code = exec.wait().await.map(|r| r.code()).unwrap_or(0);
    let _ = write_frame(&mut writer, &Frame::Exit { code }).await;
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
}

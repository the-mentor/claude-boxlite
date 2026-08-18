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

use std::io::Write as _;
use std::path::Path;

use anyhow::Result;
use tokio::io::AsyncReadExt as _;
use tokio::net::UnixStream;

use crate::proto::{ExecRequest, Frame, read_frame, write_frame};
use crate::server::socket_path;

pub enum Route {
    Socket(UnixStream),
    OwnRuntime,
}

pub async fn route(home: &Path) -> Result<Route> {
    let path = socket_path(home);
    if !path.exists() {
        return Ok(Route::OwnRuntime);
    }
    match UnixStream::connect(&path).await {
        Ok(stream) => Ok(Route::Socket(stream)),
        // Any connect failure means the file at this path is not a socket
        // someone is actively listening on. A SIGKILL'd `up` leaves a real
        // dead socket, which fails with ECONNREFUSED; a stray plain file
        // (e.g. left by a `touch`, or a half-written file from some other
        // failure) fails even earlier, with ENOTSOCK — which
        // `std::io::ErrorKind` has no stable, matchable name for and
        // reports as its catch-all `Other` kind, so it can't be singled
        // out by kind() the way ConnectionRefused/NotFound can. Both cases
        // are "no one is home" from the client's point of view, and an
        // exhaustive enumeration of every OS error that can mean that is
        // exactly the kind of file someone has to delete by hand later —
        // the failure mode this routing exists to remove. So: unlink
        // whatever is there and fall back unconditionally rather than
        // trying to name every kind that counts as stale.
        Err(_) => {
            let _ = std::fs::remove_file(&path);
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
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
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

    crossterm::terminal::enable_raw_mode()?;
    let result = proxy_loop(&mut reader, &mut writer).await;
    crossterm::terminal::disable_raw_mode()?;
    result
}

async fn proxy_loop(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<i32> {
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 4096];

    loop {
        tokio::select! {
            frame = read_frame(reader) => match frame? {
                Some(Frame::Stdout(bytes)) => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                Some(Frame::Exit { code }) => return Ok(code),
                Some(_) => {}
                None => return Ok(0),
            },
            read = stdin.read(&mut buf) => {
                let n = read?;
                if n == 0 {
                    return Ok(0);
                }
                write_frame(writer, &Frame::Stdin(buf[..n].to_vec())).await?;
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

    #[test]
    fn terminal_env_always_carries_term_and_skips_unset_vars() {
        let _lock = crate::test_env_lock::lock();
        unsafe {
            std::env::remove_var("TERM");
            for key in TERMINAL_IDENTITY_VARS {
                std::env::remove_var(key);
            }
        }
        let env = terminal_env();
        assert_eq!(env, vec![("TERM".to_string(), "xterm-256color".to_string())]);
    }

    #[test]
    fn terminal_env_forwards_set_identity_vars_and_preserves_real_term() {
        let _lock = crate::test_env_lock::lock();
        unsafe {
            std::env::set_var("TERM", "screen-256color");
            std::env::set_var("TERM_PROGRAM", "WezTerm");
            std::env::remove_var("TERM_PROGRAM_VERSION");
            std::env::remove_var("COLORTERM");
            std::env::remove_var("KITTY_WINDOW_ID");
            std::env::remove_var("WEZTERM_EXECUTABLE");
            std::env::remove_var("ITERM_SESSION_ID");
            std::env::remove_var("WT_SESSION");
            std::env::remove_var("VTE_VERSION");
        }
        let env = terminal_env();
        assert_eq!(
            env,
            vec![
                ("TERM".to_string(), "screen-256color".to_string()),
                ("TERM_PROGRAM".to_string(), "WezTerm".to_string()),
            ]
        );
        unsafe {
            std::env::remove_var("TERM");
            std::env::remove_var("TERM_PROGRAM");
        }
    }
}

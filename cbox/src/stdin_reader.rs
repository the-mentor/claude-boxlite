//! A cancel-safe substitute for `tokio::io::stdin()`.
//!
//! `tokio::io::stdin()` (pinned at tokio 1.53.1 here, per `Cargo.lock`) is
//! documented, in its own source, as the wrong tool for exactly what
//! `attach.rs`'s pump loop and `client.rs`'s proxy loop use it for:
//!
//! > This handle is best used for non-interactive uses, such as when a file
//! > is piped into the application. For technical reasons, `stdin` is
//! > implemented by using an ordinary blocking read on a separate thread,
//! > and it is impossible to cancel that read. **This can make shutdown of
//! > the runtime hang until the user presses enter.**
//! >
//! > For interactive uses, it is recommended to spawn a thread dedicated to
//! > user input and use blocking IO directly in that thread.
//!
//! That is the exact bug this module exists to remove: both loops select
//! over a guest-exit signal and a stdin read, and both exit promptly on the
//! former without ever cancelling the latter -- because it cannot be
//! cancelled. `#[tokio::main]` drops the `Runtime` at the end of `main`,
//! and dropping it waits for every outstanding task on tokio's own
//! blocking-thread pool, so that one uncancellable read parks the whole
//! process shutdown until a stray keypress finally completes it.
//!
//! The fix is the one tokio's own docs recommend, verbatim: a thread
//! dedicated to reading stdin, doing ordinary blocking IO. The key
//! difference from `tokio::io::stdin()` is that this is a plain
//! `std::thread`, not one of tokio's *own* blocking-pool threads --
//! `Runtime::drop` only waits for pools it owns, so it never waits for this
//! one. And because a normal Rust process exit (whether via returning from
//! `main` or `std::process::exit`) terminates every thread unconditionally,
//! this thread needs no cancellation or joining at all; it is simply
//! abandoned along with the rest of the process. (An `AsyncFd`-based
//! non-blocking reader was the other option raised for this fix, but it
//! requires flipping `O_NONBLOCK` on fd 0 itself -- a file-status flag
//! shared with whatever else has that same terminal open via `dup`/`fork`,
//! typically the parent shell -- and reliably flipping it back on every
//! exit path, including a `SIGKILL`, is not possible. A dedicated thread
//! touches no fd flags at all, so it carries no equivalent risk of leaking
//! non-blocking mode into the user's own shell.)
use tokio::sync::mpsc;

/// One line of defense narrower than a whole-buffer read: chunks are
/// forwarded as soon as the blocking thread's `read` returns, same as
/// `tokio::io::Stdin::read` does for its caller.
pub struct StdinReader {
    rx: mpsc::Receiver<std::io::Result<Vec<u8>>>,
}

impl StdinReader {
    /// Spawn the dedicated reader thread and return a handle to its output.
    pub fn spawn() -> Self {
        // Capacity 1: this is a handoff, not a buffer -- the reader thread
        // blocks on the channel send until the async side has consumed the
        // previous chunk, which is exactly the backpressure a synchronous
        // `read()` would have provided anyway.
        let (tx, rx) = mpsc::channel(1);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            loop {
                let mut buf = vec![0u8; 4096];
                let result = std::io::Read::read(&mut stdin, &mut buf).map(|n| {
                    buf.truncate(n);
                    buf
                });
                // An empty `Ok` (EOF) or any `Err` is the last thing this
                // thread will ever report -- stop rather than spin reading
                // an already-closed or broken stdin forever.
                let is_final = matches!(&result, Ok(chunk) if chunk.is_empty()) || result.is_err();
                if tx.blocking_send(result).is_err() || is_final {
                    return;
                }
            }
        });
        Self { rx }
    }

    /// Read the next chunk. An empty `Ok` means EOF -- mirrors
    /// `AsyncReadExt::read`'s `Ok(0)` convention closely enough for both
    /// call sites, which already treat "zero bytes" as the close signal.
    pub async fn read(&mut self) -> std::io::Result<Vec<u8>> {
        match self.rx.recv().await {
            Some(result) => result,
            // The sender only drops after sending a final EOF/Err chunk
            // (the thread `return`s right after), so in practice this arm
            // is never reached -- kept as a safe fallback rather than an
            // `unwrap`/`expect` in case that invariant ever changes.
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the real OS stdin of the test process, which under
    // `cargo test` is not a terminal -- reading from it should hit EOF
    // (or, harmlessly, block forever on some CI setups that redirect a
    // never-closing pipe; nothing in this crate's CI runs that way today).
    // The behavior that matters and *is* portable to test without a real
    // terminal is `read`'s handling of an already-closed channel, covered
    // directly below without touching a thread at all.

    #[tokio::test]
    async fn a_closed_channel_reads_as_a_clean_eof_not_a_panic() {
        let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>(1);
        drop(tx);
        let mut reader = StdinReader { rx };
        let chunk = reader.read().await.expect("a closed channel must read as Ok, not Err");
        assert!(chunk.is_empty(), "a closed channel must read as EOF (empty), not data");
    }

    #[tokio::test]
    async fn a_forwarded_chunk_is_read_back_unchanged() {
        let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>(1);
        tx.send(Ok(b"hello".to_vec())).await.unwrap();
        let mut reader = StdinReader { rx };
        let chunk = reader.read().await.unwrap();
        assert_eq!(chunk, b"hello");
    }

    #[tokio::test]
    async fn a_forwarded_error_is_read_back_as_an_error() {
        let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>(1);
        tx.send(Err(std::io::Error::other("boom"))).await.unwrap();
        let mut reader = StdinReader { rx };
        assert!(reader.read().await.is_err());
    }
}

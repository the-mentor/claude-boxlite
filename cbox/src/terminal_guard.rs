//! RAII guard for terminal state around the pump loops in `attach.rs` and
//! `client.rs`.
//!
//! `crossterm::terminal::disable_raw_mode()` alone only undoes termios state
//! this process itself set. It does nothing about modes the *guest*
//! application (Claude Code, running inside the box) enables via escape
//! sequences and normally disables itself on exit -- bracketed paste, the
//! xterm "modifyOtherKeys" keyboard protocol, alternate screen, mouse
//! tracking, cursor visibility. When a session ends abruptly -- a panic, an
//! early `?`, a broken pipe -- the guest never gets to run its own cleanup
//! sequence, and those modes leak into the user's shell: pastes come out
//! wrapped in literal `200~`...`201~`, and Ctrl-C prints `27;5;99~` instead
//! of raising SIGINT.
//!
//! `TerminalGuard` restores all of the above from a single `Drop` impl, so it
//! fires on every path a scope can be left by -- clean return, an early `?`,
//! or unwinding out of a panic -- not just a line at the end of a function
//! that any of those would skip over.
//!
//! Every sequence written here is a "turn this off" request; sending it to a
//! terminal that was never in that mode is a no-op by construction (xterm
//! and terminfo-compatible terminals ignore a disable for a mode that's
//! already off), so restoring unconditionally on every exit is safe.

use std::io::Write;

use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::terminal::LeaveAlternateScreen;

/// xterm's modifyOtherKeys has no crossterm command. `CSI > 4 ; 0 m` fully
/// disables it (mode 4, value 0) rather than merely resetting it to the
/// terminal's default, which might not be off.
const DISABLE_MODIFY_OTHER_KEYS: &[u8] = b"\x1b[>4;0m";

/// Write every restoration sequence to `out`. Split out from `Drop` so it can
/// be unit-tested against an in-memory buffer instead of a real terminal.
fn write_restore_sequences(out: &mut impl Write) -> std::io::Result<()> {
    crossterm::execute!(
        out,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show,
    )?;
    out.write_all(DISABLE_MODIFY_OTHER_KEYS)?;
    out.flush()
}

/// Enables raw mode on construction; on drop (clean return, `?`, or panic
/// unwind), restores raw mode plus every guest-set escape-sequence mode
/// listed above. `W` defaults to `Stdout` for production use; tests inject
/// an in-memory sink via [`TerminalGuard::with_writer`].
pub struct TerminalGuard<W: Write = std::io::Stdout> {
    out: W,
}

impl TerminalGuard<std::io::Stdout> {
    /// Enable raw mode and return a guard that restores everything on drop.
    pub fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self { out: std::io::stdout() })
    }
}

impl<W: Write> TerminalGuard<W> {
    /// Construct directly against an injected writer, skipping the
    /// raw-mode ioctl. Used by tests: this environment has no controlling
    /// TTY to enter raw mode on, but the `Drop` restoration logic below
    /// doesn't depend on raw mode having actually been entered, so it's
    /// exercised the same way production does.
    #[cfg(test)]
    fn with_writer(out: W) -> Self {
        Self { out }
    }
}

impl<W: Write> Drop for TerminalGuard<W> {
    fn drop(&mut self) {
        // Best-effort: this runs during panic unwinding too, so a failing
        // write here must not itself panic (that would abort the process
        // instead of completing the unwind) or mask the original error.
        let _ = write_restore_sequences(&mut self.out);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn drop_writes_every_restoration_sequence() {
        let buf = SharedBuf::default();
        let guard = TerminalGuard::with_writer(buf.clone());
        drop(guard);

        let written = buf.0.lock().unwrap().clone();
        let written = String::from_utf8_lossy(&written);

        assert!(written.contains("\x1b[?2004l"), "bracketed paste disable missing: {written:?}");
        assert!(written.contains("\x1b[?1000l") || written.contains("\x1b[?1006l"),
            "mouse capture disable missing: {written:?}");
        assert!(written.contains("\x1b[?1049l"), "leave alternate screen missing: {written:?}");
        assert!(written.contains("\x1b[?25h"), "cursor show missing: {written:?}");
        assert!(written.contains("\x1b[>4;0m"), "modifyOtherKeys disable missing: {written:?}");
    }

    #[test]
    fn drop_fires_on_panic_unwind() {
        let buf = SharedBuf::default();
        let buf_for_guard = buf.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = TerminalGuard::with_writer(buf_for_guard);
            panic!("simulated failure while the guard is held");
        }));
        assert!(result.is_err(), "the panic should have propagated out of catch_unwind");

        let written = buf.0.lock().unwrap().clone();
        assert!(!written.is_empty(), "Drop should have written restoration sequences even though the scope was left via panic, not a clean return");
    }
}

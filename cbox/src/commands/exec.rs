//! `cbox exec` — open a session in a running box.

use anyhow::{Context, Result};
use boxlite::{BoxStatus, BoxliteOptions, BoxliteRuntime};

use crate::{attach, client, config, naming};

/// `std::process::exit` truncates its argument to the low 8 bits on Unix, so
/// any negative code must be normalized before reaching it. Negative codes
/// are never a genuine POSIX exit status here — they're either
/// `proto::EXIT_CODE_SESSION_FAILED` (`i32::MIN`) or a signal-killed guest's
/// `-signal` (see that constant's doc) — and truncation would otherwise pick
/// an arbitrary, misleading byte for them: `i32::MIN & 0xff == 0`, which
/// would report a failed session as a clean success. Collapse every negative
/// code to a fixed non-zero status (1) instead; ordinary codes (0, 1, 42, ...)
/// pass through unchanged. The stderr explanation for *why* a session failed
/// is printed upstream, in `client::proxy_loop`, before this code is ever
/// returned — this function only has to guarantee the process's own exit
/// status reflects a failure too.
fn exit_status(code: i32) -> i32 {
    if code < 0 { 1 } else { code }
}

pub async fn run(name: Option<String>, cmd: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read current directory")?;
    let resolved = naming::resolve(name.as_deref(), &cwd);
    let home = config::box_home(&resolved.name);
    let cmd = if cmd.is_empty() {
        vec!["claude".to_string(), "--continue".to_string()]
    } else {
        cmd
    };

    match client::route(&home).await? {
        client::Route::Socket(stream) => {
            let code = client::proxy(stream, &cmd).await?;
            if code != 0 {
                std::process::exit(exit_status(code));
            }
            Ok(())
        }
        client::Route::OwnRuntime => {
            let runtime = BoxliteRuntime::new(BoxliteOptions {
                home_dir: home,
                image_registries: vec![],
            })
            .context("failed to open the BoxLite runtime")?;

            let litebox = runtime
                .get(&resolved.name)
                .await?
                .with_context(|| format!("no box named {}", resolved.name))?;

            // No control socket means whatever `cbox up` created this box
            // is gone. With `detach: false` now the default, that's the
            // common case, not a crash: closing that session's terminal let
            // the watchdog stop the VM. `litebox.exec()` below would start a
            // Stopped box implicitly and silently (boxlite's own doc on
            // `start()`: "Also called implicitly by exec() if the box is
            // not running") -- eating a real ~2.2s cold boot with no
            // explanation, which reads as a hang. Start it explicitly here
            // instead, after saying so, rather than starting it silently or
            // refusing outright and just pushing the same wait onto a
            // required `cbox up` first.
            let status = litebox.info().status;
            if status != BoxStatus::Running {
                eprintln!(
                    "cbox: {} is {status}; starting it (cold boot, not a resume -- ~2s)...",
                    resolved.name
                );
                litebox.start().await.with_context(|| {
                    format!("failed to start {} (was {status})", resolved.name)
                })?;
            }

            let code = attach::attach(&litebox, &cmd).await?;
            if code != 0 {
                std::process::exit(exit_status(code));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::exit_status;

    #[test]
    fn ordinary_codes_pass_through_unchanged() {
        assert_eq!(exit_status(0), 0);
        assert_eq!(exit_status(1), 1);
        assert_eq!(exit_status(42), 42);
    }

    #[test]
    fn the_session_failed_sentinel_becomes_a_real_nonzero_status() {
        // i32::MIN & 0xff == 0 -- the exact bug this function exists to fix.
        assert_ne!(exit_status(crate::proto::EXIT_CODE_SESSION_FAILED), 0);
    }

    #[test]
    fn a_signal_derived_negative_code_becomes_a_real_nonzero_status() {
        assert_ne!(exit_status(-9), 0);
        assert_ne!(exit_status(-1), 0);
    }
}

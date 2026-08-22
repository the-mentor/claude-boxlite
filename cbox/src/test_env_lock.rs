//! A process-wide lock for tests that mutate environment variables.
//!
//! `std::env::set_var`/`remove_var` touch process-global state. `cargo test`
//! runs tests in parallel threads by default, so any two env-mutating tests
//! race against each other regardless of which variables they each touch —
//! `secrets.rs`, `naming.rs`, and `env.rs` don't collide on *names* today,
//! but nothing enforces that other than convention. Every test in this
//! crate that touches the environment acquires this one shared lock before
//! doing so, which serializes them against each other under the default
//! parallel runner instead of relying on `--test-threads=1`.

use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the process-wide environment lock, recovering from poisoning.
///
/// A prior test panicking while holding this lock must not cascade into
/// every other environment-mutating test failing to acquire it — one broken
/// test should not take down the rest of the suite.
pub fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Snapshot-and-restore guard for a single env var used in tests, panic-safe
/// via `Drop`. Captures whatever the var held (set or unset) when created and
/// puts it back exactly on drop — including when the test body panics before
/// reaching its own cleanup, so one failing test can't leak env state into
/// whichever test runs after it.
///
/// Callers must hold `lock()` for the guard's whole lifetime; this only
/// handles restoration, not cross-test serialization.
pub struct EnvVarGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvVarGuard {
    /// Snapshot the current value, then set the var to `value`.
    pub fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }

    /// Snapshot the current value, then remove the var.
    pub fn remove(key: &'static str) -> Self {
        let original = std::env::var(key).ok();
        unsafe { std::env::remove_var(key) };
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

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

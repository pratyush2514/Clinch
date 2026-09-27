//! Shared helpers for the `clinch-desktop` integration suites.
//!
//! Moved out of `src/` so the main source stays test-free. Each suite
//! that needs these declares `mod common;`.
//!
//! One copy compiles per test binary and each binary uses a different
//! subset, so unused-in-this-binary helpers are expected, not dead code.
//! The allow keeps `cargo check` output free of that per-binary noise.
#![allow(dead_code)]

pub mod test_support;

use std::sync::Mutex;

/// Serializes every holder of [`ChromiumEnvGuard`].
///
/// `CLINCH_CHROMIUM_PATH` is process-wide, and several tests point it at
/// a nonexistent executable to prove a launch fails closed. Each
/// integration suite is its own test binary (and process), so this lock
/// serializes the threads inside one suite; suites in other binaries
/// cannot observe each other's env vars.
static CHROMIUM_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Points `CLINCH_CHROMIUM_PATH` at a nonexistent binary for the guard's
/// lifetime, restoring the prior value on drop.
///
/// Holds [`CHROMIUM_ENV_LOCK`] for its whole lifetime, so exactly one
/// test at a time observes the bogus path.
pub struct ChromiumEnvGuard {
    prior: Option<std::ffi::OsString>,
    /// Poisoning is irrelevant here: the lock protects an env var, not an
    /// invariant a panicking test could corrupt.
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[allow(unsafe_code)]
impl ChromiumEnvGuard {
    pub fn hold_bogus() -> Self {
        let lock = CHROMIUM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = std::env::var_os("CLINCH_CHROMIUM_PATH");
        // Edition 2024 marks env mutation unsafe (process-wide). Sound
        // because the lock above makes this the only live mutator, and
        // every browser-launching fixture either holds this guard or is
        // `#[ignore]`d.
        unsafe {
            std::env::set_var("CLINCH_CHROMIUM_PATH", "nonexistent-chromium-hermetic-test");
        }
        Self { prior, _lock: lock }
    }
}

#[allow(unsafe_code)]
impl Drop for ChromiumEnvGuard {
    fn drop(&mut self) {
        if let Some(prior) = self.prior.take() {
            unsafe {
                std::env::set_var("CLINCH_CHROMIUM_PATH", prior);
            }
        } else {
            unsafe {
                std::env::remove_var("CLINCH_CHROMIUM_PATH");
            }
        }
    }
}

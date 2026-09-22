//! Hermetic CDP test harness: scriptable fake servers and clients for tests
//! that must prove CDP conversations without a Chromium binary.
//!
//! Test-only in practice (no production path constructs it), but
//! intentionally ungated so downstream integration tests can drive scripted
//! CDP traffic against it.
pub mod fake_cdp;

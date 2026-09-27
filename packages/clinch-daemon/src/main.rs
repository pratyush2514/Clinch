//! `clinch-daemon` binary entrypoint.
//!
//! All logic lives in the library; `main` only boots the Tokio runtime and
//! delegates to [`clinch_daemon::run`].

#![deny(unsafe_code)]

#[tokio::main]
async fn main() {
    clinch_daemon::run().await;
}

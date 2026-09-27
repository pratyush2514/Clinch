//! Integration tests for `clinch_desktop::auth`.
//!
//! Moved out of `src/auth.rs` so the main source stays test-free.

use clinch_desktop::auth::*;
use url::Url;

#[test]
fn signals_map_to_panel_reasons() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        reason_for_signal(&browser_driver::AuthSignal::Authenticated),
        None
    );
    assert_eq!(
        reason_for_signal(&browser_driver::AuthSignal::LoginRedirect {
            url: "https://x/login".into()
        }),
        Some(ReauthReason::LoginRedirect)
    );
    assert_eq!(
        reason_for_signal(&browser_driver::AuthSignal::SsoChallenge {
            url: "https://auth.openai.com/authorize".into()
        }),
        Some(ReauthReason::SsoChallenge)
    );
    assert_eq!(
        reason_for_signal(&browser_driver::AuthSignal::OriginMismatch {
            current: "https://y/".into()
        }),
        Some(ReauthReason::OriginMismatch)
    );
    let panel = AuthPanel::new(
        &Url::parse("https://portal.example.com/")?,
        ReauthReason::Manual,
    );
    assert!(serde_json::to_string(&panel)?.contains("portal.example.com"));
    Ok(())
}

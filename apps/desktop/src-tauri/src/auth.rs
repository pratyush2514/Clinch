#![deny(unsafe_code)]
//! Embedded auth-panel state: in-app re-authentication without an external
//! OS browser window.
//!
//! When CDP classifies a portal landing as [`AuthSignal::LoginRedirect`],
//! [`AuthSignal::SsoChallenge`], or [`AuthSignal::OriginMismatch`], the
//! desktop layer raises this panel instead
//! of launching the user's daily browser. The user completes login/2FA in the
//! app-owned managed Chromium window (surfaced in-app via the mirrored
//! viewport); completing the panel verifies the origin, records a WAL
//! `session_events` row, and closes automatically. Raw cookie values are never
//! stored — only outcome metadata, matching the existing session-sync contract.

use serde::Serialize;
use url::Url;

/// Why the embedded panel was raised. Serialized for the React overlay.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReauthReason {
    /// Same origin but the path looks like `/login` / 2FA.
    LoginRedirect,
    /// Landed on a known identity-provider origin mid-SSO. Still foreign,
    /// still unconnected — but expected, not a hijack signal. Completing the
    /// panel here keeps failing closed until the provider redirects back.
    SsoChallenge,
    /// The portal bounced to a foreign origin (expired session, unknown hop).
    OriginMismatch,
    /// Opened proactively by the user from the React workspace.
    Manual,
}

/// DTO pushed to the React overlay; no secrets, only the portal address.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPanel {
    pub portal: String,
    pub reason: ReauthReason,
}

impl AuthPanel {
    #[must_use]
    pub fn new(portal: &Url, reason: ReauthReason) -> Self {
        Self {
            portal: portal.as_str().to_owned(),
            reason,
        }
    }
}

/// Map a [`browser_driver::AuthSignal`] onto a panel reason.
#[must_use]
pub fn reason_for_signal(signal: &browser_driver::AuthSignal) -> Option<ReauthReason> {
    match signal {
        browser_driver::AuthSignal::Authenticated => None,
        browser_driver::AuthSignal::LoginRedirect { .. } => Some(ReauthReason::LoginRedirect),
        browser_driver::AuthSignal::SsoChallenge { .. } => Some(ReauthReason::SsoChallenge),
        browser_driver::AuthSignal::OriginMismatch { .. } => Some(ReauthReason::OriginMismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

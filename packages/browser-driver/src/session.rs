#![deny(unsafe_code)]
//! Zero-touch session establishment: CDP cookie injection prior to navigation
//! plus explicit expired-session signalling for the embedded auth fallback.
//!
//! Secrets never leave this module's call path: cookies are injected straight
//! into the CDP target and only counts/origins are reported outward.

use crate::{BrowserError, IO_TIMEOUT, ManagedBrowser};
use session_sync::Cookie;
use url::Url;

/// Machine-readable result of opening a portal after cookie injection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthSignal {
    /// Same-origin landing page with no login markers.
    Authenticated,
    /// Same origin but the path looks like a login/2FA challenge.
    LoginRedirect { url: String },
    /// The portal bounced to a different origin (SSO / expired session).
    OriginMismatch { current: String },
}

impl AuthSignal {
    #[must_use]
    pub fn requires_embedded_auth(&self) -> bool {
        !matches!(self, Self::Authenticated)
    }
}

/// Classify a post-navigation URL without fetching page content.
///
/// Markers match whole path segments (`/login`, `/signin`, `/2fa`,
/// `/challenge`, `/verify`, `/auth`, …) so billing slugs like
/// `/verify-invoice-status` stay `Authenticated`. Matching is
/// case-insensitive on the path only.
#[must_use]
pub fn detect_auth_signal(current: &Url, portal: &Url) -> AuthSignal {
    if current.origin() != portal.origin() {
        return AuthSignal::OriginMismatch {
            current: current.as_str().to_owned(),
        };
    }
    let challenge = current.path_segments().is_some_and(|segments| {
        segments.into_iter().any(|segment| {
            matches!(
                segment.to_ascii_lowercase().as_str(),
                "login"
                    | "log-in"
                    | "signin"
                    | "sign-in"
                    | "2fa"
                    | "challenge"
                    | "verify"
                    | "auth"
                    | "reauth"
                    | "sso"
            )
        })
    });
    if challenge {
        AuthSignal::LoginRedirect {
            url: current.as_str().to_owned(),
        }
    } else {
        AuthSignal::Authenticated
    }
}

impl ManagedBrowser {
    /// Inject a prepared cookie set, open the portal, and classify the landing.
    ///
    /// This is the single headless-first entry point used before every macro
    /// navigation: injection and navigation stay paired so a replay never
    /// runs against an unauthenticated blank tab.
    ///
    /// # Errors
    /// Returns [`BrowserError`] when injection or navigation fails. An
    /// expired session is *not* an error — it returns
    /// [`AuthSignal::LoginRedirect`] / [`AuthSignal::OriginMismatch`] so the
    /// desktop layer can raise the embedded auth panel instead of spawning
    /// an external OS browser window.
    pub async fn establish_session(
        &self,
        cookies: &[Cookie],
        portal: &Url,
    ) -> Result<AuthSignal, BrowserError> {
        self.inject(cookies).await?;
        self.navigate(portal).await?;
        let current = self
            .page
            .url()
            .await
            .map_err(|_| BrowserError::Connection)?
            .ok_or(BrowserError::WrongOrigin)?;
        let current = Url::parse(&current).map_err(|_| BrowserError::WrongOrigin)?;
        Ok(detect_auth_signal(&current, portal))
    }

    /// Re-read the live URL and classify it (cheap reauth poll for the panel).
    ///
    /// # Errors
    /// Returns [`BrowserError`] when the CDP target cannot report its URL.
    pub async fn auth_signal(&self, portal: &Url) -> Result<AuthSignal, BrowserError> {
        let current = self
            .page
            .url()
            .await
            .map_err(|_| BrowserError::Connection)?
            .ok_or(BrowserError::WrongOrigin)?;
        let current = Url::parse(&current).map_err(|_| BrowserError::WrongOrigin)?;
        Ok(detect_auth_signal(&current, portal))
    }

    /// Resolve a backend node id to its viewport rectangle via
    /// `DOM.getBoxModel`. This is the bridge between AX identity (which node)
    /// and `SoM` geometry (where to click): no selector is ever constructed.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on CDP failure, timeout, or degenerate boxes.
    pub async fn node_rect(&self, backend_node_id: i64) -> Result<crate::Highlight, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::dom::{BackendNodeId, GetBoxModelParams};
        let params = GetBoxModelParams::builder()
            .backend_node_id(BackendNodeId::new(backend_node_id))
            .build();
        let model = tokio::time::timeout(IO_TIMEOUT, self.page.execute(params))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?
            .result
            .model;
        let points = model.border.inner();
        if points.len() != 8 || !points.iter().all(|value| value.is_finite()) {
            return Err(BrowserError::InvalidAction);
        }
        let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
        let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for pair in points.chunks_exact(2) {
            min_x = min_x.min(pair[0]);
            min_y = min_y.min(pair[1]);
            max_x = max_x.max(pair[0]);
            max_y = max_y.max(pair[1]);
        }
        let (width, height) = (max_x - min_x, max_y - min_y);
        if width <= 0.0 || height <= 0.0 {
            return Err(BrowserError::InvalidAction);
        }
        Ok(crate::Highlight {
            // AX targets carry backend ids, not CSS: the marker keeps the
            // highlight plumbing working without pretending otherwise.
            selector: format!("ax:{backend_node_id}"),
            x: min_x,
            y: min_y,
            width,
            height,
            matches: 1,
        })
    }
    /// Seed extracted `LocalStorage` pairs via `Page.addScriptToEvaluateOnNewDocument`
    ///
    /// The script runs in the portal origin before every document load, so it
    /// must be registered *before* navigating to the portal. An empty item
    /// list is a validated no-op that performs no CDP call. The script only
    /// fills missing keys, so live portal state is never overwritten.
    ///
    /// # Errors
    /// Returns [`BrowserError`] when CDP registration fails.
    pub async fn hydrate_local_storage(
        &self,
        items: &[session_sync::StorageItem],
    ) -> Result<(), BrowserError> {
        use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
        let Some(script) = session_sync::build_hydration_script(items) else {
            return Ok(());
        };
        let mut params = AddScriptToEvaluateOnNewDocumentParams::new(script);
        params.run_immediately = Some(true);
        tokio::time::timeout(IO_TIMEOUT, self.page.execute(params))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }

    /// Mirror the source browser's identity via `Network.setUserAgentOverride`.
    ///
    /// Cookie-only sync under a mismatched UA is a classic anti-bot signal, so
    /// the override is applied before the first portal navigation. The
    /// platform token is derived from the UA itself (`Windows NT` →
    /// `Windows`, otherwise `macOS`) to keep `navigator.platform` consistent.
    /// An empty or oversized UA fails closed without touching the target.
    ///
    /// # Errors
    /// Returns [`BrowserError::InvalidAction`] for malformed input and
    /// [`BrowserError`] variants for CDP failures.
    pub async fn mirror_user_agent(&self, user_agent: &str) -> Result<(), BrowserError> {
        use chromiumoxide::cdp::browser_protocol::network::SetUserAgentOverrideParams;
        if user_agent.is_empty()
            || user_agent.len() > 512
            || !user_agent.bytes().all(|byte| (0x20..=0x7E).contains(&byte))
        {
            return Err(BrowserError::InvalidAction);
        }
        let platform = if user_agent.contains("Windows NT") {
            "Windows"
        } else if user_agent.contains("Macintosh") {
            "macOS"
        } else {
            "Linux"
        };
        let params = SetUserAgentOverrideParams::builder()
            .user_agent(user_agent)
            .platform(platform)
            .build()
            .map_err(|_| BrowserError::InvalidAction)?;
        tokio::time::timeout(IO_TIMEOUT, self.page.execute(params))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Result<Url, url::ParseError> {
        Url::parse(value)
    }

    #[test]
    fn classifies_authenticated_login_and_origin_signals() -> Result<(), Box<dyn std::error::Error>>
    {
        let portal = url("https://billing.example.com/overview")?;
        assert_eq!(
            detect_auth_signal(&url("https://billing.example.com/invoices")?, &portal),
            AuthSignal::Authenticated
        );
        assert_eq!(
            detect_auth_signal(&url("https://billing.example.com/login?next=/")?, &portal),
            AuthSignal::LoginRedirect {
                url: "https://billing.example.com/login?next=/".into()
            }
        );
        assert_eq!(
            detect_auth_signal(&url("https://billing.example.com/2fa/challenge")?, &portal),
            AuthSignal::LoginRedirect {
                url: "https://billing.example.com/2fa/challenge".into()
            }
        );
        assert!(
            detect_auth_signal(&url("https://sso.example.net/login")?, &portal)
                .requires_embedded_auth()
        );
        assert!(
            !detect_auth_signal(&url("https://billing.example.com/invoices")?, &portal)
                .requires_embedded_auth()
        );
        Ok(())
    }

    #[test]
    fn billing_paths_are_not_login_markers() -> Result<(), Box<dyn std::error::Error>> {
        let portal = url("https://billing.example.com/")?;
        // Hyphenated billing slugs must not trip the challenge detector.
        for path in [
            "/invoices",
            "/billing/history",
            "/account",
            "/verify-invoice-status",
        ] {
            assert_eq!(
                detect_auth_signal(
                    &url(&format!("https://billing.example.com{path}"))?,
                    &portal
                ),
                AuthSignal::Authenticated,
                "{path} must stay authenticated"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod browser_tests {
    #[tokio::test]
    #[ignore = "Requires CLINCH_CHROMIUM_PATH and launches a real browser using a temporary profile"]
    async fn storage_hydration_registers_document_script() -> Result<(), Box<dyn std::error::Error>>
    {
        let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
        let profile = tempfile::tempdir()?;
        let browser =
            crate::ManagedBrowser::launch(std::path::Path::new(&executable), profile.path())
                .await?;
        // Exercises the `Page.addScriptToEvaluateOnNewDocument` path with a
        // validated additive-only script; the empty call proves the no-op
        // short-circuits before any CDP traffic.
        browser
            .hydrate_local_storage(&[session_sync::StorageItem {
                key: "fixture-session".into(),
                value: "fixture-value".into(),
            }])
            .await?;
        browser.hydrate_local_storage(&[]).await?;
        browser.shutdown().await?;
        profile.close()?;
        Ok(())
    }
}

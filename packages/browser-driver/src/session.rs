#![deny(unsafe_code)]
//! Zero-touch session establishment: CDP cookie injection prior to navigation
//! plus explicit expired-session signalling for the embedded auth fallback.
//!
//! Secrets never leave this module's call path: cookies are injected straight
//! into the CDP target and only counts/origins are reported outward.

use crate::{BrowserError, IO_TIMEOUT, ManagedBrowser};
use url::Url;

/// Machine-readable result of opening a portal after cookie injection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthSignal {
    /// Same-origin landing page with no login markers.
    Authenticated,
    /// Same origin but the path looks like a login/2FA challenge.
    LoginRedirect { url: String },
    /// The portal bounced to a different origin (expired session, unknown hop).
    OriginMismatch { current: String },
    /// The portal bounced to a known identity-provider origin (curated
    /// cross-root secondary or strict subdomain of the portal). Still foreign,
    /// still unconnected — but expected mid-SSO, not a hijack signal.
    SsoChallenge { url: String },
}

impl AuthSignal {
    #[must_use]
    pub fn requires_embedded_auth(&self) -> bool {
        !matches!(self, Self::Authenticated)
    }
}

/// Whether `candidate` is a known identity provider for `portal`: a curated
/// cross-root secondary (or its subdomain), or a strict subdomain of the
/// portal host itself. Same-parent siblings are deliberately excluded:
/// without a public-suffix list they are indistinguishable from unrelated
/// sites sharing a suffix.
fn is_known_sso(candidate: &str, portal: &str) -> bool {
    let candidate = candidate.trim_end_matches('.').to_ascii_lowercase();
    let portal = portal.trim_end_matches('.').to_ascii_lowercase();
    if candidate.is_empty() || portal.is_empty() {
        return false;
    }
    if candidate == portal || candidate.ends_with(&format!(".{portal}")) {
        return true;
    }
    session_sync::sso_secondaries(&portal)
        .iter()
        .any(|root| candidate == *root || candidate.ends_with(&format!(".{root}")))
        || session_sync::sso_secondaries(&candidate)
            .iter()
            .any(|root| portal == *root || portal.ends_with(&format!(".{root}")))
}

/// Classify a post-navigation URL without fetching page content.
///
/// Markers match whole path segments (`/login`, `/signin`, `/2fa`,
/// `/challenge`, `/verify`, `/auth`, …) so hyphenated slugs like
/// `/verify-file-status` stay `Authenticated`. Matching is
/// case-insensitive on the path only.
#[must_use]
pub fn detect_auth_signal(current: &Url, portal: &Url) -> AuthSignal {
    if current.origin() != portal.origin() {
        let url = current.as_str().to_owned();
        let sso = match (current.host_str(), portal.host_str()) {
            (Some(current_host), Some(portal_host)) => is_known_sso(current_host, portal_host),
            _ => false,
        };
        return if sso {
            AuthSignal::SsoChallenge { url }
        } else {
            AuthSignal::OriginMismatch { current: url }
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

    /// Opt-in DOM fallback for empty container text. When
    /// `CLINCH_DOM_CONTAINER_FALLBACK` is `1`/`true`, elements whose AX-graph
    /// rollup came back empty get one bounded `innerText` read from their
    /// nearest structural ancestor; otherwise this is a silent no-op and the
    /// default snapshot path is untouched.
    ///
    /// Mapping is per-element via `DOM.resolveNode` on the AX backend id —
    /// never index alignment between DOM order and AX order, which differ
    /// because the AX tree filters ignored and non-interactive nodes. The
    /// closest-selector uses structural tags and ARIA roles only
    /// (`tr`, `[role="row"]`, `li`, `article`, `fieldset`): no classes, no
    /// ids, no generic containers. Every element failure is skipped
    /// individually, so the snapshot never fails because of enrichment.
    pub async fn enrich_empty_containers(&self, elements: &mut [crate::AxElement]) {
        if !dom_fallback_enabled() {
            return;
        }
        let mut enriched = 0;
        for element in elements.iter_mut() {
            if !element.container_text.is_empty() {
                continue;
            }
            if enriched >= MAX_DOM_FALLBACK_TARGETS {
                break;
            }
            enriched += 1;
            if let Some(text) = self.dom_container_inner_text(element.backend_node_id).await
                && !text.trim().is_empty()
            {
                let items = crate::a11y::dom_container_items(&text);
                if !items.is_empty() {
                    element.container_text = items;
                }
            }
        }
    }

    /// Read one element's nearest structural ancestor text: resolve the AX
    /// backend id to a JS handle, then read `closest(...).innerText` capped
    /// page-side at 2,000 characters. Returns `None` on any CDP failure,
    /// missing handle, or empty text.
    async fn dom_container_inner_text(&self, backend_node_id: i64) -> Option<String> {
        use chromiumoxide::cdp::{
            browser_protocol::dom::{BackendNodeId, ResolveNodeParams},
            js_protocol::runtime::CallFunctionOnParams,
        };
        let resolve = ResolveNodeParams::builder()
            .backend_node_id(BackendNodeId::new(backend_node_id))
            .build();
        let object_id = tokio::time::timeout(IO_TIMEOUT, self.page.execute(resolve))
            .await
            .ok()?
            .map_err(|_| BrowserError::Connection)
            .ok()?
            .result
            .object
            .object_id?;
        let call = CallFunctionOnParams::builder()
            .function_declaration(DOM_CLOSEST_EXPRESSION)
            .object_id(object_id)
            .return_by_value(true)
            .silent(true)
            .build()
            .ok()?;
        let result = tokio::time::timeout(IO_TIMEOUT, self.page.execute(call))
            .await
            .ok()?
            .map_err(|_| BrowserError::Connection)
            .ok()?
            .result;
        if result.exception_details.is_some() {
            return None;
        }
        result.result.value?.as_str().map(str::to_owned)
    }
}

/// Env gate for the DOM container fallback: `CLINCH_DOM_CONTAINER_FALLBACK`
/// set to `1` or `true` (any case). Anything else — including unset — keeps
/// the pure AX-graph path.
fn dom_fallback_enabled() -> bool {
    std::env::var("CLINCH_DOM_CONTAINER_FALLBACK")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Upper bound on elements enriched per snapshot: each costs up to two CDP
/// round-trips, so the fallback stays a bounded post-pass, never a second
/// snapshot.
const MAX_DOM_FALLBACK_TARGETS: usize = 24;
/// Side-effect-free read of the nearest structural ancestor's rendered
/// text, capped page-side at 2,000 characters before shipping over CDP
/// (`dom_container_items` bounds it further into 12×100 snippets).
/// Structural tags and the ARIA row role only — deliberately no classes,
/// ids, or generic `div`/`span` containers.
const DOM_CLOSEST_EXPRESSION: &str = "function(){var el=this;if(!el||!el.closest){return '';}\
    var c=el.closest('tr,[role=\"row\"],li,article,fieldset');\
    if(!c){return '';}\
    return (c.innerText||'').slice(0,2000);}";

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Result<Url, url::ParseError> {
        Url::parse(value)
    }

    #[test]
    fn classifies_authenticated_login_and_origin_signals() -> Result<(), Box<dyn std::error::Error>>
    {
        let portal = url("https://portal.example.com/overview")?;
        assert_eq!(
            detect_auth_signal(&url("https://portal.example.com/files")?, &portal),
            AuthSignal::Authenticated
        );
        assert_eq!(
            detect_auth_signal(&url("https://portal.example.com/login?next=/")?, &portal),
            AuthSignal::LoginRedirect {
                url: "https://portal.example.com/login?next=/".into()
            }
        );
        assert_eq!(
            detect_auth_signal(&url("https://portal.example.com/2fa/challenge")?, &portal),
            AuthSignal::LoginRedirect {
                url: "https://portal.example.com/2fa/challenge".into()
            }
        );
        assert!(
            detect_auth_signal(&url("https://sso.example.net/login")?, &portal)
                .requires_embedded_auth()
        );
        assert!(
            !detect_auth_signal(&url("https://portal.example.com/files")?, &portal)
                .requires_embedded_auth()
        );
        Ok(())
    }

    #[test]
    fn hyphenated_paths_are_not_login_markers() -> Result<(), Box<dyn std::error::Error>> {
        let portal = url("https://portal.example.com/")?;
        // Hyphenated slugs must not trip the challenge detector.
        for path in [
            "/files",
            "/files/history",
            "/account",
            "/verify-file-status",
        ] {
            assert_eq!(
                detect_auth_signal(&url(&format!("https://portal.example.com{path}"))?, &portal),
                AuthSignal::Authenticated,
                "{path} must stay authenticated"
            );
        }
        Ok(())
    }

    #[test]
    fn identity_provider_hops_are_sso_not_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let portal = url("https://chatgpt.com/")?;
        // Curated cross-root IdP: expected mid-SSO, still unconnected.
        let signal = detect_auth_signal(&url("https://auth.openai.com/authorize")?, &portal);
        assert_eq!(
            signal,
            AuthSignal::SsoChallenge {
                url: "https://auth.openai.com/authorize".into()
            }
        );
        assert!(signal.requires_embedded_auth());
        // Strict subdomain of the portal: same treatment.
        let portal = url("https://claude.ai/")?;
        assert!(matches!(
            detect_auth_signal(&url("https://auth.claude.ai/login")?, &portal),
            AuthSignal::SsoChallenge { .. }
        ));
        // Unknown foreign origins stay mismatches, never challenges.
        assert!(matches!(
            detect_auth_signal(&url("https://sso.evil.com/login")?, &portal),
            AuthSignal::OriginMismatch { .. }
        ));
        // Same-parent siblings are NOT trusted without a public-suffix list.
        let portal = url("https://app.example.com/")?;
        assert!(matches!(
            detect_auth_signal(&url("https://auth.example.com/login")?, &portal),
            AuthSignal::OriginMismatch { .. }
        ));
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

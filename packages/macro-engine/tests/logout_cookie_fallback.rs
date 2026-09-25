//! `LogOut` cookie-clear fallback: after both UI gears miss, the engine
//! clears the managed profile's session cookies for the live page's
//! registrable domain and lets the verifier decide.
//!
//! The fake below presents a page with no actionable chrome (gear 1
//! misses deterministically), skips gear 2 (`navigator: None`), and
//! scripts the cookie-clear seam plus the auth probe — so the fallback
//! ordering (UI → verify → clear → verify → COMPLETED|miss) is proven
//! with no Chromium. All hosts are RFC-reserved `example.com` shapes;
//! never a real site's.
//!
//! Hermetic classifier: pinned at a dead loopback port like the other
//! worker-flow suites (see [`hermetic_classifier`]).
#![allow(unsafe_code)]

use browser_driver::{AuthState, AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::{
    ChromeActionBrowser, IntentError, MenuBrowser, PageGoalOutcome, SettingsBrowser, VerbKind,
    VerbSpec, pursue_verb_goal,
};
use tokio::sync::Mutex;
use url::Url;

fn hermetic_classifier() {
    // SAFETY: process-global, but every test in this binary writes the
    // same constant value and none depends on the live endpoint.
    unsafe {
        std::env::set_var("CLINCH_CLASSIFIER_BASE_URL", "http://127.0.0.1:9/");
    }
}

fn example_origin() -> Url {
    Url::parse("https://www.example.com/")
        .unwrap_or_else(|error| panic!("test origin parses: {error}"))
}

fn parse_url(raw: &str) -> Url {
    Url::parse(raw).unwrap_or_else(|error| panic!("test url parses: {error}"))
}

/// Fake behind [`pursue_verb_goal`]: an empty page (no menu candidates,
/// so gear 1 misses), a scripted auth probe, and a recording
/// cookie-clear seam that flips the probe to signed-out when configured
/// — like a real session end would.
struct FallbackBrowser {
    url: Mutex<Option<Url>>,
    auth: Mutex<AuthState>,
    cleared_for: Mutex<Vec<String>>,
    flip_to_signed_out_on_clear: bool,
}

impl FallbackBrowser {
    fn new(page_url: Option<Url>, flip_to_signed_out_on_clear: bool) -> Self {
        Self {
            url: Mutex::new(page_url),
            auth: Mutex::new(AuthState::Authenticated),
            cleared_for: Mutex::new(Vec::new()),
            flip_to_signed_out_on_clear,
        }
    }

    async fn cleared_hosts(&self) -> Vec<String> {
        self.cleared_for.lock().await.clone()
    }
}

impl MenuBrowser for FallbackBrowser {
    fn menu_viewport_size(&self) -> impl std::future::Future<Output = Option<(f64, f64)>> + Send {
        std::future::ready(Some((1200.0, 800.0)))
    }

    fn menu_node_rect(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Result<Highlight, BrowserError>> + Send {
        std::future::ready(Ok(Highlight {
            selector: format!("ax:{backend_node_id}"),
            x: 0.0,
            y: 0.0,
            width: 40.0,
            height: 40.0,
            matches: 1,
        }))
    }

    fn menu_snapshot(
        &self,
        _origin: &Url,
    ) -> impl std::future::Future<Output = (Vec<AxElement>, AxResyncCheck, u64)> + Send {
        std::future::ready((Vec::new(), AxResyncCheck::new(10, None), 0))
    }

    fn menu_node_expanded(
        &self,
        _backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<bool>> + Send {
        std::future::ready(None)
    }

    fn menu_click(
        &self,
        _element: &AxElement,
    ) -> impl std::future::Future<Output = Result<(), IntentError>> + Send {
        std::future::ready(Ok(()))
    }

    fn menu_dismiss(&self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }

    fn menu_screenshot(&self) -> impl std::future::Future<Output = Option<String>> + Send {
        std::future::ready(None)
    }
}

impl SettingsBrowser for FallbackBrowser {
    async fn settings_current_url(&self) -> Option<Url> {
        self.url.lock().await.clone()
    }

    fn settings_node_href(
        &self,
        _backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        std::future::ready(None)
    }

    fn settings_page_title(&self) -> impl std::future::Future<Output = Option<String>> + Send {
        std::future::ready(None)
    }
}

impl ChromeActionBrowser for FallbackBrowser {
    async fn chrome_auth_state(&self) -> AuthState {
        *self.auth.lock().await
    }

    async fn chrome_navigate(&self, url: &Url) -> Result<(), IntentError> {
        *self.url.lock().await = Some(url.clone());
        Ok(())
    }

    async fn chrome_clear_host_cookies(&self, host: &str) -> Result<usize, IntentError> {
        self.cleared_for.lock().await.push(host.to_string());
        if self.flip_to_signed_out_on_clear {
            *self.auth.lock().await = AuthState::LoggedOut;
        }
        Ok(3)
    }
}

async fn pursue_logout(
    browser: &FallbackBrowser,
    origin: &Url,
) -> Result<PageGoalOutcome, IntentError> {
    pursue_verb_goal(
        browser,
        origin,
        VerbSpec::for_kind(VerbKind::LogOut),
        None,
        None,
    )
    .await
}

#[tokio::test]
async fn logout_fallback_clears_registrable_domain_and_completes() {
    hermetic_classifier();
    let origin = example_origin();
    let browser = FallbackBrowser::new(Some(origin.clone()), true);
    match pursue_logout(&browser, &origin).await {
        Ok(PageGoalOutcome::Verified { label, .. }) => {
            assert_eq!(label, "session cookies cleared");
        }
        other => panic!("expected Verified, got {other:?}"),
    }
    // `www.example.com` clears as its registrable domain, never the full host.
    assert_eq!(
        browser.cleared_hosts().await,
        vec!["example.com".to_string()]
    );
}

#[tokio::test]
async fn logout_fallback_verifier_still_failing_is_honest_miss() {
    hermetic_classifier();
    let origin = example_origin();
    // The clear does not end the session: the probe keeps reading
    // authenticated, so the verifier must refuse COMPLETED.
    let browser = FallbackBrowser::new(Some(origin.clone()), false);
    let Err(IntentError::NoMatch(diagnostic)) = pursue_logout(&browser, &origin).await else {
        panic!("expected an honest miss");
    };
    assert!(
        diagnostic.contains(
            "log_out: UI path missed; cleared 3 session cookies for example.com; verifier: still-unknown"
        ),
        "diagnostic was: {diagnostic}"
    );
}

#[tokio::test]
async fn cookie_fallback_does_not_run_for_other_verbs() {
    hermetic_classifier();
    let origin = example_origin();
    let browser = FallbackBrowser::new(Some(origin.clone()), true);
    let outcome = pursue_verb_goal(
        &browser,
        &origin,
        VerbSpec::for_kind(VerbKind::Settings),
        None,
        None,
    )
    .await;
    assert!(
        matches!(outcome, Err(IntentError::NoMatch(_))),
        "settings UI miss must stay a miss, got {outcome:?}"
    );
    assert!(
        browser.cleared_hosts().await.is_empty(),
        "the cookie fallback is LogOut-only"
    );
}

#[tokio::test]
async fn logout_fallback_derives_registrable_domain_from_live_page_url() {
    hermetic_classifier();
    let page = parse_url("https://app.example.com/dashboard");
    let browser = FallbackBrowser::new(Some(page.clone()), true);
    let outcome = pursue_logout(&browser, &page).await;
    assert!(
        matches!(outcome, Ok(PageGoalOutcome::Verified { .. })),
        "expected Verified, got {outcome:?}"
    );
    // Multi-level subdomain still clears as the registrable domain.
    assert_eq!(
        browser.cleared_hosts().await,
        vec!["example.com".to_string()]
    );
}

#[tokio::test]
async fn logout_fallback_skips_gracefully_on_unusable_host() {
    hermetic_classifier();
    let origin = parse_url("about:blank");
    let browser = FallbackBrowser::new(None, true);
    let Err(IntentError::NoMatch(diagnostic)) = pursue_logout(&browser, &origin).await else {
        panic!("expected an honest miss");
    };
    assert!(
        diagnostic.contains("cookie-clear fallback skipped"),
        "diagnostic was: {diagnostic}"
    );
    assert!(
        browser.cleared_hosts().await.is_empty(),
        "nothing may be cleared without a usable host"
    );
}

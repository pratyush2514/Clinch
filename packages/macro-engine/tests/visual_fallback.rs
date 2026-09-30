//! Visual-grounding fallback of the sign-out lane: when an identity menu is
//! open but no control itself names the verb, one screenshot goes to a
//! vision model, Rust validates the proposed 0–1000 point, the existing
//! trusted click fires, and the existing signed-out verifier decides.
//!
//! Hermetic: a scripted fake browser and a scripted navigator — no live
//! model, no Chromium, no site-specific fixtures (RFC-reserved
//! `example.com` only).
#![allow(unsafe_code)]

use browser_driver::{AuthState, AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::{
    ChromeActionBrowser, IntentError, MenuBrowser, PageAction, PageGoalOutcome, PageNavigator,
    SettingsBrowser, VerbKind, VerbSpec, VisualLocation, pursue_chrome_action_with_vision,
    visual_point_to_pixels, visual_target_description,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;
use url::Url;

fn hermetic_classifier() {
    // SAFETY: process-global, every test in this binary writes the same
    // constant value and none depends on the live endpoint.
    unsafe {
        std::env::set_var("CLINCH_CLASSIFIER_BASE_URL", "http://127.0.0.1:9/");
    }
}

fn origin() -> Url {
    Url::parse("https://www.example.com/")
        .unwrap_or_else(|error| panic!("test origin parses: {error}"))
}

fn el(id: i64, role: &str, name: &str) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_owned(),
        name: name.to_owned(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

fn log_out_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::LogOut)
}

// ---- pure conversion ----

#[test]
fn point_converts_from_thousand_space_to_viewport_pixels() {
    assert_eq!(
        visual_point_to_pixels(500.0, 500.0, (1200.0, 800.0)),
        Some((600.0, 400.0))
    );
    assert_eq!(
        visual_point_to_pixels(0.0, 1000.0, (1200.0, 800.0)),
        Some((0.0, 800.0))
    );
}

#[test]
fn out_of_range_or_degenerate_points_are_rejected() {
    let viewport = (1200.0, 800.0);
    assert_eq!(visual_point_to_pixels(-1.0, 10.0, viewport), None);
    assert_eq!(visual_point_to_pixels(10.0, 1000.5, viewport), None);
    assert_eq!(visual_point_to_pixels(f64::NAN, 10.0, viewport), None);
    assert_eq!(visual_point_to_pixels(10.0, f64::INFINITY, viewport), None);
    assert_eq!(visual_point_to_pixels(10.0, 10.0, (0.0, 800.0)), None);
}

#[test]
fn target_description_comes_from_the_verb_vocabulary() {
    let description = visual_target_description(log_out_spec());
    assert!(description.contains("log out"), "got: {description}");
}

// ---- scripted worker flow ----

/// Page whose avatar opens a menu; the menu's items are scripted per test.
/// A coordinate click records the point and can flip the auth probe.
struct VisualBrowser {
    baseline: Vec<AxElement>,
    menu: Vec<AxElement>,
    clicks: Mutex<Vec<i64>>,
    clicks_at: Mutex<Vec<(f64, f64)>>,
    auth: Mutex<AuthState>,
    signs_out_on_click_at: bool,
}

impl VisualBrowser {
    fn new(menu_items: Vec<AxElement>, signs_out_on_click_at: bool) -> Self {
        let baseline = vec![
            el(1, "button", "User Avatar Expand user menu"),
            el(3, "link", "Home"),
        ];
        let mut menu = baseline.clone();
        menu.extend(menu_items);
        Self {
            baseline,
            menu,
            clicks: Mutex::new(Vec::new()),
            clicks_at: Mutex::new(Vec::new()),
            auth: Mutex::new(AuthState::Authenticated),
            signs_out_on_click_at,
        }
    }

    async fn clicks_at(&self) -> Vec<(f64, f64)> {
        self.clicks_at.lock().await.clone()
    }
}

impl MenuBrowser for VisualBrowser {
    fn menu_viewport_size(&self) -> impl std::future::Future<Output = Option<(f64, f64)>> + Send {
        std::future::ready(Some((1200.0, 800.0)))
    }

    fn menu_node_rect(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Result<Highlight, BrowserError>> + Send {
        std::future::ready(Ok(Highlight {
            selector: format!("ax:{backend_node_id}"),
            x: 950.0,
            y: 50.0,
            width: 40.0,
            height: 40.0,
            matches: 1,
        }))
    }

    async fn menu_snapshot(&self, _origin: &Url) -> (Vec<AxElement>, AxResyncCheck, u64) {
        let opened = self.clicks.lock().await.contains(&1);
        let tree = if opened {
            self.menu.clone()
        } else {
            self.baseline.clone()
        };
        (tree, AxResyncCheck::new(10, None), 0)
    }

    fn menu_node_expanded(
        &self,
        _backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<bool>> + Send {
        std::future::ready(None)
    }

    async fn menu_click(&self, element: &AxElement) -> Result<(), IntentError> {
        self.clicks.lock().await.push(element.backend_node_id);
        // A direct "Log out" click ends the session, like a live page.
        if element.name == "Log out" {
            *self.auth.lock().await = AuthState::LoggedOut;
        }
        Ok(())
    }

    fn menu_dismiss(&self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }

    fn menu_screenshot(&self) -> impl std::future::Future<Output = Option<String>> + Send {
        std::future::ready(Some("AAAA".to_owned()))
    }

    async fn menu_click_at(
        &self,
        x: f64,
        y: f64,
        _tried: &mut Vec<String>,
    ) -> Result<(), IntentError> {
        self.clicks_at.lock().await.push((x, y));
        if self.signs_out_on_click_at {
            *self.auth.lock().await = AuthState::LoggedOut;
        }
        Ok(())
    }
}

impl SettingsBrowser for VisualBrowser {
    fn settings_current_url(&self) -> impl std::future::Future<Output = Option<Url>> + Send {
        std::future::ready(Some(origin()))
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

impl ChromeActionBrowser for VisualBrowser {
    async fn chrome_auth_state(&self) -> AuthState {
        *self.auth.lock().await
    }

    async fn chrome_navigate(&self, _url: &Url) -> Result<(), IntentError> {
        Ok(())
    }
}

/// Navigator scripted to a fixed visual answer; counts locate calls.
struct ScriptedVision {
    answer: VisualLocation,
    calls: AtomicUsize,
}

impl ScriptedVision {
    fn arc(answer: VisualLocation) -> (Arc<dyn PageNavigator>, Arc<Self>) {
        let vision = Arc::new(Self {
            answer,
            calls: AtomicUsize::new(0),
        });
        (vision.clone(), vision)
    }
}

impl PageNavigator for ScriptedVision {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        None
    }

    fn locate_visual(&self, _target: &str, _screenshot_jpeg_b64: &str) -> VisualLocation {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.answer.clone()
    }
}

/// A menu whose items carry no log-out vocabulary — the AX-blind shape.
fn blind_menu() -> Vec<AxElement> {
    vec![el(10, "menuitem", "View Profile")]
}

async fn run(
    browser: &VisualBrowser,
    vision: Option<&Arc<dyn PageNavigator>>,
) -> Result<PageGoalOutcome, IntentError> {
    hermetic_classifier();
    pursue_chrome_action_with_vision(browser, &origin(), log_out_spec(), vision).await
}

fn miss_diagnostic(result: Result<PageGoalOutcome, IntentError>) -> String {
    match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected NoMatch miss, got {other:?}"),
    }
}

#[tokio::test]
async fn visual_click_lands_and_verifier_completes() {
    let browser = VisualBrowser::new(blind_menu(), true);
    let (navigator, vision) = ScriptedVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    match run(&browser, Some(&navigator)).await {
        Ok(PageGoalOutcome::Verified { label, .. }) => assert_eq!(label, "visual pick"),
        other => panic!("expected Verified, got {other:?}"),
    }
    assert_eq!(browser.clicks_at().await, vec![(600.0, 400.0)]);
    assert_eq!(vision.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn found_false_misses_and_existing_chain_continues() {
    let browser = VisualBrowser::new(blind_menu(), true);
    let (navigator, _) = ScriptedVision::arc(VisualLocation::NotFound);
    let diagnostic = miss_diagnostic(run(&browser, Some(&navigator)).await);
    assert!(
        diagnostic.contains("visual_fallback: missed (not found)"),
        "got: {diagnostic}"
    );
    assert!(browser.clicks_at().await.is_empty());
}

#[tokio::test]
async fn failed_call_misses_with_reason() {
    let browser = VisualBrowser::new(blind_menu(), true);
    let (navigator, _) = ScriptedVision::arc(VisualLocation::Failed("timeout".to_owned()));
    let diagnostic = miss_diagnostic(run(&browser, Some(&navigator)).await);
    assert!(
        diagnostic.contains("visual_fallback: missed (timeout)"),
        "got: {diagnostic}"
    );
    assert!(browser.clicks_at().await.is_empty());
}

#[tokio::test]
async fn out_of_viewport_coordinates_are_rejected_without_clicking() {
    let browser = VisualBrowser::new(blind_menu(), true);
    let (navigator, _) = ScriptedVision::arc(VisualLocation::Point { x: 1500.0, y: 20.0 });
    let diagnostic = miss_diagnostic(run(&browser, Some(&navigator)).await);
    assert!(
        diagnostic.contains("visual_fallback: missed (invalid coordinates)"),
        "got: {diagnostic}"
    );
    assert!(browser.clicks_at().await.is_empty());
}

#[tokio::test]
async fn click_that_does_not_sign_out_is_a_verification_miss() {
    let browser = VisualBrowser::new(blind_menu(), false);
    let (navigator, _) = ScriptedVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    let diagnostic = miss_diagnostic(run(&browser, Some(&navigator)).await);
    assert!(
        diagnostic.contains("visual_fallback: missed (verification failed)"),
        "got: {diagnostic}"
    );
    // The click happened once; the verifier refused it.
    assert_eq!(browser.clicks_at().await.len(), 1);
}

#[tokio::test]
async fn no_vision_model_skips_with_one_journal_line() {
    let browser = VisualBrowser::new(blind_menu(), true);
    let (navigator, _) = ScriptedVision::arc(VisualLocation::Unsupported);
    let diagnostic = miss_diagnostic(run(&browser, Some(&navigator)).await);
    assert_eq!(
        diagnostic
            .matches("visual_fallback: skipped (no vision model)")
            .count(),
        1,
        "got: {diagnostic}"
    );
    assert!(browser.clicks_at().await.is_empty());
}

#[tokio::test]
async fn no_navigator_leaves_the_worker_untouched() {
    let browser = VisualBrowser::new(blind_menu(), true);
    let diagnostic = miss_diagnostic(run(&browser, None).await);
    assert!(!diagnostic.contains("visual_fallback"), "got: {diagnostic}");
}

#[tokio::test]
async fn direct_ax_match_never_asks_the_model() {
    let mut menu = blind_menu();
    menu.push(el(11, "menuitem", "Log out"));
    let browser = VisualBrowser::new(menu, false);
    let (navigator, vision) = ScriptedVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    let _ = run(&browser, Some(&navigator)).await;
    assert_eq!(vision.calls.load(Ordering::Relaxed), 0);
    assert!(browser.clicks_at().await.is_empty());
    assert!(browser.clicks.lock().await.contains(&11));
}

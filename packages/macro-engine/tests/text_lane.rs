//! Text lane of the sign-out worker: when an identity menu is open but its
//! rows are role-less (invisible to the AX pick), the verb's word is found
//! as rendered text inside the menu region, the point is guarded against
//! occlusion, the trusted click fires, and the existing signed-out verifier
//! decides. No model is involved; vision is only the backup.
//!
//! Hermetic: scripted fake browser and navigator — no live model, no
//! Chromium, no site-specific fixtures (RFC-reserved `example.com` only).
#![allow(unsafe_code)]

use base64::Engine as _;
use browser_driver::{AuthState, AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::executor::{
    TextCandidate, TextLanePick, TextProbe, VISUAL_CROP_JPEG_QUALITY, journal_visual_crop_save,
    normalize_text_lane, pick_text_lane_target, save_visual_crop, text_lane_guard_passes,
    text_lane_probe_expression, text_lane_region,
};
use macro_engine::navigator::VisualCrop;
use macro_engine::{
    ChromeActionBrowser, IntentError, MenuBrowser, PageAction, PageGoalOutcome, PageNavigator,
    SettingsBrowser, VerbKind, VerbSpec, VisualLocation, pursue_chrome_action_with_vision,
};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
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

fn spec(kind: VerbKind) -> &'static VerbSpec {
    VerbSpec::for_kind(kind)
}

fn text(text: &str, cx: f64, cy: f64) -> TextCandidate {
    TextCandidate {
        text: text.to_owned(),
        cx,
        cy,
    }
}

fn probe(hit_text: &str, contains_match: bool) -> TextProbe {
    TextProbe {
        hit_text: hit_text.to_owned(),
        contains_match,
    }
}

fn region() -> VisualCrop {
    VisualCrop {
        x: 480.0,
        y: 0.0,
        w: 720.0,
        h: 720.0,
    }
}

const WORDS: &[&str] = &["log out", "log off", "sign out"];

// ---- normalization ----

#[test]
fn normalization_trims_collapses_and_casefolds() {
    for raw in [" Log Out ", "LOG\u{a0}OUT", "log  out", "\n\tLog\n Out\t"] {
        assert_eq!(normalize_text_lane(raw), "log out", "raw: {raw:?}");
    }
    assert_ne!(normalize_text_lane("Display Mode"), "log out");
    assert_ne!(normalize_text_lane("Log out of u/x"), "log out");
}

// ---- word priority, ambiguity, region ----

#[test]
fn exactly_one_match_wins() {
    let candidates = [
        text("Display Mode", 900.0, 380.0),
        text(" Log Out ", 900.0, 420.0),
    ];
    assert_eq!(
        pick_text_lane_target(&candidates, WORDS, region()),
        TextLanePick::Found {
            word: "log out".to_owned(),
            cx: 900.0,
            cy: 420.0,
        }
    );
}

#[test]
fn words_are_tried_in_vocabulary_order() {
    // "sign out" appears first on the page, but "log out" is first in the
    // vocabulary, so it wins.
    let candidates = [
        text("Sign out", 900.0, 300.0),
        text("Log out", 900.0, 420.0),
    ];
    match pick_text_lane_target(&candidates, WORDS, region()) {
        TextLanePick::Found { word, cy, .. } => {
            assert_eq!(word, "log out");
            assert!((cy - 420.0).abs() < f64::EPSILON);
        }
        other => panic!("expected Found, got {other:?}"),
    }
    // A later word is used when the earlier ones have no match.
    let candidates = [text("Sign Out", 900.0, 300.0)];
    assert!(matches!(
        pick_text_lane_target(&candidates, WORDS, region()),
        TextLanePick::Found { ref word, .. } if word == "sign out"
    ));
}

#[test]
fn two_matches_are_ambiguous_without_trying_later_words() {
    let candidates = [
        text("Log Out", 900.0, 300.0),
        text("log out", 900.0, 420.0),
        text("Sign out", 900.0, 500.0),
    ];
    assert_eq!(
        pick_text_lane_target(&candidates, WORDS, region()),
        TextLanePick::Ambiguous {
            word: "log out".to_owned()
        }
    );
}

#[test]
fn zero_matches_is_no_match() {
    let candidates = [
        text("Display Mode", 900.0, 380.0),
        text("Log out of u/x", 900.0, 420.0),
    ];
    assert_eq!(
        pick_text_lane_target(&candidates, WORDS, region()),
        TextLanePick::NoMatch
    );
    assert_eq!(
        pick_text_lane_target(&[], WORDS, region()),
        TextLanePick::NoMatch
    );
}

#[test]
fn matches_outside_the_region_are_ignored() {
    // An out-of-region "Log Out" (e.g. page footer) neither wins nor makes
    // the in-region row ambiguous.
    let candidates = [
        text("Log Out", 100.0, 300.0),
        text("Log Out", 900.0, 790.0),
        text("Log Out", 900.0, 420.0),
    ];
    assert!(matches!(
        pick_text_lane_target(&candidates, WORDS, region()),
        TextLanePick::Found { cx, cy, .. } if (cx - 900.0).abs() < f64::EPSILON && (cy - 420.0).abs() < f64::EPSILON
    ));
    let outside_only = [text("Log Out", 100.0, 300.0)];
    assert_eq!(
        pick_text_lane_target(&outside_only, WORDS, region()),
        TextLanePick::NoMatch
    );
}

#[test]
fn region_is_the_crop_square_or_the_full_viewport() {
    let viewport = (1200.0, 800.0);
    assert_eq!(
        text_lane_region(Some((970.0, 70.0)), viewport),
        Some(region())
    );
    assert_eq!(
        text_lane_region(None, viewport),
        Some(VisualCrop {
            x: 0.0,
            y: 0.0,
            w: 1200.0,
            h: 800.0,
        })
    );
    assert_eq!(text_lane_region(None, (0.0, 800.0)), None);
}

// ---- occlusion guard ----

#[test]
fn guard_accepts_the_row_itself() {
    assert!(text_lane_guard_passes(
        Some(&probe("Log Out", true)),
        "log out"
    ));
    assert!(text_lane_guard_passes(
        Some(&probe("  LOG\u{a0}OUT ", true)),
        "log out"
    ));
}

#[test]
fn guard_rejects_neighbor_container_overlay_and_missing_probe() {
    // The Display Mode misclick: the neighboring row.
    assert!(!text_lane_guard_passes(
        Some(&probe("Display Mode", false)),
        "log out"
    ));
    // A wide container (the whole menu list) contains the word but its
    // own text is every row.
    assert!(!text_lane_guard_passes(
        Some(&probe("Display Mode Log Out", true)),
        "log out"
    ));
    // Text equal but no visible matching text node inside (overlay).
    assert!(!text_lane_guard_passes(
        Some(&probe("Log Out", false)),
        "log out"
    ));
    assert!(!text_lane_guard_passes(None, "log out"));
}

#[test]
fn probe_expression_substitutes_point_and_quoted_word() {
    let expression = text_lane_probe_expression(1000.0, 420.0, "Log Out");
    assert!(
        expression.contains("(1000, 420, \"log out\")"),
        "{expression}"
    );
    assert!(!expression.contains("{x}") && !expression.contains("{word}"));
    // A quote in the word cannot break out of the JS string literal.
    let hostile = text_lane_probe_expression(1.0, 2.0, "a\"); alert(1); (\"");
    assert!(hostile.contains(r#""a\"); alert(1); (\"""#), "{hostile}");
}

// ---- crop save ----

#[test]
fn crop_save_persists_the_exact_model_bound_bytes() {
    assert_eq!(VISUAL_CROP_JPEG_QUALITY, 90);
    let bytes = vec![0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let dir = std::env::temp_dir();
    let path = save_visual_crop(&dir, &b64, 4_242)
        .unwrap_or_else(|error| panic!("temp dir is writable: {error}"));
    assert!(path.ends_with("clinch-visual-crop-4242.jpg"), "{path:?}");
    let written = std::fs::read(&path).unwrap_or_else(|error| panic!("read back: {error}"));
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, bytes);
}

#[test]
fn crop_save_failure_degrades_to_a_journal_line() {
    let missing = std::env::temp_dir()
        .join("clinch-text-lane-test-missing-dir")
        .join("nested");
    let b64 = base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]);
    assert!(save_visual_crop(&missing, &b64, 1).is_err());
    let mut tried = Vec::new();
    journal_visual_crop_save(&missing, &b64, &mut tried);
    assert_eq!(tried.len(), 1, "{tried:?}");
    assert!(
        tried[0].starts_with("visual_fallback: crop_save_failed ("),
        "{tried:?}"
    );
    let mut tried = Vec::new();
    journal_visual_crop_save(&std::env::temp_dir(), &b64, &mut tried);
    assert!(
        tried[0].starts_with("visual_fallback: crop_saved "),
        "{tried:?}"
    );
    if let Some(path) = tried[0].strip_prefix("visual_fallback: crop_saved ") {
        let _ = std::fs::remove_file(path);
    }
}

// ---- scripted worker flow ----

/// Page whose avatar (id 1) opens a menu. The menu's AX layer shows only a
/// row that doesn't name the verb — the role-less-rows shape — while the
/// scripted text candidates carry what the page actually renders.
struct TextBrowser {
    baseline: Vec<AxElement>,
    menu: Vec<AxElement>,
    candidates: Vec<TextCandidate>,
    probe: Option<TextProbe>,
    screenshot: String,
    signs_out_on_click_at: bool,
    clicks: Mutex<Vec<i64>>,
    clicks_at: Mutex<Vec<(f64, f64)>>,
    probes_at: Mutex<Vec<(f64, f64, String)>>,
    auth: Mutex<AuthState>,
    snapshots: AtomicUsize,
    candidate_calls: AtomicUsize,
    screenshots: AtomicUsize,
}

impl TextBrowser {
    fn new(candidates: Vec<TextCandidate>, probe: Option<TextProbe>, signs_out: bool) -> Self {
        let baseline = vec![
            el(1, "button", "User Avatar Expand user menu"),
            el(3, "link", "Home"),
        ];
        let mut menu = baseline.clone();
        menu.push(el(10, "menuitem", "View Profile"));
        Self {
            baseline,
            menu,
            candidates,
            probe,
            screenshot: "AAAA".to_owned(),
            signs_out_on_click_at: signs_out,
            clicks: Mutex::new(Vec::new()),
            clicks_at: Mutex::new(Vec::new()),
            probes_at: Mutex::new(Vec::new()),
            auth: Mutex::new(AuthState::Authenticated),
            snapshots: AtomicUsize::new(0),
            candidate_calls: AtomicUsize::new(0),
            screenshots: AtomicUsize::new(0),
        }
    }

    /// The Muse-shaped menu: Display Mode directly above Log Out, both
    /// role-less, plus a footer "Log Out" outside the menu region.
    fn muse_menu(probe: Option<TextProbe>, signs_out: bool) -> Self {
        Self::new(
            vec![
                text("View Profile", 1000.0, 300.0),
                text("Display Mode", 1000.0, 380.0),
                text(" Log Out ", 1000.4, 419.6),
                text("Log Out", 100.0, 760.0),
            ],
            probe,
            signs_out,
        )
    }

    async fn clicks_at(&self) -> Vec<(f64, f64)> {
        self.clicks_at.lock().await.clone()
    }

    async fn opened(&self) -> bool {
        self.clicks.lock().await.contains(&1)
    }
}

impl MenuBrowser for TextBrowser {
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
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        let tree = if self.opened().await {
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
        // A direct AX "Log out" click ends the session, like a live page.
        if element.name == "Log out" {
            *self.auth.lock().await = AuthState::LoggedOut;
        }
        Ok(())
    }

    fn menu_dismiss(&self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }

    fn menu_screenshot(&self) -> impl std::future::Future<Output = Option<String>> + Send {
        self.screenshots.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Some(self.screenshot.clone()))
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

    async fn menu_text_candidates(&self) -> Vec<TextCandidate> {
        self.candidate_calls.fetch_add(1, Ordering::SeqCst);
        if self.opened().await {
            self.candidates.clone()
        } else {
            Vec::new()
        }
    }

    async fn menu_text_probe(&self, x: f64, y: f64, word: &str) -> Option<TextProbe> {
        self.probes_at.lock().await.push((x, y, word.to_owned()));
        self.probe.clone()
    }
}

impl SettingsBrowser for TextBrowser {
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

impl ChromeActionBrowser for TextBrowser {
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
    seen: StdMutex<Vec<String>>,
}

impl ScriptedVision {
    fn arc(answer: VisualLocation) -> (Arc<dyn PageNavigator>, Arc<Self>) {
        let vision = Arc::new(Self {
            answer,
            calls: AtomicUsize::new(0),
            seen: StdMutex::new(Vec::new()),
        });
        (vision.clone(), vision)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl PageNavigator for ScriptedVision {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        None
    }

    fn locate_visual(&self, _target: &str, screenshot_jpeg_b64: &str) -> VisualLocation {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(screenshot_jpeg_b64.to_owned());
        }
        self.answer.clone()
    }
}

async fn run(
    browser: &TextBrowser,
    kind: VerbKind,
    vision: Option<&Arc<dyn PageNavigator>>,
) -> Result<PageGoalOutcome, IntentError> {
    hermetic_classifier();
    pursue_chrome_action_with_vision(browser, &origin(), spec(kind), vision).await
}

fn miss_diagnostic(result: Result<PageGoalOutcome, IntentError>) -> String {
    match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected NoMatch miss, got {other:?}"),
    }
}

#[tokio::test]
async fn muse_flow_avatar_then_log_out_row_then_verifier() {
    let browser = TextBrowser::muse_menu(Some(probe("Log Out", true)), true);
    let (navigator, vision) = ScriptedVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    match run(&browser, VerbKind::LogOut, Some(&navigator)).await {
        Ok(PageGoalOutcome::Verified {
            label, tried_lines, ..
        }) => {
            assert_eq!(label, "text pick: log out");
            assert!(
                tried_lines
                    .iter()
                    .any(|line| line == "text_lane: click_at (1000, 420) \"log out\""),
                "{tried_lines:?}"
            );
            assert!(
                !tried_lines
                    .iter()
                    .any(|line| line.contains("visual_fallback")),
                "{tried_lines:?}"
            );
        }
        other => panic!("expected Verified, got {other:?}"),
    }
    // Avatar first, then the Log Out row at its rounded center.
    assert_eq!(*browser.clicks.lock().await, vec![1]);
    assert_eq!(browser.clicks_at().await, vec![(1000.0, 420.0)]);
    assert_eq!(
        *browser.probes_at.lock().await,
        vec![(1000.0, 420.0, "log out".to_owned())]
    );
    // Vision never reached.
    assert_eq!(vision.calls(), 0);
    assert_eq!(browser.screenshots.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn text_lane_runs_without_a_vision_model() {
    let browser = TextBrowser::muse_menu(Some(probe("Log Out", true)), true);
    match run(&browser, VerbKind::LogOut, None).await {
        Ok(PageGoalOutcome::Verified { label, .. }) => assert_eq!(label, "text pick: log out"),
        other => panic!("expected Verified, got {other:?}"),
    }
    assert_eq!(browser.clicks_at().await, vec![(1000.0, 420.0)]);
}

#[tokio::test]
async fn occluded_point_never_clicks_and_vision_still_fires_once() {
    let browser = TextBrowser::muse_menu(Some(probe("Display Mode", false)), true);
    let (navigator, vision) = ScriptedVision::arc(VisualLocation::NotFound);
    let diagnostic = miss_diagnostic(run(&browser, VerbKind::LogOut, Some(&navigator)).await);
    assert!(
        diagnostic.contains("text_lane: missed (occluded: hit \"Display Mode\")"),
        "got: {diagnostic}"
    );
    assert!(browser.clicks_at().await.is_empty());
    assert_eq!(vision.calls(), 1);
    // Ordering: the text lane's miss is journaled before vision's.
    let text_at = diagnostic.find("text_lane: missed");
    let vision_at = diagnostic.find("visual_fallback: missed (not found)");
    assert!(
        matches!((text_at, vision_at), (Some(t), Some(v)) if t < v),
        "got: {diagnostic}"
    );
}

#[tokio::test]
async fn container_hit_is_treated_as_occluded() {
    let browser = TextBrowser::muse_menu(Some(probe("Display Mode Log Out", true)), true);
    let diagnostic = miss_diagnostic(run(&browser, VerbKind::LogOut, None).await);
    assert!(
        diagnostic.contains("text_lane: missed (occluded: hit \"Display Mode Log Out\")"),
        "got: {diagnostic}"
    );
    assert!(browser.clicks_at().await.is_empty());
}

#[tokio::test]
async fn no_text_match_falls_through_to_vision() {
    let browser = TextBrowser::new(
        vec![text("Display Mode", 1000.0, 380.0)],
        Some(probe("Log Out", true)),
        true,
    );
    let (navigator, vision) = ScriptedVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    match run(&browser, VerbKind::LogOut, Some(&navigator)).await {
        Ok(PageGoalOutcome::Verified { label, .. }) => assert_eq!(label, "visual pick"),
        other => panic!("expected vision's Verified, got {other:?}"),
    }
    assert_eq!(vision.calls(), 1);
    assert!(browser.probes_at.lock().await.is_empty());
}

#[tokio::test]
async fn ambiguous_rows_never_click() {
    let browser = TextBrowser::new(
        vec![
            text("Log Out", 1000.0, 380.0),
            text("Log Out", 1000.0, 420.0),
        ],
        Some(probe("Log Out", true)),
        true,
    );
    let diagnostic = miss_diagnostic(run(&browser, VerbKind::LogOut, None).await);
    assert!(
        diagnostic.contains("text_lane: missed (ambiguous)"),
        "got: {diagnostic}"
    );
    assert!(browser.clicks_at().await.is_empty());
    assert!(browser.probes_at.lock().await.is_empty());
}

#[tokio::test]
async fn clicked_then_unverified_resnapshots_and_skips_vision() {
    let browser = TextBrowser::muse_menu(Some(probe("Log Out", true)), false);
    let (navigator, vision) = ScriptedVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    let diagnostic = miss_diagnostic(run(&browser, VerbKind::LogOut, Some(&navigator)).await);
    assert!(
        diagnostic.contains("text_lane: missed (verification failed)"),
        "got: {diagnostic}"
    );
    assert!(!browser.clicks_at().await.is_empty());
    // Vision never runs on a menu the text-lane click likely closed.
    assert_eq!(vision.calls(), 0);
    assert_eq!(browser.screenshots.load(Ordering::SeqCst), 0);
    // Baseline snapshot, post-opener snapshot, then a fresh one after the
    // text-lane click.
    assert!(browser.snapshots.load(Ordering::SeqCst) >= 3);
}

#[tokio::test]
async fn direct_ax_match_never_runs_the_text_lane() {
    let mut browser = TextBrowser::muse_menu(Some(probe("Log Out", true)), false);
    browser.menu.push(el(11, "menuitem", "Log out"));
    let _ = run(&browser, VerbKind::LogOut, None).await;
    assert_eq!(browser.candidate_calls.load(Ordering::SeqCst), 0);
    assert!(browser.clicks.lock().await.contains(&11));
}

#[tokio::test]
async fn non_sign_out_verb_never_runs_the_text_lane() {
    let browser = TextBrowser::new(
        vec![text("Settings", 1000.0, 420.0)],
        Some(probe("Settings", true)),
        false,
    );
    let _ = run(&browser, VerbKind::Settings, None).await;
    assert_eq!(browser.candidate_calls.load(Ordering::SeqCst), 0);
    assert!(browser.clicks_at().await.is_empty());
}

#[tokio::test]
async fn nothing_clicked_yet_never_runs_the_text_lane() {
    // No opener on the page: the worker defers without clicking anything.
    let mut browser = TextBrowser::muse_menu(Some(probe("Log Out", true)), true);
    browser.baseline = vec![el(3, "link", "Home")];
    let _ = run(&browser, VerbKind::LogOut, None).await;
    assert!(browser.clicks.lock().await.is_empty());
    assert_eq!(browser.candidate_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn vision_crop_is_saved_byte_identical_to_what_the_model_saw() {
    // A real JPEG screenshot so the crop path runs; the text lane finds no
    // match, so vision is reached.
    let image = image::RgbImage::from_pixel(1200, 800, image::Rgb([200, 200, 200]));
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
        .encode_image(&image)
        .unwrap_or_else(|error| panic!("fixture encodes: {error}"));
    let mut browser = TextBrowser::new(Vec::new(), None, true);
    browser.screenshot = base64::engine::general_purpose::STANDARD.encode(jpeg);
    let (navigator, vision) = ScriptedVision::arc(VisualLocation::NotFound);
    let diagnostic = miss_diagnostic(run(&browser, VerbKind::LogOut, Some(&navigator)).await);
    let marker = "visual_fallback: crop_saved ";
    let start = diagnostic
        .find(marker)
        .unwrap_or_else(|| panic!("crop_saved journaled: {diagnostic}"))
        + marker.len();
    let path: String = diagnostic[start..]
        .chars()
        .take_while(|c| !matches!(c, ';' | ']' | '\n'))
        .collect::<String>()
        .trim()
        .to_owned();
    assert!(
        std::path::Path::new(&path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg")),
        "path: {path:?} in {diagnostic}"
    );
    let written = std::fs::read(&path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    let _ = std::fs::remove_file(&path);
    let sent = vision
        .seen
        .lock()
        .map(|seen| seen.first().cloned().unwrap_or_default())
        .unwrap_or_default();
    let sent_bytes = base64::engine::general_purpose::STANDARD
        .decode(sent)
        .unwrap_or_else(|error| panic!("sent is base64: {error}"));
    assert_eq!(written, sent_bytes);
}

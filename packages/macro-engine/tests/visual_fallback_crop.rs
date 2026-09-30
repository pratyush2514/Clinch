//! Visual-fallback crop: the screenshot sent to the vision model is a
//! square around the menu opener's live click point, the model answers in
//! crop space, and deterministic Rust remaps that to viewport pixels
//! before anything clicks. Without an opener point (or a decodable
//! screenshot) the full viewport goes out exactly as before.
//!
//! Hermetic: scripted fake browser and navigator, synthetic JPEGs — no live
//! model, no Chromium, no site-specific fixtures (RFC-reserved
//! `example.com` only).
#![allow(unsafe_code)]

use base64::Engine as _;
use browser_driver::{AuthState, AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::executor::{
    VISUAL_CROP_SIZE, crop_screenshot_jpeg_b64, visual_crop_rect, visual_crop_target_description,
};
use macro_engine::navigator::{CropRemapError, VisualCrop, crop_point_to_viewport};
use macro_engine::{
    ChromeActionBrowser, IntentError, MenuBrowser, PageAction, PageGoalOutcome, PageNavigator,
    SettingsBrowser, VerbKind, VerbSpec, VisualLocation, pursue_chrome_action_with_vision,
};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
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

fn crop(x: f64, y: f64, w: f64, h: f64) -> VisualCrop {
    VisualCrop { x, y, w, h }
}

/// Synthetic base64 JPEG (no data-URI prefix) of the given pixel size.
fn jpeg_b64(width: u32, height: u32) -> String {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 200, 200]));
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 80)
        .encode_image(&image)
        .unwrap_or_else(|error| panic!("fixture encodes: {error}"));
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn jpeg_dimensions(b64: &str) -> (u32, u32) {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap_or_else(|error| panic!("valid base64: {error}"));
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
        .unwrap_or_else(|error| panic!("valid JPEG: {error}"));
    (image.width(), image.height())
}

// ---- crop construction ----

#[test]
fn crop_is_centered_on_the_opener() {
    assert!((VISUAL_CROP_SIZE - 720.0).abs() < f64::EPSILON);
    assert_eq!(
        visual_crop_rect((960.0, 540.0), (1920.0, 1080.0)),
        Some(crop(600.0, 180.0, 720.0, 720.0))
    );
}

#[test]
fn crop_clamps_at_every_viewport_edge() {
    let viewport = (1920.0, 1080.0);
    // Left and top: an opener near the top-left corner.
    assert_eq!(
        visual_crop_rect((20.0, 20.0), viewport),
        Some(crop(0.0, 0.0, 720.0, 720.0))
    );
    // Right and bottom: an opener near the bottom-right corner.
    assert_eq!(
        visual_crop_rect((1900.0, 1060.0), viewport),
        Some(crop(1200.0, 360.0, 720.0, 720.0))
    );
    // Top-right avatar (the common header shape).
    assert_eq!(
        visual_crop_rect((1880.0, 30.0), viewport),
        Some(crop(1200.0, 0.0, 720.0, 720.0))
    );
    // Bottom-left.
    assert_eq!(
        visual_crop_rect((10.0, 1070.0), viewport),
        Some(crop(0.0, 360.0, 720.0, 720.0))
    );
}

#[test]
fn crop_shrinks_to_a_smaller_viewport() {
    // Height below S: the side becomes min(S, w, h) and the rect stays inside.
    assert_eq!(
        visual_crop_rect((600.0, 250.0), (1200.0, 500.0)),
        Some(crop(350.0, 0.0, 500.0, 500.0))
    );
    // Both axes below S.
    assert_eq!(
        visual_crop_rect((300.0, 200.0), (640.0, 480.0)),
        Some(crop(60.0, 0.0, 480.0, 480.0))
    );
}

#[test]
fn degenerate_inputs_build_no_crop() {
    assert_eq!(visual_crop_rect((10.0, 10.0), (0.0, 800.0)), None);
    assert_eq!(visual_crop_rect((f64::NAN, 10.0), (1200.0, 800.0)), None);
    assert_eq!(
        visual_crop_rect((10.0, 10.0), (1200.0, f64::INFINITY)),
        None
    );
}

// ---- crop → viewport remap ----

#[test]
fn remap_is_exact_at_corners_and_center() {
    let rect = crop(1200.0, 0.0, 720.0, 720.0);
    let viewport = (1920.0, 1080.0);
    assert_eq!(
        crop_point_to_viewport(0.0, 0.0, rect, viewport),
        Ok((1200.0, 0.0))
    );
    assert_eq!(
        crop_point_to_viewport(1000.0, 1000.0, rect, viewport),
        Ok((1920.0, 720.0))
    );
    assert_eq!(
        crop_point_to_viewport(500.0, 500.0, rect, viewport),
        Ok((1560.0, 360.0))
    );
    assert_eq!(
        crop_point_to_viewport(1000.0, 0.0, rect, viewport),
        Ok((1920.0, 0.0))
    );
}

#[test]
fn remap_rounds_to_whole_pixels() {
    let rect = crop(100.0, 50.0, 720.0, 720.0);
    // 333 * 0.72 = 239.76 → 240; 1 * 0.72 = 0.72 → 1.
    assert_eq!(
        crop_point_to_viewport(333.0, 1.0, rect, (1920.0, 1080.0)),
        Ok((340.0, 51.0))
    );
    // 0.5 * 0.72 = 0.36 → 0.
    assert_eq!(
        crop_point_to_viewport(0.5, 0.5, rect, (1920.0, 1080.0)),
        Ok((100.0, 50.0))
    );
}

#[test]
fn remapped_point_outside_the_viewport_is_rejected() {
    // Viewport shrank after the crop was built.
    let rect = crop(1200.0, 0.0, 720.0, 720.0);
    assert_eq!(
        crop_point_to_viewport(900.0, 100.0, rect, (1600.0, 1080.0)),
        Err(CropRemapError::OutsideViewport)
    );
    assert_eq!(
        crop_point_to_viewport(0.0, 900.0, rect, (1920.0, 600.0)),
        Err(CropRemapError::OutsideViewport)
    );
}

#[test]
fn malformed_model_points_are_invalid_not_remapped() {
    let rect = crop(0.0, 0.0, 720.0, 720.0);
    let viewport = (1920.0, 1080.0);
    for (x, y) in [
        (-1.0, 10.0),
        (10.0, 1000.5),
        (f64::NAN, 10.0),
        (10.0, f64::INFINITY),
    ] {
        assert_eq!(
            crop_point_to_viewport(x, y, rect, viewport),
            Err(CropRemapError::InvalidCoordinates)
        );
    }
}

// ---- image crop ----

#[test]
fn screenshot_is_cropped_to_the_rect() {
    let full = jpeg_b64(1200, 800);
    let cropped = crop_screenshot_jpeg_b64(&full, crop(480.0, 0.0, 720.0, 720.0), (1200.0, 800.0))
        .unwrap_or_else(|| panic!("crop succeeds"));
    assert_eq!(jpeg_dimensions(&cropped), (720, 720));
}

#[test]
fn crop_scales_with_device_pixel_ratio() {
    // A 2× screenshot of a 1200×800 CSS viewport.
    let full = jpeg_b64(2400, 1600);
    let cropped = crop_screenshot_jpeg_b64(&full, crop(480.0, 0.0, 720.0, 720.0), (1200.0, 800.0))
        .unwrap_or_else(|| panic!("crop succeeds"));
    assert_eq!(jpeg_dimensions(&cropped), (1440, 1440));
}

#[test]
fn undecodable_screenshot_builds_no_crop() {
    assert_eq!(
        crop_screenshot_jpeg_b64("AAAA", crop(0.0, 0.0, 100.0, 100.0), (1200.0, 800.0)),
        None
    );
    assert_eq!(
        crop_screenshot_jpeg_b64("not base64!", crop(0.0, 0.0, 100.0, 100.0), (1200.0, 800.0)),
        None
    );
}

// ---- target description ----

#[test]
fn crop_target_description_demands_exact_readable_text() {
    let description = visual_crop_target_description(log_out_spec());
    assert!(
        description.contains("reads exactly \"log out\" (case-insensitive)"),
        "got: {description}"
    );
    assert!(
        description.contains("cropped screenshot"),
        "got: {description}"
    );
    assert!(
        description.contains("{\"found\": false}"),
        "got: {description}"
    );
    assert!(
        description.contains("Do not guess a point near the bottom of the menu"),
        "got: {description}"
    );
}

// ---- scripted worker flow ----

/// Page whose avatar (opener rect 950,50 40×40 → center 970,70) opens an
/// AX-blind menu, in a 1200×800 viewport that shrinks to 600×400 once
/// `resized` is set; the opener rect can be made unmeasurable once the
/// menu is open.
struct CropBrowser {
    baseline: Vec<AxElement>,
    menu: Vec<AxElement>,
    screenshot: String,
    resized: Arc<AtomicBool>,
    opener_unmeasurable_when_open: bool,
    signs_out_on_click_at: bool,
    clicks: Mutex<Vec<i64>>,
    clicks_at: Mutex<Vec<(f64, f64)>>,
    auth: Mutex<AuthState>,
}

impl CropBrowser {
    fn new(screenshot: String) -> Self {
        let baseline = vec![
            el(1, "button", "User Avatar Expand user menu"),
            el(3, "link", "Home"),
        ];
        let mut menu = baseline.clone();
        menu.push(el(10, "menuitem", "View Profile"));
        Self {
            baseline,
            menu,
            screenshot,
            resized: Arc::new(AtomicBool::new(false)),
            opener_unmeasurable_when_open: false,
            signs_out_on_click_at: true,
            clicks: Mutex::new(Vec::new()),
            clicks_at: Mutex::new(Vec::new()),
            auth: Mutex::new(AuthState::Authenticated),
        }
    }

    async fn clicks_at(&self) -> Vec<(f64, f64)> {
        self.clicks_at.lock().await.clone()
    }
}

impl MenuBrowser for CropBrowser {
    fn menu_viewport_size(&self) -> impl std::future::Future<Output = Option<(f64, f64)>> + Send {
        let size = if self.resized.load(Ordering::SeqCst) {
            (600.0, 400.0)
        } else {
            (1200.0, 800.0)
        };
        std::future::ready(Some(size))
    }

    async fn menu_node_rect(&self, backend_node_id: i64) -> Result<Highlight, BrowserError> {
        if self.opener_unmeasurable_when_open && self.clicks.lock().await.contains(&1) {
            return Err(BrowserError::Connection);
        }
        Ok(Highlight {
            selector: format!("ax:{backend_node_id}"),
            x: 950.0,
            y: 50.0,
            width: 40.0,
            height: 40.0,
            matches: 1,
        })
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
        Ok(())
    }

    fn menu_dismiss(&self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }

    fn menu_screenshot(&self) -> impl std::future::Future<Output = Option<String>> + Send {
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
}

impl SettingsBrowser for CropBrowser {
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

impl ChromeActionBrowser for CropBrowser {
    async fn chrome_auth_state(&self) -> AuthState {
        *self.auth.lock().await
    }

    async fn chrome_navigate(&self, _url: &Url) -> Result<(), IntentError> {
        Ok(())
    }
}

/// Navigator scripted to a fixed answer; records what it was sent and
/// optionally resizes the page's viewport as it answers.
struct RecordingVision {
    answer: VisualLocation,
    seen: StdMutex<Vec<(String, String)>>,
    resize_on_answer: Option<Arc<AtomicBool>>,
}

impl RecordingVision {
    fn arc(answer: VisualLocation) -> (Arc<dyn PageNavigator>, Arc<Self>) {
        Self::arc_resizing(answer, None)
    }

    fn arc_resizing(
        answer: VisualLocation,
        resize_on_answer: Option<Arc<AtomicBool>>,
    ) -> (Arc<dyn PageNavigator>, Arc<Self>) {
        let vision = Arc::new(Self {
            answer,
            seen: StdMutex::new(Vec::new()),
            resize_on_answer,
        });
        (vision.clone(), vision)
    }

    fn seen(&self) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl PageNavigator for RecordingVision {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        None
    }

    fn locate_visual(&self, target: &str, screenshot_jpeg_b64: &str) -> VisualLocation {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((target.to_owned(), screenshot_jpeg_b64.to_owned()));
        if let Some(resized) = &self.resize_on_answer {
            resized.store(true, Ordering::SeqCst);
        }
        self.answer.clone()
    }
}

async fn run(
    browser: &CropBrowser,
    vision: &Arc<dyn PageNavigator>,
) -> Result<PageGoalOutcome, IntentError> {
    hermetic_classifier();
    pursue_chrome_action_with_vision(browser, &origin(), log_out_spec(), Some(vision)).await
}

fn tried_lines(result: &Result<PageGoalOutcome, IntentError>) -> String {
    match result {
        Ok(PageGoalOutcome::Verified { hit_lines, .. }) => hit_lines.join("\n"),
        Err(IntentError::NoMatch(diagnostic)) => diagnostic.clone(),
        other => panic!("unexpected outcome {other:?}"),
    }
}

#[tokio::test]
async fn cropped_answer_is_remapped_and_clicked() {
    let browser = CropBrowser::new(jpeg_b64(1200, 800));
    let (navigator, vision) = RecordingVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    let result = run(&browser, &navigator).await;
    match &result {
        Ok(PageGoalOutcome::Verified { label, .. }) => assert_eq!(label, "visual pick"),
        other => panic!("expected Verified, got {other:?}"),
    }
    // Opener center (970, 70) → crop (480, 0, 720, 720); crop center →
    // viewport (480 + 360, 0 + 360).
    assert_eq!(browser.clicks_at().await, vec![(840.0, 360.0)]);
    let seen = vision.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, visual_crop_target_description(log_out_spec()));
    assert_eq!(jpeg_dimensions(&seen[0].1), (720, 720));
}

#[tokio::test]
async fn crop_and_viewport_click_point_are_journaled() {
    // A click the verifier refuses keeps the full journal in the miss.
    let mut browser = CropBrowser::new(jpeg_b64(1200, 800));
    browser.signs_out_on_click_at = false;
    let (navigator, _) = RecordingVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    let lines = tried_lines(&run(&browser, &navigator).await);
    let crop_at = lines
        .find("visual_fallback: crop (480, 0, 720, 720)")
        .unwrap_or_else(|| panic!("crop line missing: {lines}"));
    let click_at = lines
        .find("visual_fallback: click_at (840, 360)")
        .unwrap_or_else(|| panic!("click_at line missing: {lines}"));
    assert!(crop_at < click_at, "got: {lines}");
    assert!(
        lines.contains("visual_fallback: missed (verification failed)"),
        "got: {lines}"
    );
}

#[tokio::test]
async fn unmeasurable_opener_sends_the_full_viewport_as_before() {
    let full = jpeg_b64(1200, 800);
    let mut browser = CropBrowser::new(full.clone());
    browser.opener_unmeasurable_when_open = true;
    let (navigator, vision) = RecordingVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    let result = run(&browser, &navigator).await;
    assert!(
        !tried_lines(&result).contains("visual_fallback: crop"),
        "got: {}",
        tried_lines(&result)
    );
    // Full-viewport mapping, unchanged: (500, 500) → (600, 400).
    assert_eq!(browser.clicks_at().await, vec![(600.0, 400.0)]);
    let seen = vision.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, full);
    assert!(seen[0].0.contains("log out"), "got: {}", seen[0].0);
    assert_ne!(seen[0].0, visual_crop_target_description(log_out_spec()));
}

#[tokio::test]
async fn undecodable_screenshot_sends_the_full_viewport_as_before() {
    let browser = CropBrowser::new("AAAA".to_owned());
    let (navigator, vision) = RecordingVision::arc(VisualLocation::Point { x: 500.0, y: 500.0 });
    let result = run(&browser, &navigator).await;
    assert!(!tried_lines(&result).contains("visual_fallback: crop"));
    assert_eq!(browser.clicks_at().await, vec![(600.0, 400.0)]);
    assert_eq!(vision.seen()[0].1, "AAAA");
}

#[tokio::test]
async fn remapped_point_outside_viewport_misses_without_clicking() {
    let browser = CropBrowser::new(jpeg_b64(1200, 800));
    // The crop is built against 1200×800; the viewport shrinks to 600×400
    // while the model answers, so crop (480, 0, 720, 720) + (1000, 1000)
    // remaps to (1200, 720) — off the live page.
    let (navigator, _) = RecordingVision::arc_resizing(
        VisualLocation::Point {
            x: 1000.0,
            y: 1000.0,
        },
        Some(browser.resized.clone()),
    );
    let result = run(&browser, &navigator).await;
    let lines = tried_lines(&result);
    assert!(
        lines.contains("visual_fallback: missed (remapped point outside viewport)"),
        "got: {lines}"
    );
    assert!(browser.clicks_at().await.is_empty());
}

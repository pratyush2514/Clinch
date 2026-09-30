//! Text lane across the whole identity menu: profile, settings,
//! notifications and sign out all open the same menu, then the lane reads
//! the verb's own word as rendered text and clicks it. One flow,
//! parameterized by verb — vocabulary and verifier come from the verb
//! table; guard, budget and miss ordering are verb-independent (pinned in
//! `text_lane.rs`).
//!
//! Hermetic: scripted fake browser — no live model, no Chromium, no
//! site-specific fixtures (RFC-reserved `example.com` only).
#![allow(unsafe_code)]

use browser_driver::{AuthState, AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::executor::{TextCandidate, TextLanePick, TextProbe, pick_text_lane_target};
use macro_engine::navigator::VisualCrop;
use macro_engine::{
    ChromeActionBrowser, IntentError, MenuBrowser, PageAction, PageGoalOutcome, PageNavigator,
    SettingsBrowser, VerbKind, VerbSpec, VerifierKind, VisualLocation,
    pursue_chrome_action_with_vision, verb_specs,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

fn url(path: &str) -> Url {
    origin()
        .join(path)
        .unwrap_or_else(|error| panic!("test url parses: {error}"))
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

fn text(text: &str, cx: f64, cy: f64) -> TextCandidate {
    TextCandidate {
        text: text.to_owned(),
        cx,
        cy,
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

const IDENTITY_VERBS: [VerbKind; 4] = [
    VerbKind::AccountHome,
    VerbKind::Settings,
    VerbKind::Notifications,
    VerbKind::LogOut,
];

/// A verb with no word list: the lane must decline, never invent words.
static NO_VOCABULARY_SPEC: VerbSpec = VerbSpec {
    kind: VerbKind::Settings,
    vocabulary: &[],
    verifier: VerifierKind::UrlPathTokens(&["setting"]),
};

// ---- the identity-menu set ----

#[test]
fn every_in_tree_verb_is_an_identity_menu_verb() {
    // The verb table has no non-identity verifier today; the predicate is
    // exhaustive, so a new verifier must opt in or out explicitly.
    assert_eq!(verb_specs().len(), IDENTITY_VERBS.len());
    for spec in verb_specs() {
        assert!(spec.verifier.is_identity_menu(), "{:?}", spec.kind);
    }
}

// ---- vocabulary comes from the verb ----

#[test]
fn each_verb_matches_its_own_words_in_declared_order() {
    let cases: [(VerbKind, &[&str], &str); 4] = [
        // Row text on the page, then the word that must win.
        (VerbKind::AccountHome, &["Account", "Profile"], "profile"),
        (VerbKind::Settings, &["Preferences", "Settings"], "settings"),
        (VerbKind::Notifications, &["Notifications"], "notifications"),
        (VerbKind::LogOut, &["Sign out", "Log Out"], "log out"),
    ];
    for (kind, rows, expected) in cases {
        let candidates: Vec<TextCandidate> = rows
            .iter()
            .zip([300.0, 340.0])
            .map(|(row, cy)| text(row, 900.0, cy))
            .collect();
        match pick_text_lane_target(&candidates, VerbSpec::for_kind(kind).vocabulary, region()) {
            TextLanePick::Found { word, .. } => assert_eq!(word, expected, "{kind:?}"),
            other => panic!("{kind:?}: expected Found, got {other:?}"),
        }
    }
}

#[test]
fn a_verb_never_matches_another_verbs_row() {
    let candidates = [
        text("Settings", 900.0, 300.0),
        text("Log Out", 900.0, 340.0),
    ];
    let notifications = VerbSpec::for_kind(VerbKind::Notifications).vocabulary;
    assert_eq!(
        pick_text_lane_target(&candidates, notifications, region()),
        TextLanePick::NoMatch
    );
}

#[test]
fn equality_only_a_longer_row_label_is_not_the_word() {
    // Exact equality, as for sign-out ("Log out of u/x" is not "log out"):
    // a "View Profile" row does not match the profile vocabulary, so the
    // lane misses and the existing chain takes over.
    let candidates = [text("View Profile", 900.0, 300.0)];
    assert_eq!(
        pick_text_lane_target(
            &candidates,
            VerbSpec::for_kind(VerbKind::AccountHome).vocabulary,
            region()
        ),
        TextLanePick::NoMatch
    );
}

// ---- scripted worker flow ----

/// Page whose avatar (id 1) opens a menu. The menu's AX layer shows only a
/// row that names no verb — the role-less-rows shape — while the scripted
/// text candidates carry what the page actually renders. A coordinate
/// click lands on `lands_on` (and signs out when `signs_out`).
struct MenuPage {
    baseline: Vec<AxElement>,
    menu: Vec<AxElement>,
    candidates: Vec<TextCandidate>,
    probe_text: String,
    lands_on: Url,
    landed_title: Option<String>,
    signs_out: bool,
    current: Mutex<Url>,
    clicks: Mutex<Vec<i64>>,
    clicks_at: Mutex<Vec<(f64, f64)>>,
    auth: Mutex<AuthState>,
    candidate_calls: AtomicUsize,
    probes: AtomicUsize,
    screenshots: AtomicUsize,
    cookie_clears: AtomicUsize,
    landed: AtomicBool,
    stale_reads: AtomicUsize,
}

impl MenuPage {
    /// The verb's row rendered under Display Mode (the neighbor that
    /// caught vision one row high), plus the same word in a footer outside
    /// the menu region.
    fn new(row: &str, lands_on: Url) -> Self {
        let baseline = vec![
            el(1, "button", "User Avatar Expand user menu"),
            el(3, "link", "Home"),
        ];
        let mut menu = baseline.clone();
        menu.push(el(10, "menuitem", "Display Mode"));
        Self {
            baseline,
            menu,
            candidates: vec![
                text("Display Mode", 1000.0, 380.0),
                text(&format!(" {row} "), 1000.4, 419.6),
                text(row, 100.0, 760.0),
            ],
            probe_text: row.to_owned(),
            lands_on,
            landed_title: None,
            signs_out: false,
            current: Mutex::new(origin()),
            clicks: Mutex::new(Vec::new()),
            clicks_at: Mutex::new(Vec::new()),
            auth: Mutex::new(AuthState::Authenticated),
            candidate_calls: AtomicUsize::new(0),
            probes: AtomicUsize::new(0),
            screenshots: AtomicUsize::new(0),
            cookie_clears: AtomicUsize::new(0),
            landed: AtomicBool::new(false),
            stale_reads: AtomicUsize::new(0),
        }
    }

    /// The page for `kind`: its row, and a landing its verifier accepts.
    fn for_verb(kind: VerbKind) -> Self {
        match kind {
            VerbKind::AccountHome => Self::new("Profile", url("/user/me/")),
            VerbKind::Settings => Self::new("Settings", url("/settings/")),
            VerbKind::Notifications => {
                let mut page = Self::new("Notifications", url("/notifications"));
                page.landed_title = Some("Notifications".to_owned());
                page
            }
            VerbKind::LogOut => {
                let mut page = Self::new("Log Out", origin());
                page.signs_out = true;
                page
            }
        }
    }

    async fn opened(&self) -> bool {
        self.clicks.lock().await.contains(&1)
    }

    /// The landing takes effect one URL read late, like a real navigation:
    /// the post-click wait reads its baseline first, then sees the change.
    async fn land(&self) {
        self.stale_reads.store(1, Ordering::SeqCst);
        if self.signs_out {
            *self.auth.lock().await = AuthState::LoggedOut;
        }
        self.landed.store(true, Ordering::SeqCst);
    }

    fn landed(&self) -> bool {
        self.landed.load(Ordering::SeqCst)
    }

    fn candidate_calls(&self) -> usize {
        self.candidate_calls.load(Ordering::SeqCst)
    }
}

impl MenuBrowser for MenuPage {
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
        let tree = if self.opened().await && !self.landed() {
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
        // Any AX click other than the avatar is a destination row: it
        // lands like the coordinate click does.
        if element.backend_node_id != 1 {
            self.land().await;
        }
        Ok(())
    }

    fn menu_dismiss(&self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }

    fn menu_screenshot(&self) -> impl std::future::Future<Output = Option<String>> + Send {
        self.screenshots.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Some("AAAA".to_owned()))
    }

    async fn menu_click_at(
        &self,
        x: f64,
        y: f64,
        _tried: &mut Vec<String>,
    ) -> Result<(), IntentError> {
        self.clicks_at.lock().await.push((x, y));
        self.land().await;
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

    async fn menu_text_probe(&self, _x: f64, _y: f64, _word: &str) -> Option<TextProbe> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        Some(TextProbe {
            hit_text: self.probe_text.clone(),
            contains_match: true,
        })
    }
}

impl SettingsBrowser for MenuPage {
    async fn settings_current_url(&self) -> Option<Url> {
        let stale = self
            .stale_reads
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if self.landed() && !stale {
            *self.current.lock().await = self.lands_on.clone();
        }
        Some(self.current.lock().await.clone())
    }

    fn settings_node_href(
        &self,
        _backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        std::future::ready(None)
    }

    async fn settings_page_title(&self) -> Option<String> {
        if self.landed() {
            self.landed_title.clone()
        } else {
            None
        }
    }
}

impl ChromeActionBrowser for MenuPage {
    async fn chrome_auth_state(&self) -> AuthState {
        *self.auth.lock().await
    }

    async fn chrome_navigate(&self, target: &Url) -> Result<(), IntentError> {
        *self.current.lock().await = target.clone();
        Ok(())
    }

    async fn chrome_clear_host_cookies(&self, _host: &str) -> Result<usize, IntentError> {
        self.cookie_clears.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    }
}

/// Vision that would click far from any row; counts calls. Present so the
/// tests prove the text lane wins without it, not that it was absent.
struct WrongVision {
    calls: AtomicUsize,
}

impl PageNavigator for WrongVision {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        None
    }

    fn locate_visual(&self, _target: &str, _screenshot_jpeg_b64: &str) -> VisualLocation {
        self.calls.fetch_add(1, Ordering::SeqCst);
        VisualLocation::Point { x: 10.0, y: 10.0 }
    }
}

async fn run(page: &MenuPage, spec: &VerbSpec) -> Result<PageGoalOutcome, IntentError> {
    hermetic_classifier();
    pursue_chrome_action_with_vision(page, &origin(), spec, None).await
}

fn miss_diagnostic(result: Result<PageGoalOutcome, IntentError>) -> String {
    match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected NoMatch miss, got {other:?}"),
    }
}

#[tokio::test]
async fn settings_flow_avatar_then_settings_row_then_verifier() {
    hermetic_classifier();
    let page = MenuPage::for_verb(VerbKind::Settings);
    let vision = Arc::new(WrongVision {
        calls: AtomicUsize::new(0),
    });
    let navigator: Arc<dyn PageNavigator> = vision.clone();
    let spec = VerbSpec::for_kind(VerbKind::Settings);
    match pursue_chrome_action_with_vision(&page, &origin(), spec, Some(&navigator)).await {
        Ok(PageGoalOutcome::Verified {
            label,
            landed,
            tried_lines,
            ..
        }) => {
            assert_eq!(label, "text pick: settings");
            assert_eq!(landed, url("/settings/"));
            assert!(
                tried_lines
                    .iter()
                    .any(|line| line == "text_lane: click_at (1000, 420) \"settings\""),
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
    // Avatar first, then the Settings row at its rounded center.
    assert_eq!(*page.clicks.lock().await, vec![1]);
    assert_eq!(*page.clicks_at.lock().await, vec![(1000.0, 420.0)]);
    assert_eq!(page.screenshots.load(Ordering::SeqCst), 0);
    assert_eq!(vision.calls.load(Ordering::SeqCst), 0);
    assert_eq!(page.cookie_clears.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lane_fires_and_verifies_for_every_identity_menu_verb() {
    for kind in IDENTITY_VERBS {
        let page = MenuPage::for_verb(kind);
        let spec = VerbSpec::for_kind(kind);
        match run(&page, spec).await {
            Ok(PageGoalOutcome::Verified { label, .. }) => {
                assert!(label.starts_with("text pick: "), "{kind:?}: {label}");
            }
            other => panic!("{kind:?}: expected Verified, got {other:?}"),
        }
        assert!(page.candidate_calls() > 0, "{kind:?}");
        assert_eq!(
            *page.clicks_at.lock().await,
            vec![(1000.0, 420.0)],
            "{kind:?}"
        );
        assert_eq!(page.cookie_clears.load(Ordering::SeqCst), 0, "{kind:?}");
    }
}

#[tokio::test]
async fn each_verb_keeps_its_own_verifier() {
    // The Settings row clicked, but the page stayed on the root with no
    // settings title: the settings verifier refuses, the lane journals a
    // clicked miss, and the run does not complete.
    let page = MenuPage::new("Settings", origin());
    let diagnostic = miss_diagnostic(run(&page, VerbSpec::for_kind(VerbKind::Settings)).await);
    assert!(
        diagnostic.contains("text_lane: missed (verification failed)"),
        "got: {diagnostic}"
    );
    assert!(!page.clicks_at.lock().await.is_empty());
}

#[tokio::test]
async fn verb_without_vocabulary_declines_and_falls_through() {
    let page = MenuPage::new("Settings", url("/settings/"));
    let diagnostic = miss_diagnostic(run(&page, &NO_VOCABULARY_SPEC).await);
    assert!(
        diagnostic.contains("text_lane: missed (no vocabulary)"),
        "got: {diagnostic}"
    );
    assert!(page.clicks_at.lock().await.is_empty());
    assert_eq!(page.candidate_calls(), 0);
    assert_eq!(page.probes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn direct_ax_match_never_runs_the_text_lane_for_any_verb() {
    let direct_names = [
        (VerbKind::AccountHome, "Profile"),
        (VerbKind::Settings, "Settings"),
        (VerbKind::Notifications, "Notifications"),
    ];
    for (kind, name) in direct_names {
        let mut page = MenuPage::for_verb(kind);
        page.menu.push(el(11, "menuitem", name));
        let _ = run(&page, VerbSpec::for_kind(kind)).await;
        assert_eq!(page.candidate_calls(), 0, "{kind:?}");
        assert!(page.clicks.lock().await.contains(&11), "{kind:?}");
    }
}

#[tokio::test]
async fn nothing_clicked_yet_never_runs_the_text_lane_for_any_verb() {
    for kind in IDENTITY_VERBS {
        // No opener on the page: the worker defers without clicking.
        let mut page = MenuPage::for_verb(kind);
        page.baseline = vec![el(3, "link", "Home")];
        let _ = run(&page, VerbSpec::for_kind(kind)).await;
        assert!(page.clicks.lock().await.is_empty(), "{kind:?}");
        assert_eq!(page.candidate_calls(), 0, "{kind:?}");
    }
}

#[tokio::test]
async fn vision_stays_sign_out_only() {
    // Settings text miss: no vision call even with a navigator present.
    let mut page = MenuPage::for_verb(VerbKind::Settings);
    page.candidates = vec![text("Display Mode", 1000.0, 380.0)];
    let vision = Arc::new(WrongVision {
        calls: AtomicUsize::new(0),
    });
    let navigator: Arc<dyn PageNavigator> = vision.clone();
    hermetic_classifier();
    let result = pursue_chrome_action_with_vision(
        &page,
        &origin(),
        VerbSpec::for_kind(VerbKind::Settings),
        Some(&navigator),
    )
    .await;
    let diagnostic = miss_diagnostic(result);
    assert!(
        diagnostic.contains("text_lane: missed (no text match)"),
        "got: {diagnostic}"
    );
    assert_eq!(vision.calls.load(Ordering::SeqCst), 0);
    assert_eq!(page.screenshots.load(Ordering::SeqCst), 0);
}

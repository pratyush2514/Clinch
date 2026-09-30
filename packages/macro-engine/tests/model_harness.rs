//! The model-phase harness around `pursue_with_model`: stable refs, loud
//! stale refs, batch halts, loop detection, verify-before-done, and
//! screenshots on demand. Every harness event is journaled (`model_*:`).
//!
//! Pure policy (`LoopDetector`, `page_fingerprint`, `screenshot_reason`)
//! is tested directly; the loop behavior runs against a scripted fake
//! browser and a scripted navigator that records what each turn saw.

use browser_driver::{AuthState, AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::{
    ChromeActionBrowser, IntentError, LOOP_NUDGE_REPEATS, LOOP_WINDOW, LoopDetector,
    MAX_SCREENSHOTS_PER_PASS, MenuBrowser, NavigatorTurn, NormalizedAction, PageAction,
    PageGoalOutcome, PageNavigator, STAGNATION_NUDGE_TURNS, ScreenshotReason, SettingsBrowser,
    VerbKind, VerbSpec, page_fingerprint, pursue_with_model, screenshot_reason,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

fn url(raw: &str) -> Url {
    Url::parse(raw).unwrap_or_else(|error| panic!("test url {raw} must parse: {error}"))
}

fn origin() -> Url {
    url("https://www.example.com/")
}

fn button(id: i64, name: &str) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: "button".to_string(),
        name: name.to_string(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: None,
    }
}

/// Five neutral controls: rich enough that the sparse-tree screenshot
/// rule stays off, and no name carries a verb's vocabulary.
fn neutral_tree() -> Vec<AxElement> {
    ["alpha", "beta", "gamma", "delta", "epsilon"]
        .iter()
        .zip(1..)
        .map(|(name, id)| button(id, name))
        .collect()
}

fn settings_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::Settings)
}

fn logout_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::LogOut)
}

// ---- fake browser ----

/// Static tree, scripted auth. A click lands asynchronously on a
/// per-click path (or a scripted landing), like a real page. Ids in
/// `detached` fail their box-model read, like a node React removed
/// between the snapshot and the click.
struct HarnessBrowser {
    tree: Vec<AxElement>,
    clicks: Mutex<Vec<i64>>,
    url: Arc<AsyncMutex<Url>>,
    auth: AuthState,
    land_on_click: HashMap<i64, Url>,
    detached: HashSet<i64>,
    screenshot: Option<String>,
}

impl HarnessBrowser {
    fn new(tree: Vec<AxElement>, auth: AuthState) -> Self {
        Self {
            tree,
            clicks: Mutex::new(Vec::new()),
            url: Arc::new(AsyncMutex::new(origin())),
            auth,
            land_on_click: HashMap::new(),
            detached: HashSet::new(),
            screenshot: None,
        }
    }

    fn with_landing_on_click(mut self, id: i64, landing: &str) -> Self {
        self.land_on_click.insert(id, url(landing));
        self
    }

    fn with_detached(mut self, id: i64) -> Self {
        self.detached.insert(id);
        self
    }

    fn with_screenshot(mut self) -> Self {
        self.screenshot = Some("fake-jpeg-bytes".to_owned());
        self
    }

    fn clicks(&self) -> Vec<i64> {
        self.clicks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl MenuBrowser for HarnessBrowser {
    fn menu_viewport_size(&self) -> impl std::future::Future<Output = Option<(f64, f64)>> + Send {
        std::future::ready(Some((1200.0, 800.0)))
    }

    fn menu_node_rect(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Result<Highlight, BrowserError>> + Send {
        if self.detached.contains(&backend_node_id) {
            return std::future::ready(Err(BrowserError::Connection));
        }
        std::future::ready(Ok(Highlight {
            selector: format!("ax:{backend_node_id}"),
            x: 100.0,
            y: 5.0,
            width: 40.0,
            height: 40.0,
            matches: 1,
        }))
    }

    async fn menu_snapshot(&self, _origin: &Url) -> (Vec<AxElement>, AxResyncCheck, u64) {
        (
            self.tree.clone(),
            AxResyncCheck::new(self.tree.len(), None),
            0,
        )
    }

    fn menu_node_expanded(
        &self,
        _backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<bool>> + Send {
        std::future::ready(None)
    }

    async fn menu_click(&self, element: &AxElement) -> Result<(), IntentError> {
        let id = element.backend_node_id;
        let step = {
            let mut clicks = self.clicks.lock().unwrap_or_else(PoisonError::into_inner);
            clicks.push(id);
            clicks.len()
        };
        let target = self
            .land_on_click
            .get(&id)
            .cloned()
            .unwrap_or_else(|| url(&format!("https://www.example.com/harness-step-{step}")));
        let url = Arc::clone(&self.url);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            *url.lock().await = target;
        });
        Ok(())
    }

    async fn menu_dismiss(&self) {}

    async fn menu_screenshot(&self) -> Option<String> {
        self.screenshot.clone()
    }
}

impl SettingsBrowser for HarnessBrowser {
    async fn settings_current_url(&self) -> Option<Url> {
        Some(self.url.lock().await.clone())
    }

    fn settings_node_href(
        &self,
        _backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        std::future::ready(None)
    }

    async fn settings_page_title(&self) -> Option<String> {
        None
    }
}

impl ChromeActionBrowser for HarnessBrowser {
    async fn chrome_auth_state(&self) -> AuthState {
        self.auth
    }

    async fn chrome_navigate(&self, url: &Url) -> Result<(), IntentError> {
        *self.url.lock().await = url.clone();
        Ok(())
    }
}

// ---- scripted navigator ----

/// What one turn handed the navigator.
#[derive(Clone, Debug)]
struct SeenTurn {
    goal: String,
    screenshot: bool,
    notes: Vec<String>,
}

/// Plays a queue of replies (each a whole turn: `None` declines, a vec
/// may be a batch) and records every turn it saw.
struct TurnNavigator {
    queue: Mutex<VecDeque<Option<Vec<PageAction>>>>,
    seen: Mutex<Vec<SeenTurn>>,
    vision: bool,
}

impl TurnNavigator {
    fn new(replies: Vec<Option<Vec<PageAction>>>) -> Self {
        Self {
            queue: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
            vision: true,
        }
    }

    fn singles(actions: Vec<PageAction>) -> Self {
        Self::new(
            actions
                .into_iter()
                .map(|action| Some(vec![action]))
                .collect(),
        )
    }

    fn without_vision(mut self) -> Self {
        self.vision = false;
        self
    }

    fn seen(&self) -> Vec<SeenTurn> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl PageNavigator for TurnNavigator {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        None
    }

    fn next_turn(&self, turn: &NavigatorTurn<'_>) -> Option<Vec<PageAction>> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(SeenTurn {
                goal: turn.goal.to_owned(),
                screenshot: turn.screenshot_jpeg_b64.is_some(),
                notes: turn.notes.to_vec(),
            });
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .flatten()
    }

    fn accepts_screenshots(&self) -> bool {
        self.vision
    }
}

async fn run(
    browser: &HarnessBrowser,
    navigator: Arc<TurnNavigator>,
    spec: &'static VerbSpec,
) -> Result<PageGoalOutcome, IntentError> {
    pursue_with_model(
        browser,
        &origin(),
        "harness test goal",
        navigator,
        Some(spec),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await
}

fn miss(result: Result<PageGoalOutcome, IntentError>) -> String {
    match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected the honest miss, got {other:?}"),
    }
}

fn verified_tried_lines(result: Result<PageGoalOutcome, IntentError>) -> Vec<String> {
    match result {
        Ok(PageGoalOutcome::Verified { tried_lines, .. }) => tried_lines,
        other => panic!("expected Verified, got {other:?}"),
    }
}

// ---- pure policy: loop detection ----

#[test]
fn loop_nudges_fire_at_the_named_thresholds_and_escalate() {
    let mut detector = LoopDetector::new();
    let action = NormalizedAction::click(&button(1, "Open menu"));
    let mut nudged_at = Vec::new();
    let mut texts = Vec::new();
    for count in 1..=LOOP_NUDGE_REPEATS[2] {
        if let Some(nudge) = detector.record_action(&action) {
            nudged_at.push(count);
            texts.push(nudge);
        }
    }
    assert_eq!(nudged_at, LOOP_NUDGE_REPEATS.to_vec());
    assert_eq!(LOOP_NUDGE_REPEATS, [5, 8, 12], "the report's shape");
    assert_eq!(
        texts.iter().collect::<HashSet<_>>().len(),
        3,
        "each level is a different, escalating note: {texts:?}"
    );
}

#[test]
fn loop_detector_keys_clicks_on_role_and_name_not_node_id() {
    // A re-rendered control gets a fresh backend node id; it is still
    // the same action.
    let mut detector = LoopDetector::new();
    let mut nudges = 0;
    for id in (100_i64..).take(LOOP_NUDGE_REPEATS[0]) {
        let element = button(id, "  Open Menu ");
        if detector
            .record_action(&NormalizedAction::click(&element))
            .is_some()
        {
            nudges += 1;
        }
    }
    assert_eq!(nudges, 1, "five re-rendered repeats are one loop");
}

#[test]
fn loop_window_forgets_repeats_older_than_the_window() {
    let mut detector = LoopDetector::new();
    let repeated = NormalizedAction::click(&button(1, "repeated"));
    for _ in 0..LOOP_NUDGE_REPEATS[0] - 1 {
        assert!(detector.record_action(&repeated).is_none());
    }
    for id in (10_i64..).take(LOOP_WINDOW) {
        let other = NormalizedAction::click(&button(id, &format!("other {id}")));
        let _ = detector.record_action(&other);
    }
    assert!(
        detector.record_action(&repeated).is_none(),
        "the old repeats slid out of the trailing window"
    );
}

#[test]
fn stagnation_nudge_after_unchanged_observations() {
    let mut detector = LoopDetector::new();
    let page = page_fingerprint(Some(&origin()), &neutral_tree());
    assert!(detector.record_page(page).is_none(), "first observation");
    for _ in 1..STAGNATION_NUDGE_TURNS {
        assert!(detector.record_page(page).is_none());
    }
    let nudge = detector.record_page(page);
    assert!(
        nudge
            .as_deref()
            .is_some_and(|text| text.contains("has not changed")),
        "nudge after {STAGNATION_NUDGE_TURNS} unchanged observations: {nudge:?}"
    );
    // Any change resets the count.
    let other = page_fingerprint(Some(&origin()), &[button(9, "new control")]);
    assert!(detector.record_page(other).is_none());
    assert!(detector.record_page(other).is_none());
}

// ---- pure policy: page fingerprint ----

#[test]
fn page_fingerprint_ignores_node_ids_query_and_fragment() {
    let tree = neutral_tree();
    let rerendered: Vec<AxElement> = tree
        .iter()
        .map(|element| AxElement {
            backend_node_id: element.backend_node_id + 1000,
            ..element.clone()
        })
        .collect();
    let with_query = url("https://www.example.com/?tab=2#top");
    assert_eq!(
        page_fingerprint(Some(&origin()), &tree),
        page_fingerprint(Some(&with_query), &rerendered),
    );
}

#[test]
fn page_fingerprint_changes_on_path_or_control_set() {
    let tree = neutral_tree();
    let base = page_fingerprint(Some(&origin()), &tree);
    let moved = url("https://www.example.com/next");
    assert_ne!(base, page_fingerprint(Some(&moved), &tree));
    let mut opened = tree.clone();
    opened.push(button(99, "revealed row"));
    assert_ne!(base, page_fingerprint(Some(&origin()), &opened));
}

// ---- pure policy: screenshots on demand ----

#[test]
fn screenshot_policy_is_on_demand_and_capped() {
    assert_eq!(
        screenshot_reason(0, false, 40, 0),
        Some(ScreenshotReason::FirstTurn)
    );
    assert_eq!(
        screenshot_reason(1, false, 40, 1),
        None,
        "rich page, later turn"
    );
    assert_eq!(
        screenshot_reason(3, true, 40, 1),
        Some(ScreenshotReason::VerifyTurn)
    );
    assert_eq!(
        screenshot_reason(2, false, 2, 1),
        Some(ScreenshotReason::SparseTree)
    );
    assert_eq!(
        screenshot_reason(0, true, 1, MAX_SCREENSHOTS_PER_PASS),
        None,
        "the per-pass cap wins over every reason"
    );
}

// ---- loop: stale refs fail loudly ----

#[tokio::test]
async fn stale_ref_is_not_substituted_and_the_model_rereads() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated)
        .with_landing_on_click(2, "https://www.example.com/settings");
    let navigator = Arc::new(TurnNavigator::singles(vec![
        PageAction::Click { target: 99 },
        PageAction::Click { target: 2 },
    ]));
    let lines = verified_tried_lines(run(&browser, navigator.clone(), settings_spec()).await);
    assert_eq!(
        browser.clicks(),
        vec![2],
        "nothing clicked for the stale id"
    );
    assert!(
        lines.iter().any(|line| line
            == "model_stale_ref: element 99 is not on the live page; not substituted, re-reading the page"),
        "{lines:?}"
    );
    let seen = navigator.seen();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].notes.is_empty());
    assert!(
        seen[1].notes.iter().any(|note| note.contains("stale ref")),
        "the re-read turn carries the stale-ref signal: {:?}",
        seen[1].notes
    );
    assert!(
        seen[1].notes.iter().any(|note| note.contains("99")),
        "the signal names the stale id: {:?}",
        seen[1].notes
    );
}

#[tokio::test]
async fn detached_node_is_a_stale_ref_not_a_browser_error() {
    // Id 1 is in the snapshot but its box-model read fails (removed
    // between snapshot and click). Before the harness this surfaced as
    // `IntentError::Browser` and aborted the run.
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated)
        .with_detached(1)
        .with_landing_on_click(2, "https://www.example.com/settings");
    let navigator = Arc::new(TurnNavigator::singles(vec![
        PageAction::Click { target: 1 },
        PageAction::Click { target: 2 },
    ]));
    let lines = verified_tried_lines(run(&browser, navigator, settings_spec()).await);
    assert_eq!(browser.clicks(), vec![2]);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("model_stale_ref: element 1 detached or hidden")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn stale_ref_rereads_are_bounded() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated);
    let navigator = Arc::new(TurnNavigator::singles(
        (90..98)
            .map(|target| PageAction::Click { target })
            .collect(),
    ));
    let diagnostic = miss(run(&browser, navigator.clone(), settings_spec()).await);
    assert!(browser.clicks().is_empty(), "no stale id was ever clicked");
    assert_eq!(
        navigator.seen().len(),
        3,
        "two re-reads, then the pass ends"
    );
    assert!(
        diagnostic.contains("re-read budget (2) spent"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("navigator kept naming stale elements"),
        "{diagnostic}"
    );
}

// ---- loop: batch halt ----

#[tokio::test]
async fn batch_halts_on_page_change_and_journals_the_skips() {
    // One reply carries two clicks; the first navigates, so the second —
    // chosen against the old page — must not run.
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated);
    let navigator = Arc::new(TurnNavigator::new(vec![Some(vec![
        PageAction::Click { target: 1 },
        PageAction::Click { target: 2 },
    ])]));
    let diagnostic = miss(run(&browser, navigator, settings_spec()).await);
    assert_eq!(
        browser.clicks(),
        vec![1],
        "the stale half of the batch never ran"
    );
    assert!(
        diagnostic
            .contains("model_batch: halted after 1 of 2 actions (page changed); skipped [click 2]"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn batch_halts_at_the_first_failed_action() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated);
    let navigator = Arc::new(TurnNavigator::new(vec![Some(vec![
        PageAction::Click { target: 77 },
        PageAction::Click { target: 1 },
        PageAction::Done,
    ])]));
    let diagnostic = miss(run(&browser, navigator, settings_spec()).await);
    assert!(browser.clicks().is_empty(), "nothing after the failure ran");
    assert!(
        diagnostic.contains(
            "model_batch: halted after 1 of 3 actions (stale ref); skipped [click 1, done]"
        ),
        "{diagnostic}"
    );
}

// ---- loop: verify-before-done ----

#[tokio::test]
async fn done_is_verified_on_a_fresh_read_and_journaled() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::LoggedOut);
    let navigator = Arc::new(TurnNavigator::singles(vec![PageAction::Done]));
    let lines = verified_tried_lines(run(&browser, navigator, logout_spec()).await);
    assert!(
        lines
            .iter()
            .any(|line| line == "model_done_verify: verified on a fresh read of the live page"),
        "{lines:?}"
    );
}

#[tokio::test]
async fn rejected_done_is_fed_back_with_a_screenshot_then_the_model_continues() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated)
        .with_landing_on_click(3, "https://www.example.com/settings")
        .with_screenshot();
    let navigator = Arc::new(TurnNavigator::singles(vec![
        PageAction::Done,
        PageAction::Click { target: 3 },
    ]));
    let lines = verified_tried_lines(run(&browser, navigator.clone(), settings_spec()).await);
    assert!(
        lines.iter().any(|line| line
            == "model_done_verify: rejected (goal not met on the live page); model continues"),
        "{lines:?}"
    );
    let seen = navigator.seen();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1]
            .notes
            .iter()
            .any(|note| note.contains("NOT achieved")),
        "{:?}",
        seen[1].notes
    );
    assert!(
        seen[1].screenshot,
        "the verification turn carries a screenshot"
    );
    assert_eq!(browser.clicks(), vec![3]);
}

#[tokio::test]
async fn second_rejected_done_ends_the_pass() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated);
    let navigator = Arc::new(TurnNavigator::singles(vec![
        PageAction::Done,
        PageAction::Done,
        PageAction::Click { target: 1 },
    ]));
    let diagnostic = miss(run(&browser, navigator.clone(), settings_spec()).await);
    assert_eq!(
        navigator.seen().len(),
        2,
        "no third turn after two rejections"
    );
    assert!(browser.clicks().is_empty());
    assert!(
        diagnostic.contains("model_done_verify: rejected again"),
        "{diagnostic}"
    );
}

// ---- loop: screenshots on demand, every call journaled ----

#[tokio::test]
async fn screenshots_attach_on_demand_not_every_step() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated).with_screenshot();
    let navigator = Arc::new(TurnNavigator::singles(vec![
        PageAction::Click { target: 1 },
        PageAction::Click { target: 2 },
        PageAction::Click { target: 3 },
    ]));
    let diagnostic = miss(run(&browser, navigator.clone(), settings_spec()).await);
    let shots: Vec<bool> = navigator
        .seen()
        .iter()
        .map(|turn| turn.screenshot)
        .collect();
    assert_eq!(shots, vec![true, false, false, false], "first look only");
    assert!(
        diagnostic
            .contains("model_turn: step 1/8 elements 5 screenshot first-turn notes 0 -> click 1"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("model_turn: step 2/8 elements 5 screenshot none notes 0 -> click 2"),
        "{diagnostic}"
    );
    assert_eq!(
        diagnostic.matches("model_turn:").count(),
        navigator.seen().len(),
        "one journal line per model call"
    );
}

#[tokio::test]
async fn navigator_without_vision_gets_no_capture_and_says_why() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated).with_screenshot();
    let navigator = Arc::new(TurnNavigator::singles(vec![]).without_vision());
    let diagnostic = miss(run(&browser, navigator.clone(), settings_spec()).await);
    assert!(navigator.seen().iter().all(|turn| !turn.screenshot));
    assert!(
        diagnostic
            .contains("model_screenshot: skipped (first-turn; navigator has no vision), text-only"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn goal_text_is_fixed_for_the_whole_run() {
    let browser = HarnessBrowser::new(neutral_tree(), AuthState::Authenticated);
    let navigator = Arc::new(TurnNavigator::singles(vec![
        PageAction::Click { target: 1 },
        PageAction::Click { target: 99 },
        PageAction::Click { target: 2 },
    ]));
    let _ = run(&browser, navigator.clone(), settings_spec()).await;
    let seen = navigator.seen();
    assert!(seen.len() >= 3);
    assert!(
        seen.iter().all(|turn| turn.goal == seen[0].goal),
        "no per-step goal reshaping; notes travel separately"
    );
}

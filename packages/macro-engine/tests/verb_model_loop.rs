//! The generalist model loop (`pursue_with_model`): the model acts, the
//! verifier decides.
//!
//! * Up to `MODEL_GOAL_MAX_STEPS` (8) actions; exhaustion without
//!   verification is the honest miss — never Completed.
//! * `PageAction::Done` is a decline, never a completion claim: only the
//!   verb's verifier can yield success.
//! * Clicks without verification are a miss, not a completion — the
//!   service maps this `NoMatch` to FAILED/Take Control.

use browser_driver::{AuthState, AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::{
    ChromeActionBrowser, IntentError, MenuBrowser, ModelEscalation, PageAction, PageGoalOutcome,
    PageNavigator, PositionZone, SettingsBrowser, VerbKind, VerbSpec, pursue_with_model,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

fn origin() -> Url {
    Url::parse("https://www.example.com/").expect("test origin parses")
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

fn settings_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::Settings)
}

fn logout_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::LogOut)
}

/// Lean fake for the model loop: a static tree, scripted auth. Clicks
/// change the URL asynchronously (like a real page: the navigation lands
/// after the click returns), so the loop's URL-change poll resolves on
/// its first check instead of burning its 10s timeout per click.
struct LoopBrowser {
    tree: Vec<AxElement>,
    clicks: Mutex<Vec<i64>>,
    url: Arc<AsyncMutex<Url>>,
    auth: AuthState,
    land_on_click: HashMap<i64, Url>,
    screenshot: Option<String>,
    rects: HashMap<i64, (f64, f64)>,
}

impl LoopBrowser {
    fn new(tree: Vec<AxElement>, auth: AuthState) -> Self {
        Self {
            tree,
            clicks: Mutex::new(Vec::new()),
            url: Arc::new(AsyncMutex::new(origin())),
            auth,
            land_on_click: HashMap::new(),
            screenshot: None,
            rects: HashMap::new(),
        }
    }

    fn with_screenshot(mut self, jpeg_b64: &str) -> Self {
        self.screenshot = Some(jpeg_b64.to_owned());
        self
    }

    fn with_rect(mut self, id: i64, x: f64, y: f64) -> Self {
        self.rects.insert(id, (x, y));
        self
    }

    fn with_landing_on_click(mut self, id: i64, url: &str) -> Self {
        self.land_on_click
            .insert(id, Url::parse(url).expect("test landing parses"));
        self
    }

    fn clicks(&self) -> Vec<i64> {
        self.clicks.lock().expect("clicks lock").clone()
    }
}

impl MenuBrowser for LoopBrowser {
    fn menu_viewport_size(&self) -> impl std::future::Future<Output = Option<(f64, f64)>> + Send {
        std::future::ready(Some((1200.0, 800.0)))
    }

    fn menu_node_rect(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Result<Highlight, BrowserError>> + Send {
        let (x, y) = self
            .rects
            .get(&backend_node_id)
            .copied()
            .unwrap_or((backend_node_id as f64 * 10.0, 5.0));
        std::future::ready(Ok(Highlight {
            selector: format!("ax:{backend_node_id}"),
            x,
            y,
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
            let mut clicks = self.clicks.lock().expect("clicks lock");
            clicks.push(id);
            clicks.len()
        };
        // Asynchronous like a real page: the navigation lands after the
        // click returns, so the loop's poll sees a genuine change instead
        // of timing out. The path (not the query) changes: the
        // drift check compares scheme/host/port/path only.
        let target = self.land_on_click.get(&id).cloned().unwrap_or_else(|| {
            Url::parse(&format!("https://www.example.com/loop-step-{step}"))
                .expect("step url parses")
        });
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

impl SettingsBrowser for LoopBrowser {
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

impl ChromeActionBrowser for LoopBrowser {
    async fn chrome_auth_state(&self) -> AuthState {
        self.auth
    }

    async fn chrome_navigate(&self, url: &Url) -> Result<(), IntentError> {
        *self.url.lock().await = url.clone();
        Ok(())
    }
}

/// Scripted navigator: plays a queue of actions (empty queue / `None`
/// declines), counts invocations.
struct ScriptNavigator {
    queue: Mutex<VecDeque<Option<PageAction>>>,
    calls: Mutex<usize>,
}

impl ScriptNavigator {
    fn new(actions: Vec<Option<PageAction>>) -> Self {
        Self {
            queue: Mutex::new(actions.into()),
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().expect("calls lock")
    }
}

impl PageNavigator for ScriptNavigator {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        *self.calls.lock().expect("calls lock") += 1;
        self.queue.lock().expect("queue lock").pop_front().flatten()
    }
}

/// Eight model actions, none verified: the loop spends its whole budget
/// and the honest miss comes back — never Completed.
#[tokio::test]
async fn eight_unverified_model_actions_exhaust_to_miss() {
    // Distinct names: the loop rejects re-picking an already-clicked
    // control, so one shared name would decline instead of exhausting.
    let tree: Vec<AxElement> = (1..=8)
        .map(|id| button(id, &format!("loop action {id}")))
        .collect();
    let browser = LoopBrowser::new(tree, AuthState::Authenticated);
    let actions: Vec<Option<PageAction>> = (1..=8)
        .map(|id| Some(PageAction::Click { target: id }))
        .collect();
    let navigator = Arc::new(ScriptNavigator::new(actions));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "settings test goal",
        navigator.clone(),
        Some(settings_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    let diagnostic = match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected the honest miss, got {other:?}"),
    };
    assert!(
        diagnostic.contains("exhausted 8 steps"),
        "diagnostic names the exhausted budget: {diagnostic}"
    );
    assert_eq!(navigator.calls(), 8, "the full model budget is spent");
    assert_eq!(
        browser.clicks(),
        (1..=8).collect::<Vec<_>>(),
        "every proposed action executed"
    );
}

/// `Done` is a decline: with the verifier passing (already signed out)
/// the tail completes on evidence — the completion comes from the page,
/// not from the model's claim.
#[tokio::test]
async fn done_with_verifier_passing_completes_on_evidence() {
    let browser = LoopBrowser::new(vec![button(1, "account")], AuthState::LoggedOut);
    let navigator = Arc::new(ScriptNavigator::new(vec![Some(PageAction::Done)]));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "log out test goal",
        navigator,
        Some(logout_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    match result {
        Ok(PageGoalOutcome::Verified { .. }) => {}
        other => panic!("expected verifier-backed completion, got {other:?}"),
    }
}

/// `Done` without verification is the miss, never a completion: the model
/// cannot declare the goal achieved.
#[tokio::test]
async fn done_without_verification_is_miss_not_completion() {
    let browser = LoopBrowser::new(vec![button(1, "account")], AuthState::Authenticated);
    let navigator = Arc::new(ScriptNavigator::new(vec![Some(PageAction::Done)]));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "log out test goal",
        navigator,
        Some(logout_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    assert!(
        matches!(result, Err(IntentError::NoMatch(_))),
        "Done must never complete by itself: {result:?}"
    );
}

/// The loop acted (one click) but the verifier failed: the honest miss,
/// not a completion. The service maps this `NoMatch` to FAILED/Take
/// Control.
#[tokio::test]
async fn acted_but_unverified_is_miss_not_completion() {
    let browser = LoopBrowser::new(vec![button(1, "harmless")], AuthState::Authenticated);
    let navigator = Arc::new(ScriptNavigator::new(vec![
        Some(PageAction::Click { target: 1 }),
        None,
    ]));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "settings test goal",
        navigator,
        Some(settings_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    assert!(
        matches!(result, Err(IntentError::NoMatch(_))),
        "a click is not completion: {result:?}"
    );
    assert_eq!(browser.clicks(), vec![1], "the gear acted once");
}

/// Positive control: the model's click lands on a token-named URL and the
/// verifier — not the click — completes the goal.
#[tokio::test]
async fn verified_landing_after_model_click_completes() {
    let browser = LoopBrowser::new(vec![button(1, "settings")], AuthState::Authenticated)
        .with_landing_on_click(1, "https://www.example.com/settings/account");
    let navigator = Arc::new(ScriptNavigator::new(vec![Some(PageAction::Click {
        target: 1,
    })]));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "settings test goal",
        navigator,
        Some(settings_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    match result {
        Ok(PageGoalOutcome::Verified { label, landed, .. }) => {
            assert_eq!(landed.as_str(), "https://www.example.com/settings/account");
            assert_eq!(label, "settings");
        }
        other => panic!("expected verifier-backed completion, got {other:?}"),
    }
}

// ---- phase 2: the visual turn ----

/// Navigator spy for the visual turn: records whether a screenshot
/// accompanied each proposal, then declines.
struct VisualSpyNavigator {
    seen_screenshot: Mutex<Vec<bool>>,
}

impl VisualSpyNavigator {
    fn new() -> Self {
        Self {
            seen_screenshot: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<bool> {
        self.seen_screenshot.lock().expect("spy lock").clone()
    }
}

impl PageNavigator for VisualSpyNavigator {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        None
    }

    fn next_action_visual(
        &self,
        goal: &str,
        elements: &[AxElement],
        _zones: &[Option<PositionZone>],
        screenshot_jpeg_b64: Option<&str>,
    ) -> Option<PageAction> {
        self.seen_screenshot
            .lock()
            .expect("spy lock")
            .push(screenshot_jpeg_b64.is_some());
        self.next_action(goal, elements)
    }
}

#[tokio::test]
async fn visual_turn_carries_screenshot_when_captured() {
    let browser = LoopBrowser::new(vec![button(1, "account")], AuthState::Authenticated)
        .with_screenshot("fake-jpeg-bytes");
    let navigator = Arc::new(VisualSpyNavigator::new());
    let _ = pursue_with_model(
        &browser,
        &origin(),
        "settings test goal",
        navigator.clone(),
        Some(settings_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    let seen = navigator.seen();
    assert!(!seen.is_empty(), "the navigator was consulted");
    assert!(
        seen.iter().all(|screenshot| *screenshot),
        "every turn carried the captured screenshot: {seen:?}"
    );
}

#[tokio::test]
async fn visual_turn_degrades_to_text_only_without_screenshot() {
    // `menu_screenshot` returns `None`: the capture failure degrades to
    // the text-only turn instead of failing the run.
    let browser = LoopBrowser::new(vec![button(1, "account")], AuthState::Authenticated);
    let navigator = Arc::new(VisualSpyNavigator::new());
    let result = pursue_with_model(
        &browser,
        &origin(),
        "settings test goal",
        navigator.clone(),
        Some(settings_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    assert!(
        matches!(result, Err(IntentError::NoMatch(_))),
        "the run still misses honestly: {result:?}"
    );
    let seen = navigator.seen();
    assert!(!seen.is_empty(), "the navigator was consulted");
    assert!(
        seen.iter().all(|screenshot| !screenshot),
        "no turn carried a screenshot: {seen:?}"
    );
}

// ---- LogOut model-phase header filter ----

/// Navigator spy for the `LogOut` header filter: records the head ids
/// handed to each visual turn and asserts head/zones stay aligned after
/// filtering, then declines.
struct HeadRecordingNavigator {
    heads: Mutex<Vec<Vec<i64>>>,
}

impl HeadRecordingNavigator {
    fn new() -> Self {
        Self {
            heads: Mutex::new(Vec::new()),
        }
    }

    fn heads(&self) -> Vec<Vec<i64>> {
        self.heads
            .lock()
            .map(|heads| heads.clone())
            .unwrap_or_default()
    }
}

impl PageNavigator for HeadRecordingNavigator {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        None
    }

    fn next_action_visual(
        &self,
        goal: &str,
        elements: &[AxElement],
        zones: &[Option<PositionZone>],
        _screenshot_jpeg_b64: Option<&str>,
    ) -> Option<PageAction> {
        assert_eq!(
            elements.len(),
            zones.len(),
            "head and zones stay aligned after filtering"
        );
        if let Ok(mut heads) = self.heads.lock() {
            heads.push(
                elements
                    .iter()
                    .map(|element| element.backend_node_id)
                    .collect(),
            );
        }
        self.next_action(goal, elements)
    }
}

#[tokio::test]
async fn logout_model_turn_hides_below_strip_controls() {
    // Header avatar (y-center 60) plus a feed control (y-center 900):
    // the LogOut model turn offers only the header candidate — the live
    // miss clicked a feed ad's options button from this very turn.
    let tree = vec![button(1, "avatar"), button(7, "feed options")];
    let browser = LoopBrowser::new(tree, AuthState::Authenticated)
        .with_rect(1, 950.0, 40.0)
        .with_rect(7, 900.0, 880.0);
    let navigator = Arc::new(HeadRecordingNavigator::new());
    let result = pursue_with_model(
        &browser,
        &origin(),
        "log out test goal",
        navigator.clone(),
        Some(logout_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    assert!(
        matches!(result, Err(IntentError::NoMatch(_))),
        "the decline still misses honestly: {result:?}"
    );
    let heads = navigator.heads();
    assert_eq!(heads.len(), 1, "the decline ends the pass after one turn");
    assert_eq!(
        heads[0],
        vec![1],
        "only the header control reaches the model: {:?}",
        heads[0]
    );
}

#[tokio::test]
async fn logout_model_filter_falls_back_to_full_list_when_empty() {
    // Every rect sits below the strip: the filter would empty the list,
    // so the pass keeps the full head and journals one line instead of
    // blinding the model.
    let tree = vec![button(1, "avatar"), button(7, "feed options")];
    let browser = LoopBrowser::new(tree, AuthState::Authenticated)
        .with_rect(1, 950.0, 800.0)
        .with_rect(7, 900.0, 880.0);
    let navigator = Arc::new(HeadRecordingNavigator::new());
    let result = pursue_with_model(
        &browser,
        &origin(),
        "log out test goal",
        navigator.clone(),
        Some(logout_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    let diagnostic = match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected the honest miss, got {other:?}"),
    };
    let heads = navigator.heads();
    assert_eq!(heads.len(), 1, "the decline ends the pass after one turn");
    assert_eq!(
        heads[0],
        vec![1, 7],
        "the fallback keeps the full head: {:?}",
        heads[0]
    );
    assert!(
        diagnostic.contains("logout model filter found no header-strip candidates"),
        "the fallback is journaled: {diagnostic}"
    );
    assert_eq!(
        diagnostic
            .matches("logout model filter found no header-strip candidates")
            .count(),
        1,
        "exactly one journal line per pass: {diagnostic}"
    );
}

#[tokio::test]
async fn model_turn_keeps_full_list_for_other_verbs() {
    // The header filter is LogOut-only: a settings turn still offers the
    // full head, including below-strip controls.
    let tree = vec![button(1, "avatar"), button(7, "feed options")];
    let browser = LoopBrowser::new(tree, AuthState::Authenticated)
        .with_rect(1, 950.0, 40.0)
        .with_rect(7, 900.0, 880.0);
    let navigator = Arc::new(HeadRecordingNavigator::new());
    let _ = pursue_with_model(
        &browser,
        &origin(),
        "settings test goal",
        navigator.clone(),
        Some(settings_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    let heads = navigator.heads();
    assert_eq!(heads.len(), 1, "the decline ends the pass after one turn");
    assert_eq!(
        heads[0],
        vec![1, 7],
        "other verbs see the unfiltered head: {:?}",
        heads[0]
    );
}

// ---- phase 3: escalation ----

fn escalation_of(navigator: Arc<ScriptNavigator>, model_name: &str) -> ModelEscalation {
    ModelEscalation {
        navigator,
        model_name: model_name.to_owned(),
    }
}

#[tokio::test]
async fn escalation_runs_once_after_main_miss_and_verifies() {
    // The main pass declines at once; the escalation pass's click lands
    // on a token-named URL and the verifier completes the goal.
    let browser = LoopBrowser::new(vec![button(1, "settings")], AuthState::Authenticated)
        .with_landing_on_click(1, "https://www.example.com/settings");
    let main = Arc::new(ScriptNavigator::new(vec![None]));
    let escalation_navigator = Arc::new(ScriptNavigator::new(vec![Some(PageAction::Click {
        target: 1,
    })]));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "settings test goal",
        main.clone(),
        Some(settings_spec()),
        "deterministic: nothing found".to_owned(),
        Some(escalation_of(escalation_navigator.clone(), "big-model")),
    )
    .await;
    match result {
        Ok(PageGoalOutcome::Verified { landed, .. }) => {
            assert_eq!(landed.path(), "/settings");
        }
        other => panic!("expected the escalation pass to verify, got {other:?}"),
    }
    assert_eq!(main.calls(), 1, "the main pass ran once");
    assert_eq!(
        escalation_navigator.calls(),
        1,
        "exactly one escalation pass ran"
    );
}

#[tokio::test]
async fn escalation_miss_journal_names_the_model() {
    // Both passes decline: the honest miss carries the accumulated
    // journal, including the escalation line.
    let browser = LoopBrowser::new(vec![button(1, "harmless")], AuthState::Authenticated);
    let main = Arc::new(ScriptNavigator::new(vec![None]));
    let escalation_navigator = Arc::new(ScriptNavigator::new(vec![None]));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "settings test goal",
        main.clone(),
        Some(settings_spec()),
        "deterministic: nothing found".to_owned(),
        Some(escalation_of(escalation_navigator.clone(), "big-model")),
    )
    .await;
    let diagnostic = match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected the honest miss, got {other:?}"),
    };
    assert!(
        diagnostic.contains("escalated to big-model"),
        "the journal names the escalation model: {diagnostic}"
    );
    assert_eq!(main.calls(), 1, "the main pass ran once");
    assert_eq!(
        escalation_navigator.calls(),
        1,
        "exactly one escalation pass ran"
    );
}

#[tokio::test]
async fn main_verification_never_escalates() {
    // `Done` with the verifier passing (already signed out): the main
    // tail completes on evidence — the escalation navigator stays
    // untouched.
    let browser = LoopBrowser::new(vec![button(1, "account")], AuthState::LoggedOut);
    let main = Arc::new(ScriptNavigator::new(vec![Some(PageAction::Done)]));
    let escalation_navigator = Arc::new(ScriptNavigator::new(vec![Some(PageAction::Click {
        target: 1,
    })]));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "log out test goal",
        main,
        Some(logout_spec()),
        "deterministic: nothing found".to_owned(),
        Some(escalation_of(escalation_navigator.clone(), "big-model")),
    )
    .await;
    assert!(
        matches!(result, Ok(PageGoalOutcome::Verified { .. })),
        "the main tail verifies on evidence: {result:?}"
    );
    assert_eq!(
        escalation_navigator.calls(),
        0,
        "a verified main tail never escalates"
    );
}

// ---- phase 4: the bounded `LogOut` UI chain ----

fn account_home_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::AccountHome)
}

/// `LogOut` gear 2 is a short pass, not the full hunt: eight proposed
/// actions spend only three steps, then the honest miss comes back and
/// the caller (the cookie fallback) takes over.
#[tokio::test]
async fn logout_model_pass_capped_at_three_steps() {
    // Distinct names: the loop rejects re-picking an already-clicked
    // control, so one shared name would decline instead of exhausting.
    let tree: Vec<AxElement> = (1..=8)
        .map(|id| button(id, &format!("logout loop action {id}")))
        .collect();
    let browser = LoopBrowser::new(tree, AuthState::Authenticated);
    let actions: Vec<Option<PageAction>> = (1..=8)
        .map(|id| Some(PageAction::Click { target: id }))
        .collect();
    let navigator = Arc::new(ScriptNavigator::new(actions));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "log out test goal",
        navigator.clone(),
        Some(logout_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    let diagnostic = match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected the honest miss, got {other:?}"),
    };
    assert!(
        diagnostic.contains("exhausted 3 steps"),
        "diagnostic names the bounded budget: {diagnostic}"
    );
    assert_eq!(navigator.calls(), 3, "the logout model budget is 3 steps");
    assert_eq!(
        browser.clicks(),
        vec![1, 2, 3],
        "only the first three proposed actions executed"
    );
}

/// Non-logout verbs are unaffected by the bound: the full eight-step
/// budget still applies.
#[tokio::test]
async fn account_home_model_pass_keeps_eight_steps() {
    // Neutral names: an "account"/"profile"-worded label would satisfy
    // the identity verifier on its own, ending the pass early.
    let tree: Vec<AxElement> = (1..=8)
        .map(|id| button(id, &format!("home loop action {id}")))
        .collect();
    let browser = LoopBrowser::new(tree, AuthState::Authenticated);
    let actions: Vec<Option<PageAction>> = (1..=8)
        .map(|id| Some(PageAction::Click { target: id }))
        .collect();
    let navigator = Arc::new(ScriptNavigator::new(actions));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "account home test goal",
        navigator.clone(),
        Some(account_home_spec()),
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    let diagnostic = match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected the honest miss, got {other:?}"),
    };
    assert!(
        diagnostic.contains("exhausted 8 steps"),
        "diagnostic names the full budget: {diagnostic}"
    );
    assert_eq!(navigator.calls(), 8, "non-logout verbs keep 8 steps");
}

/// The bounded `LogOut` chain runs exactly one model pass: even with an
/// escalation navigator configured, no escalation pass runs — the
/// cookie fallback is the deterministic backstop.
#[tokio::test]
async fn logout_model_pass_never_escalates() {
    let tree: Vec<AxElement> = (1..=3)
        .map(|id| button(id, &format!("logout loop action {id}")))
        .collect();
    let browser = LoopBrowser::new(tree, AuthState::Authenticated);
    let main = Arc::new(ScriptNavigator::new(vec![None]));
    let escalation_navigator = Arc::new(ScriptNavigator::new(vec![Some(PageAction::Click {
        target: 1,
    })]));
    let result = pursue_with_model(
        &browser,
        &origin(),
        "log out test goal",
        main,
        Some(logout_spec()),
        "deterministic: nothing found".to_owned(),
        Some(escalation_of(escalation_navigator.clone(), "big-model")),
    )
    .await;
    let diagnostic = match result {
        Err(IntentError::NoMatch(diagnostic)) => diagnostic,
        other => panic!("expected the honest miss, got {other:?}"),
    };
    assert!(
        !diagnostic.contains("escalated to"),
        "no escalation line in the logout journal: {diagnostic}"
    );
    assert_eq!(
        escalation_navigator.calls(),
        0,
        "the bounded logout chain suppresses the escalation pass"
    );
}

//! Spec-parameterized chrome worker: the revealed-item matcher plus the
//! full worker flow against a scripted fake, for every verb row.
//!
//! [`pursue_chrome_action`] is generic over [`ChromeActionBrowser`]; the
//! fake below scripts snapshots (steady tree, scripted menu reveal),
//! recorded clicks, a scripted URL (flipped with a delay, the way real
//! navigation commits after the CDP click returns), scripted `node_href`
//! answers, a scripted document title, and a scripted auth state — so the
//! menu-open → revealed-click → verify flow is proven with no Chromium.

use browser_driver::{AuthState, AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::{
    ChromeActionBrowser, ClickedControl, IntentError, MenuBrowser, PageGoalOutcome,
    SettingsBrowser, VerbKind, VerbSpec, chrome_action_miss_diagnostic, pursue_chrome_action,
    select_revealed_action,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use url::Url;

fn origin() -> Url {
    Url::parse("https://www.example.com/")
        .unwrap_or_else(|error| panic!("test origin parses: {error}"))
}

fn url(path: &str) -> Url {
    Url::parse(&format!("https://www.example.com{path}"))
        .unwrap_or_else(|error| panic!("test url parses: {error}"))
}

fn el(id: i64, role: &str, name: &str, landmark: Option<&str>) -> AxElement {
    AxElement {
        backend_node_id: id,
        role: role.to_string(),
        name: name.to_string(),
        description: String::new(),
        container_text: Vec::new(),
        landmark: landmark.map(str::to_string),
    }
}

fn avatar() -> AxElement {
    el(1, "button", "Open user actions", None)
}

fn clicked_avatar() -> Vec<ClickedControl> {
    vec![ClickedControl::of(&avatar())]
}

fn settings_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::Settings)
}

fn account_home_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::AccountHome)
}

fn log_out_spec() -> &'static VerbSpec {
    VerbSpec::for_kind(VerbKind::LogOut)
}

// ---- scripted ChromeActionBrowser fake ----

/// Hermetic stand-in for a live page behind [`pursue_chrome_action`]: a
/// [`MenuBrowser`] + [`SettingsBrowser`] fake (steady snapshot tree,
/// recorded clicks, scripted menu reveal, scripted URL/title/hrefs) plus
/// the [`ChromeActionBrowser`] seam — a scripted auth state (flippable per
/// click) and a navigate that commits synchronously like the worker's
/// validated-href path expects.
struct FakeChromeActionBrowser {
    tree: Vec<AxElement>,
    raw_nodes: usize,
    clicks: Mutex<Vec<i64>>,
    dismisses: Mutex<usize>,
    expanded: HashMap<i64, bool>,
    opens_menu_on_click: Option<i64>,
    menu_tree: Vec<AxElement>,
    tree_on_click: HashMap<i64, Vec<AxElement>>,
    viewport: Option<(f64, f64)>,
    rects: HashMap<i64, (f64, f64)>,
    url: Arc<Mutex<Url>>,
    title: Mutex<Option<String>>,
    hrefs: HashMap<i64, String>,
    navigate_on_click: HashMap<i64, Url>,
    auth: Mutex<AuthState>,
    auth_on_click: HashMap<i64, AuthState>,
}

impl FakeChromeActionBrowser {
    fn new(tree: Vec<AxElement>) -> Self {
        Self {
            tree,
            raw_nodes: 10,
            clicks: Mutex::new(Vec::new()),
            dismisses: Mutex::new(0),
            expanded: HashMap::new(),
            opens_menu_on_click: None,
            menu_tree: Vec::new(),
            tree_on_click: HashMap::new(),
            viewport: Some((1200.0, 800.0)),
            rects: HashMap::new(),
            url: Arc::new(Mutex::new(origin())),
            title: Mutex::new(None),
            hrefs: HashMap::new(),
            navigate_on_click: HashMap::new(),
            auth: Mutex::new(AuthState::Authenticated),
            auth_on_click: HashMap::new(),
        }
    }

    fn with_rect(mut self, id: i64, x: f64, y: f64) -> Self {
        self.rects.insert(id, (x, y));
        self
    }

    fn with_menu_on_click(mut self, id: i64, menu_tree: Vec<AxElement>) -> Self {
        self.opens_menu_on_click = Some(id);
        self.menu_tree = menu_tree;
        self
    }

    fn with_tree_on_click(mut self, id: i64, tree: Vec<AxElement>) -> Self {
        self.tree_on_click.insert(id, tree);
        self
    }

    fn with_navigate_on_click(mut self, id: i64, target: Url) -> Self {
        self.navigate_on_click.insert(id, target);
        self
    }

    fn with_href(mut self, id: i64, href: &str) -> Self {
        self.hrefs.insert(id, href.to_string());
        self
    }

    fn with_auth(self, state: AuthState) -> Self {
        Self {
            auth: Mutex::new(state),
            ..self
        }
    }

    fn with_auth_on_click(mut self, id: i64, state: AuthState) -> Self {
        self.auth_on_click.insert(id, state);
        self
    }

    /// Snapshot tree for the clicks so far: later clicks win, so a
    /// revealed-destination click can replace the menu tree with the
    /// post-click page.
    fn current_tree(&self, clicks: &[i64]) -> Vec<AxElement> {
        let mut tree = self.tree.clone();
        for id in clicks {
            if self.opens_menu_on_click.is_some_and(|open| open == *id) {
                tree.clone_from(&self.menu_tree);
            }
            if let Some(next) = self.tree_on_click.get(id) {
                tree.clone_from(next);
            }
        }
        tree
    }

    async fn clicks(&self) -> Vec<i64> {
        self.clicks.lock().await.clone()
    }
}

impl MenuBrowser for FakeChromeActionBrowser {
    fn menu_viewport_size(&self) -> impl std::future::Future<Output = Option<(f64, f64)>> + Send {
        std::future::ready(self.viewport)
    }

    fn menu_node_rect(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Result<Highlight, BrowserError>> + Send {
        let (x, y) = self
            .rects
            .get(&backend_node_id)
            .copied()
            .unwrap_or((0.0, 0.0));
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
        let clicks = self.clicks.lock().await;
        let tree = self.current_tree(&clicks);
        (tree, AxResyncCheck::new(self.raw_nodes, None), 0)
    }

    fn menu_node_expanded(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<bool>> + Send {
        std::future::ready(self.expanded.get(&backend_node_id).copied())
    }

    async fn menu_click(&self, element: &AxElement) -> Result<(), IntentError> {
        let id = element.backend_node_id;
        self.clicks.lock().await.push(id);
        if let Some(target) = self.navigate_on_click.get(&id).cloned() {
            let url = Arc::clone(&self.url);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                *url.lock().await = target;
            });
        }
        if let Some(state) = self.auth_on_click.get(&id).copied() {
            *self.auth.lock().await = state;
        }
        Ok(())
    }

    async fn menu_dismiss(&self) {
        *self.dismisses.lock().await += 1;
    }
}

impl SettingsBrowser for FakeChromeActionBrowser {
    async fn settings_current_url(&self) -> Option<Url> {
        Some(self.url.lock().await.clone())
    }

    fn settings_node_href(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        std::future::ready(self.hrefs.get(&backend_node_id).cloned())
    }

    async fn settings_page_title(&self) -> Option<String> {
        self.title.lock().await.clone()
    }
}

impl ChromeActionBrowser for FakeChromeActionBrowser {
    async fn chrome_auth_state(&self) -> AuthState {
        *self.auth.lock().await
    }

    async fn chrome_navigate(&self, url: &Url) -> Result<(), IntentError> {
        *self.url.lock().await = url.clone();
        Ok(())
    }
}

// ---- select_revealed_action: pure matcher ----

#[test]
fn revealed_action_selects_settings_wording() {
    let elements = vec![avatar(), el(10, "menuitem", "Settings", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    let selected = select_revealed_action(&elements, &clicked_avatar(), &seen, settings_spec());
    assert_eq!(selected.map(|el| el.backend_node_id), Some(10));
}

#[test]
fn revealed_action_selects_preferences_wording() {
    let elements = vec![avatar(), el(10, "menuitem", "Preferences", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    let selected = select_revealed_action(&elements, &clicked_avatar(), &seen, settings_spec());
    assert_eq!(selected.map(|el| el.backend_node_id), Some(10));
}

#[test]
fn revealed_action_selects_logout_wording() {
    let elements = vec![avatar(), el(10, "menuitem", "Log out", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    let selected = select_revealed_action(&elements, &clicked_avatar(), &seen, log_out_spec());
    assert_eq!(selected.map(|el| el.backend_node_id), Some(10));
}

#[test]
fn revealed_action_rejects_login_wording_for_logout() {
    // "Log in" is not the log-out verb: the match is exact per vocabulary
    // word, never a prefix.
    let elements = vec![avatar(), el(10, "menuitem", "Log in", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    assert!(select_revealed_action(&elements, &clicked_avatar(), &seen, log_out_spec()).is_none());
}

#[test]
fn revealed_action_ignores_stale_header_controls() {
    // A "Settings" control that was already on the page before the menu
    // opened is chrome, not a revealed destination.
    let elements = vec![avatar(), el(10, "menuitem", "Settings", None)];
    let seen: HashSet<i64> = [1, 10].into_iter().collect();
    assert!(select_revealed_action(&elements, &clicked_avatar(), &seen, settings_spec()).is_none());
}

#[test]
fn revealed_action_skips_non_actionable_roles() {
    // Headings name the surface but are not clickable destinations.
    let elements = vec![avatar(), el(20, "heading", "Settings", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    assert!(select_revealed_action(&elements, &clicked_avatar(), &seen, settings_spec()).is_none());
}

#[test]
fn revealed_action_skips_already_clicked() {
    let settings = el(10, "menuitem", "Settings", None);
    let elements = vec![avatar(), settings.clone()];
    let seen: HashSet<i64> = [1].into_iter().collect();
    let clicked = vec![ClickedControl::of(&avatar()), ClickedControl::of(&settings)];
    assert!(select_revealed_action(&elements, &clicked, &seen, settings_spec()).is_none());
}

#[test]
fn revealed_action_gated_until_worker_opens_menu() {
    // A bare page's "Settings" footer link never qualifies: the worker
    // must have opened something first (clicked-empty guard).
    let elements = vec![el(5, "link", "Settings", Some("contentinfo"))];
    let seen: HashSet<i64> = HashSet::new();
    assert!(select_revealed_action(&elements, &[], &seen, settings_spec()).is_none());
}

#[test]
fn revealed_action_identity_lane_accepts_username_handle() {
    // The identity lane treats a revealed u/name handle as a profile
    // destination even with no profile wording.
    let elements = vec![avatar(), el(10, "menuitem", "u/someuser", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    let selected = select_revealed_action(&elements, &clicked_avatar(), &seen, account_home_spec());
    assert_eq!(selected.map(|el| el.backend_node_id), Some(10));
}

#[test]
fn revealed_action_settings_lane_rejects_username_handle() {
    // The handle match is identity-lane-only: a u/name item is not a
    // settings destination.
    let elements = vec![avatar(), el(10, "menuitem", "u/someuser", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    assert!(select_revealed_action(&elements, &clicked_avatar(), &seen, settings_spec()).is_none());
}

#[test]
fn chrome_action_miss_diagnostic_reports_no_control_found() {
    let diagnostic = chrome_action_miss_diagnostic(settings_spec(), &[]);
    assert!(
        diagnostic.contains("no settings control"),
        "got: {diagnostic}"
    );
    let diagnostic = chrome_action_miss_diagnostic(log_out_spec(), &[]);
    assert!(
        diagnostic.contains("no log out control"),
        "got: {diagnostic}"
    );
}

#[test]
fn chrome_action_miss_diagnostic_lists_tried_clicks() {
    let diagnostic = chrome_action_miss_diagnostic(
        settings_spec(),
        &["button 'Open user actions' → no new controls".to_string()],
    );
    assert!(diagnostic.contains("Tried:"), "got: {diagnostic}");
    assert!(
        diagnostic.contains("Open user actions"),
        "got: {diagnostic}"
    );
}

// ---- pursue_chrome_action: full flow, settings verb ----

#[tokio::test]
async fn settings_flow_navigates_to_settings_path() {
    // Menu opens on the avatar click, revealing a "Settings" menuitem;
    // clicking it navigates to a settings path.
    let baseline = vec![avatar(), el(2, "link", "Home", None)];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Settings", None));
    let fake = FakeChromeActionBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_navigate_on_click(10, url("/settings"));

    match pursue_chrome_action(&fake, &origin(), settings_spec()).await {
        Ok(PageGoalOutcome::Navigated { label, landed }) => {
            assert_eq!(label, "Settings");
            assert_eq!(landed.path(), "/settings");
        }
        other => panic!("expected Navigated, got {other:?}"),
    }
    // Avatar opened the menu, then the revealed Settings item — nothing else.
    assert_eq!(fake.clicks().await, vec![1, 10]);
}

#[tokio::test]
async fn settings_flow_verifies_disclosure_via_heading() {
    // Clicking the revealed "Preferences" item navigates nowhere; the
    // post-click page names settings in a heading instead. The title is
    // left unset so the heading — not the title — carries the verdict.
    let baseline = vec![avatar(), el(2, "link", "Home", None)];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Preferences", None));
    let mut disclosed_tree = menu_tree.clone();
    disclosed_tree.push(el(20, "heading", "Preferences", None));
    let fake = FakeChromeActionBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_tree_on_click(10, disclosed_tree);

    match pursue_chrome_action(&fake, &origin(), settings_spec()).await {
        Ok(PageGoalOutcome::Verified {
            label,
            landed,
            username,
        }) => {
            assert_eq!(label, "Preferences");
            assert_eq!(landed, origin());
            assert_eq!(username, None);
        }
        other => panic!("expected Verified, got {other:?}"),
    }
    assert_eq!(fake.clicks().await, vec![1, 10]);
}

#[tokio::test]
async fn settings_flow_selects_blank_link_by_settings_href() {
    // The revealed control is a blank-named link: the name half of the
    // matcher sees nothing, but its page-revealed href points at a
    // settings path (stemmed: "user-settings" → "setting").
    let baseline = vec![avatar(), el(2, "link", "Home", None)];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(11, "link", "", None));
    let fake = FakeChromeActionBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_href(11, "/user-settings")
        .with_navigate_on_click(11, url("/user-settings"));

    match pursue_chrome_action(&fake, &origin(), settings_spec()).await {
        Ok(PageGoalOutcome::Navigated { landed, .. }) => {
            assert_eq!(landed.path(), "/user-settings");
        }
        other => panic!("expected Navigated, got {other:?}"),
    }
    assert_eq!(fake.clicks().await, vec![1, 11]);
}

#[tokio::test]
async fn settings_flow_misses_when_menu_has_no_candidates() {
    // No menu candidates at all: the primitive misses immediately with
    // an empty tried journal, and the worker reports the honest miss.
    let baseline = vec![el(2, "link", "Home", None), el(3, "link", "Docs", None)];
    let fake = FakeChromeActionBrowser::new(baseline);

    match pursue_chrome_action(&fake, &origin(), settings_spec()).await {
        Err(IntentError::NoMatch(diagnostic)) => {
            assert!(
                diagnostic.contains("no settings control"),
                "got: {diagnostic}"
            );
        }
        other => panic!("expected NoMatch miss, got {other:?}"),
    }
    assert!(fake.clicks().await.is_empty());
}

#[tokio::test]
async fn settings_flow_never_clicks_wrong_revealed_item() {
    // The menu opens but reveals only a "Profile" item: it matches no
    // settings vocabulary, and the pre-existing footer "Settings" link is
    // stale chrome — neither is clicked.
    let baseline = vec![
        avatar(),
        el(5, "link", "Settings", Some("contentinfo")),
        el(2, "link", "Home", None),
    ];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Profile", None));
    let fake = FakeChromeActionBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree);

    match pursue_chrome_action(&fake, &origin(), settings_spec()).await {
        Err(IntentError::NoMatch(diagnostic)) => {
            assert!(diagnostic.contains("Tried:"), "got: {diagnostic}");
            assert!(
                diagnostic.contains("Open user actions"),
                "got: {diagnostic}"
            );
        }
        other => panic!("expected NoMatch miss, got {other:?}"),
    }
    // The avatar was tried as a menu candidate; the revealed Profile item
    // (id 10) and the footer Settings link (id 5) were never touched.
    assert_eq!(fake.clicks().await, vec![1]);
}

#[tokio::test]
async fn logout_flow_dismisses_wrong_menu_before_next_attempt() {
    // Live Reddit logout shape: the first candidate opens a menu whose
    // items carry no logout vocabulary. The worker must dismiss the open
    // popup before spending its next menu-opening click — otherwise the
    // popup's light-dismiss swallows that click instead of letting it
    // reach the next candidate, and logout never executes.
    let avatar = el(1, "button", "User Avatar Expand user menu", None);
    let baseline = vec![
        avatar,
        el(2, "button", "Notifications", None),
        el(3, "link", "Home", None),
    ];
    let mut wrong_menu = baseline.clone();
    wrong_menu.push(el(10, "menuitem", "Profile", None));
    let fake = FakeChromeActionBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_rect(2, 850.0, 50.0)
        .with_menu_on_click(1, wrong_menu);

    match pursue_chrome_action(&fake, &origin(), log_out_spec()).await {
        Err(IntentError::NoMatch(diagnostic)) => {
            assert!(
                diagnostic.contains("dismissed possibly-open menu"),
                "got: {diagnostic}"
            );
        }
        other => panic!("expected NoMatch miss, got {other:?}"),
    }
    // The account-worded header button was tried first (not the
    // feed-content menu), the wrong menu was dismissed, and the next
    // candidate got its click.
    assert_eq!(fake.clicks().await, vec![1, 2]);
    assert!(
        *fake.dismisses.lock().await >= 1,
        "expected a dismiss between menu attempts"
    );
}

// ---- pursue_chrome_action: full flow, account-home verb ----

#[tokio::test]
async fn account_home_flow_short_circuits_on_guest_landing() {
    // A signed-out page has no identity chrome: the worker stops before
    // any click.
    let fake = FakeChromeActionBrowser::new(vec![avatar(), el(2, "link", "Home", None)])
        .with_auth(AuthState::LoggedOut);

    match pursue_chrome_action(&fake, &origin(), account_home_spec()).await {
        Ok(PageGoalOutcome::SignedOut) => {}
        other => panic!("expected SignedOut, got {other:?}"),
    }
    assert!(fake.clicks().await.is_empty());
}

#[tokio::test]
async fn account_home_flow_navigates_revealed_href_and_verifies() {
    // Menu opens on the avatar click, revealing a "Profile" link whose
    // page-revealed href names the user; the worker navigates the href
    // directly and the verifier confirms the username in the landing.
    let baseline = vec![avatar(), el(2, "link", "Home", None)];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "link", "Profile", None));
    let fake = FakeChromeActionBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_href(10, "/user/someuser/");

    match pursue_chrome_action(&fake, &origin(), account_home_spec()).await {
        Ok(PageGoalOutcome::Verified {
            label,
            landed,
            username,
        }) => {
            assert_eq!(label, "Profile");
            assert_eq!(landed.path(), "/user/someuser/");
            assert_eq!(username.as_deref(), Some("someuser"));
        }
        other => panic!("expected Verified, got {other:?}"),
    }
    // The avatar opened the menu; the revealed link was navigated, not
    // clicked.
    assert_eq!(fake.clicks().await, vec![1]);
}

// ---- pursue_chrome_action: full flow, log-out verb ----

#[tokio::test]
async fn logout_flow_completes_when_already_signed_out() {
    // The page already reads signed out: the goal is achieved with no clicks.
    let fake = FakeChromeActionBrowser::new(vec![avatar(), el(2, "link", "Home", None)])
        .with_auth(AuthState::LoggedOut);

    match pursue_chrome_action(&fake, &origin(), log_out_spec()).await {
        Ok(PageGoalOutcome::AlreadyThere { landed }) => {
            assert_eq!(landed, origin());
        }
        other => panic!("expected AlreadyThere, got {other:?}"),
    }
    assert!(fake.clicks().await.is_empty());
}

#[tokio::test]
async fn logout_flow_verifies_signed_out_after_click() {
    // Menu opens on the avatar click, revealing "Log out"; clicking it
    // navigates away and the page reads signed out — the verifier passes.
    let baseline = vec![avatar(), el(2, "link", "Home", None)];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Log out", None));
    let fake = FakeChromeActionBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_navigate_on_click(10, url("/login"))
        .with_auth_on_click(10, AuthState::LoggedOut);

    match pursue_chrome_action(&fake, &origin(), log_out_spec()).await {
        Ok(PageGoalOutcome::Verified {
            label,
            landed,
            username,
        }) => {
            assert_eq!(label, "Log out");
            assert_eq!(landed.path(), "/login");
            assert_eq!(username, None);
        }
        other => panic!("expected Verified, got {other:?}"),
    }
    assert_eq!(fake.clicks().await, vec![1, 10]);
}

#[tokio::test]
async fn logout_flow_misses_when_still_signed_in() {
    // The "Log out" click navigates but the page still reads signed in:
    // an honest miss, never a claimed success.
    let baseline = vec![avatar(), el(2, "link", "Home", None)];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Log out", None));
    let fake = FakeChromeActionBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_navigate_on_click(10, url("/home"));

    match pursue_chrome_action(&fake, &origin(), log_out_spec()).await {
        Err(IntentError::NoMatch(diagnostic)) => {
            assert!(diagnostic.contains("still signed in"), "got: {diagnostic}");
        }
        other => panic!("expected NoMatch miss, got {other:?}"),
    }
    assert_eq!(fake.clicks().await, vec![1, 10]);
}

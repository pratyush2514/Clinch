//! First-class settings worker: the revealed-settings matcher plus the
//! full worker flow against a scripted fake.
//!
//! [`pursue_settings_chrome_inner`] is the generic worker over
//! [`SettingsBrowser`]; the fake below extends the [`menu_open`](crate)
//! `MenuBrowser` double with a scripted URL (flipped with a delay, the
//! way real navigation commits after the CDP click returns), scripted
//! `node_href` answers, and a scripted document title — so the
//! menu-open → revealed-click → navigate/verify flow is proven with no
//! Chromium.

use browser_driver::{AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::{
    ClickedControl, IntentError, MenuBrowser, PageGoalOutcome, SettingsBrowser,
    pursue_settings_chrome_inner, select_revealed_settings, settings_miss_diagnostic,
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

// ---- scripted SettingsBrowser fake ----

/// Hermetic stand-in for a live page behind [`pursue_settings_chrome_inner`]:
/// the [`menu_open`](crate) `MenuBrowser` behavior (steady snapshot tree,
/// recorded clicks, scripted menu reveal) plus the settings seam —
/// [`SettingsBrowser`]: a scripted current URL, scripted `node_href`
/// answers, and a scripted document title. Clicks that should navigate
/// flip the URL after a short delay, modeling reality: the CDP click
/// returns before navigation commits, so the worker's post-click `from`
/// read still sees the old URL.
struct FakeSettingsBrowser {
    tree: Vec<AxElement>,
    raw_nodes: usize,
    clicks: Mutex<Vec<i64>>,
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
}

impl FakeSettingsBrowser {
    fn new(tree: Vec<AxElement>) -> Self {
        Self {
            tree,
            raw_nodes: 10,
            clicks: Mutex::new(Vec::new()),
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

impl MenuBrowser for FakeSettingsBrowser {
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
        Ok(())
    }
}

impl SettingsBrowser for FakeSettingsBrowser {
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

// ---- select_revealed_settings: pure matcher ----

#[test]
fn revealed_settings_selects_settings_wording() {
    let elements = vec![avatar(), el(10, "menuitem", "Settings", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    let selected = select_revealed_settings(&elements, &clicked_avatar(), &seen);
    assert_eq!(selected.map(|el| el.backend_node_id), Some(10));
}

#[test]
fn revealed_settings_selects_preferences_wording() {
    let elements = vec![avatar(), el(10, "menuitem", "Preferences", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    let selected = select_revealed_settings(&elements, &clicked_avatar(), &seen);
    assert_eq!(selected.map(|el| el.backend_node_id), Some(10));
}

#[test]
fn revealed_settings_ignores_stale_header_controls() {
    // A "Settings" control that was already on the page before the menu
    // opened is chrome, not a revealed destination.
    let elements = vec![avatar(), el(10, "menuitem", "Settings", None)];
    let seen: HashSet<i64> = [1, 10].into_iter().collect();
    assert!(select_revealed_settings(&elements, &clicked_avatar(), &seen).is_none());
}

#[test]
fn revealed_settings_skips_non_actionable_roles() {
    // Headings name the surface but are not clickable destinations.
    let elements = vec![avatar(), el(20, "heading", "Settings", None)];
    let seen: HashSet<i64> = [1].into_iter().collect();
    assert!(select_revealed_settings(&elements, &clicked_avatar(), &seen).is_none());
}

#[test]
fn revealed_settings_skips_already_clicked() {
    let settings = el(10, "menuitem", "Settings", None);
    let elements = vec![avatar(), settings.clone()];
    let seen: HashSet<i64> = [1].into_iter().collect();
    let clicked = vec![ClickedControl::of(&avatar()), ClickedControl::of(&settings)];
    assert!(select_revealed_settings(&elements, &clicked, &seen).is_none());
}

#[test]
fn revealed_settings_gated_until_worker_opens_menu() {
    // A bare page's "Settings" footer link never qualifies: the worker
    // must have opened something first (clicked-empty guard).
    let elements = vec![el(5, "link", "Settings", Some("contentinfo"))];
    let seen: HashSet<i64> = HashSet::new();
    assert!(select_revealed_settings(&elements, &[], &seen).is_none());
}

#[test]
fn settings_miss_diagnostic_reports_no_control_found() {
    let diagnostic = settings_miss_diagnostic(&[]);
    assert!(
        diagnostic.contains("no settings control"),
        "got: {diagnostic}"
    );
}

#[test]
fn settings_miss_diagnostic_lists_tried_clicks() {
    let diagnostic =
        settings_miss_diagnostic(&["button 'Open user actions' → no new controls".to_string()]);
    assert!(diagnostic.contains("Tried:"), "got: {diagnostic}");
    assert!(
        diagnostic.contains("Open user actions"),
        "got: {diagnostic}"
    );
}

// ---- pursue_settings_chrome_inner: full flow ----

#[tokio::test]
async fn settings_flow_navigates_to_settings_path() {
    // Menu opens on the avatar click, revealing a "Settings" menuitem;
    // clicking it navigates to a settings path.
    let baseline = vec![avatar(), el(2, "link", "Home", None)];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Settings", None));
    let fake = FakeSettingsBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_navigate_on_click(10, url("/settings"));

    match pursue_settings_chrome_inner(&fake, &origin()).await {
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
    let fake = FakeSettingsBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_tree_on_click(10, disclosed_tree);

    match pursue_settings_chrome_inner(&fake, &origin()).await {
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
    let fake = FakeSettingsBrowser::new(baseline)
        .with_rect(1, 950.0, 50.0)
        .with_menu_on_click(1, menu_tree)
        .with_href(11, "/user-settings")
        .with_navigate_on_click(11, url("/user-settings"));

    match pursue_settings_chrome_inner(&fake, &origin()).await {
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
    let fake = FakeSettingsBrowser::new(baseline);

    match pursue_settings_chrome_inner(&fake, &origin()).await {
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
async fn settings_flow_never_clicks_footer_link_without_menu() {
    // Bare page with a "Settings" footer link: the one menu candidate
    // never opens anything, and the footer link is never clicked — the
    // clicked-empty guard holds for the whole run.
    let baseline = vec![
        avatar(),
        el(5, "link", "Settings", Some("contentinfo")),
        el(2, "link", "Home", None),
    ];
    let fake = FakeSettingsBrowser::new(baseline).with_rect(1, 950.0, 50.0);

    match pursue_settings_chrome_inner(&fake, &origin()).await {
        Err(IntentError::NoMatch(diagnostic)) => {
            assert!(diagnostic.contains("Tried:"), "got: {diagnostic}");
            assert!(
                diagnostic.contains("Open user actions"),
                "got: {diagnostic}"
            );
        }
        other => panic!("expected NoMatch miss, got {other:?}"),
    }
    // The avatar was tried as a menu candidate; the footer Settings link
    // (id 5) was never touched.
    assert_eq!(fake.clicks().await, vec![1]);
}

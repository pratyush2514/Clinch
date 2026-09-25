//! Shared menu-opening primitive: pure candidate ranking plus the async
//! click → poll-for-evidence → retry loop.
//!
//! `rank_menu_candidates` orders the header strip first, then page
//! content: within each pass, (a) landmarked banner/navigation buttons,
//! (b) account-worded buttons, and — strip pass only — (c) blank-named
//! buttons inside the header strip (rightmost first), (d) remaining
//! in-strip buttons (rightmost first). The loop itself is driven against
//! a scripted [`MenuBrowser`] fake — no Chromium needed: the fake serves
//! one steady snapshot tree (a poll that outlasts the script sees a steady
//! tree instead of hanging), records clicks, and flips scripted page
//! state, proving the retry order, the evidence poll, and the timing
//! bounds.

use browser_driver::{AxElement, AxResyncCheck, BrowserError, Highlight};
use macro_engine::{
    ClickedControl, IntentError, MenuBrowser, MenuOpenBaseline, OpenMenuOutcome,
    open_identity_menu, rank_menu_candidates,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use url::Url;

fn origin() -> Url {
    Url::parse("https://www.example.com/")
        .unwrap_or_else(|error| panic!("test origin parses: {error}"))
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

fn ids(ranked: &[&AxElement]) -> Vec<i64> {
    ranked.iter().map(|el| el.backend_node_id).collect()
}

#[test]
fn header_strip_outranks_content() {
    // The live Reddit shape: a feed-content button carrying a navigation
    // landmark must not outrank the header's account button — the worker
    // clicked the post's "Open user actions" menu first, and the open
    // popup's light-dismiss then swallowed the avatar-menu click.
    let elements = vec![
        el(1, "button", "Open user actions", Some("navigation")),
        el(2, "button", "User Avatar Expand user menu", None),
        el(3, "button", "", None),
        el(4, "button", "Chat", None),
        el(5, "link", "User profile", None),
    ];
    // Avatar in the header strip; the post button below it.
    let rects = vec![
        (2, 1100.0, 40.0),
        (3, 100.0, 50.0),
        (4, 900.0, 60.0),
        (1, 1100.0, 600.0),
    ];
    let ranked = rank_menu_candidates(&elements, &[], &rects, Some(200.0));
    // Strip pass: (b) account-worded avatar, (c) blank, (d) named;
    // content pass: (a) landmarked post button. The account-worded link
    // is never a candidate.
    assert_eq!(ids(&ranked), vec![2, 3, 4, 1]);
}

#[test]
fn already_clicked_excluded_in_every_tier() {
    let elements = vec![
        el(1, "button", "Open user actions", None),
        el(2, "button", "Sections", Some("navigation")),
        el(3, "button", "", None),
        el(4, "button", "Chat", None),
    ];
    let rects = vec![(3, 100.0, 50.0), (4, 900.0, 60.0)];
    // Clicked landmarked + blank: both vanish, the rest keep their order —
    // header strip first, then content.
    let clicked = vec![
        ClickedControl::of(&elements[1]),
        ClickedControl::of(&elements[2]),
    ];
    let ranked = rank_menu_candidates(&elements, &clicked, &rects, Some(200.0));
    assert_eq!(ids(&ranked), vec![4, 1]);
    // Everything clicked: nothing left to try.
    let clicked_all: Vec<ClickedControl> = elements.iter().map(ClickedControl::of).collect();
    let ranked = rank_menu_candidates(&elements, &clicked_all, &rects, Some(200.0));
    assert!(ranked.is_empty());
}

#[test]
fn below_strip_excluded_from_geometry_tiers() {
    let elements = vec![
        el(1, "button", "Open user actions", None),
        el(3, "button", "", None),
        el(4, "button", "Chat", None),
    ];
    // Blank button below the strip: not tier (c), and not tier (d) either.
    // The in-strip named button outranks the below-strip account-worded
    // one — header chrome first.
    let rects = vec![(3, 100.0, 500.0), (4, 900.0, 60.0)];
    let ranked = rank_menu_candidates(&elements, &[], &rects, Some(200.0));
    assert_eq!(ids(&ranked), vec![4, 1]);
}

#[test]
fn unreadable_viewport_disables_geometry_tiers() {
    let elements = vec![
        el(1, "button", "Open user actions", None),
        el(2, "button", "Sections", Some("navigation")),
        el(3, "button", "", None),
        el(4, "button", "Chat", None),
    ];
    let rects = vec![(3, 100.0, 50.0), (4, 900.0, 60.0)];
    // No viewport: tiers (c) and (d) stay empty, (a) and (b) still rank.
    let ranked = rank_menu_candidates(&elements, &[], &rects, None);
    assert_eq!(ids(&ranked), vec![2, 1]);
}

#[test]
fn control_appears_once_at_highest_tier() {
    let elements = vec![
        el(1, "button", "User menu", Some("banner")),
        el(2, "button", "Open user actions", None),
    ];
    let ranked = rank_menu_candidates(&elements, &[], &[], Some(200.0));
    assert_eq!(ids(&ranked), vec![1, 2]);
}

#[test]
fn non_buttons_never_ranked() {
    let elements = vec![el(1, "link", "User menu", None), el(2, "link", "", None)];
    let rects = vec![(1, 100.0, 50.0), (2, 200.0, 50.0)];
    assert!(rank_menu_candidates(&elements, &[], &rects, Some(200.0)).is_empty());
}

#[test]
fn non_header_landmark_button_needs_geometry() {
    // A footer-landmark button is not tier (a): without an in-strip rect
    // it ranks nowhere. With one it is a tier-(d) candidate — the
    // geometry fallback is landmark-agnostic, like select_rightmost_button.
    let elements = vec![el(1, "button", "Back to top", Some("contentinfo"))];
    assert!(rank_menu_candidates(&elements, &[], &[], Some(200.0)).is_empty());
    let ranked = rank_menu_candidates(&elements, &[], &[(1, 100.0, 50.0)], Some(200.0));
    assert_eq!(ids(&ranked), vec![1]);
}

#[test]
fn blank_tier_prefers_rightmost() {
    let elements = vec![el(1, "button", "", None), el(2, "button", "", None)];
    let rects = vec![(1, 100.0, 50.0), (2, 800.0, 50.0)];
    let ranked = rank_menu_candidates(&elements, &[], &rects, Some(200.0));
    assert_eq!(ids(&ranked), vec![2, 1]);
}

#[test]
fn whitespace_only_name_counts_as_blank() {
    let elements = vec![el(1, "button", "   ", None), el(2, "button", "Chat", None)];
    let rects = vec![(1, 100.0, 50.0), (2, 900.0, 50.0)];
    // Whitespace-named button is tier (c): ahead of the named tier-(d) button.
    let ranked = rank_menu_candidates(&elements, &[], &rects, Some(200.0));
    assert_eq!(ids(&ranked), vec![1, 2]);
}

#[test]
fn empty_snapshot_ranks_nothing() {
    let ranked = rank_menu_candidates(&[], &[], &[(1, 100.0, 50.0)], Some(200.0));
    assert!(ranked.is_empty());
}

// ---- scripted MenuBrowser fake ----

/// Hermetic stand-in for a live page behind [`open_identity_menu`]:
/// serves one steady snapshot tree (a poll that outlasts the script
/// sees a steady tree instead of hanging), records clicks, and switches
/// to a scripted menu tree once a chosen control is clicked.
/// `aria-expanded` answers come from a map. Every snapshot served is
/// timestamped for early-return assertions.
struct FakeMenuBrowser {
    tree: Vec<AxElement>,
    raw_nodes: usize,
    clicks: Mutex<Vec<i64>>,
    expanded: HashMap<i64, bool>,
    opens_menu_on_click: Option<i64>,
    menu_tree: Vec<AxElement>,
    viewport: Option<(f64, f64)>,
    rects: HashMap<i64, (f64, f64)>,
    snapshot_at: Mutex<Vec<Instant>>,
}

impl FakeMenuBrowser {
    fn new(tree: Vec<AxElement>) -> Self {
        Self {
            tree,
            raw_nodes: 10,
            clicks: Mutex::new(Vec::new()),
            expanded: HashMap::new(),
            opens_menu_on_click: None,
            menu_tree: Vec::new(),
            viewport: Some((1200.0, 800.0)),
            rects: HashMap::new(),
            snapshot_at: Mutex::new(Vec::new()),
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

    fn with_expanded(mut self, id: i64, expanded: bool) -> Self {
        self.expanded.insert(id, expanded);
        self
    }

    async fn clicks(&self) -> Vec<i64> {
        self.clicks.lock().await.clone()
    }

    async fn snapshot_count(&self) -> usize {
        self.snapshot_at.lock().await.len()
    }
}

impl MenuBrowser for FakeMenuBrowser {
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
        self.snapshot_at.lock().await.push(Instant::now());
        let clicks = self.clicks.lock().await;
        let tree = if self
            .opens_menu_on_click
            .is_some_and(|id| clicks.contains(&id))
        {
            self.menu_tree.clone()
        } else {
            self.tree.clone()
        };
        (tree, AxResyncCheck::new(self.raw_nodes, None), 0)
    }

    fn menu_node_expanded(
        &self,
        backend_node_id: i64,
    ) -> impl std::future::Future<Output = Option<bool>> + Send {
        std::future::ready(self.expanded.get(&backend_node_id).copied())
    }

    async fn menu_click(&self, element: &AxElement) -> Result<(), IntentError> {
        self.clicks.lock().await.push(element.backend_node_id);
        Ok(())
    }

    async fn menu_dismiss(&self) {}

    async fn menu_screenshot(&self) -> Option<String> {
        None
    }
}

fn baseline_check() -> AxResyncCheck {
    AxResyncCheck::new(10, None)
}

// ---- open_identity_menu loop ----

#[tokio::test]
async fn menu_opens_on_first_click_with_new_actionable_controls() {
    // Baseline header: account-worded button, blank avatar button, link.
    let baseline = vec![
        el(1, "button", "Open user actions", None),
        el(2, "button", "", None),
        el(3, "link", "Home", None),
    ];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Profile", None));
    menu_tree.push(el(11, "menuitem", "Settings", None));
    let fake = FakeMenuBrowser::new(menu_tree).with_rect(2, 950.0, 50.0);
    let check = baseline_check();

    let mut clicked = Vec::new();
    let Ok(outcome) = open_identity_menu(
        &fake,
        &origin(),
        MenuOpenBaseline {
            elements: &baseline,
            check: &check,
        },
        &mut clicked,
        3,
        None,
    )
    .await
    else {
        panic!("fake browser never fails")
    };

    match outcome {
        OpenMenuOutcome::Opened { control, tried } => {
            assert_eq!(control, ClickedControl::of(&baseline[0]));
            assert!(
                tried.contains("menuitem 'Profile'"),
                "tried line names the revealed control, got: {tried}"
            );
        }
        OpenMenuOutcome::Miss { tried } => panic!("expected Opened, got Miss: {tried:?}"),
    }
    assert_eq!(fake.clicks().await, vec![1]);
    assert_eq!(clicked, vec![ClickedControl::of(&baseline[0])]);
    // Evidence was on the first tick: exactly one snapshot served, far
    // short of the 3s bound — no fixed sleep is burned.
    assert_eq!(fake.snapshot_count().await, 1);
}

#[tokio::test]
async fn menu_opens_on_second_click_after_first_went_stale() {
    let baseline = vec![
        el(1, "button", "Open user actions", None),
        el(2, "button", "", None),
        el(3, "link", "Home", None),
    ];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Profile", None));
    // Clicking id 1 lands but nothing opens: every poll tick sees the
    // unchanged tree, so the first attempt burns its full bound. The
    // re-rank must skip id 1 (already clicked, by stable identity) and
    // try the next candidate, id 2, whose click reveals the menu.
    let fake = FakeMenuBrowser::new(baseline.clone())
        .with_rect(1, 900.0, 50.0)
        .with_rect(2, 950.0, 50.0)
        .with_menu_on_click(2, menu_tree);
    let check = baseline_check();

    let start = Instant::now();
    let mut clicked = Vec::new();
    let Ok(outcome) = open_identity_menu(
        &fake,
        &origin(),
        MenuOpenBaseline {
            elements: &baseline,
            check: &check,
        },
        &mut clicked,
        3,
        None,
    )
    .await
    else {
        panic!("fake browser never fails")
    };
    let elapsed = start.elapsed();

    match outcome {
        OpenMenuOutcome::Opened { control, tried } => {
            assert_eq!(control, ClickedControl::of(&baseline[1]));
            assert!(
                tried.contains("menuitem 'Profile'"),
                "tried line names the revealed control, got: {tried}"
            );
        }
        OpenMenuOutcome::Miss { tried } => panic!("expected Opened, got Miss: {tried:?}"),
    }
    // Rank order, each clicked once: id 1 is never re-clicked even
    // though the re-rank snapshot still offers it.
    assert_eq!(fake.clicks().await, vec![1, 2]);
    assert_eq!(
        clicked,
        vec![
            ClickedControl::of(&baseline[0]),
            ClickedControl::of(&baseline[1]),
        ]
    );
    // The stale first attempt waited out its poll bound instead of
    // bailing instantly.
    assert!(
        elapsed >= Duration::from_secs(2),
        "first attempt should burn its bound, took {elapsed:?}"
    );
}

#[tokio::test]
async fn menu_never_opens_misses_after_the_cap() {
    // Three tier-(d) candidates: named in-strip buttons, rightmost first.
    let baseline = vec![
        el(1, "button", "Chat", None),
        el(2, "button", "Search", None),
        el(3, "button", "Mail", None),
    ];
    let fake = FakeMenuBrowser::new(baseline.clone())
        .with_rect(1, 100.0, 50.0)
        .with_rect(2, 200.0, 50.0)
        .with_rect(3, 300.0, 50.0);
    let check = baseline_check();

    let start = Instant::now();
    let mut clicked = Vec::new();
    let Ok(outcome) = open_identity_menu(
        &fake,
        &origin(),
        MenuOpenBaseline {
            elements: &baseline,
            check: &check,
        },
        &mut clicked,
        3,
        None,
    )
    .await
    else {
        panic!("fake browser never fails")
    };
    let elapsed = start.elapsed();

    match outcome {
        OpenMenuOutcome::Miss { tried } => {
            assert_eq!(tried.len(), 3, "one tried line per attempt: {tried:?}");
            assert!(
                tried.iter().all(|line| line.contains("no new controls")),
                "every attempt reports no evidence: {tried:?}"
            );
        }
        OpenMenuOutcome::Opened { tried, .. } => panic!("expected Miss, got Opened: {tried}"),
    }
    assert_eq!(fake.clicks().await, vec![3, 2, 1]);
    assert_eq!(clicked.len(), 3);
    // Bounded: three attempts times the ~3s poll bound — never hangs,
    // never bails before the bound either.
    assert!(
        elapsed >= Duration::from_secs(8),
        "each attempt waits the full bound, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "bounded, doesn't hang: {elapsed:?}"
    );
}

#[tokio::test]
async fn poll_returns_early_when_evidence_is_immediate() {
    let baseline = vec![el(1, "button", "Open user actions", None)];
    let mut menu_tree = baseline.clone();
    menu_tree.push(el(10, "menuitem", "Profile", None));
    // The menu is already open on the first poll tick.
    let fake = FakeMenuBrowser::new(menu_tree);
    let check = baseline_check();

    let start = Instant::now();
    let mut clicked = Vec::new();
    let Ok(outcome) = open_identity_menu(
        &fake,
        &origin(),
        MenuOpenBaseline {
            elements: &baseline,
            check: &check,
        },
        &mut clicked,
        3,
        None,
    )
    .await
    else {
        panic!("fake browser never fails")
    };
    let elapsed = start.elapsed();

    assert!(
        matches!(outcome, OpenMenuOutcome::Opened { .. }),
        "expected Opened, got {outcome:?}"
    );
    assert_eq!(fake.snapshot_count().await, 1);
    assert!(
        elapsed < Duration::from_secs(1),
        "returned at the first tick, took {elapsed:?}"
    );
}

#[tokio::test]
async fn aria_expanded_true_counts_as_open_evidence() {
    let baseline = vec![el(1, "button", "Open user actions", None)];
    // The snapshot never changes — no new controls, no menu roles — but
    // the clicked control reports aria-expanded=true.
    let fake = FakeMenuBrowser::new(baseline.clone()).with_expanded(1, true);
    let check = baseline_check();

    let mut clicked = Vec::new();
    let Ok(outcome) = open_identity_menu(
        &fake,
        &origin(),
        MenuOpenBaseline {
            elements: &baseline,
            check: &check,
        },
        &mut clicked,
        3,
        None,
    )
    .await
    else {
        panic!("fake browser never fails")
    };

    match outcome {
        OpenMenuOutcome::Opened { tried, .. } => {
            assert!(
                tried.contains("aria-expanded=true"),
                "tried line names the evidence, got: {tried}"
            );
        }
        OpenMenuOutcome::Miss { tried } => panic!("expected Opened, got Miss: {tried:?}"),
    }
    assert_eq!(fake.clicks().await, vec![1]);
    assert_eq!(fake.snapshot_count().await, 1);
}

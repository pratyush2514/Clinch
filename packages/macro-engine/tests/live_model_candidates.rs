//! Live-browser proof for the model phase's candidate list
//! (`model_candidates` over the real `menu_layout_boxes`).
//!
//! The page reproduces the lab miss: a fixed header whose bell and avatar
//! live in a shadow root and come AFTER 100 sidebar links in the DOM, so
//! the old document-order head (first 60) never offered them, while the
//! screenshot plainly showed them. The model turn must now see the bell
//! and click it, with the click evidence landing on the bell.
//!
//! Gated on `CLINCH_CHROMIUM_PATH` and `#[ignore]`: acceptance coverage, not
//! a unit test. Run with:
//! `CLINCH_CHROMIUM_PATH=/path/to/chromium cargo test -p macro-engine --test live_model_candidates -- --ignored`

use browser_driver::{AxElement, LaunchOptions, ManagedBrowser};
use macro_engine::{
    MAX_NAVIGATOR_ELEMENTS, MenuBrowser, NavigatorTurn, PageAction, PageGoalOutcome, PageNavigator,
    SettingsBrowser, VerbKind, VerbSpec, model_candidates, pursue_verb_goal, pursue_with_model,
};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;
use url::Url;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const BELL: &str = "Open inbox";
const AVATAR: &str = "Open user actions";

/// `avatar_navigates`: the avatar leaves the page instantly (step 5) or,
/// like a real account button, opens a small menu (step 6).
fn home_page(avatar_navigates: bool) -> String {
    let avatar_script = if avatar_navigates {
        "root.getElementById('avatar').addEventListener('click', () => { location.href = '/user'; });"
    } else {
        "root.getElementById('avatar').addEventListener('click', () => { root.getElementById('menu').hidden = !root.getElementById('menu').hidden; });"
    };
    let mut sidebar = String::new();
    for i in 0..100 {
        let _ = write!(sidebar, "<a href=\"/c/{i}\">Community {i}</a><br>");
    }
    let mut feed = String::new();
    for i in 0..20 {
        let _ = write!(
            feed,
            "<article><a href=\"/p/{i}\">Post {i}</a> <button>Share {i}</button></article>"
        );
    }
    format!(
        r#"<!doctype html><html><head><title>Portal</title></head><body style="margin:0">
<nav aria-label="sidebar" style="margin-top:64px;width:220px;float:left">{sidebar}</nav>
<main style="margin-left:240px;margin-top:64px">{feed}</main>
<site-header></site-header>
<script>
customElements.define('site-header', class extends HTMLElement {{
  constructor() {{
    super();
    const root = this.attachShadow({{ mode: 'open' }});
    root.innerHTML = '<div style="position:fixed;top:0;left:0;right:0;height:56px;display:flex;justify-content:flex-end;gap:12px;background:#fff">' +
      '<a href="/">Logo</a>' +
      '<a id="bell" href="/notifications" aria-label="{BELL}" style="display:inline-block;width:32px;height:32px"><svg width="32" height="32" viewBox="0 0 32 32"><path d="M16 4a8 8 0 0 0-8 8v6l-3 4h22l-3-4v-6a8 8 0 0 0-8-8z"/></svg></a>' +
      '<button id="avatar" aria-label="{AVATAR}">A</button>' +
      '<div id="menu" hidden style="position:fixed;top:56px;right:0;background:#fff"><a href="/user">Profile</a><a href="/settings">Settings</a></div></div>';
    // The bell is a link with an SVG icon (the hit lands on the icon) that
    // routes after a beat, like an SPA route change.
    root.getElementById('bell').addEventListener('click', (e) => {{ e.preventDefault(); setTimeout(() => {{ location.href = '/notifications'; }}, 300); }});
    {avatar_script}
  }}
}});
</script></body></html>"#
    )
}

fn page_for(path: &str) -> String {
    match path {
        "/" => home_page(true),
        "/muse" => home_page(false),
        "/notifications" => {
            "<!doctype html><title>Notifications</title><h1>Notifications</h1>".to_owned()
        }
        "/user" => "<!doctype html><title>Profile</title><h1>Profile</h1>".to_owned(),
        _ => String::new(),
    }
}

/// Minimal single-threaded HTTP server on 127.0.0.1, OS-assigned port.
fn spawn_server() -> TestResult<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let (ready, started) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = ready.send(());
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 8192];
            let Ok(n) = stream.read(&mut buf) else {
                continue;
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            let body = page_for(path);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    started.recv_timeout(Duration::from_secs(10))?;
    Ok(port)
}

fn names(elements: &[AxElement]) -> Vec<String> {
    elements
        .iter()
        .map(|element| element.name.clone())
        .collect()
}

fn find(elements: &[AxElement], name: &str) -> TestResult<AxElement> {
    elements
        .iter()
        .find(|element| element.name == name)
        .cloned()
        .ok_or_else(|| format!("no {name:?} in the snapshot").into())
}

/// Clicks the bell when (and only when) the harness offered it; records
/// what each turn was offered.
struct BellNavigator {
    offered: Mutex<Vec<Vec<String>>>,
}

impl PageNavigator for BellNavigator {
    fn next_action(&self, _goal: &str, _elements: &[AxElement]) -> Option<PageAction> {
        None
    }

    fn next_turn(&self, turn: &NavigatorTurn<'_>) -> Option<Vec<PageAction>> {
        self.offered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(names(turn.elements));
        let action = turn
            .elements
            .iter()
            .find(|element| element.name == BELL)
            .map_or(
                PageAction::GiveUp {
                    reason: "no bell in the offered list".to_owned(),
                },
                |bell| PageAction::Click {
                    target: bell.backend_node_id,
                },
            );
        Some(vec![action])
    }
}

/// Step 1 — the lab miss, reproduced: the full snapshot contains the bell,
/// but past the first 60 in document order.
fn assert_old_head_misses_the_bell(elements: &[AxElement]) -> TestResult {
    let bell_index = elements
        .iter()
        .position(|element| element.name == BELL)
        .ok_or("the full AX snapshot must contain the bell")?;
    assert!(
        bell_index >= MAX_NAVIGATOR_ELEMENTS,
        "fixture must put the bell past the document-order head (index {bell_index})"
    );
    let old_head = names(&elements[..MAX_NAVIGATOR_ELEMENTS]);
    assert!(!old_head.iter().any(|name| name == BELL), "the old miss");
    Ok(())
}

/// Step 2 — layout boxes are viewport-relative CSS pixels: every listed
/// node's box matches `DOM.getBoxModel` (device-pixel scale and scroll
/// offset both normalized).
async fn assert_boxes_match_box_model(browser: &ManagedBrowser, ids: &[i64]) -> TestResult {
    let boxes = browser
        .menu_layout_boxes()
        .await
        .ok_or("layout snapshot failed")?;
    for id in ids {
        let (x, y, width, height) = *boxes.get(id).ok_or("node has no layout box")?;
        let rect = browser.menu_node_rect(*id).await?;
        assert!(
            (x - rect.x).abs() < 1.0
                && (y - rect.y).abs() < 1.0
                && (width - rect.width).abs() < 1.0
                && (height - rect.height).abs() < 1.0,
            "snapshot ({x}, {y}, {width}, {height}) vs box model ({}, {}, {}, {})",
            rect.x,
            rect.y,
            rect.width,
            rect.height
        );
    }
    Ok(())
}

/// Step 3 — the new selection leads with the header.
async fn assert_candidates_lead_with_the_header(
    browser: &ManagedBrowser,
    elements: &[AxElement],
) -> TestResult {
    let boxes = browser
        .menu_layout_boxes()
        .await
        .ok_or("layout snapshot failed")?;
    let viewport = browser.menu_viewport_size().await;
    let candidates = model_candidates(elements, Some(&boxes), viewport, MAX_NAVIGATOR_ELEMENTS);
    let offered = names(&candidates.head);
    assert!(offered.iter().any(|name| name == AVATAR), "{offered:?}");
    assert!(
        offered.iter().take(3).any(|name| name == BELL),
        "the header strip leads the list: {offered:?}"
    );
    Ok(())
}

/// Step 4 — a real model turn sees the bell, clicks it, and the click
/// evidence lands on the bell.
async fn assert_model_turn_clicks_the_bell(browser: &ManagedBrowser, origin: &Url) -> TestResult {
    let navigator = Arc::new(BellNavigator {
        offered: Mutex::new(Vec::new()),
    });
    let outcome = pursue_with_model(
        browser,
        origin,
        "open notifications",
        navigator.clone(),
        None,
        "deterministic: nothing found".to_owned(),
        None,
    )
    .await;
    let Ok(PageGoalOutcome::Navigated {
        landed, hit_lines, ..
    }) = outcome
    else {
        let offered = navigator
            .offered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        return Err(format!(
            "expected the bell click to navigate, got {outcome:?}; offered {offered:?}"
        )
        .into());
    };
    assert_eq!(landed.path(), "/notifications");
    assert_eq!(hit_lines.len(), 1, "{hit_lines:?}");
    assert!(
        hit_lines[0].contains(&format!("name=\"{BELL}\"")) && !hit_lines[0].contains("MISMATCH"),
        "the click landed on the bell: {hit_lines:?}"
    );
    Ok(())
}

/// Step 5 — an instantly navigating click: the post-click probe would read
/// the NEXT page's heading; the hit-test line must describe the avatar the
/// press landed on, flagged as the pre-click probe.
async fn assert_navigating_click_reports_its_target(
    browser: &ManagedBrowser,
    origin: &Url,
) -> TestResult {
    browser.navigate(origin).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let (elements, _, _) = browser.menu_snapshot(origin).await;
    let avatar = find(&elements, AVATAR)?;
    let mut journal = Vec::new();
    browser.menu_click_reported(&avatar, &mut journal).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let landed = browser
        .settings_current_url()
        .await
        .ok_or("landed url unreadable")?;
    assert_eq!(landed.path(), "/user", "the avatar click navigated");
    assert_eq!(journal.len(), 1, "{journal:?}");
    assert!(
        journal[0].contains(&format!("name=\"{AVATAR}\""))
            && journal[0].ends_with("[pre-click probe; the click navigated]")
            && !journal[0].contains("MISMATCH"),
        "{journal:?}"
    );
    Ok(())
}

/// Step 6 — the Muse flow through the real verb path: "open notifications"
/// on a page whose bell is an icon link named "Open inbox" (no
/// notifications word, so the deterministic gear misses) and whose avatar
/// opens a menu. The model clicks the bell, the verifier confirms the
/// notifications page, and the click evidence is a match on the link the
/// icon sits in.
async fn assert_open_notifications_muse_flow(browser: &ManagedBrowser, origin: &Url) -> TestResult {
    let muse = origin.join("/muse")?;
    browser.navigate(&muse).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let navigator = Arc::new(BellNavigator {
        offered: Mutex::new(Vec::new()),
    });
    let started = std::time::Instant::now();
    let outcome = pursue_verb_goal(
        browser,
        &muse,
        VerbSpec::for_kind(VerbKind::Notifications),
        Some(navigator.clone()),
        None,
    )
    .await;
    let elapsed = started.elapsed();
    let Ok(PageGoalOutcome::Verified {
        landed,
        hit_lines,
        tried_lines,
        ..
    }) = outcome
    else {
        return Err(format!("expected a verified notifications landing, got {outcome:?}").into());
    };
    println!(
        "open notifications: verified in {elapsed:?}; tried {tried_lines:?}; hits {hit_lines:?}"
    );
    assert_eq!(landed.path(), "/notifications");
    let bell_hit = hit_lines
        .iter()
        .find(|line| line.contains(&format!("name=\"{BELL}\"")))
        .ok_or_else(|| format!("no hit-test line on the bell: {hit_lines:?}"))?;
    assert!(
        bell_hit.contains(&format!("[inside A role=link name=\"{BELL}\"]"))
            && !bell_hit.contains("MISMATCH"),
        "{bell_hit}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the flow must not stall: {elapsed:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs CLINCH_CHROMIUM_PATH and launches a real off-screen browser"]
async fn live_model_candidates_offer_the_header_past_the_document_order_head() -> TestResult {
    let chromium = std::env::var("CLINCH_CHROMIUM_PATH")
        .map_err(|_| "CLINCH_CHROMIUM_PATH must point at a Chromium executable")?;
    let port = spawn_server()?;
    let origin: Url = format!("http://127.0.0.1:{port}/").parse()?;
    let profile = tempfile::tempdir()?;
    let browser = ManagedBrowser::launch_with_options(
        std::path::Path::new(&chromium),
        profile.path(),
        LaunchOptions::offscreen_headed(),
    )
    .await?;
    browser.navigate(&origin).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;

    let (elements, _, _) = browser.menu_snapshot(&origin).await;
    assert_old_head_misses_the_bell(&elements)?;
    let bell = find(&elements, BELL)?;
    assert_boxes_match_box_model(&browser, &[bell.backend_node_id]).await?;
    // Scrolled: a far feed button and the fixed bell still match.
    let last_share = find(&elements, "Share 19")?;
    browser
        .scroll_node_into_view(last_share.backend_node_id)
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_boxes_match_box_model(
        &browser,
        &[last_share.backend_node_id, bell.backend_node_id],
    )
    .await?;
    assert_candidates_lead_with_the_header(&browser, &elements).await?;
    assert_model_turn_clicks_the_bell(&browser, &origin).await?;
    assert_navigating_click_reports_its_target(&browser, &origin).await?;
    assert_open_notifications_muse_flow(&browser, &origin).await
}

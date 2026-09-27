//! Integration tests for `browser_driver`.
//!
//! Moved out of `src/` so the main source stays test-free.

use browser_driver::*;
use session_sync::{Cookie, CookieSameSite};
use std::path::Path;
use url::Url;
fn cookie(domain: &str) -> Cookie {
    Cookie {
        name: "test".into(),
        value: zeroize::Zeroizing::new("fixture".into()),
        domain: domain.into(),
        path: "/".into(),
        secure: true,
        http_only: true,
        same_site: CookieSameSite::Lax,
        expires: None,
    }
}
#[test]
fn preserves_host_only_and_session_cookie_semantics() -> Result<(), BrowserError> {
    let host = cookie_params(&cookie("example.com"))?;
    assert!(host.domain.is_none());
    assert_eq!(host.url.as_deref(), Some("https://example.com/"));
    assert!(host.expires.is_none());
    let domain = cookie_params(&cookie(".example.com"))?;
    assert_eq!(domain.domain.as_deref(), Some(".example.com"));
    assert!(domain.url.is_none());
    Ok(())
}

#[test]
fn injection_preserves_server_expiry_exactly() -> Result<(), BrowserError> {
    // Contract: a lent cookie's `expires` reaches CDP untouched, so
    // Chromium persists it per the server's own lifetime — "sync once,
    // stay logged in". Lifetimes are never invented: a cookie without
    // `expires` stays memory-only.
    let mut lent = cookie("example.com");
    lent.expires = Some(1_893_456_000); // 2030-01-01
    let params = cookie_params(&lent)?;
    assert!(params.expires.is_some());
    let ephemeral = cookie_params(&cookie("example.com"))?;
    assert!(ephemeral.expires.is_none());
    Ok(())
}

#[test]
fn launch_options_separate_background_from_interactive() {
    // Background runs never show a window: off-screen headed, never
    // visible. Interactive is the only visible mode.
    assert_eq!(
        LaunchOptions::offscreen_headed().mode,
        WindowMode::Offscreen
    );
    assert_eq!(LaunchOptions::interactive().mode, WindowMode::Headed);
    assert_eq!(LaunchOptions::default().mode, WindowMode::Headed);
}

#[test]
fn window_mode_args_keep_offscreen_headed() {
    assert!(window_mode_args(WindowMode::Headed).is_empty());
    // Off-screen headed: real window geometry, occlusion protection,
    // and crucially no `--headless` — Cloudflare probes a headed
    // compositor here, not a headless one.
    let args = window_mode_args(WindowMode::Offscreen);
    assert!(args.contains(&"--window-position=10000,10000"));
    assert!(args.contains(&"--window-size=1920,1080"));
    assert!(args.contains(&"--disable-backgrounding-occluded-windows"));
    assert!(args.contains(&"--disable-renderer-backgrounding"));
    assert!(!args.iter().any(|arg| arg.starts_with("--headless")));
}

#[test]
fn browser_ready_paint_is_lightweight_data_page() {
    // Initial screencast paint: data scheme (never via url_policy),
    // carries the ready marker, and never touches the network.
    let Some(ready) = browser_ready_url() else {
        panic!("ready URL parses");
    };
    assert_eq!(ready.scheme(), "data");
    assert!(
        BROWSER_READY_URL_STR.contains("Browser%20Ready"),
        "ready marker, got {BROWSER_READY_URL_STR}"
    );
    assert!(BROWSER_READY_URL_STR.starts_with("data:text/html,"));
}

#[test]
fn target_reselection_switches_ids_on_origin_transitions() -> Result<(), BrowserError> {
    // Hermetic proof for post-navigation re-attachment: given one
    // `Target.getTargets` listing spanning a `google.com` →
    // `github.com` transition, selection follows the live URL from one
    // target ID to the other. Non-page and detached entries never win.
    use chromiumoxide::cdp::browser_protocol::target::{TargetId, TargetInfo};
    fn entry(id: &str, kind: &str, url: &str, attached: bool) -> TargetInfo {
        TargetInfo {
            target_id: TargetId::new(id),
            r#type: kind.into(),
            title: "fixture".into(),
            url: url.into(),
            attached,
            opener_id: None,
            can_access_opener: false,
            opener_frame_id: None,
            parent_frame_id: None,
            browser_context_id: None,
            subtype: None,
        }
    }
    let listing = vec![
        entry("google-target", "page", "https://google.com/", true),
        entry(
            "github-target",
            "page",
            "https://github.com/account/billing/history",
            true,
        ),
        entry(
            "worker",
            "service_worker",
            "https://github.com/account/billing/history",
            true,
        ),
        entry(
            "detached",
            "page",
            "https://github.com/account/billing/history",
            false,
        ),
    ];
    let parse = |url: &str| Url::parse(url).map_err(|_| BrowserError::InvalidAction);
    // Pre-navigation: the google tab is active.
    assert_eq!(
        select_active_page_target(&listing, &parse("https://google.com/")?),
        Some(TargetId::new("google-target"))
    );
    // Post-navigation: selection switches to the github tab.
    let switched = select_active_page_target(
        &listing,
        &parse("https://github.com/account/billing/history")?,
    );
    assert_eq!(switched, Some(TargetId::new("github-target")));
    assert_ne!(
        switched,
        Some(TargetId::new("google-target")),
        "origin transition switches target IDs"
    );
    // Unknown URLs keep the current handle (fail-open): no match.
    assert_eq!(
        select_active_page_target(&listing, &parse("https://other.example/")?),
        None
    );
    Ok(())
}

#[test]
fn intentional_navigation_reanchors_portal_confinement() -> Result<(), BrowserError> {
    // Hermetic proof for the google.com → github billing run: anchoring
    // the intentionally navigated entry normalizes it to its origin,
    // the journal line names the transition, and confinement then
    // accepts the github page under the google request without drift.
    // (Driver state transitions ride on these pure pieces:
    // `reanchor_portal` stores `anchor_origin`, and `ax_snapshot`
    // gates on `is_anchored_drift` — neither needs CDP to prove.)
    let google = Url::parse("https://google.com/").map_err(|_| BrowserError::InvalidAction)?;
    let entry = Url::parse("https://github.com/account/billing/history")
        .map_err(|_| BrowserError::InvalidAction)?;
    let anchor = anchor_origin(&entry);
    assert_eq!(anchor.as_str(), "https://github.com/");
    assert_eq!(
        portal_reanchored_line(None, &anchor),
        "portal_reanchored: none → https://github.com/"
    );
    assert_eq!(
        portal_reanchored_line(Some(&anchor), &anchor),
        "portal_reanchored: https://github.com/ → https://github.com/"
    );
    // Live github page, requested google portal, github anchored:
    // intentional destination, no drift.
    assert!(!is_anchored_drift(&entry, &google, Some(&anchor)));
    // Same request with no anchor still drifts (legacy strict check).
    assert!(is_anchored_drift(&entry, &google, None));
    Ok(())
}

#[test]
fn unsolicited_origin_change_fails_closed_with_anchor_error() -> Result<(), BrowserError> {
    // A page nobody navigated to matches neither the request nor the
    // anchor: drift holds, retries are skipped, and the error names the
    // active anchor plus the live page.
    let google = Url::parse("https://google.com/").map_err(|_| BrowserError::InvalidAction)?;
    let anchor = Url::parse("https://github.com/").map_err(|_| BrowserError::InvalidAction)?;
    let evil = Url::parse("https://evil.example/").map_err(|_| BrowserError::InvalidAction)?;
    assert!(is_anchored_drift(&evil, &google, Some(&anchor)));
    assert_eq!(
        drift_error_line(&anchor, evil.as_str()),
        "The page left the configured portal (anchor=https://github.com/, live=https://evil.example/)"
    );
    // ...while the requested origin itself still passes beside an anchor.
    assert!(!is_anchored_drift(&google, &google, Some(&anchor)));
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH and launches a real browser using a temporary profile"]
async fn real_cdp_cookie_injection() -> Result<(), Box<dyn std::error::Error>> {
    use chromiumoxide::cdp::browser_protocol::network::GetCookiesParams;
    let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
    let profile = tempfile::tempdir()?;
    let browser = ManagedBrowser::launch(Path::new(&executable), profile.path()).await?;
    // A document's own first script must see the override, including after
    // subsequent navigations (not just an evaluation in the initial blank tab).
    for _ in 0..2 {
        browser.navigate(&Url::parse(
            "data:text/html,<script>window.initialWebdriver = typeof navigator.webdriver</script>",
        )?).await?;
        let hidden = browser
            .page
            .evaluate(
                "window.initialWebdriver === 'undefined' && navigator.webdriver === undefined",
            )
            .await?
            .into_value::<bool>()?;
        assert!(hidden);
    }
    browser
        .inject(&[cookie("example.com"), cookie(".example.org")])
        .await?;
    let result = tokio::time::timeout(
        IO_TIMEOUT,
        browser.page.execute(
            GetCookiesParams::builder()
                .urls(["https://example.com/", "https://example.org/"])
                .build(),
        ),
    )
    .await??;
    assert_eq!(result.result.cookies.len(), 2);
    assert!(
        result
            .result
            .cookies
            .iter()
            .all(|c| c.value == "fixture" && c.http_only && c.secure)
    );
    let mut invalid = cookie("example.com");
    invalid.name = "invalid;name".into();
    assert!(browser.inject(&[invalid]).await.is_err());
    browser.shutdown().await?;
    profile.close()?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH and launches a real off-screen browser using a temporary profile"]
async fn offscreen_launch_confirms_no_window() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
    let profile = tempfile::tempdir()?;
    let options = LaunchOptions::offscreen_headed();
    assert_eq!(options.mode, WindowMode::Offscreen);
    let browser =
        ManagedBrowser::launch_with_options(Path::new(&executable), profile.path(), options)
            .await?;
    assert!(browser.is_headless());
    browser.shutdown().await?;
    profile.close()?;
    Ok(())
}

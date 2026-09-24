//! Visual test for the Muse-style in-page follow-up:
//!
//! ```text
//! open my reddit profile
//! ```
//!
//! Exercises the real pipeline against a real Chromium window:
//!
//! ```text
//! parse_grammar → detect_in_page_goal → ManagedBrowser → navigate
//! → pursue_page_goal → screenshots before/after
//! ```
//!
//! The fixture is a pair of `file:` URLs (a fake site home page linking to
//! a fake profile page). `file:` URLs need no network, so this runs
//! anywhere Chromium runs — including sandboxes where loopback HTTP is
//! blocked by Local Network Access checks.
//!
//! NOTE: `file:` URLs need same-file drift handling in the driver's
//! portal-confinement check (`is_anchored_drift`): the `url` crate mints
//! a fresh opaque origin per `Url::origin()` call on the `file` scheme,
//! so without it every snapshot reads as drifted and the pursuit sees
//! zero controls. Validated end-to-end with a temporary local-only
//! allowance; for unpatched runs this needs the file-scheme identity
//! rule as a proper fix.
//!
//! Ignored by default: needs `CLINCH_CHROMIUM_PATH` and a display
//! (use `xvfb-run` when headless). Run with:
//!
//! ```sh
//! CLINCH_CHROMIUM_PATH=/opt/meta-chromium/chrome \
//!   xvfb-run -a cargo test -p orchestration-engine \
//!   --test visual_reddit_profile -- --ignored --nocapture
//! ```
//!
//! Screenshots land in `/tmp/clinch-visual-reddit-profile/`:
//! `01-home.jpg` (before pursuit) and `02-after-pursuit.jpg`.

use browser_driver::{LaunchOptions, ManagedBrowser};
use macro_engine::pursue_page_goal;
use orchestration_engine::{detect_in_page_goal, parse_grammar};
use std::path::Path;
use std::time::Duration;
use url::Url;

/// Write the two fixture pages to disk and return their `file:` URLs.
/// `file:` URLs have stable tuple origins in the `url` crate, so the
/// driver's portal-confinement check passes; `data:` URLs get a fresh
/// opaque origin per call and always read as drifted.
fn fixture_urls(dir: &Path) -> (Url, Url) {
    let profile_html = r#"<!doctype html><html><head><title>u_someone — profile</title></head>
<body style="background:rgb(247,247,242);color:rgb(37,39,34);font-family:sans-serif">
<h1>u_someone</h1>
<p>This is the profile page. Karma: 12,345.</p>
</body></html>"#;
    let profile_path = dir.join("fixture-profile.html");
    std::fs::write(&profile_path, profile_html).unwrap();
    let profile_url = Url::from_file_path(&profile_path).expect("file url");

    let home_path = dir.join("fixture-home.html");
    let home_url = Url::from_file_path(&home_path).expect("file url");
    let home_html = format!(
        r#"<!doctype html><html><head><title>fake reddit — home</title></head>
<body style="background:rgb(247,247,242);color:rgb(37,39,34);font-family:sans-serif">
<header><nav><a href="{home_url}">Home</a></nav></header>
<div><a href="{profile_url}">View profile</a></div>
<div><a href="{home_url}">Notifications</a></div>
<main><h1>Welcome to the fake front page</h1></main>
</body></html>"#
    );
    std::fs::write(&home_path, home_html).unwrap();
    (home_url, profile_url)
}

async fn settle(browser: &ManagedBrowser, origin: &Url) {
    for _ in 0..40 {
        let (elements, _, _) = browser.ax_snapshot(origin).await;
        if !elements.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn save_shot(browser: &ManagedBrowser, path: &Path) {
        let viewport = browser.viewport().await.expect("viewport screenshot");
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &viewport.data,
        )
        .expect("base64 jpeg");
        std::fs::write(path, &bytes).unwrap();
    println!(
        "shot: {} ({} bytes, {}x{})",
        path.display(),
        bytes.len(),
        viewport.width,
        viewport.height
    );
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH and a display (xvfb-run); writes screenshots to /tmp"]
async fn visual_open_my_reddit_profile() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Grammar: the exact user prompt, with reddit as the connected origin.
    let prompt = "open my reddit profile";
    let reddit = Url::parse("https://www.reddit.com/")?;
    let grammar = parse_grammar(prompt, Some(&reddit));
    println!(
        "parse: site={:?} artifact={:?} target={:?} confidence={:?}",
        grammar.site_context, grammar.artifact_noun, grammar.target_noun, grammar.confidence
    );
    assert_eq!(grammar.site_context.as_deref(), Some("reddit"));
    assert_eq!(grammar.artifact_noun.as_deref(), Some("profile"));

    // 2. In-page goal detection against the connected portal.
    let goal = detect_in_page_goal(prompt, Some(&reddit));
    println!("detect_in_page_goal: {goal:?}");
    assert_eq!(goal.as_deref(), Some("profile"));

    // 3. Fixture pages as file: URLs (no network needed).
    let fixture_dir = Path::new("/tmp/clinch-visual-reddit-profile");
    std::fs::create_dir_all(fixture_dir).ok();
    let (home_url, profile_url) = fixture_urls(fixture_dir);

    // 4. Real browser, real navigation.
    let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
    let dir = tempfile::tempdir()?;
    let browser = ManagedBrowser::launch_with_options(
        Path::new(&executable),
        &dir.path().join("profile"),
        LaunchOptions::interactive(),
    )
    .await?;
    println!("launched chromium");

    browser.navigate(&home_url).await?;
    settle(&browser, &home_url).await;
    println!(
        "landed url scheme: {:?}",
        browser.current_url().await?.as_ref().map(|u| u.scheme())
    );

    let shot_dir = Path::new("/tmp/clinch-visual-reddit-profile");
    save_shot(&browser, &shot_dir.join("01-home.jpg")).await;

    // 5. The real pursuit loop: snapshot → find the "profile" control →
    //    click → observe the navigation. Deterministic phase only.
    let outcome = pursue_page_goal(&browser, &home_url, "profile", None).await;
    println!("outcome: {outcome:?}");

    tokio::time::sleep(Duration::from_secs(1)).await;
    save_shot(&browser, &shot_dir.join("02-after-pursuit.jpg")).await;

    // 6. The pursuit must have clicked through to the profile page.
    let final_url = browser.current_url().await?.expect("final url");
    println!("final url == profile page: {}", final_url == profile_url);
    assert_eq!(
        final_url, profile_url,
        "expected to land on the profile data: URL (outcome: {outcome:?})"
    );
    println!("done — inspect the shots above");
    Ok(())
}

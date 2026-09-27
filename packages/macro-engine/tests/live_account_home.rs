//! Live-browser proof for the account-home worker (`pursue_account_home`).
//!
//! A real off-screen Chromium against real DOM: no mocks, no fixtures — the
//! worker clicks a blind avatar, a menu opens, a profile link is clicked, the
//! URL changes, and the verifier decides. A tiny `std`-only HTTP server plays
//! the portal.
//!
//! Gated on `CLINCH_CHROMIUM_PATH` and `#[ignore]`: acceptance coverage, not
//! a unit test. Run with:
//! `CLINCH_CHROMIUM_PATH=/path/to/chromium cargo test -p macro-engine --test live_account_home -- --ignored`

use browser_driver::{LaunchOptions, ManagedBrowser};
use macro_engine::{IntentError, PageGoalOutcome, pursue_account_home, validate_revealed_href};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use url::Url;

/// Portal home page variant: signed-in ("Log out" marker), a header with a
/// *blind* avatar button (aria-label `kx7` — no account words, so the worker
/// must use the geometry fallback, like Reddit's avatar), and a hidden menu
/// whose profile link reveals the given label.
fn home_page(menu_label: &str) -> String {
    format!(
        r#"<!doctype html><html><head><title>Test Portal</title></head><body>
<header style="position:fixed;top:0;left:0;right:0;height:64px;background:#ddd">
<button>Search</button>
<button>Chat</button>
<button id="avatar" aria-label="kx7" style="float:right">KX</button>
</header>
<div id="menu" hidden="true" style="position:fixed;top:64px;right:0;background:#fff;border:1px solid #999">
<a href="/user/kx7">{menu_label}</a>
</div>
<script>
document.getElementById('avatar').addEventListener('click', function(){{
  var m = document.getElementById('menu'); m.hidden = !m.hidden;
}});
</script>
<main style="margin-top:200px">
<p>Welcome back to the portal.</p>
<p><button>Log out</button></p>
</main>
</body></html>"#
    )
}

fn guest_page() -> String {
    r#"<!doctype html><html><head><title>Test Portal</title></head><body>
<header style="position:fixed;top:0;left:0;right:0;height:64px;background:#ddd">
<button id="avatar" aria-label="kx7" style="float:right">KX</button>
</header>
<main style="margin-top:200px">
<p>Please <button>Log in</button> or <button>Sign up</button> to continue.</p>
</main>
</body></html>"#
        .to_owned()
}

/// Authenticated, but no identity chrome at all — only a decoy header button.
fn nochrome_page() -> String {
    r#"<!doctype html><html><head><title>Test Portal</title></head><body>
<header style="position:fixed;top:0;left:0;right:0;height:64px;background:#ddd">
<button>Search</button>
</header>
<main style="margin-top:200px">
<p>Welcome back to the portal.</p>
<p><button>Log out</button></p>
</main>
</body></html>"#
        .to_owned()
}

fn profile_page() -> String {
    r#"<!doctype html><html><head><title>kx7 — Test Portal</title></head><body>
<main style="margin-top:200px">
<h1>u/kx7</h1>
<p>This is kx7's own page.</p>
<p><button>Log out</button></p>
</main>
</body></html>"#
        .to_owned()
}

/// Minimal single-threaded HTTP server on 127.0.0.1, OS-assigned port.
/// Returns the port; the serving thread runs until the process exits.
fn spawn_server() -> u16 {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let port = listener.local_addr().expect("local addr").port();
        tx.send(port).expect("send port");
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
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
            let body = match path {
                "/" => home_page("Profile"),
                "/b" => home_page("u/kx7"),
                "/c" => home_page("u/other"),
                "/guest" => guest_page(),
                "/nochrome" => nochrome_page(),
                "/user/kx7" => profile_page(),
                _ => String::new(),
            };
            let status = if body.is_empty() {
                "404 Not Found"
            } else {
                "200 OK"
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("test server port")
}

fn chromium_path() -> PathBuf {
    std::env::var("CLINCH_CHROMIUM_PATH")
        .expect("CLINCH_CHROMIUM_PATH must point at a Chromium executable")
        .into()
}

async fn launch(profile_dir: &std::path::Path) -> ManagedBrowser {
    ManagedBrowser::launch_with_options(&chromium_path(), profile_dir, LaunchOptions::offscreen_headed())
        .await
        .expect("launch off-screen Chromium")
}

async fn goto(browser: &ManagedBrowser, origin: &Url, path: &str) -> Url {
    let url: Url = origin.join(path).expect("scenario url");
    browser.navigate(&url).await.expect("navigate");
    url
}

/// Pure gate: what the memory fast path and the worker's revealed-href path
/// accept or reject before any navigation happens.
fn href_gate_assertions() {
    let origin: Url = "https://www.example.com/".parse().unwrap();
    // Same-site https with a non-root path: accepted.
    assert!(validate_revealed_href("https://www.example.com/user/kx7", &origin).is_some());
    // Case/leading-www normalization: example.com is the same site as www.
    assert!(validate_revealed_href("https://example.com/user/kx7", &origin).is_some());
    // http is rejected outright.
    assert!(validate_revealed_href("http://www.example.com/user/kx7", &origin).is_none());
    // Embedded credentials are rejected, never navigated.
    assert!(
        validate_revealed_href("https://kx7:s3cret@www.example.com/user/kx7", &origin).is_none()
    );
    // Cross-site hrefs are rejected.
    assert!(validate_revealed_href("https://evil.example.net/user/kx7", &origin).is_none());
    // javascript: never becomes a destination.
    assert!(validate_revealed_href("javascript:alert(1)", &origin).is_none());
    // Root path navigates nowhere useful.
    assert!(validate_revealed_href("https://www.example.com/", &origin).is_none());
}

#[tokio::test]
#[ignore = "needs CLINCH_CHROMIUM_PATH and launches a real off-screen browser"]
async fn live_account_home_proof() {
    href_gate_assertions();

    let port = spawn_server();
    let origin: Url = format!("http://127.0.0.1:{port}/").parse().unwrap();
    let profile = tempfile::tempdir().expect("temp profile dir");
    let browser = launch(profile.path()).await;

    // 1. Happy path, label evidence: blind avatar -> geometry fallback ->
    //    menu opens -> "Profile" link clicked -> /user/kx7 verified via the
    //    account-word path segment and the profile-worded trigger label.
    goto(&browser, &origin, "/").await;
    match pursue_account_home(&browser, &origin, None, None).await {
        Ok(PageGoalOutcome::Verified {
            label,
            landed,
            username,
        }) => {
            assert_eq!(label, "Profile");
            assert_eq!(landed.path(), "/user/kx7");
            assert_eq!(username, None);
        }
        other => panic!("happy path: expected Verified, got {other:?}"),
    }

    // 2. Strongest evidence: the menu reveals a u/ handle and the landing
    //    path carries it.
    goto(&browser, &origin, "/b").await;
    match pursue_account_home(&browser, &origin, None, None).await {
        Ok(PageGoalOutcome::Verified {
            landed, username, ..
        }) => {
            assert_eq!(landed.path(), "/user/kx7");
            assert_eq!(username.as_deref(), Some("kx7"));
        }
        other => panic!("username path: expected Verified, got {other:?}"),
    }

    // 3. The verifier has teeth: the menu claims u/other but the page
    //    landed on /user/kx7 — the username must reappear in the path.
    goto(&browser, &origin, "/c").await;
    match pursue_account_home(&browser, &origin, None, None).await {
        Err(IntentError::NoMatch(diagnostic)) => {
            assert!(
                diagnostic.contains("failed verification"),
                "expected verifier rejection, got: {diagnostic}"
            );
        }
        other => panic!("wrong-user path: expected verifier rejection, got {other:?}"),
    }

    // 4. Signed-out short-circuit: identity chrome exists on the page, but
    //    the probe classifies the guest landing first — zero clicks, the
    //    URL never moves.
    let guest_url = goto(&browser, &origin, "/guest").await;
    match pursue_account_home(&browser, &origin, None, None).await {
        Ok(PageGoalOutcome::SignedOut) => {}
        other => panic!("signed-out path: expected SignedOut, got {other:?}"),
    }
    let still_there = browser
        .current_url()
        .await
        .expect("current url")
        .expect("url present");
    assert_eq!(still_there, guest_url, "signed-out run must not navigate");

    // 5. Honest miss: no identity chrome — the diagnostic names what was
    //    actually tried and what happened, never a control dump.
    goto(&browser, &origin, "/nochrome").await;
    match pursue_account_home(&browser, &origin, None, None).await {
        Err(IntentError::NoMatch(diagnostic)) => {
            assert!(
                diagnostic.contains("Tried:"),
                "miss must name attempted controls, got: {diagnostic}"
            );
            assert!(
                diagnostic.contains("Search"),
                "miss must name the decoy it tried, got: {diagnostic}"
            );
        }
        other => panic!("miss path: expected NoMatch diagnostic, got {other:?}"),
    }
}

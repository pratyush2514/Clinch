#![deny(unsafe_code)]
//! Opt-in live-Chromium proof for dynamic discovery (AX tree + Set-of-Marks).
//! Requires `CLINCH_CHROMIUM_PATH`; launches isolated headless profiles and a
//! loopback fixture page. No personal profile or real portal is touched.
use browser_driver::{LaunchOptions, ManagedBrowser, Mark};
use std::{path::Path, time::Duration};
use url::Url;

const FIXTURE_HTML: &str = "<!doctype html><html><body><button data-testid=\"pay-btn\">Pay now</button><a href=\"/docs\">Docs</a><input type=\"text\" aria-label=\"Email address\"><script>document.querySelector('button').addEventListener('click', function(){document.body.setAttribute('data-clicked','yes');});</script></body></html>";

async fn fixture_page() -> Result<(Url, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut head = [0; 4096];
                let _ = socket.read(&mut head).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{FIXTURE_HTML}",
                    FIXTURE_HTML.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    Ok((url, task))
}

async fn headless_browser(
    dir: &tempfile::TempDir,
) -> Result<ManagedBrowser, Box<dyn std::error::Error>> {
    let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
    Ok(ManagedBrowser::launch_with_options(
        Path::new(&executable),
        &dir.path().join("profile"),
        LaunchOptions::replay(),
    )
    .await?)
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn ax_snapshot_lists_interactive_controls() -> Result<(), Box<dyn std::error::Error>> {
    let (url, server) = fixture_page().await?;
    let dir = tempfile::tempdir()?;
    let browser = headless_browser(&dir).await?;
    browser.navigate(&url).await?;
    let (elements, _, _) = browser.ax_snapshot(&url).await;
    assert!(elements.iter().any(|element| element.role == "button"
        && element.name == "Pay now"
        && element.backend_node_id > 0));
    assert!(
        elements
            .iter()
            .any(|element| element.role == "link" && element.name == "Docs")
    );
    assert!(
        elements
            .iter()
            .any(|element| element.role == "textbox" && element.name == "Email address")
    );
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn picker_overlay_pick_resolves_through_binding() -> Result<(), Box<dyn std::error::Error>> {
    // Exercises the exact production path the Pick Element button drives:
    // overlay install, real dispatched input through the overlay, binding
    // event back to Rust. A timeout here reproduces the reported no-pick.
    let (url, server) = fixture_page().await?;
    let dir = tempfile::tempdir()?;
    let browser = headless_browser(&dir).await?;
    browser.navigate(&url).await?;
    browser.enable_picker().await?;
    let (elements, _, _) = browser.ax_snapshot(&url).await;
    let button = elements
        .iter()
        .find(|element| element.role == "button" && element.name == "Pay now")
        .ok_or("fixture button missing from AX snapshot")?;
    let target = browser.node_rect(button.backend_node_id).await?;
    let mark = Mark {
        index: 0,
        x: target.x,
        y: target.y,
        width: target.width,
        height: target.height,
    };
    let pick = browser.await_pick(Duration::from_secs(10));
    let click = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        browser.click_mark(&mark).await
    };
    let (picked, _) = tokio::join!(pick, click);
    let picked = picked?;
    assert_eq!(picked.primary(), Some("[data-testid=\"pay-btn\"]"));
    assert_eq!(picked.tag, "button");
    browser.disable_picker().await?;
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn som_badges_render_and_click_activates() -> Result<(), Box<dyn std::error::Error>> {
    let (url, server) = fixture_page().await?;
    let dir = tempfile::tempdir()?;
    let browser = headless_browser(&dir).await?;
    browser.navigate(&url).await?;
    let (elements, _, _) = browser.ax_snapshot(&url).await;
    let button = elements
        .iter()
        .find(|element| element.role == "button")
        .ok_or("fixture button missing")?;
    let target = browser.node_rect(button.backend_node_id).await?;
    let mark = Mark {
        index: 0,
        x: target.x,
        y: target.y,
        width: target.width,
        height: target.height,
    };
    let frame = browser.marked_viewport(&[mark]).await?;
    assert!(!frame.data.is_empty());
    // No overlay installed here, so the dispatched click reaches the page and
    // flips the fixture flag, observed through selector resolution.
    browser.clear_marks().await?;
    browser.click_mark(&mark).await?;
    browser.resolve("body[data-clicked='yes']", false).await?;
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

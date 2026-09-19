#![deny(unsafe_code)]
//! Opt-in live-Chromium proof for semantic intent execution.
//! Requires `CLINCH_CHROMIUM_PATH`; launches an isolated headless profile and
//! a loopback fixture page. No personal profile or real portal is touched.
use browser_driver::{LaunchOptions, ManagedBrowser};
use macro_engine::SemanticIntent;
use std::{path::Path, time::Duration};
use url::Url;

const FIXTURE_HTML: &str = "<!doctype html><html><body><button data-testid=\"pay-btn\">Pay now</button><a href=\"/docs\">Docs</a><input type=\"text\" aria-label=\"Email address\"><script>document.querySelector('button').addEventListener('click', function(){document.body.setAttribute('data-clicked','yes');});</script></body></html>";

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated headless fixture"]
async fn intent_executes_without_selectors() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
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
    let dir = tempfile::tempdir()?;
    let browser = ManagedBrowser::launch_with_options(
        Path::new(&std::env::var("CLINCH_CHROMIUM_PATH")?),
        &dir.path().join("profile"),
        LaunchOptions::replay(),
    )
    .await?;
    browser.navigate(&url).await?;
    tokio::time::timeout(
        Duration::from_mins(1),
        macro_engine::execute_intent(
            &browser,
            &url,
            &SemanticIntent {
                role: "button".into(),
                label_query: "Pay now".into(),
            },
        ),
    )
    .await
    .map_err(|_| "intent timed out")?
    .map_err(|error| format!("intent failed: {error}"))?;
    browser.resolve("body[data-clicked='yes']", false).await?;
    browser.clear_marks().await?;
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

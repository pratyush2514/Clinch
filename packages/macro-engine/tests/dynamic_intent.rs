#![deny(unsafe_code)]
//! Opt-in live-Chromium proof for semantic intent execution.
//! Requires `CLINCH_CHROMIUM_PATH`; launches an isolated off-screen profile and
//! a loopback fixture page. No personal profile or real portal is touched.
use browser_driver::{LaunchOptions, ManagedBrowser};
use macro_engine::SemanticIntent;
use std::{path::Path, time::Duration};
use url::Url;

const FIXTURE_HTML: &str = "<!doctype html><html><body><button data-testid=\"pay-btn\">Pay now</button><a href=\"/docs\">Docs</a><input type=\"text\" aria-label=\"Email address\"><section aria-label=\"Statements\"><h2>Statement #42</h2><button>Download</button></section><section aria-label=\"Settings\"><button>Download</button></section><script>document.querySelector('button').addEventListener('click', function(){document.body.setAttribute('data-clicked','yes');});document.querySelectorAll('section button')[0].addEventListener('click', function(){document.body.setAttribute('data-statement-clicked','yes');});document.querySelectorAll('section button')[1].addEventListener('click', function(){document.body.setAttribute('data-settings-clicked','yes');});</script></body></html>";

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated off-screen fixture"]
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
        LaunchOptions::offscreen_headed(),
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
                container_query: None,
                raw_prompt: String::new(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: None,
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

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated off-screen fixture"]
async fn container_context_disambiguates_duplicate_buttons()
-> Result<(), Box<dyn std::error::Error>> {
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
        LaunchOptions::offscreen_headed(),
    )
    .await?;
    browser.navigate(&url).await?;
    // Two identical "Download" buttons; only the Statements card matches.
    // A selector-free run must activate exactly that one.
    tokio::time::timeout(
        Duration::from_mins(1),
        macro_engine::execute_intent(
            &browser,
            &url,
            &SemanticIntent {
                role: "button".into(),
                label_query: "statement".into(),
                container_query: None,
                raw_prompt: String::new(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: None,
            },
        ),
    )
    .await
    .map_err(|_| "intent timed out")?
    .map_err(|error| format!("intent failed: {error}"))?;
    browser
        .resolve("body[data-statement-clicked='yes']", false)
        .await?;
    browser.clear_marks().await?;
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated off-screen fixture"]
async fn container_query_vetoes_out_of_scope_controls_live()
-> Result<(), Box<dyn std::error::Error>> {
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
        LaunchOptions::offscreen_headed(),
    )
    .await?;
    browser.navigate(&url).await?;
    // Both buttons are named "Download" and the Statements card sorts first:
    // without the container veto, document order would click it. The scope
    // forces the Settings card instead.
    tokio::time::timeout(
        Duration::from_mins(1),
        macro_engine::execute_intent(
            &browser,
            &url,
            &SemanticIntent {
                role: "button".into(),
                label_query: "download".into(),
                container_query: Some("settings".into()),
                raw_prompt: String::new(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: None,
            },
        ),
    )
    .await
    .map_err(|_| "intent timed out")?
    .map_err(|error| format!("intent failed: {error}"))?;
    browser
        .resolve("body[data-settings-clicked='yes']", false)
        .await?;
    assert!(
        browser
            .resolve("body[data-statement-clicked]", false)
            .await
            .is_err()
    );
    browser.clear_marks().await?;
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

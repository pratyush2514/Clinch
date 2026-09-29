#![deny(unsafe_code)]
//! Opt-in live-Chromium proof that every click path retains its
//! `click_hit_test:` journal line. Requires `CLINCH_CHROMIUM_PATH`;
//! launches an isolated off-screen profile against a loopback fixture.
use browser_driver::{LaunchOptions, ManagedBrowser};
use macro_engine::{ExecuteOutcome, IntentError, SemanticIntent};
use std::path::Path;
use url::Url;

const FIXTURE_HTML: &str = "<!doctype html><html><body><section aria-label=\"One\"><button>Alpha</button></section><section aria-label=\"Two\"><button>Item</button></section><section aria-label=\"Three\"><button>Item</button></section></body></html>";

// The URL changes 800ms after the click, so `wait_for_url_change` observes
// it move rather than racing an instant commit.
const NAV_HTML: &str = "<!doctype html><html><body><header><button id=\"go\">Docs</button></header><script>document.getElementById('go').addEventListener('click', function(){setTimeout(function(){history.pushState({}, '', '/docs');}, 800);});</script></body></html>";

const HIT_PREFIX: &str = "click_hit_test:";

fn intent(label: &str, is_plural: bool) -> SemanticIntent {
    SemanticIntent {
        role: "button".into(),
        label_query: label.into(),
        container_query: None,
        raw_prompt: String::new(),
        ordinal_index: None,
        is_last: false,
        is_plural,
        entry_url: None,
        primary_target_noun: None,
    }
}

type Launched = (ManagedBrowser, Url, tokio::task::JoinHandle<()>);

async fn launch() -> Result<Launched, Box<dyn std::error::Error>> {
    launch_page(FIXTURE_HTML).await
}

async fn launch_page(html: &'static str) -> Result<Launched, Box<dyn std::error::Error>> {
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
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                    html.len()
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
    // The profile dir must outlive the browser; leak it to the process.
    std::mem::forget(dir);
    Ok((browser, url, server))
}

fn hit_lines(text: &str) -> usize {
    text.matches(HIT_PREFIX).count()
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated off-screen fixture"]
async fn single_intent_click_journals_one_hit_line() -> Result<(), Box<dyn std::error::Error>> {
    let (browser, url, server) = launch().await?;
    let outcome = macro_engine::execute_intent(&browser, &url, &intent("Alpha", false)).await?;
    assert_eq!(hit_lines(&outcome.hit_line), 1, "{}", outcome.hit_line);
    assert!(outcome.hit_line.starts_with(HIT_PREFIX));
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated off-screen fixture"]
async fn batch_clicks_journal_one_hit_line_each() -> Result<(), Box<dyn std::error::Error>> {
    let (browser, url, server) = launch().await?;
    let ExecuteOutcome::Completed(outcomes) =
        macro_engine::execute_batch(&browser, &url, &intent("Item", true)).await?
    else {
        return Err("batch halted early".into());
    };
    assert!(outcomes.len() >= 2, "batch clicked {}", outcomes.len());
    for outcome in &outcomes {
        assert_eq!(hit_lines(&outcome.hit_line), 1, "{}", outcome.hit_line);
    }
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated off-screen fixture"]
async fn page_goal_miss_diagnostic_retains_hit_lines() -> Result<(), Box<dyn std::error::Error>> {
    let (browser, url, server) = launch().await?;
    let miss =
        macro_engine::pursue_page_goal(&browser, &url, "zzz-no-such-control", None, None).await;
    let Err(IntentError::NoMatch(diagnostic)) = miss else {
        return Err("expected an honest miss".into());
    };
    assert!(hit_lines(&diagnostic) >= 1, "{diagnostic}");
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; isolated off-screen fixture"]
async fn navigating_click_carries_its_hit_line() -> Result<(), Box<dyn std::error::Error>> {
    let (browser, url, server) = launch_page(NAV_HTML).await?;
    let outcome = macro_engine::pursue_page_goal(&browser, &url, "Docs", None, None).await?;
    let macro_engine::PageGoalOutcome::Navigated { hit_lines, .. } = outcome else {
        return Err(format!("expected Navigated, got {outcome:?}").into());
    };
    assert_eq!(hit_lines.len(), 1, "{hit_lines:?}");
    assert!(hit_lines[0].starts_with(HIT_PREFIX));
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

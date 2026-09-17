use browser_driver::{Action, ManagedBrowser, WaitCondition};
use llm_provider::{Candidate, ProviderError, RepairContext, SelectorProvider};
use macro_engine::{HealingReplay, Macro, MacroStep};
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

struct Mock {
    selector: &'static str,
    calls: AtomicUsize,
}
impl SelectorProvider for Mock {
    fn repair<'a>(
        &'a self,
        context: &'a RepairContext,
    ) -> Pin<Box<dyn Future<Output = Result<Candidate, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(context.html.contains("new-filter"));
            assert!(!context.html.contains("SECRET"));
            assert!(!context.html.contains("script"));
            Ok(Candidate {
                selector: self.selector.into(),
            })
        })
    }
}

#[tokio::test]
#[ignore = "Requires installed Chromium via CLINCH_CHROMIUM_PATH; isolated local fixture"]
async fn repairs_atomically_and_executes_once() -> Result<(), Box<dyn std::error::Error>> {
    let (origin, server) = fixture().await?;
    let dir = tempfile::tempdir()?;
    let browser = ManagedBrowser::launch(
        std::path::Path::new(&std::env::var("CLINCH_CHROMIUM_PATH")?),
        &dir.path().join("profile"),
    )
    .await?;
    browser.navigate(&origin).await?;
    let path = dir.path().join("macro.json");
    let mut recording = Macro {
        version: 1,
        last_healed_at: None,
        healing_history: Vec::new(),
        origin: origin.clone(),
        steps: vec![MacroStep {
            action: Action::Fill {
                selector: "#billing #old-filter".into(),
                value: "September".into(),
            },
            wait: None,
        }],
    };
    recording.save(&path).await?;
    let original = tokio::fs::read(&path).await?;
    let invalid = Mock {
        selector: "#missing",
        calls: AtomicUsize::new(0),
    };
    let output = dir.path().join("downloads");
    assert!(
        HealingReplay {
            browser: &browser,
            provider: &invalid,
            path: &path,
            output: &output
        }
        .step(&mut recording, 0, |_| {}, |_| async { true })
        .await
        .is_err()
    );
    assert_eq!(tokio::fs::read(&path).await?, original);
    let provider = Mock {
        selector: "#billing #new-filter",
        calls: AtomicUsize::new(0),
    };
    let approvals = AtomicUsize::new(0);
    HealingReplay {
        browser: &browser,
        provider: &provider,
        path: &path,
        output: &output,
    }
    .step(
        &mut recording,
        0,
        |_| {},
        |_| {
            approvals.fetch_add(1, Ordering::SeqCst);
            async { true }
        },
    )
    .await?;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    assert_eq!(Macro::load(&path).await?, recording);
    assert_eq!(
        recording.steps[0].action.selector(),
        Some("#billing #new-filter")
    );
    assert_eq!(
        std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .count(),
        1
    );
    browser
        .resolve("#new-filter[data-count='1']", false)
        .await?;
    assert_eq!(recording.healing_history.len(), 1);
    assert_wait_repair(&browser, &provider, &mut recording, &path, &output).await?;
    assert_submission_gate(&browser, &provider, &recording, &path, &output).await?;
    let frame = browser.viewport().await?;
    assert!(!frame.data.is_empty());
    assert!(frame.width > 0.0);
    browser.shutdown().await?;
    server.abort();
    Ok(())
}

async fn assert_wait_repair(
    browser: &ManagedBrowser,
    provider: &Mock,
    recording: &mut Macro,
    path: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    recording.steps[0].wait = Some(WaitCondition {
        selector: "#billing #old-filter".into(),
        timeout_ms: 1,
    });
    recording.save(path).await?;
    let replay = HealingReplay {
        browser,
        provider,
        path,
        output,
    };
    replay
        .step(recording, 0, |_| {}, |_| async { true })
        .await?;
    browser
        .resolve("#new-filter[data-count='2']", false)
        .await?;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(recording.healing_history.len(), 2);
    assert_eq!(Macro::load(path).await?, *recording);
    assert!(
        replay
            .step(recording, 0, |_| {}, |_| async { false })
            .await
            .is_err()
    );
    browser
        .resolve("#new-filter[data-count='2']", false)
        .await?;
    Ok(())
}

async fn fixture() -> Result<(Url, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0; 4096];
                let _ = socket.read(&mut buf).await;
                let html = "<!doctype html><section id='billing'><input id='new-filter' type='search' value='SECRET' oninput=\"this.dataset.count=String(Number(this.dataset.count||0)+1)\"><input type='password' value='SECRET'><script>/*SECRET*/</script><form id='send' onsubmit=\"event.preventDefault();this.dataset.sent='yes'\"><button>Submit fixture</button></form></section>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                    html.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    Ok((origin, server))
}

async fn assert_submission_gate(
    browser: &ManagedBrowser,
    provider: &Mock,
    recording: &Macro,
    path: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut submit = recording.clone();
    submit.steps[0].action = Action::Submit {
        selector: "#send".into(),
    };
    submit.steps[0].wait = None;
    assert!(
        browser
            .execute_action(&submit.steps[0].action, &submit.origin, output)
            .await
            .is_err()
    );
    let replay = HealingReplay {
        browser,
        provider,
        path,
        output,
    };
    assert!(
        replay
            .step(&mut submit, 0, |_| {}, |_| async { false })
            .await
            .is_err()
    );
    assert!(browser.resolve("#send[data-sent]", false).await.is_err());
    replay
        .step(&mut submit, 0, |_| {}, |_| async { true })
        .await?;
    browser.resolve("#send[data-sent='yes']", false).await?;
    Ok(())
}

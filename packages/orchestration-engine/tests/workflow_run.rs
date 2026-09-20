#![deny(unsafe_code)]
//! Opt-in real Chromium/CDP test. The server and cookie are synthetic and local-only.
use browser_driver::{Action, ManagedBrowser};
use macro_engine::{Macro, RepairStage};
use orchestration_engine::{Engine, RunMode, StepState, TaskRequest, TaskState};
use session_sync::{Cookie, CookieSameSite};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::{JoinHandle, JoinSet},
};
use url::Url;

struct Fixture {
    url: Url,
    downloads: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = Url::parse(&format!("http://{}/portal", listener.local_addr()?))?;
        let downloads = Arc::new(AtomicUsize::new(0));
        let counter = downloads.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept(), if connections.len() < 16 => {
                        let Ok((mut socket, _)) = incoming else { break; };
                        let counter = counter.clone();
                        connections.spawn(async move {
                // Chromium opens idle/preconnected sockets. They must not block active requests.
                let mut request = Vec::with_capacity(8192);
                let read = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    let mut chunk = [0; 1024];
                    while !request.ends_with(b"\r\n\r\n") {
                        let size = socket.read(&mut chunk).await?;
                        if size == 0 || request.len() + size > 8192 { return Err(std::io::Error::other("Incomplete fixture request")); }
                        request.extend_from_slice(&chunk[..size]);
                    }
                    Ok::<_, std::io::Error>(())
                }).await;
                if !matches!(read, Ok(Ok(()))) { return; }
                let request = String::from_utf8_lossy(&request);
                let authenticated = request.contains("fixture_session=local_test");
                let (status, headers, body) = if !authenticated {
                    (
                        "401 Unauthorized",
                        "Content-Type: text/html\r\n",
                        "<h1>Sign in</h1>",
                    )
                } else if request.starts_with("GET /report.pdf ") {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (
                        "200 OK",
                        "Content-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\n",
                        "%PDF-1.4\nClinch local file fixture\n%%EOF\n",
                    )
                } else if request.starts_with("GET /blobs ") {
                    (
                        "200 OK",
                        "Content-Type: text/html\r\n",
                        r"<!doctype html><title>Blob files</title>
                        <a class='blob-file' id='direct' download='report'>Direct blob</a>
                        <a class='blob-file' href='/report.pdf' id='generated'>Generate blob</a>
                        <script>
                        const pdf = new Blob(['%PDF-1.4\nClinch blob fixture\n%%EOF'], {type:'application/pdf'});
                        document.querySelector('#direct').href = URL.createObjectURL(pdf);
                        document.querySelector('#generated').onclick = event => {
                            event.preventDefault();
                            const link = document.createElement('a');
                            link.href = URL.createObjectURL(pdf); link.download = 'report';
                            document.body.appendChild(link); link.click(); link.remove();
                        };
                        </script>",
                    )
                } else if request.starts_with("GET /archive ") {
                    (
                        "200 OK",
                        "Content-Type: text/html\r\n",
                        "<!doctype html><title>Clinch files fixture</title><a class='report' href='/report.pdf'>Download report</a><input type='search' id='filter' oninput=\"this.dataset.applied = this.value === '2026-09' ? 'yes' : 'no'\"><input type='password' id='secret'><div id='hidden' hidden>Hidden</div>",
                    )
                } else {
                    (
                        "200 OK",
                        "Content-Type: text/html\r\n",
                        "<!doctype html><title>Clinch portal fixture</title><a id='archive' href='/archive'>File archive</a>",
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self {
            url,
            downloads,
            task,
        })
    }
}

#[tokio::test]
#[ignore = "Requires CLINCH_CHROMIUM_PATH; opens an isolated Chromium and local synthetic file portal"]
async fn records_replays_downloads_and_flags_only_the_broken_step()
-> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
    let fixture = Fixture::start().await?;
    let directory = tempfile::tempdir()?;
    let browser = ManagedBrowser::launch(Path::new(&executable), &directory.path().join("profile"))
        .await
        .map_err(|error| format!("Recording browser launch: {error}"))?;
    // Exercise the session-sync cookie contract and actual Network.setCookie path.
    // This is not evidence of platform Keychain decryption or real-portal acceptance.
    inject_fixture_session(&browser).await?;
    let pool = playbook_store::initialize(&directory.path().join("clinch.db")).await?;
    let engine = Engine::new(pool.clone()).await?;
    let request = TaskRequest {
        workflow: "fixture".into(),
        portal_url: fixture.url.clone(),
        link_selector: Some("#archive".into()),
        download_selector: "a.report".into(),
    };
    let mut events = Vec::new();
    let first = engine
        .run_task(&request, &browser, directory.path(), |event| {
            if let Some(gate) = &event.approval {
                assert!(engine.decide(gate.task_id, gate.step_index, true).is_ok());
            }
            events.push(event);
        })
        .await?;
    assert_eq!(first.state, TaskState::Completed, "{first:?}");
    assert_eq!(first.mode, RunMode::Record);
    verify_download(&engine, &first).await?;
    assert_eq!(fixture.downloads.load(Ordering::SeqCst), 1);
    assert!(events.iter().any(|event| {
        event
            .highlight
            .as_ref()
            .is_some_and(|target| target.selector == "a.report")
    }));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].task.revision <= pair[1].task.revision)
    );
    assert!(!serde_json::to_string(&events)?.contains("local_test"));
    let checkpoints: i64 =
        sqlx::query_scalar("SELECT count(*) FROM task_checkpoints WHERE task_id=?")
            .bind(first.id.0)
            .fetch_one(&pool)
            .await?;
    assert_eq!(checkpoints, 8); // planned + three start/end boundaries + completed
    let macro_path = directory.path().join("macros/fixture.json");
    let macro_bytes = tokio::fs::read(&macro_path).await?;
    let recorded = Macro::load(&macro_path).await?;
    assert_eq!(recorded.steps.len(), 3);
    assert!(!String::from_utf8_lossy(&macro_bytes).contains("local_test"));
    // Intentionally unusable planning inputs: Run 2 must use only the recorded plan.
    let mut replay_request = request.clone();
    replay_request.download_selector.clear();
    replay_request.link_selector = Some(String::new());
    assert!(matches!(
        engine
            .run_task(&replay_request, &browser, directory.path(), |_| {})
            .await,
        Err(orchestration_engine::EngineError::HeadlessRequired)
    ));
    let browser = headless_replay(&browser, &executable, directory.path())
        .await
        .map_err(|error| format!("Headless restart: {error}"))?;
    let start = Instant::now();
    let second = engine
        .run_task(&replay_request, &browser, directory.path(), |event| {
            if let Some(gate) = event.approval {
                assert!(engine.decide(gate.task_id, gate.step_index, true).is_ok());
            }
        })
        .await?;
    let elapsed = start.elapsed();
    assert_eq!(
        second.state,
        TaskState::Completed,
        "{second:?}; fixture downloads={}",
        fixture.downloads.load(Ordering::SeqCst)
    );
    assert_eq!(second.mode, RunMode::Replay);
    verify_download(&engine, &second).await?;
    assert_eq!(fixture.downloads.load(Ordering::SeqCst), 2);
    assert_eq!(tokio::fs::read(&macro_path).await?, macro_bytes);
    assert_ne!(
        second.plan.steps[2].output.files,
        first.plan.steps[2].output.files
    );
    verify_actions(&browser, &fixture.url, directory.path()).await?;
    verify_repairs(&engine, &request, &browser, directory.path(), &fixture).await?;
    verify_blob_downloads(&browser, &fixture.url, directory.path())
        .await
        .map_err(|error| format!("Blob download: {error}"))?;
    browser.shutdown().await?;
    pool.close().await;
    directory.close()?;
    verify_latency(&second, elapsed);
    Ok(())
}

async fn inject_fixture_session(
    browser: &ManagedBrowser,
) -> Result<(), browser_driver::BrowserError> {
    browser
        .inject(&[Cookie {
            name: "fixture_session".into(),
            value: zeroize::Zeroizing::new("local_test".into()),
            domain: "127.0.0.1".into(),
            path: "/".into(),
            secure: false,
            http_only: true,
            same_site: CookieSameSite::Lax,
            expires: None,
        }])
        .await?;
    Ok(())
}

async fn verify_blob_downloads(
    browser: &ManagedBrowser,
    origin: &Url,
    root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    browser.navigate(&origin.join("/blobs")?).await?;
    let output = root.join("blob-downloads");
    let result = browser
        .execute_action(
            &Action::DownloadLinks {
                selector: "a.blob-file".into(),
            },
            origin,
            &output,
        )
        .await?;
    assert_eq!(result.files.len(), 2);
    for file in result.files {
        let path = Path::new(&file.path);
        assert!(path.is_absolute());
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("pdf"));
        assert!(tokio::fs::read(path).await?.starts_with(b"%PDF-"));
        assert!(
            !path.with_extension("").exists(),
            "Raw GUID must be removed before returning from CDP"
        );
    }
    Ok(())
}

async fn headless_replay(
    browser: &ManagedBrowser,
    executable: &str,
    root: &Path,
) -> Result<ManagedBrowser, browser_driver::BrowserError> {
    let browser = browser
        .restart(
            Path::new(executable),
            &root.join("profile"),
            browser_driver::LaunchOptions { headless: true },
        )
        .await?;
    assert!(browser.is_headless());
    Ok(browser)
}

async fn verify_download(
    engine: &Engine,
    first: &orchestration_engine::Task,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(first.plan.steps[2].output.files.len(), 1);
    let first_file = &first.plan.steps[2].output.files[0];
    assert_eq!(
        Path::new(&first_file.path)
            .extension()
            .and_then(|ext| ext.to_str()),
        Some("pdf")
    );
    assert_eq!(
        engine.load(first.id).await?.plan.steps[2].output.files[0].path,
        first_file.path
    );
    assert!(
        tokio::fs::read(&first_file.path)
            .await?
            .starts_with(b"%PDF-1.4")
    );
    Ok(())
}

fn verify_latency(task: &orchestration_engine::Task, elapsed: std::time::Duration) {
    eprintln!(
        "Real native CDP replay: {} ms; step timings: {:?}",
        elapsed.as_millis(),
        task.plan
            .steps
            .iter()
            .map(|step| step.elapsed_ms)
            .collect::<Vec<_>>()
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "Local fixture replay exceeded one second: {elapsed:?}"
    );
}

async fn verify_actions(
    browser: &ManagedBrowser,
    origin: &Url,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use browser_driver::{BrowserError, SelectorIssue, WaitCondition};
    for (selector, issue) in [
        ("#absent", SelectorIssue::Missing),
        ("input", SelectorIssue::Ambiguous),
        ("[", SelectorIssue::Invalid),
        ("#hidden", SelectorIssue::NotVisible),
    ] {
        assert!(
            matches!(browser.resolve(selector, false).await, Err(BrowserError::Selector(actual)) if actual == issue)
        );
    }
    let fill = Action::Fill {
        selector: "#filter".into(),
        value: "2026-09".into(),
    };
    browser.execute_action(&fill, origin, output).await?;
    browser
        .wait_for(
            &WaitCondition {
                selector: "#filter[data-applied=yes]".into(),
                timeout_ms: 1000,
            },
            origin,
        )
        .await?;
    let secret = Action::Fill {
        selector: "#secret".into(),
        value: "not-a-real-password".into(),
    };
    assert!(matches!(
        browser.execute_action(&secret, origin, output).await,
        Err(BrowserError::InvalidAction)
    ));
    Ok(())
}

async fn verify_repairs(
    engine: &Engine,
    request: &TaskRequest,
    browser: &ManagedBrowser,
    root: &Path,
    fixture: &Fixture,
) -> Result<(), Box<dyn std::error::Error>> {
    let macro_path = root.join("macros/fixture.json");
    let mut recorded = Macro::load(&macro_path).await?;
    recorded.steps[2].action = Action::DownloadLinks {
        selector: "a.no-longer-exists".into(),
    };
    recorded.save(&macro_path).await?;
    let broken = engine
        .run_task(request, browser, root, |event| {
            if let Some(gate) = event.approval {
                assert!(engine.decide(gate.task_id, gate.step_index, true).is_ok());
            }
        })
        .await?;
    assert_eq!(broken.state, TaskState::NeedsRepair);
    assert_eq!(broken.plan.steps[0].state, StepState::Completed);
    assert_eq!(broken.plan.steps[1].state, StepState::Completed);
    assert_eq!(broken.plan.steps[2].state, StepState::NeedsRepair);
    let repair = broken.repair.as_ref().ok_or("Missing repair request")?;
    assert_eq!(repair.step_index, 2);
    assert_eq!(repair.stage, RepairStage::Target);
    assert_eq!(repair.selector, "a.no-longer-exists");
    assert_eq!(fixture.downloads.load(Ordering::SeqCst), 2);
    assert_eq!(engine.load(broken.id).await?.state, TaskState::NeedsRepair);

    // A missing postcondition must be flagged as a wait failure, not a safe-to-repeat action.
    recorded.steps[0].wait = Some(browser_driver::WaitCondition {
        selector: "#missing-after-navigation".into(),
        timeout_ms: 25,
    });
    recorded.save(&macro_path).await?;
    let wait_failed = engine
        .run_task(request, browser, root, |event| {
            if let Some(gate) = event.approval {
                assert!(engine.decide(gate.task_id, gate.step_index, true).is_ok());
            }
        })
        .await?;
    assert_eq!(
        wait_failed.repair.ok_or("Missing wait repair")?.stage,
        RepairStage::Wait
    );
    assert_eq!(wait_failed.plan.steps[1].state, StepState::Pending);
    assert_eq!(fixture.downloads.load(Ordering::SeqCst), 2);
    Ok(())
}

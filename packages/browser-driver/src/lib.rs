#![deny(unsafe_code)]
//! One managed, isolated Chromium child; native CDP only.
pub mod a11y;
pub mod actions;
mod cursor;
#[cfg(windows)]
mod offscreen;
pub mod picker;
mod preview;
pub mod screencast;
pub mod session;
pub mod som;
pub use som::click_event_sequence;
pub mod test_utils;
pub use a11y::{
    AX_TARGET_RESYNC_LINE, AxElement, AxResyncCheck, interactive_elements,
    interactive_elements_all, render_semantic_list,
};
pub use actions::{Action, ActionOutput, DownloadedFile, Highlight, SelectorIssue, WaitCondition};
/// Raw accessibility node: the wire shape [`interactive_elements`] flattens.
/// Re-exported so integration tests can parse scripted trees without
/// reaching into the CDP bindings directly.
pub use chromiumoxide::cdp::browser_protocol::accessibility::{AxNode, AxValue, AxValueType};
pub use chromiumoxide::cdp::browser_protocol::dom::BackendNodeId;
use chromiumoxide::{
    Browser, Page,
    cdp::browser_protocol::network::{
        CookieSameSite as CdpSameSite, DeleteCookiesParams, GetCookiesParams, SetCookieParams,
        TimeSinceEpoch,
    },
    cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams,
};
pub use cursor::{
    CLICK_DWELL_MS, CursorEmitter, CursorEvent, CursorEventKind, FALLBACK_VIEWPORT, MAX_WAYPOINTS,
    NO_CURSOR_SESSION, WAYPOINT_INTERVAL_MS, WAYPOINT_SPACING_PX, travel_waypoints,
};
use futures::StreamExt;
pub use picker::{
    PICKER_BINDING, PickedElement, PickerRect, parse_binding_payload, rank_selectors_from_attrs,
};
pub use preview::{DomRegion, Viewport};
pub use screencast::{SCREENCAST_JPEG_QUALITY, ScreencastFrame};
pub use session::{
    AuthSignal, AuthState, ChallengeKind, LayoutBox, LayoutBoxes, detect_auth_signal,
    href_from_attributes,
};
use session_sync::{Cookie, CookieSameSite};
pub use som::{ClickHitTest, Mark};
use std::{
    path::Path,
    process::Stdio,
    sync::{
        Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    task::JoinHandle,
};
use url::Url;

pub const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval while waiting for the launched Chromium to publish its
/// `DevTools` endpoint file.
const ENDPOINT_POLL_MS: u64 = 100;
/// `DevTools` endpoint wait: a cold start (Defender rescan, cold disk cache,
/// profile init after a force-killed predecessor) routinely exceeds the
/// interactive I/O budget on Windows. This only gates the poll loop — a
/// fast launch costs nothing extra.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(60);
/// Endpoint-wait attempts: the timed-out child is reaped on drop
/// (`kill_on_drop`) and the retry runs warmer.
const ENDPOINT_ATTEMPTS: u32 = 2;
/// Bytes of captured Chromium stderr kept for a failed-launch diagnostic.
const STDERR_TAIL_BYTES: u64 = 4096;

/// Name the launch stage that failed. `BrowserError` stays coarse for the
/// UI; the stage label on stderr is what makes a launch failure
/// diagnosable instead of a generic "could not be started".
fn log_launch_stage(stage: &str, detail: &dyn std::fmt::Debug) {
    eprintln!("[clinch:browser] launch failed at stage={stage}: {detail:?}");
}

/// Allocate a free loopback TCP port for Chromium's remote-debugging server.
///
/// Binds `127.0.0.1:0` and returns the assigned port, closing the listener
/// immediately. The caller hands the port to Chromium's
/// `--remote-debugging-port` promptly, before anything else can claim it.
///
/// # Errors
///
/// Returns the underlying I/O error if the loopback bind fails.
pub fn pick_free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Probe a Chromium `DevTools` HTTP endpoint and return the browser WebSocket URL.
///
/// Issues `GET /json/version` over plain HTTP/1.1 and extracts
/// `webSocketDebuggerUrl` from the JSON body. Returns `None` while the
/// server is still starting, when the status is not 200, or when the body
/// is not the expected JSON - the caller polls until the server is ready.
///
/// The body is delimited by `Content-Length`, never by EOF: Chromium's
/// `DevTools` HTTP server keeps the connection open even when asked for
/// `Connection: close`, so waiting for EOF would burn the whole probe
/// timeout on every poll. Responses without `Content-Length` fall back to
/// close-delimited reads.
pub async fn probe_devtools_http(port: u16) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
    /// Bound for headers plus body; the version document is a few hundred bytes.
    const MAX_RESPONSE: usize = 64 * 1024;

    let Ok(Ok(mut stream)) = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    else {
        return None;
    };
    let request = format!(
        "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    match tokio::time::timeout(PROBE_TIMEOUT, stream.write_all(request.as_bytes())).await {
        Ok(Ok(())) => {}
        _ => return None,
    }
    // Read until the header terminator; the server may never close the
    // connection, so headers are located by `\r\n\r\n`, not by EOF.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if raw.len() > MAX_RESPONSE {
            return None;
        }
        let n = match tokio::time::timeout(PROBE_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => return None, // EOF before the headers completed.
            Ok(Ok(n)) => n,
            _ => return None,
        };
        raw.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_header_end(&raw) {
            break end;
        }
    };
    let head = std::str::from_utf8(&raw[..header_end]).ok()?;
    let mut lines = head.lines();
    let status = lines.next().unwrap_or("");
    if !status.starts_with("HTTP/1.0 200") && !status.starts_with("HTTP/1.1 200") {
        return None;
    }
    let content_length: Option<usize> = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok());

    let mut body = raw[header_end..].to_vec();
    match content_length {
        Some(want) => {
            if want > MAX_RESPONSE {
                return None;
            }
            while body.len() < want {
                let n = match tokio::time::timeout(PROBE_TIMEOUT, stream.read(&mut chunk)).await {
                    Ok(Ok(0)) => return None, // EOF before the full body arrived.
                    Ok(Ok(n)) => n,
                    _ => return None,
                };
                body.extend_from_slice(&chunk[..n]);
                if body.len() > MAX_RESPONSE {
                    return None;
                }
            }
            body.truncate(want);
        }
        None => {
            // Close-delimited body: read until the server hangs up.
            loop {
                if body.len() > MAX_RESPONSE {
                    return None;
                }
                match tokio::time::timeout(PROBE_TIMEOUT, stream.read(&mut chunk)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => body.extend_from_slice(&chunk[..n]),
                    _ => return None,
                }
            }
        }
    }
    let parsed: serde_json::Value = serde_json::from_slice(&body).ok()?;
    parsed
        .get("webSocketDebuggerUrl")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Locate the end of an HTTP header block (`\r\n\r\n`), returning the offset
/// just past it.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

/// Grace period for orderly Chromium shutdown (Browser.close, then process
/// wait) before falling back to killing the child.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("Chromium could not be launched; check the executable configuration")]
    Launch,
    #[error("Chromium did not respond in time")]
    Timeout,
    #[error("Chromium connection failed")]
    Connection,
    #[error("Chromium rejected the cookie import")]
    Injection,
    #[error("The portal could not be opened")]
    Navigation,
    #[error("The page is replacing its execution context")]
    PageChanging,
    #[error("The recorded selector needs repair")]
    Selector(SelectorIssue),
    #[error("The action or its target is not permitted")]
    InvalidAction,
    #[error("The page left the configured portal")]
    WrongOrigin,
    #[error("The download failed or was canceled")]
    Download,
    #[error("Local download storage is unavailable")]
    Storage,
    #[error("The element picker is unavailable or timed out")]
    Picker,
}

/// Window visibility for a managed Chromium launch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowMode {
    /// Visible headed window (interactive use).
    #[default]
    Headed,
    /// Headed Chromium positioned off-screen and hidden via OS APIs: a
    /// real compositor/GPU for bot-mitigation probes, no visible window.
    /// The OS hide is Windows-only; elsewhere the window sits off-monitor
    /// but keeps its taskbar entry.
    Offscreen,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LaunchOptions {
    pub mode: WindowMode,
}

impl LaunchOptions {
    /// Foreground browser for consent, manual login, and element picking.
    /// The window is app-owned; it never touches the user's daily profile.
    #[must_use]
    pub fn interactive() -> Self {
        Self {
            mode: WindowMode::Headed,
        }
    }

    /// Background runs (macro replay included) launch off-screen headed:
    /// a real headed Chromium (compositor, plugins, screen metrics) with
    /// no visible window. True `--headless=new` is gone — it is the most
    /// fingerprinted mode and nothing about a background run needs it.
    #[must_use]
    pub fn offscreen_headed() -> Self {
        Self {
            mode: WindowMode::Offscreen,
        }
    }
}

/// CLI flags for the window mode, beyond the shared base set. Pure so the
/// off-screen contract (position, size, occlusion flags — and crucially no
/// `--headless`) is hermetically testable.
pub fn window_mode_args(mode: WindowMode) -> &'static [&'static str] {
    match mode {
        WindowMode::Headed => &[],
        WindowMode::Offscreen => &[
            // Far-positive: off every monitor, including negative-offset
            // multi-monitor layouts. Real HWND + compositor for
            // bot-mitigation probes; the OS hide (below) removes the
            // taskbar button on Windows.
            "--window-position=10000,10000",
            "--window-size=1920,1080",
            // An occluded window would otherwise get throttled timers and
            // frozen frames — both detectable, both fatal to a challenge
            // that auto-resolves.
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
        ],
    }
}

/// Select the active top-level page target for `current_url` from a
/// `Target.getTargets` listing: the attached `page`-type target whose URL
/// matches the live browser URL exactly. Returns `None` when no listing
/// entry matches, so callers keep their current handle (fail-open).
/// Pure over the listing, so origin transitions are provable hermetically.
/// Crate-visible for the snapshot re-attachment path in [`a11y`].
pub fn select_active_page_target(
    targets: &[chromiumoxide::cdp::browser_protocol::target::TargetInfo],
    current_url: &Url,
) -> Option<chromiumoxide::cdp::browser_protocol::target::TargetId> {
    targets
        .iter()
        .find(|target| {
            target.r#type == "page" && target.attached && target.url == current_url.as_str()
        })
        .map(|target| target.target_id.clone())
}

/// Normalize an entry URL to its portal anchor: scheme plus host (and an
/// explicit port), without path, query, or fragment. Pure so the binding
/// is hermetically provable.
pub fn anchor_origin(entry: &Url) -> Url {
    let mut anchor = entry.clone();
    anchor.set_path("/");
    anchor.set_query(None);
    anchor.set_fragment(None);
    anchor
}

/// Same-site origin comparison for portal confinement: equivalent to
/// `Url::origin()` equality except a leading `www.` on the host is ignored
/// (`https://www.reddit.com` and `https://reddit.com` are the same site).
/// Strict origin equality breaks the moment a grounded route and the live
/// page disagree on the `www.` prefix — the site redirects bare→www (or
/// vice versa), every snapshot then reads as drifted and comes back empty,
/// and the worker, the auth probe, and the model fallback all go blind at
/// once (caught live: grounder returned bare `reddit.com` while the tab sat
/// on `www.reddit.com`). Scheme and port still compare strictly, and only
/// the `www.` alias is folded — `mail.example.com` is not `example.com`.
/// This mirrors the `www.`-folding the identity memory and the
/// account-home verifier already apply.
#[must_use]
pub fn same_site_origin(a: &Url, b: &Url) -> bool {
    fn normalized_host(url: &Url) -> Option<String> {
        url.host_str().map(|host| {
            let lower = host.to_lowercase();
            lower.strip_prefix("www.").unwrap_or(&lower).to_owned()
        })
    }
    a.scheme() == b.scheme()
        && a.port_or_known_default() == b.port_or_known_default()
        && normalized_host(a) == normalized_host(b)
}

/// Anchored confinement verdict: drift holds only when the live page
/// origin matches neither the call's requested origin nor the active
/// anchor. The anchor is set solely by intentional navigation, so with no
/// anchor this is exactly the legacy strict check. Host comparison folds
/// the `www.` alias (see [`same_site_origin`]).
pub fn is_anchored_drift(live: &Url, requested: &Url, anchor: Option<&Url>) -> bool {
    !same_site_origin(live, requested)
        && anchor.is_none_or(|pinned| !same_site_origin(live, pinned))
}

/// Drift error text naming the active anchor and the live page, e.g.
/// `The page left the configured portal (anchor=https://github.com/,
/// live=https://evil.example/)`.
pub fn drift_error_line(anchor: &Url, live: &str) -> String {
    format!(
        "The page left the configured portal (anchor={}, live={})",
        anchor.as_str(),
        live
    )
}

/// Journal line for an intentional-navigation re-anchor, e.g.
/// `portal_reanchored: none → https://github.com/`.
#[must_use]
pub fn portal_reanchored_line(previous: Option<&Url>, current: &Url) -> String {
    format!(
        "portal_reanchored: {} → {}",
        previous.map_or("none", Url::as_str),
        current.as_str()
    )
}

/// Lightweight first paint for fresh background browsers: a `data:` page
/// rendering `Browser Ready` on a clean background so the screencast
/// preview canvas paints immediately instead of showing a black `about:blank`
/// rectangle. Never navigated via `url_policy` (data scheme is portal-only
/// here); direct `page.goto` only.
pub const BROWSER_READY_URL_STR: &str = "data:text/html,<html><body%20style=\"background:%23f7f7f2;color:%23252722;font-family:sans-serif\">Browser%20Ready</body></html>";

/// Parse the ready-paint URL. `None` only when the constant itself is
/// malformed (never for the checked-in value); callers fall back to
/// `about:blank` instead of failing launch.
#[must_use]
pub fn browser_ready_url() -> Option<Url> {
    Url::parse(BROWSER_READY_URL_STR).ok()
}

// No Debug: CDP objects may contain session data.
pub struct ManagedBrowser {
    mode: WindowMode,
    child: Mutex<Option<Child>>,
    browser: Browser,
    pub page: Page,
    handler: JoinHandle<()>,
    /// Intentional-navigation portal anchor, origin-normalized. Set only by
    /// [`ManagedBrowser::reanchor_portal`] after a proposed route
    /// navigates — never defaulted on launch or session attach, so the
    /// starting tab's incidental origin can never silently confine later
    /// runs. `None` means legacy strict confinement against the call's
    /// requested origin.
    anchored_portal: Mutex<Option<Url>>,
    /// UI cursor-event sink, installed by the service layer while the
    /// screencast pump runs. `None` means no overlay is listening: input
    /// dispatches normally, only the cursor stays invisible.
    cursor_emitter: Mutex<Option<CursorEmitter>>,
    /// Last CDP screencast session id observed by the frame pump
    /// ([`NO_CURSOR_SESSION`] before the first frame). Cursor events carry
    /// it so the UI can apply the same stale-session filter as frames.
    cursor_session: AtomicI64,
    /// Last position (page CSS pixels) where trusted input landed.
    /// [`ManagedBrowser::click_mark`] travels the pointer from here through
    /// intermediate `MouseMoved` dispatches instead of teleporting. `None`
    /// until the first click, and reset to `None` on L1 challenge restart:
    /// the fresh page's pointer position is unknown.
    last_cursor: Mutex<Option<(f64, f64)>>,
}

impl ManagedBrowser {
    /// Spawn Chromium and wait for its `DevTools` HTTP server to answer.
    ///
    /// Chromium's stderr is captured to a per-attempt temp file: a failed
    /// launch must leave diagnostics behind instead of failing silent.
    /// The file is removed best-effort on success; on timeout the tail is
    /// logged (a slow crash looks identical to a slow start from the
    /// outside) and the child is reaped by `kill_on_drop` on drop.
    ///
    /// The debugging port is allocated by us (a free loopback port) rather
    /// than `--remote-debugging-port=0`: on some Linux builds Chrome binds
    /// the ephemeral port but its `DevTools` HTTP server never answers
    /// (observed on Chrome 154 + Xvfb + WSL2 — the `DevToolsActivePort`
    /// file is written, `ss` shows LISTEN, `/json/version` stays mute).
    /// Readiness is confirmed over HTTP (`/json/version` yields the exact
    /// `webSocketDebuggerUrl`), which also removes the file-vs-bind race.
    async fn spawn_and_wait_endpoint(
        executable: &Path,
        profile: &Path,
        options: LaunchOptions,
    ) -> Result<(Child, String), BrowserError> {
        let stderr_path = std::env::temp_dir().join(format!(
            "clinch-chromium-{}.log",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis())
        ));
        let stderr_file = std::fs::File::create(&stderr_path).map_err(|error| {
            log_launch_stage("stderr_capture_create", &error);
            BrowserError::Launch
        })?;
        let debug_port = pick_free_port().map_err(|error| {
            log_launch_stage("debug_port_alloc", &error);
            let _ = std::fs::remove_file(&stderr_path);
            BrowserError::Launch
        })?;
        let mut command = Command::new(executable);
        command
            .arg(format!("--remote-debugging-port={debug_port}"))
            .args([
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-blink-features=AutomationControlled",
                "--no-sandbox",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::process::Stdio::from(stderr_file))
            .kill_on_drop(true);
        for arg in window_mode_args(options.mode) {
            command.arg(arg);
        }
        // Hide the helper console, but keep the requested interactive browser window.
        #[cfg(target_os = "windows")]
        command.creation_flags(0x0800_0000);
        let child = command.spawn().map_err(|error| {
            log_launch_stage("spawn", &error);
            let _ = std::fs::remove_file(&stderr_path);
            BrowserError::Launch
        })?;
        let endpoint = tokio::time::timeout(ENDPOINT_TIMEOUT, async {
            loop {
                if let Some(ws_url) = probe_devtools_http(debug_port).await {
                    return ws_url;
                }
                tokio::time::sleep(Duration::from_millis(ENDPOINT_POLL_MS)).await;
            }
        })
        .await;
        if let Ok(endpoint) = endpoint {
            let _ = std::fs::remove_file(&stderr_path);
            Ok((child, endpoint))
        } else {
            eprintln!(
                "[clinch:browser] launch failed at stage=devtools_endpoint \
                 (timeout after {}s)",
                ENDPOINT_TIMEOUT.as_secs()
            );
            Self::log_stderr_tail(&stderr_path);
            let _ = std::fs::remove_file(&stderr_path);
            // `child` drops here; `kill_on_drop` reaps it.
            Err(BrowserError::Timeout)
        }
    }

    /// Log the tail of a failed launch's captured stderr, best-effort.
    fn log_stderr_tail(stderr_path: &Path) {
        let tail = std::fs::File::open(stderr_path)
            .and_then(|mut file| {
                use std::io::{Read, Seek, SeekFrom};
                let len = file.metadata()?.len();
                let start = len.saturating_sub(STDERR_TAIL_BYTES);
                file.seek(SeekFrom::Start(start))?;
                let mut buf = String::new();
                file.read_to_string(&mut buf)?;
                Ok::<_, std::io::Error>(buf)
            })
            .unwrap_or_default();
        let tail = tail.trim();
        if !tail.is_empty() {
            eprintln!("[clinch:browser] chromium stderr tail: {tail}");
        }
    }

    /// Launch a separate headed Chromium with a persistent app-owned profile.
    ///
    /// # Errors
    /// Reports launch, connection, or timeout failures without exposing CDP data.
    pub async fn launch(executable: &Path, profile: &Path) -> Result<Self, BrowserError> {
        Self::launch_with_options(executable, profile, LaunchOptions::default()).await
    }

    /// Launch the managed profile with explicit window visibility.
    /// # Errors
    /// Reports launch, connection, or timeout failures.
    pub async fn launch_with_options(
        executable: &Path,
        profile: &Path,
        options: LaunchOptions,
    ) -> Result<Self, BrowserError> {
        tokio::fs::create_dir_all(profile).await.map_err(|error| {
            log_launch_stage("profile_dir_create", &error);
            BrowserError::Launch
        })?;
        // Cold-start retry: the first attempt may time out while Windows
        // finishes a cold launch (Defender rescan, cold cache, profile
        // init after a force-killed predecessor). The timed-out child is
        // reaped by `kill_on_drop`; the retry runs warmer.
        let mut attempt = 0;
        let (child, endpoint) = loop {
            attempt += 1;
            match Self::spawn_and_wait_endpoint(executable, profile, options).await {
                Ok(launched) => break launched,
                Err(BrowserError::Timeout) if attempt < ENDPOINT_ATTEMPTS => {
                    eprintln!(
                        "[clinch:browser] devtools endpoint timeout \
                         (attempt {attempt}/{ENDPOINT_ATTEMPTS}); retrying launch"
                    );
                }
                Err(other) => return Err(other),
            }
        };
        // Off-screen headed owns real windows: hide them best-effort so no
        // taskbar button or Alt+Tab entry appears. Detached: the top-level
        // window can appear seconds after spawn on a cold start, and
        // blocking launch on the sweep would stall the escalation that is
        // already time-boxed. A second sweep runs after escalation settles
        // (`ManagedBrowser::hide_windows`); a brief flicker before a sweep
        // lands is accepted and documented.
        #[cfg(windows)]
        if options.mode == WindowMode::Offscreen
            && let Some(pid) = child.id()
        {
            tokio::task::spawn(async move {
                let _ =
                    tokio::task::spawn_blocking(move || offscreen::hide_process_windows(pid)).await;
            });
        }
        let (browser, mut handler) = tokio::time::timeout(IO_TIMEOUT, Browser::connect(endpoint))
            .await
            .map_err(|_| {
                eprintln!("[clinch:browser] launch failed at stage=cdp_connect (timeout)");
                BrowserError::Timeout
            })?
            .map_err(|error| {
                eprintln!("[clinch:browser] launch failed at stage=cdp_connect: {error:?}");
                BrowserError::Connection
            })?;
        let task = tokio::spawn(async move {
            // Owned by ManagedBrowser; command futures expose connection failures.
            while let Some(result) = handler.next().await {
                if result.is_err() {
                    break;
                }
            }
        });
        let Ok(Ok(page)) = tokio::time::timeout(IO_TIMEOUT, browser.new_page("about:blank")).await
        else {
            eprintln!("[clinch:browser] launch failed at stage=new_page");
            task.abort();
            return Err(BrowserError::Connection);
        };
        // Register in the main world before any portal scripts can run. Keep the
        // managed owner alive during setup so failures clean up the child/task.
        let managed = Self {
            mode: options.mode,
            child: Mutex::new(Some(child)),
            browser,
            page,
            handler: task,
            // Deliberately unset: the starting tab's incidental origin must
            // never confine later runs — only intentional navigation anchors.
            anchored_portal: Mutex::new(None),
            cursor_emitter: Mutex::new(None),
            cursor_session: AtomicI64::new(NO_CURSOR_SESSION),
            last_cursor: Mutex::new(None),
        };
        let mut script = AddScriptToEvaluateOnNewDocumentParams::new(
            "Object.defineProperty(navigator, 'webdriver', { get: () => undefined });",
        );
        script.run_immediately = Some(true);
        tokio::time::timeout(IO_TIMEOUT, managed.page.execute(script))
            .await
            .map_err(|_| {
                eprintln!("[clinch:browser] launch failed at stage=stealth_script (timeout)");
                BrowserError::Timeout
            })?
            .map_err(|error| {
                eprintln!("[clinch:browser] launch failed at stage=stealth_script: {error:?}");
                BrowserError::Connection
            })?;
        // Initial paint so the screencast preview never shows a black box:
        // best-effort by design — a failed ready paint leaves `about:blank`
        // rather than failing launch.
        if let Some(ready) = browser_ready_url() {
            let _ = tokio::time::timeout(IO_TIMEOUT, managed.page.goto(ready.as_str())).await;
        }
        Ok(managed)
    }

    /// The launch window mode.
    pub fn window_mode(&self) -> WindowMode {
        self.mode
    }

    /// Attach (or clear, with `None`) the UI cursor-event sink. The service
    /// layer installs it when the screencast pump starts and clears it on
    /// release, so a later session never inherits a stale emitter.
    pub fn set_cursor_emitter(&self, emitter: Option<CursorEmitter>) {
        if let Ok(mut guard) = self.cursor_emitter.lock() {
            *guard = emitter;
        }
    }

    /// Record the CDP screencast session id observed by the frame pump.
    /// Cursor events carry it so the UI can apply the same stale-session
    /// filter it uses for frames.
    pub fn note_screencast_session(&self, session_id: i64) {
        self.cursor_session.store(session_id, Ordering::Relaxed);
    }

    /// Forward one cursor position to the UI overlay, if a sink is attached.
    /// Best-effort and synchronous: a missing or poisoned sink only means no
    /// cursor event — input dispatch itself is never affected.
    fn emit_cursor(&self, event: CursorEvent) {
        if let Ok(guard) = self.cursor_emitter.lock()
            && let Some(emit) = guard.as_ref()
        {
            emit(event);
        }
    }

    /// Whether the session shows no *visible* window. Off-screen headed
    /// counts: it owns real windows, but they are positioned off-monitor
    /// and OS-hidden, so every no-visible-window contract holds for it —
    /// replay gating (`NoVisibleWindowRequired`), the picker refusal (an
    /// invisible overlay can never be clicked), and status reporting.
    pub fn is_headless(&self) -> bool {
        !matches!(self.mode, WindowMode::Headed)
    }

    /// Restart the same app-owned profile, carrying session cookies only in memory.
    /// # Errors
    /// Fails closed if cookies cannot be preserved or the new process cannot start.
    pub async fn restart(
        &self,
        executable: &Path,
        profile: &Path,
        options: LaunchOptions,
    ) -> Result<Self, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::network::CookieParam;
        let cookies = tokio::time::timeout(IO_TIMEOUT, self.browser.get_cookies())
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        let mut params = Vec::with_capacity(cookies.len());
        for cookie in cookies {
            if cookie.partition_key_opaque == Some(true) {
                return Err(BrowserError::Injection);
            }
            let mut param = CookieParam::new(cookie.name, cookie.value);
            if cookie.domain.starts_with('.') {
                param.domain = Some(cookie.domain);
            } else {
                param.url = Some(format!(
                    "{}://{}/",
                    if cookie.secure { "https" } else { "http" },
                    cookie.domain
                ));
            }
            param.path = Some(cookie.path);
            param.secure = Some(cookie.secure);
            param.http_only = Some(cookie.http_only);
            param.same_site = cookie.same_site;
            if !cookie.session {
                param.expires = Some(TimeSinceEpoch::new(cookie.expires));
            }
            param.priority = Some(cookie.priority);
            param.source_scheme = Some(cookie.source_scheme);
            param.source_port = Some(cookie.source_port);
            param.partition_key = cookie.partition_key;
            params.push(param);
        }
        self.shutdown().await?;
        let managed = Self::launch_with_options(executable, profile, options).await?;
        // Carry the cursor sink across the restart so the overlay keeps
        // working mid-run; the screencast session id resets because the new
        // process will produce a new one (until the pump notes it, the UI
        // drops cursor events through its session latch — exactly right).
        if let Ok(guard) = self.cursor_emitter.lock()
            && let Some(emitter) = guard.as_ref()
        {
            managed.set_cursor_emitter(Some(emitter.clone()));
        }
        managed
            .cursor_session
            .store(NO_CURSOR_SESSION, Ordering::Relaxed);
        // The fresh page's pointer position is unknown: reset explicitly so
        // the next click skips travel rather than gliding from a stale
        // position (the fresh launch inits `None`, this pins the L1
        // contract against future init refactors).
        if let Ok(mut guard) = managed.last_cursor.lock() {
            *guard = None;
        }
        tokio::time::timeout(IO_TIMEOUT, managed.browser.set_cookies(params))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Injection)?;
        Ok(managed)
    }

    /// Inject a cookie set through Network.setCookie, preserving the
    /// server's expiry semantics exactly: cookies that arrive with an
    /// `expires` are written to the app profile's cookie database by
    /// Chromium (persistent login), cookies without one stay memory-only.
    /// No lifetime is ever invented — a session cookie the server meant to
    /// be ephemeral must not become a 30-day token. Used by session lending
    /// ("sync once, stay logged in") and the older import paths alike.
    ///
    /// # Errors
    /// Reports rejected or timed-out commands; never marks partial import successful.
    pub async fn inject(&self, cookies: &[Cookie]) -> Result<(), BrowserError> {
        for cookie in cookies {
            let params = cookie_params(cookie)?;
            // Current CDP returns an empty result on success and a protocol error on failure.
            tokio::time::timeout(IO_TIMEOUT, self.page.execute(params))
                .await
                .map_err(|_| BrowserError::Timeout)?
                .map_err(|_| BrowserError::Injection)?;
        }
        Ok(())
    }

    /// Delete every cookie the profile holds for `host` — bare host and
    /// leading-dot domain forms, plus subdomains of it. The revocation
    /// path behind "Forget this site": the persisted session leaves the
    /// app profile's cookie database, so the next run reads the site as
    /// logged out. Returns the number of cookies deleted.
    ///
    /// # Errors
    /// Reports connection failure or timeout.
    pub async fn clear_host_cookies(&self, host: &str) -> Result<usize, BrowserError> {
        let cookies =
            tokio::time::timeout(IO_TIMEOUT, self.page.execute(GetCookiesParams::default()))
                .await
                .map_err(|_| BrowserError::Timeout)?
                .map_err(|_| BrowserError::Connection)?
                .result
                .cookies;
        let mut cleared = 0;
        for cookie in cookies.iter().filter(|cookie| {
            let domain = cookie.domain.trim_start_matches('.');
            // Suffix match on a dot boundary: `evilreddit.com` must not
            // match `reddit.com`.
            domain == host || domain.ends_with(&format!(".{host}"))
        }) {
            let params = DeleteCookiesParams::builder()
                .name(&cookie.name)
                .domain(&cookie.domain)
                .build()
                .map_err(|_| BrowserError::Injection)?;
            tokio::time::timeout(IO_TIMEOUT, self.page.execute(params))
                .await
                .map_err(|_| BrowserError::Timeout)?
                .map_err(|_| BrowserError::Injection)?;
            cleared += 1;
        }
        Ok(cleared)
    }

    /// Read the live target's current URL, if the page reports a parseable
    /// one. Additive accessor for navigation pre-conditions elsewhere;
    /// never navigates, never fails closed on drift — callers decide.
    ///
    /// # Errors
    /// Reports connection failure or timeout.
    pub async fn current_url(&self) -> Result<Option<Url>, BrowserError> {
        let current = tokio::time::timeout(IO_TIMEOUT, self.page.url())
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(current.and_then(|url| Url::parse(&url).ok()))
    }

    /// Open the portal for session verification or manual login.
    ///
    /// # Errors
    /// Reports navigation failure or timeout.
    pub async fn navigate(&self, portal: &Url) -> Result<(), BrowserError> {
        tokio::time::timeout(IO_TIMEOUT, self.page.goto(portal.as_str()))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Navigation)?;
        Ok(())
    }

    /// Re-anchor the driver on the live top-level page session after a
    /// settled navigation. Cross-origin renderer swaps can leave CDP
    /// domains armed on the previous process; this delegates to the same
    /// targeted re-attachment the snapshot path uses (re-query targets,
    /// select the live page, ensure attachment, re-arm Accessibility).
    /// Fail-open by design — returns nothing — because the snapshot path
    /// re-verifies before acting.
    pub async fn refresh_target_session(&self) {
        let page_url = self.current_url().await.ok().flatten();
        self.reattach_active_target(page_url.as_ref()).await;
    }

    /// Active portal anchor, if intentional navigation set one. `None`
    /// until the first [`ManagedBrowser::reanchor_portal`] call.
    #[must_use]
    pub fn portal_anchor(&self) -> Option<Url> {
        self.anchored_portal.lock().ok()?.clone()
    }

    /// Bind portal confinement to an intentionally navigated entry route's
    /// scheme and host. Returns the previous anchor for journaling;
    /// idempotent — re-anchoring the same origin changes nothing
    /// downstream. Fail-open: a poisoned lock leaves the anchor untouched
    /// and reports no previous anchor.
    pub fn reanchor_portal(&self, entry: &Url) -> Option<Url> {
        self.anchored_portal
            .lock()
            .ok()
            .and_then(|mut slot| slot.replace(anchor_origin(entry)))
    }

    /// Confinement check honoring intentional navigation: passes when the
    /// live page matches the requested origin or the anchored portal, and
    /// fails closed otherwise (drift, unreadable URL, or CDP failure).
    /// With no anchor this is exactly [`ManagedBrowser::check_origin`].
    ///
    /// # Errors
    /// Returns [`BrowserError`] on drift, unreadable URLs, or CDP failures.
    pub async fn check_anchored_origin(&self, origin: &Url) -> Result<(), BrowserError> {
        let anchor = self.portal_anchor();
        let live = self.current_url().await?.ok_or(BrowserError::WrongOrigin)?;
        if is_anchored_drift(&live, origin, anchor.as_ref()) {
            return Err(BrowserError::WrongOrigin);
        }
        Ok(())
    }

    /// Stop and reap the managed child before its profile can be reopened.
    ///
    /// # Errors
    /// Reports failed or timed-out process shutdown.
    pub async fn shutdown(&self) -> Result<(), BrowserError> {
        let child = self
            .child
            .lock()
            .map_err(|_| BrowserError::Connection)?
            .take();
        if let Some(mut child) = child {
            // A closing socket may not acknowledge Browser.close. Process exit is
            // the authoritative result; prefer graceful exit to flush profile data.
            let _ = tokio::time::timeout(
                SHUTDOWN_GRACE,
                self.browser
                    .execute(chromiumoxide::cdp::browser_protocol::browser::CloseParams::default()),
            )
            .await;
            if !matches!(
                tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await,
                Ok(Ok(_))
            ) {
                tokio::time::timeout(IO_TIMEOUT, child.kill())
                    .await
                    .map_err(|_| BrowserError::Timeout)?
                    .map_err(|_| BrowserError::Connection)?;
            }
        }
        self.handler.abort();
        Ok(())
    }

    /// Best-effort synchronous termination for the native app exit callback.
    pub fn terminate(&self) {
        self.handler.abort();
        if let Ok(mut guard) = self.child.lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.start_kill();
        }
    }

    /// Re-run the best-effort Win32 window hide for this session's process.
    /// Catches top-level windows that appeared after the launch-time sweep
    /// (slow cold starts). Used after challenge escalation settles, so a
    /// persistent challenge never leaves a taskbar button while it waits
    /// for L2 Take Control. No-op off Windows.
    #[cfg(windows)]
    pub async fn hide_windows(&self) {
        let pid = self
            .child
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().and_then(|child| child.id()));
        if let Some(pid) = pid {
            let _ = tokio::task::spawn_blocking(move || offscreen::hide_process_windows(pid)).await;
        }
    }

    /// No-op off Windows: there is no top-level window to hide. `async` is
    /// kept so call sites don't need platform gates.
    #[cfg(not(windows))]
    #[allow(clippy::unused_async)]
    pub async fn hide_windows(&self) {}
}

impl Drop for ManagedBrowser {
    fn drop(&mut self) {
        self.terminate();
        // Child::kill_on_drop terminates our child, never the user's daily browser.
    }
}

/// Map a bridge cookie to CDP `Network.setCookie` params, preserving the
/// server's expiry semantics exactly: a validated `expires` is passed
/// through so Chromium persists the cookie to the profile's cookie
/// database; an absent `expires` stays a memory-only session cookie.
/// Lifetimes are never invented.
pub fn cookie_params(cookie: &Cookie) -> Result<SetCookieParams, BrowserError> {
    let mut builder = SetCookieParams::builder()
        .name(&cookie.name)
        .value(cookie.value.as_str())
        .path(&cookie.path)
        .secure(cookie.secure)
        .http_only(cookie.http_only);
    if cookie.domain.starts_with('.') {
        builder = builder.domain(&cookie.domain);
    } else {
        // Omitting domain and supplying URL preserves host-only cookie semantics.
        let scheme = if cookie.secure { "https" } else { "http" };
        builder = builder.url(format!("{scheme}://{}/", cookie.domain));
    }
    builder = match cookie.same_site {
        CookieSameSite::Unspecified => builder,
        CookieSameSite::None => builder.same_site(CdpSameSite::None),
        CookieSameSite::Lax => builder.same_site(CdpSameSite::Lax),
        CookieSameSite::Strict => builder.same_site(CdpSameSite::Strict),
    };
    if let Some(expires) = cookie.expires {
        // Avoid a 2038 cutoff. CDP represents seconds as a floating-point number.
        if !(0..=253_402_300_799).contains(&expires) {
            return Err(BrowserError::Injection);
        }
        // The validated year-9999 bound is below f64's exact integer limit (2^53).
        #[allow(clippy::cast_precision_loss)]
        let seconds = expires as f64;
        builder = builder.expires(TimeSinceEpoch::new(seconds));
    }
    builder.build().map_err(|_| BrowserError::Injection)
}

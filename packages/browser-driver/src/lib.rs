#![deny(unsafe_code)]
//! One managed, isolated Chromium child; native CDP only.
pub mod a11y;
mod actions;
#[cfg(windows)]
mod offscreen;
mod picker;
mod preview;
mod screencast;
mod session;
mod som;
pub mod test_utils;
pub use a11y::{
    AX_TARGET_RESYNC_LINE, AxElement, AxResyncCheck, interactive_elements, render_semantic_list,
};
pub use actions::{Action, ActionOutput, DownloadedFile, Highlight, SelectorIssue, WaitCondition};
/// Raw accessibility node: the wire shape [`interactive_elements`] flattens.
/// Re-exported so integration tests can parse scripted trees without
/// reaching into the CDP bindings directly.
pub use chromiumoxide::cdp::browser_protocol::accessibility::AxNode;
use chromiumoxide::{
    Browser, Page,
    cdp::browser_protocol::network::{
        CookieSameSite as CdpSameSite, SetCookieParams, TimeSinceEpoch,
    },
    cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams,
};
use futures::StreamExt;
pub use picker::{
    PICKER_BINDING, PickedElement, PickerRect, parse_binding_payload, rank_selectors_from_attrs,
};
pub use preview::{DomRegion, Viewport};
pub use screencast::{SCREENCAST_JPEG_QUALITY, ScreencastFrame};
pub use session::{AuthSignal, detect_auth_signal};
use session_sync::{Cookie, CookieSameSite};
pub use som::Mark;
use std::{path::Path, process::Stdio, sync::Mutex, time::Duration};
use tokio::{
    process::{Child, Command},
    task::JoinHandle,
};
use url::Url;

const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval while waiting for the launched Chromium to publish its
/// `DevTools` endpoint file.
const ENDPOINT_POLL_MS: u64 = 100;
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowMode {
    /// Visible headed window (interactive use).
    #[default]
    Headed,
    /// `--headless=new`: no OS window at all.
    Headless,
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

    /// Background macro replay. No OS window may spawn on this path —
    /// enforced by `Engine::run_task` (`HeadlessRequired`) and asserted by
    /// the `headless_replay_*` integration tests.
    #[must_use]
    pub fn replay() -> Self {
        Self {
            mode: WindowMode::Headless,
        }
    }

    /// Automatic bot-challenge escalation: headed for Cloudflare's probes,
    /// off-screen and OS-hidden so no window appears. Never interactive —
    /// the window is hidden, not handed to the user.
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
fn window_mode_args(mode: WindowMode) -> &'static [&'static str] {
    match mode {
        WindowMode::Headless => &["--headless=new"],
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
pub(crate) fn select_active_page_target(
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
fn anchor_origin(entry: &Url) -> Url {
    let mut anchor = entry.clone();
    anchor.set_path("/");
    anchor.set_query(None);
    anchor.set_fragment(None);
    anchor
}

/// Anchored confinement verdict: drift holds only when the live page
/// origin matches neither the call's requested origin nor the active
/// anchor. The anchor is set solely by intentional navigation, so with no
/// anchor this is exactly the legacy strict check.
fn is_anchored_drift(live: &Url, requested: &Url, anchor: Option<&Url>) -> bool {
    live.origin() != requested.origin()
        && anchor.is_none_or(|pinned| live.origin() != pinned.origin())
}

/// Drift error text naming the active anchor and the live page, e.g.
/// `The page left the configured portal (anchor=https://github.com/,
/// live=https://evil.example/)`.
fn drift_error_line(anchor: &Url, live: &str) -> String {
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
    page: Page,
    handler: JoinHandle<()>,
    /// Intentional-navigation portal anchor, origin-normalized. Set only by
    /// [`ManagedBrowser::reanchor_portal`] after a proposed route
    /// navigates — never defaulted on launch or session attach, so the
    /// starting tab's incidental origin can never silently confine later
    /// runs. `None` means legacy strict confinement against the call's
    /// requested origin.
    anchored_portal: Mutex<Option<Url>>,
}

impl ManagedBrowser {
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
        tokio::fs::create_dir_all(profile)
            .await
            .map_err(|_| BrowserError::Launch)?;
        // Stale endpoint files must not connect us to an unrelated earlier process.
        let endpoint_file = profile.join("DevToolsActivePort");
        match tokio::fs::remove_file(&endpoint_file).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(BrowserError::Launch),
        }
        let mut command = Command::new(executable);
        command
            .args([
                "--remote-debugging-port=0",
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
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for arg in window_mode_args(options.mode) {
            command.arg(arg);
        }
        // Hide the helper console, but keep the requested interactive browser window.
        #[cfg(target_os = "windows")]
        command.creation_flags(0x0800_0000);
        let child = command.spawn().map_err(|_| BrowserError::Launch)?;
        // Off-screen headed owns real windows: hide them best-effort so no
        // taskbar button or Alt+Tab entry appears. A brief flicker before
        // the hide lands is accepted and documented; failure just leaves
        // the off-screen window in place.
        #[cfg(windows)]
        if options.mode == WindowMode::Offscreen
            && let Some(pid) = child.id()
        {
            let _ = tokio::task::spawn_blocking(move || offscreen::hide_process_windows(pid)).await;
        }
        let endpoint = tokio::time::timeout(IO_TIMEOUT, async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&endpoint_file).await {
                    let mut lines = contents.lines();
                    if let (Some(port), Some(path)) = (lines.next(), lines.next())
                        && let Ok(port) = port.parse::<u16>()
                        && port != 0
                        && path.starts_with("/devtools/browser/")
                    {
                        return format!("ws://127.0.0.1:{port}{path}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(ENDPOINT_POLL_MS)).await;
            }
        })
        .await
        .map_err(|_| BrowserError::Timeout)?;
        let (browser, mut handler) = tokio::time::timeout(IO_TIMEOUT, Browser::connect(endpoint))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
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
        };
        let mut script = AddScriptToEvaluateOnNewDocumentParams::new(
            "Object.defineProperty(navigator, 'webdriver', { get: () => undefined });",
        );
        script.run_immediately = Some(true);
        tokio::time::timeout(IO_TIMEOUT, managed.page.execute(script))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
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

    /// Whether the session shows no *visible* window. Off-screen headed
    /// counts: it owns real windows, but they are positioned off-monitor
    /// and OS-hidden, so every no-visible-window contract holds for it —
    /// replay gating (`HeadlessRequired`), the picker refusal (an
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
        tokio::time::timeout(IO_TIMEOUT, managed.browser.set_cookies(params))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Injection)?;
        Ok(managed)
    }

    /// Inject only the prepared cookie set through Network.setCookie.
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
}

impl Drop for ManagedBrowser {
    fn drop(&mut self) {
        self.terminate();
        // Child::kill_on_drop terminates our child, never the user's daily browser.
    }
}

fn cookie_params(cookie: &Cookie) -> Result<SetCookieParams, BrowserError> {
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

#[cfg(test)]
mod tests {
    use super::*;
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
    fn launch_options_separate_replay_from_interactive() {
        // Phase B headless-first contract: replays never spawn an OS window.
        assert_eq!(LaunchOptions::replay().mode, WindowMode::Headless);
        assert_eq!(LaunchOptions::interactive().mode, WindowMode::Headed);
        assert_eq!(LaunchOptions::default().mode, WindowMode::Headed);
        // The escalation rung is headed under the hood but shows no window.
        assert_eq!(
            LaunchOptions::offscreen_headed().mode,
            WindowMode::Offscreen
        );
        assert_ne!(LaunchOptions::offscreen_headed().mode, WindowMode::Headless);
    }

    #[test]
    fn window_mode_args_keep_offscreen_headed_not_headless() {
        // Headless carries exactly the headless flag and nothing else.
        assert_eq!(window_mode_args(WindowMode::Headless), &["--headless=new"]);
        // Visible headed adds nothing: it must not inherit off-screen flags.
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
    #[ignore = "Requires CLINCH_CHROMIUM_PATH and launches a real headless browser using a temporary profile"]
    async fn headless_launch_confirms_no_window() -> Result<(), Box<dyn std::error::Error>> {
        let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
        let profile = tempfile::tempdir()?;
        let options = LaunchOptions::replay();
        assert_eq!(options.mode, WindowMode::Headless);
        let browser =
            ManagedBrowser::launch_with_options(Path::new(&executable), profile.path(), options)
                .await?;
        assert!(browser.is_headless());
        browser.shutdown().await?;
        profile.close()?;
        Ok(())
    }
}

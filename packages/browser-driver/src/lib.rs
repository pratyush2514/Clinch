#![deny(unsafe_code)]
//! One managed, isolated Chromium child; native CDP only.
pub mod a11y;
mod actions;
mod picker;
mod preview;
mod session;
mod som;
pub use a11y::{AxElement, interactive_elements, render_semantic_list};
pub use actions::{Action, ActionOutput, DownloadedFile, Highlight, SelectorIssue, WaitCondition};
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

#[derive(Clone, Copy, Debug, Default)]
pub struct LaunchOptions {
    pub headless: bool,
}

impl LaunchOptions {
    /// Foreground browser for consent, manual login, and element picking.
    /// The window is app-owned; it never touches the user's daily profile.
    #[must_use]
    pub fn interactive() -> Self {
        Self { headless: false }
    }

    /// Background macro replay. No OS window may spawn on this path —
    /// enforced by `Engine::run_task` (`HeadlessRequired`) and asserted by
    /// the `headless_replay_*` integration tests.
    #[must_use]
    pub fn replay() -> Self {
        Self { headless: true }
    }
}

// No Debug: CDP objects may contain session data.
pub struct ManagedBrowser {
    headless: bool,
    child: Mutex<Option<Child>>,
    browser: Browser,
    page: Page,
    handler: JoinHandle<()>,
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
        if options.headless {
            command.arg("--headless=new");
        }
        // Hide the helper console, but keep the requested interactive browser window.
        #[cfg(target_os = "windows")]
        command.creation_flags(0x0800_0000);
        let child = command.spawn().map_err(|_| BrowserError::Launch)?;
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
                tokio::time::sleep(Duration::from_millis(100)).await;
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
            headless: options.headless,
            child: Mutex::new(Some(child)),
            browser,
            page,
            handler: task,
        };
        let mut script = AddScriptToEvaluateOnNewDocumentParams::new(
            "Object.defineProperty(navigator, 'webdriver', { get: () => undefined });",
        );
        script.run_immediately = Some(true);
        tokio::time::timeout(IO_TIMEOUT, managed.page.execute(script))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(managed)
    }

    pub fn is_headless(&self) -> bool {
        self.headless
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
                Duration::from_secs(5),
                self.browser
                    .execute(chromiumoxide::cdp::browser_protocol::browser::CloseParams::default()),
            )
            .await;
            if !matches!(
                tokio::time::timeout(Duration::from_secs(5), child.wait()).await,
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
        assert!(LaunchOptions::replay().headless);
        assert!(!LaunchOptions::interactive().headless);
        assert!(!LaunchOptions::default().headless);
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
        assert!(options.headless);
        let browser =
            ManagedBrowser::launch_with_options(Path::new(&executable), profile.path(), options)
                .await?;
        assert!(browser.is_headless());
        browser.shutdown().await?;
        profile.close()?;
        Ok(())
    }
}

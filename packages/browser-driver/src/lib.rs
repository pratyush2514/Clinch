#![deny(unsafe_code)]
//! One managed, isolated Chromium child; native CDP only.
use chromiumoxide::{
    Browser, Page,
    cdp::browser_protocol::network::{
        CookieSameSite as CdpSameSite, SetCookieParams, TimeSinceEpoch,
    },
};
use futures::StreamExt;
use session_sync::{Cookie, CookieSameSite};
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
}

// No Debug: CDP objects may contain session data.
pub struct ManagedBrowser {
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
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
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
        Ok(Self {
            child: Mutex::new(Some(child)),
            browser,
            page,
            handler: task,
        })
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

    #[tokio::test]
    #[ignore = "Requires CLINCH_CHROMIUM_PATH and launches a real browser using a temporary profile"]
    async fn real_cdp_cookie_injection() -> Result<(), Box<dyn std::error::Error>> {
        use chromiumoxide::cdp::browser_protocol::network::GetCookiesParams;
        let executable = std::env::var("CLINCH_CHROMIUM_PATH")?;
        let profile = tempfile::tempdir()?;
        let browser = ManagedBrowser::launch(Path::new(&executable), profile.path()).await?;
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
}

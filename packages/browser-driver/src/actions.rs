#![deny(unsafe_code)]
//! Small typed CDP vocabulary. No arbitrary JavaScript or credentials in macros.
use crate::{BrowserError, IO_TIMEOUT, ManagedBrowser, same_site_origin};
use chromiumoxide::cdp::browser_protocol::browser::{
    DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};
use url::Url;

/// Poll interval for condition waits: tight enough to notice fast DOM
/// updates, loose enough to avoid CDP spam while a page settles.
const WAIT_POLL_MS: u64 = 25;
/// Upper bound on one typed action's CDP execution, generous enough for
/// slow downloads to start streaming before the caller gives up.
const ACTION_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Navigate {
        url: Url,
    },
    Submit {
        selector: String,
    },
    Click {
        selector: String,
    },
    /// Only non-secret filter inputs; credentials remain in the manual-login flow.
    Fill {
        selector: String,
        value: String,
    },
    DownloadLinks {
        selector: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WaitCondition {
    pub selector: String,
    pub timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectorIssue {
    Missing,
    Ambiguous,
    Invalid,
    NotVisible,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Highlight {
    pub selector: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub matches: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedFile {
    pub path: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ActionOutput {
    pub files: Vec<DownloadedFile>,
}

#[derive(Deserialize)]
struct Resolution {
    issue: Option<SelectorIssue>,
    target: Option<Highlight>,
}

/// Map a CDP evaluation failure to the right [`BrowserError`]: a navigation
/// destroying the JS execution context mid-read is a retryable race, not a
/// dead connection. Shared with the viewport preview so a capture raced by
/// a committing navigation reports the same retryable error.
pub(crate) fn evaluation_error(error: chromiumoxide::error::CdpError) -> BrowserError {
    use chromiumoxide::error::CdpError;
    match error {
        CdpError::Chrome(error)
            if error.message.contains("Cannot find context")
                || error.message.contains("Execution context was destroyed") =>
        {
            BrowserError::PageChanging
        }
        _ => BrowserError::Connection,
    }
}

impl Action {
    #[must_use]
    pub fn selector(&self) -> Option<&str> {
        match self {
            Self::Navigate { .. } => None,
            Self::Submit { selector }
            | Self::Click { selector }
            | Self::Fill { selector, .. }
            | Self::DownloadLinks { selector } => Some(selector),
        }
    }

    /// # Errors
    /// Rejects unsafe URLs, oversize inputs, or empty selectors.
    pub fn validate(&self, origin: &Url) -> Result<(), BrowserError> {
        if let Some(selector) = self.selector()
            && (selector.trim().is_empty() || selector.len() > 2048)
        {
            return Err(BrowserError::InvalidAction);
        }
        match self {
            Self::Navigate { url }
                if !same_site_origin(url, origin)
                    || !matches!(url.scheme(), "https" | "http")
                    || !url.username().is_empty()
                    || url.password().is_some() =>
            {
                Err(BrowserError::InvalidAction)
            }
            Self::Fill { value, .. } if value.len() > 4096 => Err(BrowserError::InvalidAction),
            _ => Ok(()),
        }
    }
}

// Blob URLs retain their creator's origin. Opaque and cross-origin blobs are rejected.
pub fn validate_download_url(url: &Url, origin: &Url) -> Result<(), BrowserError> {
    if url.scheme() == "blob" {
        let inner = Url::parse(url.path()).map_err(|_| BrowserError::InvalidAction)?;
        Action::Navigate { url: inner }.validate(origin)
    } else {
        Action::Navigate { url: url.clone() }.validate(origin)
    }
}

pub fn matches_download_url(actual: &str, expected: &Url, origin: &Url) -> bool {
    let Ok(actual) = Url::parse(actual) else {
        return false;
    };
    validate_download_url(&actual, origin).is_ok()
        && (actual == *expected || actual.scheme() == "blob")
}

impl ManagedBrowser {
    /// Check origin before reading or acting on a portal DOM.
    /// # Errors
    /// Returns connection errors or a wrong-origin failure (e.g. login redirect).
    pub async fn check_origin(&self, origin: &Url) -> Result<(), BrowserError> {
        let current = tokio::time::timeout(IO_TIMEOUT, self.page.url())
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        let current = current
            .and_then(|s| Url::parse(&s).ok())
            .ok_or(BrowserError::WrongOrigin)?;
        if !same_site_origin(&current, origin) {
            return Err(BrowserError::WrongOrigin);
        }
        Ok(())
    }

    /// Resolve a selector without collecting page text or input values.
    /// # Errors
    /// Distinguishes missing, ambiguous, invalid, and invisible selectors from CDP failures.
    pub async fn resolve(&self, selector: &str, multiple: bool) -> Result<Highlight, BrowserError> {
        let selector_json =
            serde_json::to_string(selector).map_err(|_| BrowserError::InvalidAction)?;
        let expression = format!(
            r"(() => {{
            let nodes; try {{ nodes = document.querySelectorAll({selector_json}); }}
            catch (_) {{ return {{issue:'invalid'}}; }}
            if (!nodes.length) return {{issue:'missing'}};
            const e = nodes[0], r = e.getBoundingClientRect();
            const s = getComputedStyle(e);
            if (!r.width || !r.height || s.visibility === 'hidden' || s.display === 'none')
                return {{issue:'not_visible'}};
            return {{target: {{selector:{selector_json}, x:r.x,y:r.y,width:r.width,height:r.height,matches:nodes.length}}}};
        }})()"
        );
        let result = tokio::time::timeout(IO_TIMEOUT, self.page.evaluate(expression))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(evaluation_error)?
            .into_value::<Resolution>()
            .map_err(|_| BrowserError::Connection)?;
        if let Some(issue) = result.issue {
            return Err(BrowserError::Selector(issue));
        }
        let target = result.target.ok_or(BrowserError::Connection)?;
        if !multiple && target.matches != 1 {
            return Err(BrowserError::Selector(SelectorIssue::Ambiguous));
        }
        if target.matches > 25 {
            return Err(BrowserError::InvalidAction);
        }
        Ok(target)
    }

    /// Bounded condition wait, checked immediately before polling; no fixed action delays.
    /// # Errors
    /// Returns selector errors on deadline, or propagates connection/origin failures.
    pub async fn wait_for(&self, wait: &WaitCondition, origin: &Url) -> Result<(), BrowserError> {
        if wait.timeout_ms == 0 || wait.timeout_ms > 30_000 {
            return Err(BrowserError::InvalidAction);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait.timeout_ms);
        loop {
            self.check_origin(origin).await?;
            match self.resolve(&wait.selector, true).await {
                Ok(_) => return Ok(()),
                Err(
                    error @ BrowserError::Selector(
                        SelectorIssue::Missing | SelectorIssue::NotVisible,
                    ),
                ) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(WAIT_POLL_MS)).await;
                }
                // Navigation destroys the old JS context. Retry only this read-only probe,
                // never the preceding click or download, and retain the original deadline.
                Err(BrowserError::PageChanging) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(BrowserError::Timeout);
                    }
                    tokio::time::sleep(Duration::from_millis(WAIT_POLL_MS)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Execute typed actions through the existing native CDP WebSocket connection.
    /// # Errors
    /// Returns explicit validation, selector, CDP, timeout, or download errors.
    pub async fn execute_action(
        &self,
        action: &Action,
        origin: &Url,
        output: &Path,
    ) -> Result<ActionOutput, BrowserError> {
        action.validate(origin)?;
        if let Action::Navigate { url } = action {
            self.navigate(url).await?;
            self.check_origin(origin).await?;
            return Ok(ActionOutput::default());
        }
        self.check_origin(origin).await?;
        let selector = action.selector().ok_or(BrowserError::InvalidAction)?;
        self.resolve(selector, matches!(action, Action::DownloadLinks { .. }))
            .await?;
        tokio::time::timeout(ACTION_TIMEOUT, async {
            match action {
                Action::Click { .. } => {
                    let element = self.page.find_element(selector).await.map_err(|_| BrowserError::Connection)?;
                    // Phase A navigation only: arbitrary buttons/forms are not an approval bypass.
                    let href = element.attribute("href").await.map_err(|_| BrowserError::Connection)?
                        .ok_or(BrowserError::InvalidAction)?;
                    let target = self.link_url(&href).await?;
                    Action::Navigate { url: target }.validate(origin)?;
                    element.click().await.map_err(|_| BrowserError::Connection)?;
                    Ok(ActionOutput::default())
                }
                Action::Fill { value, .. } => {
                    let element = self.page.find_element(selector).await.map_err(|_| BrowserError::Connection)?;
                    let allowed = element.call_js_fn("function() { return this.tagName === 'INPUT' && ['search','date','month','number'].includes(this.type); }", false).await
                        .map_err(|_| BrowserError::Connection)?.result.value.and_then(|value| value.as_bool()).ok_or(BrowserError::Connection)?;
                    if !allowed { return Err(BrowserError::InvalidAction); }
                    element.call_js_fn("function() { this.value = ''; }", false).await.map_err(|_| BrowserError::Connection)?;
                    element.focus().await.map_err(|_| BrowserError::Connection)?;
                    self.page.execute(chromiumoxide::cdp::browser_protocol::input::InsertTextParams::new(value)).await.map_err(|_| BrowserError::Connection)?;
                    Ok(ActionOutput::default())
                }
                Action::DownloadLinks { .. } => self.download_links(selector, origin, output).await,
                Action::Submit { .. } | Action::Navigate { .. } => Err(BrowserError::InvalidAction),
            }
        }).await.map_err(|_| BrowserError::Timeout)?
    }

    /// Submit only after the orchestration gate has granted this exact action.
    /// # Errors
    /// Rejects non-form targets, cross-origin form destinations, and CDP failures.
    pub async fn submit_approved(
        &self,
        selector: &str,
        origin: &Url,
    ) -> Result<ActionOutput, BrowserError> {
        self.check_origin(origin).await?;
        self.resolve(selector, false).await?;
        tokio::time::timeout(IO_TIMEOUT, async {
            let element = self.page.find_element(selector).await.map_err(|_| BrowserError::Connection)?;
            let valid = element.call_js_fn("function() { return this.tagName === 'FORM' && new URL(this.action,location.href).origin === location.origin && !this.target; }", false).await
                .map_err(|_| BrowserError::Connection)?.result.value.and_then(|v| v.as_bool()).unwrap_or(false);
            if !valid { return Err(BrowserError::InvalidAction); }
            element.call_js_fn("function() { this.requestSubmit(); }", false).await.map_err(|_| BrowserError::Connection)?;
            Ok(ActionOutput::default())
        }).await.map_err(|_| BrowserError::Timeout)?
    }

    async fn link_url(&self, href: &str) -> Result<Url, BrowserError> {
        let current = self
            .page
            .url()
            .await
            .map_err(|_| BrowserError::Connection)?
            .ok_or(BrowserError::WrongOrigin)?;
        Url::parse(&current)
            .and_then(|url| url.join(href))
            .map_err(|_| BrowserError::InvalidAction)
    }

    async fn download_links(
        &self,
        selector: &str,
        origin: &Url,
        output: &Path,
    ) -> Result<ActionOutput, BrowserError> {
        tokio::fs::create_dir_all(output)
            .await
            .map_err(|_| BrowserError::Storage)?;
        // App service supplies an absolute, run-scoped directory. Windows canonicalize()
        // introduces a verbatim \\?\ prefix that Chromium's download path rejects.
        if !output.is_absolute() {
            return Err(BrowserError::Storage);
        }
        let mut params = SetDownloadBehaviorParams::new(SetDownloadBehaviorBehavior::AllowAndName);
        params.download_path = Some(output.to_string_lossy().into_owned());
        params.events_enabled = Some(true);
        self.browser
            .execute(params)
            .await
            .map_err(|_| BrowserError::Download)?;
        let mut begins = self
            .browser
            .event_listener::<EventDownloadWillBegin>()
            .await
            .map_err(|_| BrowserError::Connection)?;
        let mut progress = self
            .browser
            .event_listener::<EventDownloadProgress>()
            .await
            .map_err(|_| BrowserError::Connection)?;
        let elements = self
            .page
            .find_elements(selector)
            .await
            .map_err(|_| BrowserError::Connection)?;
        if elements.is_empty() || elements.len() > 25 {
            return Err(BrowserError::InvalidAction);
        }
        // Validate every link before the first download. No JS URLs, forms, or off-origin targets.
        let mut urls = Vec::with_capacity(elements.len());
        for element in &elements {
            let href = element
                .attribute("href")
                .await
                .map_err(|_| BrowserError::Connection)?
                .ok_or(BrowserError::InvalidAction)?;
            let url = self.link_url(&href).await?;
            validate_download_url(&url, origin)?;
            urls.push(url);
        }
        let frame = self
            .page
            .mainframe()
            .await
            .map_err(|_| BrowserError::Connection)?
            .ok_or(BrowserError::Connection)?;
        let mut files = Vec::with_capacity(elements.len());
        for (element, url) in elements.iter().zip(urls) {
            self.check_origin(origin).await?;
            element.click().await.map_err(|_| BrowserError::Download)?;
            let begin = begins.next().await.ok_or(BrowserError::Connection)?;
            if begin.frame_id != frame
                || !matches_download_url(&begin.url, &url, origin)
                || begin.guid.is_empty()
                || !begin
                    .guid
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            {
                return Err(BrowserError::Download);
            }
            loop {
                let event = progress.next().await.ok_or(BrowserError::Connection)?;
                if event.guid != begin.guid {
                    continue;
                }
                match event.state {
                    DownloadProgressState::Canceled => return Err(BrowserError::Download),
                    DownloadProgressState::Completed => break,
                    DownloadProgressState::InProgress => {}
                }
            }
            // CDP completion is the write-completion watchdog, including memory-backed blobs.
            // Finalize each file before clicking another link or returning its path to callers.
            let path = filesystem_tool::preserve_extension(&output.join(&begin.guid))
                .await
                .map_err(|_| BrowserError::Storage)?;
            let metadata = tokio::fs::metadata(&path)
                .await
                .map_err(|_| BrowserError::Storage)?;
            if !metadata.is_file() || metadata.len() == 0 {
                return Err(BrowserError::Download);
            }
            files.push(DownloadedFile {
                path: path.to_string_lossy().into_owned(),
                bytes: metadata.len(),
            });
        }
        Ok(ActionOutput { files })
    }
}

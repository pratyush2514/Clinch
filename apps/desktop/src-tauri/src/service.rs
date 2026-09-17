#![deny(unsafe_code)]
use browser_driver::{LaunchOptions, ManagedBrowser};
use orchestration_engine::{Engine, EngineError, InvoiceRequest, Task, TaskEvent, TaskId};
use serde::Serialize;
use session_sync::{FallbackReason, PreparedSync, SyncRequest};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{OnceCell, Semaphore};

#[derive(Debug, Serialize)]
#[serde(tag = "code", content = "message", rename_all = "snake_case")]
pub enum AppError {
    InvalidInput(&'static str),
    Busy,
    StorageUnavailable,
    BrowserUnavailable,
    Internal,
    StaleApproval,
    SessionRequired,
    WorkflowFailed,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatus {
    ready: bool,
    cookie_import_supported: bool,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionStatus {
    CookiesImported { count: usize },
    ManualLogin { reason: Option<FallbackReason> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalPreview {
    id: u64,
    title: &'static str,
    description: &'static str,
}

pub struct AppService {
    data: PathBuf,
    home: PathBuf,
    db: OnceCell<sqlx::SqlitePool>,
    browser: Mutex<Option<Arc<ManagedBrowser>>>,
    operation: Semaphore,
    approval: Mutex<Option<u64>>,
    next_approval: AtomicU64,
    engine: OnceCell<Engine>,
    session_origin: Mutex<Option<url::Url>>,
}

impl AppService {
    pub fn new(data: PathBuf, home: PathBuf) -> Self {
        Self {
            data,
            home,
            db: OnceCell::new(),
            browser: Mutex::new(None),
            operation: Semaphore::new(1),
            approval: Mutex::new(None),
            next_approval: AtomicU64::new(1),
            engine: OnceCell::new(),
            session_origin: Mutex::new(None),
        }
    }

    async fn database(&self) -> Result<&sqlx::SqlitePool, AppError> {
        self.db
            .get_or_try_init(|| async {
                playbook_store::initialize(&self.data.join("clinch.db"))
                    .await
                    .map_err(|_| AppError::StorageUnavailable)
            })
            .await
    }

    pub async fn initialize(&self) -> Result<StorageStatus, AppError> {
        self.engine().await?;
        Ok(StorageStatus {
            ready: true,
            cookie_import_supported: cfg!(target_os = "macos"),
        })
    }

    async fn engine(&self) -> Result<&Engine, AppError> {
        self.engine
            .get_or_try_init(|| async {
                let engine = Engine::new(self.database().await?.clone())
                    .await
                    .map_err(|error| engine_error(&error))?;
                engine
                    .recover()
                    .await
                    .map_err(|error| engine_error(&error))?;
                Ok(engine)
            })
            .await
    }

    pub async fn harvest(
        &self,
        request: &InvoiceRequest,
        emit: impl FnMut(TaskEvent) + Send,
    ) -> Result<Task, AppError> {
        session_sync::validate_portal(request.portal_url.as_str())
            .map_err(|_| AppError::InvalidInput("Enter a valid HTTPS billing page URL."))?;
        if request.portal_url.query().is_some() || request.portal_url.fragment().is_some() {
            return Err(AppError::InvalidInput(
                "Use a stable billing page URL without a query or fragment.",
            ));
        }
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let connected = self
            .session_origin
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone();
        if connected.is_none_or(|url| url.origin() != request.portal_url.origin()) {
            return Err(AppError::SessionRequired);
        }
        let mode = Engine::run_mode(request, &self.data)
            .await
            .map_err(|error| engine_error(&error))?;
        let browser = self
            .browser(mode == orchestration_engine::RunMode::Replay)
            .await?;
        self.engine()
            .await?
            .harvest(request, &browser, &self.data, emit)
            .await
            .map_err(|error| engine_error(&error))
    }

    pub async fn decide_task(
        &self,
        id: TaskId,
        index: usize,
        approved: bool,
    ) -> Result<(), AppError> {
        self.engine()
            .await?
            .decide(id, index, approved)
            .map_err(|_| AppError::StaleApproval)
    }

    pub async fn viewport(&self) -> Result<browser_driver::Viewport, AppError> {
        let browser = self
            .browser
            .lock()
            .map_err(|_| AppError::Internal)?
            .clone()
            .ok_or(AppError::SessionRequired)?;
        browser
            .viewport()
            .await
            .map_err(|_| AppError::BrowserUnavailable)
    }

    pub async fn task(&self, id: TaskId) -> Result<Task, AppError> {
        self.engine()
            .await?
            .load(id)
            .await
            .map_err(|error| engine_error(&error))
    }

    pub async fn downloaded_file(&self, id: TaskId, index: usize) -> Result<PathBuf, AppError> {
        let task = self.task(id).await?;
        if task.state != orchestration_engine::TaskState::Completed {
            return Err(AppError::WorkflowFailed);
        }
        let file = task
            .plan
            .steps
            .iter()
            .flat_map(|step| &step.output.files)
            .nth(index)
            .ok_or(AppError::InvalidInput("Unknown downloaded file."))?;
        let root = tokio::fs::canonicalize(self.data.join("downloads").join(id.0.to_string()))
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        let path = tokio::fs::canonicalize(&file.path)
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        if !path.starts_with(root)
            || !tokio::fs::metadata(&path)
                .await
                .map_err(|_| AppError::StorageUnavailable)?
                .is_file()
        {
            return Err(AppError::InvalidInput("Invalid downloaded file."));
        }
        Ok(path)
    }

    async fn browser(&self, headless: bool) -> Result<Arc<ManagedBrowser>, AppError> {
        let existing = self.browser.lock().map_err(|_| AppError::Internal)?.clone();
        let executable =
            std::env::var_os("CLINCH_CHROMIUM_PATH").map_or_else(default_chromium, PathBuf::from);
        let profile = self.data.join("browser-profile");
        let options = LaunchOptions { headless };
        let browser = if let Some(browser) = existing {
            if browser.is_headless() == headless {
                return Ok(browser);
            }
            let restarted = browser.restart(&executable, &profile, options).await;
            if let Ok(browser) = restarted {
                browser
            } else {
                *self.browser.lock().map_err(|_| AppError::Internal)? = None;
                *self.session_origin.lock().map_err(|_| AppError::Internal)? = None;
                return Err(AppError::BrowserUnavailable);
            }
        } else {
            ManagedBrowser::launch_with_options(&executable, &profile, options)
                .await
                .map_err(|_| AppError::BrowserUnavailable)?
        };
        let browser = Arc::new(browser);
        *self.browser.lock().map_err(|_| AppError::Internal)? = Some(browser.clone());
        Ok(browser)
    }

    async fn record(&self, outcome: &str) -> Result<(), AppError> {
        sqlx::query("INSERT INTO session_events (outcome) VALUES (?)")
            .bind(outcome)
            .execute(self.database().await?)
            .await
            .map_err(|_| AppError::StorageUnavailable)?;
        Ok(())
    }

    pub async fn sync(&self, request: SyncRequest) -> Result<SessionStatus, AppError> {
        let request = request.validate().map_err(|_| {
            AppError::InvalidInput(
                "Consent, a valid HTTPS portal, and a valid profile are required.",
            )
        })?;
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        self.database().await?;
        let prepared = session_sync::prepare(&request, &self.home).await;
        let browser = self.browser(false).await?;
        let status = match prepared {
            PreparedSync::Cookies(cookies) => {
                if browser.inject(&cookies).await.is_ok() {
                    self.record("cookies_imported_unverified").await?;
                    SessionStatus::CookiesImported {
                        count: cookies.len(),
                    }
                } else {
                    self.record("manual_login_injection_failed").await?;
                    SessionStatus::ManualLogin {
                        reason: Some(FallbackReason::InjectionFailed),
                    }
                }
            }
            PreparedSync::ManualLogin(reason) => {
                self.record("manual_login_extraction_unavailable").await?;
                SessionStatus::ManualLogin {
                    reason: Some(reason),
                }
            }
        };
        browser
            .navigate(request.portal())
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        *self.session_origin.lock().map_err(|_| AppError::Internal)? =
            Some(request.portal().clone());
        Ok(status)
    }

    pub async fn manual_login(&self, portal_url: &str) -> Result<SessionStatus, AppError> {
        // Validation is shared with sync, but manual login never reads source cookies or Keychain.
        let portal = session_sync::validate_portal(portal_url)
            .map_err(|_| AppError::InvalidInput("Enter a valid HTTPS portal URL."))?;
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        let browser = self.browser(false).await?;
        browser
            .navigate(&portal)
            .await
            .map_err(|_| AppError::BrowserUnavailable)?;
        self.record("manual_login_requested").await?;
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = Some(portal);
        Ok(SessionStatus::ManualLogin { reason: None })
    }

    pub async fn close_browser(&self) -> Result<(), AppError> {
        let _permit = self.operation.try_acquire().map_err(|_| AppError::Busy)?;
        *self.session_origin.lock().map_err(|_| AppError::Internal)? = None;
        let browser = self.browser.lock().map_err(|_| AppError::Internal)?.take();
        if let Some(browser) = browser {
            browser
                .shutdown()
                .await
                .map_err(|_| AppError::BrowserUnavailable)?;
        }
        Ok(())
    }

    pub fn terminate_browser(&self) {
        if let Ok(guard) = self.browser.lock()
            && let Some(browser) = guard.as_ref()
        {
            browser.terminate();
        }
    }

    pub fn preview_approval(&self) -> Result<ApprovalPreview, AppError> {
        let id = self.next_approval.fetch_add(1, Ordering::Relaxed);
        let mut pending = self.approval.lock().map_err(|_| AppError::Internal)?;
        if pending.is_some() {
            return Err(AppError::Busy);
        }
        *pending = Some(id);
        Ok(ApprovalPreview {
            id,
            title: "Sentinel Gate · IPC check",
            description: "Approve this dummy action to verify the Rust-to-React round trip. No form is submitted, no payment is made, and no file is downloaded.",
        })
    }

    pub fn resolve_approval(&self, id: u64, approved: bool) -> Result<bool, AppError> {
        let mut pending = self.approval.lock().map_err(|_| AppError::Internal)?;
        if *pending != Some(id) {
            return Err(AppError::StaleApproval);
        }
        *pending = None;
        Ok(approved)
    }
}

fn engine_error(error: &EngineError) -> AppError {
    match error {
        EngineError::Invalid => AppError::InvalidInput(
            "Check the workflow name, portal, and selectors. Saved workflows must use the same billing URL.",
        ),
        EngineError::Database(_) | EngineError::Io(_) => AppError::StorageUnavailable,
        _ => AppError::WorkflowFailed,
    }
}

fn default_chromium() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    }
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        PathBuf::from("google-chrome")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn harvester_requires_a_connected_session_and_exclusive_browser_access()
    -> Result<(), Box<dyn std::error::Error>> {
        let service = AppService::new(PathBuf::new(), PathBuf::new());
        let request = InvoiceRequest {
            workflow: "bills".into(),
            portal_url: url::Url::parse("https://example.com/billing")?,
            billing_selector: None,
            invoice_selector: "a.invoice".into(),
        };
        assert!(matches!(
            service.harvest(&request, |_| {}).await,
            Err(AppError::SessionRequired)
        ));
        let _permit = service.operation.try_acquire()?;
        assert!(matches!(
            service.harvest(&request, |_| {}).await,
            Err(AppError::Busy)
        ));
        assert!(matches!(service.close_browser().await, Err(AppError::Busy)));
        Ok(())
    }
    #[tokio::test]
    async fn file_actions_only_resolve_completed_task_downloads()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let service = AppService::new(dir.path().to_owned(), dir.path().to_owned());
        service.initialize().await.map_err(|_| "initialize")?;
        let root = dir.path().join("downloads/1");
        tokio::fs::create_dir_all(&root).await?;
        let file = root.join("download.dat");
        let outside = dir.path().join("outside.dat");
        tokio::fs::write(&file, b"data").await?;
        tokio::fs::write(&outside, b"data").await?;
        let request = InvoiceRequest {
            workflow: "fixture".into(),
            portal_url: url::Url::parse("https://example.com")?,
            billing_selector: None,
            invoice_selector: "a.invoice".into(),
        };
        let mut plan = request.plan()?;
        plan.steps[1]
            .output
            .files
            .push(browser_driver::DownloadedFile {
                path: file.to_string_lossy().into_owned(),
                bytes: 4,
            });
        let mut task = Task {
            id: TaskId(1),
            revision: 0,
            workflow: "fixture".into(),
            mode: orchestration_engine::RunMode::Record,
            state: orchestration_engine::TaskState::Completed,
            plan,
            repair: None,
            failure: None,
            elapsed_ms: 1,
        };
        let pool = service.database().await.map_err(|_| "database")?;
        for case in 0..4 {
            match case {
                1 => task.state = orchestration_engine::TaskState::Running,
                2 => {
                    task.state = orchestration_engine::TaskState::Completed;
                    task.plan.steps[1].output.files[0].path =
                        outside.to_string_lossy().into_owned();
                }
                3 => {
                    task.plan.steps[1].output.files[0].path =
                        root.join("missing.dat").to_string_lossy().into_owned();
                }
                _ => {}
            }
            sqlx::query("INSERT OR REPLACE INTO tasks(id, revision, snapshot) VALUES(1, 0, ?)")
                .bind(serde_json::to_string(&task)?)
                .execute(pool)
                .await?;
            let result = service.downloaded_file(TaskId(1), 0).await;
            assert_eq!(result.is_ok(), case == 0);
            assert!(service.downloaded_file(TaskId(1), 10).await.is_err());
        }
        pool.close().await;
        Ok(())
    }

    #[test]
    fn dummy_approval_is_explicit_and_single_use() -> Result<(), AppError> {
        let service = AppService::new(PathBuf::new(), PathBuf::new());
        assert!(matches!(
            service.resolve_approval(1, true),
            Err(AppError::StaleApproval)
        ));
        let preview = service.preview_approval()?;
        assert!(matches!(service.preview_approval(), Err(AppError::Busy)));
        assert!(!service.resolve_approval(preview.id, false)?);
        assert!(matches!(
            service.resolve_approval(preview.id, true),
            Err(AppError::StaleApproval)
        ));
        let preview = service.preview_approval()?;
        assert!(service.resolve_approval(preview.id, true)?);
        Ok(())
    }
}

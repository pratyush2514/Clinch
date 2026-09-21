#![deny(unsafe_code)]
//! Task → Plan → Step execution with durable boundaries and typed progress.
mod entity_resolver;
mod intent_resolver;
mod portal_routes;
mod route_proposer;
mod runner;
mod store;
mod task;
mod url_policy;
use browser_driver::{Action, Highlight, ManagedBrowser, WaitCondition};
pub use entity_resolver::{AccountDirectory, DirectoryError, RepoRef, resolve_repo_entity};
pub use intent_resolver::{
    CommandMatch, ExtractedVariable, ParsedIntent, VariableExtraction, VariableKind,
    decompose_command, ephemeral_name, extract_dynamic_variables, extract_identifier,
    parse_intent_structured, resolve_command,
};
pub use macro_engine::{Macro, MacroError, MacroStep, Recorder, ReplayError};
pub use portal_routes::portal_route;
pub use route_proposer::{
    LlmUrlProposer, ResolutionContext, ResolvedRoute, RouteSource, resolve_entry_url,
};
pub use runner::{
    SequenceEvent, SequenceOutcome, SequencePhase, SequenceStatus, StepError, StepOutcome,
    execute_step, run_playbook_sequence,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::{path::Path, time::Instant};
pub use task::{FailureReason, Plan, RunMode, Step, StepState, Task, TaskId, TaskState};
use url::Url;
pub use url_policy::{UrlRejected, validate_proposed_url};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("Macro replay requires a headless browser")]
    HeadlessRequired,
    #[error("Task transition is not valid")]
    Transition,
    #[error("Task or workflow input is invalid")]
    Invalid,
    #[error("Checkpoint conflicts with another writer")]
    Conflict,
    #[error("Checkpoint database failed")]
    Database(#[from] sqlx::Error),
    #[error("Checkpoint data is invalid")]
    Json(#[from] serde_json::Error),
    #[error("Macro operation failed")]
    Macro(#[from] MacroError),
    #[error("Workflow storage failed")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRequest {
    pub workflow: String,
    pub portal_url: Url,
    pub link_selector: Option<String>,
    pub download_selector: String,
}

impl TaskRequest {
    fn validate_identity(&self) -> Result<(), EngineError> {
        if self.workflow.is_empty()
            || self.workflow.len() > 64
            || !self
                .workflow
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(EngineError::Invalid);
        }
        Ok(())
    }

    /// # Errors
    /// Rejects unsafe workflow paths, invalid URLs, and malformed plans.
    pub fn plan(&self) -> Result<Plan, EngineError> {
        self.validate_identity()?;
        let wait = |selector: &str| {
            Some(WaitCondition {
                selector: selector.into(),
                timeout_ms: 5000,
            })
        };
        let first_selector = self
            .link_selector
            .as_deref()
            .unwrap_or(&self.download_selector);
        let mut steps = vec![MacroStep {
            action: Action::Navigate {
                url: self.portal_url.clone(),
            },
            wait: wait(first_selector),
        }];
        if let Some(selector) = &self.link_selector {
            steps.push(MacroStep {
                action: Action::Click {
                    selector: selector.clone(),
                },
                wait: wait(&self.download_selector),
            });
        }
        steps.push(MacroStep {
            action: Action::DownloadLinks {
                selector: self.download_selector.clone(),
            },
            wait: None,
        });
        Plan::new(Macro {
            version: 1,
            last_healed_at: None,
            healing_history: Vec::new(),
            origin: self.portal_url.clone(),
            steps,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEvent {
    pub task: Task,
    pub highlight: Option<Highlight>,
    pub approval: Option<GateRequest>,
}

impl TaskEvent {
    fn progress(task: &Task) -> Self {
        Self {
            task: task.clone(),
            highlight: None,
            approval: None,
        }
    }
}

/// Service callers serialize access to the single managed browser. SQL revisions additionally
/// prevent stale checkpoint writers; no database transaction spans browser I/O.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateRequest {
    pub task_id: TaskId,
    pub step_index: usize,
    pub action: Action,
}

struct PendingGate {
    request: GateRequest,
    reply: tokio::sync::oneshot::Sender<bool>,
}

pub struct Engine {
    pool: SqlitePool,
    gate: std::sync::Mutex<Option<PendingGate>>,
}

impl Engine {
    /// # Errors
    /// Returns database errors, including refusal of a non-WAL database.
    pub async fn new(pool: SqlitePool) -> Result<Self, EngineError> {
        store::initialize(&pool).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS sentinel_decisions (id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL, step_index INTEGER NOT NULL, action_json TEXT NOT NULL, approved INTEGER NOT NULL, decided_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)").execute(&pool).await?;
        Ok(Self {
            pool,
            gate: std::sync::Mutex::new(None),
        })
    }

    /// Resolve exactly one pending local decision. Stale and duplicate decisions fail closed.
    /// # Errors
    /// Rejects mismatched task/step identities and consumed requests.
    pub fn decide(
        &self,
        task_id: TaskId,
        step_index: usize,
        approved: bool,
    ) -> Result<(), EngineError> {
        let mut gate = self.gate.lock().map_err(|_| EngineError::Transition)?;
        if gate
            .as_ref()
            .is_none_or(|g| g.request.task_id != task_id || g.request.step_index != step_index)
        {
            return Err(EngineError::Transition);
        }
        gate.take()
            .ok_or(EngineError::Transition)?
            .reply
            .send(approved)
            .map_err(|_| EngineError::Transition)
    }

    async fn consent(
        &self,
        task: &Task,
        index: usize,
        action: Action,
        emit: &std::sync::Mutex<&mut (impl FnMut(TaskEvent) + Send)>,
    ) -> bool {
        // Navigation and file downloads retain their existing contract; interactive actions require consent.
        if matches!(
            action,
            Action::Navigate { .. } | Action::DownloadLinks { .. }
        ) {
            return true;
        }
        let request = GateRequest {
            task_id: task.id,
            step_index: index,
            action,
        };
        let (reply, receive) = tokio::sync::oneshot::channel();
        if let Ok(mut gate) = self.gate.lock() {
            if gate.is_some() {
                return false;
            }
            *gate = Some(PendingGate {
                request: request.clone(),
                reply,
            });
        } else {
            return false;
        }
        if let Ok(mut emit) = emit.lock() {
            emit(TaskEvent {
                task: task.clone(),
                highlight: None,
                approval: Some(request.clone()),
            });
        }
        let approved = tokio::time::timeout(std::time::Duration::from_mins(5), receive)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false);
        if let Ok(mut gate) = self.gate.lock() {
            *gate = None;
        }
        let Ok(action) = serde_json::to_string(&request.action) else {
            return false;
        };
        let Ok(index) = i64::try_from(index) else {
            return false;
        };
        let recorded = sqlx::query("INSERT INTO sentinel_decisions (task_id, step_index, action_json, approved) VALUES (?, ?, ?, ?)")
            .bind(task.id.0).bind(index).bind(action).bind(approved).execute(&self.pool).await.is_ok();
        approved && recorded
    }

    /// Load the last durable task state without executing or retrying any action.
    /// # Errors
    /// Returns database, decoding, or missing-task errors.
    pub async fn load(&self, id: TaskId) -> Result<Task, EngineError> {
        store::load(&self.pool, id).await
    }

    /// Recover unfinished runs at application startup, before admitting new browser operations.
    /// # Errors
    /// Returns storage errors or a concurrent-writer conflict.
    pub async fn recover(&self) -> Result<(), EngineError> {
        for mut task in store::unfinished(&self.pool).await? {
            task.interrupt()?;
            store::checkpoint(&self.pool, &mut task).await?;
        }
        Ok(())
    }

    async fn prepare(
        request: &TaskRequest,
        root: &Path,
    ) -> Result<(Plan, RunMode, std::path::PathBuf), EngineError> {
        request.validate_identity()?;
        let path = root
            .join("macros")
            .join(format!("{}.json", request.workflow));
        let (plan, mode) = match Macro::load(&path).await {
            Ok(recorded) => {
                if recorded.origin != request.portal_url {
                    return Err(EngineError::Invalid);
                }
                (Plan::new(recorded)?, RunMode::Replay)
            }
            Err(MacroError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                (request.plan()?, RunMode::Record)
            }
            Err(error) => return Err(error.into()),
        };
        Ok((plan, mode, path))
    }

    /// Determine the validated workflow mode before choosing a browser configuration.
    /// # Errors
    /// Rejects invalid requests and corrupt or mismatched saved macros.
    pub async fn run_mode(request: &TaskRequest, root: &Path) -> Result<RunMode, EngineError> {
        Ok(Self::prepare(request, root).await?.1)
    }

    /// Plan/record on first run, load/replay on subsequent runs. A corrupt macro fails closed.
    /// # Errors
    /// Returns invalid input, macro publication, or durable checkpoint failures.
    pub async fn run_task(
        &self,
        request: &TaskRequest,
        browser: &ManagedBrowser,
        root: &Path,
        mut emit: impl FnMut(TaskEvent) + Send,
    ) -> Result<Task, EngineError> {
        let (plan, mode, path) = Self::prepare(request, root).await?;
        if mode == RunMode::Replay && !browser.is_headless() {
            return Err(EngineError::HeadlessRequired);
        }
        let mut task = Task::new(request.workflow.clone(), plan, mode);
        store::create(&self.pool, &mut task).await?;
        emit(TaskEvent::progress(&task));
        let started = Instant::now();
        let output = root.join("downloads").join(task.id.0.to_string());
        let mut recorder = Recorder::new(task.plan.recording.origin.clone());
        for index in 0..task.plan.steps.len() {
            task.start_step(index)?;
            store::checkpoint(&self.pool, &mut task).await?;
            emit(TaskEvent::progress(&task));
            let step_started = Instant::now();
            let snapshot = task.clone();
            let events = std::sync::Mutex::new(&mut emit);
            let mut recording = task.plan.recording.clone();
            let result = macro_engine::HealingReplay {
                browser,
                provider: &llm_provider::LocalProvider,
                path: &path,
                output: &output,
            }
            .step(
                &mut recording,
                index,
                |highlight| {
                    if let Ok(mut emit) = events.lock() {
                        emit(TaskEvent {
                            task: snapshot.clone(),
                            highlight: Some(highlight),
                            approval: None,
                        });
                    }
                },
                |action| self.consent(&snapshot, index, action, &events),
            )
            .await;
            task.plan.recording = recording;
            let step = &task.plan.recording.steps[index];
            let elapsed = milliseconds(step_started.elapsed());
            match result {
                Ok(result) => {
                    if mode == RunMode::Record {
                        recorder.record_completed(step)?;
                    }
                    task.complete_step(index, result, elapsed)?;
                }
                Err(ReplayError::Repair(repair)) => task.repair_step(index, repair, elapsed)?,
                Err(ReplayError::ApprovalRequired) => {
                    task.fail_step(index, elapsed, FailureReason::ApprovalRequired)?;
                }
                Err(ReplayError::Publication(_)) => {
                    task.fail_step(index, elapsed, FailureReason::MacroPublication)?;
                }
                Err(ReplayError::Browser(error)) => {
                    task.fail_step(index, elapsed, FailureReason::from(&error))?;
                }
            }
            task.elapsed_ms = milliseconds(started.elapsed());
            store::checkpoint(&self.pool, &mut task).await?;
            emit(TaskEvent::progress(&task));
            if task.state != TaskState::Running {
                return Ok(task);
            }
        }
        if mode == RunMode::Record {
            // Steps remain durable if publication fails; never claim a replayable completed run.
            if let Err(error) = recorder.finish()?.save(&path).await {
                task.fail_publication()?;
                store::checkpoint(&self.pool, &mut task).await?;
                emit(TaskEvent::progress(&task));
                return Err(error.into());
            }
        }
        task.finish()?;
        task.elapsed_ms = milliseconds(started.elapsed());
        store::checkpoint(&self.pool, &mut task).await?;
        emit(TaskEvent::progress(&task));
        Ok(task)
    }
}

fn milliseconds(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn gate_blocks_and_consumes_only_matching_decisions()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let pool = playbook_store::initialize(&dir.path().join("gate.db")).await?;
        let engine = Engine::new(pool.clone()).await?;
        let request = TaskRequest {
            workflow: "gate".into(),
            portal_url: Url::parse("https://example.com")?,
            link_selector: None,
            download_selector: "a.report".into(),
        };
        let mut task = Task::new("gate".into(), request.plan()?, RunMode::Record);
        task.id = TaskId(42);
        for approved in [false, true] {
            let mut emit = |event: TaskEvent| {
                let Some(gate) = event.approval.as_ref() else {
                    panic!("Pending gate missing");
                };
                assert!(engine.decide(TaskId(41), gate.step_index, true).is_err());
            };
            let events = std::sync::Mutex::new(&mut emit);
            let consent = engine.consent(
                &task,
                0,
                Action::Submit {
                    selector: "form".into(),
                },
                &events,
            );
            tokio::pin!(consent);
            tokio::select! {
                biased;
                _ = &mut consent => panic!("Gate resolved without a decision"),
                () = tokio::task::yield_now() => {}
            }
            engine.decide(task.id, 0, approved)?;
            assert!(engine.decide(task.id, 0, true).is_err());
            assert_eq!(consent.await, approved);
        }
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sentinel_decisions WHERE task_id=42")
                .fetch_one(&pool)
                .await?;
        assert_eq!(count.0, 2);
        Ok(())
    }

    #[test]
    fn task_plans_reject_paths_and_invalid_selectors() -> Result<(), Box<dyn std::error::Error>> {
        let mut request = TaskRequest {
            workflow: "reports".into(),
            portal_url: Url::parse("https://example.com/files")?,
            link_selector: None,
            download_selector: "a.report".into(),
        };
        assert_eq!(request.plan()?.steps.len(), 2);
        for workflow in ["../reports", "C:\\reports", "reports/name", "", "."] {
            request.workflow = workflow.into();
            assert!(request.plan().is_err());
        }
        request.workflow = "reports".into();
        request.link_selector = Some(String::new());
        assert!(request.plan().is_err());
        request.link_selector = None;
        request.portal_url = Url::parse("https://user:secret@example.com/")?;
        assert!(request.plan().is_err());
        Ok(())
    }
}

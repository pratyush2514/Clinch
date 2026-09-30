#![deny(unsafe_code)]
//! Versioned native-CDP replay with bounded, localized selector healing.
pub mod executor;
pub mod navigator;
pub mod semantic;
pub use browser_driver::{
    Action, ActionOutput, BrowserError, Highlight, ManagedBrowser, SelectorIssue, WaitCondition,
};
pub use executor::{
    BatchDriver, CLICK_FAILED_REASON, ChromeActionBrowser, ClickedControl, DriftDetail,
    ExecuteOutcome, FOLLOW_POLL_MS, FOLLOW_TIMEOUT_MS, FastReplayMetrics, IdentityMenuPolicy,
    IntentError, IntentOutcome, MAX_BATCH_CLICKS, MatchTier, MenuBrowser, MenuOpenBaseline,
    ModelCandidates, ModelEscalation, OpenMenuOutcome, PageGoalOutcome, RESOLVE_COST_USD,
    ResolveOutcome, ResolvedIntent, SETTLE_POLL_MS, SETTLE_TIMEOUT_MS, SemanticIntent,
    SettingsBrowser, VerbKind, VerbSpec, VerifierKind, chrome_action_miss_diagnostic,
    click_batch_with, click_point_in_viewport, ensure_at_entry_url, entry_url_mismatched,
    execute_batch, execute_intent, grounding_diagnostic, header_strip_candidates,
    label_names_profile, model_candidates, open_identity_menu, page_goal_diagnostic,
    path_names_account, pick_rightmost, preview_batch, pursue_account_home, pursue_chrome_action,
    pursue_chrome_action_with_vision, pursue_page_goal, pursue_verb_goal, pursue_with_model,
    rank_menu_candidates, resolve_batch, resolve_fast, resolve_intent, resolve_with_drift,
    same_site_host, select_already_open_menu_target, select_already_open_menu_target_tiered,
    select_menu_button, select_page_control, select_revealed_action, select_revealed_action_tiered,
    semantic_opener_winner, semantic_revealed_target, settle_probe_text, strong_openers,
    tried_label, username_from_href, username_from_menu_text, validate_revealed_href, verb_specs,
    verify_account_landing, verify_notifications_surface, verify_noun_landing, verify_verb,
    visual_target_description, wait_for_settled_candidates,
};
pub use navigator::{
    LOOP_NUDGE_REPEATS, LOOP_WINDOW, LoopDetector, MAX_BATCH_ACTIONS, MAX_DONE_REJECTIONS,
    MAX_NAVIGATOR_ELEMENTS, MAX_SCREENSHOTS_PER_PASS, MAX_STALE_REREADS, NavigatorTurn,
    NormalizedAction, PageAction, PageNavigator, PositionZone, SPARSE_TREE_ELEMENTS,
    STAGNATION_NUDGE_TURNS, ScreenshotReason, VisualLocation, page_fingerprint, screenshot_reason,
    visual_point_to_pixels, zone_for,
};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path};
use url::Url;

pub const VERSION: u32 = 1;
const MAX_BYTES: u64 = 1_048_576;
/// Upper bound on one selector-repair provider call: healing must stay a
/// bounded detour, never stall a run on a wedged model.
const REPAIR_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, thiserror::Error)]
pub enum MacroError {
    #[error("Unsupported macro version")]
    Version,
    #[error("Invalid or incomplete macro")]
    Invalid,
    #[error("Macro JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("Macro storage failed")]
    Io(#[from] std::io::Error),
    #[error("Macro writer failed")]
    Writer,
    #[error("Selector repair failed")]
    Provider(#[from] llm_provider::ProviderError),
    #[error("CDP action failed")]
    Browser(#[from] BrowserError),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MacroStep {
    pub action: Action,
    pub wait: Option<WaitCondition>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Macro {
    pub version: u32,
    #[serde(default)]
    pub last_healed_at: Option<u64>,
    #[serde(default)]
    pub healing_history: Vec<HealingEvent>,
    pub origin: Url,
    pub steps: Vec<MacroStep>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HealingEvent {
    pub at: u64,
    pub step_index: usize,
    pub stage: RepairStage,
    pub old_selector: String,
    pub new_selector: String,
}

impl Macro {
    /// # Errors
    /// Rejects unknown versions, oversized plans, invalid inputs, and cross-origin actions.
    pub fn validate(&self) -> Result<(), MacroError> {
        if self.version != VERSION {
            return Err(MacroError::Version);
        }
        if self.steps.is_empty()
            || self.steps.len() > 100
            || self.healing_history.len() > 1000
            || !matches!(self.origin.scheme(), "https" | "http")
            || self.origin.host_str().is_none()
            || !self.origin.username().is_empty()
            || self.origin.password().is_some()
        {
            return Err(MacroError::Invalid);
        }
        for step in &self.steps {
            step.action.validate(&self.origin)?;
            if let Some(wait) = &step.wait
                && (wait.selector.trim().is_empty()
                    || wait.selector.len() > 2048
                    || wait.timeout_ms == 0
                    || wait.timeout_ms > 30_000)
            {
                return Err(MacroError::Invalid);
            }
        }
        Ok(())
    }

    /// Atomic publication: failed recordings never replace a usable macro.
    /// # Errors
    /// Returns validation, serialization, or filesystem errors.
    pub async fn save(&self, path: &Path) -> Result<(), MacroError> {
        self.validate()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(MacroError::Invalid);
        }
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let parent = path.parent().ok_or(MacroError::Invalid)?;
            std::fs::create_dir_all(parent)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            temp.write_all(&bytes)?;
            temp.as_file().sync_all()?;
            temp.persist(path)
                .map_err(|error| MacroError::Io(error.error))?;
            Ok(())
        })
        .await
        .map_err(|_| MacroError::Writer)?
    }

    /// # Errors
    /// Rejects missing, malformed, oversized, or unsupported macro files.
    pub async fn load(path: &Path) -> Result<Self, MacroError> {
        use tokio::io::AsyncReadExt;
        let file = tokio::fs::File::open(path).await?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).await?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(MacroError::Invalid);
        }
        let result: Self = serde_json::from_slice(&bytes)?;
        result.validate()?;
        Ok(result)
    }
}

/// Only successfully completed actions are admitted to a recording.
pub struct Recorder {
    origin: Url,
    steps: Vec<MacroStep>,
}

impl Recorder {
    #[must_use]
    pub fn new(origin: Url) -> Self {
        Self {
            origin,
            steps: Vec::new(),
        }
    }

    /// Called after both the CDP action and its wait condition complete.
    /// # Errors
    /// Rejects invalid actions and oversized recordings.
    pub fn record_completed(&mut self, step: &MacroStep) -> Result<(), MacroError> {
        step.action.validate(&self.origin)?;
        if self.steps.len() >= 100 {
            return Err(MacroError::Invalid);
        }
        self.steps.push(step.clone());
        Ok(())
    }

    /// # Errors
    /// Rejects empty or invalid recordings.
    pub fn finish(self) -> Result<Macro, MacroError> {
        let recording = Macro {
            version: VERSION,
            last_healed_at: None,
            healing_history: Vec::new(),
            origin: self.origin,
            steps: self.steps,
        };
        recording.validate()?;
        Ok(recording)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepairStage {
    Target,
    Wait,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepairRequest {
    pub step_index: usize,
    pub selector: String,
    pub stage: RepairStage,
    pub issue: SelectorIssue,
}

/// A post-action wait failure must not cause the action to be repeated automatically.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("A selector needs targeted repair")]
    Repair(RepairRequest),
    #[error("CDP execution failed")]
    Browser(#[from] BrowserError),
    #[error("Repair could not be published")]
    Publication(#[from] MacroError),
    #[error("Local approval was not granted")]
    ApprovalRequired,
}

fn failure(error: BrowserError, index: usize, selector: &str, stage: RepairStage) -> ReplayError {
    match error {
        BrowserError::Selector(issue) => ReplayError::Repair(RepairRequest {
            step_index: index,
            selector: selector.into(),
            stage,
            issue,
        }),
        error => ReplayError::Browser(error),
    }
}

/// Resolve and stream the highlight before dispatch, then execute and check the postcondition.
/// # Errors
/// Returns a narrow repair request or a typed browser failure. Never retries side effects.
pub async fn replay_step(
    browser: &ManagedBrowser,
    origin: &Url,
    index: usize,
    step: &MacroStep,
    output: &Path,
    mut highlight: impl FnMut(Highlight),
) -> Result<ActionOutput, ReplayError> {
    step.action.validate(origin)?;
    if let Some(selector) = step.action.selector() {
        browser.check_origin(origin).await?;
        let target = browser
            .resolve(
                selector,
                matches!(step.action, Action::DownloadLinks { .. }),
            )
            .await
            .map_err(|error| failure(error, index, selector, RepairStage::Target))?;
        highlight(target);
    }
    let result = browser
        .execute_action(&step.action, origin, output)
        .await
        .map_err(|error| {
            failure(
                error,
                index,
                step.action.selector().unwrap_or(""),
                RepairStage::Target,
            )
        })?;
    if let Some(wait) = &step.wait {
        browser
            .wait_for(wait, origin)
            .await
            .map_err(|error| failure(error, index, &wait.selector, RepairStage::Wait))?;
    }
    Ok(result)
}

/// One repair attempt per target/wait. Publication precedes execution; waits never replay actions.
pub struct HealingReplay<'a> {
    pub browser: &'a ManagedBrowser,
    pub provider: &'a dyn llm_provider::SelectorProvider,
    pub path: &'a Path,
    pub output: &'a Path,
}

impl HealingReplay<'_> {
    async fn repair(
        &self,
        recording: &mut Macro,
        request: RepairRequest,
    ) -> Result<(), ReplayError> {
        let region = self
            .browser
            .repair_region(&request.selector, &recording.origin)
            .await
            .map_err(|_| ReplayError::Repair(request.clone()))?;
        if region.html.len() > 12000 {
            return Err(ReplayError::Repair(request));
        }
        let context = llm_provider::RepairContext {
            selector: request.selector.clone(),
            html: region.html,
            bounds: region.bounds,
        };
        let candidate = tokio::time::timeout(
            std::time::Duration::from_secs(REPAIR_TIMEOUT_SECS),
            self.provider.repair(&context),
        )
        .await
        .map_err(|_| ReplayError::Repair(request.clone()))?
        .map_err(|_| ReplayError::Repair(request.clone()))?;
        if candidate.selector.trim().is_empty()
            || candidate.selector.len() > 2048
            || candidate.selector == request.selector
        {
            return Err(ReplayError::Repair(request));
        }
        self.browser.check_origin(&recording.origin).await?;
        let multiple = request.stage == RepairStage::Wait
            || matches!(
                recording.steps[request.step_index].action,
                Action::DownloadLinks { .. }
            );
        let target = self
            .browser
            .resolve(&candidate.selector, multiple)
            .await
            .map_err(|_| ReplayError::Repair(request.clone()))?;
        let [x, y, width, height] = context.bounds;
        if target.x < x
            || target.y < y
            || target.x + target.width > x + width + 1.0
            || target.y + target.height > y + height + 1.0
        {
            return Err(ReplayError::Repair(request));
        }
        let action = if request.stage == RepairStage::Wait {
            Action::Navigate {
                url: recording.origin.clone(),
            }
        } else {
            recording.steps[request.step_index].action.clone()
        };
        self.browser
            .validate_repair_target(
                &action,
                &candidate.selector,
                &region.scope,
                &recording.origin,
            )
            .await
            .map_err(|_| ReplayError::Repair(request.clone()))?;
        let mut patched = recording.clone();
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| BrowserError::InvalidAction)?
            .as_secs();
        patched.last_healed_at = Some(at);
        patched.healing_history.push(HealingEvent {
            at,
            step_index: request.step_index,
            stage: request.stage,
            old_selector: request.selector.clone(),
            new_selector: candidate.selector.clone(),
        });
        let step = &mut patched.steps[request.step_index];
        match request.stage {
            RepairStage::Wait => {
                step.wait
                    .as_mut()
                    .ok_or(BrowserError::InvalidAction)?
                    .selector = candidate.selector;
            }
            RepairStage::Target => match &mut step.action {
                Action::Submit { selector }
                | Action::Click { selector }
                | Action::Fill { selector, .. }
                | Action::DownloadLinks { selector } => *selector = candidate.selector,
                Action::Navigate { .. } => return Err(BrowserError::InvalidAction.into()),
            },
        }
        // A first-run plan is published only after every step completes.
        if self.path.try_exists().map_err(MacroError::Io)? {
            patched.save(self.path).await?;
        }
        *recording = patched;
        Ok(())
    }

    /// # Errors
    /// Bounded repair, browser, consent, and atomic publication failures stop execution.
    pub async fn step<F, Fut>(
        &self,
        recording: &mut Macro,
        index: usize,
        mut highlight: impl FnMut(Highlight),
        mut consent: F,
    ) -> Result<ActionOutput, ReplayError>
    where
        F: FnMut(Action) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        recording.validate()?;
        let step = recording
            .steps
            .get(index)
            .ok_or(BrowserError::InvalidAction)?;
        if let Some(selector) = step.action.selector() {
            self.browser.check_origin(&recording.origin).await?;
            if let Err(error) = self
                .browser
                .resolve(
                    selector,
                    matches!(step.action, Action::DownloadLinks { .. }),
                )
                .await
            {
                match failure(error, index, selector, RepairStage::Target) {
                    ReplayError::Repair(request) => self.repair(recording, request).await?,
                    error => return Err(error),
                }
            }
        }
        let step = &recording.steps[index];
        if let Some(selector) = step.action.selector() {
            highlight(
                self.browser
                    .resolve(
                        selector,
                        matches!(step.action, Action::DownloadLinks { .. }),
                    )
                    .await?,
            );
            if !consent(step.action.clone()).await {
                return Err(ReplayError::ApprovalRequired);
            }
            highlight(
                self.browser
                    .resolve(
                        selector,
                        matches!(step.action, Action::DownloadLinks { .. }),
                    )
                    .await?,
            );
        }
        let result = if let Action::Submit { selector } = &step.action {
            self.browser
                .submit_approved(selector, &recording.origin)
                .await?
        } else {
            self.browser
                .execute_action(&step.action, &recording.origin, self.output)
                .await?
        };
        if let Some(wait) = &step.wait
            && let Err(error) = self.browser.wait_for(wait, &recording.origin).await
        {
            match failure(error, index, &wait.selector, RepairStage::Wait) {
                ReplayError::Repair(request) => self.repair(recording, request).await?,
                error => return Err(error),
            }
            self.browser
                .wait_for(
                    recording.steps[index]
                        .wait
                        .as_ref()
                        .ok_or(BrowserError::InvalidAction)?,
                    &recording.origin,
                )
                .await?;
        }
        Ok(result)
    }
}

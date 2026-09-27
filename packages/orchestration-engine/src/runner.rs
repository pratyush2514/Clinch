#![deny(unsafe_code)]
//! Playbook step dispatch: one step, two execution paths.
//!
//! Legacy steps replay their recorded action exactly (v1 macro semantics,
//! including wait conditions). Semantic steps resolve `{role, label}` against
//! the live AX tree and click through the backend node — no selector is ever
//! constructed. Plural semantic steps (`is_plural`) act on every eligible
//! control instead of the top scorer, behind an approval that names those
//! controls first. Every path honors the same highlight-then-act ordering as
//! first-run execution, and all of them require explicit approval for
//! anything beyond navigation and file downloads.

use browser_driver::{Action, ActionOutput, AxElement, BrowserError, Highlight, ManagedBrowser};
use macro_engine::SemanticIntent;
use std::{future::Future, path::Path};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// Grounding failure carrying the engine's step-log diagnostic: target
    /// queries plus every evaluated candidate and its score.
    #[error("{0}")]
    NoMatch(String),
    #[error("Local approval was not granted")]
    ApprovalRequired,
    /// A plural step left its route mid-batch. Partial progress is reported
    /// rather than hidden, so the UI can say how many controls were acted on
    /// before the page moved. The diverged URL is deliberately absent: URLs
    /// can carry tokens, and this value reaches journals and error strings.
    #[error("Batch halted after {clicks_completed} click(s) at candidate {failed_candidate_index}")]
    BatchHalted {
        reason: &'static str,
        clicks_completed: usize,
        failed_candidate_index: usize,
    },
    #[error("A recorded selector needs targeted repair")]
    NeedsRepair(macro_engine::RepairRequest),
    #[error("CDP execution failed")]
    Browser(#[from] BrowserError),
    #[error("Step definition is invalid")]
    Invalid,
}

/// What an intent-consent request asks approval for: the intent itself plus,
/// for a plural step, every control the run resolved and is about to click.
///
/// Single-target steps carry an empty `candidates` list — there is one
/// target and the intent already describes it. Plural steps carry the real
/// resolved set, so the gate can name a count and itemize labels before any
/// CDP click lands. Bundling both into one value keeps a single consent seam
/// instead of a second parallel callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentApproval {
    pub intent: SemanticIntent,
    pub candidates: Vec<AxElement>,
}

impl IntentApproval {
    /// A single-target request: the intent speaks for itself.
    #[must_use]
    pub fn single(intent: SemanticIntent) -> Self {
        Self {
            intent,
            candidates: Vec::new(),
        }
    }

    /// A plural request carrying the resolved controls to be clicked.
    #[must_use]
    pub fn batch(intent: SemanticIntent, candidates: Vec<AxElement>) -> Self {
        Self { intent, candidates }
    }

    /// Whether this request covers a plural batch. Derived from the intent,
    /// not from candidate emptiness, so a resolved-but-empty set can never
    /// masquerade as a single-target approval.
    #[must_use]
    pub fn is_batch(&self) -> bool {
        self.intent.is_plural
    }
}

/// What one dispatched step produced: its output plus the last highlight the
/// UI consumed while it ran (`None` only when nothing was ever targeted).
#[derive(Debug)]
pub struct StepOutcome {
    pub output: ActionOutput,
    pub highlight: Option<Highlight>,
}

fn needs_consent(action: &Action) -> bool {
    matches!(
        action,
        Action::Click { .. } | Action::Fill { .. } | Action::Submit { .. }
    )
}

/// Map a macro-engine intent failure onto this layer's step error.
fn intent_failure(error: macro_engine::IntentError) -> StepError {
    match error {
        macro_engine::IntentError::NoMatch(reason) => StepError::NoMatch(reason),
        macro_engine::IntentError::Browser(error) => StepError::Browser(error),
    }
}

/// Dispatch one plural semantic step: resolve the batch read-only, ask for
/// consent naming those controls, then execute.
///
/// The ordering is the point. A single-target step can ask first because its
/// intent already describes the one thing it will do; a batch cannot, because
/// "click every matching control" is not something a person can evaluate
/// without knowing which controls those are. So this resolves first and
/// approves second — and an empty field fails closed before any gate opens,
/// since a run nobody can assess is worse than a run that stops.
///
/// # Errors
/// Returns [`StepError::NoMatch`] when nothing resolves, [`StepError`]
/// `ApprovalRequired` on denial, [`StepError::BatchHalted`] when the page left
/// the batch route mid-run, and [`StepError::Browser`] on CDP failure.
async fn execute_batch_step<F, H, Ha>(
    browser: &ManagedBrowser,
    origin: &Url,
    step_index: usize,
    intent: &SemanticIntent,
    highlight: &mut F,
    consent_intent: &mut H,
) -> Result<StepOutcome, StepError>
where
    F: FnMut(Highlight),
    H: FnMut(usize, IntentApproval) -> Ha,
    Ha: Future<Output = bool>,
{
    let candidates = macro_engine::preview_batch(browser, origin, intent)
        .await
        .map_err(intent_failure)?;
    if !consent_intent(
        step_index,
        IntentApproval::batch(intent.clone(), candidates),
    )
    .await
    {
        return Err(StepError::ApprovalRequired);
    }
    // Re-resolves internally, so a page that changed during the approval wait
    // is re-grounded instead of clicked through stale node handles. Same
    // contract the ad-hoc batch lane holds.
    match macro_engine::execute_batch(browser, origin, intent)
        .await
        .map_err(intent_failure)?
    {
        macro_engine::ExecuteOutcome::Completed(clicks) => {
            // Every click is its own progress event, so a batch of ten
            // reports ten targets rather than one opaque step.
            let mut last = None;
            for click in clicks {
                highlight(click.highlight.clone());
                last = Some(click.highlight);
            }
            Ok(StepOutcome {
                output: ActionOutput::default(),
                highlight: last,
            })
        }
        macro_engine::ExecuteOutcome::HaltedEarly {
            reason,
            clicks_completed,
            failed_candidate_index,
            ..
        } => Err(StepError::BatchHalted {
            reason,
            clicks_completed,
            failed_candidate_index,
        }),
    }
}

/// Execute one Playbook step against the live target.
///
/// `step_index` feeds repair metadata so a broken legacy selector still
/// reports its real position. Single-target semantic consent approves the
/// human-readable intent itself before any snapshot runs, so a denied or
/// unresolvable intent never touches the page. Plural steps invert that
/// order deliberately: they resolve read-only first so the gate can name the
/// controls, and only then ask — a batch approval that could not say "three
/// invoices" would not be informed consent.
///
/// # Errors
/// Returns [`StepError`] on invalid definitions, denied approvals,
/// unresolvable intents, drift-halted batches, repair flags, and CDP
/// failures.
// Eight parameters is the honest arity here: target, inputs, step identity,
// and three independent callbacks (progress, action consent, intent consent).
#[allow(clippy::too_many_arguments)]
pub async fn execute_step<F, G, H, Fa, Ha>(
    browser: &ManagedBrowser,
    origin: &Url,
    output: &Path,
    step_index: usize,
    step: &playbook_store::Step,
    mut highlight: F,
    mut consent_action: G,
    mut consent_intent: H,
) -> Result<StepOutcome, StepError>
where
    F: FnMut(Highlight),
    G: FnMut(usize, Action) -> Fa,
    Fa: Future<Output = bool>,
    H: FnMut(usize, IntentApproval) -> Ha,
    Ha: Future<Output = bool>,
{
    match step {
        playbook_store::Step::LegacySelector { action, wait } => {
            let planned = macro_engine::MacroStep {
                action: action.clone(),
                wait: wait.clone(),
            };
            if needs_consent(action) && !consent_action(step_index, action.clone()).await {
                return Err(StepError::ApprovalRequired);
            }
            // Submit bypasses the generic executor by design; only an
            // approved, validated form target may fire (mirrors first-run).
            if let Action::Submit { selector } = &planned.action {
                planned
                    .action
                    .validate(origin)
                    .map_err(|_| StepError::Invalid)?;
                let output = browser
                    .submit_approved(selector, origin)
                    .await
                    .map_err(StepError::from)?;
                return Ok(StepOutcome {
                    output,
                    highlight: None,
                });
            }
            let mut seen = None;
            let output = macro_engine::replay_step(
                browser,
                origin,
                step_index,
                &planned,
                output,
                |target| {
                    seen = Some(target.clone());
                    highlight(target);
                },
            )
            .await
            .map_err(|error| match error {
                macro_engine::ReplayError::Repair(request) => StepError::NeedsRepair(request),
                macro_engine::ReplayError::Browser(error) => StepError::Browser(error),
                macro_engine::ReplayError::Publication(_)
                | macro_engine::ReplayError::ApprovalRequired => StepError::Invalid,
            })?;
            Ok(StepOutcome {
                output,
                highlight: seen,
            })
        }
        playbook_store::Step::Semantic { intent } if intent.is_plural => {
            execute_batch_step(
                browser,
                origin,
                step_index,
                intent,
                &mut highlight,
                &mut consent_intent,
            )
            .await
        }
        playbook_store::Step::Semantic { intent } => {
            if !consent_intent(step_index, IntentApproval::single(intent.clone())).await {
                return Err(StepError::ApprovalRequired);
            }
            let outcome = macro_engine::execute_intent(browser, origin, intent)
                .await
                .map_err(intent_failure)?;
            highlight(outcome.highlight.clone());
            Ok(StepOutcome {
                output: ActionOutput::default(),
                highlight: Some(outcome.highlight),
            })
        }
    }
}

/// Per-step phase streamed to the UI during a sequence run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SequencePhase {
    Started,
    Running,
    Completed,
    Blocked,
}

/// One progress event: position, phase, and the latest highlight (present on
/// `Running` events while a target is acted on).
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SequenceEvent {
    pub step_index: usize,
    pub total_steps: usize,
    pub phase: SequencePhase,
    pub highlight: Option<Highlight>,
}

/// Terminal status of a finished sequence run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SequenceStatus {
    Completed,
    NeedsRepair,
    Denied,
    Failed,
}

/// Final result of a sequence run: how far it got and how it ended. Returned
/// by value on every path — terminal states are data, not errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SequenceOutcome {
    pub completed_steps: usize,
    pub total_steps: usize,
    pub status: SequenceStatus,
    pub stopped_at: Option<usize>,
}

#[must_use]
pub fn terminal_status(error: &StepError) -> SequenceStatus {
    match error {
        StepError::NeedsRepair(_) => SequenceStatus::NeedsRepair,
        StepError::ApprovalRequired => SequenceStatus::Denied,
        // A drift-halted batch was approved and did act, so it is a failure,
        // not a denial: `completed_steps` still reports the steps that
        // finished, and `stopped_at` names the step that broke.
        StepError::BatchHalted { .. }
        | StepError::NoMatch(_)
        | StepError::Browser(_)
        | StepError::Invalid => SequenceStatus::Failed,
    }
}

/// Run validated Playbook steps in order, streaming progress. The first
/// terminal step stops the run immediately: repair flags, denials, and hard
/// failures (including timeouts) never fall through into later steps.
// Nine parameters is the honest arity: target, inputs, step identity, and
// four independent callbacks (progress, highlight fan-out, two consents).
#[allow(clippy::too_many_arguments)]
pub async fn run_playbook_sequence<E, G, H, Fa, Ha>(
    browser: &ManagedBrowser,
    portal: &Url,
    output: &Path,
    steps: &[playbook_store::Step],
    mut emit: E,
    mut consent_action: G,
    mut consent_intent: H,
) -> SequenceOutcome
where
    E: FnMut(SequenceEvent),
    G: FnMut(usize, Action) -> Fa,
    Fa: Future<Output = bool>,
    H: FnMut(usize, IntentApproval) -> Ha,
    Ha: Future<Output = bool>,
{
    let total_steps = steps.len();
    let mut completed_steps = 0;
    for (index, step) in steps.iter().enumerate() {
        emit(SequenceEvent {
            step_index: index,
            total_steps,
            phase: SequencePhase::Started,
            highlight: None,
        });
        let result = {
            let emit_highlight = &mut emit;
            execute_step(
                browser,
                portal,
                output,
                index,
                step,
                |target| {
                    emit_highlight(SequenceEvent {
                        step_index: index,
                        total_steps,
                        phase: SequencePhase::Running,
                        highlight: Some(target),
                    });
                },
                &mut consent_action,
                &mut consent_intent,
            )
            .await
        };
        match result {
            Ok(_) => {
                completed_steps += 1;
                emit(SequenceEvent {
                    step_index: index,
                    total_steps,
                    phase: SequencePhase::Completed,
                    highlight: None,
                });
            }
            Err(error) => {
                let status = terminal_status(&error);
                emit(SequenceEvent {
                    step_index: index,
                    total_steps,
                    phase: SequencePhase::Blocked,
                    highlight: None,
                });
                return SequenceOutcome {
                    completed_steps,
                    total_steps,
                    status,
                    stopped_at: Some(index),
                };
            }
        }
    }
    SequenceOutcome {
        completed_steps,
        total_steps,
        status: SequenceStatus::Completed,
        stopped_at: None,
    }
}

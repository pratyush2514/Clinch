#![deny(unsafe_code)]
//! Playbook step dispatch: one step, two execution paths.
//!
//! Legacy steps replay their recorded action exactly (v1 macro semantics,
//! including wait conditions). Semantic steps resolve `{role, label}` against
//! the live AX tree and click through the backend node — no selector is ever
//! constructed. Both paths honor the same highlight-then-act ordering as
//! first-run execution, and both require explicit approval for anything
//! beyond navigation and file downloads.

use browser_driver::{Action, ActionOutput, BrowserError, Highlight, ManagedBrowser};
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
    #[error("A recorded selector needs targeted repair")]
    NeedsRepair(macro_engine::RepairRequest),
    #[error("CDP execution failed")]
    Browser(#[from] BrowserError),
    #[error("Step definition is invalid")]
    Invalid,
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

/// Execute one Playbook step against the live target.
///
/// `step_index` feeds repair metadata so a broken legacy selector still
/// reports its real position. Semantic consent approves the human-readable
/// intent itself before any snapshot runs, so a denied or unresolvable intent
/// never touches the page.
///
/// # Errors
/// Returns [`StepError`] on invalid definitions, denied approvals,
/// unresolvable intents, repair flags, and CDP failures.
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
    H: FnMut(usize, SemanticIntent) -> Ha,
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
        playbook_store::Step::Semantic { intent } => {
            if !consent_intent(step_index, intent.clone()).await {
                return Err(StepError::ApprovalRequired);
            }
            let outcome = macro_engine::execute_intent(browser, origin, intent)
                .await
                .map_err(|error| match error {
                    macro_engine::IntentError::NoMatch(reason) => StepError::NoMatch(reason),
                    macro_engine::IntentError::Browser(error) => StepError::Browser(error),
                })?;
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

fn terminal_status(error: &StepError) -> SequenceStatus {
    match error {
        StepError::NeedsRepair(_) => SequenceStatus::NeedsRepair,
        StepError::ApprovalRequired => SequenceStatus::Denied,
        StepError::NoMatch(_) | StepError::Browser(_) | StepError::Invalid => {
            SequenceStatus::Failed
        }
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
    H: FnMut(usize, SemanticIntent) -> Ha,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_cover_every_step_error() {
        assert_eq!(
            terminal_status(&StepError::ApprovalRequired),
            SequenceStatus::Denied
        );
        assert_eq!(
            terminal_status(&StepError::NoMatch(String::new())),
            SequenceStatus::Failed
        );
        assert_eq!(terminal_status(&StepError::Invalid), SequenceStatus::Failed);
        assert_eq!(
            terminal_status(&StepError::Browser(BrowserError::Timeout)),
            SequenceStatus::Failed
        );
    }

    #[test]
    fn event_and_outcome_shapes_match_the_ipc_contract() -> Result<(), Box<dyn std::error::Error>> {
        let event = SequenceEvent {
            step_index: 1,
            total_steps: 3,
            phase: SequencePhase::Running,
            highlight: None,
        };
        let json = serde_json::to_string(&event)?;
        assert!(json.contains("\"stepIndex\":1"));
        assert!(json.contains("\"phase\":\"running\""));
        let outcome = SequenceOutcome {
            completed_steps: 2,
            total_steps: 3,
            status: SequenceStatus::NeedsRepair,
            stopped_at: Some(2),
        };
        let json = serde_json::to_string(&outcome)?;
        assert!(json.contains("\"status\":\"needs_repair\""));
        assert!(json.contains("\"stoppedAt\":2"));
        Ok(())
    }
}

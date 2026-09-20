#![deny(unsafe_code)]
use crate::EngineError;
use browser_driver::ActionOutput;
use macro_engine::{Macro, RepairRequest};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct TaskId(pub i64);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Planned,
    Running,
    NeedsRepair,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    Completed,
    NeedsRepair,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Record,
    Replay,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    Browser,
    Timeout,
    WrongOrigin,
    InvalidAction,
    Download,
    Storage,
    MacroPublication,
    ApprovalRequired,
}

impl From<&browser_driver::BrowserError> for FailureReason {
    fn from(error: &browser_driver::BrowserError) -> Self {
        use browser_driver::BrowserError;
        match error {
            BrowserError::Timeout => Self::Timeout,
            BrowserError::WrongOrigin => Self::WrongOrigin,
            BrowserError::InvalidAction => Self::InvalidAction,
            BrowserError::Download => Self::Download,
            BrowserError::Storage => Self::Storage,
            _ => Self::Browser,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub state: StepState,
    pub output: ActionOutput,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Plan {
    pub(crate) recording: Macro,
    pub steps: Vec<Step>,
}

impl Plan {
    /// # Errors
    /// Returns macro validation errors; state is always initialized internally.
    pub fn new(recording: Macro) -> Result<Self, EngineError> {
        recording.validate()?;
        let steps = recording
            .steps
            .iter()
            .map(|_| Step {
                state: StepState::Pending,
                output: ActionOutput::default(),
                elapsed_ms: 0,
            })
            .collect();
        Ok(Self { recording, steps })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: TaskId,
    pub revision: i64,
    pub workflow: String,
    pub mode: RunMode,
    pub state: TaskState,
    pub plan: Plan,
    pub repair: Option<RepairRequest>,
    pub failure: Option<FailureReason>,
    pub elapsed_ms: u64,
}

impl Task {
    pub(crate) fn new(workflow: String, plan: Plan, mode: RunMode) -> Self {
        Self {
            id: TaskId::default(),
            revision: 0,
            workflow,
            mode,
            state: TaskState::Planned,
            plan,
            repair: None,
            failure: None,
            elapsed_ms: 0,
        }
    }

    pub(crate) fn start_step(&mut self, index: usize) -> Result<(), EngineError> {
        if !matches!(self.state, TaskState::Planned | TaskState::Running)
            || self
                .plan
                .steps
                .get(index)
                .is_none_or(|step| step.state != StepState::Pending)
            || self.plan.steps[..index]
                .iter()
                .any(|step| step.state != StepState::Completed)
        {
            return Err(EngineError::Transition);
        }
        self.state = TaskState::Running;
        self.plan.steps[index].state = StepState::Running;
        Ok(())
    }

    fn running_step(&mut self, index: usize) -> Result<&mut Step, EngineError> {
        if self.state != TaskState::Running {
            return Err(EngineError::Transition);
        }
        self.plan
            .steps
            .get_mut(index)
            .filter(|step| step.state == StepState::Running)
            .ok_or(EngineError::Transition)
    }

    pub(crate) fn complete_step(
        &mut self,
        index: usize,
        output: ActionOutput,
        elapsed_ms: u64,
    ) -> Result<(), EngineError> {
        let step = self.running_step(index)?;
        step.state = StepState::Completed;
        step.output = output;
        step.elapsed_ms = elapsed_ms;
        Ok(())
    }

    pub(crate) fn repair_step(
        &mut self,
        index: usize,
        repair: RepairRequest,
        elapsed_ms: u64,
    ) -> Result<(), EngineError> {
        let step = self.running_step(index)?;
        step.state = StepState::NeedsRepair;
        step.elapsed_ms = elapsed_ms;
        self.repair = Some(repair);
        self.state = TaskState::NeedsRepair;
        Ok(())
    }

    pub(crate) fn fail_step(
        &mut self,
        index: usize,
        elapsed_ms: u64,
        reason: FailureReason,
    ) -> Result<(), EngineError> {
        let step = self.running_step(index)?;
        step.state = StepState::Failed;
        step.elapsed_ms = elapsed_ms;
        self.state = TaskState::Failed;
        self.failure = Some(reason);
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<(), EngineError> {
        if self.state != TaskState::Running
            || self
                .plan
                .steps
                .iter()
                .any(|step| step.state != StepState::Completed)
        {
            return Err(EngineError::Transition);
        }
        self.state = TaskState::Completed;
        Ok(())
    }

    pub(crate) fn fail_publication(&mut self) -> Result<(), EngineError> {
        self.finish()?;
        self.state = TaskState::Failed;
        self.failure = Some(FailureReason::MacroPublication);
        Ok(())
    }

    pub(crate) fn interrupt(&mut self) -> Result<(), EngineError> {
        if !matches!(self.state, TaskState::Planned | TaskState::Running) {
            return Err(EngineError::Transition);
        }
        for step in &mut self.plan.steps {
            if step.state == StepState::Running {
                step.state = StepState::Interrupted;
            }
        }
        self.state = TaskState::Interrupted;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskRequest;
    #[test]
    fn transitions_reject_skipping_repeating_and_false_completion() -> Result<(), EngineError> {
        let request: TaskRequest = serde_json::from_str(
            r#"{"workflow":"reports","portalUrl":"https://example.com","linkSelector":null,"downloadSelector":"a.report"}"#,
        )?;
        let mut task = Task::new("reports".into(), request.plan()?, RunMode::Record);
        assert!(task.start_step(1).is_err());
        assert!(task.finish().is_err());
        assert!(task.start_step(99).is_err());
        task.start_step(0)?;
        assert!(task.start_step(0).is_err());
        task.complete_step(0, ActionOutput::default(), 1)?;
        assert!(task.complete_step(0, ActionOutput::default(), 1).is_err());
        assert!(task.finish().is_err());
        task.start_step(1)?;
        task.fail_step(1, 2, FailureReason::Browser)?;
        assert!(task.start_step(1).is_err());
        assert!(task.finish().is_err());
        Ok(())
    }
}

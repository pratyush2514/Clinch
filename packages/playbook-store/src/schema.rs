#![deny(unsafe_code)]
//! Versioned Playbook schema: human-readable automation definitions.
//!
//! Saved v1 macros (`macros/<workflow>.json`) keep replaying byte-for-byte;
//! this schema sits above them. [`Playbook::from_macro`] lifts any saved v1
//! macro losslessly — actions map one-to-one with waits preserved — so
//! existing workflows migrate forward instead of being rewritten. Unknown
//! versions and step kinds fail closed.

use browser_driver::{Action, WaitCondition};
use macro_engine::{Macro, SemanticIntent};
use serde::{Deserialize, Serialize};
use url::Url;

/// Current Playbook envelope version. The macro replay format keeps its own
/// independent versioning inside `macro-engine`.
pub const SCHEMA_VERSION: u32 = 1;
const MAX_STEPS: usize = 100;
const MAX_NAME_LEN: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("Unsupported playbook version")]
    Version,
    #[error("Invalid playbook definition")]
    Invalid,
    #[error("Playbook JSON is invalid")]
    Json(#[from] serde_json::Error),
}

/// One executable Playbook step. Legacy steps replay the exact recorded
/// action; semantic steps resolve `{role, label}` against the live AX tree at
/// run time and never touch a selector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    LegacySelector {
        action: Action,
        wait: Option<WaitCondition>,
    },
    Semantic {
        intent: SemanticIntent,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Playbook {
    pub version: u32,
    pub name: String,
    pub origin: Url,
    pub steps: Vec<Step>,
}

impl Playbook {
    /// # Errors
    /// Rejects unsupported versions, unsafe names/origins, and invalid steps.
    pub fn new(name: String, origin: Url, steps: Vec<Step>) -> Result<Self, SchemaError> {
        let playbook = Self {
            version: SCHEMA_VERSION,
            name,
            origin,
            steps,
        };
        playbook.validate()?;
        Ok(playbook)
    }

    /// # Errors
    /// Rejects unsupported versions, unsafe names/origins, oversized plans,
    /// and invalid steps.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.version != SCHEMA_VERSION {
            return Err(SchemaError::Version);
        }
        if self.name.is_empty()
            || self.name.len() > MAX_NAME_LEN
            || !self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(SchemaError::Invalid);
        }
        if !matches!(self.origin.scheme(), "https" | "http")
            || self.origin.host_str().is_none()
            || !self.origin.username().is_empty()
            || self.origin.password().is_some()
        {
            return Err(SchemaError::Invalid);
        }
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS {
            return Err(SchemaError::Invalid);
        }
        for step in &self.steps {
            match step {
                Step::LegacySelector { action, wait } => {
                    action
                        .validate(&self.origin)
                        .map_err(|_| SchemaError::Invalid)?;
                    // Mirrors the macro-engine wait bounds so migration is total.
                    if let Some(condition) = wait
                        && (condition.selector.trim().is_empty()
                            || condition.selector.len() > 2048
                            || condition.timeout_ms == 0
                            || condition.timeout_ms > 30_000)
                    {
                        return Err(SchemaError::Invalid);
                    }
                }
                Step::Semantic { intent } => {
                    intent.validate().map_err(|_| SchemaError::Invalid)?;
                }
            }
        }
        Ok(())
    }

    /// Lift a saved v1 macro into a Playbook without loss: every action and
    /// wait condition becomes a legacy step. Healing metadata stays with the
    /// macro file — execution history is not part of the definition.
    ///
    /// # Errors
    /// Rejects invalid macros and unsafe workflow names.
    pub fn from_macro(name: String, macro_definition: &Macro) -> Result<Self, SchemaError> {
        macro_definition
            .validate()
            .map_err(|_| SchemaError::Invalid)?;
        let steps = macro_definition
            .steps
            .iter()
            .map(|recorded| Step::LegacySelector {
                action: recorded.action.clone(),
                wait: recorded.wait.clone(),
            })
            .collect();
        Self::new(name, macro_definition.origin.clone(), steps)
    }

    /// # Errors
    /// Returns JSON errors for malformed documents.
    pub fn render(&self) -> Result<String, SchemaError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse and validate in one step: unknown versions and step kinds fail
    /// here, never at execution time.
    ///
    /// # Errors
    /// Returns JSON, version, or definition errors.
    pub fn parse(json: &str) -> Result<Self, SchemaError> {
        let playbook: Self = serde_json::from_str(json)?;
        playbook.validate()?;
        Ok(playbook)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Result<Url, url::ParseError> {
        Url::parse("https://portal.example.com/")
    }

    fn macro_fixture() -> Result<Macro, serde_json::Error> {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "lastHealedAt": null,
            "healingHistory": [],
            "origin": "https://portal.example.com/",
            "steps": [
                {"action": {"type": "navigate", "url": "https://portal.example.com/"}, "wait": {"selector": "a.report", "timeoutMs": 5000}},
                {"action": {"type": "download_links", "selector": "a.report"}, "wait": null},
            ],
        }))
    }

    #[test]
    fn v1_macros_migrate_losslessly() -> Result<(), Box<dyn std::error::Error>> {
        let recording = macro_fixture()?;
        let playbook = Playbook::from_macro("reports".into(), &recording)?;
        assert_eq!(playbook.version, SCHEMA_VERSION);
        assert_eq!(playbook.steps.len(), 2);
        assert!(matches!(
            &playbook.steps[0],
            Step::LegacySelector { wait: Some(_), .. }
        ));
        // Round-trip through the envelope preserves everything.
        let revived = Playbook::parse(&playbook.render()?)?;
        assert_eq!(revived, playbook);
        Ok(())
    }

    fn pay_step() -> Step {
        Step::Semantic {
            intent: SemanticIntent {
                role: "button".into(),
                label_query: "Pay".into(),
                container_query: None,
                raw_prompt: String::new(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: None,
            },
        }
    }

    #[test]
    fn semantic_steps_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let playbook = Playbook::new("pay".into(), origin()?, vec![pay_step()])?;
        let revived = Playbook::parse(&playbook.render()?)?;
        assert_eq!(revived, playbook);
        Ok(())
    }

    #[test]
    fn unknown_versions_kinds_and_identities_fail_closed() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut playbook = Playbook::new("pay".into(), origin()?, vec![pay_step()])?;
        playbook.version = 99;
        assert!(matches!(playbook.validate(), Err(SchemaError::Version)));
        assert!(Playbook::parse(r#"{"version":1,"name":"x","origin":"https://example.com/","steps":[{"kind":"xpath","selector":"//a"}]}"#).is_err());
        for name in ["", "../reports", "reports/name", &"b".repeat(65)] {
            assert!(Playbook::new(name.into(), origin()?, vec![pay_step()]).is_err());
        }
        assert!(Playbook::new("empty".into(), origin()?, Vec::new()).is_err());
        assert!(
            Playbook::parse(
                r#"{"version":1,"name":"x","origin":"https://user:secret@example.com/","steps":[]}"#
            )
            .is_err()
        );
        Ok(())
    }
}

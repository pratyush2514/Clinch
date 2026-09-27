//! Integration tests for the `playbook_store::schema` envelope.
//!
//! Moved out of `src/schema.rs` so the main source stays test-free.

use macro_engine::{Macro, SemanticIntent};
use playbook_store::schema::*;
use url::Url;

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
fn unknown_versions_kinds_and_identities_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
    let mut playbook = Playbook::new("pay".into(), origin()?, vec![pay_step()])?;
    playbook.version = 99;
    assert!(matches!(playbook.validate(), Err(SchemaError::Version)));
    assert!(Playbook::parse(r#"{"version":1,"name":"x","origin":"https://example.com/","steps":[{"kind":"xpath","selector":"//a"}]}"#).is_err());
    for name in ["", "../reports", "reports/name", &"b".repeat(65)] {
        assert!(Playbook::new(name.into(), origin()?, vec![pay_step()]).is_err());
    }
    assert!(Playbook::new("empty".into(), origin()?, Vec::new()).is_err());
    assert!(
        Playbook::new("pay".into(), origin()?, vec![pay_step()])?
            .with_description(Some("x".repeat(281)))
            .validate()
            .is_err()
    );
    assert!(
        Playbook::parse(
            r#"{"version":1,"name":"x","origin":"https://user:secret@example.com/","steps":[]}"#
        )
        .is_err()
    );
    Ok(())
}

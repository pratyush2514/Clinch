//! Integration tests for the `macro_engine` crate root.
//!
//! Moved out of `src/lib.rs` so the main source stays test-free.

use macro_engine::*;
use proptest::prelude::*;
use url::Url;

#[test]
fn validates_every_action_and_wait_before_replay() -> Result<(), Box<dyn std::error::Error>> {
    let origin = Url::parse("https://example.com/")?;
    for target in [
        "https://other.example/",
        "javascript:alert(1)",
        "file:///tmp/report",
        "https://user:password@example.com/",
    ] {
        let recording = Macro {
            version: VERSION,
            last_healed_at: None,
            healing_history: Vec::new(),
            origin: origin.clone(),
            steps: vec![MacroStep {
                action: Action::Navigate {
                    url: Url::parse(target)?,
                },
                wait: None,
            }],
        };
        assert!(recording.validate().is_err());
    }
    let mut recording = Macro {
        version: VERSION,
        last_healed_at: None,
        healing_history: Vec::new(),
        origin,
        steps: vec![MacroStep {
            action: Action::DownloadLinks {
                selector: "a.report".into(),
            },
            wait: Some(WaitCondition {
                selector: "a.report".into(),
                timeout_ms: 0,
            }),
        }],
    };
    assert!(recording.validate().is_err());
    recording.steps[0].wait = None;
    assert!(recording.validate().is_ok());
    recording.steps = vec![recording.steps[0].clone(); 101];
    assert!(recording.validate().is_err());
    assert!(
        serde_json::from_str::<Action>(r#"{"type":"evaluate","script":"document.cookie"}"#)
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn version_validation_and_atomic_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mut recorder = Recorder::new(Url::parse("https://example.com/")?);
    assert!(
        Recorder::new(Url::parse("https://example.com/")?)
            .finish()
            .is_err()
    );
    recorder.record_completed(&MacroStep {
        action: Action::DownloadLinks {
            selector: "a.report".into(),
        },
        wait: None,
    })?;
    let mut saved = recorder.finish()?;
    let path = dir.path().join("macro.json");
    saved.save(&path).await?;
    assert_eq!(Macro::load(&path).await?, saved);
    saved.version = 99;
    assert!(matches!(saved.save(&path).await, Err(MacroError::Version)));
    assert_eq!(Macro::load(&path).await?.version, VERSION);
    tokio::fs::write(&path, b"{broken").await?;
    assert!(Macro::load(&path).await.is_err());
    Ok(())
}

proptest! {
    #[test]
    fn action_roundtrip_preserves_unicode(selector in "[a-zA-Z#][a-zA-Z0-9_-]{0,80}", value in ".{0,256}") {
        let step = MacroStep { action: Action::Fill { selector, value }, wait: None };
        let json = serde_json::to_string(&step)?;
        prop_assert_eq!(serde_json::from_str::<MacroStep>(&json)?, step);
    }
}

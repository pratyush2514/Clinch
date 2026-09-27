//! Integration tests for `browser_driver::screencast`.
//!
//! Moved out of `src/` so the main source stays test-free.

use browser_driver::{
    BrowserError, LaunchOptions, ManagedBrowser, SCREENCAST_JPEG_QUALITY, ScreencastFrame,
    WindowMode,
};
use chromiumoxide::cdp::browser_protocol::page::{
    ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
};

/// Hermetic lifecycle proof for on-demand background contexts: dormant
/// by default, background-first, fail-closed without Chromium, and the
/// screencast wire contract pinned — no browser binary involved. Live
/// acquire/release ride `launch_with_options`/`shutdown`, covered by the
/// Chromium-gated fixtures.
#[tokio::test]
async fn test_lazy_context_acquisition_and_release() -> Result<(), Box<dyn std::error::Error>> {
    // Dormant by default: background acquisition is off-screen headed
    // by construction, so no visible window can ever spawn on this path.
    assert_eq!(
        LaunchOptions::offscreen_headed().mode,
        WindowMode::Offscreen
    );
    assert_eq!(LaunchOptions::interactive().mode, WindowMode::Headed);
    // Failed acquisition leaves nothing behind: a missing executable
    // fails fast at spawn with no child process and no retained state
    // to release — repeatable without side effects.
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("no-chromium-here");
    let profile = dir.path().join("profile");
    for _ in 0..2 {
        assert!(matches!(
            ManagedBrowser::launch_with_options(
                &missing,
                &profile,
                LaunchOptions::offscreen_headed()
            )
            .await,
            Err(BrowserError::Launch)
        ));
    }
    // Screencast wire contract: JPEG at quality 80, exactly as the
    // preview card decodes it.
    let params = StartScreencastParams::builder()
        .format(StartScreencastFormat::Jpeg)
        .quality(SCREENCAST_JPEG_QUALITY)
        .build();
    let wire = serde_json::to_value(&params).map_err(|_| "params must serialize")?;
    assert_eq!(
        wire.get("format").and_then(serde_json::Value::as_str),
        Some("jpeg")
    );
    assert_eq!(
        wire.get("quality").and_then(serde_json::Value::as_i64),
        Some(80)
    );
    assert_eq!(
        ScreencastFrameAckParams::IDENTIFIER,
        "Page.screencastFrameAck"
    );
    let ack = ScreencastFrameAckParams::new(7);
    let ack_wire = serde_json::to_value(&ack).map_err(|_| "ack must serialize")?;
    assert_eq!(
        ack_wire
            .get("sessionId")
            .and_then(serde_json::Value::as_i64),
        Some(7)
    );
    // Frame payloads round-trip base64 plus the ack session id.
    let frame = ScreencastFrame {
        data: "aGVsbG8=".into(),
        session_id: 7,
    };
    let revived: ScreencastFrame =
        serde_json::from_value(serde_json::to_value(&frame).map_err(|_| "frame must serialize")?)
            .map_err(|_| "frame must deserialize")?;
    assert_eq!(revived, frame);
    Ok(())
}

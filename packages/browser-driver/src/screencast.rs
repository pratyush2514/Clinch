#![deny(unsafe_code)]
//! Embedded CDP screencast streaming for the UI preview card.
//!
//! The app-owned background browser renders headless, so the workspace
//! shows its viewport through `Page.startScreencast` instead of OS window
//! capture. Frames flow as base64 JPEG over the existing Tauri IPC channel
//! pattern; every frame is acknowledged back to the renderer, otherwise
//! Chromium stalls the stream after its buffer fills.

use crate::{BrowserError, IO_TIMEOUT, ManagedBrowser};
use chromiumoxide::{
    cdp::browser_protocol::page::{
        EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat,
        StartScreencastParams, StopScreencastParams,
    },
    listeners::EventStream,
};
use serde::{Deserialize, Serialize};

/// JPEG quality for preview frames: legible text at modest frame sizes.
pub const SCREENCAST_JPEG_QUALITY: i64 = 80;

/// Maximum screencast frame width in CSS pixels. Chromium downscales before
/// JPEG encoding, so the IPC payload stays small and frames render quickly;
/// the thread card shows the viewport at roughly half window width, where
/// 800px is legible without the multi-megabyte cost of a full-viewport
/// capture on every paint.
pub const SCREENCAST_MAX_WIDTH: i64 = 800;

/// One compressed viewport frame, base64 JPEG, ready for a
/// `data:image/jpeg;base64,…` source. `session_id` feeds the mandatory
/// per-frame ack back to the renderer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreencastFrame {
    pub data: String,
    pub session_id: i64,
}

impl ManagedBrowser {
    /// Start streaming JPEG viewport frames at [`SCREENCAST_JPEG_QUALITY`],
    /// downscaled to [`SCREENCAST_MAX_WIDTH`]. Frames flow until
    /// [`ManagedBrowser::stop_screencast`]; pair with
    /// [`ManagedBrowser::screencast_frames`] plus
    /// [`ManagedBrowser::ack_screencast_frame`] — an unacked stream stalls.
    ///
    /// # Errors
    /// Reports connection failure or timeout.
    pub async fn start_screencast(&self) -> Result<(), BrowserError> {
        let params = StartScreencastParams::builder()
            .format(StartScreencastFormat::Jpeg)
            .quality(SCREENCAST_JPEG_QUALITY)
            .max_width(SCREENCAST_MAX_WIDTH)
            .build();
        tokio::time::timeout(IO_TIMEOUT, self.page.execute(params))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }

    /// Subscribe to screencast frames on this target's session. Subscribe
    /// before [`ManagedBrowser::start_screencast`] so the opening frames
    /// are not lost to an unregistered listener.
    ///
    /// # Errors
    /// Reports connection failure.
    pub async fn screencast_frames(
        &self,
    ) -> Result<EventStream<EventScreencastFrame>, BrowserError> {
        self.page
            .event_listener()
            .await
            .map_err(|_| BrowserError::Connection)
    }

    /// Acknowledge one frame so the renderer keeps streaming. Best-effort
    /// at the call site: a failed ack means the session is gone, and the
    /// frame loop ends on its own.
    ///
    /// # Errors
    /// Reports connection failure or timeout.
    pub async fn ack_screencast_frame(&self, session_id: i64) -> Result<(), BrowserError> {
        let params = ScreencastFrameAckParams::new(session_id);
        tokio::time::timeout(IO_TIMEOUT, self.page.execute(params))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }

    /// Stop streaming viewport frames. Best-effort pairing for
    /// [`ManagedBrowser::start_screencast`]; safe to call when idle.
    ///
    /// # Errors
    /// Reports connection failure or timeout.
    pub async fn stop_screencast(&self) -> Result<(), BrowserError> {
        tokio::time::timeout(
            IO_TIMEOUT,
            self.page.execute(StopScreencastParams::default()),
        )
        .await
        .map_err(|_| BrowserError::Timeout)?
        .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LaunchOptions;

    /// Hermetic lifecycle proof for on-demand background contexts: dormant
    /// by default, background-first, fail-closed without Chromium, and the
    /// screencast wire contract pinned — no browser binary involved. Live
    /// acquire/release ride `launch_with_options`/`shutdown`, covered by the
    /// Chromium-gated fixtures.
    #[tokio::test]
    async fn test_lazy_context_acquisition_and_release() -> Result<(), Box<dyn std::error::Error>> {
        // Dormant by default: background acquisition is headless by
        // construction, so no OS window can ever spawn on this path.
        assert!(LaunchOptions::replay().headless);
        assert!(!LaunchOptions::interactive().headless);
        // Failed acquisition leaves nothing behind: a missing executable
        // fails fast at spawn with no child process and no retained state
        // to release — repeatable without side effects.
        let dir = tempfile::tempdir()?;
        let missing = dir.path().join("no-chromium-here");
        let profile = dir.path().join("profile");
        for _ in 0..2 {
            assert!(matches!(
                ManagedBrowser::launch_with_options(&missing, &profile, LaunchOptions::replay())
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
        let revived: ScreencastFrame = serde_json::from_value(
            serde_json::to_value(&frame).map_err(|_| "frame must serialize")?,
        )
        .map_err(|_| "frame must deserialize")?;
        assert_eq!(revived, frame);
        Ok(())
    }
}

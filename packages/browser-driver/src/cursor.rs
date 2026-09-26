//! Agent cursor position events for the UI overlay.
//!
//! CDP screencast frames carry no cursor: the synthetic pointer is invisible
//! in the managed-Chromium panel, so a run that clicks the avatar looks
//! identical to one that does nothing. Every trusted-input dispatch emits a
//! [`CursorEvent`] — page CSS pixels, viewport-relative — so the frontend can
//! render a visible cursor travelling, hovering, and clicking: the legible,
//! Muse-style navigation the product promises. Purely presentational:
//! automation never reads these events, and a missing sink never fails a
//! dispatch.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Sentinel [`CursorEvent::session_id`] when no screencast session has been
/// observed yet. The UI drops such events through the same session latch it
/// uses for frames, so a cursor can never render without its stream.
pub const NO_CURSOR_SESSION: i64 = -1;

/// Viewport assumed when the live query fails. Matches the off-screen headed
/// `--window-size` launch flag: the session the cursor overlay is built for.
pub const FALLBACK_VIEWPORT: (f64, f64) = (1920.0, 1080.0);

/// Which phase of the human-like input sequence the pointer is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorEventKind {
    Move,
    Press,
    Release,
}

/// One synthetic pointer position. Coordinates are page CSS pixels relative
/// to the viewport origin; `viewport_width`/`viewport_height` name the space
/// they are relative to so the UI can scale them onto the rendered frame.
/// `session_id` is the CDP screencast session latched by the frame pump, so
/// the UI can apply its proven stale-session filter.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CursorEvent {
    pub x: f64,
    pub y: f64,
    pub kind: CursorEventKind,
    pub viewport_width: f64,
    pub viewport_height: f64,
    pub session_id: i64,
}

impl CursorEvent {
    /// Build the event for one dispatched input phase. Pure so the payload
    /// shape the UI depends on stays hermetically testable.
    #[must_use]
    pub fn new(
        kind: CursorEventKind,
        x: f64,
        y: f64,
        viewport: (f64, f64),
        session_id: i64,
    ) -> Self {
        Self {
            x,
            y,
            kind,
            viewport_width: viewport.0,
            viewport_height: viewport.1,
            session_id,
        }
    }
}

/// Sink the service layer installs to forward cursor events to the UI.
/// `Arc` so an L1 challenge restart can carry the sink across to the new
/// browser instead of silently losing the cursor mid-run.
pub type CursorEmitter = Arc<dyn Fn(CursorEvent) + Send + Sync + 'static>;

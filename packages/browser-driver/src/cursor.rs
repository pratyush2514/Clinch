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

/// Desired spacing between cursor-travel waypoints, in page CSS pixels.
/// Matches the overlay's visual glide: dense enough to read as motion,
/// sparse enough that a cross-viewport travel stays a short burst.
pub const WAYPOINT_SPACING_PX: f64 = 40.0;

/// Hard cap on travel waypoints per click. A far target still stops here:
/// the final hover dispatch covers the remaining distance, so a long drag
/// never turns into a long march.
pub const MAX_WAYPOINTS: usize = 12;

/// Pause between consecutive travel-waypoint dispatches, in milliseconds.
/// Constant and deterministic: the glide timing replays exactly.
pub const WAYPOINT_INTERVAL_MS: u64 = 18;

/// Hold time between the press and release of a trusted click, in
/// milliseconds. A human holds the button briefly; a press with no dwell
/// reads as a bounce. Constant and deterministic — no jitter, so replays
/// stay reproducible.
pub const CLICK_DWELL_MS: u64 = 100;

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

/// Intermediate points for a human-like pointer glide from `from` toward
/// `to`: evenly spaced at [`WAYPOINT_SPACING_PX`] along the straight line,
/// capped at [`MAX_WAYPOINTS`]. Excludes both endpoints — the caller never
/// dispatches a redundant move onto the resting point, and the final hover
/// dispatch covers `to` exactly. Progress is monotonic: each waypoint is
/// strictly closer to the target than the previous one. Empty when `from`
/// is (approximately) `to`, so a repeated click on the same spot never
/// replays a glide. Pure geometry — no CDP, hermetically testable.
#[must_use]
pub fn travel_waypoints(from: (f64, f64), to: (f64, f64)) -> Vec<(f64, f64)> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let distance = dx.hypot(dy);
    if distance < f64::EPSILON {
        return Vec::new();
    }
    // Walk out from `from` in spacing-sized steps, stopping at the cap or
    // before the target: the final hover dispatch covers `to` exactly, so
    // the last point always leaves a gap smaller than one hop. Cast-free
    // by construction (accumulating `f64` steps, capping by vector length).
    let mut points = Vec::new();
    let mut stepped = WAYPOINT_SPACING_PX;
    while points.len() < MAX_WAYPOINTS && stepped < distance {
        let t = stepped / distance;
        points.push((from.0 + dx * t, from.1 + dy * t));
        stepped += WAYPOINT_SPACING_PX;
    }
    points
}

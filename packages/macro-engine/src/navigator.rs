#![deny(unsafe_code)]
//! Model-guided in-page navigation: the narrow decision Muse makes.
//!
//! The pursuit loop ([`crate::pursue_page_goal`]) first tries deterministic
//! heuristics (free, instant, auditable). When those find nothing, an
//! optional [`PageNavigator`] — a small model behind this trait — makes the
//! one decision heuristics cannot: *which element advances the goal?*
//!
//! The fence is structural, not promissory:
//!
//! * The model only ever picks from [`PageAction`], a closed enum. It cannot
//!   express anything outside `click` / `done` / `give_up` —
//!   [`serde`] deserialization rejects everything else, so no JSON-schema
//!   validator or tool-calling library is needed. Rust's type system *is*
//!   the integration.
//! * The `target` element id is validated against the live snapshot before
//!   any click: the model can only touch what the harness showed it.
//! * The model never sees credentials, cookies, or page HTML — only the
//!   goal string and the element list (id, role, name, landmark, coarse
//!   position zone).
//! * One bounded HTTP call per step; the loop still caps total steps.

use browser_driver::AxElement;

/// How many snapshot elements one navigation decision may see. Bounds the
/// prompt: a portal header plus its menus fit comfortably; the rest of the
/// page is noise for a follow-up. Shared by the zone measurer (which must
/// zone exactly the slice the navigator renders) and the renderer itself.
pub const MAX_NAVIGATOR_ELEMENTS: usize = 60;

/// The closed action set a navigator may express. Deserialized directly
/// from the model's strict-JSON reply — any shape outside these three
/// variants fails to parse and the navigator declines.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PageAction {
    /// Click the element with this snapshot id.
    Click { target: i64 },
    /// The goal is already achieved on this page; nothing to click.
    Done,
    /// No element can advance the goal.
    GiveUp { reason: String },
}

/// Decides the next in-page step toward `goal`, given the live snapshot's
/// actionable elements. Synchronous by contract — one bounded model call —
/// so the async pursuit loop invokes it on a blocking thread.
///
/// Returns `None` on any failure (transport, timeout, malformed JSON):
/// a stalled or confused model degrades to the honest miss, never a hang.
///
/// The model never sees credentials, cookies, or page HTML — only the
/// goal string, the element list, and (when the caller supplies one) a
/// single viewport screenshot.
pub trait PageNavigator: Send + Sync {
    fn next_action(&self, goal: &str, elements: &[AxElement]) -> Option<PageAction>;

    /// Zone-aware variant: `zones[i]` is the coarse on-page position of
    /// `elements[i]` (`None` when geometry was unavailable). The default
    /// drops the zones so existing implementers keep working; navigators
    /// that render the element list override this to surface position.
    fn next_action_zoned(
        &self,
        goal: &str,
        elements: &[AxElement],
        zones: &[Option<PositionZone>],
    ) -> Option<PageAction> {
        let _ = zones;
        self.next_action(goal, elements)
    }

    /// Vision variant: like [`PageNavigator::next_action_zoned`] but with an
    /// optional viewport screenshot accompanying the element list.
    /// `screenshot_jpeg_b64` is base64 JPEG with NO data-URI prefix, or
    /// `None` when capture failed. The default ignores the screenshot and
    /// delegates to [`PageNavigator::next_action_zoned`], so existing
    /// implementers keep working; navigators with a vision-capable model
    /// override this to send the image alongside the element list.
    fn next_action_visual(
        &self,
        goal: &str,
        elements: &[AxElement],
        zones: &[Option<PositionZone>],
        screenshot_jpeg_b64: Option<&str>,
    ) -> Option<PageAction> {
        let _ = screenshot_jpeg_b64;
        self.next_action_zoned(goal, elements, zones)
    }

    /// Visual grounding: where on the screenshot is `target` (a plain-words
    /// description of a control)? For menus the AX tree does not expose —
    /// the model looks at pixels, not the tree. The default is
    /// [`VisualLocation::Unsupported`], so existing implementers keep
    /// working; navigators with a vision-capable model override this.
    /// `screenshot_jpeg_b64` is base64 JPEG with NO data-URI prefix.
    fn locate_visual(&self, target: &str, screenshot_jpeg_b64: &str) -> VisualLocation {
        let _ = (target, screenshot_jpeg_b64);
        VisualLocation::Unsupported
    }
}

/// Answer to a [`PageNavigator::locate_visual`] query. Coordinates are the
/// model's raw 0–1000 normalized space; [`visual_point_to_pixels`]
/// validates and converts them before anything clicks.
#[derive(Clone, Debug, PartialEq)]
pub enum VisualLocation {
    /// The model pointed at a spot (0–1000 on each axis).
    Point { x: f64, y: f64 },
    /// The model answered `{"found": false}`.
    NotFound,
    /// The call failed (timeout, transport, malformed reply); the reason is
    /// a short journal-safe phrase.
    Failed(String),
    /// No vision-capable model is configured.
    Unsupported,
}

/// Convert a model point in 0–1000 space to viewport pixels
/// `(width, height)`. Rejects non-finite values and anything outside
/// `0..=1000` on either axis (returns `None`) so an out-of-viewport answer
/// never reaches the input pipeline. Pure and unit-tested.
#[must_use]
pub fn visual_point_to_pixels(x: f64, y: f64, viewport: (f64, f64)) -> Option<(f64, f64)> {
    let (width, height) = viewport;
    let in_range = |value: f64| value.is_finite() && (0.0..=1000.0).contains(&value);
    if !in_range(x) || !in_range(y) || !width.is_finite() || !height.is_finite() {
        return None;
    }
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some((x / 1000.0 * width, y / 1000.0 * height))
}

/// Viewport-pixel rectangle the visual fallback cropped the screenshot to;
/// the model's 0–1000 answer is in this rectangle's space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisualCrop {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Why a crop-space point was not converted to a clickable viewport point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CropRemapError {
    /// Non-finite, or outside `0..=1000` on either axis.
    InvalidCoordinates,
    /// Converted cleanly but lands outside the live viewport.
    OutsideViewport,
}

/// Convert a model point in the crop's 0–1000 space to viewport pixels:
/// `vx = x + round(px * w / 1000)`, `vy = y + round(py * h / 1000)`, then
/// reject anything outside `0..=width` × `0..=height` of the live
/// `viewport` so nothing off-page reaches the input pipeline. Pure and
/// unit-tested.
///
/// # Errors
/// [`CropRemapError::InvalidCoordinates`] for a malformed model point,
/// [`CropRemapError::OutsideViewport`] when the remapped point is off-page.
pub fn crop_point_to_viewport(
    px: f64,
    py: f64,
    crop: VisualCrop,
    viewport: (f64, f64),
) -> Result<(f64, f64), CropRemapError> {
    let in_range = |value: f64| value.is_finite() && (0.0..=1000.0).contains(&value);
    if !in_range(px) || !in_range(py) {
        return Err(CropRemapError::InvalidCoordinates);
    }
    let vx = crop.x + (px * crop.w / 1000.0).round();
    let vy = crop.y + (py * crop.h / 1000.0).round();
    let (width, height) = viewport;
    let inside =
        |value: f64, max: f64| value.is_finite() && max > 0.0 && (0.0..=max).contains(&value);
    if !inside(vx, width) || !inside(vy, height) {
        return Err(CropRemapError::OutsideViewport);
    }
    Ok((vx, vy))
}

/// Coarse on-page position of one element, rendered for the model as
/// `top-left` … `bottom-right`. Computed against the bounding box of the
/// rendered control set — distribution-relative, so it needs no viewport
/// metrics and stays meaningful on scrolled pages. Lets the model pick an
/// unnamed avatar button by its header position instead of guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionZone {
    TopLeft,
    TopCenter,
    TopRight,
    MiddleLeft,
    MiddleCenter,
    MiddleRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl std::fmt::Display for PositionZone {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            PositionZone::TopLeft => "top-left",
            PositionZone::TopCenter => "top-center",
            PositionZone::TopRight => "top-right",
            PositionZone::MiddleLeft => "middle-left",
            PositionZone::MiddleCenter => "middle-center",
            PositionZone::MiddleRight => "middle-right",
            PositionZone::BottomLeft => "bottom-left",
            PositionZone::BottomCenter => "bottom-center",
            PositionZone::BottomRight => "bottom-right",
        };
        formatter.write_str(label)
    }
}

/// Zone of the point `(x, y)` inside `bounds = (min_x, min_y, max_x,
/// max_y)`: each axis split into thirds. Pure and unit-tested; the pursuit
/// loop supplies the bounds from the measured control set.
#[must_use]
pub fn zone_for(x: f64, y: f64, bounds: (f64, f64, f64, f64)) -> PositionZone {
    let (min_x, min_y, max_x, max_y) = bounds;
    let third = |value: f64, min: f64, max: f64| -> u8 {
        if max <= min {
            return 1;
        }
        let ratio = ((value - min) / (max - min)).clamp(0.0, 1.0);
        if ratio < 1.0 / 3.0 {
            0
        } else if ratio < 2.0 / 3.0 {
            1
        } else {
            2
        }
    };
    match (third(x, min_x, max_x), third(y, min_y, max_y)) {
        (0, 0) => PositionZone::TopLeft,
        (1, 0) => PositionZone::TopCenter,
        (2, 0) => PositionZone::TopRight,
        (0, 1) => PositionZone::MiddleLeft,
        (1, 1) => PositionZone::MiddleCenter,
        (2, 1) => PositionZone::MiddleRight,
        (0, 2) => PositionZone::BottomLeft,
        (1, 2) => PositionZone::BottomCenter,
        _ => PositionZone::BottomRight,
    }
}

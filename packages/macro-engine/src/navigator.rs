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

    /// Harness entry point for one model turn: the element list, the
    /// optional screenshot, and the harness notes (stale-ref signals, loop
    /// nudges, a rejected `done`) accumulated since the last turn. Returns
    /// the proposed actions in order — normally exactly one; a model that
    /// over-answers with several is executed as a batch that halts on the
    /// first page change or failure — or `None` on any failure.
    ///
    /// The default drops the notes and delegates to
    /// [`PageNavigator::next_action_visual`], so existing implementers keep
    /// working; navigators that talk to a model override this to render
    /// the notes into the (fixed-shape) user message.
    fn next_turn(&self, turn: &NavigatorTurn<'_>) -> Option<Vec<PageAction>> {
        self.next_action_visual(
            turn.goal,
            turn.elements,
            turn.zones,
            turn.screenshot_jpeg_b64,
        )
        .map(|action| vec![action])
    }

    /// Whether a screenshot attached to the next turn would reach the
    /// model. The harness skips the capture (and journals why) when this
    /// is `false` — e.g. after a text-only model rejected the image part.
    /// The default is `true`, so existing implementers keep their shape.
    fn accepts_screenshots(&self) -> bool {
        true
    }

    /// Why the model rejected an attached screenshot, once it has:
    /// sanitized `http <status>: <provider error body>` (or a transport
    /// phrase) — never a key, never image bytes. `None` means no rejection
    /// happened or the navigator cannot say; the harness journals that
    /// explicitly instead of inventing a reason.
    fn vision_rejection(&self) -> Option<String> {
        None
    }
}

/// One model turn's input, as the harness hands it to
/// [`PageNavigator::next_turn`]. `zones[i]` describes `elements[i]`;
/// `notes` are plain-language harness signals for this turn only (never
/// page content, never credentials).
#[derive(Clone, Copy, Debug)]
pub struct NavigatorTurn<'a> {
    pub goal: &'a str,
    pub elements: &'a [AxElement],
    pub zones: &'a [Option<PositionZone>],
    pub screenshot_jpeg_b64: Option<&'a str>,
    pub notes: &'a [String],
}

// ---- Model-phase harness policy (pure; the loop lives in the executor) ----

/// Trailing window of normalized actions the loop detector counts
/// repeats over.
pub const LOOP_WINDOW: usize = 20;

/// Repeat counts (of one normalized action inside [`LOOP_WINDOW`]) at
/// which the model gets an escalating plain-language nudge.
pub const LOOP_NUDGE_REPEATS: [usize; 3] = [5, 8, 12];

/// Consecutive unchanged page observations (same host+path and the same
/// control set) before the page-stagnation nudge.
pub const STAGNATION_NUDGE_TURNS: usize = 3;

/// Most actions executed from one model reply. The vocabulary asks for
/// exactly one; anything past this is skipped and journaled.
pub const MAX_BATCH_ACTIONS: usize = 3;

/// Stale-ref re-reads per pass: a model that keeps naming ids the live
/// page no longer has stops acting after this many.
pub const MAX_STALE_REREADS: usize = 2;

/// Rejected `done` claims per pass: after this many the next `done` ends
/// the pass (the verification tail still runs).
pub const MAX_DONE_REJECTIONS: usize = 1;

/// Screenshots attached per pass — the bound on vision spend.
pub const MAX_SCREENSHOTS_PER_PASS: usize = 3;

/// A snapshot with at most this many actionable controls is treated as
/// canvas-heavy / opaque to the AX tree, and earns a screenshot.
pub const SPARSE_TREE_ELEMENTS: usize = 3;

/// Why a screenshot accompanies a model turn. Screenshots are on demand,
/// never every step: the model is stateless per turn, so no screenshot
/// history is ever resent either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenshotReason {
    /// First look at the page in this pass.
    FirstTurn,
    /// The turn after a rejected `done`: the model re-checks visually.
    VerifyTurn,
    /// The AX tree exposes almost nothing (canvas-heavy page).
    SparseTree,
}

impl std::fmt::Display for ScreenshotReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            ScreenshotReason::FirstTurn => "first-turn",
            ScreenshotReason::VerifyTurn => "verify-turn",
            ScreenshotReason::SparseTree => "sparse-tree",
        })
    }
}

/// Screenshot policy for one turn: `turn` is the 0-based turn index in
/// the pass, `verify_turn` marks the turn after a rejected `done`,
/// `snapshot_len` is the full actionable-control count, `sent` how many
/// screenshots this pass already attached. `None` means text-only.
#[must_use]
pub fn screenshot_reason(
    turn: usize,
    verify_turn: bool,
    snapshot_len: usize,
    sent: usize,
) -> Option<ScreenshotReason> {
    if sent >= MAX_SCREENSHOTS_PER_PASS {
        return None;
    }
    if verify_turn {
        Some(ScreenshotReason::VerifyTurn)
    } else if turn == 0 {
        Some(ScreenshotReason::FirstTurn)
    } else if snapshot_len <= SPARSE_TREE_ELEMENTS {
        Some(ScreenshotReason::SparseTree)
    } else {
        None
    }
}

/// A model action normalized for loop detection: clicks key on what the
/// user perceives (role + trimmed lowercase name), never the snapshot's
/// node id, so a re-rendered control still counts as the same action.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NormalizedAction {
    Click {
        role: String,
        name: String,
    },
    /// A click on an id the live page does not have.
    StaleClick,
    Done,
    GiveUp,
}

impl NormalizedAction {
    /// Normalized click on `element`.
    #[must_use]
    pub fn click(element: &AxElement) -> Self {
        Self::Click {
            role: element.role.clone(),
            name: element.name.trim().to_lowercase(),
        }
    }
}

/// Deterministic 64-bit hash of anything hashable (std's `SipHash` with
/// fixed keys: stable within a process, which is all a run needs).
fn stable_hash<T: std::hash::Hash + ?Sized>(value: &T) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Fingerprint of the live page for stagnation detection and batch
/// halting: host + path (query and fragment ignored) plus the ordered
/// role+name set of the actionable controls. Node ids are excluded — a
/// re-render that changes nothing visible is not a page change.
#[must_use]
pub fn page_fingerprint(url: Option<&url::Url>, elements: &[AxElement]) -> u64 {
    let location = url.map_or_else(String::new, |url| {
        format!("{}{}", url.host_str().unwrap_or(""), url.path())
    });
    let controls: Vec<(&str, String)> = elements
        .iter()
        .map(|element| (element.role.as_str(), element.name.trim().to_lowercase()))
        .collect();
    stable_hash(&(location, controls))
}

/// Loop detection over normalized action hashes plus page stagnation.
/// Pure: the loop feeds it every proposed action and every observation,
/// and turns the returned nudge (if any) into a harness note + journal
/// line. Nudges escalate at [`LOOP_NUDGE_REPEATS`]; the stagnation nudge
/// fires every [`STAGNATION_NUDGE_TURNS`] unchanged observations.
#[derive(Debug, Default)]
pub struct LoopDetector {
    window: std::collections::VecDeque<u64>,
    last_page: Option<u64>,
    stagnant: usize,
}

impl LoopDetector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one proposed action; `Some(nudge)` when its repeat count in
    /// the trailing window hits a [`LOOP_NUDGE_REPEATS`] threshold.
    pub fn record_action(&mut self, action: &NormalizedAction) -> Option<String> {
        let key = stable_hash(action);
        self.window.push_back(key);
        while self.window.len() > LOOP_WINDOW {
            self.window.pop_front();
        }
        let repeats = self.window.iter().filter(|seen| **seen == key).count();
        let level = LOOP_NUDGE_REPEATS
            .iter()
            .position(|threshold| *threshold == repeats)?;
        Some(match level {
            0 => format!(
                "You have proposed the same action {repeats} times and it has not advanced the goal. Try a different control."
            ),
            1 => format!(
                "Still repeating the same action ({repeats} times). Choose a clearly different control, or give_up if none can advance the goal."
            ),
            _ => format!(
                "The same action has been proposed {repeats} times. Stop repeating it: pick a different control or give_up."
            ),
        })
    }

    /// Record one page observation; `Some(nudge)` when the page has not
    /// changed for [`STAGNATION_NUDGE_TURNS`] consecutive observations.
    pub fn record_page(&mut self, fingerprint: u64) -> Option<String> {
        if self.last_page == Some(fingerprint) {
            self.stagnant += 1;
        } else {
            self.stagnant = 0;
        }
        self.last_page = Some(fingerprint);
        (self.stagnant > 0 && self.stagnant.is_multiple_of(STAGNATION_NUDGE_TURNS)).then(|| {
            format!(
                "The page has not changed over the last {} actions. Your clicks are having no visible effect; try a different control or give_up.",
                self.stagnant
            )
        })
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

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
//!   goal string and the element list (id, role, name, landmark).
//! * One bounded HTTP call per step; the loop still caps total steps.

use browser_driver::AxElement;

/// The closed action set a navigator may express. Deserialized directly
/// from the model's strict-JSON reply — any shape outside these three
/// variants fails to parse and the navigator declines.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PageAction {
    /// Click the element with this snapshot id.
    Click {
        target: i64,
    },
    /// The goal is already achieved on this page; nothing to click.
    Done,
    /// No element can advance the goal.
    GiveUp {
        reason: String,
    },
}

/// Decides the next in-page step toward `goal`, given the live snapshot's
/// actionable elements. Synchronous by contract — one bounded model call —
/// so the async pursuit loop invokes it on a blocking thread.
///
/// Returns `None` on any failure (transport, timeout, malformed JSON):
/// a stalled or confused model degrades to the honest miss, never a hang.
pub trait PageNavigator: Send + Sync {
    fn next_action(&self, goal: &str, elements: &[AxElement]) -> Option<PageAction>;
}

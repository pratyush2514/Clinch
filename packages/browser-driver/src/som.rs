#![deny(unsafe_code)]
//! Set-of-Marks overlay: numbered badges plus coordinate click targets.
//!
//! Instead of brittle selectors, visible controls get floating index badges;
//! a badge index maps directly to an absolute viewport click point. The
//! injected script creates its own overlay layer and never queries page
//! markup: no tag names, class names, or pattern matching appear here.

use crate::{
    BrowserError, CLICK_DWELL_MS, CursorEvent, CursorEventKind, FALLBACK_VIEWPORT, IO_TIMEOUT,
    ManagedBrowser, WAYPOINT_INTERVAL_MS, travel_waypoints,
};
use chromiumoxide::cdp::{
    browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
        DispatchMouseEventType, MouseButton,
    },
    js_protocol::runtime::EvaluateParams,
};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Upper bound on badges per overlay; matches the AX snapshot cap so one
/// snapshot always fits one overlay.
pub const MAX_MARKS: usize = 300;

/// One badged rectangle in CSS viewport pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mark {
    pub index: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Mark {
    pub fn validate(&self) -> Result<(), BrowserError> {
        for value in [self.x, self.y, self.width, self.height] {
            if !value.is_finite() {
                return Err(BrowserError::InvalidAction);
            }
        }
        if self.width <= 0.0 || self.height <= 0.0 {
            return Err(BrowserError::InvalidAction);
        }
        Ok(())
    }

    /// Absolute click target: the rectangle center.
    #[must_use]
    pub fn click_point(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

fn validate_marks(marks: &[Mark]) -> Result<(), BrowserError> {
    if marks.is_empty() || marks.len() > MAX_MARKS {
        return Err(BrowserError::InvalidAction);
    }
    for mark in marks {
        mark.validate()?;
    }
    Ok(())
}

/// Badge overlay script. `marks` are embedded as JSON; the script builds a
/// pointer-transparent layer with one red-circle index badge plus one outline
/// box per rectangle. Re-running replaces the previous layer, so overlays are
/// idempotent without ever selecting page content.
pub fn overlay_script(marks: &[Mark]) -> Result<String, BrowserError> {
    validate_marks(marks)?;
    let payload = serde_json::to_string(marks).map_err(|_| BrowserError::InvalidAction)?;
    Ok(format!(
        r"((marks) => {{
  if (window.__clinchMarkLayer) window.__clinchMarkLayer.remove();
  const layer = document.createElement('div');
  layer.style.cssText = 'position:fixed;inset:0;pointer-events:none;z-index:2147483647;';
  for (const m of marks) {{
    const box = document.createElement('div');
    box.style.cssText = 'position:absolute;left:' + m.x + 'px;top:' + m.y + 'px;width:' + m.width + 'px;height:' + m.height + 'px;border:2px solid #dc2626;box-sizing:border-box;';
    const badge = document.createElement('div');
    badge.textContent = String(m.index);
    badge.style.cssText = 'position:absolute;left:' + m.x + 'px;top:' + Math.max(0, m.y - 26) + 'px;min-width:24px;height:24px;border-radius:12px;background:#dc2626;color:#fff;font:600 13px/24px system-ui,sans-serif;text-align:center;padding:0 4px;';
    layer.append(box, badge);
  }}
  document.documentElement.appendChild(layer);
  window.__clinchMarkLayer = layer;
  window.__clinchClearMarks = () => {{ layer.remove(); window.__clinchMarkLayer = null; }};
}})({payload})",
    ))
}

pub const TEARDOWN_EXPRESSION: &str = "if(window.__clinchClearMarks){window.__clinchClearMarks();}";

impl ManagedBrowser {
    /// Render index badges for `marks` on the live target. An empty list
    /// clears any existing overlay instead of failing.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on invalid geometry or CDP failure.
    pub async fn show_marks(&self, marks: &[Mark]) -> Result<(), BrowserError> {
        let expression = if marks.is_empty() {
            TEARDOWN_EXPRESSION.to_owned()
        } else {
            overlay_script(marks)?
        };
        tokio::time::timeout(IO_TIMEOUT, self.page.evaluate(expression))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }

    /// Remove the badge overlay, if present.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on CDP failure or timeout.
    pub async fn clear_marks(&self) -> Result<(), BrowserError> {
        self.show_marks(&[]).await
    }
}

/// Trusted-input click sequence for one press at `(x, y)`: hover first so
/// the target sees the same `mousemove → mousedown → mouseup` ordering a
/// human produces, then press and release the left button. Some pages only
/// arm their handlers on hover (React synthetic events, `:hover`-gated
/// menus), so a press without a preceding move can land on a control that
/// never "sees" the pointer. Pure so the ordering and geometry are
/// hermetically testable; [`ManagedBrowser::click_mark`] only executes what
/// this builds.
///
/// # Errors
/// Returns [`BrowserError::InvalidAction`] when the CDP params fail to build.
pub fn click_event_sequence(x: f64, y: f64) -> Result<[DispatchMouseEventParams; 3], BrowserError> {
    let hover = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseMoved)
        .x(x)
        .y(y)
        .build()
        .map_err(|_| BrowserError::InvalidAction)?;
    let press = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .buttons(1)
        .click_count(1)
        .build()
        .map_err(|_| BrowserError::InvalidAction)?;
    let release = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .buttons(0)
        .build()
        .map_err(|_| BrowserError::InvalidAction)?;
    Ok([hover, press, release])
}

/// Page-side hit-test probe: `document.elementFromPoint` sampled at the
/// click coordinates. The badge overlay is pointer-transparent, so the
/// reported element is the page's own control under the click point, not
/// our layer. Returns `null` when nothing is under the point. Fields are
/// capped at 80 characters page-side; [`ClickHitTest::from_probe`] caps
/// again so any raw payload stays bounded.
const HIT_TEST_EXPRESSION: &str = "(() => {
  let el = document.elementFromPoint({x}, {y});
  if (!el) return null;
  for (let depth = 0; depth < 32 && el.shadowRoot; depth++) {
    const inner = el.shadowRoot.elementFromPoint({x}, {y});
    if (!inner || inner === el) break;
    el = inner;
  }
  const ROLES = ['button','link','menuitem','menuitemcheckbox','menuitemradio','tab','option','checkbox','radio','switch','combobox','textbox'];
  const implicitRole = (n) => {
    const t = n.tagName;
    if (t === 'A' && n.hasAttribute('href')) return 'link';
    if (t === 'BUTTON' || t === 'SUMMARY') return 'button';
    if (t === 'SELECT') return 'combobox';
    if (t === 'TEXTAREA') return 'textbox';
    if (t === 'INPUT') { const k = (n.type||'text').toLowerCase(); return (k === 'button' || k === 'submit' || k === 'reset' || k === 'image') ? 'button' : (k === 'checkbox' || k === 'radio') ? k : 'textbox'; }
    return '';
  };
  const interactiveRole = (n) => { const r = (n.getAttribute('role')||'').toLowerCase(); return ROLES.includes(r) ? r : implicitRole(n); };
  let target = null;
  for (let n = el, depth = 0; n && depth < 16; depth++) {
    if (n.nodeType === 1 && interactiveRole(n)) { target = n; break; }
    const root = n.getRootNode && n.getRootNode();
    n = n.parentElement || (root && root.host) || null;
  }
  const label = (n) => (n.getAttribute('aria-label')||n.getAttribute('title')||n.innerText||'').trim().slice(0,80);
  const within = target && target !== el ? {tag: target.tagName, role: interactiveRole(target), name: label(target)} : null;
  return {tag: el.tagName, role: el.getAttribute('role')||'', name: (el.getAttribute('aria-label')||el.innerText||'').trim().slice(0,80), idcls: ('#'+(el.id||'')+' .'+(el.className||'').toString().split(/\\s+/).join('.')).slice(0,80), within};
})()";

/// Substitute the click point into the probe const's `{x}`/`{y}`
/// placeholders. Pure so the substitution stays unit-testable; `format!`
/// needs a literal format string, hence plain `replace`.
pub fn hit_test_expression(x: f64, y: f64) -> String {
    HIT_TEST_EXPRESSION
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
}

/// Cap on probe field lengths, in characters. Mirrors the page-side
/// `.slice(0, 80)`; Rust counts chars where JS counts UTF-16 units, which
/// is close enough for a diagnostic.
const HIT_TEST_FIELD_CAP: usize = 80;

/// Cap on an unavailable report's reason, in characters: CDP error text
/// is bounded so one failure can't flood the journal.
const HIT_TEST_REASON_CAP: usize = 160;

/// What the page itself reports under a click point: the element
/// `document.elementFromPoint` finds at the coordinates the trusted CDP
/// input events landed on. Pure diagnostic — the click never depends on
/// it. Field names are a contract: the macro-engine journals
/// [`ClickHitTest::journal_line`].
#[derive(Clone, Debug)]
pub struct ClickHitTest {
    pub x: f64,
    pub y: f64,
    pub available: bool,
    pub tag: String,
    pub role: String,
    pub name: String,
    /// Why the probe could not report (empty when `available`): the
    /// journal says what failed instead of a bare "unavailable".
    pub reason: String,
    /// Provenance note rendered after the line (empty for the ordinary
    /// post-click report): set when the pre-click probe stood in for a
    /// failed post-click probe.
    pub note: String,
    /// The nearest interactive ancestor (`tag`, `role`, `name`) of the hit
    /// element, crossing shadow hosts, when the hit element is not itself
    /// interactive — e.g. the SVG icon inside an "Open inbox" link. The
    /// press activates that ancestor, so a match there is a match. Empty
    /// tag when absent.
    pub within_tag: String,
    pub within_role: String,
    pub within_name: String,
}

impl ClickHitTest {
    /// The never-available report: every probe failure (eval error, timeout,
    /// page exception, `null` hit, unparsable payload) funnels here so the
    /// diagnostic can never fail the click it follows. `reason` names the
    /// failure for the journal.
    #[must_use]
    pub fn unavailable(x: f64, y: f64, reason: &str) -> Self {
        ClickHitTest {
            x,
            y,
            available: false,
            tag: String::new(),
            role: String::new(),
            name: String::new(),
            reason: reason.chars().take(HIT_TEST_REASON_CAP).collect(),
            note: String::new(),
            within_tag: String::new(),
            within_role: String::new(),
            within_name: String::new(),
        }
    }

    /// Combine the post-click report (`self`) with the pre-click probe at
    /// the same point. A click that navigates tears the page down under
    /// the post-click probe; the pre-click probe (the element the press
    /// was dispatched onto) then stands in, flagged as such. An available
    /// post-click report wins unchanged, so the ordinary line keeps its
    /// exact format. When both fail, both reasons are kept.
    #[must_use]
    pub fn with_pre_click_fallback(self, pre: ClickHitTest) -> Self {
        if self.available {
            return self;
        }
        if pre.available {
            return ClickHitTest {
                note: format!("pre-click probe; post-click probe failed: {}", self.reason),
                ..pre
            };
        }
        let reason = format!("post-click: {}; pre-click: {}", self.reason, pre.reason);
        ClickHitTest { reason, ..self }
    }

    /// [`ClickHitTest::with_pre_click_fallback`], plus: when the click
    /// `navigated` (the page URL changed between the two probes), the
    /// post-click probe describes the next page, not the click target, so
    /// an available pre-click report wins, flagged as such. Without
    /// navigation this is exactly [`ClickHitTest::with_pre_click_fallback`].
    #[must_use]
    pub fn resolve_with_pre_click(self, pre: ClickHitTest, navigated: bool) -> Self {
        if navigated && pre.available {
            return ClickHitTest {
                note: "pre-click probe; the click navigated".to_owned(),
                ..pre
            };
        }
        self.with_pre_click_fallback(pre)
    }

    /// Parse a `Runtime.evaluate` probe payload into a report. Public so the
    /// parsing stays hermetically testable without a live page. A payload
    /// with no usable tag degrades to an unavailable report rather than an
    /// error, and every field is re-capped at [`HIT_TEST_FIELD_CAP`].
    #[must_use]
    pub fn from_probe(x: f64, y: f64, value: &serde_json::Value) -> Self {
        fn field(value: &serde_json::Value, key: &str) -> String {
            value
                .get(key)
                .and_then(|field| field.as_str())
                .unwrap_or("")
                .chars()
                .take(HIT_TEST_FIELD_CAP)
                .collect()
        }
        let tag = field(value, "tag");
        if tag.is_empty() {
            return Self::unavailable(
                x,
                y,
                if value.is_null() {
                    "no element at the click point"
                } else {
                    "probe returned no tag"
                },
            );
        }
        ClickHitTest {
            x,
            y,
            available: true,
            tag,
            role: field(value, "role"),
            name: field(value, "name"),
            reason: String::new(),
            note: String::new(),
            within_tag: value
                .get("within")
                .map(|within| field(within, "tag"))
                .unwrap_or_default(),
            within_role: value
                .get("within")
                .map(|within| field(within, "role"))
                .unwrap_or_default(),
            within_name: value
                .get("within")
                .map(|within| field(within, "name"))
                .unwrap_or_default(),
        }
    }

    /// One journal line for the hit test. Pure and hermetically testable:
    /// - available: `click_hit_test: (X, Y) -> <tag> role=<role> name="<name>"`
    ///   (coordinates rounded to integers; an empty role renders as `-`)
    /// - plus ` MISMATCH(expected role="<er>" name~="<en>")` when neither the
    ///   role equals the expected role (case-insensitively) nor the name
    ///   contains the expected name (case-insensitively)
    /// - plus ` [inside <tag> role=<role> name="<name>"]` when the hit
    ///   element sits inside an interactive ancestor; either one matching
    ///   the expectation is a match (the press activates the ancestor)
    /// - plus ` [<note>]` when the pre-click probe stood in
    /// - unavailable: `click_hit_test: (X, Y) unavailable (<reason>)`
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn journal_line(&self, expected_role: &str, expected_name: &str) -> String {
        if !self.available {
            let reason = if self.reason.is_empty() {
                "probe reported no reason"
            } else {
                self.reason.as_str()
            };
            return format!(
                "click_hit_test: ({}, {}) unavailable ({reason})",
                self.x.round() as i64,
                self.y.round() as i64,
            );
        }
        let role = if self.role.is_empty() {
            "-"
        } else {
            self.role.as_str()
        };
        // Coordinates are validated-finite viewport points (or probe inputs);
        // `round() as i64` saturates rather than traps on extremes.
        let mut line = format!(
            "click_hit_test: ({}, {}) -> {} role={} name=\"{}\"",
            self.x.round() as i64,
            self.y.round() as i64,
            self.tag,
            role,
            self.name,
        );
        if !self.within_tag.is_empty() {
            let within_role = if self.within_role.is_empty() {
                "-"
            } else {
                self.within_role.as_str()
            };
            let _ = write!(
                line,
                " [inside {} role={within_role} name=\"{}\"]",
                self.within_tag, self.within_name
            );
        }
        let expected = expected_name.to_lowercase();
        let matches = |role: &str, name: &str| {
            role.eq_ignore_ascii_case(expected_role) || name.to_lowercase().contains(&expected)
        };
        let hit_matches = matches(&self.role, &self.name);
        let within_matches =
            !self.within_tag.is_empty() && matches(&self.within_role, &self.within_name);
        if !(hit_matches || within_matches) {
            let _ = write!(
                line,
                " MISMATCH(expected role=\"{expected_role}\" name~=\"{expected_name}\")"
            );
        }
        if !self.note.is_empty() {
            let _ = write!(line, " [{}]", self.note);
        }
        line
    }
}

impl ManagedBrowser {
    /// Click the center of `mark` with a human-like hover → press → release
    /// through trusted CDP input events (never a synthetic DOM click).
    ///
    /// The pointer first glides from its last known landing point through
    /// intermediate `MouseMoved` dispatches (≈40px apart, capped, ~18ms
    /// between them), then the strict hover → press → release sequence runs
    /// with a constant 100ms dwell between press and release. The glide is
    /// best-effort — a failed waypoint aborts it and the strict click still
    /// runs — and the first move of a session (no last position) skips it.
    ///
    /// Runs the exact same input as [`ManagedBrowser::click_mark_reported`];
    /// it just discards the post-click hit-test report.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on invalid geometry, CDP failure, or timeout.
    pub async fn click_mark(&self, mark: &Mark) -> Result<(), BrowserError> {
        self.click_mark_reported(mark).await.map(|_| ())
    }

    /// Click the center of `mark` exactly like [`ManagedBrowser::click_mark`],
    /// then best-effort probe `document.elementFromPoint` at the click point:
    /// the report says which element the page itself has under the
    /// coordinates the trusted input events landed on. The probe never fails
    /// the click — any probe failure yields an unavailable report instead.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on invalid geometry, CDP failure, or timeout
    /// of the click itself; the post-click probe is best-effort.
    pub async fn click_mark_reported(&self, mark: &Mark) -> Result<ClickHitTest, BrowserError> {
        // Pre-click probe at the exact point the press will land: read-only
        // and before any input, so the input sequence itself is unchanged.
        // It stands in only when the post-click probe fails (a navigating
        // click tears the page down under it).
        let pre = if mark.validate().is_ok() {
            let (px, py) = mark.click_point();
            Some(self.hit_test(px, py).await)
        } else {
            None
        };
        let url_before = self.current_url().await.ok().flatten();
        let (x, y) = self.click_mark_core(mark).await?;
        let post = self.hit_test(x, y).await;
        // A click that navigated leaves the post-click probe reading the
        // NEXT page (seen live: the bell click reported the notifications
        // page's H1 as a MISMATCH). The pre-click probe is then the only
        // truthful record of what the press landed on.
        let url_after = self.current_url().await.ok().flatten();
        let navigated =
            matches!((&url_before, &url_after), (Some(before), Some(after)) if before != after);
        Ok(match pre {
            Some(pre) => post.resolve_with_pre_click(pre, navigated),
            None => post,
        })
    }

    /// Best-effort `document.elementFromPoint` probe at `(x, y)`. Never
    /// fails: an eval error, a timeout, a page exception, a `null` hit, or
    /// an unparsable payload all degrade to an unavailable report, so the
    /// diagnostic can never break the click it follows.
    async fn hit_test(&self, x: f64, y: f64) -> ClickHitTest {
        let expression = hit_test_expression(x, y);
        let Ok(params) = EvaluateParams::builder()
            .expression(expression)
            .return_by_value(true)
            .build()
        else {
            return ClickHitTest::unavailable(x, y, "probe expression rejected");
        };
        let evaluation = match tokio::time::timeout(IO_TIMEOUT, self.page.execute(params)).await {
            Ok(Ok(evaluation)) => evaluation,
            // The CDP error text (e.g. a destroyed execution context after a
            // navigation) is the useful part; it carries no page content.
            Ok(Err(error)) => {
                return ClickHitTest::unavailable(x, y, &format!("probe failed: {error}"));
            }
            Err(_) => return ClickHitTest::unavailable(x, y, "probe timed out"),
        };
        // `execute` wraps the CDP response: CommandResponse.result is the
        // EvaluateReturns, whose .result is the RemoteObject.
        let returns = evaluation.result;
        if returns.exception_details.is_some() {
            return ClickHitTest::unavailable(x, y, "page exception in probe");
        }
        match returns.result.value {
            Some(value) if !value.is_null() => ClickHitTest::from_probe(x, y, &value),
            _ => ClickHitTest::unavailable(x, y, "no element at the click point"),
        }
    }

    /// The click itself, shared by [`ManagedBrowser::click_mark`] and
    /// [`ManagedBrowser::click_mark_reported`] so both run byte-identical
    /// input: waypoint glide from the last landing point, hover → press →
    /// release with a 100ms press dwell, cursor events, and the
    /// `last_cursor` update. Returns the click point for post-click
    /// diagnostics.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on invalid geometry, CDP failure, or timeout.
    async fn click_mark_core(&self, mark: &Mark) -> Result<(f64, f64), BrowserError> {
        mark.validate()?;
        let (x, y) = mark.click_point();
        // The live layout viewport is the exact coordinate space of the CDP
        // input events below; fall back to the launch size if the query fails.
        let viewport = self.viewport_size().await.unwrap_or(FALLBACK_VIEWPORT);
        let session_id = self.cursor_session.load(Ordering::Relaxed);
        // Travel the pointer from its last landing point so the overlay
        // glides instead of teleporting. Best-effort: a failed waypoint
        // stops the travel and the real click sequence (strict) runs next.
        // Copy the position out of the lock first: the guard must not be
        // held across the dispatches below.
        let last: Option<(f64, f64)> = self.last_cursor.lock().map_or(None, |guard| *guard);
        if let Some(from) = last {
            for (wx, wy) in travel_waypoints(from, (x, y)) {
                let moved = DispatchMouseEventParams::builder()
                    .r#type(DispatchMouseEventType::MouseMoved)
                    .x(wx)
                    .y(wy)
                    .build()
                    .map_err(|_| BrowserError::InvalidAction)?;
                let dispatched = matches!(
                    tokio::time::timeout(IO_TIMEOUT, self.page.execute(moved)).await,
                    Ok(Ok(_))
                );
                if !dispatched {
                    break;
                }
                // Emit only after the dispatch succeeded: the cursor marks
                // where input actually landed, never where it was merely
                // aimed.
                self.emit_cursor(CursorEvent::new(
                    CursorEventKind::Move,
                    wx,
                    wy,
                    viewport,
                    session_id,
                ));
                tokio::time::sleep(Duration::from_millis(WAYPOINT_INTERVAL_MS)).await;
            }
        }
        // Zip the human-like phases onto the CDP events so the cursor overlay
        // shows exactly what the page just received, in order.
        let phases = [
            CursorEventKind::Move,
            CursorEventKind::Press,
            CursorEventKind::Release,
        ];
        for (event, kind) in click_event_sequence(x, y)?.into_iter().zip(phases) {
            tokio::time::timeout(IO_TIMEOUT, self.page.execute(event))
                .await
                .map_err(|_| BrowserError::Timeout)?
                .map_err(|_| BrowserError::Connection)?;
            // Emit only after the dispatch succeeded: the cursor marks where
            // input actually landed, never where it was merely aimed.
            self.emit_cursor(CursorEvent::new(kind, x, y, viewport, session_id));
            // Deterministic dwell between press and release: a human holds
            // the button briefly. Constant and bounded — no jitter, so
            // replays stay reproducible.
            if kind == CursorEventKind::Press {
                tokio::time::sleep(Duration::from_millis(CLICK_DWELL_MS)).await;
            }
        }
        // The pointer now rests on the click target: the next click travels
        // from here.
        if let Ok(mut guard) = self.last_cursor.lock() {
            *guard = Some((x, y));
        }
        Ok((x, y))
    }

    /// Dismiss any open popup layer with an Escape keypress (down + up).
    /// Best-effort: with nothing open it is a harmless no-op.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on CDP failure or timeout.
    pub async fn press_escape(&self) -> Result<(), BrowserError> {
        for event_type in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let key = DispatchKeyEventParams::builder()
                .r#type(event_type)
                .key("Escape")
                .code("Escape")
                .windows_virtual_key_code(27)
                .build()
                .map_err(|_| BrowserError::InvalidAction)?;
            tokio::time::timeout(IO_TIMEOUT, self.page.execute(key))
                .await
                .map_err(|_| BrowserError::Timeout)?
                .map_err(|_| BrowserError::Connection)?;
        }
        Ok(())
    }

    /// Render badges and capture the marked viewport in one step, for
    /// planners that decide from the annotated screenshot.
    ///
    /// # Errors
    /// Returns [`BrowserError`] on invalid geometry, CDP failure, or timeout.
    pub async fn marked_viewport(&self, marks: &[Mark]) -> Result<crate::Viewport, BrowserError> {
        self.show_marks(marks).await?;
        self.viewport().await
    }
}

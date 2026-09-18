#![deny(unsafe_code)]
//! Interactive CDP visual element picker.
//!
//! The overlay is injected with `Page.addScriptToEvaluateOnNewDocument` so it
//! survives portal navigations during a picking session. Hover paints the
//! mandated highlight box; click streams a rank-ordered selector chain back
//! over `Runtime.addBinding` (`__clinch_element_picked__`). Only structural
//! metadata crosses the binding — never credentials, cookie values, or full
//! page text.

use crate::{BrowserError, IO_TIMEOUT, ManagedBrowser};
use serde::{Deserialize, Serialize};

/// CDP binding name shared by the injected script and [`ManagedBrowser::await_pick`].
pub const PICKER_BINDING: &str = "__clinch_element_picked__";

/// Bounding rectangle in CSS pixels, as reported by `getBoundingClientRect`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PickerRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Structural description of one user-picked element.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PickedElement {
    pub tag: String,
    pub selectors: Vec<String>,
    pub rect: PickerRect,
    /// Short visible-text snippet for the workflow card preview (≤200 chars).
    pub text: String,
}

impl PickedElement {
    /// Validate a binding payload before it reaches the workflow planner.
    ///
    /// # Errors
    /// Rejects empty/oversized selectors, non-finite geometry, or text that
    /// exceeds the preview budget.
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.tag.trim().is_empty() || self.tag.len() > 32 {
            return Err(BrowserError::Picker);
        }
        if self.selectors.is_empty() || self.selectors.len() > 8 {
            return Err(BrowserError::Picker);
        }
        for selector in &self.selectors {
            if selector.trim().is_empty() || selector.len() > 2048 {
                return Err(BrowserError::Picker);
            }
        }
        let r = &self.rect;
        for v in [r.x, r.y, r.width, r.height] {
            if !v.is_finite() {
                return Err(BrowserError::Picker);
            }
        }
        if r.width <= 0.0 || r.height <= 0.0 || r.width > 10_000.0 || r.height > 10_000.0 {
            return Err(BrowserError::Picker);
        }
        if self.text.len() > 500 {
            return Err(BrowserError::Picker);
        }
        Ok(())
    }

    /// Strongest selector first, mirroring the injected script's ranking.
    #[must_use]
    pub fn primary(&self) -> Option<&str> {
        self.selectors.first().map(String::as_str)
    }
}

/// Parse one `Runtime.bindingCalled` payload into a validated [`PickedElement`].
///
/// # Errors
/// Returns [`BrowserError::Picker`] for malformed JSON or failed validation.
pub fn parse_binding_payload(payload: &str) -> Result<PickedElement, BrowserError> {
    if payload.len() > 8192 {
        return Err(BrowserError::Picker);
    }
    let picked: PickedElement = serde_json::from_str(payload).map_err(|_| BrowserError::Picker)?;
    picked.validate()?;
    Ok(picked)
}

/// Pure-Rust mirror of the injected script's selector ranking, kept testable
/// without a live browser: `data-testid` → `aria-label` → unique `#id` →
/// concise relative path.
#[must_use]
pub fn rank_selectors_from_attrs(
    tag: &str,
    id: Option<&str>,
    testid: Option<&str>,
    aria_label: Option<&str>,
    classes: &[String],
    type_attr: Option<&str>,
) -> Vec<String> {
    let mut selectors = Vec::with_capacity(4);
    if let Some(testid) = testid.filter(|v| !v.trim().is_empty() && v.len() <= 256) {
        selectors.push(format!("[data-testid=\"{}\"]", escape_attr(testid)));
    }
    if let Some(aria) = aria_label.filter(|v| !v.trim().is_empty() && v.len() <= 256) {
        selectors.push(format!("[aria-label=\"{}\"]", escape_attr(aria)));
    }
    if let Some(id) = id.filter(|v| !v.trim().is_empty() && v.len() <= 256) {
        selectors.push(format!("#{}", escape_ident(id)));
    }
    let mut path = tag.to_ascii_lowercase();
    for class in classes.iter().take(2).filter(|c| {
        !c.trim().is_empty()
            && c.len() <= 64
            && c.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }) {
        path.push('.');
        path.push_str(class);
    }
    if let Some(t) = type_attr.filter(|v| !v.trim().is_empty() && v.len() <= 32) {
        use std::fmt::Write as _;
        let _ = write!(path, "[type=\"{}\"]", escape_attr(t));
    }
    selectors.push(path);
    selectors
}

fn escape_attr(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_ident(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_string()
            } else {
                format!("\\{c}")
            }
        })
        .collect()
}

/// Overlay script: hover highlight + prioritized selector engine + binding stream.
///
/// Kept dependency-free (no frameworks) so it runs on any portal page.
/// `__clinchPickerTeardown` removes all listeners and restores outlines.
const PICKER_SCRIPT: &str = r#"
(() => {
  if (window.__clinchPickerActive) return;
  window.__clinchPickerActive = true;
  let last = null; let lastOutline = ''; let lastBg = '';
  const cssEscape = (v) => (window.CSS && CSS.escape ? CSS.escape(v) : String(v).replace(/[^a-zA-Z0-9_-]/g, '\\$&'));
  const attrEscape = (v) => String(v).replace(/\\/g, '\\\\').replace(/\"/g, '\\\"');
  function rank(el) {
    const out = [];
    const testid = el.getAttribute && el.getAttribute('data-testid');
    if (testid && testid.trim()) out.push('[data-testid=\"' + attrEscape(testid.trim()) + '\"]');
    const aria = el.getAttribute && el.getAttribute('aria-label');
    if (aria && aria.trim()) out.push('[aria-label=\"' + attrEscape(aria.trim()) + '\"]');
    if (el.id && el.id.trim() && document.querySelectorAll('#' + cssEscape(el.id.trim())).length === 1)
      out.push('#' + cssEscape(el.id.trim()));
    let path = el.localName || el.tagName.toLowerCase();
    const classes = (el.className && el.className.baseVal !== undefined ? '' : String(el.className || '')).split(/\\s+/).filter(Boolean).slice(0, 2);
    for (const c of classes) if (/^[a-zA-Z0-9_-]{1,64}$/.test(c)) path += '.' + c;
    const t = el.getAttribute && el.getAttribute('type');
    if (t && t.trim() && /^(submit|button|search|date|month|number)$/.test(t.trim())) path += '[type=\"' + t.trim() + '\"]';
    out.push(path);
    return out.slice(0, 4);
  }
  function onOver(e) {
    const el = e.target;
    if (!el || el === document.documentElement || el === document.body) return;
    if (last && last !== el) { last.style.outline = lastOutline; last.style.background = lastBg; }
    if (last !== el) { lastOutline = el.style.outline; lastBg = el.style.background; last = el; }
    el.style.outline = '2px solid #3b82f6';
    el.style.background = 'rgba(59, 130, 246, 0.1)';
  }
  function onOut(e) {
    const el = e.target;
    if (el && el === last) { el.style.outline = lastOutline; el.style.background = lastBg; last = null; }
  }
  function onClick(e) {
    const el = e.target;
    if (!el || el === document.documentElement || el === document.body) return;
    e.preventDefault(); e.stopPropagation();
    const r = el.getBoundingClientRect();
    const text = ((el.innerText || el.textContent || '').trim().slice(0, 200));
    const payload = JSON.stringify({ tag: (el.tagName || 'div').toLowerCase(), selectors: rank(el),
      rect: { x: r.x, y: r.y, width: r.width, height: r.height }, text });
    try { window.__clinch_element_picked__(payload); } catch (_) {}
  }
  document.addEventListener('pointerover', onOver, true);
  document.addEventListener('pointerout', onOut, true);
  document.addEventListener('click', onClick, true);
  window.__clinchPickerTeardown = () => {
    window.__clinchPickerActive = false;
    document.removeEventListener('pointerover', onOver, true);
    document.removeEventListener('pointerout', onOut, true);
    document.removeEventListener('click', onClick, true);
    if (last) { last.style.outline = lastOutline; last.style.background = lastBg; last = null; }
    delete window.__clinchPickerTeardown;
  };
})();"#;

impl ManagedBrowser {
    /// Inject the picker overlay into the live target and arm the binding.
    /// Safe to call repeatedly; the script self-guards via `__clinchPickerActive`.
    ///
    /// # Errors
    /// Returns [`BrowserError`] when CDP registration fails.
    pub async fn enable_picker(&self) -> Result<(), BrowserError> {
        use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
        use chromiumoxide::cdp::js_protocol::runtime::AddBindingParams;
        tokio::time::timeout(
            IO_TIMEOUT,
            self.page.execute(AddBindingParams::new(PICKER_BINDING)),
        )
        .await
        .map_err(|_| BrowserError::Timeout)?
        .map_err(|_| BrowserError::Connection)?;
        let mut script = AddScriptToEvaluateOnNewDocumentParams::new(PICKER_SCRIPT);
        script.run_immediately = Some(true);
        tokio::time::timeout(IO_TIMEOUT, self.page.execute(script))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        tokio::time::timeout(IO_TIMEOUT, self.page.evaluate(PICKER_SCRIPT))
            .await
            .map_err(|_| BrowserError::Timeout)?
            .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }

    /// Remove the overlay and restore any highlighted outline.
    ///
    /// # Errors
    /// Returns [`BrowserError`] when the teardown evaluation fails.
    pub async fn disable_picker(&self) -> Result<(), BrowserError> {
        tokio::time::timeout(
            IO_TIMEOUT,
            self.page
                .evaluate("window.__clinchPickerTeardown && window.__clinchPickerTeardown()"),
        )
        .await
        .map_err(|_| BrowserError::Timeout)?
        .map_err(|_| BrowserError::Connection)?;
        Ok(())
    }

    /// Wait for the next `__clinch_element_picked__` binding call.
    ///
    /// # Errors
    /// Returns [`BrowserError::Picker`] on timeout, closed stream, or an
    /// invalid payload. Call [`Self::disable_picker`] afterwards.
    pub async fn await_pick(
        &self,
        timeout: std::time::Duration,
    ) -> Result<PickedElement, BrowserError> {
        use chromiumoxide::cdp::js_protocol::runtime::EventBindingCalled;
        use futures::StreamExt;
        let mut events = self
            .page
            .event_listener::<EventBindingCalled>()
            .await
            .map_err(|_| BrowserError::Connection)?;
        let event = tokio::time::timeout(timeout, async {
            while let Some(event) = events.next().await {
                if event.name == PICKER_BINDING {
                    return Some(event.payload.clone());
                }
            }
            None
        })
        .await
        .map_err(|_| BrowserError::Picker)?
        .ok_or(BrowserError::Picker)?;
        parse_binding_payload(&event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_payload_roundtrip_and_validation() -> Result<(), Box<dyn std::error::Error>> {
        let picked = PickedElement {
            tag: "a".into(),
            selectors: vec!["[data-testid=\"invoice-link\"]".into(), "a.invoice".into()],
            rect: PickerRect {
                x: 8.0,
                y: 16.0,
                width: 120.0,
                height: 24.0,
            },
            text: "Download invoice".into(),
        };
        let payload = serde_json::to_string(&picked)?;
        assert_eq!(parse_binding_payload(&payload)?, picked);
        assert_eq!(picked.primary(), Some("[data-testid=\"invoice-link\"]"));
        Ok(())
    }

    #[test]
    fn binding_payload_rejects_shape_and_bounds_violations() {
        // Wrong envelope: binding sends exactly one JSON object.
        assert!(parse_binding_payload("not-json").is_err());
        assert!(parse_binding_payload("{\"tag\":\"a\"}").is_err());
        // Oversized selector / text budgets fail closed.
        let bad = PickedElement {
            tag: "a".into(),
            selectors: vec!["x".repeat(3000)],
            rect: PickerRect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            text: String::new(),
        };
        assert!(bad.validate().is_err());
        let bad_rect = PickedElement {
            tag: "a".into(),
            selectors: vec!["a".into()],
            rect: PickerRect {
                x: f64::NAN,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            text: String::new(),
        };
        assert!(bad_rect.validate().is_err());
        let empty = PickedElement {
            tag: "a".into(),
            selectors: Vec::new(),
            rect: PickerRect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            text: String::new(),
        };
        assert!(empty.validate().is_err());
        assert!(parse_binding_payload(&"x".repeat(9000)).is_err());
    }

    #[test]
    fn selector_ranking_prefers_stable_hooks() {
        let ranked = rank_selectors_from_attrs(
            "BUTTON",
            Some("submit-btn"),
            Some("invoice-submit"),
            Some("Submit invoice"),
            &["primary-btn".into(), "extra".into()],
            Some("submit"),
        );
        assert_eq!(ranked[0], "[data-testid=\"invoice-submit\"]");
        assert_eq!(ranked[1], "[aria-label=\"Submit invoice\"]");
        assert_eq!(ranked[2], "#submit-btn");
        assert!(ranked[3].starts_with("button."));
        assert!(ranked[3].contains("[type=\"submit\"]"));

        let minimal = rank_selectors_from_attrs("a", None, None, None, &[], None);
        assert_eq!(minimal, vec!["a".to_string()]);
    }

    #[test]
    fn picker_script_contains_mandated_contract() {
        assert!(PICKER_SCRIPT.contains("__clinch_element_picked__"));
        assert!(PICKER_SCRIPT.contains("outline"));
        assert!(PICKER_SCRIPT.contains("2px solid #3b82f6"));
        assert!(PICKER_SCRIPT.contains("rgba(59, 130, 246, 0.1)"));
        assert!(PICKER_SCRIPT.contains("data-testid"));
        assert!(PICKER_SCRIPT.contains("aria-label"));
        assert!(PICKER_SCRIPT.contains("__clinchPickerTeardown"));
        assert_eq!(PICKER_BINDING, "__clinch_element_picked__");
    }
}

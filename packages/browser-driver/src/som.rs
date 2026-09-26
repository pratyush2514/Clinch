#![deny(unsafe_code)]
//! Set-of-Marks overlay: numbered badges plus coordinate click targets.
//!
//! Instead of brittle selectors, visible controls get floating index badges;
//! a badge index maps directly to an absolute viewport click point. The
//! injected script creates its own overlay layer and never queries page
//! markup: no tag names, class names, or pattern matching appear here.

use crate::{
    BrowserError, CursorEvent, CursorEventKind, FALLBACK_VIEWPORT, IO_TIMEOUT, ManagedBrowser,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    MouseButton,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

/// Upper bound on badges per overlay; matches the AX snapshot cap so one
/// snapshot always fits one overlay.
const MAX_MARKS: usize = 300;

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
    fn validate(&self) -> Result<(), BrowserError> {
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
fn overlay_script(marks: &[Mark]) -> Result<String, BrowserError> {
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

const TEARDOWN_EXPRESSION: &str = "if(window.__clinchClearMarks){window.__clinchClearMarks();}";

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

impl ManagedBrowser {
    /// Click the center of `mark` with a human-like hover → press → release
    /// through trusted CDP input events (never a synthetic DOM click).
    ///
    /// # Errors
    /// Returns [`BrowserError`] on invalid geometry, CDP failure, or timeout.
    pub async fn click_mark(&self, mark: &Mark) -> Result<(), BrowserError> {
        mark.validate()?;
        let (x, y) = mark.click_point();
        // The live layout viewport is the exact coordinate space of the CDP
        // input events below; fall back to the launch size if the query fails.
        let viewport = self.viewport_size().await.unwrap_or(FALLBACK_VIEWPORT);
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
            self.emit_cursor(CursorEvent::new(
                kind,
                x,
                y,
                viewport,
                self.cursor_session.load(Ordering::Relaxed),
            ));
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(index: usize) -> Mark {
        Mark {
            index,
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 40.0,
        }
    }

    #[test]
    fn click_point_is_the_rectangle_center() {
        assert_eq!(mark(3).click_point(), (60.0, 40.0));
    }

    #[test]
    fn validation_rejects_degenerate_geometry() {
        assert!(mark(0).validate().is_ok());
        for broken in [
            Mark {
                x: f64::NAN,
                ..mark(0)
            },
            Mark {
                width: 0.0,
                ..mark(0)
            },
            Mark {
                height: -5.0,
                ..mark(0)
            },
        ] {
            assert!(broken.validate().is_err());
        }
        assert!(overlay_script(&[]).is_err());
        assert!(overlay_script(&vec![mark(0); MAX_MARKS + 1]).is_err());
    }

    #[test]
    fn overlay_embeds_badges_and_teardown() -> Result<(), Box<dyn std::error::Error>> {
        let script = overlay_script(&[mark(0), mark(7)])?;
        assert!(script.contains("\"index\":0"));
        assert!(script.contains("\"index\":7"));
        assert!(script.contains("#dc2626"));
        assert!(script.contains("__clinchClearMarks"));
        assert!(script.contains("__clinchMarkLayer"));
        // No page-content selection: the script only creates its own layer.
        assert!(!script.contains("querySelector"));
        assert_eq!(
            TEARDOWN_EXPRESSION,
            "if(window.__clinchClearMarks){window.__clinchClearMarks();}"
        );
        Ok(())
    }
}

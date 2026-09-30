//! Integration tests for `browser_driver::som`.
//!
//! Moved out of `src/` so the main source stays test-free.

use browser_driver::som::*;

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
fn hit_test_expression_substitutes_coordinates() {
    let expr = hit_test_expression(120.5, 80.25);
    assert!(expr.contains("document.elementFromPoint(120.5, 80.25)"));
    // The const uses single braces (it is substituted with `replace`,
    // not `format!`); doubled braces would be a JS syntax error.
    assert!(!expr.contains("{{"));
    assert!(!expr.contains("}}"));
    assert!(expr.contains("return {tag:"));
    assert!(expr.contains("if (!el) return null;"));
    // Shadow-DOM controls (web-component headers) resolve to the deepest
    // element, not the host.
    assert!(expr.contains("el.shadowRoot.elementFromPoint(120.5, 80.25)"));
    // The nearest interactive ancestor rides along as `within`.
    assert!(expr.contains("within};"));
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

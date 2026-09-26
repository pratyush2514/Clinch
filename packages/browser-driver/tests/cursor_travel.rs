#![deny(unsafe_code)]
//! Cursor travel and click-dwell contract: the overlay must glide through
//! evenly spaced waypoints instead of teleporting between click targets,
//! and a press must hold briefly before the release. Hermetic: only the
//! waypoint math and the dwell constant are asserted, no browser is
//! launched. Example shapes only — no real sites.

use browser_driver::{
    CLICK_DWELL_MS, MAX_WAYPOINTS, WAYPOINT_INTERVAL_MS, WAYPOINT_SPACING_PX, travel_waypoints,
};

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

#[test]
fn travel_progress_is_monotonic_toward_the_target() {
    let target = (960.0, 540.0);
    let waypoints = travel_waypoints((60.0, 40.0), target);
    assert!(!waypoints.is_empty(), "a real travel must glide");
    // Every waypoint is strictly closer to the target than the last: the
    // cursor never doubles back, so the glide reads as one smooth motion.
    let mut remaining: Vec<f64> = waypoints.iter().map(|&p| distance(p, target)).collect();
    remaining.push(0.0); // the hover dispatch covers the target exactly
    for pair in remaining.windows(2) {
        assert!(
            pair[0] > pair[1],
            "travel stalled or reversed: {pair:?} toward {target:?}"
        );
    }
}

#[test]
fn far_travel_is_capped_at_max_waypoints() {
    // A diagonal far longer than 12 waypoints worth of spacing.
    let waypoints = travel_waypoints((0.0, 0.0), (5000.0, 5000.0));
    assert_eq!(waypoints.len(), MAX_WAYPOINTS);
    assert_eq!(waypoints.len(), 12);
}

#[test]
fn waypoint_count_scales_with_distance() {
    // 200px at ~40px spacing: 4 interior points (the 200px mark would
    // coincide with the target, which the hover covers).
    let waypoints = travel_waypoints((0.0, 0.0), (200.0, 0.0));
    assert_eq!(waypoints.len(), 4);
    // 100px at ~40px spacing: 2 interior points.
    let waypoints = travel_waypoints((0.0, 0.0), (100.0, 0.0));
    assert_eq!(waypoints.len(), 2);
}

#[test]
fn from_approximately_equal_to_target_yields_no_waypoints() {
    // A repeated click on the same spot must not replay a glide.
    assert!(travel_waypoints((960.0, 540.0), (960.0, 540.0)).is_empty());
    assert!(travel_waypoints((1.0, 2.0), (1.0, 2.0 + f64::EPSILON / 2.0)).is_empty());
}

#[test]
fn waypoints_exclude_both_endpoints() {
    let from = (60.0, 40.0);
    // 447px: short enough to avoid the waypoint cap, so the last point is
    // within one hop of the target.
    let target = (460.0, 240.0);
    let waypoints = travel_waypoints(from, target);
    assert!(!waypoints.is_empty());
    // `from` is the resting point (no redundant move), `to` is covered by
    // the final hover dispatch.
    assert!(waypoints.iter().all(|&p| p != from));
    assert!(waypoints.iter().all(|&p| p != target));
    // The last waypoint leaves a gap the hover covers exactly: the final
    // hover dispatch must not be redundant either.
    let last = waypoints[waypoints.len() - 1];
    assert!(
        distance(last, target) < WAYPOINT_SPACING_PX + 1.0,
        "last waypoint {last:?} is not within one hop of the target"
    );
}

#[test]
fn exact_multiple_of_spacing_excludes_the_target() {
    // 80px is an exact multiple of the 40px spacing: without the guard the
    // second point would coincide with `to`, duplicating the hover.
    let waypoints = travel_waypoints((0.0, 0.0), (80.0, 0.0));
    assert_eq!(waypoints.len(), 1);
    assert_eq!(waypoints[0], (40.0, 0.0));
}

#[test]
fn waypoints_are_evenly_spaced_and_collinear() {
    let from = (100.0, 200.0);
    let target = (500.0, 200.0); // 400px: 9 interior points (400 is an exact multiple of the spacing)
    let waypoints = travel_waypoints(from, target);
    assert_eq!(waypoints.len(), 9);
    let mut points = vec![from];
    points.extend_from_slice(&waypoints);
    for pair in points.windows(2) {
        // Horizontal travel: straight line, constant spacing.
        assert!((pair[0].1 - pair[1].1).abs() < 1e-9, "not collinear");
        let gap = distance(pair[0], pair[1]);
        assert!(
            (gap - WAYPOINT_SPACING_PX).abs() < 1e-9,
            "gap {gap} is not the ~{WAYPOINT_SPACING_PX}px spacing"
        );
    }
    // First waypoint sits ~40px out from the resting point.
    assert!((distance(from, waypoints[0]) - WAYPOINT_SPACING_PX).abs() < 1e-9);
}

#[test]
fn diagonal_travel_lands_on_the_segment() {
    let from = (10.0, 20.0);
    let target = (410.0, 320.0);
    let waypoints = travel_waypoints(from, target);
    assert!(!waypoints.is_empty());
    let total = distance(from, target);
    for &p in &waypoints {
        // On the straight line between the endpoints (triangle equality).
        let via = distance(from, p) + distance(p, target);
        assert!(
            (via - total).abs() < 1e-9,
            "waypoint {p:?} is off the from→target line"
        );
    }
}

#[test]
fn click_dwell_is_a_bounded_constant() {
    // The press→release hold is deterministic: constant 100ms, no jitter,
    // so click replays are reproducible. Read through a closure so the
    // "bounded" assertions are not constant expressions.
    let dwell = || CLICK_DWELL_MS;
    assert_eq!(dwell(), 100);
    assert!(dwell() > 0);
    assert!(dwell() < 1_000, "dwell must stay a brief hold");
}

#[test]
fn waypoint_interval_is_a_bounded_constant() {
    // Read through a closure so the bound assertions are not constant
    // expressions (see above).
    let interval = || WAYPOINT_INTERVAL_MS;
    assert_eq!(interval(), 18);
    assert!(interval() > 0);
    assert!(interval() < 1_000, "travel must stay a short burst");
}

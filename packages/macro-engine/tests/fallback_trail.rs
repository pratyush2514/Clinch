use macro_engine::executor::fallback_tried_lines;

const FALLBACK: &str =
    "log_out: UI path missed; cleared 4 session cookies for example.com; verifier: signed-out";

#[test]
fn trail_keeps_order_with_fallback_line_last() {
    let ui_miss = "logout_ui_bounded: gear1_click -> menu_verify; \
                   logout_ui_bounded: opener_retry; \
                   logout_ui_bounded: model_pass -> fallback";
    assert_eq!(
        fallback_tried_lines(ui_miss, FALLBACK),
        vec![
            "logout_ui_bounded: gear1_click -> menu_verify".to_string(),
            "logout_ui_bounded: opener_retry".to_string(),
            "logout_ui_bounded: model_pass -> fallback".to_string(),
            FALLBACK.to_string(),
        ]
    );
}

#[test]
fn empty_ui_miss_yields_only_fallback_line() {
    assert_eq!(
        fallback_tried_lines("", FALLBACK),
        vec![FALLBACK.to_string()]
    );
}

#[test]
fn single_segment_ui_miss_works() {
    assert_eq!(
        fallback_tried_lines("logout_ui_bounded: gear1_click -> fallback", FALLBACK),
        vec![
            "logout_ui_bounded: gear1_click -> fallback".to_string(),
            FALLBACK.to_string(),
        ]
    );
}

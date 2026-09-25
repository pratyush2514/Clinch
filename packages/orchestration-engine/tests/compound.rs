#![deny(unsafe_code)]
//! `split_compound`: deterministic compound-prompt splitting.

use orchestration_engine::split_compound;

fn owned(segments: &[&str]) -> Vec<String> {
    segments
        .iter()
        .map(|segment| (*segment).to_owned())
        .collect()
}

#[test]
fn splits_two_verb_led_segments() {
    // The contract example: multi-word verbs match on the segment's
    // leading words ("log out me from the reddit" is verb-led).
    assert_eq!(
        split_compound("log out me from the reddit and re-open the reddit"),
        Some(owned(&["log out me from the reddit", "re-open the reddit"]))
    );
}

#[test]
fn single_action_prompt_is_not_compound() {
    // One action, no separator: the caller keeps single-prompt behavior.
    assert_eq!(split_compound("open reddit for me"), None);
}

#[test]
fn non_verb_led_segments_are_not_compound() {
    // Two segments, but neither is an action: not a compound prompt.
    assert_eq!(split_compound("peanut butter and jelly"), None);
}

#[test]
fn and_then_wins_over_and() {
    // Longest separator first: " and then " splits before " and " can.
    assert_eq!(
        split_compound("open reddit and then open my profile on reddit"),
        Some(owned(&["open reddit", "open my profile on reddit"]))
    );
}

#[test]
fn separators_are_case_insensitive_and_preserve_case() {
    // Matching runs on the lowered copy; the returned segments keep the
    // prompt's original casing.
    assert_eq!(
        split_compound("Open X AND then open Y"),
        Some(owned(&["Open X", "open Y"]))
    );
}

#[test]
fn three_segments_split_in_order() {
    assert_eq!(
        split_compound("open reddit then open my profile and then close the tab"),
        Some(owned(&["open reddit", "open my profile", "close the tab"]))
    );
}

#[test]
fn trailing_separator_leaves_one_segment() {
    // The empty tail is dropped: one segment left, so not compound.
    assert_eq!(split_compound("open reddit and"), None);
}

#[test]
fn one_non_verb_led_segment_vetoes_the_whole_prompt() {
    // Every segment must be verb-led: one noun phrase poisons the chain.
    assert_eq!(split_compound("open reddit and the weather"), None);
}

#[test]
fn word_boundaries_keep_substrings_from_splitting() {
    // "stand" contains "and", but only the standalone separator splits:
    // two verb-led segments survive the boundary check.
    assert_eq!(
        split_compound("open the stand and close the door"),
        Some(owned(&["open the stand", "close the door"]))
    );
    // "wand" is a whole word, so it does split — but the verb-led gate
    // still rejects the chain.
    assert_eq!(split_compound("wand then open the settings"), None);
    // A verb that is only a prefix of a longer word is not verb-led.
    assert_eq!(split_compound("opener reddit and opener profile"), None);
}

#[test]
fn multi_word_verbs_lead_their_segments() {
    assert_eq!(
        split_compound("take me to the settings and then sign me out"),
        Some(owned(&["take me to the settings", "sign me out"]))
    );
    assert_eq!(
        split_compound("go to reddit and navigate to my profile"),
        Some(owned(&["go to reddit", "navigate to my profile"]))
    );
}

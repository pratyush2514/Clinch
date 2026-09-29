//! Verb spec table: closed noun → spec mapping. Identity nouns
//! (`profile`, `account`) resolve to the account-home spec, settings nouns
//! (`setting(s)`, `preference(s)`) to the settings spec, and the log-out
//! phrases to the log-out spec; every other artifact noun keeps the generic
//! noun-hunt path.
use orchestration_engine::{VerbKind, VerifierKind, spec_for_noun, verb_specs};

fn spec_kind(noun: &str) -> Option<VerbKind> {
    spec_for_noun(noun).map(|spec| spec.kind)
}

#[test]
fn identity_nouns_map_to_account_home_spec() {
    for noun in ["profile", "account", " Profile ", "ACCOUNT"] {
        let spec = spec_for_noun(noun).unwrap_or_else(|| panic!("noun maps: {noun}"));
        assert_eq!(spec.kind, VerbKind::AccountHome, "noun: {noun}");
        assert_eq!(
            spec.verifier,
            VerifierKind::IdentityEvidence,
            "noun: {noun}"
        );
    }
}

#[test]
fn settings_nouns_map_to_settings_spec() {
    for noun in [
        "setting",
        "settings",
        "preference",
        "preferences",
        " Setting ",
        "PREFERENCES",
    ] {
        let spec = spec_for_noun(noun).unwrap_or_else(|| panic!("noun maps: {noun}"));
        assert_eq!(spec.kind, VerbKind::Settings, "noun: {noun}");
        assert_eq!(
            spec.verifier,
            VerifierKind::UrlPathTokens(&["setting", "preference"]),
            "noun: {noun}"
        );
    }
    // The funnel passes the canonical noun; the follow-up detector
    // passes the stemmed noun. Both reach the same row.
    assert_eq!(spec_kind("settings"), Some(VerbKind::Settings));
    assert_eq!(spec_kind("setting"), Some(VerbKind::Settings));
}

#[test]
fn logout_phrases_map_to_logout_spec_case_insensitively() {
    for noun in [
        "log out", "log off", "sign out", "LOG OUT", "Log Off", "Sign Out",
    ] {
        let spec = spec_for_noun(noun).unwrap_or_else(|| panic!("noun maps: {noun}"));
        assert_eq!(spec.kind, VerbKind::LogOut, "noun: {noun}");
        assert_eq!(spec.verifier, VerifierKind::AuthSignedOut, "noun: {noun}");
    }
}

#[test]
fn other_nouns_stay_generic() {
    assert_eq!(spec_for_noun("pricing"), None);
    assert_eq!(spec_for_noun("messages"), None);
    assert_eq!(spec_for_noun(""), None);
    // Multi-word phrases never map: the grammar hands the dispatcher a
    // single artifact noun (or a verb phrase for log-out), so a phrase
    // here is a caller bug.
    assert_eq!(spec_for_noun("my profile page"), None);
    // Near-misses stay generic: the table is exact, never substring.
    assert_eq!(spec_for_noun("profiles"), None);
    assert_eq!(spec_for_noun("logout"), None);
}

#[test]
fn verb_table_has_four_rows_with_generic_vocabulary() {
    let specs = verb_specs();
    assert_eq!(specs.len(), 4);
    // Generic words only — no site names, selectors, or URLs in the
    // match vocabulary.
    for spec in specs {
        assert!(
            !spec.vocabulary.is_empty(),
            "verb {:?} has empty vocabulary",
            spec.kind
        );
        for word in spec.vocabulary {
            assert!(
                !word.contains('.') && !word.contains('/') && !word.contains('#'),
                "vocabulary word is not a generic word: {word}"
            );
        }
    }
    // Every VerbKind is covered, exactly once.
    for kind in [
        VerbKind::AccountHome,
        VerbKind::Settings,
        VerbKind::LogOut,
        VerbKind::Notifications,
    ] {
        assert_eq!(
            specs.iter().filter(|spec| spec.kind == kind).count(),
            1,
            "kind: {kind:?}"
        );
    }
}

#[test]
fn verb_kind_key_is_stable() {
    // The key is the identity-memory row key: it must not drift.
    assert_eq!(VerbKind::AccountHome.as_str(), "account_home");
    assert_eq!(VerbKind::Settings.as_str(), "settings");
    assert_eq!(VerbKind::LogOut.as_str(), "log_out");
}

#[test]
fn for_kind_covers_every_verb() {
    use orchestration_engine::VerbSpec;
    assert_eq!(
        VerbSpec::for_kind(VerbKind::AccountHome).kind,
        VerbKind::AccountHome
    );
    assert_eq!(
        VerbSpec::for_kind(VerbKind::Settings).kind,
        VerbKind::Settings
    );
    assert_eq!(VerbSpec::for_kind(VerbKind::LogOut).kind, VerbKind::LogOut);
}

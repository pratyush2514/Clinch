//! Goal-class mapping: closed noun → goal class table. Identity nouns
//! (`profile`, `account`) route to the account-home worker; every other
//! artifact noun keeps the generic noun-hunt path.
use orchestration_engine::{GoalClass, goal_class_for};

#[test]
fn identity_nouns_map_to_account_home() {
    assert_eq!(goal_class_for("profile"), Some(GoalClass::AccountHome));
    assert_eq!(goal_class_for("account"), Some(GoalClass::AccountHome));
    assert_eq!(goal_class_for(" Profile "), Some(GoalClass::AccountHome));
    assert_eq!(goal_class_for("ACCOUNT"), Some(GoalClass::AccountHome));
}

#[test]
fn other_nouns_stay_generic() {
    assert_eq!(goal_class_for("settings"), None);
    assert_eq!(goal_class_for("pricing"), None);
    assert_eq!(goal_class_for(""), None);
    // Multi-word phrases never map: the grammar hands the dispatcher a
    // single stemmed artifact noun, so a phrase here is a caller bug.
    assert_eq!(goal_class_for("my profile page"), None);
}

#[test]
fn goal_class_key_is_stable() {
    // The key is the identity-memory row key: it must not drift.
    assert_eq!(GoalClass::AccountHome.as_str(), "account_home");
}

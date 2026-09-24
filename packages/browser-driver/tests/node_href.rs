//! `href_from_attributes`: the pure scan behind `ManagedBrowser::node_href`.
//! CDP flattens a node's attributes into alternating name/value strings;
//! this is the decision, without the CDP call.
use browser_driver::href_from_attributes;

fn attrs(pairs: &[(&str, &str)]) -> Vec<String> {
    pairs
        .iter()
        .flat_map(|(name, value)| [(*name).to_owned(), (*value).to_owned()])
        .collect()
}

#[test]
fn finds_href_among_other_attributes() {
    let attributes = attrs(&[
        ("class", "avatar"),
        ("href", "https://www.example.com/user/someuser/"),
        ("target", "_self"),
    ]);
    assert_eq!(
        href_from_attributes(Some(&attributes)).as_deref(),
        Some("https://www.example.com/user/someuser/")
    );
}

#[test]
fn attribute_name_match_is_case_insensitive() {
    let attributes = attrs(&[("HREF", "/user/someuser/")]);
    assert_eq!(
        href_from_attributes(Some(&attributes)).as_deref(),
        Some("/user/someuser/")
    );
}

#[test]
fn trims_whitespace_around_value() {
    let attributes = attrs(&[("href", "  /user/someuser/  ")]);
    assert_eq!(
        href_from_attributes(Some(&attributes)).as_deref(),
        Some("/user/someuser/")
    );
}

#[test]
fn returns_none_when_absent() {
    let attributes = attrs(&[("class", "avatar"), ("title", "menu")]);
    assert_eq!(href_from_attributes(Some(&attributes)), None);
}

#[test]
fn returns_none_for_empty_value() {
    let attributes = attrs(&[("href", "   ")]);
    assert_eq!(href_from_attributes(Some(&attributes)), None);
}

#[test]
fn returns_none_for_missing_attributes() {
    assert_eq!(href_from_attributes(None), None);
}

#[test]
fn ignores_odd_trailing_element() {
    // A malformed flattening with a dangling name never panics and
    // never yields a value.
    let mut attributes = attrs(&[("class", "avatar")]);
    attributes.push("href".to_owned());
    assert_eq!(href_from_attributes(Some(&attributes)), None);
}

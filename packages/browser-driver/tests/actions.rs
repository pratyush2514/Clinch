//! Integration tests for `browser_driver::actions`.
//!
//! Moved out of `src/` so the main source stays test-free.

use browser_driver::actions::*;
use url::Url;
#[test]
fn blob_downloads_are_scoped_to_the_portal_without_allowing_blob_navigation()
-> Result<(), Box<dyn std::error::Error>> {
    let origin = Url::parse("https://github.com/settings/files")?;
    let expected = Url::parse("https://github.com/report")?;
    let blob = Url::parse("blob:https://github.com/fixture-guid")?;
    assert!(validate_download_url(&blob, &origin).is_ok());
    assert!(matches_download_url(blob.as_str(), &expected, &origin));
    assert!(matches_download_url(expected.as_str(), &expected, &origin));
    assert!(Action::Navigate { url: blob }.validate(&origin).is_err());
    for invalid in [
        "blob:https://evil.example/guid",
        "blob:null/guid",
        "blob:https://user:secret@github.com/guid",
        "javascript:alert(1)",
        "data:application/pdf,abc",
        "https://github.com/unexpected",
        "blob:http://github.com/guid",
    ] {
        assert!(!matches_download_url(invalid, &expected, &origin));
    }
    Ok(())
}

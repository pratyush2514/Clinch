#![deny(unsafe_code)]
//! Curated portal route table: the single place static deep links live.
//!
//! Each entry maps `(portal, intent class)` to the portal's canonical route
//! for that class, with a comment citing why the route is canonical. The
//! table stays tiny by policy — unknown portals and classes resolve to
//! `None` and fall through to the next resolver tier. Every entry is
//! re-validated by `url_policy` on use, so a bad edit fails closed instead
//! of navigating somewhere unexpected.

/// Static portal routes: `(portal token, intent class, canonical URL)`.
/// Portal tokens match the `PORTALS` detector vocabulary exactly.
const PORTAL_ROUTES: &[(&str, &str, &str)] = &[
    // GitHub payment history: https://github.com/account/billing/history
    // serves the "Billing History" page with the per-payment invoice table
    // (receipt/invoice downloads live there; the page itself sits behind
    // sign-in, matching the app's authenticated-session model).
    (
        "github",
        "invoices",
        "https://github.com/account/billing/history",
    ),
    (
        "github",
        "invoice",
        "https://github.com/account/billing/history",
    ),
    (
        "github",
        "billing",
        "https://github.com/account/billing/history",
    ),
];

/// Portal tokens the detector recognizes. Must stay in sync with the
/// portal column of [`PORTAL_ROUTES`]; exact lowercase tokens only, no
/// aliases — unknown spellings fall through instead of guessing.
pub(crate) const PORTALS: &[&str] = &["github"];

/// Singular stem for tolerant matching: one trailing `s` off longer tokens,
/// never `ss`. Mirrors the intent parser's `singular_stem` so a singular
/// `invoice` and a plural `invoices` resolve to the same class.
fn stem_intent_class(token: &str) -> String {
    if token.len() > 3 && token.ends_with('s') && !token.ends_with("ss") {
        token[..token.len() - 1].to_owned()
    } else {
        token.to_owned()
    }
}

/// Normalize an intent class for lookup: lowercase, trim, then stem.
fn normalize_intent_class(raw: &str) -> String {
    stem_intent_class(raw.trim().to_ascii_lowercase().as_str())
}

/// Look up the canonical route for a portal and intent class.
/// Case-, whitespace-, and stem-insensitive on the intent class
/// (`invoice` matches `invoices`); anything unlisted returns `None`.
#[must_use]
pub fn portal_route(portal: &str, intent_class: &str) -> Option<&'static str> {
    let portal = portal.trim().to_ascii_lowercase();
    let class = normalize_intent_class(intent_class);
    PORTAL_ROUTES
        .iter()
        .find(|(entry_portal, entry_class, _)| {
            *entry_portal == portal && normalize_intent_class(entry_class) == class
        })
        .map(|(_, _, url)| *url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_route_returns_billing_history_for_github_invoices() {
        // Intent reads as "billing history"; the canonical route serving it
        // is the payment-history invoice table (see table comment).
        // Both singular and plural noun variants must resolve (ad-hoc
        // parsing emits singular `invoice` via `extract_primary_noun`).
        assert_eq!(
            portal_route("github", "invoices"),
            Some("https://github.com/account/billing/history")
        );
        assert_eq!(
            portal_route("github", "invoice"),
            Some("https://github.com/account/billing/history")
        );
        assert_eq!(
            portal_route("github", "billing"),
            Some("https://github.com/account/billing/history")
        );
        assert_eq!(
            portal_route(" GitHub ", "INVOICE"),
            Some("https://github.com/account/billing/history")
        );
        assert_eq!(
            portal_route("github", " Invoices "),
            Some("https://github.com/account/billing/history")
        );
        assert_eq!(
            portal_route("github", "BILLING"),
            Some("https://github.com/account/billing/history")
        );
        assert_eq!(portal_route("github", "receipts"), None);
        assert_eq!(portal_route("stripe", "invoices"), None);
        assert_eq!(portal_route("", ""), None);
    }
}

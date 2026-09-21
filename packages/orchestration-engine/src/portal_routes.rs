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
    ("github", "invoices", "https://github.com/account/billing/history"),
    ("github", "invoice", "https://github.com/account/billing/history"),
];

/// Portal tokens the detector recognizes. Must stay in sync with the
/// portal column of [`PORTAL_ROUTES`]; exact lowercase tokens only, no
/// aliases — unknown spellings fall through instead of guessing.
pub(crate) const PORTALS: &[&str] = &["github"];

/// Look up the canonical route for a portal and intent class.
/// Case- and whitespace-insensitive; anything unlisted returns `None`.
#[must_use]
pub fn portal_route(portal: &str, intent_class: &str) -> Option<&'static str> {
    let portal = portal.trim().to_ascii_lowercase();
    let class = intent_class.trim().to_ascii_lowercase();
    PORTAL_ROUTES
        .iter()
        .find(|(entry_portal, entry_class, _)| *entry_portal == portal && *entry_class == class)
        .map(|(_, _, url)| *url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_route_returns_billing_history_for_github_invoices() {
        // Intent reads as "billing history"; the canonical route serving it
        // is the documented personal billing page (see table comment).
        assert_eq!(
            portal_route("github", "invoices"),
            Some("https://github.com/settings/billing")
        );
        assert_eq!(
            portal_route(" GitHub ", "INVOICE"),
            Some("https://github.com/settings/billing")
        );
        assert_eq!(portal_route("github", "receipts"), None);
        assert_eq!(portal_route("stripe", "invoices"), None);
        assert_eq!(portal_route("", ""), None);
    }
}

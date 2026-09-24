//! Goal classes for in-page follow-ups: *kinds of page tasks*, never sites
//! or procedures. A goal class tells the dispatcher which worker owns the
//! artifact noun ("take me to my account page" is a different hunt than
//! "take me to the pricing page"). The mapping is a closed table over
//! artifact nouns — no site names, no URL templates, no selectors.

/// The goal classes Clinch understands for in-page follow-ups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalClass {
    /// "my profile", "my account": the signed-in user's own page on the
    /// current origin. Pursued via the header identity chrome, verified by
    /// page-revealed identity evidence, remembered per origin for repeats.
    AccountHome,
}

impl GoalClass {
    /// Stable key used for identity-memory rows and journal lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GoalClass::AccountHome => "account_home",
        }
    }
}

/// Closed noun → goal class mapping. Only identity nouns qualify: every
/// other artifact noun keeps the generic noun-hunt path, so a new goal
/// class is an explicit product decision, not an emergent match.
#[must_use]
pub fn goal_class_for(noun: &str) -> Option<GoalClass> {
    match noun.trim().to_lowercase().as_str() {
        "profile" | "account" => Some(GoalClass::AccountHome),
        _ => None,
    }
}

//! Deterministic parser contract: funnel steps 1–2 (aside stripping, then
//! slot splitting).
//!
//! Raw prompts carry conversational filler that used to pollute the grammar
//! slots: `"i want you to open X for me"` parsed `target="you"`,
//! `"…make sure ill do the login first"` parsed `target="login"`, and
//! `"open reddit for me i want to log in"` parsed site `"log"` —
//! `"i want to log in"` is a login-policy aside, never a destination.
//! This module strips that filler first (login-intent phrases included),
//! then splits the cleaned prompt into exactly two slots:
//!
//! * `site_slot`: an opaque string for the dynamic routing ladder — never
//!   checked against a site list here, so there is no hardcoded routing
//!   and no site-specific anything;
//! * `object_slot`: one of a closed set of object nouns (profile, settings,
//!   notifications, messages) for the existing in-page machinery.
//!
//! Everything else is discarded; it never reaches a resolver.
//! Deterministic only: no LLM, no network, no model calls.

use crate::intent_resolver::{
    OPEN_VERBS, STOPWORDS, is_action_verb, parse_grammar, singular_stem, tokens,
};

/// Conversational filler stripped before grammar parsing, matched
/// case-insensitively against the raw prompt on whole-word boundaries.
///
/// * `"can you"`, `"could you"`, `"please"`, `"for me"` — politeness filler.
/// * `"i want you to"`, `"i'd like you to"` (also `"id like you to"`) —
///   framing that would otherwise donate `you` as the target noun.
/// * `"make sure"` — handled by [`ASIDE_CLAUSE`]: strips the phrase **and
///   everything after it in its clause**, so `"make sure ill do the login
///   first"` is one aside and `login` never lands in a noun slot.
const ASIDE_PHRASES: &[&str] = &[
    "i'd like you to",
    "id like you to",
    "i want you to",
    "could you",
    "can you",
    "please",
    "for me",
];

/// Clause-introducer stripped together with its whole tail: `"make sure"`
/// and everything after it to the end of the prompt is a single aside.
const ASIDE_CLAUSE: &str = "make sure";

/// Markers that make an aside a login-policy hint: the user announced they
/// will log in themselves, so the funnel must not treat `login` as content.
const LOGIN_MARKERS: &[&str] = &["login", "log in", "log-in", "signin", "sign in", "sign-in"];

/// Login-intent phrases stripped as asides, matched case-insensitively on
/// whole-word boundaries, longest first. The user announced they will log
/// in themselves — a login-policy hint, never a destination. The long
/// clauses precede the bare words they contain (`"i want to log in"`
/// before `"log in"`) so the whole intent reads as one aside instead of
/// leaking `log`/`login` into the grammar slots (live: `"open reddit for
/// me i want to log in"` parsed site `"log"`).
const LOGIN_ASIDE_PHRASES: &[&str] = &[
    "i'd like to log in",
    "id like to log in",
    "i want to log in",
    "i want to login",
    "i want to sign in",
    "i will log in",
    "i will login",
    "i will sign in",
    "log in",
    "log-in",
    "login",
    "sign in",
    "sign-in",
    "signin",
];

/// Single tokens that are login actions, never destinations. Belt and
/// braces behind [`LOGIN_ASIDE_PHRASES`]: any surviving login word is
/// refused as a site slot, so `"log"` / `"login"` can never route to
/// `log.com` even when no surrounding clause was stripped.
fn is_login_word(token: &str) -> bool {
    matches!(token, "log" | "login" | "log-in" | "signin" | "sign-in")
}

/// Pronouns that can never be a site: a site slot names a destination, and
/// a destination is never `you`.
const PRONOUNS: &[&str] = &[
    "i",
    "me",
    "my",
    "mine",
    "myself",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
    "he",
    "him",
    "his",
    "himself",
    "she",
    "her",
    "hers",
    "herself",
    "it",
    "its",
    "itself",
    "we",
    "us",
    "our",
    "ours",
    "ourselves",
    "they",
    "them",
    "their",
    "theirs",
    "themselves",
];

/// The result of [`strip_asides`]: the cleaned prompt, the asides found in
/// order of appearance (for journaling), and the login-policy hint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AsideInfo {
    /// The prompt with every aside span removed and whitespace collapsed.
    pub cleaned: String,
    /// Asides found, in order of appearance. Fixed phrases are reported in
    /// their documented canonical form; the `"make sure"` clause is
    /// reported as its full captured text.
    pub asides: Vec<String>,
    /// True when any aside mentions login / log in / sign in.
    pub login_hint: bool,
}

/// Strip conversational asides from a raw prompt, deterministically.
///
/// Matching is case-insensitive against the raw prompt with whole-word
/// boundaries (`"pleased"` does not match `"please"`). `"make sure"`
/// captures its whole tail as one aside. Returns the cleaned prompt plus
/// the asides found, in order, plus the login-policy hint.
#[must_use]
pub fn strip_asides(prompt: &str) -> AsideInfo {
    // Char vectors with a 1:1 index mapping, so offsets never drift on
    // non-ASCII input even though matching runs on the lowered copy.
    let original: Vec<char> = prompt.chars().collect();
    let lowered: Vec<char> = original
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();

    // (start, end, reported text)
    let mut spans: Vec<(usize, usize, String)> = Vec::new();

    // "make sure" first: it eats its whole tail, which may contain other
    // asides — the whole clause is one aside by definition.
    let tail_cut = if let Some(start) = find_phrase(&lowered, 0, ASIDE_CLAUSE) {
        let text: String = original[start..].iter().collect();
        let text = text.trim().to_string();
        spans.push((start, original.len(), text));
        start
    } else {
        original.len()
    };

    for phrase in ASIDE_PHRASES {
        let mut from = 0;
        while let Some(start) = find_phrase(&lowered[..tail_cut], from, phrase) {
            let end = start + phrase.chars().count();
            spans.push((start, end, (*phrase).to_string()));
            from = end;
        }
    }

    // Login-intent phrases, longest first: `"i want to log in"` wins over
    // the `"log in"` it contains, so an already-recorded span is never
    // re-reported. Scoped to `..tail_cut` like the filler phrases, so a
    // `"make sure … login …"` clause keeps its single-aside shape.
    for phrase in LOGIN_ASIDE_PHRASES {
        let mut from = 0;
        while let Some(start) = find_phrase(&lowered[..tail_cut], from, phrase) {
            let end = start + phrase.chars().count();
            let covered = spans
                .iter()
                .any(|(kept_start, kept_end, _)| start < *kept_end && *kept_start < end);
            if !covered {
                spans.push((start, end, (*phrase).to_string()));
            }
            from = end;
        }
    }
    spans.sort_by_key(|(start, _, _)| *start);

    let login_hint = spans.iter().any(|(_, _, text)| {
        let text = text.to_lowercase();
        LOGIN_MARKERS.iter().any(|marker| text.contains(marker))
    });

    let mut removed = vec![false; original.len()];
    for (start, end, _) in &spans {
        for slot in removed.iter_mut().take(*end).skip(*start) {
            *slot = true;
        }
    }
    let kept: String = original
        .iter()
        .enumerate()
        .filter(|(index, _)| !removed[*index])
        .map(|(_, c)| *c)
        .collect();
    let cleaned = kept.split_whitespace().collect::<Vec<_>>().join(" ");

    AsideInfo {
        cleaned,
        asides: spans.into_iter().map(|(_, _, text)| text).collect(),
        login_hint,
    }
}

/// Find `phrase` in `haystack` at or after `from`, on whole-word
/// boundaries: the characters immediately before and after the match must
/// not be alphanumeric. Returns the char offset of the match.
fn find_phrase(haystack: &[char], from: usize, phrase: &str) -> Option<usize> {
    let needle: Vec<char> = phrase.chars().collect();
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let mut index = from;
    while index + needle.len() <= haystack.len() {
        let before_ok = index == 0 || !haystack[index - 1].is_alphanumeric();
        let after = index + needle.len();
        let after_ok = after == haystack.len() || !haystack[after].is_alphanumeric();
        if before_ok && after_ok && haystack[index..after] == needle[..] {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Closed set of object nouns the funnel recognizes, mapped from whole
/// stemmed tokens: profile|account → [`ObjectClass::AccountHome`],
/// setting(s)|preference(s) → [`ObjectClass::Settings`],
/// notification(s) → [`ObjectClass::Notifications`],
/// message(s) → [`ObjectClass::Messages`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectClass {
    AccountHome,
    Settings,
    Notifications,
    Messages,
}

/// Canonical noun for the existing in-page machinery: `"profile"`,
/// `"settings"`, `"notifications"`, or `"messages"`.
#[must_use]
pub const fn object_noun(class: ObjectClass) -> &'static str {
    match class {
        ObjectClass::AccountHome => "profile",
        ObjectClass::Settings => "settings",
        ObjectClass::Notifications => "notifications",
        ObjectClass::Messages => "messages",
    }
}

/// Classify one stemmed token into an [`ObjectClass`], if it is an object
/// noun. Stemming uses the shared [`singular_stem`], so `settings` and
/// `preferences` classify exactly as their singulars do.
fn classify_object_token(stemmed: &str) -> Option<ObjectClass> {
    match stemmed {
        "profile" | "account" => Some(ObjectClass::AccountHome),
        "setting" | "preference" => Some(ObjectClass::Settings),
        "notification" => Some(ObjectClass::Notifications),
        "message" => Some(ObjectClass::Messages),
        _ => None,
    }
}

/// The two funnel slots plus the aside journal and login-policy hint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunnelSlots {
    /// Opaque destination string for the dynamic routing ladder. Never a
    /// pronoun (`you` is explicitly denied), never a stopword, and never a
    /// substring match — one-character recovery fires only on a standalone
    /// single-character token.
    pub site_slot: Option<String>,
    /// Whether the site slot came from the grammar's prepositional
    /// `site_context` (`"open settings on reddit"`) rather than a bare
    /// `target_noun` (`"open settings"`). The object-noun exclusion only
    /// demotes the latter.
    pub site_from_context: bool,
    /// Recognized object noun, or `None` when the prompt names none.
    pub object_slot: Option<ObjectClass>,
    /// Asides stripped from the raw prompt, in order of appearance.
    pub asides: Vec<String>,
    /// True when any aside mentions login / log in / sign in.
    pub login_hint: bool,
}

/// Split a raw prompt into funnel slots.
///
/// Runs [`strip_asides`] internally, then:
/// * `site_slot`: the existing `parse_grammar` on the cleaned prompt. Its
///   `site_context` wins when present ([`FunnelSlots::site_from_context`]
///   records that); otherwise its `target_noun` — unless that is a
///   pronoun, a stopword, or a login word (`you` is never a site, and
///   neither are `log`/`login`/`signin`: they name the login action, not
///   a destination). When the tokenizer dropped the destination entirely
///   (one-character names are invisible to it), [`single_char_token`]
///   recovers the standalone token.
/// * `object_slot`: whole-token stemmed match over the cleaned prompt's
///   tokens, so the object is found whether it precedes the site (`"open
///   settings on reddit"`) or follows it (`"open reddit's settings"`).
///
/// Everything else is discarded; it never reaches a resolver.
#[must_use]
pub fn split_slots(prompt: &str) -> FunnelSlots {
    let info = strip_asides(prompt);
    // Cold prompt: no connected origin, so the adjectival-site promotion
    // cannot fire and no portal host can leak into the slots.
    let grammar = parse_grammar(&info.cleaned, None);
    let from_context = grammar
        .site_context
        .as_deref()
        .is_some_and(|site| !is_pronoun(site) && !is_login_word(site));
    let site_slot = grammar
        .site_context
        .filter(|site| !is_pronoun(site) && !is_login_word(site))
        .or_else(|| {
            grammar.target_noun.filter(|target| {
                !is_pronoun(target)
                    && !STOPWORDS.contains(&target.as_str())
                    && !is_login_word(target)
            })
        })
        .or_else(|| single_char_token(&info.cleaned));
    let object_slot = tokens(&info.cleaned)
        .iter()
        .map(|token| singular_stem(token))
        .find_map(|stemmed| classify_object_token(&stemmed));
    FunnelSlots {
        site_slot,
        site_from_context: from_context,
        object_slot,
        asides: info.asides,
        login_hint: info.login_hint,
    }
}

/// Whether a slot token is a pronoun. `"you"` must never be a site slot:
/// the old pipeline grounded it as a destination.
fn is_pronoun(token: &str) -> bool {
    PRONOUNS.contains(&token)
}

/// One-character recovery for destinations the tokenizer drops
/// (`"open the X and"` → `"x"`): the grammar's own `direct_object_token`
/// recovery stops at the article before the token, so this scans the raw
/// cleaned words after the clause's open-verb, skipping stopwords,
/// pronouns, and verbs. Whole-token only: it fires solely on a standalone
/// single alphanumeric character — never a substring, so `"box"` and
/// `"you"` can never yield `"x"`.
fn single_char_token(cleaned: &str) -> Option<String> {
    let raw: Vec<String> = cleaned
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    let verb_at = raw
        .iter()
        .position(|word| OPEN_VERBS.contains(&word.as_str()))?;
    raw.iter().skip(verb_at + 1).find_map(|word| {
        if is_single_alnum(word)
            && !STOPWORDS.contains(&word.as_str())
            && !is_pronoun(word)
            && !is_action_verb(word)
        {
            Some(word.clone())
        } else {
            None
        }
    })
}

/// Exactly one alphanumeric character — a whole token, never a substring.
fn is_single_alnum(word: &str) -> bool {
    let mut chars = word.chars();
    matches!(chars.next(), Some(c) if c.is_alphanumeric()) && chars.next().is_none()
}

/// Funnel claim gate (pure): the cleaned prompt's first content verb is an
/// open-class verb ([`crate::open_verb_leads`]). The aside strip runs
/// first, so `"open the X for me and make sure ill do the login first"`
/// claims — routing must never inspect the login aside.
#[must_use]
pub fn funnel_claims(prompt: &str) -> bool {
    crate::open_verb_leads(&strip_asides(prompt).cleaned)
}

/// Whether the site candidate names one of the generic object classes
/// ([`ObjectClass`]) instead of a real destination: `"settings"` as a site
/// is the *goal*, not a place to navigate to.
fn is_object_noun(name: &str) -> bool {
    classify_object_token(&singular_stem(name)).is_some()
}

/// Object-noun exclusion (pure): when the grammar found no prepositional
/// site context, a site candidate the parser derived from `target_noun` is
/// one of the generic object nouns (Settings, Messages, Profile, …) — it
/// names the *goal*, not a destination, and routing must not ground it as
/// a site. `"open settings on reddit"` keeps site `reddit` because the
/// `on`-phrase produced a real `site_context`; `"open settings"` demotes
/// the site to `None` and keeps the object.
fn exclude_object_noun_site(slots: &FunnelSlots) -> Option<&str> {
    slots
        .site_slot
        .as_deref()
        .filter(|site| slots.site_from_context || !is_object_noun(site))
}

/// Content token eligible as a site mention: not a stopword, not a
/// pronoun, not an action verb, not a generic object noun, and never a
/// login word — the same content bar the grammar's adjectival rule
/// applies, minus its portal-verification (the ladder verifies instead).
fn is_site_mention_token(text: &str) -> bool {
    !STOPWORDS.contains(&text)
        && !is_pronoun(text)
        && !is_action_verb(text)
        && !is_object_noun(text)
        && !is_login_word(text)
}

/// Adjectival site mention in a cleaned prompt: the first site-mention
/// token that is not the excluded target noun. `"open my reddit profile"`
/// (target `"profile"`, excluded as an object noun) yields `"reddit"`;
/// `"open settings"` yields nothing — the only content token was the
/// target itself. Portal verification is NOT done here: a portal match
/// becomes `AlreadyOnOrigin` downstream, a mismatch becomes `GroundSite`
/// for the ladder to verify — never an in-page pursuit on the wrong
/// portal.
fn adjectival_site(cleaned: &str, excluded_target: Option<&str>) -> Option<String> {
    tokens(cleaned).iter().find_map(|token| {
        let text = token.as_str();
        if Some(text) == excluded_target || !is_site_mention_token(text) {
            None
        } else {
            Some(text.to_owned())
        }
    })
}

/// Routing decision for one funnel-claimed prompt, produced by
/// [`funnel_plan`]: pure data, no I/O. The dispatcher executes it.
///
/// The lane split with the other Tier 2B call site is deliberate: the
/// funnel's parser decides *routing* (which site to open, the plan's
/// `AskParser` arm); `follow_slots`' parser only names Stage 2's
/// *follow noun* on an already-searching page and never routes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunnelDecision {
    /// Non-open verb, plural, or otherwise not the funnel's prompt: the
    /// dispatcher keeps the existing pipeline untouched.
    Declined,
    /// The site matches the live portal (alias-aware): pursue the object
    /// in-page when present, otherwise the portal is already the answer —
    /// `SiteSearch` is never consulted.
    AlreadyOnOrigin {
        /// The resolved site slot.
        site: String,
        /// The object to pursue on the live page, if any.
        object: Option<ObjectClass>,
    },
    /// A site slot that is not the live portal: resolve it through the
    /// site-only ladder ([`crate::resolve_site_entry_url`]) — never search.
    GroundSite {
        /// The site slot to ground.
        site: String,
        /// The object to pursue once the site lands, if any.
        object: Option<ObjectClass>,
    },
    /// A bare object with a live portal (`"open settings"`): the portal
    /// derived from the current URL *is* the site — pursue the object
    /// in-page, no `SiteSearch`.
    ImplicitSite {
        /// The object to pursue on the live portal's page.
        object: ObjectClass,
    },
    /// No site, no usable object: the fenced parser gets one bounded shot
    /// at the cleaned prompt. Its site slot re-enters the ladder; anything
    /// else is the honest miss — never a search fallback.
    AskParser,
}

/// The funnel's whole routing answer for one prompt: the slots, the
/// cleaned prompt (asides never travel downstream of this plan), the
/// decision, and the journal lines the dispatcher records when it claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunnelPlan {
    /// The split slots, with the object-noun site exclusion applied.
    pub slots: FunnelSlots,
    /// The aside-cleaned prompt: every downstream reader — parser,
    /// routing, in-page resolution — uses this, never the raw prompt.
    pub cleaned: String,
    /// The routing decision.
    pub decision: FunnelDecision,
    /// Journal lines to record when the funnel claims the prompt.
    pub journal_lines: Vec<String>,
}

impl FunnelPlan {
    fn declined() -> Self {
        Self {
            slots: FunnelSlots::default(),
            cleaned: String::new(),
            decision: FunnelDecision::Declined,
            journal_lines: Vec::new(),
        }
    }
}

/// Plan the funnel's routing for one prompt (pure, deterministic).
///
/// Claims only non-plural prompts whose first content verb is an
/// open-class verb ([`funnel_claims`]), after aside stripping — the
/// saved lane, the connected batch lane, and retrieval verbs keep the
/// existing pipeline. Steps, in order:
///
/// 1. `split_slots` on the raw prompt (strips asides internally);
/// 2. object-noun exclusion ([`exclude_object_noun_site`]): a
///    site-from-`target_noun` that is really an object noun demotes to no
///    site; then the adjectival-site mention ([`adjectival_site`]): a
///    leftover content token (`"reddit"` in `"open my reddit profile"`)
///    is a site mention for the ladder to verify;
/// 3. routing:
///    * site matches the live portal → [`FunnelDecision::AlreadyOnOrigin`]
///      (object: pursue in-page; none: already there — `SiteSearch` never
///      consulted);
///    * bare object + live portal → [`FunnelDecision::ImplicitSite`];
///    * site, not the portal → [`FunnelDecision::GroundSite`] (the
///      site-only ladder — never search);
///    * neither → [`FunnelDecision::AskParser`] (the fenced Tier 2B
///      routing shot on the cleaned prompt; no site means the honest
///      miss).
///
/// `portal_host` is the live portal's host (or the connected portal's), if
/// the dispatcher knows it yet; `None` keeps every decision that does not
/// need it and reports the portal-dependent ones against the unknown.
#[must_use]
pub fn funnel_plan(prompt: &str, is_plural: bool, portal_host: Option<&str>) -> FunnelPlan {
    if is_plural || !funnel_claims(prompt) {
        return FunnelPlan::declined();
    }
    let info = strip_asides(prompt);
    let mut slots = split_slots(prompt);
    // Object-noun exclusion first: a site-from-`target_noun` that is really
    // an object noun demotes to no site (the journal below records the
    // routed slots, never the raw goal-as-site).
    let excluded_target = slots.site_slot.clone();
    slots.site_slot = exclude_object_noun_site(&slots).map(str::to_owned);
    // Adjectival site mention: the cold parse drops adjectives (`"open my
    // reddit profile"` keeps only target `"profile"`), so a leftover
    // content token is a site mention the ladder verifies — never an
    // in-page pursuit on the wrong portal.
    if slots.site_slot.is_none() {
        slots.site_slot = adjectival_site(&info.cleaned, excluded_target.as_deref());
    }
    // Journal the asides and the login policy first: the user announced
    // they will log in themselves, so the dispatcher must not attempt it.
    let mut journal_lines = vec![format!(
        "funnel_slots: site={:?} object={:?} asides={:?}",
        slots.site_slot,
        slots.object_slot.map(object_noun),
        slots.asides,
    )];
    if slots.login_hint {
        journal_lines.push(
            "aside_policy: user_will_login — will not attempt login; Take Control available"
                .to_owned(),
        );
    }
    let site = slots.site_slot.clone();
    let already_here = site
        .as_deref()
        .is_some_and(|site| portal_host.is_some_and(|host| crate::site_matches_host(site, host)));
    let decision = match (site, slots.object_slot) {
        (Some(site), object) if already_here => FunnelDecision::AlreadyOnOrigin { site, object },
        (Some(site), object) => FunnelDecision::GroundSite { site, object },
        (None, Some(object)) if portal_host.is_some() => FunnelDecision::ImplicitSite { object },
        (None, _) => FunnelDecision::AskParser,
    };
    FunnelPlan {
        slots,
        cleaned: info.cleaned,
        decision,
        journal_lines,
    }
}

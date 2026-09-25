#![deny(unsafe_code)]
//! Tiered cold-path entry resolution: attach a navigable `entry_url` to
//! ad-hoc intents that have none, before dispatch reaches the macro engine.
//!
//! Tier order, first hit wins. Tier 1 — the saved-playbook lookup — is the
//! caller's pre-step: this runs only when `resolve_command` already yielded
//! an ephemeral intent, so nothing here reimplements matching. Proven
//! workflows (including the seeded GitHub invoice harvester) therefore
//! answer before any tier below is consulted:
//!
//! 1. Saved playbook — decided upstream, never re-run here.
//! 2. Explicit domain or URL in the prompt (`open amazon.in`): the user's
//!    typed destination is ground truth, so it precedes every adapter —
//!    and fails closed when malformed, since a typo is not fixed by a
//!    model guess.
//! 3. Connected-account entity (`entity_resolver`), only with a directory.
//! 4. LLM fallback, only with a configured adapter — and its output is
//!    untrusted input, validated like every other tier.
//! 5. Direct-open grounding ladder, only for high-confidence single-target
//!    opens (`open amazon`): a user-saved site shortcut, then a structured
//!    site directory (Brave Search API when keyed, keyless `DuckDuckGo`
//!    otherwise), then a fenced domain grounder (site slot + region hint →
//!    bare domain, validated in Rust). An ungrounded direct open is a miss
//!    the UI can act on — it never falls through to a search page, because
//!    a guessed SERP click is worse than asking. In-page goals (site +
//!    artifact noun) ground only the site through the same ladder and may
//!    fall through to tier 6 when no rung knows it.
//! 6. Site-ladder retry — the grammar's site slots (`site_context`, then
//!    `target_noun`) through the same site-only ladder, for prompts no
//!    earlier tier grounded. First hit wins; a total miss is an honest
//!    miss. The engine never navigates to a visible search page: no search
//!    template, no guessed TLDs, no SERP scraping.
//!
//! There is deliberately no static route table between them: a curated
//! `(portal, class) → URL` list needed a portal whitelist to stay
//! meaningful, and both of its jobs are now covered — proven routes by
//! tier 1, unknown ones by tiers 2 and 5.
//!
//! Validation follows provenance, not tier order. User-directed targets —
//! the typed domain (tier 2), saved shortcuts, and the site-directory and
//! grounder hits (tier 5), where the user named the site and the machine
//! only resolved the name — face structural validation only (absolute
//! `https`, no credentials, parseable):
//! [`crate::url_policy::validate_user_directed_url`]. Machine-invented
//! targets — the account entity (tier 3) and the LLM fallback (tier 4) —
//! face the host allowlist:
//! [`crate::url_policy::validate_proposed_url`]. The grounder output
//! additionally passes [`validate_grounded_domain`] first, so it is a
//! Rust-checked bare domain before it becomes a URL at all. Any
//! validation failure returns `None` immediately — a corrupt tier never
//! falls through to a weaker one. The last tier only runs when no stronger
//! tier proposed anything; an invalid stronger proposal still fails closed
//! without falling through.
//!
//! # Slot resolution runs alongside URL resolution
//!
//! The tiers above answer *where to start*. [`resolve_slots`] answers *what
//! to look for once there* — the noun the dispatcher grounds on — and it
//! has its own sub-cascade:
//!
//! * **Tier 2A** — the deterministic grammar parse reports
//!   [`crate::Confidence::High`], so it is used as-is at zero token cost.
//! * **Tier 2B** — low confidence with a parser configured: the fenced
//!   [`crate::IntentParser`] seam supplies slots, bounded by
//!   [`crate::PARSER_TIMEOUT_MS`].
//! * **Tier 2C** — no parser, a declined parse, a timeout, or output that
//!   failed the slot fence: the low-confidence grammar slots stand, and
//!   Stage 2 falls back to the intent's own probe text. Degradation, never
//!   failure — offline is a normal outcome.

use crate::{
    entity_resolver::{AccountDirectory, resolve_repo_entity},
    intent_parser::{IntentParser, parse_prompt_bounded},
    intent_resolver::{ParsedGrammar, is_direct_open, parse_grammar},
    url_policy::{validate_proposed_url, validate_user_directed_url},
};
use std::sync::Arc;
use zeroize::Zeroizing;

/// Where a resolved route came from. Recorded on the resolution and logged
/// with the navigation proposal so wrong sources are debuggable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteSource {
    AccountEntity,
    LlmFallback,
    /// The prompt named a domain or URL outright (`open amazon.in`).
    ExplicitDomain,
    /// A user-saved site shortcut matched the target noun.
    Shortcut,
    /// A structured site directory (Brave Search API) resolved the name.
    SiteSearch,
    /// A fenced domain grounder resolved the site name to a bare domain
    /// (e.g. `amazon` → `amazon.in`) using the site slot plus a region
    /// hint. The domain is validated in Rust before navigation; see
    /// [`validate_grounded_domain`].
    DomainGrounded,
}

/// A validated navigation target plus its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRoute {
    pub url: url::Url,
    pub source: RouteSource,
    /// Which directory backend served a [`RouteSource::SiteSearch`] hit
    /// (`"brave"` / `"ddg"` / the adapter's label). `None` for every other
    /// source. The dispatcher journals it (`· via ddg`) so Session
    /// Activity names the backend behind a directory navigation.
    pub directory_backend: Option<&'static str>,
    /// A `route_vetoed: …` note carried when the directory rung returned a
    /// host-mismatched hit that the plausibility veto rejected before a
    /// later rung grounded the name. `None` when the directory rung was
    /// never tried, missed, or accepted. The dispatcher journals it
    /// alongside `route_proposed` so a veto reads as a veto, not a miss.
    pub directory_veto: Option<String>,
}

/// Optional resolution inputs. Every one is inert when unset, which is the
/// production default: no account directory is wired (no stored GitHub
/// credential exists to back one), no URL adapter is configured, and the
/// shipped intent parser declines. Unset inputs degrade the cascade to the
/// honest miss rather than failing it.
pub struct ResolutionContext<'a> {
    pub account_dir: Option<&'a dyn AccountDirectory>,
    pub llm: Option<&'a dyn LlmUrlProposer>,
    /// Fenced slot parser consulted only for low-confidence prompts, and
    /// only for slots — it never proposes a URL. See
    /// [`crate::intent_parser`] for the fence and the timeout.
    pub parser: Option<&'a Arc<dyn IntentParser>>,
    /// User-saved site shortcuts, consulted by the direct-open ladder.
    pub shortcuts: Option<&'a dyn ShortcutStore>,
    /// Structured site directory, consulted by the direct-open ladder when
    /// no shortcut or explicit domain grounded the site.
    pub site_search: Option<&'a dyn SiteSearchClient>,
    /// Fenced domain grounder, consulted by the direct-open ladder after
    /// shortcuts and before the site directory. Takes only the site slot
    /// plus a region hint; returns a bare domain validated in Rust.
    pub domain_grounder: Option<&'a dyn DomainGrounder>,
    /// Region hint for the domain grounder (e.g. `IN`), or empty when
    /// unknown. Derived from system timezone; see [`system_region_hint`].
    pub region_hint: &'a str,
}

/// Last-resort URL proposer. Synchronous by contract: adapters needing I/O
/// must bound it internally, and whatever comes back is validated as
/// untrusted input — never navigated on good faith.
pub trait LlmUrlProposer: Send + Sync {
    /// Propose a single URL for the prompt, or nothing.
    fn propose_url(&self, prompt: &str) -> Option<String>;
}

/// Validate one tier's output, halting the whole resolution on failure.
fn accept(url: &str, source: RouteSource) -> Option<ResolvedRoute> {
    validate_proposed_url(url)
        .map(|url| ResolvedRoute {
            url,
            source,
            directory_backend: None,
            directory_veto: None,
        })
        .ok()
}

/// Validate one user-directed tier's output: structural checks only, no
/// host allowlist — the user named the destination. See
/// [`crate::url_policy::validate_user_directed_url`] for the trust
/// rationale.
fn accept_user_directed(url: &str, source: RouteSource) -> Option<ResolvedRoute> {
    validate_user_directed_url(url)
        .map(|url| ResolvedRoute {
            url,
            source,
            directory_backend: None,
            directory_veto: None,
        })
        .ok()
}

/// Validate a directory hit like any user-directed target, carrying the
/// serving backend on the route for the journal line. The hit already
/// passed the plausibility veto ([`directory_veto_note`]); this is the
/// unchanged structural bar (absolute https, real host, no credentials).
fn accept_site_search(hit: &SiteHit) -> Option<ResolvedRoute> {
    validate_user_directed_url(&hit.url)
        .map(|url| ResolvedRoute {
            url,
            source: RouteSource::SiteSearch,
            directory_backend: Some(hit.backend),
            directory_veto: None,
        })
        .ok()
}

/// A user-maintained site-name → URL directory. Names are normalized
/// (lowercase, trimmed) at the write boundary; lookups take the grammar's
/// already-normalized target noun. Synchronous and infallible by contract:
/// a missing shortcut is `None`, never an error.
pub trait ShortcutStore: Send + Sync {
    /// The saved URL for `name`, if the user stored one.
    fn shortcut_url(&self, name: &str) -> Option<String>;
}

/// In-memory [`ShortcutStore`], built from the persisted shortcuts once
/// per dispatch. Loading up front keeps the trait runtime-agnostic — no
/// async, no blocking inside the engine — and the ladder lookup itself is
/// pure. Keys are expected normalized (lowercase); the lookup re-lowercases
/// defensively so a caller passing `Amazon` still hits the `amazon` row.
#[derive(Debug, Default)]
pub struct InMemoryShortcuts {
    map: std::collections::HashMap<String, String>,
}

impl InMemoryShortcuts {
    /// Build from already-normalized name → URL pairs, e.g. straight out
    /// of the rows returned by the playbook store's `list_site_shortcuts`.
    #[must_use]
    pub fn new(map: std::collections::HashMap<String, String>) -> Self {
        Self { map }
    }
}

impl ShortcutStore for InMemoryShortcuts {
    fn shortcut_url(&self, name: &str) -> Option<String> {
        self.map.get(&name.trim().to_lowercase()).cloned()
    }
}

/// A structured site directory: a site name in, a destination URL out.
///
/// This is deliberately not SERP scraping. A search API returns ranked
/// results as data; the resolver takes the top URL and validates it like
/// any user-directed target. No HTML is fetched, no ads or AI overviews
/// are parsed, and bot challenges cannot produce a false destination —
/// failure is `None`, which degrades to asking the user.
pub trait SiteSearchClient: Send + Sync {
    /// The best destination hit for `site_name`, or `None`. The hit names
    /// the backend that served it so the journal can say which directory
    /// answered; the plausibility veto ([`directory_veto_note`]) decides
    /// whether the URL may be navigated.
    fn search_site(&self, site_name: &str) -> Option<SiteHit>;
}

/// One directory answer: the destination URL plus the backend that served
/// it (`"brave"` / `"ddg"` for the shipped adapters, the fake's label in
/// tests). Carried so a wrong confident navigation can be told apart from
/// a miss in the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteHit {
    pub url: String,
    pub backend: &'static str,
}

/// Brave Search API as a [`SiteSearchClient`]. The key comes from the
/// `CLINCH_BRAVE_API_KEY` environment variable and is zeroized on drop;
/// [`Self::from_env`] returns `None` when it is absent, which simply
/// disables this rung of the ladder — offline stays a normal outcome.
pub struct BraveSiteSearch {
    api_key: Zeroizing<String>,
    agent: ureq::Agent,
}

impl BraveSiteSearch {
    /// Build from the environment, or `None` when no key is configured.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let key: String = std::env::var("CLINCH_BRAVE_API_KEY")
            .ok()?
            .trim()
            .to_owned();
        if key.is_empty() {
            return None;
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(10)))
            .build()
            .into();
        Some(Self {
            api_key: Zeroizing::new(key),
            agent,
        })
    }
}

/// Pull the top web-result URL out of a Brave Search API response.
/// Pure so tests prove the parsing without network.
fn brave_top_url(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("web")?
        .get("results")?
        .as_array()?
        .first()?
        .get("url")?
        .as_str()
        .map(str::to_owned)
}

impl SiteSearchClient for BraveSiteSearch {
    fn search_site(&self, site_name: &str) -> Option<SiteHit> {
        let query: String = url::form_urlencoded::byte_serialize(site_name.as_bytes()).collect();
        let endpoint = format!(
            "https://api.search.brave.com/res/v1/web/search?q={query}&count=1&safesearch=moderate"
        );
        let payload: serde_json::Value = self
            .agent
            .get(&endpoint)
            .header("X-Subscription-Token", self.api_key.as_str())
            .header("Accept", "application/json")
            .call()
            .ok()?
            .into_body()
            .read_json()
            .ok()?;
        brave_top_url(&payload).map(|url| SiteHit {
            url,
            backend: "brave",
        })
    }
}

/// `DuckDuckGo`'s no-JS HTML endpoint as a [`SiteSearchClient`]: keyless,
/// best-effort, zero-config.
///
/// No API key, no signup — a plain blocking GET to the HTML endpoint with
/// a browser user-agent, parsed in memory. The browser never sees a search
/// page; only the extracted target URL leaves this module.
///
/// Honesty notes, read before relying on this rung: DDG throttles
/// programmatic clients per IP (HTTP 202 / anomaly pages under load), the
/// markup can change without notice, and scraping is ToS-gray. Every one
/// of those failure modes is `None`, which falls through to the next
/// ladder rung — never a guess. The sanctioned upgrade is
/// [`BraveSiteSearch`] when `CLINCH_BRAVE_API_KEY` is set.
pub struct DuckDuckGoSiteSearch {
    agent: ureq::Agent,
}

impl DuckDuckGoSiteSearch {
    /// Keyless constructor: nothing to configure.
    #[must_use]
    pub fn new() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(10)))
            .build()
            .into();
        Self { agent }
    }
}

impl Default for DuckDuckGoSiteSearch {
    fn default() -> Self {
        Self::new()
    }
}

/// `DuckDuckGo` HTML endpoint and a browser user-agent: without the latter
/// DDG serves the anomaly page to programmatic clients.
const DDG_HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/?q=";
const DDG_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Pull the first organic result URL out of a `DuckDuckGo` HTML response.
/// Pure so tests prove the parsing without network.
///
/// Result anchors look like
/// `<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=<pct-encoded>&amp;rut=…">`.
/// The `uddg` query param is the real target, percent-encoded. Anchors
/// whose target is DDG's own ad redirect (`/y.js`) are skipped — ads are
/// not organic results, and taking one would route the user to an ad
/// network instead of the site they named.
fn ddg_top_url(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(a_at) = rest.find("<a") {
        let after_a = &rest[a_at + 2..];
        // `<a` must be followed by whitespace or `>`: skip `<abbr` etc.
        if !after_a
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
        {
            rest = after_a;
            continue;
        }
        let tag = after_a.split('>').next()?;
        rest = &after_a[tag.len()..];
        if !tag.contains("result__a") {
            continue;
        }
        let href = ddg_href(tag)?;
        let target = ddg_unwrap_target(href)?;
        // Ad redirect: DDG wraps paid results in its own `/y.js` click
        // tracker. Never an organic destination.
        if target.contains("duckduckgo.com/y.js") {
            continue;
        }
        if target.starts_with("https://") || target.starts_with("http://") {
            return Some(target);
        }
    }
    None
}

/// The `href="…"` value of an anchor tag, or `None`.
fn ddg_href(tag: &str) -> Option<&str> {
    let after = tag.split("href=\"").nth(1)?;
    after.split('"').next()
}

/// Resolve a DDG result href to its target URL: unwrap the `/l/?uddg=`
/// redirect wrapper (percent-decoded), or pass a direct link through.
fn ddg_unwrap_target(href: &str) -> Option<String> {
    let href = href.replace("&amp;", "&");
    if let Some((_, query)) = href.split_once('?') {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if key == "uddg" {
                return Some(value.into_owned());
            }
        }
    }
    if href.starts_with("https://") || href.starts_with("http://") {
        return Some(href);
    }
    None
}

impl SiteSearchClient for DuckDuckGoSiteSearch {
    fn search_site(&self, site_name: &str) -> Option<SiteHit> {
        let query: String = url::form_urlencoded::byte_serialize(site_name.as_bytes()).collect();
        let response = self
            .agent
            .get(&format!("{DDG_HTML_ENDPOINT}{query}"))
            .header("User-Agent", DDG_USER_AGENT)
            .call()
            .ok()?;
        // DDG throttles programmatic clients with HTTP 202 anomaly pages:
        // only a 200 carries results. Anything else falls through to the
        // next ladder rung.
        if response.status() != 200 {
            return None;
        }
        let html = response.into_body().read_to_string().ok()?;
        ddg_top_url(&html).map(|url| SiteHit {
            url,
            backend: "ddg",
        })
    }
}

/// The composite directory rung: Brave's sanctioned search API when
/// `CLINCH_BRAVE_API_KEY` is configured, `DuckDuckGo`'s keyless HTML
/// endpoint as the zero-config fallback.
///
/// One [`SiteSearchClient`] so the ladder keeps a single directory rung:
/// shortcut → directory → grounder → honest miss. The LLM grounder stays
/// below the directory either way — ranking is the ground truth of what a
/// site name means; a generative guess cannot outrank it.
pub struct ChainedSiteSearch {
    primary: Option<BraveSiteSearch>,
    fallback: DuckDuckGoSiteSearch,
}

impl ChainedSiteSearch {
    /// Build from the environment: Brave when its key is present, `DuckDuckGo`
    /// always. The directory rung is therefore never "unconfigured".
    #[must_use]
    pub fn new() -> Self {
        Self {
            primary: BraveSiteSearch::from_env(),
            fallback: DuckDuckGoSiteSearch::new(),
        }
    }

    /// Which backends this chain will try, for the miss journal line.
    #[must_use]
    pub fn backend_label(&self) -> &'static str {
        if self.primary.is_some() {
            "brave→ddg"
        } else {
            "ddg"
        }
    }
}

impl Default for ChainedSiteSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl SiteSearchClient for ChainedSiteSearch {
    fn search_site(&self, site_name: &str) -> Option<SiteHit> {
        if let Some(primary) = &self.primary
            && let Some(hit) = primary.search_site(site_name)
        {
            return Some(hit);
        }
        self.fallback.search_site(site_name)
    }
}

/// Registrable-label approximation for the directory plausibility veto:
/// strip a single leading `www.`, then take the last two dot-labels
/// (`www.reddit.com` → `reddit.com`, `support.microsoft.com` →
/// `microsoft.com`).
///
/// This is deliberately NOT a public-suffix list: `amazon.co.uk` labels as
/// `co.uk`, so a directory hit there vetoes and the ladder falls through
/// to the grounder rung. A real PSL crate would fix that; the
/// approximation is documented here because the veto is a plausibility
/// heuristic — it turns a wrong confident navigation into a miss — not a
/// security boundary. Structural URL validation (`accept_*`) is unchanged
/// and still owns safety.
fn registrable_label(host: &str) -> &str {
    let host = host.strip_prefix("www.").unwrap_or(host);
    // Last two dot-separated labels; hosts with fewer than two labels
    // (e.g. `localhost`) keep the whole host.
    let mut parts = host.rsplitn(3, '.');
    match (parts.next(), parts.next()) {
        (Some(tld), Some(sld)) => {
            let start = host.len() - sld.len() - 1 - tld.len();
            &host[start..]
        }
        _ => host,
    }
}

/// Blessed site-name → host exceptions, checked before the containment
/// rule. Exactly one entry: `gmail` is served from `mail.google.com`,
/// whose registrable label (`google.com`) matches neither containment
/// direction. This is a deliberate single exception, not a site table — do
/// not grow it. A directory hit the name cannot plausibly explain must
/// veto, never alias.
const SITE_HOST_ALIASES: [(&str, &str); 1] = [("gmail", "mail.google.com")];

/// Site↔host plausibility as a pure predicate: the alias-aware containment
/// rule the directory veto applies. `true` when the registrable label of
/// `host` contains the site name or vice versa (`reddit` ↔ `reddit.com`),
/// with the single blessed alias checked first (`gmail` ↔
/// `mail.google.com`). Shared by the directory veto, the funnel's
/// already-on-origin check, and the settle contract — one rule, three
/// readers, so they can never drift.
#[must_use]
pub fn site_matches_host(site_name: &str, host: &str) -> bool {
    let name = site_name.trim().to_lowercase();
    let host = host.trim().to_lowercase();
    if name.is_empty() || host.is_empty() {
        return false;
    }
    // Alias check first: exact normalized-name match, registrable-domain
    // equality on the returned host. Strict on purpose — `gmail` → a
    // `gmail.com` hit still fails; only the blessed host is accepted.
    for (alias_name, alias_host) in SITE_HOST_ALIASES {
        if name == alias_name {
            return registrable_label(&host) == registrable_label(alias_host);
        }
    }
    let label = registrable_label(&host);
    label.contains(&name) || name.contains(label)
}

/// Plausibility veto for one directory hit: `None` when the hit may serve
/// the queried site name, `Some(note)` carrying a `route_vetoed: …`
/// journal line when it may not. The name is normalized (lowercase,
/// trimmed); the hit is accepted when the alias map blesses it or when
/// the name and the host's registrable label contain one another in either
/// direction (`reddit` ↔ `reddit.com`). Anything else — a support article
/// for a site query, an unparseable URL, an empty name — is never accepted
/// blindly: the caller falls through to the grounder rung.
fn directory_veto_note(site_name: &str, hit: &SiteHit) -> Option<String> {
    let name = site_name.trim().to_lowercase();
    let note = |host: &str, reason: &str| {
        format!("route_vetoed: site_search '{name}' → {host} · {reason}; trying grounder")
    };
    if name.is_empty() {
        return Some(note("?", "empty site name"));
    }
    let Some(host) = url::Url::parse(&hit.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    else {
        return Some(note("?", "unparseable URL"));
    };
    if site_matches_host(&name, &host) {
        None
    } else {
        Some(note(&host, "host mismatch"))
    }
}

/// One Tier 3b directory step: query the client, veto host-mismatched hits,
/// accept plausible ones. Returns the accepted route plus, when the hit
/// was vetoed, the journal note — the caller falls through to the grounder
/// rung and carries the note on whatever route grounds next, so the veto
/// reads as a veto in Session Activity rather than a silent miss.
fn directory_step(
    client: &dyn SiteSearchClient,
    site_name: &str,
) -> (Option<ResolvedRoute>, Option<String>) {
    let Some(hit) = client.search_site(site_name) else {
        return (None, None);
    };
    if let Some(note) = directory_veto_note(site_name, &hit) {
        return (None, Some(note));
    }
    (accept_site_search(&hit), None)
}

/// The site-only grounding ladder, shared by Tier 3 (direct opens), Tier 3b
/// (in-page goals), and the funnel: a user-saved site shortcut, then the
/// structured site directory with the plausibility veto, then the fenced
/// domain grounder. One ladder, three callers — never forked, so a rung
/// fix lands everywhere at once.
///
/// Takes the already-parsed site slot alone (never the raw prompt): the
/// artifact noun is the dispatcher's business, pursued on the live page.
/// A vetoed directory hit rides the grounder's route on
/// [`ResolvedRoute::directory_veto`] so the journal reads as a veto, not a
/// silent miss. `None` when no rung knows the site — the caller owns the
/// honest miss; this function never searches.
#[must_use]
pub fn resolve_site_entry_url(site: &str, ctx: &ResolutionContext<'_>) -> Option<ResolvedRoute> {
    // User-saved site shortcut. A corrupt saved URL is skipped — stale
    // data must not veto the rung below it.
    if let Some(store) = ctx.shortcuts
        && let Some(url) = store.shortcut_url(site)
        && let Some(route) = accept_user_directed(&url, RouteSource::Shortcut)
    {
        return Some(route);
    }
    // Structured site directory, composite: Brave's sanctioned API when
    // `CLINCH_BRAVE_API_KEY` is set, DuckDuckGo's keyless HTML endpoint as
    // the zero-config fallback (`ChainedSiteSearch`). Either way the rung
    // is never unconfigured — DDG works out of the box. This rung runs
    // BEFORE the LLM grounder: a search backend returns ranked results as
    // data, and ranking is the ground truth of what a site name means — it
    // cannot hallucinate the way a generative model can (`claude` →
    // `open.com`). The call is backend HTTP in memory; the browser never
    // sees a search page.
    //
    // Trust note: the directory resolves a *name the user typed*, the same
    // way asking an assistant to "open amazon" does — it is name
    // resolution, not a machine-invented destination. The structural bar
    // (absolute https, real host, no credentials) stays the honest one, but
    // a structurally-valid URL can still be the WRONG site — a support
    // article once shipped as a confident navigation. So every hit passes
    // a plausibility veto first ([`directory_veto_note`]): the host's
    // registrable label must contain the queried name or vice versa, with
    // exactly one blessed alias (`gmail` → `mail.google.com`). A vetoed
    // hit is never navigated — the ladder falls through to the grounder
    // rung and the veto rides the journal line. The veto is a heuristic,
    // not a security boundary: `amazon-phishing.com` still contains
    // `amazon`, so phishing-shaped hosts pass it the way they pass any
    // name check.
    let mut directory_veto: Option<String> = None;
    if let Some(client) = ctx.site_search {
        let (route, veto) = directory_step(client, site);
        directory_veto = veto;
        if let Some(route) = route {
            return Some(route);
        }
    }
    // Fenced domain grounder: site slot + region hint → bare domain.
    // Fallback when the directory finds nothing, is throttled, or had its
    // hit vetoed (both backends degrade to `None`, never a guess). The
    // grounder sees only the normalized site name and the region code,
    // never the raw prompt. Its output is validated in Rust (https, valid
    // TLD, no credentials, no raw IP) before navigation. A malformed
    // response degrades to the next rung, never to a guessed
    // `www.{noun}.com`.
    if let Some(grounder) = ctx.domain_grounder
        && let Some(domain) = grounder.ground_domain(site, ctx.region_hint)
        && let Some(url) = validate_grounded_domain(&domain)
        && let Some(route) = accept_user_directed(&url, RouteSource::DomainGrounded)
    {
        return Some(ResolvedRoute {
            directory_veto,
            ..route
        });
    }
    // Ungrounded and no directory: a miss. The caller surfaces "try a full
    // domain or save a site shortcut" — asking once beats a scraped SERP
    // that may be a challenge page or an ad.
    None
}

/// A fenced domain grounder: a site name in, a bare domain out.
///
/// This is the Muse-like rung. The caller passes only the already-parsed
/// site slot (e.g. `amazon`) plus a region hint (e.g. `IN` derived from
/// `Asia/Kolkata`) — never the raw prompt, never page HTML. The grounder
/// returns a single bare domain as strict JSON (`{"domain": "amazon.in"}`),
/// or `None` when it cannot ground.
///
/// The returned domain is untrusted input: [`validate_grounded_domain`]
/// enforces HTTPS, a well-formed hostname with a valid TLD, no credentials,
/// and no raw IP addresses before anything navigates. A malformed or
/// missing response degrades to the honest miss, never to a guessed
/// `www.{noun}.com`.
pub trait DomainGrounder: Send + Sync {
    /// The best bare domain for `site_name` in `region_hint`, or `None`.
    ///
    /// `site_name` is the grammar's normalized target noun (e.g. `amazon`);
    /// `region_hint` is an ISO region code like `IN` or `US`, or empty when
    /// unknown. Implementations must return a bare domain only
    /// (e.g. `amazon.in`), never a full URL, path, or credentials.
    fn ground_domain(&self, site_name: &str, region_hint: &str) -> Option<String>;
}

/// Stub grounder for production default: always declines.
///
/// Like [`crate::StubIntentParser`], this keeps offline a normal outcome.
/// Wire a real LLM-backed grounder here when the Tier 2B provider decision
/// (local Ollama vs cloud API) is made; until then the ladder degrades to
/// the honest miss.
#[derive(Debug, Default)]
pub struct StubDomainGrounder;

impl DomainGrounder for StubDomainGrounder {
    fn ground_domain(&self, _site_name: &str, _region_hint: &str) -> Option<String> {
        None
    }
}

/// Derive an ISO region hint from an IANA timezone name.
///
/// `Asia/Kolkata` → `IN`, `America/New_York` → `US`, etc. This is a small
/// closed mapping for the common cases; unknown zones yield `None` rather
/// than a guess. The grounder treats an unknown region as empty hint.
#[must_use]
pub fn region_hint_from_timezone(tz: &str) -> Option<&'static str> {
    // Continent/City → region. Keep this closed and small; it is a hint,
    // not knowledge about sites.
    if tz.starts_with("Asia/Kolkata")
        || tz.starts_with("Asia/Calcutta")
        || tz.starts_with("Asia/Delhi")
        || tz.starts_with("Asia/Mumbai")
    {
        return Some("IN");
    }
    if tz.starts_with("America/") {
        // Americas: map the common ones, default US for the rest.
        if tz.starts_with("America/Sao_Paulo") {
            return Some("BR");
        }
        if tz.starts_with("America/Mexico") {
            return Some("MX");
        }
        if tz.starts_with("America/Toronto") || tz.starts_with("America/Vancouver") {
            return Some("CA");
        }
        return Some("US");
    }
    if tz.starts_with("Europe/") {
        if tz.starts_with("Europe/London") {
            return Some("GB");
        }
        if tz.starts_with("Europe/Paris") {
            return Some("FR");
        }
        if tz.starts_with("Europe/Berlin") {
            return Some("DE");
        }
        return Some("EU");
    }
    if tz.starts_with("Asia/Tokyo") {
        return Some("JP");
    }
    if tz.starts_with("Asia/Shanghai") || tz.starts_with("Asia/Hong_Kong") {
        return Some("CN");
    }
    if tz.starts_with("Australia/") {
        return Some("AU");
    }
    None
}

/// The system's region hint for the domain grounder.
///
/// Reads the `TZ` environment variable or falls back to a UTC default.
/// Returns an ISO region code like `IN`, or empty string when unknown —
/// the grounder must handle empty as "no preference".
#[must_use]
pub fn system_region_hint() -> String {
    // Try TZ env, then /etc/timezone (Debian), then empty.
    if let Ok(tz) = std::env::var("TZ")
        && let Some(region) = region_hint_from_timezone(tz.trim())
    {
        return region.to_owned();
    }
    if let Ok(tz) = std::fs::read_to_string("/etc/timezone")
        && let Some(region) = region_hint_from_timezone(tz.trim())
    {
        return region.to_owned();
    }
    String::new()
}

/// Validate a grounder-returned bare domain and build the navigation URL.
///
/// Enforces, in Rust (never trusting the grounder):
/// - well-formed hostname via [`is_bare_domain`] (dot-separated labels,
///   2+ letter TLD),
/// - no raw IP addresses (v4 or v6),
/// - absolute `https` URL with no embedded credentials (via
///   [`validate_user_directed_url`]).
///
/// Returns the validated `https://{domain}` URL string, or `None` when the
/// domain is malformed. Callers degrade to the honest miss on `None`.
#[must_use]
pub fn validate_grounded_domain(domain: &str) -> Option<String> {
    let domain = domain.trim().trim_end_matches('.').to_lowercase();
    if domain.is_empty() || domain.len() > 253 {
        return None;
    }
    // The grounder returns a bare domain only — no paths, no credentials.
    // (`is_bare_domain` tolerates a single path for the explicit-domain
    // tier; this rung is stricter by design.)
    if domain.contains('/') || domain.contains('@') || domain.contains(':') {
        return None;
    }
    // Reject raw IPs: the TLD check in `is_bare_domain` already blocks most,
    // but be explicit — a grounder must never yield a literal address.
    if domain.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    if !is_bare_domain(&domain) {
        return None;
    }
    let url = format!("https://{domain}");
    // Structural validation only (no allowlist): this is name resolution
    // like SiteSearch, not a machine-proposed allowlisted target.
    validate_user_directed_url(&url).ok()?;
    Some(url)
}

/// Whether `token` is shaped like a bare domain: dot-separated labels of
/// letters, digits, and interior hyphens, ending in a 2+ letter TLD. An
/// optional single path (`github.com/settings`) is allowed and preserved.
/// This is shape, not knowledge — no site list is consulted.
fn is_bare_domain(token: &str) -> bool {
    if token.len() > 253 || token.is_empty() {
        return false;
    }
    let host = token.split('/').next().unwrap_or("");
    if !host.contains('.') {
        return false;
    }
    let mut labels = host.split('.');
    let tld = labels.next_back().unwrap_or("");
    if tld.len() < 2 || !tld.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Pull an explicit destination out of a direct-open prompt: a full URL the
/// user typed (`open https://github.com/settings/billing`), or a bare
/// domain (`open amazon.in` → `https://amazon.in`). Only the first
/// domain-shaped token wins; trailing punctuation is stripped. `None` when
/// the prompt names no destination outright — never a guess.
#[must_use]
pub fn explicit_url_in_prompt(prompt: &str) -> Option<String> {
    for piece in prompt.split(|character: char| {
        character.is_whitespace() || matches!(character, '"' | '\'' | '(' | ')' | '<' | '>')
    }) {
        let token = piece
            .trim_matches(|character: char| matches!(character, '.' | '!' | '?' | ',' | ';' | ':'));
        if token.is_empty() {
            continue;
        }
        if let Some(rest) = token
            .strip_prefix("https://")
            .or_else(|| token.strip_prefix("http://"))
        {
            // A typed URL must name a host; the validator checks the rest.
            if !rest.is_empty()
                && rest
                    .split('/')
                    .next()
                    .is_some_and(|host| host.contains('.'))
            {
                return Some(token.to_owned());
            }
            continue;
        }
        if is_bare_domain(token) {
            return Some(format!("https://{token}"));
        }
    }
    None
}

/// Which sub-tier produced the follow slots. Journaled so a wrong follow
/// is attributable to grammar, a parser, or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotSource {
    /// Tier 2A: deterministic grammar, high confidence, zero tokens.
    GrammarFastPath,
    /// Tier 2B: fenced parser seam answered and its slots passed the fence.
    IntentParser,
    /// Tier 2C: nobody grounded the slots. Whatever low-confidence grammar
    /// found still travels, and Stage 2 falls back to the intent's own
    /// probe text.
    Ungrounded,
}

impl SlotSource {
    /// Short tier label for journal lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GrammarFastPath => "grammar",
            Self::IntentParser => "parser",
            Self::Ungrounded => "ungrounded",
        }
    }
}

/// Grounded slots plus the tier that produced them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSlots {
    pub grammar: ParsedGrammar,
    pub source: SlotSource,
}

/// Resolve the slots the dispatcher grounds on, deferring to the parser
/// seam only when the deterministic parse is not confident.
///
/// Total by construction — there is no failure mode, only progressively
/// weaker evidence:
///
/// 1. High-confidence grammar wins outright and the parser is never called,
///    so crisp commands cost nothing (tier 2A).
/// 2. Otherwise a configured parser gets one bounded shot (tier 2B). Its
///    output is sanitized before it is believed.
/// 3. A missing, declining, stalled, or fence-failing parser leaves the
///    low-confidence grammar in place (tier 2C).
///
/// Step 3 is why an offline machine still works: the low-confidence
/// grammar still travels, and Stage 2 grounds on the intent's probe
/// text instead of a parsed site name.
#[must_use]
pub fn resolve_slots(
    prompt: &str,
    connected_origin: Option<&url::Url>,
    ctx: &ResolutionContext<'_>,
) -> ResolvedSlots {
    let grammar = crate::intent_resolver::parse_grammar(prompt, connected_origin);
    if grammar.confidence.is_high() {
        return ResolvedSlots {
            grammar,
            source: SlotSource::GrammarFastPath,
        };
    }
    if let Some(parser) = ctx.parser
        && let Some(slots) = parse_prompt_bounded(parser, prompt)
    {
        return ResolvedSlots {
            grammar: ParsedGrammar::from_slots(&slots),
            source: SlotSource::IntentParser,
        };
    }
    ResolvedSlots {
        grammar,
        source: SlotSource::Ungrounded,
    }
}

/// Resolve an entry URL for an ad-hoc prompt.
///
/// Deterministic and offline unless an adapter is configured. The prompt is
/// the only input besides the connected origin (used only to keep portal
/// host words out of noun slots): no intent class, no topic word, and no
/// current-tab URL, so starting on `google.com` or `about:blank` never
/// blocks cross-domain pre-navigation. (The class parameter existed for
/// the deleted route table; every remaining tier reads the prompt itself.)
///
/// Precedence, strongest signal first:
///
/// 0. Explicit domain or URL in the prompt (`open amazon.in`) — the user's
///    typed destination is ground truth, so it precedes every adapter. A
///    malformed one fails closed: a typo is not fixed by a model guess.
/// 1. Connected-account entity, only with a directory wired.
/// 2. Configured LLM adapter, output untrusted until validated.
/// 3. Direct-open grounding ladder — saved shortcut, then fenced domain
///    grounder, then structured directory. An ungrounded site returns `None`
///    so the caller can ask the user instead of scraping a search page.
///    Prompts naming a site plus an artifact noun ("open my profile on the
///    reddit") ground the site alone through the same ladder; the artifact
///    is pursued on the live page, never searched. A site no rung knows
///    falls through to Tier 4 below.
/// 4. Site-ladder retry: the grammar's site slots (`site_context`, then
///    `target_noun`, deduped, non-empty) through the normal site-only
///    ladder ([`resolve_site_entry_url`]); first hit wins. On total miss
///    returns `None` — the honest miss. The engine never navigates to a
///    visible search page: no search template, no guessed TLDs, no SERP
///    scraping.
#[must_use]
pub fn resolve_entry_url(
    prompt: &str,
    connected_origin: Option<&url::Url>,
    ctx: &ResolutionContext<'_>,
) -> Option<ResolvedRoute> {
    // Tier 0: the prompt's own explicit domain or URL. Typed by the user,
    // so it outranks the account directory and the model — neither gets
    // to reinterpret a destination the user spelled out.
    if let Some(url) = explicit_url_in_prompt(prompt) {
        return accept_user_directed(&url, RouteSource::ExplicitDomain);
    }
    // Tier 1: connected-account entity, only with a directory wired.
    if let Some(dir) = ctx.account_dir
        && let Some(url) = resolve_repo_entity(prompt, dir)
    {
        return accept(&url, RouteSource::AccountEntity);
    }
    // Tier 2: configured LLM adapter, output untrusted until validated.
    if let Some(llm) = ctx.llm
        && let Some(url) = llm.propose_url(prompt)
    {
        return accept(&url, RouteSource::LlmFallback);
    }
    // Tier 3: direct-open grounding ladder — the shared site-only ladder
    // ([`resolve_site_entry_url`]) over the target noun. High-confidence
    // single-target opens never touch a search page: the destination
    // comes from the user's own data, a structured directory, a fenced
    // grounder, or nowhere. An ungrounded site returns `None` so the caller
    // can ask the user instead of scraping a search page.
    let grammar = parse_grammar(prompt, connected_origin);
    if is_direct_open(prompt, &grammar) {
        if let Some(target) = grammar.target_noun.as_deref()
            && let Some(route) = resolve_site_entry_url(target, ctx)
        {
            return Some(route);
        }
        // Ungrounded: a miss. The caller surfaces "try a full domain or
        // save a site shortcut" — asking once beats a scraped SERP that may
        // be a challenge page or an ad.
        return None;
    }
    // Tier 3b: in-page goal — the prompt names a site and carries an
    // artifact noun ("open my profile on the reddit"), so it is not a
    // direct open and must never become a search query when the site
    // grounds. Ground ONLY the site through the shared site-only ladder
    // ([`resolve_site_entry_url`]); the artifact is pursued on the live
    // page by the dispatcher, never searched. When no rung knows the site,
    // fall through to Tier 4: with nothing to navigate to, the grammar's
    // site slots get one more pass through the site-only ladder (and in
    // production the directory rung below is always live, so this corner
    // is theoretical).
    //
    // Coordinator veto mirrors `is_direct_open`: "open my profile on
    // reddit and twitter" is a multi-target prompt and must not silently
    // pursue one of its targets.
    let coordinator = crate::intent_resolver::tokens(prompt)
        .iter()
        .any(|token| matches!(token.as_str(), "and" | "or"));
    if !coordinator
        && let (Some(site), Some(_artifact)) = (
            grammar.site_context.as_deref(),
            grammar.artifact_noun.as_deref(),
        )
        && let Some(route) = resolve_site_entry_url(site, ctx)
    {
        return Some(route);
    }
    // 3d. Site ungrounded (or coordinator, or no artifact): fall through
    // to the Tier 4 site-ladder retry below.
    // Tier 4: site-ladder retry over the already-parsed grammar — no
    // visible search page, no guessed TLDs. The grammar's site slots
    // (`site_context`, then `target_noun`, deduped, non-empty) each run
    // the normal site-only ladder; the first hit wins. On total miss
    // return `None`: the caller already renders "try the full domain or
    // save a site shortcut" guidance for ungrounded prompts. Only runs
    // when no stronger tier proposed anything; invalid stronger proposals
    // already returned `None` above without falling through.
    //
    // Multi-target veto, mirroring tier 3b: a prompt naming two sites
    // ("open my profile on reddit and twitter") must not silently pursue
    // one of its targets — honest miss instead of one site's home page.
    if coordinator {
        return None;
    }
    let mut tried: Vec<&str> = Vec::new();
    for site in [
        grammar.site_context.as_deref(),
        grammar.target_noun.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|site| !site.trim().is_empty())
    {
        if tried.contains(&site) {
            continue;
        }
        tried.push(site);
        if let Some(route) = resolve_site_entry_url(site, ctx) {
            return Some(route);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_resolver::{DirectoryError, RepoRef};

    struct FixtureDirectory {
        repos: Vec<RepoRef>,
    }

    impl AccountDirectory for FixtureDirectory {
        fn github_repos(&self) -> Result<Vec<RepoRef>, DirectoryError> {
            Ok(self.repos.clone())
        }
    }

    struct MockLlm {
        answer: Option<String>,
    }

    impl LlmUrlProposer for MockLlm {
        fn propose_url(&self, _prompt: &str) -> Option<String> {
            self.answer.clone()
        }
    }

    fn repo(owner: &str, name: &str) -> RepoRef {
        RepoRef {
            owner: owner.into(),
            name: name.into(),
            html_url: format!("https://github.com/{owner}/{name}"),
        }
    }

    fn empty_ctx() -> ResolutionContext<'static> {
        ResolutionContext {
            account_dir: None,
            llm: None,
            parser: None,
            shortcuts: None,
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        }
    }

    #[test]
    fn tier_order_prefers_account_entity_over_later_tiers() {
        // A connected repo named like the prompt's artifact must win over
        // the tiers below it.
        let dir = FixtureDirectory {
            repos: vec![
                repo("fixture-owner", "invoices"),
                repo("fixture-owner", "website"),
            ],
        };
        let ctx = ResolutionContext {
            account_dir: Some(&dir),
            llm: None,
            parser: None,
            shortcuts: None,
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        };
        let Some(resolved) = resolve_entry_url("download all my invoices from github", None, &ctx)
        else {
            panic!("entity tier resolves");
        };
        assert_eq!(resolved.source, RouteSource::AccountEntity);
        assert_eq!(
            resolved.url.as_str(),
            "https://github.com/fixture-owner/invoices"
        );
    }

    #[test]
    fn entity_resolver_falls_through_to_honest_miss_without_connected_account() {
        // No directory wired, and no ladder rung grounds the site slot
        // either: an entity-dependent prompt is an honest miss, not a
        // search page. The caller turns the miss into "try the full domain
        // or save a site shortcut".
        assert_eq!(
            resolve_entry_url("check out my portopsy on github", None, &empty_ctx()),
            None
        );
    }

    #[test]
    fn direct_open_without_grounding_is_a_miss_not_a_search() {
        // Behavior change, by design: "open amazon for me" is a direct
        // open, so with no shortcut, no explicit domain, and no directory
        // key it misses instead of scraping a search page. The caller
        // turns the miss into "try a full domain or save a site shortcut".
        // TLDs are still never guessed: the miss carries no URL at all.
        assert_eq!(
            resolve_entry_url("open amazon for me", None, &empty_ctx()),
            None
        );
        assert_eq!(resolve_entry_url("open amazon", None, &empty_ctx()), None);
        // Non-direct-open prompts are no different: with no rung grounding
        // the grammar's site slots, they miss honestly too — there is no
        // search page to fall back to.
        assert_eq!(
            resolve_entry_url("download all my invoices from github", None, &empty_ctx()),
            None
        );
        // Empty prompts keep the miss dead-end (no empty search navigation).
        assert_eq!(resolve_entry_url("   ", None, &empty_ctx()), None);
    }

    struct StubSiteSearch {
        answer: Option<String>,
    }

    impl SiteSearchClient for StubSiteSearch {
        fn search_site(&self, _site_name: &str) -> Option<SiteHit> {
            self.answer.clone().map(|url| SiteHit {
                url,
                backend: "stub",
            })
        }
    }

    fn in_page_ctx(search: &StubSiteSearch) -> ResolutionContext<'_> {
        ResolutionContext {
            account_dir: None,
            llm: None,
            parser: None,
            shortcuts: None,
            site_search: Some(search),
            domain_grounder: None,
            region_hint: "",
        }
    }

    #[test]
    fn in_page_goal_grounds_site_never_searches() {
        // "open my profile on the reddit" names a site and carries an
        // artifact noun: the ladder grounds the SITE, and the prompt must
        // never become a search query. The artifact noun is the
        // dispatcher's business, not the URL's.
        let search = StubSiteSearch {
            answer: Some("https://www.reddit.com/".to_owned()),
        };
        let Some(resolved) =
            resolve_entry_url("open my profile on the reddit", None, &in_page_ctx(&search))
        else {
            panic!("in-page goal grounds the site");
        };
        assert_eq!(resolved.source, RouteSource::SiteSearch);
        assert_eq!(resolved.url.as_str(), "https://www.reddit.com/");
    }

    #[test]
    fn in_page_goal_with_ungrounded_site_is_a_miss() {
        // No rung knows the site and there is nowhere to navigate: the
        // prompt is an honest miss — no search page stands in for a
        // destination. (In production the directory rung is always live via
        // the keyless DDG fallback, so this corner is theoretical.)
        let search = StubSiteSearch { answer: None };
        assert_eq!(
            resolve_entry_url("open my profile on the reddit", None, &in_page_ctx(&search)),
            None
        );
    }

    #[test]
    fn coordinated_prompt_is_honest_miss_in_tier_4() {
        // "open my profile on reddit and twitter" is multi-target: tier 3b
        // refuses the in-page goal and tier 4 now vetoes the site ladder
        // too — no silent pursuit of one target's site, honest miss
        // instead of a search page or one site's home page.
        let search = StubSiteSearch {
            answer: Some("https://www.reddit.com/".to_owned()),
        };
        assert!(
            resolve_entry_url(
                "open my profile on reddit and twitter",
                None,
                &in_page_ctx(&search),
            )
            .is_none(),
            "multi-target prompt must miss honestly"
        );
    }

    #[test]
    fn validation_rejects_credentials_and_non_https_across_all_tiers() {
        // Entity tier with a hostile directory URL fails closed without
        // falling through to search.
        struct EvilDirectory;
        impl AccountDirectory for EvilDirectory {
            fn github_repos(&self) -> Result<Vec<RepoRef>, DirectoryError> {
                Ok(vec![RepoRef {
                    owner: "evil".into(),
                    name: "portopsy".into(),
                    html_url: "https://user:secret@github.com/evil/portopsy".into(),
                }])
            }
        }
        let evil_dir = EvilDirectory;
        let ctx = ResolutionContext {
            account_dir: Some(&evil_dir),
            llm: None,
            parser: None,
            shortcuts: None,
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        };
        assert_eq!(
            resolve_entry_url("check out my portopsy on github", None, &ctx),
            None
        );
        // LLM tier: credentials and non-https both fail closed.
        for answer in [
            "https://user:pass@github.com/settings/billing",
            "http://github.com/settings/billing",
            "javascript:alert(1)",
            "data:text/html,hi",
        ] {
            let evil = MockLlm {
                answer: Some(answer.into()),
            };
            let ctx = ResolutionContext {
                account_dir: None,
                llm: Some(&evil),
                parser: None,
                shortcuts: None,
                site_search: None,
                domain_grounder: None,
                region_hint: "",
            };
            assert_eq!(
                resolve_entry_url("open the dashboard thing on github", None, &ctx),
                None,
                "{answer} must fail closed"
            );
        }
        // A direct open the ladder cannot ground fails closed — it never
        // falls back to a search page. "open amazon for me" names a
        // destination, so a miss asks the user rather than scraping a SERP.
        assert_eq!(
            resolve_entry_url("open amazon for me", None, &empty_ctx()),
            None,
            "ungrounded direct open must miss, never search"
        );
        // The site-ladder retry is the last tier and it never invents a
        // destination either: an ungrounded non-direct-open prompt is a
        // miss, not a search page — nothing here can emit credentials,
        // non-https, or a guessed host.
        assert_eq!(
            resolve_entry_url("download the monthly site report", None, &empty_ctx()),
            None,
            "ungrounded non-direct-open prompt must miss, never search"
        );
    }

    #[test]
    fn llm_fallback_output_must_pass_validation() {
        // A hostile model answer fails closed instead of navigating.
        let evil = MockLlm {
            answer: Some("https://github.com.evil.com/x".into()),
        };
        let ctx = ResolutionContext {
            account_dir: None,
            llm: Some(&evil),
            parser: None,
            shortcuts: None,
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        };
        assert_eq!(
            resolve_entry_url("open the dashboard thing on github", None, &ctx),
            None
        );
        // A well-formed answer from a configured adapter flows through.
        let kind = MockLlm {
            answer: Some("https://github.com/settings/billing".into()),
        };
        let ctx = ResolutionContext {
            account_dir: None,
            llm: Some(&kind),
            parser: None,
            shortcuts: None,
            site_search: None,
            domain_grounder: None,
            region_hint: "",
        };
        let Some(resolved) = resolve_entry_url("open the dashboard thing on github", None, &ctx)
        else {
            panic!("valid LLM answer resolves");
        };
        assert_eq!(resolved.source, RouteSource::LlmFallback);
    }

    #[test]
    fn explicit_domain_outranks_configured_adapters() {
        // The user's typed destination is ground truth: neither a saved
        // shortcut, a structured directory, nor a configured model gets
        // to reinterpret it.
        let mut shortcuts = std::collections::HashMap::new();
        shortcuts.insert("github".into(), "https://example.com/other".into());
        let store = InMemoryShortcuts::new(shortcuts);
        let directory = MockDirectory {
            url: Some("https://directory.example/result".into()),
        };
        let llm = MockLlm {
            answer: Some("https://github.com/settings/billing".into()),
        };
        let ctx = ResolutionContext {
            account_dir: None,
            llm: Some(&llm),
            parser: None,
            shortcuts: Some(&store),
            site_search: Some(&directory),
            domain_grounder: None,
            region_hint: "",
        };
        let Some(resolved) = resolve_entry_url("open github.com/pratyush2514", None, &ctx) else {
            panic!("explicit domain resolves");
        };
        assert_eq!(resolved.url.as_str(), "https://github.com/pratyush2514");
        assert_eq!(resolved.source, RouteSource::ExplicitDomain);
    }

    #[test]
    fn resolution_is_prompt_only_with_no_static_route_table() {
        // Nothing between the entity tier and the honest miss: a
        // portal-shaped prompt that the deleted table used to answer now
        // misses instead of inventing a deep link — no static route table,
        // no search page standing in for a destination.
        let ctx = empty_ctx();
        for prompt in [
            "download all my invoices from github",
            "download my github billing invoices",
        ] {
            assert_eq!(
                resolve_entry_url(prompt, None, &ctx),
                None,
                "{prompt} misses honestly"
            );
        }
    }

    struct MockDirectory {
        url: Option<String>,
    }

    impl SiteSearchClient for MockDirectory {
        fn search_site(&self, _site_name: &str) -> Option<SiteHit> {
            self.url.clone().map(|url| SiteHit {
                url,
                backend: "mock",
            })
        }
    }

    fn ladder_ctx<'a>(
        shortcuts: Option<&'a dyn ShortcutStore>,
        site_search: Option<&'a dyn SiteSearchClient>,
        domain_grounder: Option<&'a dyn DomainGrounder>,
        region_hint: &'a str,
    ) -> ResolutionContext<'a> {
        ResolutionContext {
            account_dir: None,
            llm: None,
            parser: None,
            shortcuts,
            site_search,
            domain_grounder,
            region_hint,
        }
    }

    #[test]
    fn ladder_explicit_domain_wins_without_any_lookup() {
        // "open amazon.in" names the destination outright: no shortcut, no
        // directory, no network — and no TLD was guessed, it was typed.
        let ctx = ladder_ctx(None, None, None, "");
        let Some(resolved) = resolve_entry_url("open amazon.in", None, &ctx) else {
            panic!("explicit domain resolves");
        };
        assert_eq!(resolved.source, RouteSource::ExplicitDomain);
        assert_eq!(resolved.url.as_str(), "https://amazon.in/");
        // A full typed URL (with path) travels verbatim.
        let Some(resolved) = resolve_entry_url(
            "open https://github.com/settings/billing for me",
            None,
            &ctx,
        ) else {
            panic!("typed URL resolves");
        };
        assert_eq!(resolved.source, RouteSource::ExplicitDomain);
        assert_eq!(resolved.url.as_str(), "https://github.com/settings/billing");
    }

    #[test]
    fn ladder_explicit_domain_beats_shortcut_and_directory() {
        // The prompt's own domain outranks stored data: this invocation
        // named amazon.in, even if a shortcut says otherwise.
        let shortcuts = InMemoryShortcuts::new(
            [("amazon".to_owned(), "https://www.amazon.com/".to_owned())]
                .into_iter()
                .collect(),
        );
        let directory = MockDirectory {
            url: Some("https://www.amazon.in/".to_owned()),
        };
        let ctx = ladder_ctx(Some(&shortcuts), Some(&directory), None, "");
        let Some(resolved) = resolve_entry_url("open amazon.in", None, &ctx) else {
            panic!("explicit domain resolves");
        };
        assert_eq!(resolved.source, RouteSource::ExplicitDomain);
        assert_eq!(resolved.url.as_str(), "https://amazon.in/");
    }

    #[test]
    fn ladder_shortcut_grounds_a_bare_site_name() {
        // The learned path: "open amazon for me" hits the user's saved
        // shortcut with no network at all — politeness filler included.
        let shortcuts = InMemoryShortcuts::new(
            [("amazon".to_owned(), "https://www.amazon.in/".to_owned())]
                .into_iter()
                .collect(),
        );
        let ctx = ladder_ctx(Some(&shortcuts), None, None, "");
        for prompt in ["open amazon", "open amazon for me", "please open amazon"] {
            let Some(resolved) = resolve_entry_url(prompt, None, &ctx) else {
                panic!("{prompt} resolves via shortcut");
            };
            assert_eq!(resolved.source, RouteSource::Shortcut, "{prompt}");
            assert_eq!(resolved.url.as_str(), "https://www.amazon.in/", "{prompt}");
        }
    }

    #[test]
    fn ladder_site_search_grounds_a_cold_name() {
        // No shortcut, no typed domain, but a directory is configured: the
        // structured lookup grounds the name — still no SERP scraping.
        let directory = MockDirectory {
            url: Some("https://www.amazon.in/".to_owned()),
        };
        let ctx = ladder_ctx(None, Some(&directory), None, "");
        let Some(resolved) = resolve_entry_url("open amazon for me", None, &ctx) else {
            panic!("directory resolves the cold name");
        };
        assert_eq!(resolved.source, RouteSource::SiteSearch);
        assert_eq!(resolved.url.as_str(), "https://www.amazon.in/");
    }

    struct MockGrounder {
        domain: Option<String>,
        expect_region: Option<String>,
    }

    impl DomainGrounder for MockGrounder {
        fn ground_domain(&self, site_name: &str, region_hint: &str) -> Option<String> {
            if let Some(expected) = &self.expect_region {
                assert_eq!(region_hint, expected, "region hint passed through");
            }
            // Only answer for the site the test set up — proves the ladder
            // passes the parsed slot, not the raw prompt.
            if site_name == "amazon" {
                self.domain.clone()
            } else {
                None
            }
        }
    }

    #[test]
    fn ladder_domain_grounder_grounds_with_region_hint() {
        // Muse-like: "open amazon for me" with no shortcut and no directory
        // key still opens directly via the fenced grounder — site slot
        // ("amazon") + region hint ("IN") → bare domain ("amazon.in").
        let grounder = MockGrounder {
            domain: Some("amazon.in".to_owned()),
            expect_region: Some("IN".to_owned()),
        };
        let ctx = ladder_ctx(None, None, Some(&grounder), "IN");
        let Some(resolved) = resolve_entry_url("open amazon for me", None, &ctx) else {
            panic!("grounder resolves the cold name");
        };
        assert_eq!(resolved.source, RouteSource::DomainGrounded);
        assert_eq!(resolved.url.as_str(), "https://amazon.in/");
    }

    #[test]
    fn ladder_site_search_beats_grounder_but_loses_to_shortcut() {
        // Precedence: shortcut (user data) > site directory (ranked search
        // data) > grounder (generative fallback). Ranking is the ground
        // truth of what a site name means; it cannot hallucinate.
        let shortcuts = InMemoryShortcuts::new(
            [("amazon".to_owned(), "https://www.amazon.in/".to_owned())]
                .into_iter()
                .collect(),
        );
        let grounder = MockGrounder {
            domain: Some("amazon.com".to_owned()),
            expect_region: None,
        };
        let directory = MockDirectory {
            // `amazon.in`: the plausibility veto accepts it (registrable
            // label `amazon.in` contains the queried name). `amazon.co.uk`
            // would veto here — the last-two-labels approximation labels it
            // `co.uk` — which is exactly what the veto is for; see the
            // integration tests in `tests/route_veto.rs`.
            url: Some("https://www.amazon.in/".to_owned()),
        };
        // Shortcut wins over directory.
        let ctx = ladder_ctx(Some(&shortcuts), Some(&directory), Some(&grounder), "IN");
        let Some(resolved) = resolve_entry_url("open amazon", None, &ctx) else {
            panic!("shortcut wins");
        };
        assert_eq!(resolved.source, RouteSource::Shortcut);
        // Without shortcut, directory wins over grounder.
        let ctx = ladder_ctx(None, Some(&directory), Some(&grounder), "IN");
        let Some(resolved) = resolve_entry_url("open amazon", None, &ctx) else {
            panic!("directory beats grounder");
        };
        assert_eq!(resolved.source, RouteSource::SiteSearch);
        assert_eq!(resolved.url.as_str(), "https://www.amazon.in/");
        // Without directory, the grounder is the fallback.
        let ctx = ladder_ctx(None, None, Some(&grounder), "IN");
        let Some(resolved) = resolve_entry_url("open amazon", None, &ctx) else {
            panic!("grounder is the fallback");
        };
        assert_eq!(resolved.source, RouteSource::DomainGrounded);
        assert_eq!(resolved.url.as_str(), "https://amazon.com/");
    }

    #[test]
    fn ladder_domain_grounder_malformed_falls_through() {
        // A grounder that returns junk never navigates: malformed domains
        // degrade to the next rung, never to a guessed URL.
        for bad in [
            "not a domain",
            "192.168.1.1",
            "https://amazon.in/", // full URL, not a bare domain
            "amazon",
            "",
        ] {
            let grounder = MockGrounder {
                domain: Some(bad.to_owned()),
                expect_region: None,
            };
            let directory = MockDirectory {
                url: Some("https://www.amazon.in/".to_owned()),
            };
            let ctx = ladder_ctx(None, Some(&directory), Some(&grounder), "IN");
            let Some(resolved) = resolve_entry_url("open amazon", None, &ctx) else {
                panic!("falls through to directory for {bad:?}");
            };
            assert_eq!(
                resolved.source,
                RouteSource::SiteSearch,
                "bad grounder output {bad:?} must not navigate"
            );
        }
        // And when nothing else grounds it, the miss is honest.
        let grounder = MockGrounder {
            domain: Some("bogus!!".to_owned()),
            expect_region: None,
        };
        let ctx = ladder_ctx(None, None, Some(&grounder), "IN");
        assert_eq!(resolve_entry_url("open amazon", None, &ctx), None);
    }

    #[test]
    fn validate_grounded_domain_policy() {
        // Valid bare domains → https URLs.
        assert_eq!(
            validate_grounded_domain("amazon.in").as_deref(),
            Some("https://amazon.in")
        );
        assert_eq!(
            validate_grounded_domain("  WWW.AMAZON.IN. ").as_deref(),
            Some("https://www.amazon.in")
        );
        // Rejected: no TLD, raw IPs, credentials, URLs, empty.
        assert_eq!(validate_grounded_domain("amazon"), None);
        assert_eq!(validate_grounded_domain("192.168.1.1"), None);
        assert_eq!(validate_grounded_domain("::1"), None);
        assert_eq!(validate_grounded_domain("user:pass@amazon.in"), None);
        assert_eq!(validate_grounded_domain("https://amazon.in"), None);
        assert_eq!(validate_grounded_domain(""), None);
        assert_eq!(validate_grounded_domain("amazon.in/path"), None);
    }

    #[test]
    fn region_hint_mapping() {
        assert_eq!(region_hint_from_timezone("Asia/Kolkata"), Some("IN"));
        assert_eq!(region_hint_from_timezone("Asia/Calcutta"), Some("IN"));
        assert_eq!(region_hint_from_timezone("America/New_York"), Some("US"));
        assert_eq!(region_hint_from_timezone("Europe/Paris"), Some("FR"));
        assert_eq!(region_hint_from_timezone("Europe/London"), Some("GB"));
        assert_eq!(region_hint_from_timezone("Mars/Olympus"), None);
    }

    #[test]
    fn stub_grounder_declines() {
        let stub = StubDomainGrounder;
        assert_eq!(stub.ground_domain("amazon", "IN"), None);
        // With only the stub wired, the cold name still misses honestly.
        let ctx = ladder_ctx(None, None, Some(&stub), "IN");
        assert_eq!(resolve_entry_url("open amazon for me", None, &ctx), None);
    }

    #[test]
    fn ladder_malformed_explicit_domain_fails_closed() {
        // A typed destination that is not navigable (non-https) is a miss,
        // not a search: Googling a typo'd scheme fixes nothing.
        let ctx = ladder_ctx(None, None, None, "");
        assert_eq!(resolve_entry_url("open http://amazon.in", None, &ctx), None);
    }

    #[test]
    fn ladder_skips_multi_target_and_retrieval_prompts() {
        // "open amazon and flipkart" must never silently open one target:
        // it is not a direct open, and with no rung grounding the grammar's
        // site slots it misses honestly instead of searching.
        let ctx = empty_ctx();
        assert_eq!(
            resolve_entry_url("open amazon and flipkart", None, &ctx),
            None,
            "multi-target misses, never searches"
        );
        // A retrieval verb is not an open: "find amazon" misses the same
        // way when no rung knows the site.
        assert_eq!(
            resolve_entry_url("find amazon", None, &ctx),
            None,
            "retrieval verb misses, never searches"
        );
    }

    #[test]
    fn explicit_url_shapes() {
        assert_eq!(
            explicit_url_in_prompt("open amazon.in").as_deref(),
            Some("https://amazon.in")
        );
        assert_eq!(
            explicit_url_in_prompt("open amazon.in.").as_deref(),
            Some("https://amazon.in")
        );
        assert_eq!(
            explicit_url_in_prompt("visit github.com/settings").as_deref(),
            Some("https://github.com/settings")
        );
        assert_eq!(
            explicit_url_in_prompt("open https://example.com/a?b=c").as_deref(),
            Some("https://example.com/a?b=c")
        );
        // Not domains: bare names, versions, and numbers stay ungrounded.
        assert_eq!(explicit_url_in_prompt("open amazon"), None);
        assert_eq!(explicit_url_in_prompt("open chapter 2.0"), None);
        assert_eq!(explicit_url_in_prompt("call 911"), None);
        assert_eq!(explicit_url_in_prompt(""), None);
    }

    #[test]
    fn brave_top_url_parses_the_response_envelope() {
        let payload = serde_json::json!({
            "web": { "results": [
                { "title": "Amazon.in", "url": "https://www.amazon.in/" },
                { "title": "Other", "url": "https://example.com/" },
            ]},
        });
        assert_eq!(
            brave_top_url(&payload).as_deref(),
            Some("https://www.amazon.in/")
        );
        assert_eq!(brave_top_url(&serde_json::json!({})), None);
        assert_eq!(
            brave_top_url(&serde_json::json!({"web": {"results": []}})),
            None
        );
    }

    #[test]
    fn brave_client_is_absent_without_a_key() {
        // No key, no client: the ladder degrades to ask-and-learn rather
        // than failing. (The live call itself is never made in tests.)
        let key = std::env::var("CLINCH_BRAVE_API_KEY").ok();
        if key.is_none_or(|key| key.trim().is_empty()) {
            assert!(BraveSiteSearch::from_env().is_none());
        }
    }

    /// Fixture shaped like the live HTML endpoint: a paid ad anchor first,
    /// then organic results. The parser must skip the ad and unwrap the
    /// `uddg` param of the first organic hit.
    const DDG_FIXTURE: &str = r#"
<div class="result results_links results_links_deep result--ad ">
  <div class="links_main links_deep result__body">
    <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fad_domain%3Dask%252Dchat.ai&amp;rut=aaa">Ad</a></h2>
  </div>
</div>
<div class="result results_links results_links_deep web-result ">
  <div class="links_main links_deep result__body">
    <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fclaude.com%2F&amp;rut=bbb">Claude</a></h2>
  </div>
</div>
<div class="result results_links results_links_deep web-result ">
  <div class="links_main links_deep result__body">
    <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FClaude&amp;rut=ccc">Wikipedia</a></h2>
  </div>
</div>
"#;

    #[test]
    fn ddg_top_url_skips_ads_and_unwraps_uddg() {
        assert_eq!(
            ddg_top_url(DDG_FIXTURE).as_deref(),
            Some("https://claude.com/")
        );
    }

    #[test]
    fn ddg_top_url_returns_none_without_organic_results() {
        // Ad-only page: paid results are never destinations.
        let ads_only = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fad_domain%3Dx&amp;rut=a">Ad</a>"#;
        assert_eq!(ddg_top_url(ads_only), None);
        // Anomaly / throttle page and empty bodies carry no results.
        assert_eq!(ddg_top_url(""), None);
        assert_eq!(ddg_top_url("<html><body>anomaly</body></html>"), None);
    }

    #[test]
    fn ddg_unwrap_target_decodes_and_passes_through() {
        assert_eq!(
            ddg_unwrap_target("//duckduckgo.com/l/?uddg=https%3A%2F%2Fclaude.com%2F&amp;rut=x")
                .as_deref(),
            Some("https://claude.com/")
        );
        // Direct links pass through; junk is rejected.
        assert_eq!(
            ddg_unwrap_target("https://example.com/").as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(ddg_unwrap_target("/relative/path"), None);
        assert_eq!(ddg_unwrap_target("javascript:void(0)"), None);
    }

    #[test]
    fn chained_site_search_always_offers_ddg() {
        // The composite rung is never "unconfigured": even without a Brave
        // key the keyless fallback is present. (No network in tests — only
        // the wiring is asserted here.)
        let chain = ChainedSiteSearch {
            primary: None,
            fallback: DuckDuckGoSiteSearch::new(),
        };
        assert_eq!(chain.backend_label(), "ddg");
    }
}

#![deny(unsafe_code)]
//! Live LLM-backed [`DomainGrounder`] adapters.
//!
//! The grounder is the Muse-like rung of the direct-open ladder: it turns a
//! site slot (`amazon`) plus a region hint (`IN`) into a bare domain
//! (`amazon.in`). The fence is strict — only the already-normalized site
//! name and the region code leave the machine. No prompt text, no page HTML,
//! no URLs, no selectors.
//!
//! Two providers, selected by `CLINCH_GROUNDER_PROVIDER`:
//!
//! * `groq` — Groq's OpenAI-compatible chat API, the default cloud path.
//!   The key comes from `GROQ_API_KEY` and is zeroized on drop; without a
//!   key the adapter is not built and the ladder keeps its declining stub,
//!   so a missing key degrades to the honest miss instead of failing.
//! * `ollama` — a local Ollama daemon (`CLINCH_OLLAMA_URL`, default
//!   `http://localhost:11434`) running `CLINCH_OLLAMA_MODEL` (default
//!   `qwen2.5:1.5b`). No key, no traffic beyond localhost.
//!
//! Every response must be strict JSON — `{"domain": "amazon.in"}` — and the
//! extracted domain is returned to the ladder, which validates it in Rust
//! ([`crate::route_proposer::validate_grounded_domain`]) before anything
//! navigates. Any network error, timeout, or malformed response is `None`:
//! offline stays a normal outcome, and a stalled model can never hang
//! dispatch past [`GROUNDER_TIMEOUT`].

use crate::route_proposer::DomainGrounder;
use std::time::Duration;
use zeroize::Zeroizing;

/// Bounded single-pass timeout for one grounding call. Groq answers in
/// milliseconds; a local Ollama daemon may cold-load the model, so the
/// bound is generous but finite — the ladder never waits on a model
/// indefinitely.
const GROUNDER_TIMEOUT: Duration = Duration::from_secs(15);

/// Default Groq OpenAI-compatible base URL. Overridable for tests via
/// `CLINCH_GROQ_BASE_URL`. Shared with the intent-parser adapter so both
/// providers read the same defaults.
pub(crate) const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
/// Default Groq chat model: Groq's recommended replacement for the retired
/// `llama-3.1-8b-instant` (decommissioned 2026-08-16; requests to it fail
/// with a `model_decommissioned` error). Overridable via `CLINCH_GROQ_MODEL`.
/// Groq retires models aggressively — re-check
/// <https://console.groq.com/docs/deprecations> when grounding starts
/// missing; the miss journal line carries the provider's error code.
/// Shared with the intent-parser adapter so both providers read the same
/// defaults.
pub(crate) const GROQ_MODEL: &str = "openai/gpt-oss-20b";
/// Default local Ollama base URL. Overridable via `CLINCH_OLLAMA_URL`.
/// Shared with the intent-parser adapter.
pub(crate) const OLLAMA_BASE_URL: &str = "http://localhost:11434";
/// Small local model that answers JSON reliably. Overridable via
/// `CLINCH_OLLAMA_MODEL`. Shared with the intent-parser adapter.
pub(crate) const OLLAMA_MODEL: &str = "qwen2.5:1.5b";

/// The single instruction both providers receive. It names no sites — the
/// only site knowledge in the whole call is the one slot in the user line.
const SYSTEM_PROMPT: &str = "You are a domain grounder. Given a site name and an ISO region code, reply with ONLY a JSON object like {\"domain\": \"amazon.in\"} containing a bare domain name: no scheme, no path, no credentials, no explanation, no other text. If you are unsure, make your best guess — never return an empty object.";

/// Which live provider backs the grounder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrounderProvider {
    /// Groq cloud API (`GROQ_API_KEY`).
    Groq,
    /// Local Ollama daemon (no key).
    Ollama,
}

impl GrounderProvider {
    /// Parse the `CLINCH_GROUNDER_PROVIDER` value. Case-insensitive,
    /// whitespace-tolerant; anything else is `None`, which keeps the stub.
    #[must_use]
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "groq" => Some(Self::Groq),
            "ollama" => Some(Self::Ollama),
            _ => None,
        }
    }

    /// Short name used in diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Groq => "groq",
            Self::Ollama => "ollama",
        }
    }
}

/// Raw environment values for grounder construction. [`LlmDomainGrounder::from_env`]
/// reads the process environment into this; tests build it directly, so no
/// test ever mutates the process environment.
#[derive(Debug, Default)]
pub struct GrounderEnv {
    /// `CLINCH_GROUNDER_PROVIDER`.
    pub provider: Option<String>,
    /// `GROQ_API_KEY`.
    pub groq_api_key: Option<String>,
    /// `CLINCH_GROQ_BASE_URL`.
    pub groq_base_url: Option<String>,
    /// `CLINCH_GROQ_MODEL`.
    pub groq_model: Option<String>,
    /// `CLINCH_OLLAMA_URL`.
    pub ollama_url: Option<String>,
    /// `CLINCH_OLLAMA_MODEL`.
    pub ollama_model: Option<String>,
}

/// LLM-backed [`DomainGrounder`]. Synchronous by trait contract: the one
/// HTTP call is bounded by [`GROUNDER_TIMEOUT`], and every failure mode —
/// DNS, TLS, timeout, non-JSON, wrong shape — is `None`, never an error.
pub struct LlmDomainGrounder {
    pub provider: GrounderProvider,
    api_key: Zeroizing<String>,
    pub base_url: String,
    pub model: String,
    agent: ureq::Agent,
    /// Sanitized failure detail from the most recent `ground_domain` call
    /// (`None` when the last call succeeded or declined without a provider
    /// failure). Never holds credentials: the key travels only in the
    /// Authorization header, which never appears in error text.
    last_error: std::sync::Mutex<Option<String>>,
}

impl LlmDomainGrounder {
    /// Build from the environment, or `None` when no live provider is
    /// configured. `groq` additionally requires `GROQ_API_KEY`; an empty or
    /// missing key means the adapter is not built at all — the caller keeps
    /// the declining stub instead of a half-configured client.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_env_values(&GrounderEnv {
            provider: std::env::var("CLINCH_GROUNDER_PROVIDER").ok(),
            groq_api_key: std::env::var("GROQ_API_KEY").ok(),
            groq_base_url: std::env::var("CLINCH_GROQ_BASE_URL").ok(),
            groq_model: std::env::var("CLINCH_GROQ_MODEL").ok(),
            ollama_url: std::env::var("CLINCH_OLLAMA_URL").ok(),
            ollama_model: std::env::var("CLINCH_OLLAMA_MODEL").ok(),
        })
    }

    /// Build from explicit values, or `None` when they select no live
    /// provider. Pure — tests exercise the whole selection matrix without
    /// touching the process environment (whose mutation is `unsafe` under
    /// this crate's edition).
    #[must_use]
    pub fn from_env_values(env: &GrounderEnv) -> Option<Self> {
        let provider = GrounderProvider::from_env_value(env.provider.as_deref().unwrap_or(""))?;
        match provider {
            GrounderProvider::Groq => {
                let key = env.groq_api_key.as_deref().unwrap_or("").trim();
                if key.is_empty() {
                    return None;
                }
                Some(Self::groq(
                    key,
                    env.groq_base_url.as_deref().unwrap_or(""),
                    env.groq_model.as_deref().unwrap_or(""),
                ))
            }
            GrounderProvider::Ollama => Some(Self::ollama(
                env.ollama_url.as_deref().unwrap_or(""),
                env.ollama_model.as_deref().unwrap_or(""),
            )),
        }
    }

    /// Groq adapter against an explicit base URL and model. Empty strings
    /// fall back to the defaults; the test suite points the base URL at a
    /// loopback mock.
    #[must_use]
    pub fn groq(api_key: &str, base_url: &str, model: &str) -> Self {
        Self::new(
            GrounderProvider::Groq,
            api_key,
            if base_url.trim().is_empty() {
                GROQ_BASE_URL
            } else {
                base_url.trim().trim_end_matches('/')
            },
            if model.trim().is_empty() {
                GROQ_MODEL
            } else {
                model.trim()
            },
        )
    }

    /// Ollama adapter against an explicit base URL and model. Empty strings
    /// fall back to the defaults; the test suite points the base URL at a
    /// loopback mock.
    #[must_use]
    pub fn ollama(base_url: &str, model: &str) -> Self {
        Self::new(
            GrounderProvider::Ollama,
            "",
            if base_url.trim().is_empty() {
                OLLAMA_BASE_URL
            } else {
                base_url.trim().trim_end_matches('/')
            },
            if model.trim().is_empty() {
                OLLAMA_MODEL
            } else {
                model.trim()
            },
        )
    }

    fn new(provider: GrounderProvider, api_key: &str, base_url: &str, model: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(GROUNDER_TIMEOUT))
            // Error statuses must stay readable: the provider's error code
            // (e.g. Groq's `model_decommissioned`) is the diagnostic that
            // tells a provider failure apart from a clean decline.
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            provider,
            api_key: Zeroizing::new(api_key.to_owned()),
            base_url: base_url.to_owned(),
            model: model.to_owned(),
            agent,
            last_error: std::sync::Mutex::new(None),
        }
    }

    /// Sanitized failure detail from the most recent [`DomainGrounder::ground_domain`]
    /// call, or `None` when the last call succeeded or declined without a
    /// provider failure. Never contains credentials.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|guard| guard.clone())
    }

    /// Record a sanitized provider failure for [`Self::last_error`]. Also
    /// echoes to stderr: under `npm run tauri dev` the terminal is the
    /// reliable channel when the UI is the thing misbehaving. Sanitized —
    /// never contains credentials.
    fn record_error(&self, detail: String) {
        eprintln!("[clinch:grounder] {detail}");
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = Some(detail);
        }
    }

    /// POST a JSON body and parse the JSON response, or return a sanitized
    /// one-line failure: transport/timeout kind, HTTP status plus the
    /// provider's error code, or malformed body. The detail never includes
    /// credentials — the key travels only in the Authorization header.
    fn post_json(
        &self,
        provider: &str,
        url: &str,
        auth: Option<&str>,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let request = self.agent.post(url);
        let request = match auth {
            Some(token) => request.header("Authorization", token),
            None => request,
        };
        let response = request.send_json(body).map_err(|err| {
            let message = err.to_string().to_lowercase();
            let kind = if message.contains("timed out") || message.contains("timeout") {
                "timeout"
            } else {
                "transport error"
            };
            format!("{provider} request failed ({kind})")
        })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!(
                "{provider} http {status} ({})",
                error_body_detail(provider, response.into_body())
            ));
        }
        response
            .into_body()
            .read_json::<serde_json::Value>()
            .map_err(|_| format!("{provider} malformed response body"))
    }

    /// One Groq chat completion; the assistant message content is the raw
    /// strict-JSON payload (or `None` on any failure, with the sanitized
    /// cause recorded for [`Self::last_error`]).
    fn groq_completion(&self, site: &str, region: &str) -> Option<String> {
        let auth = format!("Bearer {}", self.api_key.as_str());
        // gpt-oss is a reasoning model: its reasoning tokens draw from
        // max_tokens before any content is emitted. A 32-token budget was
        // consumed entirely by reasoning — Groq answered HTTP 200 with
        // finish_reason "length" and an empty content string. 256 leaves
        // headroom for the ~10-token JSON answer; the cost is negligible.
        let mut body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 256,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": format!("site: {site}\nregion: {region}")},
            ],
        });
        // reasoning_effort is a 400 on models that cannot reason — gate it
        // on the gpt-oss family, whose default "medium" effort would
        // otherwise spend the budget thinking instead of answering.
        if self.model.contains("gpt-oss") {
            body["reasoning_effort"] = serde_json::Value::String("low".to_owned());
        }
        let payload = match self.post_json(
            "groq",
            &format!("{}/chat/completions", self.base_url),
            Some(auth.as_str()),
            body,
        ) {
            Ok(payload) => payload,
            Err(detail) => {
                self.record_error(detail);
                return None;
            }
        };
        let first_choice = payload
            .get("choices")
            .and_then(|choices| choices.as_array())
            .and_then(|choices| choices.first());
        let content: Option<String> = first_choice
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
            .map(str::to_owned);
        match content {
            Some(content) if !content.trim().is_empty() => Some(content),
            Some(_) => {
                // HTTP 200 with an empty string: the model spent its token
                // budget reasoning. finish_reason names the cause without
                // any model output, so there is nothing to sanitize.
                let reason = first_choice
                    .and_then(|choice| choice.get("finish_reason"))
                    .and_then(|reason| reason.as_str())
                    .unwrap_or("unknown");
                self.record_error(format!(
                    "groq returned empty content (finish_reason: {reason})"
                ));
                None
            }
            None => {
                self.record_error(
                    "groq malformed response (no choices/message/content)".to_owned(),
                );
                None
            }
        }
    }

    /// One Ollama generation with `format: "json"`; the `response` field is
    /// the raw strict-JSON payload (or `None` on any failure, with the
    /// sanitized cause recorded for [`Self::last_error`]).
    fn ollama_generate(&self, site: &str, region: &str) -> Option<String> {
        let payload = match self.post_json(
            "ollama",
            &format!("{}/api/generate", self.base_url),
            None,
            serde_json::json!({
                "model": self.model,
                "stream": false,
                "format": "json",
                "prompt": format!("{SYSTEM_PROMPT}\nsite: {site}\nregion: {region}"),
            }),
        ) {
            Ok(payload) => payload,
            Err(detail) => {
                self.record_error(detail);
                return None;
            }
        };
        if let Some(response) = payload.get("response").and_then(|r| r.as_str()) {
            Some(response.to_owned())
        } else {
            self.record_error("ollama malformed response (no response field)".to_owned());
            None
        }
    }
}

/// The useful fragment of an HTTP error body: Groq's OpenAI-compatible
/// envelope carries `{"error": {"code": ...}}` (e.g. `model_decommissioned`,
/// `invalid_api_key`, `rate_limit_exceeded`); anything else is returned as a
/// truncated raw excerpt. Capped at 160 chars — error payloads are small,
/// and the journal line must stay one line.
fn error_body_detail(provider: &str, mut body: ureq::Body) -> String {
    let text = body.read_to_string().unwrap_or_default();
    let excerpt: String = text.chars().take(160).collect();
    let groq_code = (provider == "groq")
        .then(|| groq_error_code(&excerpt))
        .flatten();
    if let Some(code) = groq_code {
        return code;
    }
    if excerpt.trim().is_empty() {
        "empty error body".to_owned()
    } else {
        excerpt
    }
}

/// Groq's OpenAI-compatible error envelope carries
/// `{"error": {"code": ...}}` (e.g. `model_decommissioned`,
/// `invalid_api_key`, `rate_limit_exceeded`).
fn groq_error_code(excerpt: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(excerpt)
        .ok()?
        .pointer("/error/code")?
        .as_str()
        .map(str::to_owned)
}

/// Extract the `domain` field from a grounder's strict-JSON response.
///
/// Accepts `{"domain": "amazon.in"}` with optional surrounding whitespace or
/// markdown fences (models wrap fenced output even when told not to); extra
/// fields are ignored. Anything else — prose, a bare domain, a missing or
/// non-string `domain` — is `None`, and the ladder degrades to its next
/// rung. This parses only; [`crate::route_proposer::validate_grounded_domain`]
/// decides whether the domain may be navigated.
fn parse_domain(object: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(object).ok()?;
    value.get("domain")?.as_str().map(str::to_owned)
}

pub fn extract_domain(content: &str) -> Option<String> {
    let trimmed = content.trim();
    // Models decorate the JSON they were asked to emit bare: strip a
    // surrounding markdown fence, then surrounding quotes, then whitespace.
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    let unquoted = unfenced
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            unfenced
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .map_or(unfenced, str::trim);
    // Whole-string parse first (the contract), then a salvage pass over the
    // first `{`…last `}` span, for models that wrap the JSON in prose.
    parse_domain(unquoted).or_else(|| {
        let start = unquoted.find('{')?;
        let end = unquoted.rfind('}')?;
        if end <= start {
            return None;
        }
        parse_domain(unquoted[start..=end].trim())
    })
}

impl DomainGrounder for LlmDomainGrounder {
    fn ground_domain(&self, site_name: &str, region_hint: &str) -> Option<String> {
        // A fresh call supersedes any previous failure detail: `last_error`
        // always describes the call that just ran.
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = None;
        }
        // The fence: only the normalized slot and the region code are ever
        // sent. An empty slot never touches the network.
        let site = site_name.trim();
        if site.is_empty() {
            return None;
        }
        // A verb is never a site name: grounding `open` would navigate to
        // `open.com`. Decline without touching the network — the grammar
        // normally prevents this, and the fence holds even if a future
        // parser seam slips one through.
        if crate::intent_resolver::is_action_verb(&site.to_ascii_lowercase()) {
            return None;
        }
        let content = match self.provider {
            GrounderProvider::Groq => self.groq_completion(site, region_hint.trim()),
            GrounderProvider::Ollama => self.ollama_generate(site, region_hint.trim()),
        }?;
        if let Some(domain) = extract_domain(&content) {
            Some(domain)
        } else {
            // HTTP 200 but nothing usable came back. This used to die
            // silent as a "clean decline"; surface the raw model output
            // (debug-formatted and truncated) so the terminal and the
            // journal show what the model actually emitted. An empty
            // object is the model's abstention, not a parse failure —
            // name it so the journal distinguishes the two.
            let preview: String = content.chars().take(300).collect();
            let preview = if content.chars().count() > 300 {
                format!("{preview}…")
            } else {
                preview
            };
            let kind = if content.trim() == "{}" {
                "returned an empty object instead of grounding"
            } else {
                "returned unparseable content"
            };
            self.record_error(format!("{} {kind}: {preview:?}", self.provider.as_str()));
            None
        }
    }
}

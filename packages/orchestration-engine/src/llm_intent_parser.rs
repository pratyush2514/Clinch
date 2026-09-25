#![deny(unsafe_code)]
//! Live LLM-backed [`IntentParser`] adapter: the real Tier 2B.
//!
//! The seam ([`crate::intent_parser`]) shipped with [`crate::StubIntentParser`],
//! so irregular phrasing degraded straight to search garbage — `"can you
//! open the X for me and make sure ill do the login first"` parsed
//! `target="login"`, and neither prompt ever reached a parser because the
//! confidence gate only consults the seam on low confidence (which the old
//! buried-verb check never reported). This module plugs a model into that
//! seam behind the same fence.
//!
//! What the model may return is unchanged: slots, and only slots —
//! `{ action, artifact_noun, site_context }`. The adapter returns raw
//! [`ParsedSlots`] and the existing seam
//! ([`crate::parse_prompt_bounded`] → [`ParsedSlots::sanitized`]) applies
//! the fence; this module neither weakens nor reimplements it. A hostile
//! or sloppy model still degrades to search.
//!
//! Provider selection reuses the grounder's env scheme
//! ([`crate::domain_grounder`]): `CLINCH_GROUNDER_PROVIDER` (`groq`,
//! default cloud path, or `ollama`, local), `GROQ_API_KEY` (zeroized on
//! drop), `CLINCH_GROQ_MODEL` (default `openai/gpt-oss-20b`). Without a
//! configured provider [`LlmIntentParser::from_env`] returns `None` and the
//! caller keeps the declining stub — offline stays the default.
//!
//! Bounded like the grounder: one blocking HTTP POST with a 15s client
//! timeout as a backstop. The seam's own 1.5s bound governs in practice —
//! [`crate::PARSER_TIMEOUT_MS`] — so a stalled provider costs the caller
//! the timeout and nothing more.

use crate::domain_grounder::{GrounderEnv, GrounderProvider};
use crate::intent_parser::{IntentParser, ParsedSlots};
use std::time::Duration;
use zeroize::Zeroizing;

/// Backstop for one slot-parse HTTP round trip. The seam's
/// [`crate::PARSER_TIMEOUT_MS`] bound governs in practice — a stalled
/// provider is abandoned at 1.5s by the caller — so this only bounds the
/// detached worker the timeout leaves behind.
const PARSER_HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// The single instruction both providers receive. It names no sites — the
/// only site knowledge in the whole call is the one prompt in the user
/// line. `action` is phrased as the closed UI-target vocabulary the fence
/// already enforces ([`crate::intent_resolver::INTENT_ROLES`]): `link` for
/// open/navigate/go-to, `button`, `textbox`, `combobox`.
const SYSTEM_PROMPT: &str = "You are an intent slot extractor. Given a user's command, reply with ONLY a JSON object with exactly these three keys — \"action\", \"artifact_noun\", \"site_context\" — and no other text, no explanation, no markdown, no extra keys.\n\n- \"action\": the kind of UI target the command acts on — \"link\" for open/navigate/go-to commands, \"button\", \"textbox\", or \"combobox\". One of those four words, nothing else.\n- \"artifact_noun\": the thing acted on, as a single lowercase word (for example \"invoice\"), or null when the command names none.\n- \"site_context\": the site or service named in the command, as a single lowercase word (for example \"github\"), or null when none is named.\n\nBoth nouns are bare single tokens: never a URL, selector, path, code, or multi-word phrase.\n\nExample: \"can you open the amazon for me\" → {\"action\":\"link\",\"artifact_noun\":null,\"site_context\":\"amazon\"}.";

/// The strict JSON contract a provider answers with. `deny_unknown_fields`
/// rejects a model that invents keys; a missing `action` fails the parse.
/// Missing noun slots read as `None` — declining a slot is legitimate, and
/// the fence normalizes blank the same way.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SlotsResponse {
    action: String,
    artifact_noun: Option<String>,
    site_context: Option<String>,
}

/// LLM-backed [`IntentParser`]. Synchronous by trait contract: the one
/// HTTP call is bounded by [`PARSER_HTTP_TIMEOUT`], and every failure mode —
/// DNS, TLS, timeout, non-JSON, wrong shape — is `None`, never an error.
/// Returned slots are raw; the fence runs in [`crate::parse_prompt_bounded`].
pub struct LlmIntentParser {
    provider: GrounderProvider,
    api_key: Zeroizing<String>,
    base_url: String,
    model: String,
    agent: ureq::Agent,
}

impl LlmIntentParser {
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
                crate::domain_grounder::GROQ_BASE_URL
            } else {
                base_url.trim().trim_end_matches('/')
            },
            if model.trim().is_empty() {
                crate::domain_grounder::GROQ_MODEL
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
                crate::domain_grounder::OLLAMA_BASE_URL
            } else {
                base_url.trim().trim_end_matches('/')
            },
            if model.trim().is_empty() {
                crate::domain_grounder::OLLAMA_MODEL
            } else {
                model.trim()
            },
        )
    }

    fn new(provider: GrounderProvider, api_key: &str, base_url: &str, model: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(PARSER_HTTP_TIMEOUT))
            // Error statuses must stay readable as `None`: the seam treats
            // every provider failure as a decline, so there is nothing to
            // diagnose — just don't let an error status become a panic.
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            provider,
            api_key: Zeroizing::new(api_key.to_owned()),
            base_url: base_url.to_owned(),
            model: model.to_owned(),
            agent,
        }
    }

    /// POST a JSON body and parse the JSON response. `None` on transport
    /// failure, timeout, non-2xx status, or malformed body — every provider
    /// failure is a decline, never an error. The key travels only in the
    /// Authorization header, which never appears in any output.
    fn post_json(
        &self,
        url: &str,
        auth: Option<&str>,
        body: serde_json::Value,
    ) -> Option<serde_json::Value> {
        let request = self.agent.post(url);
        let request = match auth {
            Some(token) => request.header("Authorization", token),
            None => request,
        };
        let response = request.send_json(body).ok()?;
        if !(200..300).contains(&response.status().as_u16()) {
            return None;
        }
        response.into_body().read_json::<serde_json::Value>().ok()
    }

    /// One Groq chat completion; the assistant message content is the raw
    /// strict-JSON payload, or `None` on any failure.
    fn groq_completion(&self, prompt: &str) -> Option<String> {
        let auth = format!("Bearer {}", self.api_key.as_str());
        // gpt-oss is a reasoning model: its reasoning tokens draw from
        // max_tokens before any content is emitted, so the budget leaves
        // headroom for the short JSON answer (mirrors the grounder).
        let mut body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 256,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": prompt},
            ],
        });
        // reasoning_effort is a 400 on models that cannot reason — gate it
        // on the gpt-oss family (mirrors the grounder).
        if self.model.contains("gpt-oss") {
            body["reasoning_effort"] = serde_json::Value::String("low".to_owned());
        }
        let payload = self.post_json(
            &format!("{}/chat/completions", self.base_url),
            Some(auth.as_str()),
            body,
        )?;
        let content = payload
            .get("choices")
            .and_then(|choices| choices.as_array())
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())?;
        (!content.trim().is_empty()).then(|| content.to_owned())
    }

    /// One Ollama generation with `format: "json"`; the `response` field is
    /// the raw strict-JSON payload, or `None` on any failure.
    fn ollama_generate(&self, prompt: &str) -> Option<String> {
        let payload = self.post_json(
            &format!("{}/api/generate", self.base_url),
            None,
            serde_json::json!({
                "model": self.model,
                "stream": false,
                "format": "json",
                "prompt": format!("{SYSTEM_PROMPT}\n{prompt}"),
            }),
        )?;
        let response = payload.get("response").and_then(|r| r.as_str())?;
        (!response.trim().is_empty()).then(|| response.to_owned())
    }
}

/// Parse the provider's strict-JSON answer into slots.
///
/// Whole-string parse first (the contract), then a salvage pass over the
/// first `{`…last `}` span for models that wrap the JSON in prose or a
/// markdown fence. `deny_unknown_fields` and the required `action` key
/// make a malformed or inventive answer `None` — the seam then degrades to
/// search. This parses only; [`ParsedSlots::sanitized`] decides whether the
/// values may be believed.
fn extract_slots(content: &str) -> Option<SlotsResponse> {
    let trimmed = content.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    serde_json::from_str(unfenced).ok().or_else(|| {
        let start = unfenced.find('{')?;
        let end = unfenced.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str(unfenced[start..=end].trim()).ok()
    })
}

impl IntentParser for LlmIntentParser {
    fn parse_prompt(&self, prompt: &str) -> Option<ParsedSlots> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return None;
        }
        let content = match self.provider {
            GrounderProvider::Groq => self.groq_completion(prompt),
            GrounderProvider::Ollama => self.ollama_generate(prompt),
        }?;
        let slots = extract_slots(&content)?;
        // Raw on purpose: the fence (bare-token check, closed action
        // vocabulary) runs in `parse_prompt_bounded` via `sanitized()`,
        // so a hostile payload is rejected after the call, not trusted
        // at the boundary.
        Some(ParsedSlots {
            action: slots.action,
            artifact_noun: slots.artifact_noun,
            site_context: slots.site_context,
        })
    }
}

#![deny(unsafe_code)]
//! Live LLM-backed [`macro_engine::PageNavigator`] adapters.
//!
//! This is the Muse-like rung of the in-page follow-up: when the
//! deterministic heuristics in [`macro_engine::pursue_page_goal`] find no
//! control mentioning the goal noun, a small model makes the one decision
//! heuristics cannot — *which element advances the goal?*
//!
//! The fence is structural:
//!
//! * Only the goal string and the rendered element list (id, role, name,
//!   landmark, coarse position zone) leave the machine. No prompt text beyond
//!   the goal, no page HTML, no cookies, no URLs.
//! * The model's reply must deserialize into [`macro_engine::PageAction`],
//!   a closed three-variant enum — this is the typesafe integration, and
//!   [`serde`] already provides it. No JSON-schema validator or
//!   tool-calling library is involved; anything outside `click` / `done` /
//!   `give_up` fails to parse and the navigator declines.
//! * The picked element id is validated against the live snapshot by the
//!   pursuit loop before anything clicks.
//!
//! Provider selection mirrors the domain grounder via
//! `CLINCH_NAVIGATOR_PROVIDER` (`groq` | `ollama`); unset means no
//! navigator and the follow-up stays purely deterministic. Groq needs
//! `GROQ_API_KEY` (zeroized on drop); Ollama needs only the local daemon.

use crate::domain_grounder::GrounderProvider;
use macro_engine::{MAX_NAVIGATOR_ELEMENTS, PageAction, PageNavigator, PositionZone};
use std::time::Duration;
use zeroize::Zeroizing;

/// Bounded single-pass timeout for one navigation decision. Same bound as
/// the domain grounder: Groq answers in milliseconds, a local Ollama daemon
/// may cold-load the model, and the pursuit loop never waits indefinitely.
const NAVIGATOR_TIMEOUT: Duration = Duration::from_secs(15);

const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
const GROQ_MODEL: &str = "openai/gpt-oss-20b";
const OLLAMA_BASE_URL: &str = "http://localhost:11434";
const OLLAMA_MODEL: &str = "qwen2.5:1.5b";

/// Visible names are truncated so one long label cannot eat the budget.
const MAX_NAME_CHARS: usize = 60;

/// The single instruction both providers receive. It names no sites and no
/// controls — the only page knowledge in the call is the rendered element
/// list in the user line.
const SYSTEM_PROMPT: &str = "You are a web page navigator. Given a goal and a numbered list of page elements, reply with ONLY one JSON object describing the next single action — no other text.\n\n{\"action\": \"click\", \"target\": 42} — click the element with this id\n{\"action\": \"done\"} — the goal is already achieved on this page; nothing to click\n{\"action\": \"give_up\", \"reason\": \"brief reason\"} — no element can advance the goal\n\nRules: target must be an id from the list. Prefer elements whose visible name relates to the goal. A [zone] suffix like [top-right] names the element's coarse on-page position — account controls usually live there. If the goal hides behind a menu, click the menu button first.";

/// Raw environment values for navigator construction.
/// [`LlmPageNavigator::from_env`] reads the process environment into this;
/// tests build it directly, so no test ever mutates the process environment.
#[derive(Debug, Default)]
pub struct NavigatorEnv {
    /// `CLINCH_NAVIGATOR_PROVIDER`.
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

/// LLM-backed [`PageNavigator`]. Synchronous by trait contract: the one
/// HTTP call is bounded by [`NAVIGATOR_TIMEOUT`], and every failure mode —
/// DNS, TLS, timeout, non-JSON, wrong shape — is `None`, never an error.
pub struct LlmPageNavigator {
    provider: GrounderProvider,
    api_key: Zeroizing<String>,
    base_url: String,
    model: String,
    agent: ureq::Agent,
}

impl LlmPageNavigator {
    /// Build from the environment, or `None` when no live provider is
    /// configured. `CLINCH_NAVIGATOR_PROVIDER` must name `groq` or `ollama`
    /// — unset keeps the follow-up purely deterministic. `groq`
    /// additionally requires `GROQ_API_KEY`.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_env_values(&NavigatorEnv {
            provider: std::env::var("CLINCH_NAVIGATOR_PROVIDER").ok(),
            groq_api_key: std::env::var("GROQ_API_KEY").ok(),
            groq_base_url: std::env::var("CLINCH_GROQ_BASE_URL").ok(),
            groq_model: std::env::var("CLINCH_GROQ_MODEL").ok(),
            ollama_url: std::env::var("CLINCH_OLLAMA_URL").ok(),
            ollama_model: std::env::var("CLINCH_OLLAMA_MODEL").ok(),
        })
    }

    /// Build from explicit values, or `None` when they select no live
    /// provider. Pure — tests exercise the selection matrix without
    /// touching the process environment.
    #[must_use]
    pub fn from_env_values(env: &NavigatorEnv) -> Option<Self> {
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
    /// fall back to the defaults.
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
            .timeout_global(Some(NAVIGATOR_TIMEOUT))
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

    /// POST a JSON body and parse the JSON response, or return a sanitized
    /// one-line failure. Mirrors the grounder's contract: the detail never
    /// includes credentials — the key travels only in the Authorization
    /// header.
    fn post_json(
        &self,
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
            format!("navigator request failed ({kind})")
        })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("navigator http {status}"));
        }
        response
            .into_body()
            .read_json::<serde_json::Value>()
            .map_err(|_| "navigator malformed response body".to_owned())
    }

    /// One Groq chat completion; the assistant message content is the raw
    /// strict-JSON payload, or `None` on any failure.
    fn groq_completion(&self, goal: &str, elements: &str) -> Option<String> {
        let auth = format!("Bearer {}", self.api_key.as_str());
        // gpt-oss is a reasoning model: its reasoning tokens draw from
        // max_tokens before any content is emitted. 256 leaves headroom for
        // the ~15-token JSON answer; the cost is negligible.
        let mut body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 256,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": format!("goal: {goal}\nelements:\n{elements}")},
            ],
        });
        if self.model.contains("gpt-oss") {
            body["reasoning_effort"] = serde_json::Value::String("low".to_owned());
        }
        let payload = self
            .post_json(
                &format!("{}/chat/completions", self.base_url),
                Some(auth.as_str()),
                body,
            )
            .ok()?;
        payload
            .get("choices")?
            .as_array()?
            .first()?
            .get("message")?
            .get("content")?
            .as_str()
            .filter(|content| !content.trim().is_empty())
            .map(str::to_owned)
    }

    /// One Ollama generation with `format: "json"`; the `response` field is
    /// the raw strict-JSON payload, or `None` on any failure.
    fn ollama_generate(&self, goal: &str, elements: &str) -> Option<String> {
        let payload = self
            .post_json(
                &format!("{}/api/generate", self.base_url),
                None,
                serde_json::json!({
                    "model": self.model,
                    "stream": false,
                    "format": "json",
                    "prompt": format!("{SYSTEM_PROMPT}\ngoal: {goal}\nelements:\n{elements}"),
                }),
            )
            .ok()?;
        payload.get("response")?.as_str().map(str::to_owned)
    }
}

impl PageNavigator for LlmPageNavigator {
    fn next_action(
        &self,
        goal: &str,
        elements: &[browser_driver::AxElement],
    ) -> Option<PageAction> {
        let zones = vec![None; elements.len().min(MAX_NAVIGATOR_ELEMENTS)];
        self.next_action_zoned(goal, elements, &zones)
    }

    fn next_action_zoned(
        &self,
        goal: &str,
        elements: &[browser_driver::AxElement],
        zones: &[Option<PositionZone>],
    ) -> Option<PageAction> {
        let rendered = render_elements(elements, zones);
        let content = match self.provider {
            GrounderProvider::Groq => self.groq_completion(goal, &rendered)?,
            GrounderProvider::Ollama => self.ollama_generate(goal, &rendered)?,
        };
        parse_page_action(&content)
    }
}

/// Render the snapshot's actionable elements for the model: one line per
/// element — `[id] role "name" (landmark) [zone]`. Capped at
/// [`MAX_NAVIGATOR_ELEMENTS`] lines with truncated names so the prompt
/// stays bounded; header controls (where follow-up targets live) come
/// first in snapshot document order. `zones[i]` describes line `i`;
/// missing zones render as nothing rather than a guess.
fn render_elements(
    elements: &[browser_driver::AxElement],
    zones: &[Option<PositionZone>],
) -> String {
    elements
        .iter()
        .take(MAX_NAVIGATOR_ELEMENTS)
        .enumerate()
        .map(|(index, element)| {
            let name: String = element.name.chars().take(MAX_NAME_CHARS).collect();
            let landmark = element
                .landmark
                .as_deref()
                .map(|landmark| format!(" ({landmark})"))
                .unwrap_or_default();
            let zone = zones
                .get(index)
                .copied()
                .flatten()
                .map(|zone| format!(" [{zone}]"))
                .unwrap_or_default();
            format!(
                "[{}] {} \"{name}\"{landmark}{zone}",
                element.backend_node_id, element.role
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse one [`PageAction`] from the model's strict-JSON reply. Tolerates
/// the decorations models add despite instructions — surrounding markdown
/// fences, wrapping quotes, prose around the object — exactly like the
/// domain grounder's extractor. Anything that still isn't one of the three
/// action shapes is `None`: this is the typesafe boundary, and it declines
/// rather than guessing.
fn parse_page_action(content: &str) -> Option<PageAction> {
    let trimmed = content.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    // Whole-string parse first (the contract), then a salvage pass over the
    // first `{`…last `}` span, for models that wrap the JSON in prose.
    serde_json::from_str(unfenced).ok().or_else(|| {
        let start = unfenced.find('{')?;
        let end = unfenced.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str(unfenced[start..=end].trim()).ok()
    })
}

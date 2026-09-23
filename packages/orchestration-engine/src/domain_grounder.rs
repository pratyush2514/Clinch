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
/// `CLINCH_GROQ_BASE_URL`.
const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
/// Default Groq chat model: Groq's recommended replacement for the retired
/// `llama-3.1-8b-instant` (decommissioned 2026-08-16; requests to it fail
/// with a `model_decommissioned` error). Overridable via `CLINCH_GROQ_MODEL`.
/// Groq retires models aggressively — re-check
/// <https://console.groq.com/docs/deprecations> when grounding starts
/// missing; the miss journal line carries the provider's error code.
const GROQ_MODEL: &str = "openai/gpt-oss-20b";
/// Default local Ollama base URL. Overridable via `CLINCH_OLLAMA_URL`.
const OLLAMA_BASE_URL: &str = "http://localhost:11434";
/// Small local model that answers JSON reliably. Overridable via
/// `CLINCH_OLLAMA_MODEL`.
const OLLAMA_MODEL: &str = "qwen2.5:1.5b";

/// The single instruction both providers receive. It names no sites — the
/// only site knowledge in the whole call is the one slot in the user line.
const SYSTEM_PROMPT: &str = "You are a domain grounder. Given a site name and an ISO region code, reply with ONLY a JSON object like {\"domain\": \"amazon.in\"} containing a bare domain name: no scheme, no path, no credentials, no explanation, no other text.";

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
    provider: GrounderProvider,
    api_key: Zeroizing<String>,
    base_url: String,
    model: String,
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

    /// Record a sanitized provider failure for [`Self::last_error`].
    fn record_error(&self, detail: String) {
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
        let payload = match self.post_json(
            "groq",
            &format!("{}/chat/completions", self.base_url),
            Some(auth.as_str()),
            serde_json::json!({
                "model": self.model,
                "temperature": 0,
                "max_tokens": 32,
                "messages": [
                    {"role": "system", "content": SYSTEM_PROMPT},
                    {"role": "user", "content": format!("site: {site}\nregion: {region}")},
                ],
            }),
        ) {
            Ok(payload) => payload,
            Err(detail) => {
                self.record_error(detail);
                return None;
            }
        };
        let content: Option<String> = (|| {
            payload
                .get("choices")?
                .as_array()?
                .first()?
                .get("message")?
                .get("content")?
                .as_str()
                .map(str::to_owned)
        })();
        if let Some(content) = content {
            Some(content)
        } else {
            self.record_error("groq malformed response (no choices/message/content)".to_owned());
            None
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
/// ```json fences (models wrap fenced output even when told not to); extra
/// fields are ignored. Anything else — prose, a bare domain, a missing or
/// non-string `domain` — is `None`, and the ladder degrades to its next
/// rung. This parses only; [`crate::route_proposer::validate_grounded_domain`]
/// decides whether the domain may be navigated.
fn extract_domain(content: &str) -> Option<String> {
    let trimmed = content.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    let value: serde_json::Value = serde_json::from_str(unfenced).ok()?;
    value.get("domain")?.as_str().map(str::to_owned)
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
        let content = match self.provider {
            GrounderProvider::Groq => self.groq_completion(site, region_hint.trim()),
            GrounderProvider::Ollama => self.ollama_generate(site, region_hint.trim()),
        }?;
        extract_domain(&content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Loopback mock that answers one HTTP request with `response_body` as
    /// `application/json`, then exits. Returns the base URL to point the
    /// adapter at — hermetic: no internet, no fixed port. `None` when the
    /// loopback bind itself fails (the test then fails closed with a
    /// clear panic instead of an `expect`).
    fn mock_server(status: u16, response_body: String) -> Option<String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let addr = listener.local_addr().ok()?;
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Read the full request (headers + Content-Length body) so the
            // client never sees a reset before the response is written.
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let Ok(n) = stream.read(&mut chunk) else {
                    return;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let header_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map_or(buf.len(), |pos| pos + 4);
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let content_length: usize = headers
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|line| line.split(':').nth(1))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            let mut body_read = buf.len().saturating_sub(header_end);
            while body_read < content_length {
                let Ok(n) = stream.read(&mut chunk) else {
                    return;
                };
                if n == 0 {
                    break;
                }
                body_read += n;
            }
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        Some(format!("http://{addr}"))
    }

    /// Bind a mock server or fail the test with a clear message. Keeps the
    /// happy-path tests readable without `expect`.
    fn mock_base(response_body: String) -> String {
        match mock_server(200, response_body) {
            Some(base) => base,
            None => panic!("loopback mock failed to bind"),
        }
    }

    /// Bind a mock server answering with an HTTP error status (Groq error
    /// envelope when `response_body` carries one).
    fn mock_status(status: u16, response_body: String) -> String {
        match mock_server(status, response_body) {
            Some(base) => base,
            None => panic!("loopback mock failed to bind"),
        }
    }

    #[test]
    fn extract_domain_accepts_strict_json_shapes() {
        assert_eq!(
            extract_domain(r#"{"domain":"amazon.in"}"#).as_deref(),
            Some("amazon.in")
        );
        assert_eq!(
            extract_domain("  \n{\"domain\": \"amazon.in\"}  ").as_deref(),
            Some("amazon.in")
        );
        assert_eq!(
            extract_domain("```json\n{\"domain\":\"amazon.in\"}\n```").as_deref(),
            Some("amazon.in")
        );
        // Extra fields are tolerated; only `domain` is read.
        assert_eq!(
            extract_domain(r#"{"domain":"amazon.in","confidence":0.9}"#).as_deref(),
            Some("amazon.in")
        );
    }

    #[test]
    fn extract_domain_rejects_everything_else() {
        assert_eq!(extract_domain("amazon.in"), None);
        assert_eq!(extract_domain(""), None);
        assert_eq!(extract_domain("not json at all"), None);
        assert_eq!(extract_domain(r#"{"url":"https://amazon.in"}"#), None);
        assert_eq!(extract_domain(r#"{"domain":42}"#), None);
        assert_eq!(extract_domain(r#"["amazon.in"]"#), None);
    }

    #[test]
    fn groq_grounds_end_to_end_through_loopback() {
        // A canned Groq chat-completions envelope; the adapter must pull the
        // content out and the strict parser must yield the bare domain.
        let envelope = serde_json::json!({
            "choices": [{"message": {"content": "{\"domain\": \"amazon.in\"}"}}],
        })
        .to_string();
        let base = mock_base(envelope);
        let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
        assert_eq!(
            grounder.ground_domain("amazon", "IN").as_deref(),
            Some("amazon.in")
        );
    }

    #[test]
    fn groq_malformed_response_declines() {
        let base = mock_base("this is not json".to_owned());
        let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
        assert_eq!(grounder.ground_domain("amazon", "IN"), None);
    }

    #[test]
    fn groq_wrong_shape_declines() {
        // Valid JSON, but no usable `domain`: still a decline.
        let envelope = serde_json::json!({
            "choices": [{"message": {"content": "{\"url\": \"https://amazon.in\"}"}}],
        })
        .to_string();
        let base = mock_base(envelope);
        let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
        assert_eq!(grounder.ground_domain("amazon", "IN"), None);
        // A clean decline is not a provider failure: nothing to diagnose.
        assert_eq!(grounder.last_error(), None);
    }

    #[test]
    fn groq_malformed_body_records_diagnostic() {
        let base = mock_base("this is not json".to_owned());
        let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
        assert_eq!(grounder.ground_domain("amazon", "IN"), None);
        let Some(err) = grounder.last_error() else {
            panic!("malformed body records a diagnostic");
        };
        assert!(err.contains("malformed"), "unexpected detail: {err}");
        assert!(
            !err.contains("test-key"),
            "detail must never echo credentials"
        );
    }

    #[test]
    fn groq_http_error_records_provider_code() {
        // The exact failure mode of a decommissioned model: HTTP 400 with
        // Groq's OpenAI-compatible error envelope.
        let envelope = serde_json::json!({
            "error": {
                "message": "The model `llama-3.1-8b-instant` has been decommissioned.",
                "type": "invalid_request_error",
                "code": "model_decommissioned",
            },
        })
        .to_string();
        let base = mock_status(400, envelope);
        let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
        assert_eq!(grounder.ground_domain("amazon", "IN"), None);
        assert_eq!(
            grounder.last_error().as_deref(),
            Some("groq http 400 (model_decommissioned)")
        );
    }

    #[test]
    fn groq_http_error_without_envelope_records_excerpt() {
        let base = mock_status(500, "upstream exploded".to_owned());
        let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
        assert_eq!(grounder.ground_domain("amazon", "IN"), None);
        assert_eq!(
            grounder.last_error().as_deref(),
            Some("groq http 500 (upstream exploded)")
        );
    }

    #[test]
    fn groq_transport_failure_records_kind() {
        // Unroutable port: connection refused before any byte is sent.
        let grounder = LlmDomainGrounder::groq("key", "http://127.0.0.1:9", "model");
        assert_eq!(grounder.ground_domain("amazon", "IN"), None);
        assert_eq!(
            grounder.last_error().as_deref(),
            Some("groq request failed (transport error)")
        );
    }

    #[test]
    fn ollama_http_error_records_status() {
        let base = mock_status(500, "model not found".to_owned());
        let grounder = LlmDomainGrounder::ollama(&base, "test-model");
        assert_eq!(grounder.ground_domain("flipkart", "IN"), None);
        assert_eq!(
            grounder.last_error().as_deref(),
            Some("ollama http 500 (model not found)")
        );
    }

    #[test]
    fn successful_call_leaves_no_diagnostic() {
        let envelope = serde_json::json!({
            "choices": [{"message": {"content": "{\"domain\": \"amazon.in\"}"}}],
        })
        .to_string();
        let base = mock_base(envelope);
        let grounder = LlmDomainGrounder::groq("test-key", &base, "test-model");
        assert_eq!(
            grounder.ground_domain("amazon", "IN").as_deref(),
            Some("amazon.in")
        );
        assert_eq!(grounder.last_error(), None);
    }

    #[test]
    fn ollama_grounds_end_to_end_through_loopback() {
        let envelope =
            serde_json::json!({"response": "{\"domain\": \"flipkart.com\"}"}).to_string();
        let base = mock_base(envelope);
        let grounder = LlmDomainGrounder::ollama(&base, "test-model");
        assert_eq!(
            grounder.ground_domain("flipkart", "IN").as_deref(),
            Some("flipkart.com")
        );
    }

    #[test]
    fn empty_site_name_never_touches_the_network() {
        // Point at an unroutable port: an empty slot must decline before
        // any socket is opened.
        let grounder = LlmDomainGrounder::groq("key", "http://127.0.0.1:9", "model");
        assert_eq!(grounder.ground_domain("   ", "IN"), None);
        let grounder = LlmDomainGrounder::ollama("http://127.0.0.1:9", "model");
        assert_eq!(grounder.ground_domain("", ""), None);
    }

    #[test]
    fn provider_value_parses_tolerantly() {
        assert_eq!(
            GrounderProvider::from_env_value("groq"),
            Some(GrounderProvider::Groq)
        );
        assert_eq!(
            GrounderProvider::from_env_value(" Ollama "),
            Some(GrounderProvider::Ollama)
        );
        assert_eq!(GrounderProvider::from_env_value(""), None);
        assert_eq!(GrounderProvider::from_env_value("openai"), None);
    }

    #[test]
    fn from_env_values_is_absent_without_a_provider() {
        assert!(LlmDomainGrounder::from_env_values(&GrounderEnv::default()).is_none());
        assert!(
            LlmDomainGrounder::from_env_values(&GrounderEnv {
                provider: Some("openai".to_owned()),
                ..Default::default()
            })
            .is_none()
        );
    }

    #[test]
    fn from_env_values_groq_requires_a_key() {
        // No key at all.
        assert!(
            LlmDomainGrounder::from_env_values(&GrounderEnv {
                provider: Some("groq".to_owned()),
                ..Default::default()
            })
            .is_none()
        );
        // A whitespace-only key is the same as a missing one.
        assert!(
            LlmDomainGrounder::from_env_values(&GrounderEnv {
                provider: Some("groq".to_owned()),
                groq_api_key: Some("   ".to_owned()),
                ..Default::default()
            })
            .is_none()
        );
        // A real key builds the adapter against the default base URL.
        let Some(grounder) = LlmDomainGrounder::from_env_values(&GrounderEnv {
            provider: Some("groq".to_owned()),
            groq_api_key: Some("test-key".to_owned()),
            ..Default::default()
        }) else {
            panic!("groq builds with a key");
        };
        assert_eq!(grounder.provider, GrounderProvider::Groq);
        assert_eq!(grounder.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(grounder.model, "openai/gpt-oss-20b");
    }

    #[test]
    fn from_env_values_ollama_needs_no_key_and_applies_defaults() {
        let Some(grounder) = LlmDomainGrounder::from_env_values(&GrounderEnv {
            provider: Some("ollama".to_owned()),
            ..Default::default()
        }) else {
            panic!("ollama builds keyless");
        };
        assert_eq!(grounder.provider, GrounderProvider::Ollama);
        assert_eq!(grounder.base_url, "http://localhost:11434");
        assert_eq!(grounder.model, "qwen2.5:1.5b");
    }
}

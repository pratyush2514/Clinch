#![deny(unsafe_code)]
//! Localhost WebSocket bridge for the Clinch Companion (MV3) extension.
//!
//! The extension reads the live browser's cookies (which Clinch cannot unwrap
//! itself when the profile uses App-Bound encryption) and pushes them over
//! loopback on explicit request. Security model, enforced below:
//! - Loopback-only bind (`127.0.0.1`): never a LAN socket.
//! - Server-initiated requests only: the extension cannot push an unsolicited
//!   session. Payloads are accepted solely for pending, unexpired, single-use
//!   request IDs minted by a Clinch UI action; anything else is ignored.
//! - Authoritative server-side domain filter: cookies outside the portal's
//!   scope are dropped even if a compromised client sends them.
//! - No secret persistence: accepted cookies flow straight into CDP injection
//!   (in-memory). They are never written to `SQLite`, logs, or IPC responses.
//! - Bounded everything: connection cap with oldest-eviction, per-message and
//!   per-field size caps, response timeout.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// Loopback port the companion extension dials. Distinct from Chrome's 9222
/// remote-debugging default; the managed browser uses `--remote-debugging-port=0`.
pub const BRIDGE_PORT: u16 = 9223;
/// How long the desktop waits for the extension to answer one request.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
/// Pending requests expire even if the extension never answers.
const REQUEST_TTL: Duration = Duration::from_mins(1);
/// Concurrent extension sockets; beyond this the oldest is evicted.
const MAX_CONNECTIONS: usize = 4;
/// Oversized frames are dropped unread (largest legitimate payload is a few KB).
const MAX_FRAME_BYTES: usize = 256 * 1024;
const MAX_COOKIES: usize = 500;
const MAX_VALUE_LEN: usize = 16 * 1024;
const MAX_NAME_LEN: usize = 256;
const MAX_UA_LEN: usize = 512;

/// Ancestor suffixes plus curated cross-root secondaries for `host`.
/// Scope rules live in `session-sync` (shared with cookie extraction);
/// the only addition here is subdomains of the exact portal host. Must still
/// mirror `KNOWN_SSO_SECONDARIES` in `packages/extension-bridge/background.js`;
/// the desktop filter below is authoritative, the extension's only shrinks
/// the payload. Extend the shared table when adding providers.
fn scope_roots(host: &str) -> Vec<String> {
    let mut roots = session_sync::ancestor_roots(host);
    roots.extend(session_sync::sso_secondaries(host));
    roots
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("The Clinch Companion extension is not connected")]
    NoExtension,
    #[error("The companion extension did not answer in time")]
    Timeout,
    #[error("The bridge request expired before the extension answered")]
    Stale,
    #[error("The companion answered for the wrong domain")]
    DomainMismatch,
    #[error("The companion found no usable cookies for this portal")]
    NoCookies,
    #[error("The companion payload was invalid")]
    Invalid,
    #[error("The companion bridge is unavailable")]
    Unavailable,
}

/// Whether a cookie domain may ride along with a `host` sync: the host
/// itself, its subdomains, or a scoped SSO root. Lookalikes
/// (`evil-example.com`) never match the dot boundary.
fn in_scope(cookie_domain: &str, host: &str) -> bool {
    let domain = cookie_domain.trim_start_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || host.is_empty() {
        return false;
    }
    if domain == host || domain.ends_with(&format!(".{host}")) {
        return true;
    }
    scope_roots(&host)
        .iter()
        .any(|root| domain == *root || domain.ends_with(&format!(".{root}")))
}

/// Cookie wire shape emitted by `background.js`. Lenient on transport
/// metadata (`path`, flags) — strict validation happens in
/// [`validate_response`], never here.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireCookie {
    name: String,
    value: String,
    domain: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    secure: bool,
    #[serde(default)]
    http_only: bool,
    #[serde(default = "default_same_site")]
    same_site: String,
    #[serde(default)]
    expiration_date: Option<f64>,
}

fn default_path() -> String {
    "/".to_owned()
}

fn default_same_site() -> String {
    "unspecified".to_owned()
}

#[derive(Debug, serde::Deserialize)]
// No `deny_unknown_fields`: the envelope carries routing metadata (`type`)
// that is dispatched on before this shape is parsed.
#[serde(rename_all = "camelCase")]
struct SyncResponse {
    request_id: String,
    domain: String,
    cookies: Vec<WireCookie>,
    user_agent: String,
}

#[derive(Debug, serde::Deserialize)]
struct SyncErrorReport {
    #[serde(default)]
    request_id: String,
    #[serde(default)]
    reason: String,
}

/// Validated bridge session: in-scope cookies plus the source UA, ready for
/// CDP injection. Contains live secrets — never logged or serialized.
pub struct BridgeSession {
    pub cookies: Vec<session_sync::Cookie>,
    pub user_agent: String,
}

fn validate_response(
    portal_host: &str,
    response: SyncResponse,
) -> Result<BridgeSession, BridgeError> {
    if response.request_id.is_empty() || response.request_id.len() > 128 {
        return Err(BridgeError::Invalid);
    }
    if !response.domain.eq_ignore_ascii_case(portal_host) {
        return Err(BridgeError::DomainMismatch);
    }
    if response.user_agent.is_empty()
        || response.user_agent.len() > MAX_UA_LEN
        || !response
            .user_agent
            .bytes()
            .all(|byte| (0x20..=0x7E).contains(&byte))
    {
        return Err(BridgeError::Invalid);
    }
    let mut cookies = Vec::new();
    for wire in response.cookies.into_iter().take(MAX_COOKIES) {
        // Empty values are tombstones, not session material.
        if wire.name.is_empty()
            || wire.name.len() > MAX_NAME_LEN
            || wire.value.is_empty()
            || wire.value.len() > MAX_VALUE_LEN
        {
            continue;
        }
        if !in_scope(&wire.domain, portal_host) {
            continue;
        }
        let same_site = match wire.same_site.to_ascii_lowercase().as_str() {
            "no_restriction" => session_sync::CookieSameSite::None,
            "lax" => session_sync::CookieSameSite::Lax,
            "strict" => session_sync::CookieSameSite::Strict,
            _ => session_sync::CookieSameSite::Unspecified,
        };
        // Mirrors the `browser-driver` expiry bound (year 9999, f64-safe).
        let expires = wire.expiration_date.and_then(|epoch| {
            if epoch.is_finite() && (0.0..=253_402_300_799.0).contains(&epoch) {
                // The range check above keeps the value below 2^53, exactly
                // representable as both `f64` and `i64`.
                #[allow(clippy::cast_possible_truncation)]
                Some(epoch as i64)
            } else {
                None
            }
        });
        cookies.push(session_sync::Cookie {
            name: wire.name,
            value: zeroize::Zeroizing::new(wire.value),
            domain: wire.domain,
            path: if wire.path.is_empty() {
                default_path()
            } else {
                wire.path
            },
            secure: wire.secure,
            http_only: wire.http_only,
            same_site,
            expires,
        });
    }
    if cookies.is_empty() {
        return Err(BridgeError::NoCookies);
    }
    Ok(BridgeSession {
        cookies,
        user_agent: response.user_agent,
    })
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "SYNC_SESSION")]
    SyncSession {
        #[serde(rename = "requestId")]
        request_id: String,
        domain: String,
    },
}

struct Pending {
    portal_host: String,
    created: Instant,
    reply: Option<oneshot::Sender<Result<BridgeSession, BridgeError>>>,
}

struct Connection {
    created: Instant,
    outbox: mpsc::Sender<String>,
}

pub struct BridgeServer {
    pending: Mutex<HashMap<String, Pending>>,
    connections: Mutex<HashMap<u64, Connection>>,
    next_connection: AtomicU64,
    next_request: AtomicU64,
    alive: AtomicBool,
    local_addr: Mutex<Option<SocketAddr>>,
}

impl BridgeServer {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
            next_request: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            local_addr: Mutex::new(None),
        }
    }

    /// Bind loopback and spawn the accept loop. The returned handle must be
    /// kept alive by the owner (`AppService`); dropping it aborts the
    /// listener. `port` is a parameter (production passes [`BRIDGE_PORT`))
    /// so tests can bind ephemeral ports.
    ///
    /// # Errors
    /// Returns the OS bind error when the port is unavailable.
    pub async fn start(port: u16) -> std::io::Result<(Arc<Self>, JoinHandle<()>)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let server = Arc::new(Self::new());
        *server
            .local_addr
            .lock()
            .map_err(|_| std::io::Error::other("bridge state lock failed"))? =
            Some(listener.local_addr()?);
        let task = tokio::spawn(accept_loop(server.clone(), listener));
        Ok((server, task))
    }

    /// Bound socket address, if the listener started.
    #[must_use]
    pub fn local_port(&self) -> Option<u16> {
        self.local_addr
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(SocketAddr::port))
    }

    /// Whether the accept loop is still running.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Currently connected companion sockets.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.lock().map_or(0, |guard| guard.len())
    }

    /// Ask the connected extension for `portal`'s session. Fails fast with
    /// [`BridgeError::NoExtension`] when no companion is connected.
    ///
    /// # Errors
    /// Returns `NoExtension`, `Timeout`, or the validated extension-side
    /// failure (`DomainMismatch`, `NoCookies`, `Invalid`, `Stale`).
    pub async fn request_sync(
        &self,
        portal: &url::Url,
        timeout: Duration,
    ) -> Result<BridgeSession, BridgeError> {
        let host = portal.host_str().ok_or(BridgeError::Invalid)?.to_owned();
        let id = format!(
            "{:x}{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
            self.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let (reply, receive) = oneshot::channel();
        {
            let mut pending = self.pending.lock().map_err(|_| BridgeError::Unavailable)?;
            pending.retain(|_, request| request.created.elapsed() < REQUEST_TTL);
            pending.insert(
                id.clone(),
                Pending {
                    portal_host: host.clone(),
                    created: Instant::now(),
                    reply: Some(reply),
                },
            );
        }
        // Grace window for a companion mid-reconnect (TCP + WS handshake +
        // registration take a few hundred ms); a genuinely absent extension
        // still fails fast right after it.
        let mut connected = false;
        for _ in 0..20 {
            connected = self.connections.lock().is_ok_and(|guard| !guard.is_empty());
            if connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !connected {
            self.pending.lock().map(|mut guard| guard.remove(&id)).ok();
            return Err(BridgeError::NoExtension);
        }
        let message = serde_json::to_string(&ServerMessage::SyncSession {
            request_id: id.clone(),
            domain: host,
        })
        .map_err(|_| BridgeError::Unavailable)?;
        self.broadcast(&message);
        match tokio::time::timeout(timeout, receive).await {
            Err(_) => {
                self.pending.lock().map(|mut guard| guard.remove(&id)).ok();
                Err(BridgeError::Timeout)
            }
            Ok(Err(_)) => Err(BridgeError::Stale),
            Ok(Ok(result)) => result,
        }
    }

    fn broadcast(&self, message: &str) {
        let targets: Vec<mpsc::Sender<String>> = self
            .connections
            .lock()
            .map(|guard| {
                guard
                    .values()
                    .map(|connection| connection.outbox.clone())
                    .collect()
            })
            .unwrap_or_default();
        for outbox in targets {
            // Bounded outbox: a wedged companion is skipped, never blocks sync.
            let _ = outbox.try_send(message.to_owned());
        }
    }

    fn handle_text(self: &Arc<Self>, text: &str) {
        if text.len() > MAX_FRAME_BYTES {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("SYNC_SESSION_RESPONSE") => {
                let Ok(response) = serde_json::from_value::<SyncResponse>(value) else {
                    return;
                };
                let request_id = response.request_id.clone();
                self.complete_response(&request_id, move |host| validate_response(host, response));
            }
            Some("SYNC_SESSION_ERROR") => {
                let report: SyncErrorReport =
                    serde_json::from_value(value).unwrap_or(SyncErrorReport {
                        request_id: String::new(),
                        reason: String::new(),
                    });
                let error = match report.reason.as_str() {
                    "no_cookies" => BridgeError::NoCookies,
                    _ => BridgeError::Invalid,
                };
                self.complete_request(&report.request_id, Err(error));
            }
            _ => {}
        }
    }

    fn complete_response(
        self: &Arc<Self>,
        request_id: &str,
        validate: impl FnOnce(&str) -> Result<BridgeSession, BridgeError>,
    ) {
        let pending = self
            .pending
            .lock()
            .map(|mut guard| guard.remove(request_id))
            .ok()
            .flatten();
        // Unknown or already-consumed IDs are ignored: responses are single-use.
        let Some(request) = pending else { return };
        if request.created.elapsed() > REQUEST_TTL {
            if let Some(reply) = request.reply {
                let _ = reply.send(Err(BridgeError::Stale));
            }
            return;
        }
        let result = validate(&request.portal_host);
        if let Some(reply) = request.reply {
            let _ = reply.send(result);
        }
    }

    fn complete_request(&self, request_id: &str, result: Result<BridgeSession, BridgeError>) {
        let pending = self
            .pending
            .lock()
            .map(|mut guard| guard.remove(request_id))
            .ok()
            .flatten();
        if let Some(request) = pending
            && let Some(reply) = request.reply
        {
            let _ = reply.send(result);
        }
    }
}

async fn accept_loop(server: Arc<BridgeServer>, listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            server.alive.store(false, Ordering::Relaxed);
            eprintln!("[clinch] companion bridge accept loop ended");
            return;
        };
        let Ok(ws) = async_tungstenite::tokio::accept_async(stream).await else {
            continue;
        };
        serve_connection(server.clone(), ws).await;
    }
}

async fn serve_connection<S>(server: Arc<BridgeServer>, ws: async_tungstenite::WebSocketStream<S>)
where
    // `accept_async` adapts the tokio socket into futures-io; the generic
    // stream therefore bounds on futures traits, not tokio's.
    S: futures::AsyncRead + futures::AsyncWrite + Unpin + Send + 'static,
{
    use async_tungstenite::tungstenite::Message;
    use futures::StreamExt;

    let (mut sink, mut stream) = ws.split();
    let (outbox, mut inbox) = mpsc::channel::<String>(8);
    let id = server.next_connection.fetch_add(1, Ordering::Relaxed);
    {
        let Ok(mut connections) = server.connections.lock() else {
            return;
        };
        if connections.len() >= MAX_CONNECTIONS {
            // Evict the oldest socket; a fresh companion always wins.
            if let Some(oldest) = connections
                .iter()
                .min_by_key(|(_, connection)| connection.created)
                .map(|(id, _)| *id)
            {
                connections.remove(&oldest);
            }
        }
        connections.insert(
            id,
            Connection {
                created: Instant::now(),
                outbox,
            },
        );
    }
    loop {
        tokio::select! {
            incoming = stream.next() => {
                let Some(message) = incoming else { break };
                let Ok(message) = message else { break };
                match message {
                    Message::Text(text) => server.handle_text(&text),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            outgoing = inbox.recv() => {
                let Some(text) = outgoing else { break };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        }
    }
    server
        .connections
        .lock()
        .map(|mut guard| {
            guard.remove(&id);
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_fixture() -> serde_json::Value {
        serde_json::json!({
            "requestId": "req-1",
            "domain": "chatgpt.com",
            "userAgent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/131.0.0.0",
            "cookies": [
                {"name": "session", "value": "abc", "domain": "chatgpt.com", "path": "/",
                 "secure": true, "httpOnly": true, "sameSite": "lax", "expirationDate": 4_102_444_800.0},
                {"name": "sso", "value": "def", "domain": ".openai.com", "path": "/",
                 "secure": true, "httpOnly": true, "sameSite": "strict"},
                {"name": "evil", "value": "x", "domain": "chatgpt.com.evil.com", "path": "/",
                 "secure": false, "httpOnly": false, "sameSite": "unspecified"},
                {"name": "huge", "value": "y".repeat(MAX_VALUE_LEN + 1), "domain": "chatgpt.com",
                 "path": "/", "secure": true, "httpOnly": true, "sameSite": "lax"},
            ],
        })
    }

    #[test]
    fn scope_covers_portal_subdomains_and_curated_sso() {
        // The worked chatgpt.com example from the bridge contract.
        for domain in [
            "chatgpt.com",
            ".chatgpt.com",
            "auth.openai.com",
            ".openai.com",
        ] {
            assert!(in_scope(domain, "chatgpt.com"), "{domain}");
        }
        for domain in [
            "evil.com",
            "openai.com.evil.com",
            "chatgpt.com.evil.com",
            "notchatgpt.com",
            "",
        ] {
            assert!(!in_scope(domain, "chatgpt.com"), "{domain}");
        }
        // Generic suffix rule without any curated entry.
        assert!(in_scope("auth.example.com", "portal.example.com"));
        assert!(in_scope(".sso.example.com", "portal.example.com"));
        assert!(!in_scope("evil-example.com", "portal.example.com"));
    }

    #[test]
    fn validation_filters_and_rejects_by_rule() -> Result<(), Box<dyn std::error::Error>> {
        let response: SyncResponse = serde_json::from_value(response_fixture())?;
        let session = validate_response("chatgpt.com", response)?;
        // Evil-domain and oversized cookies are dropped; two survive.
        assert_eq!(session.cookies.len(), 2);
        assert_eq!(session.cookies[0].domain, "chatgpt.com");
        assert!(matches!(
            session.cookies[0].same_site,
            session_sync::CookieSameSite::Lax
        ));
        assert_eq!(session.cookies[0].expires, Some(4_102_444_800));
        assert_eq!(session.cookies[1].domain, ".openai.com");
        assert!(session.user_agent.contains("Windows NT"));

        let mismatch: SyncResponse = serde_json::from_value(serde_json::json!({
            "requestId": "req-1", "domain": "evil.com", "userAgent": "Mozilla/5.0",
            "cookies": [{"name": "a", "value": "b", "domain": "evil.com"}],
        }))?;
        assert!(matches!(
            validate_response("chatgpt.com", mismatch),
            Err(BridgeError::DomainMismatch)
        ));

        let empty: SyncResponse = serde_json::from_value(serde_json::json!({
            "requestId": "req-1", "domain": "chatgpt.com", "userAgent": "Mozilla/5.0",
            "cookies": [{"name": "a", "value": "b", "domain": "evil.com"}],
        }))?;
        assert!(matches!(
            validate_response("chatgpt.com", empty),
            Err(BridgeError::NoCookies)
        ));

        let bad_ua: SyncResponse = serde_json::from_value(serde_json::json!({
            "requestId": "req-1", "domain": "chatgpt.com", "userAgent": "",
            "cookies": [{"name": "a", "value": "b", "domain": "chatgpt.com"}],
        }))?;
        assert!(matches!(
            validate_response("chatgpt.com", bad_ua),
            Err(BridgeError::Invalid)
        ));
        Ok(())
    }

    #[test]
    fn unknown_request_ids_are_ignored() {
        let server = BridgeServer::new();
        server.complete_request("no-such-request", Err(BridgeError::Invalid));
        assert!(server.pending.lock().is_ok_and(|guard| guard.is_empty()));
    }

    #[tokio::test]
    async fn bridge_round_trip_over_loopback() -> Result<(), Box<dyn std::error::Error>> {
        use async_tungstenite::tungstenite::Message;
        use futures::StreamExt;

        let (server, _task) = BridgeServer::start(0).await?;
        let port = server.local_port().ok_or("listener has no port")?;
        // Regression guard for `ERR_CONNECTION_REFUSED`: the socket must be
        // an explicit IPv4 loopback bind — never the `localhost` hostname
        // (which may resolve to `::1` while the extension dials `127.0.0.1`).
        let bound = server
            .local_addr
            .lock()
            .map_err(|_| "state lock failed")?
            .ok_or("listener has no address")?;
        assert!(bound.ip().is_loopback());
        assert!(bound.ip().is_ipv4());
        let portal = url::Url::parse("https://chatgpt.com/")?;

        // Fake companion: answer the first SYNC_SESSION with the fixture.
        let responder = tokio::spawn(async move {
            let (mut ws, _) =
                async_tungstenite::tokio::connect_async(format!("ws://127.0.0.1:{port}"))
                    .await
                    .map_err(|_| "connect failed")?;
            while let Some(message) = ws.next().await {
                let Message::Text(text) = message.map_err(|_| "read failed")? else {
                    continue;
                };
                if !text.contains("SYNC_SESSION") || text.contains("RESPONSE") {
                    continue;
                }
                let request: serde_json::Value =
                    serde_json::from_str(&text).map_err(|_| "bad request")?;
                let id = request
                    .get("requestId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing requestId")?;
                let mut payload = response_fixture();
                // The unit-test fixture omits the envelope tag that the real
                // extension always sends; the server routes on it.
                payload["type"] = serde_json::Value::String("SYNC_SESSION_RESPONSE".to_owned());
                payload["requestId"] = serde_json::Value::String(id.to_owned());
                ws.send(Message::Text(
                    serde_json::to_string(&payload)
                        .map_err(|_| "encode")?
                        .into(),
                ))
                .await
                .map_err(|_| "send failed")?;
                break;
            }
            ws.close(None).await.map_err(|_| "close failed")?;
            Ok::<_, &str>(())
        });

        let session = server
            .request_sync(&portal, Duration::from_secs(10))
            .await
            .map_err(|error| format!("request failed: {error}"))?;
        assert_eq!(session.cookies.len(), 2);
        assert_eq!(session.cookies[0].domain, "chatgpt.com");
        assert!(server.is_alive());
        responder
            .await
            .map_err(|_| "responder panicked")?
            .map_err(|reason| format!("responder: {reason}"))?;
        Ok(())
    }
}

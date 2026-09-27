//! Scriptable fake-CDP server plus a lockstep JSON-RPC client, both speaking
//! the `DevTools` wire format over a loopback WebSocket.
//!
//! The server answers each incoming `{"id","method","params"}` frame from a
//! script queue and records the received method order, so tests pin the
//! exact CDP conversation production code performs. Anything the script
//! does not expect gets a JSON-RPC error reply plus a recorded violation,
//! which fails the test instead of hanging it.
//!
//! Deliberate scope: this emulates message framing and scripted payloads,
//! not a browser. Target init chains, session routing, and page lifecycle
//! stay behind the `CLINCH_CHROMIUM_PATH`-gated fixtures; the resync
//! conversation order and target-selection policy — the logic this
//! workspace owns — are what the tests below prove.
//!
//! Test-only: exercised solely from `tests/`.

use async_tungstenite::tungstenite::Message;
use futures::StreamExt;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

/// Response budget for one fake roundtrip: loopback only, so anything
/// slower indicates a stuck peer rather than a slow network.
const ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(5);
/// Loopback interface for the fake endpoint: never a LAN socket, mirroring
/// the companion bridge's bind contract.
const LOOPBACK_HOST: &str = "127.0.0.1";

/// One scripted exchange: when the client sends `method`, reply `result`.
#[derive(Clone, Debug)]
pub struct ScriptStep {
    /// Exact CDP method name, e.g. `"Accessibility.getFullAXTree"`.
    pub method: &'static str,
    /// JSON value placed under the reply's `result` key.
    pub result: serde_json::Value,
}

impl ScriptStep {
    /// Shorthand for one scripted method reply.
    #[must_use]
    pub fn reply(method: &'static str, result: serde_json::Value) -> Self {
        Self { method, result }
    }
}

/// Scripted CDP endpoint bound to `127.0.0.1:0`. Serves connections
/// sequentially from the script queue; every reply — expected or error —
/// is immediate, so tests never wait on timeouts.
pub struct FakeCdpServer {
    url: String,
    received: Arc<Mutex<Vec<String>>>,
    violations: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl FakeCdpServer {
    /// Bind loopback and start serving `script` in order.
    ///
    /// # Errors
    /// Returns the OS bind error when loopback is unavailable.
    pub async fn start(script: Vec<ScriptStep>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(format!("{LOOPBACK_HOST}:0")).await?;
        let url = format!("ws://{LOOPBACK_HOST}:{}/", listener.local_addr()?.port());
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let received = Arc::new(Mutex::new(Vec::new()));
        let violations = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(accept_loop(
            listener,
            script,
            Arc::clone(&received),
            Arc::clone(&violations),
        ));
        Ok(Self {
            url,
            received,
            violations,
            task,
        })
    }

    /// WebSocket URL for [`FakeCdpClient::connect`].
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Methods received so far, in arrival order.
    #[must_use]
    pub fn received_methods(&self) -> Vec<String> {
        self.received
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Order violations recorded so far (`expected X, got Y`).
    #[must_use]
    pub fn violations(&self) -> Vec<String> {
        self.violations
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Stop serving. The loopback listener dies with the task.
    pub fn shutdown(self) {
        self.task.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    script: Arc<Mutex<VecDeque<ScriptStep>>>,
    received: Arc<Mutex<Vec<String>>>,
    violations: Arc<Mutex<Vec<String>>>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let Ok(ws) = async_tungstenite::tokio::accept_async(stream).await else {
            continue;
        };
        serve_connection(
            ws,
            Arc::clone(&script),
            Arc::clone(&received),
            Arc::clone(&violations),
        )
        .await;
    }
}

async fn serve_connection(
    mut ws: async_tungstenite::WebSocketStream<async_tungstenite::tokio::ConnectStream>,
    script: Arc<Mutex<VecDeque<ScriptStep>>>,
    received: Arc<Mutex<Vec<String>>>,
    violations: Arc<Mutex<Vec<String>>>,
) {
    use async_tungstenite::tokio::ConnectStream;
    async fn serve(
        ws: &mut async_tungstenite::WebSocketStream<ConnectStream>,
        script: &Arc<Mutex<VecDeque<ScriptStep>>>,
        received: &Arc<Mutex<Vec<String>>>,
        violations: &Arc<Mutex<Vec<String>>>,
    ) -> bool {
        let text = match ws.next().await {
            Some(Ok(Message::Text(text))) => text.to_string(),
            _ => return false,
        };
        let request: serde_json::Value = match serde_json::from_str(&text) {
            Ok(request) => request,
            Err(_) => return true,
        };
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        if let Ok(mut guard) = received.lock() {
            guard.push(method.clone());
        }
        let reply = match script.lock().ok().and_then(|mut guard| guard.pop_front()) {
            Some(step) if step.method == method => {
                serde_json::json!({"id": id, "result": step.result})
            }
            Some(step) => {
                if let Ok(mut guard) = violations.lock() {
                    guard.push(format!("expected {}, got {method}", step.method));
                }
                serde_json::json!({"id": id, "error": {"code": -32601, "message": "script exhausted or out of order"}})
            }
            None => {
                if let Ok(mut guard) = violations.lock() {
                    guard.push(format!("unexpected call {method} past end of script"));
                }
                serde_json::json!({"id": id, "error": {"code": -32601, "message": "script exhausted"}})
            }
        };
        ws.send(Message::Text(reply.to_string().into()))
            .await
            .is_ok()
    }
    while serve(&mut ws, &script, &received, &violations).await {}
}

/// Lockstep JSON-RPC client for [`FakeCdpServer`]: one call sends exactly
/// one command frame and awaits the matching response id.
pub struct FakeCdpClient {
    ws: async_tungstenite::WebSocketStream<async_tungstenite::tokio::ConnectStream>,
    next_id: u64,
}

impl FakeCdpClient {
    /// Connect to [`FakeCdpServer::url`].
    ///
    /// # Errors
    /// Returns a message when the handshake fails.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let (ws, _) = async_tungstenite::tokio::connect_async(url)
            .await
            .map_err(|error| format!("connect failed: {error}"))?;
        Ok(Self { ws, next_id: 1 })
    }

    /// Send `method` and return its `result` payload, or the server's error
    /// text. Guards the roundtrip so a wedged peer fails the test instead
    /// of hanging it.
    ///
    /// # Errors
    /// Returns the server error text, a protocol message, or a timeout.
    pub async fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = serde_json::json!({"id": id, "method": method, "params": params});
        self.ws
            .send(Message::Text(frame.to_string().into()))
            .await
            .map_err(|error| format!("send failed: {error}"))?;
        let reply = tokio::time::timeout(ROUNDTRIP_TIMEOUT, async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Text(text))) => {
                        let reply: serde_json::Value = serde_json::from_str(&text)
                            .map_err(|error| format!("bad frame: {error}"))?;
                        if reply.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                            continue;
                        }
                        if let Some(error) = reply.get("error") {
                            return Err(format!("fake CDP error: {error}"));
                        }
                        return reply
                            .get("result")
                            .cloned()
                            .ok_or_else(|| "fake CDP reply without result".to_owned());
                    }
                    _ => return Err("connection closed".to_owned()),
                }
            }
        })
        .await
        .map_err(|_| "fake CDP roundtrip timed out".to_owned())??;
        Ok(reply)
    }

    /// Close the connection.
    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// Canonical billing-history tree shared by hermetic tests: one static
/// header plus three download links, each nested in a row cell carrying
/// invoice evidence — mirroring the payment-history rows the settle loop
/// waits past the header for, and the surroundings the noun gate matches.
#[must_use]
pub fn billing_history_tree() -> serde_json::Value {
    fn text(value: &str) -> serde_json::Value {
        serde_json::json!({"type": "string", "value": value})
    }
    fn node(
        id: &str,
        role: Option<&str>,
        name: Option<&str>,
        backend: Option<i64>,
        parent: Option<&str>,
        children: &[&str],
    ) -> serde_json::Value {
        let mut node = serde_json::json!({
            "nodeId": id,
            "ignored": false,
        });
        if let Some(role) = role {
            node["role"] = text(role);
        }
        if let Some(name) = name {
            node["name"] = text(name);
        }
        if let Some(backend) = backend {
            node["backendDOMNodeId"] = serde_json::json!(backend);
        }
        if let Some(parent) = parent {
            node["parentId"] = serde_json::json!(parent);
        }
        if !children.is_empty() {
            node["childIds"] = serde_json::json!(children);
        }
        node
    }
    fn row(index: i64, invoice: &str) -> Vec<serde_json::Value> {
        let row = format!("row{index}");
        let cell = format!("c{index}");
        let label = format!("t{index}");
        let link = format!("r{index}");
        vec![
            node(&row, Some("row"), None, None, None, &[&cell]),
            node(&cell, None, None, None, Some(&row), &[&label, &link]),
            node(
                &label,
                Some("StaticText"),
                Some(&format!("Invoices {invoice}")),
                None,
                Some(&cell),
                &[],
            ),
            node(
                &link,
                Some("link"),
                Some("Download"),
                Some(index),
                Some(&cell),
                &[],
            ),
        ]
    }
    let mut nodes = vec![node(
        "h",
        Some("StaticText"),
        Some("Invoice"),
        None,
        None,
        &[],
    )];
    for (index, invoice) in ["INV-001", "INV-002", "INV-003"].iter().enumerate() {
        nodes.extend(row(i64::try_from(index).unwrap_or(0) + 1, invoice));
    }
    serde_json::json!({ "nodes": nodes })
}

/// Canonical search-results tree shared by hermetic Stage-2 tests.
///
/// Shaped like a real results page and deliberately hostile to naive
/// selection: the engine's own chrome comes **first** in document order and
/// its links mention the target noun too, so a "click the first link that
/// matches" implementation picks chrome and fails the test. Only the
/// landmark check separates them. The organic results then interleave a
/// non-matching competitor between two matching entries, so "first organic
/// link" is also insufficient — the noun gate has to hold.
#[must_use]
pub fn search_results_tree() -> serde_json::Value {
    fn text(value: &str) -> serde_json::Value {
        serde_json::json!({"type": "string", "value": value})
    }
    fn node(
        id: &str,
        role: Option<&str>,
        name: Option<&str>,
        backend: Option<i64>,
        parent: Option<&str>,
        children: &[&str],
    ) -> serde_json::Value {
        let mut node = serde_json::json!({ "nodeId": id, "ignored": false });
        if let Some(role) = role {
            node["role"] = text(role);
        }
        if let Some(name) = name {
            node["name"] = text(name);
        }
        if let Some(backend) = backend {
            node["backendDOMNodeId"] = serde_json::json!(backend);
        }
        if let Some(parent) = parent {
            node["parentId"] = serde_json::json!(parent);
        }
        if !children.is_empty() {
            node["childIds"] = serde_json::json!(children);
        }
        node
    }
    let mut nodes = vec![
        // Engine chrome: a `navigation` landmark whose links also say
        // "Amazon". Document order puts it ahead of every organic result.
        node(
            "nav",
            Some("navigation"),
            Some("Search modes"),
            None,
            None,
            &["nav1", "nav2"],
        ),
        node(
            "nav1",
            Some("link"),
            Some("Images"),
            Some(101),
            Some("nav"),
            &[],
        ),
        node(
            "nav2",
            Some("link"),
            Some("Shopping results for Amazon"),
            Some(102),
            Some("nav"),
            &[],
        ),
    ];
    // Organic results, in document order. Only 2 and 4 mention the noun.
    let organic = [
        ("r1", 1_i64, "Flipkart Online Shopping", "Compare prices"),
        (
            "r2",
            2,
            "Amazon.in - Online Shopping",
            "Low prices across India",
        ),
        ("r3", 3, "Best shopping sites 2026", "A roundup"),
        ("r4", 4, "Amazon.com official site", "Shop now"),
    ];
    for (id, backend, label, blurb) in organic {
        let group = format!("{id}g");
        let caption = format!("{id}t");
        nodes.push(node(
            &group,
            Some("group"),
            None,
            None,
            None,
            &[id, &caption],
        ));
        nodes.push(node(
            id,
            Some("link"),
            Some(label),
            Some(backend),
            Some(&group),
            &[],
        ));
        nodes.push(node(
            &caption,
            Some("StaticText"),
            Some(blurb),
            None,
            Some(&group),
            &[],
        ));
    }
    serde_json::json!({ "nodes": nodes })
}

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
//! Test-only: reachable solely through `#[cfg(test)]`.

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

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide::cdp::browser_protocol::{
        accessibility::AxNode,
        target::{TargetId, TargetInfo},
    };

    fn target_entry(id: &str, kind: &str, url: &str, attached: bool) -> serde_json::Value {
        serde_json::json!({
            "targetId": id,
            "type": kind,
            "title": "fixture",
            "url": url,
            "attached": attached,
            "canAccessOpener": false,
        })
    }

    #[tokio::test]
    async fn test_fake_cdp_cross_origin_process_swap_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let billing_url = "https://github.com/account/billing/history";
        let fake = FakeCdpServer::start(vec![
            ScriptStep::reply("Accessibility.enable", serde_json::json!({})),
            // Stale session right after the swap: the domain answers, but
            // the tree is empty.
            ScriptStep::reply(
                "Accessibility.getFullAXTree",
                serde_json::json!({"nodes": []}),
            ),
            ScriptStep::reply(
                "Target.getTargets",
                serde_json::json!({"targetInfos": [
                    target_entry("github-target", "page", billing_url, true),
                ]}),
            ),
            ScriptStep::reply("Accessibility.enable", serde_json::json!({})),
            // Re-armed session serves the rendered tree.
            ScriptStep::reply("Accessibility.getFullAXTree", billing_history_tree()),
        ])
        .await
        .map_err(|error| format!("fake server failed to start: {error}"))?;
        let run = async {
            let mut client = FakeCdpClient::connect(fake.url()).await?;
            // The exact resync conversation `ax_snapshot` performs: enable,
            // tree, targets, enable, tree.
            client
                .call("Accessibility.enable", serde_json::json!({}))
                .await?;
            let stale = client
                .call("Accessibility.getFullAXTree", serde_json::json!({}))
                .await?;
            let stale_nodes: Vec<AxNode> =
                serde_json::from_value(stale.get("nodes").cloned().unwrap_or_default())
                    .map_err(|error| format!("bad stale tree: {error}"))?;
            assert!(
                crate::interactive_elements(&stale_nodes).is_empty(),
                "stale session yields zero candidates, forcing the retry"
            );
            let listing = client
                .call("Target.getTargets", serde_json::json!({}))
                .await?;
            let targets: Vec<TargetInfo> =
                serde_json::from_value(listing.get("targetInfos").cloned().unwrap_or_default())
                    .map_err(|error| format!("bad target listing: {error}"))?;
            assert_eq!(targets.len(), 1);
            client
                .call("Accessibility.enable", serde_json::json!({}))
                .await?;
            let rendered = client
                .call("Accessibility.getFullAXTree", serde_json::json!({}))
                .await?;
            let nodes: Vec<AxNode> =
                serde_json::from_value(rendered.get("nodes").cloned().unwrap_or_default())
                    .map_err(|error| format!("bad rendered tree: {error}"))?;
            let elements = crate::interactive_elements(&nodes);
            assert_eq!(
                elements
                    .iter()
                    .map(|element| element.backend_node_id)
                    .collect::<Vec<_>>(),
                vec![1, 2, 3],
                "retry recovers every invoice row, header excluded"
            );
            assert!(
                elements.iter().all(|element| element.name == "Download"),
                "recovered rows carry their visible labels"
            );
            // The conversation ran in production order, with nothing extra
            // and nothing out of sequence.
            assert_eq!(
                fake.received_methods(),
                vec![
                    "Accessibility.enable",
                    "Accessibility.getFullAXTree",
                    "Target.getTargets",
                    "Accessibility.enable",
                    "Accessibility.getFullAXTree",
                ]
            );
            assert!(fake.violations().is_empty());
            client.close().await;
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let outcome = tokio::time::timeout(Duration::from_secs(10), run).await;
        fake.shutdown();
        outcome.map_err(|_| "fake CDP roundtrip timed out")??;
        Ok(())
    }

    #[tokio::test]
    async fn test_fake_cdp_portal_reanchoring_drift_guard() -> Result<(), Box<dyn std::error::Error>>
    {
        let billing_url = "https://github.com/account/billing/history";
        let fake = FakeCdpServer::start(vec![ScriptStep::reply(
            "Target.getTargets",
            serde_json::json!({"targetInfos": [
                target_entry("google-target", "page", "https://google.com/", true),
                target_entry("github-target", "page", billing_url, true),
                target_entry("worker", "service_worker", billing_url, true),
                target_entry("detached", "page", billing_url, false),
            ]}),
        )])
        .await
        .map_err(|error| format!("fake server failed to start: {error}"))?;
        let run = async {
            let mut client = FakeCdpClient::connect(fake.url()).await?;
            let listing = client
                .call("Target.getTargets", serde_json::json!({}))
                .await?;
            // Wire-parsed listing through the real production selector.
            let targets: Vec<TargetInfo> =
                serde_json::from_value(listing.get("targetInfos").cloned().unwrap_or_default())
                    .map_err(|error| format!("bad target listing: {error}"))?;
            let url = |raw: &str| url::Url::parse(raw).map_err(|error| format!("bad url: {error}"));
            // Pre-navigation the google tab is active; post-navigation the
            // selection switches to the github tab — the re-anchor.
            let before = crate::select_active_page_target(&targets, &url("https://google.com/")?);
            let after = crate::select_active_page_target(&targets, &url(billing_url)?);
            assert_eq!(before, Some(TargetId::new("google-target")));
            assert_eq!(after, Some(TargetId::new("github-target")));
            assert_ne!(before, after, "origin transition switches target IDs");
            // Unknown destinations and non-page/detached entries never win:
            // the guard keeps the current handle instead of guessing.
            assert_eq!(
                crate::select_active_page_target(&targets, &url("https://other.example/")?),
                None
            );
            assert_eq!(fake.received_methods(), vec!["Target.getTargets"]);
            assert!(fake.violations().is_empty());
            client.close().await;
            Ok::<_, Box<dyn std::error::Error>>(())
        };
        let outcome = tokio::time::timeout(Duration::from_secs(10), run).await;
        fake.shutdown();
        outcome.map_err(|_| "fake CDP roundtrip timed out")??;
        Ok(())
    }
}

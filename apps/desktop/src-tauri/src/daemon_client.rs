#![deny(unsafe_code)]
//! WebSocket client for the clinch-daemon (remote mode).
//!
//! When `CLINCH_DAEMON_URL` is set, every Tauri command proxies to the daemon
//! over this client instead of running the embedded engine. Results and
//! errors travel as opaque [`serde_json::Value`] and are forwarded untouched,
//! so the frontend observes identical payloads in both modes.
//!
//! Privacy: message payloads are never logged — only connection-level events
//! (connect, disconnect) reach stderr.

use async_tungstenite::tungstenite::Message;
use clinch_protocol::{ClientRequest, ServerPush, ServerResponse, WireMessage, event};
use futures::StreamExt;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, oneshot};

/// How long one proxied command may run before the client gives up.
/// Dispatches and playbook runs legitimately take minutes.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound on the TCP + WebSocket handshake; the daemon is loopback-local.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Screencast/cursor fan-out. One subscriber is the norm; a lagging one
/// skips frames (`Lagged`) rather than stalling the read loop.
const PUSH_FANOUT: usize = 64;

type Outcome = Result<serde_json::Value, serde_json::Value>;
type OutcomeTx = oneshot::Sender<Outcome>;

fn internal_error(message: &str) -> serde_json::Value {
    serde_json::json!({"code": "internal", "message": message})
}

/// Thin async client over the daemon's JSON-over-WebSocket protocol
/// (`packages/clinch-protocol`).
///
/// One client owns one WebSocket connection: an outbound pump serializes
/// queued messages, an inbound pump routes responses to their callers by
/// request id, `clinch-progress` pushes to the matching streaming call, and
/// screencast/cursor pushes to a broadcast fan-out. A malformed frame is
/// dropped; it never kills the connection. When the connection dies, every
/// in-flight call fails fast instead of hanging until its timeout.
pub struct DaemonClient {
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, OutcomeTx>>>,
    progress_routes: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<serde_json::Value>>>>,
    push_tx: broadcast::Sender<ServerPush>,
    write_tx: mpsc::UnboundedSender<Message>,
}

impl DaemonClient {
    /// Open the WebSocket and start the pump tasks.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let (ws, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            async_tungstenite::tokio::connect_async(url),
        )
        .await
        .map_err(|_| format!("connection to {url} timed out"))?
        .map_err(|error| format!("websocket handshake failed: {error}"))?;

        let (mut write_half, read_half) = ws.split();
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Message>();
        let pending: Arc<Mutex<HashMap<u64, OutcomeTx>>> = Arc::new(Mutex::new(HashMap::new()));
        let progress_routes: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<serde_json::Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (push_tx, _) = broadcast::channel::<ServerPush>(PUSH_FANOUT);

        // Outbound pump: dropping the client closes `write_tx`, which ends
        // this task and closes the socket.
        tokio::spawn(async move {
            while let Some(message) = write_rx.recv().await {
                if write_half.send(message).await.is_err() {
                    break;
                }
            }
        });

        // Inbound pump.
        {
            let pending = Arc::clone(&pending);
            let progress_routes = Arc::clone(&progress_routes);
            let push_tx = push_tx.clone();
            let ping_tx = write_tx.clone();
            tokio::spawn(async move {
                let mut read_half = read_half;
                while let Some(message) = read_half.next().await {
                    let Ok(message) = message else { break };
                    match message {
                        Message::Text(text) => {
                            route_text(&text, &pending, &progress_routes, &push_tx);
                        }
                        Message::Ping(data) => {
                            let _ = ping_tx.send(Message::Pong(data));
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
                fail_all(&pending, &progress_routes);
            });
        }

        Ok(Self {
            next_id: AtomicU64::new(1),
            pending,
            progress_routes,
            push_tx,
            write_tx,
        })
    }

    /// Invoke one daemon command and await its settled outcome.
    pub async fn call(&self, cmd: &str, params: serde_json::Value) -> Outcome {
        let Some((id, rx)) = self.send_request(cmd, params) else {
            return Err(internal_error(
                "daemon connection lost before the request was sent",
            ));
        };
        match tokio::time::timeout(RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(internal_error("daemon call cancelled")),
            Err(_) => {
                // The daemon never answered: stop tracking the id so a late
                // response cannot complete a future call.
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                if let Ok(mut routes) = self.progress_routes.lock() {
                    routes.remove(&id);
                }
                Err(internal_error("daemon request timed out"))
            }
        }
    }

    /// Invoke a streaming daemon command (`dispatch_natural_command`,
    /// `execute_playbook`, `run_task`). Progress pushes arrive on the
    /// receiver in order; the oneshot resolves with the settled outcome.
    /// Dropping the receiver early just stops delivery — the daemon run
    /// continues (the protocol has no cancel).
    ///
    /// `async` by contract (mirrors `call`); the body currently needs no
    /// `.await`, which may change if the handshake ever does.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn call_with_progress(
        &self,
        cmd: &str,
        params: serde_json::Value,
    ) -> (
        mpsc::UnboundedReceiver<serde_json::Value>,
        oneshot::Receiver<Outcome>,
    ) {
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let Some((id, rx)) = self.send_request(cmd, params) else {
            let (tx, rx_done) = oneshot::channel();
            let _ = tx.send(Err(internal_error(
                "daemon connection lost before the request was sent",
            )));
            return (progress_rx, rx_done);
        };
        if let Ok(mut routes) = self.progress_routes.lock() {
            routes.insert(id, progress_tx);
        }
        (progress_rx, rx)
    }

    /// Subscribe to screencast/cursor pushes. Each subscriber gets its own
    /// stream starting at subscribe time; used by the per-acquire context
    /// forwarder in `lib.rs`.
    pub fn subscribe_pushes(&self) -> broadcast::Receiver<ServerPush> {
        self.push_tx.subscribe()
    }

    /// Queue one request; registers the outcome channel before sending so a
    /// fast local daemon cannot answer before we are listening.
    fn send_request(
        &self,
        cmd: &str,
        params: serde_json::Value,
    ) -> Option<(u64, oneshot::Receiver<Outcome>)> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = ClientRequest {
            id,
            cmd: cmd.to_owned(),
            params,
        };
        let Ok(text) = serde_json::to_string(&request) else {
            return None;
        };
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock().ok()?;
        pending.insert(id, tx);
        drop(pending);
        if self.write_tx.send(Message::Text(text.into())).is_err() {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            return None;
        }
        Some((id, rx))
    }
}

/// Route one inbound text frame. Malformed JSON, JSON that matches no wire
/// shape, and pushes for unknown request ids are dropped silently — the
/// connection stays alive.
fn route_text(
    text: &str,
    pending: &Mutex<HashMap<u64, OutcomeTx>>,
    progress_routes: &Mutex<HashMap<u64, mpsc::UnboundedSender<serde_json::Value>>>,
    push_tx: &broadcast::Sender<ServerPush>,
) {
    let Ok(message) = serde_json::from_str::<WireMessage>(text) else {
        return;
    };
    match message {
        WireMessage::Response(response) => {
            let outcome_tx = pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&response.id));
            // The final outcome ends the progress stream; the receiver
            // observes closure after draining what arrived.
            if let Ok(mut routes) = progress_routes.lock() {
                routes.remove(&response.id);
            }
            if let Some(tx) = outcome_tx {
                let outcome = outcome_from(response);
                let _ = tx.send(outcome);
            }
        }
        WireMessage::Push(push) => {
            if push.event == event::PROGRESS {
                if let Some(id) = push.id {
                    let route = progress_routes
                        .lock()
                        .ok()
                        .and_then(|routes| routes.get(&id).cloned());
                    if let Some(tx) = route {
                        let _ = tx.send(push.payload);
                    }
                }
            } else if push.event == event::SCREENCAST_FRAME || push.event == event::CURSOR_MOVED {
                let _ = push_tx.send(push);
            }
            // Unknown push events are ignored.
        }
        // The daemon never sends requests; ignore defensively.
        WireMessage::Request(_) => {}
    }
}

fn outcome_from(response: ServerResponse) -> Outcome {
    if response.ok {
        Ok(response.result.unwrap_or(serde_json::Value::Null))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| serde_json::json!({"code": "internal", "message": null})))
    }
}

/// The socket died: fail every in-flight call now instead of leaving it
/// hanging until its timeout, and close all progress streams.
fn fail_all(
    pending: &Mutex<HashMap<u64, OutcomeTx>>,
    progress_routes: &Mutex<HashMap<u64, mpsc::UnboundedSender<serde_json::Value>>>,
) {
    if let Ok(mut pending) = pending.lock() {
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(internal_error("daemon connection lost")));
        }
    }
    if let Ok(mut routes) = progress_routes.lock() {
        routes.clear();
    }
}

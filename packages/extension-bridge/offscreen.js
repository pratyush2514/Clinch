/* Clinch Companion — offscreen socket owner (MV3).
 *
 * MV3 suspends the service worker after ~30 s idle, which kills any
 * WebSocket it holds; the worker's reconnect then flaps on a 30 s alarm
 * cycle and a sync tap can land in a dead window ("not connected" with the
 * browser open). This document owns the loopback WebSocket instead:
 * offscreen documents are not subject to worker suspension, so the socket
 * stays up for as long as the browser runs.
 *
 * Division of labour (deliberate):
 * - This document: socket lifecycle only (connect/backoff, heartbeat with
 *   real PONG correlation, message dispatch). It never touches cookies.
 * - The service worker (background.js): reads cookies on demand and owns
 *   the watchdog alarm that recreates this document if Chrome reclaims it.
 *
 * Message types on chrome.runtime (all fire-and-forget broadcasts except
 * WORKER_SYNC_REQUEST, which uses sendResponse):
 * - WORKER_SYNC_REQUEST {requestId, domain}   -> worker (expects a reply)
 * - STATUS_PING_QUERY {nonce}                 -> this document (status page)
 * - STATUS_PING_PONG {nonce, ok, rttMs}       -> status page (from here)
 *
 * Socket state is mirrored to chrome.storage.session ("bridgeSocket") so
 * the status page can render it without any messaging round-trip.
 */

const SOCKET_URL = "ws://127.0.0.1:9223";
const RECONNECT_CAP_MS = 30_000;
const HEARTBEAT_MS = 25_000;
const PONG_TIMEOUT_MS = 10_000;
const WORKER_REPLY_TIMEOUT_MS = 15_000;

let socket = null;
let reconnectDelayMs = 1000;
let heartbeatTimer = null;
/** nonce -> { resolve(ms|null), started } for heartbeat and ping-test PONGs. */
const pendingPongs = new Map();

function setSocketState(state) {
  chrome.storage.session
    .set({ bridgeSocket: state, bridgeSocketAt: Date.now() })
    .catch(() => {});
}

function send(message) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(message));
  }
}

/** One stable id per install, minted on first boot and shared with the
 * status page through storage. Sent in HELLO so the desktop app can label
 * each connected companion ("Brave · a1b2c3d4"). */
async function ensureInstallId() {
  try {
    const stored = await chrome.storage.local.get("installId");
    if (typeof stored.installId === "string" && stored.installId) {
      return stored.installId;
    }
  } catch {
    // Storage unavailable: fall through to a session-scoped id.
  }
  const fresh =
    typeof crypto !== "undefined" && crypto.randomUUID
      ? crypto.randomUUID()
      : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  try {
    await chrome.storage.local.set({ installId: fresh });
  } catch {
    // Best effort: HELLO still goes out with the fresh id.
  }
  return fresh;
}

async function helloPayload() {
  const installId = await ensureInstallId();
  let browser = "Browser";
  try {
    const brands = navigator.userAgentData?.brands ?? [];
    const pick =
      brands.find(
        (b) => /brave|chrome|edge|opera|vivaldi/i.test(b.brand) && !/chromium/i.test(b.brand),
      ) ??
      brands.find((b) => !/chromium/i.test(b.brand)) ??
      brands[0];
    if (pick && pick.brand) browser = pick.brand;
  } catch {
    // userAgentData unavailable; keep the fallback label.
  }
  return { type: "HELLO", name: "clinch-companion", version: 1, installId, browser };
}

function scheduleReconnect() {
  const delay = reconnectDelayMs;
  reconnectDelayMs = Math.min(reconnectDelayMs * 2, RECONNECT_CAP_MS);
  setTimeout(connect, delay);
}

function stopHeartbeat() {
  if (heartbeatTimer !== null) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = null;
  }
}

function startHeartbeat() {
  stopHeartbeat();
  heartbeatTimer = setInterval(() => {
    const nonce = `hb-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
    const started = Date.now();
    const timer = setTimeout(() => {
      // No PONG in time: the socket is wedged; drop and reconnect.
      pendingPongs.delete(nonce);
      try {
        socket && socket.close();
      } catch {
        // onclose drives the reconnect.
      }
    }, PONG_TIMEOUT_MS);
    pendingPongs.set(nonce, {
      started,
      resolve: () => {
        clearTimeout(timer);
        pendingPongs.delete(nonce);
      },
    });
    send({ type: "PING", nonce });
  }, HEARTBEAT_MS);
}

function connect() {
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
    return;
  }
  setSocketState("connecting");
  let ws;
  try {
    ws = new WebSocket(SOCKET_URL);
  } catch {
    setSocketState("closed");
    scheduleReconnect();
    return;
  }
  socket = ws;

  ws.onopen = async () => {
    reconnectDelayMs = 1000;
    setSocketState("open");
    send(await helloPayload());
    startHeartbeat();
  };
  ws.onmessage = (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    if (!message || typeof message.type !== "string") return;
    if (message.type === "SYNC_SESSION") {
      void handleSyncSession(message);
    } else if (message.type === "PONG" && typeof message.nonce === "string") {
      const pending = pendingPongs.get(message.nonce);
      if (pending) pending.resolve();
    }
    // Unknown types are ignored for forward compatibility.
  };
  const drop = (state) => {
    if (socket === ws) {
      socket = null;
      stopHeartbeat();
      setSocketState(state);
      scheduleReconnect();
    }
  };
  ws.onclose = () => drop("closed");
  ws.onerror = () => {
    try {
      ws.close();
    } catch {
      // Close is best-effort; onclose drives the reconnect.
    }
  };
}

/** One sync request: ask the worker for cookies, relay the answer. */
async function handleSyncSession(message) {
  const { requestId, domain } = message;
  const fail = (reason) => send({ type: "SYNC_SESSION_ERROR", requestId, reason });
  if (typeof requestId !== "string" || !requestId || typeof domain !== "string" || !domain) {
    return;
  }
  let reply;
  try {
    reply = await Promise.race([
      chrome.runtime.sendMessage({ type: "WORKER_SYNC_REQUEST", requestId, domain }),
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error("worker timeout")), WORKER_REPLY_TIMEOUT_MS),
      ),
    ]);
  } catch {
    fail("worker_unavailable");
    return;
  }
  if (!reply || typeof reply !== "object") {
    fail("internal");
    return;
  }
  if (reply.ok) {
    send({
      type: "SYNC_SESSION_RESPONSE",
      requestId,
      domain,
      cookies: reply.cookies,
      userAgent: reply.userAgent,
    });
  } else {
    fail(typeof reply.reason === "string" && reply.reason ? reply.reason : "internal");
  }
}

/** Status page ping test: measure a real loopback round-trip. */
async function runPingTest(nonce) {
  const answer = (ok, rttMs) =>
    chrome.runtime.sendMessage({ type: "STATUS_PING_PONG", nonce, ok, rttMs }).catch(() => {});
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    answer(false, null);
    return;
  }
  const started = Date.now();
  const pongNonce = `ping-${nonce}-${started}`;
  const rttMs = await new Promise((resolve) => {
    const timer = setTimeout(() => {
      pendingPongs.delete(pongNonce);
      resolve(null);
    }, PONG_TIMEOUT_MS);
    pendingPongs.set(pongNonce, {
      started,
      resolve: () => {
        clearTimeout(timer);
        pendingPongs.delete(pongNonce);
        resolve(Date.now() - started);
      },
    });
    send({ type: "PING", nonce: pongNonce });
  });
  answer(rttMs !== null, rttMs);
}

chrome.runtime.onMessage.addListener((message) => {
  // The worker handles WORKER_SYNC_REQUEST; the status page listens for
  // STATUS_PING_PONG. This document only answers ping-test queries.
  if (message && message.type === "STATUS_PING_QUERY" && typeof message.nonce === "string") {
    void runPingTest(message.nonce);
  }
});

connect();

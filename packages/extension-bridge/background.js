/* Clinch Companion (MV3) — scoped loopback session bridge.
 *
 * Load unpacked at chrome://extensions (Developer mode) while the Clinch
 * desktop app runs. The worker dials ws://127.0.0.1:9223 and answers exactly
 * one message kind: SYNC_SESSION { requestId, domain }. Cookies for anything
 * outside that domain's scope are never read off the wire format, never
 * stored, and never sent. Raw values live only in this response message.
 *
 * Two deliberate notes:
 * - "alarms" exists because MV3 suspends idle service workers (~30 s),
 *   which would silently kill the "persistent" socket otherwise.
 * - KNOWN_SSO_SECONDARIES mirrors apps/desktop/src-tauri/src/ws_server.rs.
 *   The desktop re-validates every cookie; this client filter only shrinks
 *   the payload. Keep both tables in sync when adding providers.
 */

const SOCKET_URL = "ws://127.0.0.1:9223";
const MAX_COOKIES = 500;
const MAX_VALUE_LENGTH = 16 * 1024;
const RECONNECT_CAP_MS = 30_000;

// Cross-root SSO secondaries the suffix rule cannot derive
// (chatgpt.com shares no suffix with openai.com).
const KNOWN_SSO_SECONDARIES = {
  "chatgpt.com": ["openai.com", "auth.openai.com"],
};

let socket = null;
let reconnectDelayMs = 1000;
let alarmInstalled = false;

function scopeRoots(host) {
  const h = host.toLowerCase();
  const labels = h.split(".");
  const roots = [];
  for (let start = 1; start < labels.length - 1; start++) {
    roots.push(labels.slice(start).join("."));
  }
  const extra = KNOWN_SSO_SECONDARIES[h];
  if (extra) roots.push(...extra);
  const parent = labels.slice(1).join(".");
  const parentExtra = parent && parent !== h ? KNOWN_SSO_SECONDARIES[parent] : undefined;
  if (parentExtra) roots.push(...parentExtra);
  return roots;
}

function inScope(cookieDomain, host) {
  if (!cookieDomain || !host) return false;
  const d = cookieDomain.toLowerCase().replace(/^\./, "");
  const h = host.toLowerCase();
  if (d === h || d.endsWith(`.${h}`)) return true;
  return scopeRoots(h).some((root) => d === root || d.endsWith(`.${root}`));
}

function scheduleReconnect() {
  const delay = reconnectDelayMs;
  reconnectDelayMs = Math.min(reconnectDelayMs * 2, RECONNECT_CAP_MS);
  setTimeout(connect, delay);
}

function connect() {
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
    return;
  }
  let ws;
  try {
    ws = new WebSocket(SOCKET_URL);
  } catch {
    scheduleReconnect();
    return;
  }
  socket = ws;

  ws.onopen = () => {
    reconnectDelayMs = 1000;
    if (!alarmInstalled) {
      chrome.alarms.create("clinch-keepalive", { periodInMinutes: 0.5 });
      alarmInstalled = true;
    }
    send({ type: "HELLO", name: "clinch-companion", version: 1 });
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
      void handleSync(message);
    } else if (message.type === "PING") {
      send({ type: "PONG" });
    }
    // Unknown types are ignored for forward compatibility.
  };
  const drop = () => {
    if (socket === ws) {
      socket = null;
      scheduleReconnect();
    }
  };
  ws.onclose = drop;
  ws.onerror = () => {
    try {
      ws.close();
    } catch {
      // Close is best-effort; onclose drives the reconnect.
    }
  };
}

function send(message) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(message));
  }
}

async function readTabUserAgent() {
  try {
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (tab && tab.id != null) {
      const [result] = await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        func: () => navigator.userAgent,
      });
      if (result && typeof result.result === "string" && result.result) {
        return result.result;
      }
    }
  } catch {
    // Restricted pages (chrome://, web store) reject injection — fall through.
  }
  return navigator.userAgent;
}

async function handleSync(message) {
  const { requestId, domain } = message;
  const fail = (reason) => send({ type: "SYNC_SESSION_ERROR", requestId, reason });
  if (typeof requestId !== "string" || !requestId || typeof domain !== "string" || !domain) {
    return;
  }
  try {
    const all = await chrome.cookies.getAll({});
    const cookies = [];
    for (const cookie of all) {
      if (cookies.length >= MAX_COOKIES) break;
      if (!inScope(cookie.domain, domain)) continue;
      if (typeof cookie.value !== "string" || cookie.value.length > MAX_VALUE_LENGTH) continue;
      cookies.push({
        name: cookie.name,
        value: cookie.value,
        domain: cookie.domain,
        path: cookie.path,
        secure: cookie.secure,
        httpOnly: cookie.httpOnly,
        sameSite: cookie.sameSite,
        expirationDate: cookie.expirationDate,
      });
    }
    if (cookies.length === 0) {
      fail("no_cookies");
      return;
    }
    const userAgent = await readTabUserAgent();
    send({ type: "SYNC_SESSION_RESPONSE", requestId, domain, cookies, userAgent });
    await chrome.storage.session.set({ lastSyncAt: Date.now(), lastDomain: domain });
  } catch {
    fail("internal");
  }
}

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name !== "clinch-keepalive") return;
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    connect();
  } else {
    send({ type: "PING" });
  }
});

chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
connect();

/* Clinch Companion (MV3) — scoped loopback session bridge.
 *
 * Load unpacked at chrome://extensions (Developer mode) while the Clinch
 * desktop app runs. Two parts:
 *
 * - offscreen.js (offscreen document): owns the ws://127.0.0.1:9223 socket
 *   so it survives service-worker suspension. It never touches cookies.
 * - This worker: answers cookie reads on demand and runs a watchdog alarm
 *   that recreates the offscreen document if Chrome reclaims it.
 *
 * Privacy posture, unchanged: cookies are read only when Clinch requests a
 * domain (the tap is the consent event), only in-scope cookies are ever
 * collected, and raw values live only in the single response message.
 *
 * KNOWN_SSO_SECONDARIES mirrors apps/desktop/src-tauri/src/ws_server.rs.
 * The desktop re-validates every cookie; the scoped queries below only
 * shrink what is read. Keep both tables in sync when adding providers.
 */

const OFFSCREEN_DOCUMENT_PATH = "offscreen.html";
// MV3 offers no dedicated offscreen reason for a persistent local socket;
// this document's sole job is owning the loopback WebSocket (data shuttling
// between the extension and the desktop app), which BLOBS covers least
// badly. If Chrome adds a socket-specific reason, switch to it.
const OFFSCREEN_CREATE_REASONS = ["BLOBS"];
const OFFSCREEN_JUSTIFICATION =
  "Own the loopback WebSocket to the Clinch desktop app so session sync " +
  "stays reachable while the service worker sleeps (MV3).";
const WATCHDOG_ALARM = "clinch-watchdog";
const WATCHDOG_MINUTES = 0.5;
const MAX_COOKIES = 500;
const MAX_VALUE_LENGTH = 16 * 1024;

// Cross-root SSO secondaries the suffix rule cannot derive
// (chatgpt.com shares no suffix with openai.com).
const KNOWN_SSO_SECONDARIES = {
  "chatgpt.com": ["openai.com", "auth.openai.com"],
};

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

async function ensureOffscreenDocument() {
  try {
    if (await chrome.offscreen.hasDocument()) return;
    await chrome.offscreen.createDocument({
      url: OFFSCREEN_DOCUMENT_PATH,
      reasons: OFFSCREEN_CREATE_REASONS,
      justification: OFFSCREEN_JUSTIFICATION,
    });
  } catch {
    // Creation races (two watchdogs, startup overlap) land here; the next
    // watchdog tick retries.
  }
}

function toWireCookie(cookie) {
  return {
    name: cookie.name,
    value: cookie.value,
    domain: cookie.domain,
    path: cookie.path,
    secure: cookie.secure,
    httpOnly: cookie.httpOnly,
    sameSite: cookie.sameSite,
    expirationDate: cookie.expirationDate,
  };
}

/** Scoped read: one cookie-store query per scope root instead of the whole
 * jar. `domain` filters match the domain and its subdomains, so the
 * inScope check below is belt-and-braces against store quirks. */
async function readScopedCookies(domain) {
  // Include the exact requested domain first: for a two-label host like
  // "x.com", scopeRoots alone can return nothing to query.
  const roots = [...new Set([domain, ...scopeRoots(domain)])];
  const seen = new Set();
  const cookies = [];
  for (const root of roots) {
    let batch;
    try {
      batch = await chrome.cookies.getAll({ domain: root });
    } catch {
      continue;
    }
    for (const cookie of batch) {
      if (cookies.length >= MAX_COOKIES) break;
      const key = `${cookie.domain}\n${cookie.path}\n${cookie.name}`;
      if (seen.has(key)) continue;
      seen.add(key);
      if (!inScope(cookie.domain, domain)) continue;
      if (typeof cookie.value !== "string" || cookie.value.length > MAX_VALUE_LENGTH) continue;
      cookies.push(toWireCookie(cookie));
    }
    if (cookies.length >= MAX_COOKIES) break;
  }
  return cookies;
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

/** Answer one sync request from the offscreen document. Never throws: the
 * caller always gets a shaped reply. */
async function handleSyncRequest(domain) {
  try {
    const cookies = await readScopedCookies(domain);
    if (cookies.length === 0) {
      return { ok: false, reason: "no_cookies" };
    }
    const userAgent = await readTabUserAgent();
    await chrome.storage.session.set({ lastSyncAt: Date.now(), lastDomain: domain });
    return { ok: true, cookies, userAgent };
  } catch {
    return { ok: false, reason: "internal" };
  }
}

/** Offscreen documents only expose chrome.runtime — chrome.storage is
 * undefined there (MV3 quirk), so the worker owns every storage write the
 * offscreen document needs: socket-state diagnostics and the install id. */
async function writeSocketState(state) {
  const payload = { bridgeSocket: state, bridgeSocketAt: Date.now() };
  for (const area of ["session", "local"]) {
    try {
      const store = chrome.storage && chrome.storage[area];
      if (!store) continue;
      await store.set(payload);
      return;
    } catch {
      // try the next area
    }
  }
}

/** One stable id per install, minted on first boot. Shared with the status
 * page and the offscreen document's HELLO through chrome.storage.local. */
async function ensureWorkerInstallId() {
  try {
    const stored = await chrome.storage.local.get("installId");
    if (typeof stored.installId === "string" && stored.installId) {
      return stored.installId;
    }
  } catch {
    // fall through to minting
  }
  const fresh =
    typeof crypto !== "undefined" && crypto.randomUUID
      ? crypto.randomUUID()
      : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  try {
    await chrome.storage.local.set({ installId: fresh });
  } catch {
    // Best effort: still return the fresh id for this session.
  }
  return fresh;
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || typeof message.type !== "string") return false;
  if (message.type === "OFFSCREEN_SOCKET_STATE") {
    void writeSocketState(
      typeof message.state === "string" ? message.state : "unknown"
    );
    return false; // fire-and-forget
  }
  if (message.type === "OFFSCREEN_GET_INSTALL_ID") {
    ensureWorkerInstallId().then((installId) => sendResponse({ installId }));
    return true; // async reply
  }
  if (message.type !== "WORKER_SYNC_REQUEST") return false;
  const { requestId, domain } = message;
  if (typeof requestId !== "string" || !requestId || typeof domain !== "string" || !domain) {
    sendResponse({ ok: false, reason: "internal" });
    return false;
  }
  handleSyncRequest(domain).then(sendResponse);
  return true; // async reply
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name !== WATCHDOG_ALARM) return;
  void ensureOffscreenDocument();
});

async function boot() {
  await ensureOffscreenDocument();
  try {
    await chrome.alarms.create(WATCHDOG_ALARM, { periodInMinutes: WATCHDOG_MINUTES });
  } catch {
    // Alarm setup is best-effort; startup already ensured the document.
  }
}

chrome.runtime.onStartup.addListener(boot);
chrome.runtime.onInstalled.addListener(boot);
void boot();

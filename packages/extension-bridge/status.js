/* Clinch Companion — options/status page.
 * Read-only diagnostics: nothing here sends cookies anywhere. */

const $ = (id) => document.getElementById(id);

function browserBrand() {
  try {
    const brands = navigator.userAgentData?.brands ?? [];
    const pick =
      brands.find(
        (b) => /brave|chrome|edge|opera|vivaldi/i.test(b.brand) && !/chromium/i.test(b.brand),
      ) ??
      brands.find((b) => !/chromium/i.test(b.brand)) ??
      brands[0];
    if (pick && pick.brand) return pick.brand;
  } catch {
    // fall through
  }
  return "Browser";
}

function timeAgo(ts) {
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (s < 5) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  return `${Math.floor(m / 60)}h ago`;
}

async function refresh() {
  $("extVersion").textContent = chrome.runtime.getManifest().version;
  $("browser").textContent = browserBrand();

  let installId = "";
  try {
    const stored = await chrome.storage.local.get("installId");
    installId = typeof stored.installId === "string" ? stored.installId : "";
  } catch {
    // ignore
  }
  if (!installId) {
    installId = crypto.randomUUID();
    try {
      await chrome.storage.local.set({ installId });
    } catch {
      // ignore
    }
  }
  $("installId").textContent = installId.slice(0, 8) + "…";

  let socket = "unknown";
  let socketAt = 0;
  try {
    const s = await chrome.storage.session.get(["bridgeSocket", "bridgeSocketAt"]);
    if (typeof s.bridgeSocket === "string") socket = s.bridgeSocket;
    if (typeof s.bridgeSocketAt === "number") socketAt = s.bridgeSocketAt;
  } catch {
    // ignore
  }
  const dot = $("socketDot");
  dot.className = "dot " + (socket === "open" ? "ok" : socket === "connecting" ? "warn" : "bad");
  $("socketState").textContent =
    socket + (socketAt ? ` (${timeAgo(socketAt)})` : "");

  try {
    const s = await chrome.storage.session.get(["lastSyncAt", "lastDomain"]);
    $("lastSync").textContent =
      typeof s.lastSyncAt === "number"
        ? `${timeAgo(s.lastSyncAt)}${s.lastDomain ? ` · ${s.lastDomain}` : ""}`
        : "never";
  } catch {
    $("lastSync").textContent = "never";
  }
}

$("pingBtn").addEventListener("click", () => {
  const nonce = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
  $("pingResult").textContent = "Pinging…";
  const done = (ok, rttMs) => {
    $("pingResult").textContent = ok
      ? `Reachable — round trip ${rttMs} ms.`
      : "No answer. Is the Clinch desktop app running?";
  };
  const timer = setTimeout(() => done(false), 12_000);
  const onPong = (message) => {
    if (message && message.type === "STATUS_PING_PONG" && message.nonce === nonce) {
      chrome.runtime.onMessage.removeListener(onPong);
      clearTimeout(timer);
      done(message.ok === true, typeof message.rttMs === "number" ? message.rttMs : null);
    }
  };
  chrome.runtime.onMessage.addListener(onPong);
  chrome.runtime
    .sendMessage({ type: "STATUS_PING_QUERY", nonce })
    .catch(() => {
      chrome.runtime.onMessage.removeListener(onPong);
      clearTimeout(timer);
      done(false);
    });
});

void refresh();
setInterval(refresh, 2000);

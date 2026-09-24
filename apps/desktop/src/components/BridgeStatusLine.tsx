import type { BridgeConnectionInfo } from "../lib/ipc";

/**
 * The bridge-status line under a session-lending card's actions.
 *
 * - No companion attached: an honest "not detected" hint instead of a Sync
 *   button that fails after the tap.
 * - One companion: a quiet "connected (Browser)" confirmation.
 * - Several (Brave + Chrome, two profiles): a source picker so the tap
 *   pulls the session from the browser the user means. `sourceId` is the
 *   server-side connection key; a stale key falls back to broadcast
 *   backend-side, so the tap never dies on a reconnect race.
 */
export default function BridgeStatusLine({
  connections,
  sourceId,
  onSelectSource,
}: {
  connections: BridgeConnectionInfo[];
  sourceId: number | null;
  onSelectSource: (id: number | null) => void;
}) {
  if (connections.length === 0) {
    return (
      <p className="notice">
        ○ Companion not detected — open your browser with the Clinch Companion
        extension installed, then sync.
      </p>
    );
  }
  const label = (c: BridgeConnectionInfo) => {
    const browser = c.browser || "Browser";
    if (connections.length <= 1) return browser;
    const short = c.installId ? c.installId.slice(0, 8) : "…";
    return `${browser} · ${short}`;
  };
  return (
    <p className="notice">
      <span aria-hidden="true">●</span> Companion connected
      {connections.length > 1 ? (
        <>
          {" — "}sync from{" "}
          <select
            aria-label="Choose which browser to sync from"
            value={sourceId ?? ""}
            onChange={(e) =>
              onSelectSource(e.target.value === "" ? null : Number(e.target.value))
            }
          >
            {connections.map((c) => (
              <option key={c.id} value={c.id}>
                {label(c)}
              </option>
            ))}
          </select>
        </>
      ) : (
        <> ({label(connections[0])})</>
      )}
    </p>
  );
}

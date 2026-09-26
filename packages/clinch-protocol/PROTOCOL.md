# clinch-daemon wire protocol

JSON over WebSocket. The daemon listens on `127.0.0.1:18790` by default
(`--port` / `CLINCH_DAEMON_PORT` override). Loopback only — never expose this
port; it runs the full engine with no authentication.

When the Tauri app sets `CLINCH_DAEMON_URL=ws://127.0.0.1:18790` it runs in
**remote mode**: every command below proxies to the daemon, and the daemon's
pushed events feed the app's `browser-screencast-frame` /
`browser-cursor-moved` emitters unchanged. Results and errors are opaque JSON:
the daemon serializes the exact types the embedded commands return today, so
the frontend cannot tell the modes apart.

## Message shapes

Client → server:

```json
{ "id": 7, "cmd": "browser_context_status", "params": {} }
```

Server → client (settled outcome):

```json
{ "id": 7, "ok": true, "result": { "...": "..." } }
{ "id": 7, "ok": false, "error": { "code": "busy", "message": null } }
```

Server → client (unsolicited push):

```json
{ "event": "browser-screencast-frame", "payload": { "...": "..." } }
{ "event": "browser-cursor-moved", "payload": { "...": "..." } }
{ "event": "clinch-progress", "id": 7, "payload": { "...": "..." } }
```

Malformed JSON, unknown `cmd`, and bad `params` are answered (when an `id`
is readable) with `ok: false` and a `{code:"invalid_input"}`-shaped error —
the connection is never dropped for a bad message.

## Commands

`params` is always a JSON object. Types below are the Rust types on the
daemon side; the JSON is their serde form.

| cmd | params | result | notes |
|---|---|---|---|
| `task_decision` | `{id, index, approved}` | `null` | |
| `dispatch_natural_command` | `{prompt}` | `DispatchOutcome` | streams `clinch-progress` (`PlaybookEvent`) until settled |
| `browser_viewport` | `{}` | `Viewport` | |
| `acquire_browser_context` | `{}` | `ContextStatus` | subscribes this connection to frame+cursor pushes |
| `release_browser_context` | `{}` | `null` | unsubscribes |
| `take_control` | `{url?}` | `ContextStatus` | remote note: needs a display on the daemon host |
| `browser_context_status` | `{}` | `ContextStatus` | |
| `initialize` | `{}` | `StorageStatus` | |
| `sync_session` | `{request: SyncRequest}` | `SessionStatus` | |
| `manual_login` | `{portal_url}` | `SessionStatus` | |
| `close_browser` | `{}` | `null` | |
| `bridge_status` | `{}` | `BridgeStatus` | served by the daemon's own bridge (port 9223) |
| `bridge_sync_session` | `{portal_url}` | `SessionStatus` | |
| `lend_session` | `{request: LendRequest}` | `LendOutcome` | |
| `forget_site_session` | `{host}` | `usize` | |
| `auth_status` | `{}` | `AuthPanel?` | |
| `begin_embedded_auth` | `{portal_url}` | `AuthPanel` | |
| `complete_embedded_auth` | `{}` | `SessionStatus` | |
| `cancel_embedded_auth` | `{}` | `null` | |
| `picker_enable` | `{}` | `null` | |
| `picker_status` | `{}` | `PickerStatus` | |
| `picker_pick` | `{timeout_ms?}` | `PickedElement` | |
| `picker_disable` | `{}` | `null` | |
| `preview_intent` | `{role, label}` | `IntentPreview` | |
| `save_playbook` | `{name, portal_url, steps: Step[]}` | `String` (id) | |
| `save_run_as_workflow` | `{run_id, name, description?}` | `String` (id) | |
| `list_playbooks` | `{}` | `PlaybookSummary[]` | |
| `save_site_shortcut` | `{name, url}` | `SiteShortcut` | |
| `list_site_shortcuts` | `{}` | `SiteShortcut[]` | |
| `delete_site_shortcut` | `{name}` | `bool` | |
| `execute_playbook` | `{id}` | `SequenceOutcome` | streams `clinch-progress` (`PlaybookEvent`) |
| `decide_playbook` | `{run_id, index, approved}` | `null` | |
| `get_poc_metrics` | `{}` | `PocMetrics` | |
| `run_task` | `{request: TaskRequest}` | `Task` | streams `clinch-progress` (`TaskEvent`) |
| `get_task` | `{id}` | `Task` | |
| `preview_approval` | `{}` | `ApprovalPreview` | |
| `resolve_approval` | `{id, approved}` | `bool` | |
| `downloaded_file_path` | `{id, index}` | `String` (abs path) | daemon-only helper; the Tauri thin client translates to `\\wsl$\` and reveals locally |

`downloaded_file_action` (reveal/open in the OS) is intentionally **not**
proxied: files live on the daemon host, so the thin client resolves the path
via `downloaded_file_path` and opens it with the local OS opener.

## Event payloads

- `browser-screencast-frame`: `browser_driver::ScreencastFrame` JSON —
  identical to the embedded Tauri event payload.
- `browser-cursor-moved`: `browser_driver::CursorEvent` JSON — identical.
- `clinch-progress`: `PlaybookEvent` / `orchestration_engine::TaskEvent`
  JSON, with the originating request `id`.

## Security notes

- Bind is `127.0.0.1` only. In WSL2 the Windows host reaches it via the
  built-in localhost relay; the Companion extension likewise dials
  `127.0.0.1:9223` from Windows Chrome.
- No auth on the socket: loopback is the trust boundary, same as the
  Companion bridge. Do not port-forward it.
- No credentials cross the socket beyond what the embedded IPC already
  carries (cookie values never leave the daemon; only outcome metadata).

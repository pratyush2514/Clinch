#![deny(unsafe_code)]
//! Wire protocol between `clinch-daemon` (the engine, on Linux/WSL2) and its
//! clients (the Tauri thin client in remote mode).
//!
//! JSON over WebSocket. Three message shapes:
//!
//! - Client → server: [`ClientRequest`] — one command invocation.
//! - Server → client: [`ServerResponse`] — the command's settled outcome.
//! - Server → client: [`ServerPush`] — unsolicited events (screencast frames,
//!   cursor positions, progress for long-running commands).
//!
//! Results and errors travel as opaque [`serde_json::Value`]: the daemon
//! serializes the exact types the embedded Tauri commands return today
//! (including `AppError`-shaped errors), and the
//! Tauri proxy forwards the JSON untouched — so the TypeScript frontend
//! observes byte-identical payloads in embedded and remote mode.
//!
//! The full command table lives in `PROTOCOL.md`.

use serde::{Deserialize, Serialize};

/// Default loopback port for the daemon. Override with `--port` or
/// `CLINCH_DAEMON_PORT`.
pub const DEFAULT_PORT: u16 = 18790;
/// Env var the Tauri app reads: when set (e.g. `ws://127.0.0.1:18790`) the
/// app runs in remote mode and proxies commands to the daemon. Absent =
/// embedded mode, unchanged behavior.
pub const DAEMON_URL_ENV: &str = "CLINCH_DAEMON_URL";
/// Env var (and `--port` flag) selecting the daemon's listen port.
pub const DAEMON_PORT_ENV: &str = "CLINCH_DAEMON_PORT";
/// WSL distro name used to translate daemon-side Linux paths to Windows
/// `\\wsl$\` paths for local file reveal. Default `Ubuntu`.
pub const WSL_DISTRO_ENV: &str = "CLINCH_WSL_DISTRO";
pub const DEFAULT_WSL_DISTRO: &str = "Ubuntu";

/// Pushed event names. The first two are exactly the Tauri event names the
/// frontend already subscribes to; the daemon reuses them so the frontend is
/// untouched.
pub mod event {
    /// Live screencast frame payload (`browser_driver::ScreencastFrame` JSON).
    pub const SCREENCAST_FRAME: &str = "browser-screencast-frame";
    /// Agent cursor payload (`browser_driver::CursorEvent` JSON).
    pub const CURSOR_MOVED: &str = "browser-cursor-moved";
    /// Progress for long-running commands (`dispatch_natural_command`,
    /// `execute_playbook`, `run_task`). Carries the originating request `id`.
    pub const PROGRESS: &str = "clinch-progress";
}

/// Command names. One per Tauri command the daemon serves; see PROTOCOL.md
/// for each command's `params` shape.
pub mod cmd {
    pub const TASK_DECISION: &str = "task_decision";
    pub const DISPATCH_NATURAL_COMMAND: &str = "dispatch_natural_command";
    pub const BROWSER_VIEWPORT: &str = "browser_viewport";
    pub const ACQUIRE_BROWSER_CONTEXT: &str = "acquire_browser_context";
    pub const RELEASE_BROWSER_CONTEXT: &str = "release_browser_context";
    pub const TAKE_CONTROL: &str = "take_control";
    pub const BROWSER_CONTEXT_STATUS: &str = "browser_context_status";
    pub const INITIALIZE: &str = "initialize";
    pub const SYNC_SESSION: &str = "sync_session";
    pub const MANUAL_LOGIN: &str = "manual_login";
    pub const CLOSE_BROWSER: &str = "close_browser";
    pub const BRIDGE_STATUS: &str = "bridge_status";
    pub const BRIDGE_SYNC_SESSION: &str = "bridge_sync_session";
    pub const LEND_SESSION: &str = "lend_session";
    pub const FORGET_SITE_SESSION: &str = "forget_site_session";
    pub const AUTH_STATUS: &str = "auth_status";
    pub const BEGIN_EMBEDDED_AUTH: &str = "begin_embedded_auth";
    pub const COMPLETE_EMBEDDED_AUTH: &str = "complete_embedded_auth";
    pub const CANCEL_EMBEDDED_AUTH: &str = "cancel_embedded_auth";
    pub const PICKER_ENABLE: &str = "picker_enable";
    pub const PICKER_STATUS: &str = "picker_status";
    pub const PICKER_PICK: &str = "picker_pick";
    pub const PICKER_DISABLE: &str = "picker_disable";
    pub const PREVIEW_INTENT: &str = "preview_intent";
    pub const SAVE_PLAYBOOK: &str = "save_playbook";
    pub const SAVE_RUN_AS_WORKFLOW: &str = "save_run_as_workflow";
    pub const LIST_PLAYBOOKS: &str = "list_playbooks";
    pub const SAVE_SITE_SHORTCUT: &str = "save_site_shortcut";
    pub const LIST_SITE_SHORTCUTS: &str = "list_site_shortcuts";
    pub const DELETE_SITE_SHORTCUT: &str = "delete_site_shortcut";
    pub const EXECUTE_PLAYBOOK: &str = "execute_playbook";
    pub const DECIDE_PLAYBOOK: &str = "decide_playbook";
    pub const GET_POC_METRICS: &str = "get_poc_metrics";
    pub const RUN_TASK: &str = "run_task";
    pub const GET_TASK: &str = "get_task";
    pub const PREVIEW_APPROVAL: &str = "preview_approval";
    pub const RESOLVE_APPROVAL: &str = "resolve_approval";
    /// Daemon-only helper (no Tauri command): returns the daemon-side
    /// absolute path of a downloaded file so the thin client can reveal it
    /// locally (via `\\wsl$\` translation on Windows).
    pub const DOWNLOADED_FILE_PATH: &str = "downloaded_file_path";
}

/// Client → server: one command invocation. `params` is a JSON object whose
/// shape is documented per command in PROTOCOL.md.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClientRequest {
    /// Client-chosen correlation id, echoed in the response and in any
    /// progress pushes for this invocation.
    pub id: u64,
    /// One of [`cmd::*`].
    pub cmd: String,
    /// Command arguments as a JSON object. Unknown commands and malformed
    /// params are answered with `ok: false`, never a dropped connection.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Server → client: the settled outcome of one [`ClientRequest`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServerResponse {
    pub id: u64,
    /// Exactly one of `result` / `error` is `Some`.
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

impl ServerResponse {
    #[must_use]
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// `error` carries the daemon's serialized `AppError` JSON unchanged, so
    /// the frontend sees the same error shape as embedded mode.
    #[must_use]
    pub fn err(id: u64, error: serde_json::Value) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

/// Server → client: unsolicited event. `event` is one of [`event::*`];
/// progress pushes also carry the originating request `id`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServerPush {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub payload: serde_json::Value,
}

/// The wire envelope: every WebSocket text message is one of these.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum WireMessage {
    Request(ClientRequest),
    Response(ServerResponse),
    Push(ServerPush),
}

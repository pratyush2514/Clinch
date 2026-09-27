//! `clinch-daemon`: the Clinch engine as a standalone Linux daemon.
//!
//! Hosting split: the full engine (`AppService`, the Companion bridge, the
//! auth helpers) runs here and is served over a localhost WebSocket speaking
//! the [`clinch_protocol`] wire protocol. The Tauri app becomes a thin
//! client in remote mode (`CLINCH_DAEMON_URL` set); with it unset the app
//! keeps running embedded, unchanged.
//!
//! The engine is included, not moved — the three files stay canonical in
//! `apps/desktop/src-tauri/src/` and are compiled into this crate via
//! `#[path]`, so embedded and remote mode always run the same code.
//!
//! Security: the listener binds `127.0.0.1` only. Loopback is the trust
//! boundary — the socket runs the full engine with no authentication, same
//! as the Companion bridge. Never bind this to a public interface and do
//! not port-forward it.

#![deny(unsafe_code)]

#[path = "../../../apps/desktop/src-tauri/src/auth.rs"]
pub mod auth;
#[path = "../../../apps/desktop/src-tauri/src/service.rs"]
pub mod service;
#[path = "../../../apps/desktop/src-tauri/src/ws_server.rs"]
pub mod ws_server;

use async_tungstenite::tungstenite::Message;
pub use clinch_protocol::{ClientRequest, ServerPush, ServerResponse, WireMessage, cmd, event};
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Per-connection state: the outbound queue plus whether this connection is
/// the current screencast/cursor subscriber.
pub struct ConnCtx {
    out: mpsc::UnboundedSender<WireMessage>,
    stream_subscribed: bool,
}

impl ConnCtx {
    pub fn new(out: mpsc::UnboundedSender<WireMessage>) -> Self {
        Self {
            out,
            stream_subscribed: false,
        }
    }
}

/// A long-running command whose progress streams as `clinch-progress` pushes
/// until the final [`ServerResponse`] settles it.
pub enum StreamKind {
    DispatchNatural {
        prompt: String,
    },
    ExecutePlaybook {
        id: String,
    },
    RunTask {
        request: orchestration_engine::TaskRequest,
    },
}

/// What the connection loop must do after [`handle_request`] classifies one
/// request. Kept as an enum (rather than `Option<ServerResponse>`) because
/// streaming commands settle later, through the connection's outbound queue
/// — the handler itself stays socket-free and unit-testable.
pub enum Action {
    /// Send this response immediately.
    Reply(ServerResponse),
    /// Accepted: a spawned task streams `clinch-progress` pushes for `id`
    /// and then the final response. No immediate reply.
    Stream { id: u64, kind: StreamKind },
}

/// `{"code":"invalid_input","message":"..."}` — the shape for unknown
/// commands, malformed params, and malformed JSON with a readable id.
pub fn invalid_input(id: u64, message: &str) -> ServerResponse {
    ServerResponse::err(
        id,
        serde_json::json!({ "code": "invalid_input", "message": message }),
    )
}

/// Best-effort JSON: serialization of engine types cannot realistically
/// fail; `Null` is the fail-closed fallback, never a panic.
pub fn to_json(value: &impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

pub fn settle<T: serde::Serialize>(
    id: u64,
    result: Result<T, service::AppError>,
) -> ServerResponse {
    match result {
        Ok(value) => ServerResponse::ok(id, to_json(&value)),
        Err(error) => ServerResponse::err(id, to_json(&error)),
    }
}

/// Deserialize `req.params` into `T`.
///
/// # Errors
///
/// Returns an `invalid_input` [`ServerResponse`] when the params do not
/// match `T`.
pub fn from_params<T>(req: &ClientRequest) -> Result<T, ServerResponse>
where
    T: for<'de> serde::Deserialize<'de>,
{
    serde_json::from_value(req.params.clone())
        .map_err(|_| invalid_input(req.id, "malformed params"))
}

/// Classify one inbound text message. Returns `None` when no request id is
/// readable — the message is ignored and the connection stays open. A
/// structurally broken message *with* a readable id becomes an empty-cmd
/// request so the sender gets an `invalid_input` reply it can correlate.
pub fn parse_request(text: &str) -> Option<ClientRequest> {
    if let Ok(req) = serde_json::from_str::<ClientRequest>(text) {
        return Some(req);
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let id = value.get("id")?.as_u64()?;
    Some(ClientRequest {
        id,
        cmd: String::new(),
        params: serde_json::Value::Null,
    })
}

/// Dispatch one request against the engine. Pure request/response mapping —
/// no sockets — so tests exercise this directly.
#[allow(clippy::too_many_lines)]
pub async fn handle_request(
    service: &service::AppService,
    req: ClientRequest,
    ctx: &mut ConnCtx,
) -> Action {
    // Param shapes, one per command (see PROTOCOL.md). Missing optional
    // fields fall back to the embedded defaults.
    #[derive(serde::Deserialize)]
    struct DecisionParams {
        id: orchestration_engine::TaskId,
        index: usize,
        approved: bool,
    }
    #[derive(serde::Deserialize)]
    struct PromptParams {
        prompt: String,
    }
    #[derive(serde::Deserialize)]
    struct UrlParams {
        #[serde(default)]
        url: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct PortalParams {
        portal_url: String,
    }
    #[derive(serde::Deserialize)]
    struct SyncParams {
        request: session_sync::SyncRequest,
    }
    #[derive(serde::Deserialize)]
    struct LendParams {
        request: service::LendRequest,
    }
    #[derive(serde::Deserialize)]
    struct HostParams {
        host: String,
    }
    #[derive(serde::Deserialize)]
    struct PickerPickParams {
        #[serde(default)]
        timeout_ms: Option<u64>,
    }
    #[derive(serde::Deserialize)]
    struct IntentParams {
        role: String,
        label: String,
    }
    #[derive(serde::Deserialize)]
    struct SavePlaybookParams {
        name: String,
        portal_url: String,
        steps: Vec<playbook_store::Step>,
    }
    #[derive(serde::Deserialize)]
    struct SaveRunParams {
        run_id: String,
        name: String,
        #[serde(default)]
        description: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct ShortcutParams {
        name: String,
        url: String,
    }
    #[derive(serde::Deserialize)]
    struct NameParams {
        name: String,
    }
    #[derive(serde::Deserialize)]
    struct IdParams {
        id: String,
    }
    #[derive(serde::Deserialize)]
    struct PlaybookDecisionParams {
        run_id: u64,
        index: usize,
        approved: bool,
    }
    #[derive(serde::Deserialize)]
    struct RunTaskParams {
        request: orchestration_engine::TaskRequest,
    }
    #[derive(serde::Deserialize)]
    struct GetTaskParams {
        id: orchestration_engine::TaskId,
    }
    #[derive(serde::Deserialize)]
    struct ApprovalParams {
        id: u64,
        approved: bool,
    }
    #[derive(serde::Deserialize)]
    struct DownloadedFileParams {
        id: orchestration_engine::TaskId,
        index: usize,
    }

    let reply = |response: ServerResponse| Action::Reply(response);
    match req.cmd.as_str() {
        cmd::TASK_DECISION => {
            let params: DecisionParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service
                    .decide_task(params.id, params.index, params.approved)
                    .await,
            ))
        }
        cmd::DISPATCH_NATURAL_COMMAND => {
            let params: PromptParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            Action::Stream {
                id: req.id,
                kind: StreamKind::DispatchNatural {
                    prompt: params.prompt,
                },
            }
        }
        cmd::BROWSER_VIEWPORT => reply(settle(req.id, service.viewport().await)),
        cmd::ACQUIRE_BROWSER_CONTEXT => {
            let frame_tx = ctx.out.clone();
            let cursor_tx = ctx.out.clone();
            let status = service
                .acquire_context(
                    move |frame: browser_driver::ScreencastFrame| {
                        let _ = frame_tx.send(WireMessage::Push(ServerPush {
                            // The engine's event names match the wire
                            // protocol's (`browser-screencast-frame`), so the
                            // frontend observes identical payloads.
                            event: service::SCREENCAST_EVENT.to_owned(),
                            id: None,
                            payload: to_json(&frame),
                        }));
                    },
                    move |cursor: browser_driver::CursorEvent| {
                        let _ = cursor_tx.send(WireMessage::Push(ServerPush {
                            event: service::CURSOR_EVENT.to_owned(),
                            id: None,
                            payload: to_json(&cursor),
                        }));
                    },
                )
                .await;
            // Mirrors the engine's double-acquire semantics: `acquire_context`
            // replaces the cursor sink and aborts the previous frame pump, so
            // the newest acquirer wins and an older subscriber simply stops
            // receiving — no new behavior invented here.
            ctx.stream_subscribed = true;
            reply(settle(req.id, status))
        }
        cmd::RELEASE_BROWSER_CONTEXT => {
            ctx.stream_subscribed = false;
            reply(settle(req.id, service.release_context().await))
        }
        cmd::TAKE_CONTROL => {
            let params: UrlParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            // Proxied as-is. NOTE: take_control needs a display on the daemon
            // host; under Xvfb it cannot be interactive — behavior unchanged
            // from embedded mode.
            reply(settle(req.id, service.take_control(params.url).await))
        }
        cmd::BROWSER_CONTEXT_STATUS => reply(settle(req.id, service.context_status())),
        cmd::INITIALIZE => reply(settle(req.id, service.initialize().await)),
        cmd::SYNC_SESSION => {
            let params: SyncParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(req.id, service.sync(params.request).await))
        }
        cmd::MANUAL_LOGIN => {
            let params: PortalParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.manual_login(&params.portal_url).await,
            ))
        }
        cmd::CLOSE_BROWSER => reply(settle(req.id, service.close_browser().await)),
        cmd::BRIDGE_STATUS => reply(settle(req.id, service.bridge_status())),
        cmd::BRIDGE_SYNC_SESSION => {
            let params: PortalParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.bridge_sync(&params.portal_url).await,
            ))
        }
        cmd::LEND_SESSION => {
            let params: LendParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(req.id, service.lend_session(params.request).await))
        }
        cmd::FORGET_SITE_SESSION => {
            let params: HostParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.forget_site_session(params.host).await,
            ))
        }
        cmd::AUTH_STATUS => reply(settle(req.id, service.auth_status())),
        cmd::BEGIN_EMBEDDED_AUTH => {
            let params: PortalParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.begin_embedded_auth(&params.portal_url).await,
            ))
        }
        cmd::COMPLETE_EMBEDDED_AUTH => {
            reply(settle(req.id, service.complete_embedded_auth().await))
        }
        cmd::CANCEL_EMBEDDED_AUTH => reply(settle(req.id, service.cancel_embedded_auth().await)),
        cmd::PICKER_ENABLE => reply(settle(req.id, service.picker_enable().await)),
        cmd::PICKER_STATUS => reply(settle(req.id, service.picker_status())),
        cmd::PICKER_PICK => {
            let params: PickerPickParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service
                    .picker_pick(params.timeout_ms.unwrap_or(60_000))
                    .await,
            ))
        }
        cmd::PICKER_DISABLE => reply(settle(req.id, service.picker_disable().await)),
        cmd::PREVIEW_INTENT => {
            let params: IntentParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.preview_intent(params.role, params.label).await,
            ))
        }
        cmd::SAVE_PLAYBOOK => {
            let params: SavePlaybookParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service
                    .save_playbook(params.name, params.portal_url, params.steps)
                    .await,
            ))
        }
        cmd::SAVE_RUN_AS_WORKFLOW => {
            let params: SaveRunParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service
                    .save_run_as_workflow(params.run_id, params.name, params.description)
                    .await,
            ))
        }
        cmd::LIST_PLAYBOOKS => reply(settle(req.id, service.list_playbooks().await)),
        cmd::SAVE_SITE_SHORTCUT => {
            let params: ShortcutParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.save_site_shortcut(params.name, params.url).await,
            ))
        }
        cmd::LIST_SITE_SHORTCUTS => reply(settle(req.id, service.list_site_shortcuts().await)),
        cmd::DELETE_SITE_SHORTCUT => {
            let params: NameParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.delete_site_shortcut(params.name).await,
            ))
        }
        cmd::EXECUTE_PLAYBOOK => {
            let params: IdParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            Action::Stream {
                id: req.id,
                kind: StreamKind::ExecutePlaybook { id: params.id },
            }
        }
        cmd::DECIDE_PLAYBOOK => {
            let params: PlaybookDecisionParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.decide_playbook(params.run_id, params.index, params.approved),
            ))
        }
        cmd::GET_POC_METRICS => reply(settle(req.id, service.poc_metrics().await)),
        cmd::RUN_TASK => {
            let params: RunTaskParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            Action::Stream {
                id: req.id,
                kind: StreamKind::RunTask {
                    request: params.request,
                },
            }
        }
        cmd::GET_TASK => {
            let params: GetTaskParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(req.id, service.task(params.id).await))
        }
        cmd::PREVIEW_APPROVAL => reply(settle(req.id, service.preview_approval())),
        cmd::RESOLVE_APPROVAL => {
            let params: ApprovalParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            reply(settle(
                req.id,
                service.resolve_approval(params.id, params.approved),
            ))
        }
        cmd::DOWNLOADED_FILE_PATH => {
            let params: DownloadedFileParams = match from_params(&req) {
                Ok(params) => params,
                Err(response) => return reply(response),
            };
            // Daemon-only helper (no Tauri command): the thin client
            // translates the daemon-side absolute path to `\\wsl$\` and
            // reveals it with the local OS opener.
            let result = service
                .downloaded_file(params.id, params.index)
                .await
                .map(|path| path.to_string_lossy().into_owned());
            reply(settle(req.id, result))
        }
        // Unknown command (including the empty-cmd salvage from
        // `parse_request`): answered, never a dropped connection.
        _ => reply(invalid_input(req.id, "unknown command")),
    }
}

/// Run a long-lived command on a spawned task: every emitted engine event
/// becomes a `clinch-progress` push carrying the request id, and the settled
/// outcome becomes the final [`ServerResponse`].
pub fn spawn_streaming(
    service: Arc<service::AppService>,
    out: mpsc::UnboundedSender<WireMessage>,
    id: u64,
    kind: StreamKind,
) {
    tokio::spawn(async move {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ServerPush>();
        let run = async move {
            let push = |payload: serde_json::Value| {
                let _ = progress_tx.send(ServerPush {
                    event: event::PROGRESS.to_owned(),
                    id: Some(id),
                    payload,
                });
            };
            match kind {
                StreamKind::DispatchNatural { prompt } => {
                    let outcome = service
                        .dispatch_natural_command(prompt, |event| push(to_json(&event)))
                        .await;
                    settle(id, outcome)
                }
                StreamKind::ExecutePlaybook { id: playbook_id } => {
                    let outcome = service
                        .execute_playbook(playbook_id, |event| push(to_json(&event)))
                        .await;
                    settle(id, outcome)
                }
                StreamKind::RunTask { request } => {
                    let outcome = service
                        .run_task(&request, |event| push(to_json(&event)))
                        .await;
                    settle(id, outcome)
                }
            }
        };
        tokio::pin!(run);
        loop {
            tokio::select! {
                Some(msg) = progress_rx.recv() => {
                    if out.send(WireMessage::Push(msg)).is_err() {
                        break;
                    }
                }
                response = &mut run => {
                    let _ = out.send(WireMessage::Response(response));
                    break;
                }
            }
        }
    });
}

/// Serve one WebSocket connection: read text messages, dispatch, and pump
/// the outbound queue (responses, progress, frames, cursor) to the socket.
/// A bad message never drops the connection.
pub async fn serve_connection(service: Arc<service::AppService>, stream: tokio::net::TcpStream) {
    let Ok(ws) = async_tungstenite::tokio::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut incoming) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WireMessage>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(text) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut ctx = ConnCtx::new(out_tx.clone());
    while let Some(message) = incoming.next().await {
        let Ok(message) = message else {
            break;
        };
        let Message::Text(text) = message else {
            continue;
        };
        let Some(req) = parse_request(&text) else {
            // No readable id: nothing to correlate — ignore and stay open.
            continue;
        };
        // Log command names and ids only — never params or payloads.
        eprintln!("[clinch-daemon] cmd={} id={}", req.cmd, req.id);
        match handle_request(&service, req, &mut ctx).await {
            Action::Reply(response) => {
                if out_tx.send(WireMessage::Response(response)).is_err() {
                    break;
                }
            }
            Action::Stream { id, kind } => {
                spawn_streaming(service.clone(), out_tx.clone(), id, kind);
            }
        }
    }
    // One deliberate adaptation vs embedded mode: a disconnected subscriber
    // must not hold the managed browser hostage. The engine re-attaches
    // lazily on the next acquire, so this only idles the context.
    if ctx.stream_subscribed {
        let _ = service.release_context().await;
    }
    drop(ctx);
    drop(out_tx);
    let _ = writer.await;
}

pub fn home_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from)
}

/// Engine data dir: `~/.local/share/clinch-daemon`, honoring `XDG_DATA_HOME`.
pub fn data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.trim().is_empty()
    {
        return PathBuf::from(xdg).join("clinch-daemon");
    }
    home_dir()
        .join(".local")
        .join("share")
        .join("clinch-daemon")
}

/// `--port <u16>` wins; then `CLINCH_DAEMON_PORT`; then 18790.
///
/// # Errors
///
/// Returns a message when an argument is unknown or a port value is invalid.
pub fn resolve_port(args: &[String]) -> Result<u16, String> {
    let mut port: Option<u16> = None;
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "--port" {
            let value = rest
                .next()
                .ok_or_else(|| "--port needs a value".to_owned())?;
            port = Some(
                value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port value: {value}"))?,
            );
        } else if arg == "--help" || arg == "-h" {
            return Err("help".to_owned());
        } else {
            return Err(format!("unknown argument: {arg}"));
        }
    }
    if let Some(port) = port {
        return Ok(port);
    }
    if let Ok(env) = std::env::var(clinch_protocol::DAEMON_PORT_ENV)
        && !env.trim().is_empty()
    {
        return env
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("invalid {}: {env}", clinch_protocol::DAEMON_PORT_ENV));
    }
    Ok(clinch_protocol::DEFAULT_PORT)
}

pub fn usage() -> &'static str {
    "usage: clinch-daemon [--port <u16>]\n\nServes the Clinch engine over a localhost WebSocket\n(JSON protocol, see clinch-protocol/PROTOCOL.md).\n\n  --port <u16>   listen port (default: $CLINCH_DAEMON_PORT or 18790)\n\nBinds 127.0.0.1 only — never expose this port."
}

/// Bind the daemon listener on loopback. Split out so tests can bind port 0.
///
/// # Errors
///
/// Propagates the OS error when the loopback bind fails.
pub async fn bind_listener(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(("127.0.0.1", port)).await
}

pub async fn run() {
    let args: Vec<String> = std::env::args().collect();
    let port = match resolve_port(&args) {
        Ok(port) => port,
        Err(reason) => {
            if reason == "help" {
                println!("{}", usage());
                return;
            }
            eprintln!("[clinch-daemon] error: {reason}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };

    let data = data_dir();
    let home = home_dir();
    if let Err(error) = std::fs::create_dir_all(&data) {
        eprintln!(
            "[clinch-daemon] error: cannot create data dir {}: {error}",
            data.display()
        );
        std::process::exit(1);
    }

    let service = Arc::new(service::AppService::new(data, home));
    if let Err(error) = service.initialize().await {
        eprintln!(
            "[clinch-daemon] error: engine initialize failed: {}",
            to_json(&error)
        );
        std::process::exit(1);
    }
    match service.ensure_bridge().await {
        Ok(bridge_port) => {
            eprintln!("[clinch-daemon] companion bridge on 127.0.0.1:{bridge_port}");
        }
        Err(error) => {
            eprintln!(
                "[clinch-daemon] error: bridge failed to start: {}",
                to_json(&error)
            );
            std::process::exit(1);
        }
    }

    let listener = match bind_listener(port).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[clinch-daemon] error: cannot bind 127.0.0.1:{port}: {error}");
            std::process::exit(1);
        }
    };
    eprintln!("[clinch-daemon] listening on 127.0.0.1:{port}");
    loop {
        tokio::select! {
            () = shutdown() => {
                eprintln!("[clinch-daemon] shutting down");
                // Hard-stop the managed browser like the desktop app does
                // on exit; in-flight runs fail closed on their own tasks.
                service.terminate_browser();
                break;
            }
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        eprintln!("[clinch-daemon] accept error: {error}");
                        continue;
                    }
                };
                let service = service.clone();
                tokio::spawn(async move {
                    serve_connection(service, stream).await;
                });
            }
        }
    }
}

/// Ctrl-C / SIGTERM: the daemon's only shutdown path.
pub async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

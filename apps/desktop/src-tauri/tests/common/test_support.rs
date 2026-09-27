use async_tungstenite::tungstenite::Message;
use clinch_protocol::{ClientRequest, ServerPush, ServerResponse, WireMessage};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpListener;

/// Minimal in-test daemon: accepts one connection, answers each
/// `ClientRequest` with the raw text frames `behavior` returns (so tests
/// can also inject malformed frames). Returns the `ws://` URL to dial.
pub async fn spawn_mock_daemon(
    behavior: impl Fn(ClientRequest) -> Vec<String> + Send + Sync + 'static,
) -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("bind mock daemon: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("mock daemon address: {error}"))?;
    let behavior = Arc::new(behavior);
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut ws) = async_tungstenite::tokio::accept_async(stream).await else {
            return;
        };
        while let Some(message) = ws.next().await {
            let Ok(Message::Text(text)) = message else {
                continue;
            };
            // Answer unparseable requests the way the real daemon does:
            // `ok: false`, connection stays up.
            let Ok(request) = serde_json::from_str::<ClientRequest>(&text) else {
                let response = ServerResponse::err(
                    0,
                    serde_json::json!({"code": "invalid_input", "message": null}),
                );
                send_text(&mut ws, &response).await;
                continue;
            };
            for raw in behavior(request) {
                if ws.send(Message::Text(raw.into())).await.is_err() {
                    return;
                }
            }
        }
    });
    Ok(format!("ws://{address}"))
}

async fn send_text<S>(ws: &mut S, response: &ServerResponse)
where
    S: SinkExt<Message, Error = async_tungstenite::tungstenite::Error> + Unpin,
{
    if let Ok(text) = serde_json::to_string(&WireMessage::Response(response.clone())) {
        let _ = ws.send(Message::Text(text.into())).await;
    }
}

fn wire_text(message: &WireMessage) -> String {
    serde_json::to_string(message).unwrap_or_default()
}

pub fn response_text(response: &ServerResponse) -> String {
    wire_text(&WireMessage::Response(response.clone()))
}

pub fn push_text(push: &ServerPush) -> String {
    wire_text(&WireMessage::Push(push.clone()))
}

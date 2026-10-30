//! A minimal CPP/1 client over the frame's WebSocket (`/ws?token=`): one JSON envelope per text
//! message, requests answered by `res`, stream items and events ignored or collected.

use cc_protocol::envelope::{Message, RpcError};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[derive(Debug, Clone, Deserialize)]
pub struct FrameInfo {
    pub port: u16,
    /// In frame.json only where there is no Keychain; on macOS it is read from the Keychain.
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub pid: u32,
}

pub fn home_dir(explicit: Option<&str>) -> PathBuf {
    if let Some(home) = explicit {
        return PathBuf::from(home);
    }
    if let Ok(home) = std::env::var("CORTEX_HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home);
    }
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".cortexcode")
}

pub fn read_frame_info(home: &Path) -> Result<FrameInfo, String> {
    if let Ok(token) = std::env::var("CORTEX_COMMAND_TOKEN") {
        let url = std::env::var("CORTEX_COMMAND_URL").map_err(|_| "command access has no frame URL".to_owned())?;
        let port = url
            .rsplit(':')
            .next()
            .and_then(|port| port.parse::<u16>().ok())
            .ok_or_else(|| "command access has an invalid frame URL".to_owned())?;
        return Ok(FrameInfo { port, token, pid: 0 });
    }
    let path = home.join("run/frame.json");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("the frame is not running ({}: {error})", path.display()))?;
    let mut info: FrameInfo = serde_json::from_str(&text).map_err(|error| format!("{} is not valid: {error}", path.display()))?;
    if info.token.is_empty() {
        info.token = keychain_token(home)?;
    }
    Ok(info)
}

/// The frame's API token from the macOS Keychain (the frame writes it there at every start),
/// through `/usr/bin/security`, which made the item, so nothing asks.
fn keychain_token(home: &Path) -> Result<String, String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", "CortexCode frame token", "-a", &home.display().to_string(), "-w"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot read the frame's token from the Keychain: {error}"))?;
    if !output.status.success() {
        return Err(format!("the frame's token is not in the Keychain: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_owned())
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Client {
    socket: Socket,
    next_id: u64,
}

impl Client {
    pub async fn connect(info: &FrameInfo) -> Result<Client, String> {
        let url = format!("ws://127.0.0.1:{}/ws?token={}", info.port, info.token);
        let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|error| format!("cannot connect to the frame on port {}: {error}", info.port))?;
        Ok(Client { socket, next_id: 1 })
    }

    pub async fn connect_url(url: &str) -> Result<Client, String> {
        let (socket, _) = tokio_tungstenite::connect_async(url).await.map_err(|error| format!("cannot connect to {url}: {error}"))?;
        Ok(Client { socket, next_id: 1 })
    }

    /// Call a method and wait for its final response. Stream items are passed to `on_item`.
    pub async fn call_with(&mut self, method: &str, params: Value, mut on_item: impl FnMut(&Value)) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Message::Req { id, method: method.to_owned(), params };
        let text = serde_json::to_string(&request).map_err(|error| RpcError::internal(error.to_string()))?;
        self.socket.send(WsMessage::Text(text.into())).await.map_err(|error| RpcError::internal(format!("send failed: {error}")))?;
        while let Some(frame) = self.socket.next().await {
            let frame = frame.map_err(|error| RpcError::internal(format!("connection error: {error}")))?;
            let text = match frame {
                WsMessage::Text(text) => text.to_string(),
                WsMessage::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                WsMessage::Close(_) => break,
                _ => continue,
            };
            let Ok(message) = serde_json::from_str::<Message>(&text) else { continue };
            match message {
                Message::Res { id: got, ok, result, error } if got == id => {
                    return if ok {
                        Ok(result.unwrap_or(Value::Null))
                    } else {
                        Err(error.unwrap_or_else(|| RpcError::internal("error response without details")))
                    };
                }
                Message::Item { id: got, data } if got == id => on_item(&data),
                Message::Req { id: request_id, method, .. } => {
                    // cx serves nothing; answer so the peer is not left waiting.
                    let reply = Message::Res { id: request_id, ok: false, result: None, error: Some(RpcError::unknown_method(&method)) };
                    if let Ok(text) = serde_json::to_string(&reply) {
                        let _ = self.socket.send(WsMessage::Text(text.into())).await;
                    }
                }
                _ => {}
            }
        }
        Err(RpcError::closed())
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_with(method, params, |_| {}).await
    }

    pub async fn close(mut self) {
        let _ = self.socket.close(None).await;
    }
}

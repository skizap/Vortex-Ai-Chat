//! Minimal Chromium DevTools Protocol (CDP) client over a loopback websocket.
//!
//! We deliberately implement only the handful of commands the browser tools
//! need (Target.createTarget/attachToTarget, Page.navigate,
//! Runtime.evaluate) instead of pulling a heavyweight crate. The browser
//! always runs locally (ws://127.0.0.1/...) with an isolated profile.

use crate::errors::ToolError;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

pub struct CdpConnection {
    write: Mutex<futures::stream::SplitSink<WsStream, WsMessage>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsMessage = tokio_tungstenite::tungstenite::Message;

impl CdpConnection {
    /// Connect to a `ws://` DevTools endpoint.
    pub async fn connect(ws_url: &str) -> Result<Self, ToolError> {
        let (stream, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| ToolError::Execution(format!("connecting to Chrome DevTools: {e}")))?;
        let (write, read) = stream.split();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut read = read;
            while let Some(Ok(msg)) = read.next().await {
                let Ok(text) = msg.into_text() else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
                    continue;
                };
                let sender = reader_pending.lock().await.remove(&id);
                if let Some(sender) = sender {
                    let _ = sender.send(v);
                }
            }
            // Connection closed: fail any still-pending calls.
            let mut map = reader_pending.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx
                    .send(serde_json::json!({"error": {"message": "devtools connection closed"}}));
            }
        });
        Ok(Self {
            write: Mutex::new(write),
            next_id: AtomicU64::new(1),
            pending,
        })
    }

    /// Send a CDP command and await its matching response.
    pub async fn command(
        &self,
        method: &str,
        params: serde_json::Value,
        session_id: Option<&str>,
        timeout_secs: u64,
    ) -> Result<serde_json::Value, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = serde_json::json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            msg["sessionId"] = serde_json::json!(sid);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let send_result = {
            let mut w = self.write.lock().await;
            w.send(WsMessage::Text(msg.to_string())).await
        };
        if let Err(e) = send_result {
            self.pending.lock().await.remove(&id);
            return Err(ToolError::Execution(format!("devtools send failed: {e}")));
        }
        let received = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await;
        match received {
            Ok(Ok(result)) => {
                if let Some(err) = result.get("error") {
                    return Err(ToolError::Execution(format!(
                        "devtools error: {}",
                        err.get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown")
                    )));
                }
                Ok(result
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null))
            }
            Ok(Err(_)) => Err(ToolError::Execution("devtools response dropped".into())),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(ToolError::Timeout)
            }
        }
    }
}

/// A page tab inside the browser, identified by a CDP sessionId.
#[derive(Clone)]
pub struct CdpPage {
    pub target_id: String,
    pub session_id: String,
}

impl CdpPage {
    pub async fn create(conn: &CdpConnection, url: &str) -> Result<Self, ToolError> {
        let result = conn
            .command(
                "Target.createTarget",
                serde_json::json!({ "url": url }),
                None,
                15,
            )
            .await?;
        let target_id = result
            .get("targetId")
            .and_then(|t| t.as_str())
            .ok_or_else(|| ToolError::Execution("no targetId in devtools response".into()))?
            .to_string();
        let result = conn
            .command(
                "Target.attachToTarget",
                serde_json::json!({ "targetId": target_id, "flatten": true }),
                None,
                10,
            )
            .await?;
        let session_id = result
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ToolError::Execution("no sessionId in devtools response".into()))?
            .to_string();
        Ok(Self {
            target_id,
            session_id,
        })
    }

    pub async fn navigate(&self, conn: &CdpConnection, url: &str) -> Result<(), ToolError> {
        conn.command(
            "Page.navigate",
            serde_json::json!({ "url": url }),
            Some(&self.session_id),
            30,
        )
        .await?;
        Ok(())
    }

    /// Evaluate a JavaScript expression; returns the JSON `value` of the result.
    pub async fn evaluate(
        &self,
        conn: &CdpConnection,
        expression: &str,
    ) -> Result<serde_json::Value, ToolError> {
        let result = conn
            .command(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": false
                }),
                Some(&self.session_id),
                30,
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            return Err(ToolError::Execution(format!(
                "page script error: {}",
                exception
                    .pointer("/exception/description")
                    .or_else(|| exception.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("unknown")
            )));
        }
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    pub async fn close(&self, conn: &CdpConnection) {
        let _ = conn
            .command(
                "Target.closeTarget",
                serde_json::json!({ "targetId": self.target_id }),
                None,
                5,
            )
            .await;
    }
}

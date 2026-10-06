//! Typed API client (gloo-net) + SSE event source, both fully in Rust/WASM.

use serde::de::DeserializeOwned;
use vortex_types::{
    ApprovalInfo, Capabilities, ChatRequestDto, ConversationSummary, ModelInfo, PreviewInfo,
    RunInfo, Settings, StoredMessage, ToolCallRecord,
};

const BASE: &str = "/api";

pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let resp = gloo_net::http::Request::get(&format!("{BASE}{path}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !(200..400).contains(&status) {
        return Err(parse_error(&text));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid response: {e}"))
}

pub async fn send_json<T: DeserializeOwned>(
    method: &str,
    path: &str,
    body: &str,
) -> Result<T, String> {
    let url = format!("{BASE}{path}");
    let req = match method {
        "POST" => gloo_net::http::Request::post(&url),
        "PUT" => gloo_net::http::Request::put(&url),
        "PATCH" => gloo_net::http::Request::patch(&url),
        "DELETE" => gloo_net::http::Request::delete(&url),
        _ => return Err(format!("unsupported method {method}")),
    };
    let resp = req
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !(200..400).contains(&status) {
        return Err(parse_error(&text));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid response: {e}"))
}

fn parse_error(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unexpected server error".to_string())
}

pub async fn health() -> Result<Capabilities, String> {
    get_json("/health").await
}

pub async fn settings() -> Result<Settings, String> {
    get_json("/settings").await
}

pub async fn save_settings(s: &Settings) -> Result<Settings, String> {
    let body = serde_json::to_string(s).map_err(|e| e.to_string())?;
    send_json("PUT", "/settings", &body).await
}

pub async fn models() -> Result<Vec<ModelInfo>, String> {
    let resp = get_json::<ModelsResp>("/models").await?;
    Ok(resp.models)
}

#[derive(serde::Deserialize)]
struct ModelsResp {
    models: Vec<ModelInfo>,
}

pub async fn conversations(q: &str) -> Result<Vec<ConversationSummary>, String> {
    if q.trim().is_empty() {
        get_json("/conversations").await
    } else {
        get_json(&format!("/conversations?q={}", urlencode(q))).await
    }
}

pub async fn create_conversation(
    title: &str,
    mode: vortex_types::Mode,
) -> Result<ConversationSummary, String> {
    let body = serde_json::json!({ "title": title, "mode": mode.as_str() }).to_string();
    send_json("POST", "/conversations", &body).await
}

pub async fn rename_conversation(id: &str, title: &str) -> Result<(), String> {
    let body = serde_json::json!({ "title": title }).to_string();
    send_json::<serde_json::Value>("PATCH", &format!("/conversations/{id}"), &body)
        .await
        .map(|_| ())
}

pub async fn delete_conversation(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("DELETE", &format!("/conversations/{id}"), "")
        .await
        .map(|_| ())
}

pub async fn messages(id: &str) -> Result<Vec<StoredMessage>, String> {
    get_json(&format!("/conversations/{id}/messages")).await
}

pub async fn send_chat(id: &str, dto: &ChatRequestDto) -> Result<RunId, String> {
    let body = serde_json::to_string(dto).map_err(|e| e.to_string())?;
    send_json("POST", &format!("/conversations/{id}/chat"), &body).await
}

#[derive(serde::Deserialize)]
pub struct RunId {
    pub run_id: String,
}

pub async fn stop_run(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("POST", &format!("/runs/{id}/stop"), "")
        .await
        .map(|_| ())
}

pub async fn retry_run(id: &str) -> Result<RunId, String> {
    send_json("POST", &format!("/runs/{id}/retry"), "").await
}

pub async fn run_state(id: &str) -> Result<RunSnapshot, String> {
    get_json(&format!("/runs/{id}/state")).await
}

#[derive(serde::Deserialize)]
pub struct RunSnapshot {
    pub run: Option<RunInfo>,
    pub agents: Vec<RunInfo>,
    pub tool_calls: Vec<ToolCallRecord>,
}

pub async fn approvals() -> Result<Vec<ApprovalInfo>, String> {
    get_json("/approvals").await
}

pub async fn decide_approval(id: &str, approve: bool) -> Result<(), String> {
    let body = serde_json::json!({ "approve": approve }).to_string();
    send_json::<serde_json::Value>("POST", &format!("/approvals/{id}/decide"), &body)
        .await
        .map(|_| ())
}

pub async fn previews() -> Result<Vec<PreviewInfo>, String> {
    get_json("/previews").await
}

pub async fn check_workspace(path: &str) -> Result<String, String> {
    let resp = get_json::<serde_json::Value>(&format!("/workspace/check?path={}", urlencode(path)))
        .await?;
    resp.get("root")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "unexpected response".to_string())
}

pub async fn runs_for_conversation(id: &str) -> Result<Vec<RunInfo>, String> {
    get_json(&format!("/runs?conversation_id={id}")).await
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

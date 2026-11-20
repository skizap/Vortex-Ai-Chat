//! Typed API client (gloo-net) + SSE event source, both fully in Rust/WASM.

use crate::diagnostics;
use serde::de::DeserializeOwned;
use vortex_types::{
    ApprovalInfo, AttachmentInfo, BrowserOperation, Capabilities, ChatRequestDto, CheckpointInfo,
    ConversationSummary, ModelInfo, PreviewInfo, RunInfo, Settings, StoredMessage, ToolCallRecord,
};

const BASE: &str = "/api";

/// Server-generated correlation id from a diagnostics-mode response. Only a
/// strict UUID survives (see `build_report`); everything else is dropped.
fn request_id_of(resp: &gloo_net::http::Response) -> Option<String> {
    resp.headers().get(diagnostics::REQUEST_ID_HEADER)
}

pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    get_json_optional(path, false)
        .await?
        .ok_or_else(|| "resource not found".into())
}

async fn get_json_optional<T: DeserializeOwned>(
    path: &str,
    allow_404: bool,
) -> Result<Option<T>, String> {
    let run_id = diagnostics::extract_run_id(path);
    let resp = authorize(gloo_net::http::Request::get(&format!("{BASE}{path}")))
        .send()
        .await
        .map_err(|e| {
            diagnostics::report_api_failure(
                BrowserOperation::ApiTransport,
                None,
                None,
                run_id.as_deref(),
            );
            e.to_string()
        })?;
    let status = resp.status();
    if allow_404 && status == 404 {
        return Ok(None);
    }
    let text = resp.text().await.map_err(|e| {
        diagnostics::report_api_failure(
            BrowserOperation::ApiTransport,
            None,
            request_id_of(&resp).as_deref(),
            run_id.as_deref(),
        );
        e.to_string()
    })?;
    // Only 2xx is success: a 3xx reaching this point means a redirect the
    // browser did not follow, and its body is not the expected JSON.
    if !(200..300).contains(&status) {
        diagnostics::report_api_failure(
            BrowserOperation::ApiStatus,
            Some(status),
            request_id_of(&resp).as_deref(),
            run_id.as_deref(),
        );
        return Err(parse_error(&text));
    }
    serde_json::from_str(&text).map(Some).map_err(|e| {
        diagnostics::report_api_failure(
            BrowserOperation::ApiDecode,
            Some(status),
            request_id_of(&resp).as_deref(),
            run_id.as_deref(),
        );
        format!("invalid response: {e}")
    })
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
    let run_id = diagnostics::extract_run_id(path);
    let resp = authorize(req)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| {
            diagnostics::report_api_failure(
                BrowserOperation::ApiTransport,
                None,
                None,
                run_id.as_deref(),
            );
            e.to_string()
        })?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| {
        diagnostics::report_api_failure(
            BrowserOperation::ApiTransport,
            None,
            request_id_of(&resp).as_deref(),
            run_id.as_deref(),
        );
        e.to_string()
    })?;
    if !(200..300).contains(&status) {
        diagnostics::report_api_failure(
            BrowserOperation::ApiStatus,
            Some(status),
            request_id_of(&resp).as_deref(),
            run_id.as_deref(),
        );
        return Err(parse_error(&text));
    }
    serde_json::from_str(&text).map_err(|e| {
        diagnostics::report_api_failure(
            BrowserOperation::ApiDecode,
            Some(status),
            request_id_of(&resp).as_deref(),
            run_id.as_deref(),
        );
        format!("invalid response: {e}")
    })
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

// ---- attachments ----------------------------------------------------------

/// Upload one file as multipart/form-data. The browser sets the Content-Type
/// (with boundary) automatically — it must NOT be set manually here.
pub async fn upload_attachment(
    conv_id: &str,
    file: &web_sys::File,
) -> Result<AttachmentInfo, String> {
    let form = web_sys::FormData::new().map_err(|e| format!("form: {e:?}"))?;
    form.append_with_blob_and_filename("file", file, &file.name())
        .map_err(|e| format!("form append: {e:?}"))?;
    let url = format!("{BASE}/conversations/{conv_id}/attachments");
    let resp = authorize(gloo_net::http::Request::post(&url))
        .body(form)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| {
            diagnostics::report_api_failure(BrowserOperation::ApiTransport, None, None, None);
            e.to_string()
        })?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !(200..300).contains(&status) {
        diagnostics::report_api_failure(
            BrowserOperation::ApiStatus,
            Some(status),
            request_id_of(&resp).as_deref(),
            None,
        );
        return Err(parse_error(&text));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid response: {e}"))
}

pub async fn attachment_meta(id: &str) -> Result<Option<AttachmentInfo>, String> {
    get_json_optional(&format!("/attachments/{id}/meta"), true).await
}

pub async fn attachment_list(conv_id: &str) -> Result<Vec<AttachmentInfo>, String> {
    get_json(&format!("/conversations/{conv_id}/attachments")).await
}

pub async fn attachment_delete(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("DELETE", &format!("/attachments/{id}"), "")
        .await
        .map(|_| ())
}

/// The LAN access token from the page URL (`?token=...`), if present. Media
/// URLs must carry it because `<img>`/`<video>` cannot set auth headers.
fn page_lan_token() -> Option<String> {
    #[cfg(not(target_arch = "wasm32"))]
    return None;
    #[cfg(target_arch = "wasm32")]
    {
        let search = web_sys::window()?.location().search().ok()?;
        web_sys::UrlSearchParams::new_with_str(&search)
            .ok()?
            .get("token")
            .filter(|token| !token.is_empty())
    }
}

pub(crate) fn authorize(req: gloo_net::http::RequestBuilder) -> gloo_net::http::RequestBuilder {
    match page_lan_token() {
        Some(t) => req.header("Authorization", &format!("Bearer {t}")),
        None => req,
    }
}

/// Append the LAN token to a relative API URL when the page carries one.
pub(crate) fn with_lan_token(url: &str) -> String {
    match page_lan_token() {
        Some(t) => format!(
            "{url}{}token={}",
            if url.contains('?') { '&' } else { '?' },
            urlencode(&t)
        ),
        None => url.to_string(),
    }
}

/// Serve URL for an attachment (token-aware).
pub fn attachment_url(id: &str) -> String {
    with_lan_token(&format!("/api/attachments/{id}"))
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

/// File checkpoints for a conversation, newest first, cursor-paginated
/// (`before` = smallest id of the previous page).
pub async fn checkpoints(
    conv_id: &str,
    before: Option<i64>,
    limit: u32,
) -> Result<Vec<CheckpointInfo>, String> {
    let mut query = format!("limit={limit}");
    if let Some(b) = before {
        query.push_str(&format!("&before={b}"));
    }
    get_json(&format!("/conversations/{conv_id}/checkpoints?{query}")).await
}

#[derive(serde::Deserialize)]
pub struct RestoreResult {
    pub ok: bool,
    pub path: String,
    /// "restored" (content written back) or "trashed" (the snapshot recorded
    /// that the file did not exist, so the current file was moved to trash).
    pub action: String,
}

pub async fn restore_checkpoint(id: i64) -> Result<RestoreResult, String> {
    send_json("POST", &format!("/checkpoints/{id}/restore"), "").await
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            // %20, not '+': a '+' is only a space in form-encoded bodies; in a
            // URL path/query a literal '+' decodes ambiguously.
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

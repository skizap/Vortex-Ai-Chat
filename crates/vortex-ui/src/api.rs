//! Typed API client (gloo-net) + SSE event source, both fully in Rust/WASM.

use crate::diagnostics;
use serde::de::DeserializeOwned;
use vortex_types::dto::ModelsResponse;
use vortex_types::{
    ApprovalInfo, AttachmentInfo, BrowserOperation, Capabilities, ChatRequestDto, CheckpointInfo,
    ConversationSummary, Job, JobBudgets, ModelInfo, Occurrence, OverlapPolicy, ParkedApproval,
    PreviewInfo, Routine, RoutineVersion, RunInfo, ScheduleSpec, Settings, StoredMessage,
    ToolCallRecord,
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

pub async fn test_search() -> Result<vortex_types::SearchTestResult, String> {
    send_json("POST", "/settings/search/test", "").await
}

/// Client-editable recommended-shortlist query for `GET /api/models`. All
/// parameters are optional; the server applies documented defaults, clamps
/// values and echoes the effective ones in the response. Session-only: the
/// UI never persists these.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CatalogQuery {
    pub max_input_usd_per_mtok: Option<f64>,
    pub max_output_usd_per_mtok: Option<f64>,
    pub recommended_limit: Option<u32>,
}

impl CatalogQuery {
    fn to_query_string(self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.max_input_usd_per_mtok {
            parts.push(format!("max_input_usd_per_mtok={v}"));
        }
        if let Some(v) = self.max_output_usd_per_mtok {
            parts.push(format!("max_output_usd_per_mtok={v}"));
        }
        if let Some(v) = self.recommended_limit {
            parts.push(format!("recommended_limit={v}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }
}

pub async fn models() -> Result<Vec<ModelInfo>, String> {
    let resp = model_catalog(CatalogQuery::default()).await?;
    Ok(resp.models)
}

pub(crate) async fn model_catalog(q: CatalogQuery) -> Result<ModelsResponse, String> {
    get_json(&format!("/models{}", q.to_query_string())).await
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

/// Rename a conversation. Resolves with the canonical (trimmed, normalized)
/// title as stored by the server — callers must use that value, never the
/// raw draft, so the row and the DB can never disagree.
pub async fn rename_conversation(id: &str, title: &str) -> Result<String, String> {
    let body = serde_json::json!({ "title": title }).to_string();
    let resp = send_json::<RenameResp>("PATCH", &format!("/conversations/{id}"), &body).await?;
    Ok(resp.title)
}

#[derive(serde::Deserialize)]
struct RenameResp {
    title: String,
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
    /// Present when the server auto-titled this conversation on its first
    /// accepted turn (placeholder origin only; manual titles never change).
    /// The chat UI may apply it to the sidebar row; the sidebar additionally
    /// refreshes the summary list on run start, so this is a fast path only.
    #[serde(default)]
    pub title: Option<String>,
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

// ---- routines & jobs -------------------------------------------------------

/// Wizard input for `POST /api/jobs`: the server creates the routine (v1),
/// the job, and its dedicated conversation atomically. `schedule: None`
/// creates a manual-only job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobWizard {
    pub name: String,
    pub instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<vortex_types::Mode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub schedule: Option<ScheduleSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budgets: Option<JobBudgets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap: Option<OverlapPolicy>,
}

/// Partial update for `PATCH /api/jobs/{id}`; omitted fields stay unchanged.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct JobPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap: Option<OverlapPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budgets: Option<JobBudgets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// `GET /api/routines/{id}` response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RoutineDetail {
    pub routine: Routine,
    pub versions: Vec<RoutineVersion>,
}

/// `GET /api/jobs/{id}` response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct JobDetail {
    pub job: Job,
    pub occurrences: Vec<Occurrence>,
}

/// Human decision for an occurrence flagged `needs_human_review`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    Continue,
    Dismiss,
}

#[derive(serde::Serialize)]
struct RoutineCreate<'a> {
    name: &'a str,
    instructions: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<vortex_types::Mode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct RoutineVersionCreate<'a> {
    instructions: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<vortex_types::Mode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
}

pub async fn routines() -> Result<Vec<Routine>, String> {
    get_json("/routines").await
}

/// Creates a routine as a human-authored draft (activation is a separate,
/// human-only endpoint).
pub async fn create_routine(
    name: &str,
    instructions: &str,
    description: Option<&str>,
    mode: Option<vortex_types::Mode>,
    model: Option<&str>,
) -> Result<Routine, String> {
    let body = serde_json::to_string(&RoutineCreate {
        name,
        instructions,
        description,
        mode,
        model,
    })
    .map_err(|e| e.to_string())?;
    send_json("POST", "/routines", &body).await
}

pub async fn routine_detail(id: &str) -> Result<RoutineDetail, String> {
    get_json(&format!("/routines/{id}")).await
}

/// Draft → active. The server answers 409 when the routine is not a draft;
/// the error text surfaces through the usual `parse_error` path.
pub async fn activate_routine(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("POST", &format!("/routines/{id}/activate"), "")
        .await
        .map(|_| ())
}

pub async fn archive_routine(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("POST", &format!("/routines/{id}/archive"), "")
        .await
        .map(|_| ())
}

pub async fn add_routine_version(
    id: &str,
    instructions: &str,
    mode: Option<vortex_types::Mode>,
    model: Option<&str>,
) -> Result<RoutineVersion, String> {
    let body = serde_json::to_string(&RoutineVersionCreate {
        instructions,
        mode,
        model,
    })
    .map_err(|e| e.to_string())?;
    send_json("POST", &format!("/routines/{id}/versions"), &body).await
}

pub async fn jobs() -> Result<Vec<Job>, String> {
    get_json("/jobs").await
}

pub async fn create_job(wizard: &JobWizard) -> Result<Job, String> {
    let body = serde_json::to_string(wizard).map_err(|e| e.to_string())?;
    send_json("POST", "/jobs", &body).await
}

pub async fn job_detail(id: &str) -> Result<JobDetail, String> {
    get_json(&format!("/jobs/{id}")).await
}

pub async fn patch_job(id: &str, patch: &JobPatch) -> Result<Job, String> {
    let body = serde_json::to_string(patch).map_err(|e| e.to_string())?;
    send_json("PATCH", &format!("/jobs/{id}"), &body).await
}

pub async fn activate_job(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("POST", &format!("/jobs/{id}/activate"), "")
        .await
        .map(|_| ())
}

pub async fn pause_job(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("POST", &format!("/jobs/{id}/pause"), "")
        .await
        .map(|_| ())
}

pub async fn resume_job(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("POST", &format!("/jobs/{id}/resume"), "")
        .await
        .map(|_| ())
}

/// Start a manual occurrence right now.
pub async fn run_job(id: &str) -> Result<Occurrence, String> {
    send_json("POST", &format!("/jobs/{id}/run"), "").await
}

/// Persist a steering note for the next occurrence/continuation; `None`
/// clears it (serialized as an explicit `null`).
pub async fn steer_job(id: &str, note: Option<&str>) -> Result<(), String> {
    let body = serde_json::json!({ "note": note }).to_string();
    send_json::<serde_json::Value>("POST", &format!("/jobs/{id}/steer"), &body)
        .await
        .map(|_| ())
}

pub async fn delete_job(id: &str) -> Result<(), String> {
    send_json::<serde_json::Value>("DELETE", &format!("/jobs/{id}"), "")
        .await
        .map(|_| ())
}

pub async fn job_occurrences(id: &str, limit: u32) -> Result<Vec<Occurrence>, String> {
    get_json(&format!("/jobs/{id}/occurrences?limit={limit}")).await
}

/// Parked approvals belonging to job occurrences (the Jobs-tab inbox).
pub async fn jobs_inbox() -> Result<Vec<ParkedApproval>, String> {
    get_json("/jobs/inbox").await
}

pub async fn resolve_review(occurrence_id: &str, action: ReviewAction) -> Result<(), String> {
    let body = serde_json::json!({ "action": action }).to_string();
    send_json::<serde_json::Value>(
        "POST",
        &format!("/occurrences/{occurrence_id}/resolve-review"),
        &body,
    )
    .await
    .map(|_| ())
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

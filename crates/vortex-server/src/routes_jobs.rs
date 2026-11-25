//! REST + SSE routes for routines, jobs, occurrences, and the approval inbox.
//! SA6 owns the implementation and route registration. Every route goes
//! through the same Host/Origin/LAN guard as the rest of the API; activation
//! is a distinct human endpoint with no model-reachable path.
//!
//! Semantics worth pinning:
//! - Routines created via the API always land as `draft`; only the human
//!   activate endpoint flips them to `active`. The job wizard is an explicit
//!   human creation, so it activates both the routine and the job it creates.
//! - Jobs pin an exact routine version; editing a routine never changes what
//!   a pinned job runs.
//! - Manual runs insert a `scheduled` occurrence at the current instant; the
//!   supervisor's due-query admits it on its next tick (the UNIQUE
//!   `(job_id, scheduled_for)` constraint is the double-fire guard).
//! - The SSE channel mirrors `routes_runs::run_events` (seq-stamped replay,
//!   subscribe-before-replay, per-connection dedup, `replay_gap`, keep-alive)
//!   over the supervisor's job-event log. REST mutations do not emit events
//!   (the supervisor owns the log); clients refetch after mutations.

use crate::state::{err_json, AppState};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use vortex_agents::clock;
use vortex_agents::supervisor::Supervisor;
use vortex_types::{
    Job, JobBudgets, JobEvent, JobStatus, Mode, NewJob, NewRoutine, OccurrenceStatus,
    OverlapPolicy, RoutineStatus, ScheduleSpec,
};

/// Default mode for routine authoring bodies (frozen contract: "agent").
fn default_mode() -> Mode {
    Mode::Agent
}

#[derive(Deserialize)]
struct CreateRoutineBody {
    name: String,
    instructions: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_mode")]
    mode: Mode,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct AddVersionBody {
    instructions: String,
    #[serde(default = "default_mode")]
    mode: Mode,
    #[serde(default)]
    model: Option<String>,
}

/// Wizard shape: explicit human creation of a routine (v1, activated) plus a
/// job pinned to it (activated). `schedule` is required but may be `null`
/// (manual-only job).
#[derive(Deserialize)]
struct WizardJobBody {
    name: String,
    instructions: String,
    #[serde(default = "default_mode")]
    mode: Mode,
    #[serde(default)]
    model: Option<String>,
    schedule: Option<ScheduleSpec>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    budgets: JobBudgets,
    #[serde(default)]
    overlap: OverlapPolicy,
}

/// Pin shape: a job pinned to an existing routine version (which must exist;
/// its status is left untouched — draft routines may be pinned).
#[derive(Deserialize)]
struct PinJobBody {
    name: String,
    routine_id: String,
    routine_version: u32,
    schedule: Option<ScheduleSpec>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    budgets: JobBudgets,
    #[serde(default)]
    overlap: OverlapPolicy,
}

#[derive(Deserialize)]
struct SteerBody {
    note: Option<String>,
}

#[derive(Deserialize)]
struct ResolveReviewBody {
    action: String,
}

/// Decode a JSON body into a typed request, mapping serde failures to
/// actionable 400s in the uniform error shape.
fn decode<T: serde::de::DeserializeOwned>(
    body: serde_json::Value,
) -> Result<T, Box<axum::response::Response>> {
    serde_json::from_value(body).map_err(|e| {
        Box::new(err_json(
            StatusCode::BAD_REQUEST,
            format!("invalid request body: {e}"),
        ))
    })
}

/// Trimmed non-empty field check with an actionable 400.
fn required(field: &str, value: &str) -> Result<String, Box<axum::response::Response>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Box::new(err_json(
            StatusCode::BAD_REQUEST,
            format!("'{field}' is required and must not be empty"),
        )));
    }
    Ok(trimmed.to_string())
}

/// Normalize optional free-text fields: empty/whitespace-only means `None`.
fn optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

/// Validate a schedule through the clock module (5-field cron + local-only
/// timezone); the clock's message is already actionable.
fn validate_schedule(schedule: Option<&ScheduleSpec>) -> Result<(), Box<axum::response::Response>> {
    if let Some(spec) = schedule {
        clock::validate_schedule(spec)
            .map_err(|e| Box::new(err_json(StatusCode::BAD_REQUEST, e)))?;
    }
    Ok(())
}

fn internal(op: &'static str, e: vortex_core::error::CoreError) -> axum::response::Response {
    crate::diagnostics::db_failure(op, None);
    err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{op}: {e}"))
}

// ---------------------------------------------------------------------------
// Routines
// ---------------------------------------------------------------------------

/// GET /api/routines — all routines, most recently updated first.
pub async fn list_routines(State(state): State<Arc<AppState>>) -> axum::response::Response {
    match state.db.list_routines().await {
        Ok(routines) => Json(routines).into_response(),
        Err(_) => {
            crate::diagnostics::db_failure("list_routines", None);
            Json(Vec::<vortex_types::Routine>::new()).into_response()
        }
    }
}

/// POST /api/routines — create a routine (always draft, version 1 written
/// atomically). Mode defaults to "agent" and is validated by serde.
pub async fn create_routine(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let body: CreateRoutineBody = match decode(body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let name = match required("name", &body.name) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let instructions = match required("instructions", &body.instructions) {
        Ok(i) => i,
        Err(resp) => return *resp,
    };
    let new = NewRoutine {
        name,
        description: body.description.trim().to_string(),
        instructions,
        mode: body.mode,
        model: optional_text(body.model),
    };
    match state.db.create_routine(new).await {
        Ok(routine) => Json(routine).into_response(),
        Err(e) => internal("create_routine", e),
    }
}

/// GET /api/routines/{id} — `{routine, versions}` (versions newest first).
pub async fn get_routine(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let routine = match state.db.get_routine(id.clone()).await {
        Ok(Some(r)) => r,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "routine not found"),
        Err(e) => return internal("get_routine", e),
    };
    let versions = match state.db.list_routine_versions(id.clone()).await {
        Ok(v) => v,
        Err(_) => {
            crate::diagnostics::db_failure("list_routine_versions", Some(&id));
            Vec::new()
        }
    };
    Json(serde_json::json!({ "routine": routine, "versions": versions })).into_response()
}

/// POST /api/routines/{id}/activate — human-only draft → active. 404 when
/// missing; 409 when not a draft. No model-reachable path activates routines.
pub async fn activate_routine(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match state.db.activate_routine(id.clone()).await {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => match state.db.get_routine(id).await {
            Ok(Some(r)) => err_json(
                StatusCode::CONFLICT,
                format!(
                    "routine is not a draft (status: {}); only draft routines can be activated",
                    r.status.as_str()
                ),
            ),
            Ok(None) => err_json(StatusCode::NOT_FOUND, "routine not found"),
            Err(e) => internal("get_routine", e),
        },
        Err(e) => internal("activate_routine", e),
    }
}

/// POST /api/routines/{id}/archive — draft|active → archived.
pub async fn archive_routine(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match state.db.archive_routine(id.clone()).await {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => match state.db.get_routine(id).await {
            Ok(Some(_)) => err_json(StatusCode::CONFLICT, "routine is already archived"),
            Ok(None) => err_json(StatusCode::NOT_FOUND, "routine not found"),
            Err(e) => internal("get_routine", e),
        },
        Err(e) => internal("archive_routine", e),
    }
}

/// POST /api/routines/{id}/versions — append an immutable version and bump
/// `current_version`; the routine's status never changes and pinned jobs
/// keep running exactly what they pinned.
pub async fn add_routine_version(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let body: AddVersionBody = match decode(body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let instructions = match required("instructions", &body.instructions) {
        Ok(i) => i,
        Err(resp) => return *resp,
    };
    match state.db.get_routine(id.clone()).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "routine not found"),
        Err(e) => return internal("get_routine", e),
    }
    match state
        .db
        .add_routine_version(id, instructions, body.mode, optional_text(body.model))
        .await
    {
        Ok(version) => Json(version).into_response(),
        Err(vortex_core::error::CoreError::Config(msg)) => err_json(StatusCode::NOT_FOUND, msg),
        Err(e) => internal("add_routine_version", e),
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// GET /api/jobs — all jobs, oldest first.
pub async fn list_jobs(State(state): State<Arc<AppState>>) -> axum::response::Response {
    match state.db.list_jobs().await {
        Ok(jobs) => Json(jobs).into_response(),
        Err(_) => {
            crate::diagnostics::db_failure("list_jobs", None);
            Json(Vec::<Job>::new()).into_response()
        }
    }
}

/// POST /api/jobs — two body shapes:
/// (a) wizard `{name, instructions, mode?, model?, schedule, workspace?,
///     budgets?, overlap?}` → routine v1 ACTIVE + job ACTIVE (explicit human
///     creation);
/// (b) pin `{name, routine_id, routine_version, schedule, workspace?,
///     budgets?, overlap?}` → job ACTIVE pinned to an existing version.
/// `schedule` may be `null` (manual-only). Invalid cron/timezone → 400 with
/// the clock's actionable message.
pub async fn create_job(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    if body.get("routine_id").is_some() {
        create_pinned_job(state, body).await
    } else if body.get("instructions").is_some() {
        create_wizard_job(state, body).await
    } else {
        err_json(
            StatusCode::BAD_REQUEST,
            "expected a wizard body {name, instructions, schedule, ...} or a pin body \
             {name, routine_id, routine_version, schedule, ...}",
        )
    }
}

async fn create_wizard_job(
    state: Arc<AppState>,
    body: serde_json::Value,
) -> axum::response::Response {
    // serde defaults a missing Option to None; the contract requires the
    // schedule key explicitly (object or null), so check presence first.
    if body.get("schedule").is_none() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "missing field 'schedule': pass a schedule object {\"cron\": ...} or null for a \
             manual-only job",
        );
    }
    let body: WizardJobBody = match decode(body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let name = match required("name", &body.name) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let instructions = match required("instructions", &body.instructions) {
        Ok(i) => i,
        Err(resp) => return *resp,
    };
    if let Err(resp) = validate_schedule(body.schedule.as_ref()) {
        return *resp;
    }
    let routine = match state
        .db
        .create_routine(NewRoutine {
            name: name.clone(),
            description: String::new(),
            instructions,
            mode: body.mode,
            model: optional_text(body.model),
        })
        .await
    {
        Ok(r) => r,
        Err(e) => return internal("create_routine", e),
    };
    // The wizard is an explicit human creation: activate the routine it just
    // wrote (the store lands every routine as draft for agent-authored ones).
    match state.db.activate_routine(routine.id.clone()).await {
        Ok(true) => {}
        Ok(false) => {
            return internal(
                "activate_routine",
                vortex_core::error::CoreError::Config(
                    "wizard routine could not be activated".into(),
                ),
            )
        }
        Err(e) => return internal("activate_routine", e),
    }
    let new = NewJob {
        name,
        routine_id: routine.id.clone(),
        routine_version: 1,
        schedule: body.schedule,
        overlap: body.overlap,
        budgets: body.budgets,
        workspace: optional_text(body.workspace),
    };
    finish_job_creation(state, new).await
}

async fn create_pinned_job(
    state: Arc<AppState>,
    body: serde_json::Value,
) -> axum::response::Response {
    if body.get("schedule").is_none() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "missing field 'schedule': pass a schedule object {\"cron\": ...} or null for a \
             manual-only job",
        );
    }
    let body: PinJobBody = match decode(body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let name = match required("name", &body.name) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    if let Err(resp) = validate_schedule(body.schedule.as_ref()) {
        return *resp;
    }
    match state.db.get_routine(body.routine_id.clone()).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return err_json(
                StatusCode::NOT_FOUND,
                format!("routine '{}' not found", body.routine_id),
            )
        }
        Err(e) => return internal("get_routine", e),
    }
    match state
        .db
        .get_routine_version(body.routine_id.clone(), body.routine_version)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            return err_json(
                StatusCode::NOT_FOUND,
                format!(
                    "routine '{}' version {} not found",
                    body.routine_id, body.routine_version
                ),
            )
        }
        Err(e) => return internal("get_routine_version", e),
    }
    let new = NewJob {
        name,
        routine_id: body.routine_id,
        routine_version: body.routine_version,
        schedule: body.schedule,
        overlap: body.overlap,
        budgets: body.budgets,
        workspace: optional_text(body.workspace),
    };
    finish_job_creation(state, new).await
}

/// Shared tail of both creation shapes: the store creates the job (draft) and
/// its dedicated conversation atomically; explicit human creation activates
/// the job, then the fresh row is returned.
async fn finish_job_creation(state: Arc<AppState>, new: NewJob) -> axum::response::Response {
    let job = match state.db.create_job(new).await {
        Ok(j) => j,
        Err(vortex_core::error::CoreError::Config(msg)) => {
            return err_json(StatusCode::NOT_FOUND, msg)
        }
        Err(e) => return internal("create_job", e),
    };
    match state
        .db
        .set_job_status(job.id.clone(), JobStatus::Active)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return internal(
                "set_job_status",
                vortex_core::error::CoreError::Config("job vanished after creation".into()),
            )
        }
        Err(e) => return internal("set_job_status", e),
    }
    match state.db.get_job(job.id).await {
        Ok(Some(job)) => Json(job).into_response(),
        Ok(None) => err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => internal("get_job", e),
    }
}

/// GET /api/jobs/{id} — `{job, occurrences}` (latest 50, newest first).
pub async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let job = match state.db.get_job(id.clone()).await {
        Ok(Some(j)) => j,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => return internal("get_job", e),
    };
    let occurrences = match state.db.list_occurrences(id.clone(), 50).await {
        Ok(o) => o,
        Err(_) => {
            crate::diagnostics::db_failure("list_occurrences", Some(&id));
            Vec::new()
        }
    };
    Json(serde_json::json!({ "job": job, "occurrences": occurrences })).into_response()
}

/// PATCH /api/jobs/{id} — partial update of `{name?, schedule?, overlap?,
/// budgets?, workspace?}`. Absent fields keep their current values; an
/// explicit `schedule: null` clears the schedule (manual-only). Budgets merge
/// field-by-field over the current caps. `next_run_at` is recomputed by the
/// supervisor on its next tick — this handler only persists.
pub async fn patch_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let job = match state.db.get_job(id.clone()).await {
        Ok(Some(j)) => j,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => return internal("get_job", e),
    };

    let mut name = job.name.clone();
    if let Some(v) = body.get("name") {
        let Some(s) = v.as_str() else {
            return err_json(StatusCode::BAD_REQUEST, "'name' must be a string");
        };
        name = match required("name", s) {
            Ok(n) => n,
            Err(resp) => return *resp,
        };
    }

    let mut schedule = job.schedule.clone();
    if let Some(v) = body.get("schedule") {
        if v.is_null() {
            schedule = None;
        } else {
            let spec: ScheduleSpec = match decode(v.clone()) {
                Ok(s) => s,
                Err(resp) => return *resp,
            };
            if let Err(resp) = validate_schedule(Some(&spec)) {
                return *resp;
            }
            schedule = Some(spec);
        }
    }

    let mut overlap = job.overlap;
    if let Some(v) = body.get("overlap") {
        overlap = match decode(v.clone()) {
            Ok(o) => o,
            Err(resp) => return *resp,
        };
    }

    // Merge provided budget keys over the current caps so a partial budgets
    // object never silently resets the other guardrails to their defaults.
    let mut budgets = job.budgets;
    if let Some(v) = body.get("budgets") {
        let obj = match v.as_object() {
            Some(o) => o,
            None => return err_json(StatusCode::BAD_REQUEST, "'budgets' must be an object"),
        };
        let mut merged = match serde_json::to_value(budgets) {
            Ok(m) => m,
            Err(e) => {
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("serializing budgets: {e}"),
                )
            }
        };
        for (k, val) in obj {
            merged[k] = val.clone();
        }
        budgets = match decode(merged) {
            Ok(b) => b,
            Err(resp) => return *resp,
        };
    }

    let mut workspace = job.workspace.clone();
    if let Some(v) = body.get("workspace") {
        if v.is_null() {
            workspace = None;
        } else {
            let s = match v.as_str() {
                Some(s) => s,
                None => {
                    return err_json(
                        StatusCode::BAD_REQUEST,
                        "'workspace' must be a string or null",
                    )
                }
            };
            workspace = optional_text(Some(s.to_string()));
        }
    }

    match state
        .db
        .update_job_config(id.clone(), name, schedule, overlap, budgets, workspace)
        .await
    {
        Ok(true) => {}
        Ok(false) => return err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => return internal("update_job_config", e),
    }
    match state.db.get_job(id).await {
        Ok(Some(job)) => Json(job).into_response(),
        Ok(None) => err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => internal("get_job", e),
    }
}

/// POST /api/jobs/{id}/activate — human-only draft → active. When the pinned
/// routine is still a draft it is activated FIRST (a job must never run
/// against an unactivated routine). 409 when the job is not a draft.
pub async fn activate_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let job = match state.db.get_job(id.clone()).await {
        Ok(Some(j)) => j,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => return internal("get_job", e),
    };
    if job.status != JobStatus::Draft {
        return err_json(
            StatusCode::CONFLICT,
            format!(
                "job is not a draft (status: {}); only draft jobs can be activated",
                job.status.as_str()
            ),
        );
    }
    if let Ok(Some(routine)) = state.db.get_routine(job.routine_id.clone()).await {
        if routine.status == RoutineStatus::Draft {
            if let Err(e) = state.db.activate_routine(job.routine_id.clone()).await {
                return internal("activate_routine", e);
            }
        }
    }
    match state.db.set_job_status(id, JobStatus::Active).await {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => internal("set_job_status", e),
    }
}

/// POST /api/jobs/{id}/pause — active → paused; 409 otherwise.
pub async fn pause_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    transition_job(state, id, JobStatus::Active, JobStatus::Paused, "paused").await
}

/// POST /api/jobs/{id}/resume — paused|circuit_broken → active, resetting the
/// consecutive-failure counter (a human resume is a fresh start); 409
/// otherwise.
pub async fn resume_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let job = match state.db.get_job(id.clone()).await {
        Ok(Some(j)) => j,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => return internal("get_job", e),
    };
    if !matches!(job.status, JobStatus::Paused | JobStatus::CircuitBroken) {
        return err_json(
            StatusCode::CONFLICT,
            format!(
                "job cannot be resumed from status '{}'; only paused or circuit_broken jobs resume",
                job.status.as_str()
            ),
        );
    }
    if let Err(e) = state.db.set_job_status(id.clone(), JobStatus::Active).await {
        return internal("set_job_status", e);
    }
    if let Err(e) = state.db.reset_job_failures(id).await {
        return internal("reset_job_failures", e);
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

/// Shared active → target transition with a 409 on any other source status.
async fn transition_job(
    state: Arc<AppState>,
    id: String,
    from: JobStatus,
    to: JobStatus,
    verb: &str,
) -> axum::response::Response {
    let job = match state.db.get_job(id.clone()).await {
        Ok(Some(j)) => j,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => return internal("get_job", e),
    };
    if job.status != from {
        return err_json(
            StatusCode::CONFLICT,
            format!(
                "job cannot be {verb} from status '{}'; expected '{}'",
                job.status.as_str(),
                from.as_str()
            ),
        );
    }
    match state.db.set_job_status(id, to).await {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => internal("set_job_status", e),
    }
}

/// POST /api/jobs/{id}/run — manual occurrence at the current instant (UTC
/// RFC 3339 millis). The supervisor's due-query admits it within ~1 s. The
/// job must be active; a UNIQUE conflict means this instant already fired.
pub async fn run_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let job = match state.db.get_job(id.clone()).await {
        Ok(Some(j)) => j,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => return internal("get_job", e),
    };
    if job.status != JobStatus::Active {
        return err_json(
            StatusCode::CONFLICT,
            format!(
                "job must be active to run manually (status: {})",
                job.status.as_str()
            ),
        );
    }
    match state.db.insert_occurrence(id, vortex_core::db::now()).await {
        Ok(Some(occ)) => Json(occ).into_response(),
        Ok(None) => err_json(StatusCode::CONFLICT, "already fired for this instant"),
        Err(e) => internal("insert_occurrence", e),
    }
}

/// POST /api/jobs/{id}/steer `{note: string|null}` — persist a one-shot
/// steering note applied to the next occurrence/continuation (consumed once
/// by the supervisor); `null` clears it.
pub async fn steer_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    // The note key is required (string or null); serde would silently default
    // a missing Option to None, which would clear the note by accident.
    if body.get("note").is_none() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "missing field 'note': pass a steering note string, or null to clear it",
        );
    }
    let body: SteerBody = match decode(body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    match state
        .db
        .set_job_steer_note(id, optional_text(body.note))
        .await
    {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => internal("set_job_steer_note", e),
    }
}

/// DELETE /api/jobs/{id} — delete the job, its occurrences, and its dedicated
/// conversation (store cascade).
pub async fn delete_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match state.db.delete_job(id).await {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => internal("delete_job", e),
    }
}

/// GET /api/jobs/{id}/occurrences?limit=N — newest first; default 50, cap 200.
pub async fn list_job_occurrences(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    match state.db.get_job(id.clone()).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => return internal("get_job", e),
    }
    let limit = match q.get("limit") {
        None => 50,
        Some(raw) => match raw.trim().parse::<u32>() {
            Ok(0) => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "'limit' must be a positive integer (1-200)",
                )
            }
            Ok(n) => n.min(200),
            Err(_) => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "'limit' must be a positive integer (1-200)",
                )
            }
        },
    };
    match state.db.list_occurrences(id, limit).await {
        Ok(occurrences) => Json(occurrences).into_response(),
        Err(e) => internal("list_occurrences", e),
    }
}

// ---------------------------------------------------------------------------
// Inbox + human review resolution
// ---------------------------------------------------------------------------

/// GET /api/jobs/inbox — pending approvals belonging to parked
/// (waiting_approval) job occurrences, with job context for the UI.
pub async fn job_inbox(State(state): State<Arc<AppState>>) -> axum::response::Response {
    match state.db.list_parked_job_approvals().await {
        Ok(parked) => Json(parked).into_response(),
        Err(e) => internal("list_parked_job_approvals", e),
    }
}

/// POST /api/occurrences/{id}/resolve-review `{action: "continue"|"dismiss"}`.
/// Only occurrences flagged `needs_human_review` can be resolved (409
/// otherwise). `continue` clears the reason (keeping the Interrupted status)
/// and spawns the supervisor's catch-up so the continuation decision tree
/// re-admits the occurrence promptly. `dismiss` fails the occurrence with a
/// human-readable reason and NEVER increments the circuit breaker. Approval
/// decisions themselves reuse POST /api/approvals/{id}/decide.
pub async fn resolve_review(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let body: ResolveReviewBody = match decode(body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let occ = match state.db.get_occurrence(id.clone()).await {
        Ok(Some(o)) => o,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "occurrence not found"),
        Err(e) => return internal("get_occurrence", e),
    };
    if !occ.needs_human_review {
        return err_json(
            StatusCode::CONFLICT,
            "occurrence is not flagged for human review",
        );
    }
    match body.action.as_str() {
        "continue" => {
            // SA1's update_occurrence writes `reason` unconditionally, so
            // passing None clears it to NULL (Some("") would be wrong); the
            // Interrupted status is kept and run_id survives via COALESCE.
            // A reason-less Interrupted row is exactly what the supervisor's
            // catch-up re-processes through the continuation decision tree.
            match state
                .db
                .update_occurrence(id.clone(), OccurrenceStatus::Interrupted, None, None)
                .await
            {
                Ok(true) => {}
                Ok(false) => return err_json(StatusCode::NOT_FOUND, "occurrence not found"),
                Err(e) => return internal("update_occurrence", e),
            }
            if let Err(resp) = clear_review_flag(&state, &id).await {
                return resp;
            }
            let supervisor = state.supervisor.clone();
            tokio::spawn(async move { supervisor.catch_up().await });
            Json(serde_json::json!({"ok": true})).into_response()
        }
        "dismiss" => {
            match state
                .db
                .update_occurrence(
                    id.clone(),
                    OccurrenceStatus::Failed,
                    None,
                    Some("dismissed by human".into()),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => return err_json(StatusCode::NOT_FOUND, "occurrence not found"),
                Err(e) => return internal("update_occurrence", e),
            }
            if let Err(resp) = clear_review_flag(&state, &id).await {
                return resp;
            }
            // No record_job_failure: a human dismissal never trips the breaker.
            Json(serde_json::json!({"ok": true})).into_response()
        }
        other => err_json(
            StatusCode::BAD_REQUEST,
            format!("unknown action '{other}': expected \"continue\" or \"dismiss\""),
        ),
    }
}

/// SA1's `update_occurrence` does not touch `needs_human_review`; a resolved
/// review must stop blocking the UI (and re-resolution), so the flag is
/// cleared directly. Status/reason transitions still go through the store.
async fn clear_review_flag(state: &AppState, occ_id: &str) -> Result<(), axum::response::Response> {
    let id = occ_id.to_string();
    let cleared = state
        .db
        .with_conn(move |c| {
            Ok(c.execute(
                "UPDATE job_occurrences SET needs_human_review = 0, updated_at = ?2 \
                 WHERE id = ?1",
                [id, vortex_core::db::now()],
            )? > 0)
        })
        .await;
    match cleared {
        Ok(_) => Ok(()),
        Err(e) => Err(internal("clear_review_flag", e)),
    }
}

// ---------------------------------------------------------------------------
// SSE: job events (mirrors routes_runs::run_events)
// ---------------------------------------------------------------------------

/// Replay cursor: `Last-Event-ID` header and/or `?since=<seq>`; the newer
/// (larger) cursor wins; neither present means replay from 0.
const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");

fn replay_cursor(headers: &HeaderMap, query: &HashMap<String, String>) -> u64 {
    let from_header = headers
        .get(&LAST_EVENT_ID)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let from_query = query.get("since").and_then(|s| s.parse::<u64>().ok());
    from_header.max(from_query).unwrap_or(0)
}

/// GET /api/jobs/events — SSE of `JobEvent` (no run-id path; one channel for
/// all jobs). Events carry seq-stamped `id:` fields; on (re-)subscription the
/// retained log is replayed from the client's cursor, then the live broadcast
/// is tailed with per-connection dedup. A cursor below the retained window
/// emits a named `replay_gap` event first (data = oldest retained seq).
pub async fn job_events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let since = replay_cursor(&headers, &q);
    // Subscribe *before* replaying so no event can slip between the log
    // snapshot and the live tail (duplicates are filtered per connection).
    let rx = state.supervisor.subscribe();
    sse_response(live_job_event_stream(&state.supervisor, since, rx))
}

/// Replay everything the log retains above `since`, then tail the broadcast.
/// `rx` must have been created before the replay call so the overlap can only
/// duplicate; `last_sent` then delivers every event exactly once.
fn live_job_event_stream(
    supervisor: &Supervisor,
    since: u64,
    rx: broadcast::Receiver<(u64, JobEvent)>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let (replayed, oldest_retained, newest) = supervisor.replay_from(since);
    let mut head: Vec<Result<Event, Infallible>> = Vec::new();
    // A cursor above the newest seq is a future cursor: clamp it so the live
    // tail does not swallow new events until the sequence catches up.
    let mut last_sent = newest.map_or(0, |n| since.min(n));
    if let Some(oldest) = oldest_retained {
        if since.saturating_add(1) < oldest {
            head.push(Ok(Event::default()
                .event("replay_gap")
                .data(oldest.to_string())));
        }
    }
    for (seq, ev) in replayed {
        if seq > last_sent {
            last_sent = seq;
            head.push(Ok(Event::default()
                .id(seq.to_string())
                .json_data(ev)
                .expect("job event serializes")));
        }
    }
    let live = BroadcastStream::new(rx).filter_map(move |item| match item {
        Ok((seq, ev)) => {
            if seq <= last_sent {
                return None; // replay/live overlap: already delivered
            }
            last_sent = seq;
            Some(Ok(Event::default()
                .id(seq.to_string())
                .json_data(ev)
                .expect("job event serializes")))
        }
        Err(_lagged) => Some(Ok(Event::default().event("replay_gap").data("lagged"))),
    });
    futures::stream::iter(head).chain(live)
}

fn sse_response<S>(stream: S) -> axum::response::Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

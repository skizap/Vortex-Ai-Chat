//! Jobs tab: scheduled autonomous work — job list + create wizard, job detail
//! with occurrence history, the parked-approval inbox, and the routine
//! library. Live updates arrive over `/api/jobs/events` with the same
//! replay/reconnect semantics as the run SSE in `sse.rs`.
//!
//! Honesty: schedules fire only while Vortex is running; the banner says so,
//! and manual-only jobs are labeled "manual only" rather than given a fake
//! next-run time.

use crate::api::{self, JobDetail, JobPatch, JobWizard, ReviewAction};
use crate::state::{AppCtx, Tab};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashMap;
use vortex_types::{
    Job, JobBudgets, JobEvent, JobStatus, MissedPolicy, Occurrence, OccurrenceStatus,
    OverlapPolicy, ParkedApproval, Routine, RoutineStatus, RoutineVersion, RunStatus, ScheduleSpec,
};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// Schedule presets for the wizard: three inputs (name, instructions, preset)
/// are enough to create a scheduled job. `None` = manual-only.
const PRESETS: [(&str, Option<&str>); 4] = [
    ("daily", Some("0 9 * * *")),
    ("hourly", Some("0 * * * *")),
    ("weekdays", Some("0 9 * * 1-5")),
    ("manual", None),
];

/// RFC 3339 (stored UTC) → compact local-time label; falls back to the raw
/// string if a timestamp ever fails to parse.
fn local_time(stamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%b %d, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| stamp.to_string())
}

fn job_badge_class(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Draft => "badge jobs-status-draft",
        JobStatus::Active => "badge jobs-status-active",
        JobStatus::Paused => "badge jobs-status-paused",
        JobStatus::CircuitBroken => "badge jobs-status-broken",
    }
}

fn routine_badge_class(status: RoutineStatus) -> &'static str {
    match status {
        RoutineStatus::Draft => "badge jobs-status-draft",
        RoutineStatus::Active => "badge jobs-status-active",
        RoutineStatus::Archived => "badge jobs-status-draft",
    }
}

fn occ_badge_class(status: OccurrenceStatus) -> &'static str {
    match status {
        OccurrenceStatus::Succeeded => "badge jobs-occ-ok",
        OccurrenceStatus::Failed => "badge jobs-occ-bad",
        OccurrenceStatus::Running | OccurrenceStatus::Scheduled => "badge jobs-occ-live",
        OccurrenceStatus::WaitingApproval => "badge jobs-occ-wait",
        OccurrenceStatus::Interrupted | OccurrenceStatus::Skipped => "badge jobs-status-draft",
    }
}

fn next_run_label(job: &Job) -> String {
    if job.schedule.is_none() {
        return "manual only".into();
    }
    match &job.next_run_at {
        Some(t) => format!("next {}", local_time(t)),
        None => "next tick pending".into(),
    }
}

fn schedule_label(job: &Job) -> String {
    match &job.schedule {
        None => "manual only".into(),
        Some(s) => format!(
            "{} · missed ticks: {}",
            s.cron,
            match s.missed_policy {
                MissedPolicy::RunOnceIfMissed => "run once",
                MissedPolicy::Skip => "skip",
            }
        ),
    }
}

fn upsert_job(ctx: AppCtx, job: Job) {
    ctx.jobs.update(|list| {
        if let Some(existing) = list.iter_mut().find(|j| j.id == job.id) {
            *existing = job;
        } else {
            list.push(job);
        }
    });
}

fn upsert_routine(ctx: AppCtx, routine: Routine) {
    ctx.routines.update(|list| {
        if let Some(existing) = list.iter_mut().find(|r| r.id == routine.id) {
            *existing = routine;
        } else {
            list.push(routine);
        }
    });
}

fn upsert_occurrence(list: &mut Vec<Occurrence>, occ: Occurrence) {
    if let Some(existing) = list.iter_mut().find(|o| o.id == occ.id) {
        *existing = occ;
    } else {
        list.push(occ);
        // UTC RFC 3339 millis sort lexicographically = chronologically.
        list.sort_by(|a, b| b.scheduled_for.cmp(&a.scheduled_for));
    }
}

/// Keep the newest occurrence per job for the list's "last outcome" column.
fn remember_occurrence(last_occ: RwSignal<HashMap<String, Occurrence>>, occ: Occurrence) {
    last_occ.update(|map| {
        let newer = match map.get(&occ.job_id) {
            Some(existing) => occ.updated_at >= existing.updated_at,
            None => true,
        };
        if newer {
            map.insert(occ.job_id.clone(), occ);
        }
    });
}

// ---- revision-guarded list refreshes ---------------------------------------
// Every fetch captures `jobs_revision` before awaiting and discards its
// result if the revision bumped meanwhile (tab re-entry / cleanup), mirroring
// the `replace_settings` staleness pattern in state.rs.

fn spawn_refresh_jobs(ctx: AppCtx, last_occ: RwSignal<HashMap<String, Occurrence>>) {
    let revision = ctx.jobs_revision.get_untracked();
    spawn_local(async move {
        match api::jobs().await {
            Ok(list) => {
                if ctx.jobs_revision.get_untracked() != revision {
                    return;
                }
                ctx.jobs.set(list.clone());
                ctx.jobs_loaded.set(true);
                for job in list {
                    spawn_local(async move {
                        if let Ok(occs) = api::job_occurrences(&job.id, 1).await {
                            if ctx.jobs_revision.get_untracked() != revision {
                                return;
                            }
                            if let Some(occ) = occs.into_iter().next() {
                                remember_occurrence(last_occ, occ);
                            }
                        }
                    });
                }
            }
            Err(e) => ctx.set_toast(format!("could not load jobs: {e}")),
        }
    });
}

fn spawn_refresh_routines(ctx: AppCtx) {
    let revision = ctx.jobs_revision.get_untracked();
    spawn_local(async move {
        match api::routines().await {
            Ok(list) if ctx.jobs_revision.get_untracked() == revision => ctx.routines.set(list),
            Ok(_) => {}
            Err(e) => ctx.set_toast(format!("could not load routines: {e}")),
        }
    });
}

fn spawn_refresh_inbox(ctx: AppCtx) {
    let revision = ctx.jobs_revision.get_untracked();
    spawn_local(async move {
        match api::jobs_inbox().await {
            Ok(list) if ctx.jobs_revision.get_untracked() == revision => ctx.jobs_inbox.set(list),
            Ok(_) => {}
            Err(e) => ctx.set_toast(format!("could not load the approval inbox: {e}")),
        }
    });
}

fn refresh_detail(ctx: AppCtx, detail: RwSignal<Option<JobDetail>>, id: &str) {
    let id = id.to_string();
    spawn_local(async move {
        match api::job_detail(&id).await {
            Ok(d) if ctx.selected_job.get_untracked().as_deref() == Some(id.as_str()) => {
                detail.set(Some(d));
            }
            Ok(_) => {}
            Err(e) => ctx.set_toast(format!("could not refresh job: {e}")),
        }
    });
}

// ---- jobs SSE ----------------------------------------------------------------

struct JobsSubscription {
    source: web_sys::EventSource,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: Closure<dyn FnMut()>,
    _on_lagged: Closure<dyn FnMut()>,
    _on_replay_gap: Closure<dyn FnMut()>,
}

thread_local! {
    static JOBS_SUBSCRIPTION: std::cell::RefCell<Option<JobsSubscription>> =
        const { std::cell::RefCell::new(None) };
    /// Last seq seen on the jobs stream, for `?since=` re-subscriptions.
    /// Browser-driven auto-reconnects carry the cursor via Last-Event-ID.
    static JOBS_LAST_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn set_jobs_subscription(sub: JobsSubscription) {
    JOBS_SUBSCRIPTION.with(|s| {
        if let Some(old) = s.borrow_mut().replace(sub) {
            old.source.close();
        }
    });
}

fn close_jobs_subscription() {
    JOBS_SUBSCRIPTION.with(|s| {
        if let Some(old) = s.borrow_mut().take() {
            old.source.close();
        }
    });
}

fn subscribe_jobs(
    ctx: AppCtx,
    revision: u64,
    last_occ: RwSignal<HashMap<String, Occurrence>>,
    detail: RwSignal<Option<JobDetail>>,
) -> Result<JobsSubscription, String> {
    let since = JOBS_LAST_SEQ.with(|c| c.get());
    let url = if since > 0 {
        format!("/api/jobs/events?since={since}")
    } else {
        "/api/jobs/events".to_string()
    };
    let source = web_sys::EventSource::new(&api::with_lan_token(&url)).map_err(|_| {
        crate::diagnostics::report_sse_failure(vortex_types::BrowserOperation::SseConnect, None);
        "the browser refused to open the jobs event stream".to_string()
    })?;

    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            if let Ok(seq) = ev.last_event_id().parse::<u64>() {
                JOBS_LAST_SEQ.with(|c| {
                    if seq > c.get() {
                        c.set(seq);
                    }
                });
            }
            let Some(text) = ev.data().as_string() else {
                return;
            };
            let Ok(event) = serde_json::from_str::<JobEvent>(&text) else {
                crate::diagnostics::report_sse_failure(
                    vortex_types::BrowserOperation::SseDecode,
                    None,
                );
                return;
            };
            handle_jobs_event(ctx, last_occ, detail, event);
        });

    // Retained-log window evicted what we missed (or the consumer lagged):
    // refetch the authoritative lists; the live stream continues afterwards.
    let on_gap = move || {
        crate::diagnostics::report_sse_failure(vortex_types::BrowserOperation::SseGap, None);
        if ctx.jobs_revision.get_untracked() != revision {
            return;
        }
        spawn_refresh_jobs(ctx, last_occ);
        spawn_refresh_routines(ctx);
        spawn_refresh_inbox(ctx);
    };
    let on_lagged = Closure::<dyn FnMut()>::new(on_gap);
    let on_replay_gap = Closure::<dyn FnMut()>::new(on_gap);

    let source_err = source.clone();
    let on_error = Closure::<dyn FnMut()>::new(move || {
        // A stale subscription (tab left and re-entered) must not resync or
        // replace the current visit's stream.
        if ctx.tab.get_untracked() != Tab::Jobs || ctx.jobs_revision.get_untracked() != revision {
            return;
        }
        crate::diagnostics::report_sse_failure(vortex_types::BrowserOperation::SseReconnect, None);
        let closed = source_err.ready_state() == web_sys::EventSource::CLOSED;
        if closed {
            // The browser will not reconnect a CLOSED source on its own:
            // resync, then re-subscribe after a short delay.
            set_timeout(
                move || {
                    if ctx.tab.get_untracked() != Tab::Jobs
                        || ctx.jobs_revision.get_untracked() != revision
                    {
                        return;
                    }
                    spawn_refresh_jobs(ctx, last_occ);
                    spawn_refresh_routines(ctx);
                    spawn_refresh_inbox(ctx);
                    match subscribe_jobs(ctx, revision, last_occ, detail) {
                        Ok(sub) => set_jobs_subscription(sub),
                        Err(e) => {
                            crate::diagnostics::report_sse_failure(
                                vortex_types::BrowserOperation::SseReconnect,
                                None,
                            );
                            ctx.set_toast(format!("jobs stream reconnect failed: {e}"));
                        }
                    }
                },
                std::time::Duration::from_secs(1),
            );
        } else {
            // CONNECTING: the EventSource reconnects by itself (carrying
            // Last-Event-ID); fetch what we missed while it was down.
            spawn_refresh_jobs(ctx, last_occ);
            spawn_refresh_routines(ctx);
            spawn_refresh_inbox(ctx);
        }
    });

    for (name, cb) in [
        ("message", on_message.as_ref().unchecked_ref()),
        ("error", on_error.as_ref().unchecked_ref()),
        ("lagged", on_lagged.as_ref().unchecked_ref()),
        ("replay_gap", on_replay_gap.as_ref().unchecked_ref()),
    ] {
        source
            .add_event_listener_with_callback(name, cb)
            .map_err(|_| {
                source.close();
                crate::diagnostics::report_sse_failure(
                    vortex_types::BrowserOperation::SseConnect,
                    None,
                );
                "could not attach to the jobs event stream".to_string()
            })?;
    }

    Ok(JobsSubscription {
        source,
        _on_message: on_message,
        _on_error: on_error,
        _on_lagged: on_lagged,
        _on_replay_gap: on_replay_gap,
    })
}

fn handle_jobs_event(
    ctx: AppCtx,
    last_occ: RwSignal<HashMap<String, Occurrence>>,
    detail: RwSignal<Option<JobDetail>>,
    event: JobEvent,
) {
    match event {
        JobEvent::JobUpdated { job } => {
            detail.update(|d| {
                if let Some(d) = d {
                    if d.job.id == job.id {
                        d.job = job.clone();
                    }
                }
            });
            upsert_job(ctx, job);
        }
        JobEvent::RoutineUpdated { routine } => upsert_routine(ctx, routine),
        JobEvent::OccurrenceUpdated { occurrence } => {
            detail.update(|d| {
                if let Some(d) = d {
                    if d.job.id == occurrence.job_id {
                        upsert_occurrence(&mut d.occurrences, occurrence.clone());
                    }
                }
            });
            remember_occurrence(last_occ, occurrence);
        }
        JobEvent::ApprovalParked { title, .. } => {
            ctx.set_toast(format!("Approval parked: {title}"));
            spawn_refresh_inbox(ctx);
        }
        JobEvent::JobCircuitBroken { reason, .. } => {
            ctx.set_toast(format!("Job circuit breaker tripped: {reason}"));
            spawn_refresh_jobs(ctx, last_occ);
        }
    }
}

// ---- tab root ----------------------------------------------------------------

#[component]
pub fn JobsView() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let last_occ: RwSignal<HashMap<String, Occurrence>> = RwSignal::new(HashMap::new());
    let detail: RwSignal<Option<JobDetail>> = RwSignal::new(None);

    // New visit: invalidate in-flight list fetches from the previous visit,
    // then load fresh data and open the live event stream.
    ctx.bump_jobs_revision();
    let revision = ctx.jobs_revision.get_untracked();
    spawn_refresh_jobs(ctx, last_occ);
    spawn_refresh_routines(ctx);
    spawn_refresh_inbox(ctx);
    match subscribe_jobs(ctx, revision, last_occ, detail) {
        Ok(sub) => set_jobs_subscription(sub),
        Err(e) => ctx.set_toast(e),
    }
    on_cleanup(move || {
        close_jobs_subscription();
        ctx.bump_jobs_revision();
    });

    // Load the selected job's detail (job + occurrence history).
    Effect::new(move |_| {
        let Some(id) = ctx.selected_job.get() else {
            detail.set(None);
            return;
        };
        spawn_local(async move {
            match api::job_detail(&id).await {
                Ok(d) if ctx.selected_job.get_untracked().as_deref() == Some(id.as_str()) => {
                    detail.set(Some(d));
                }
                Ok(_) => {}
                Err(e) => ctx.set_toast(format!("could not load job: {e}")),
            }
        });
    });

    view! {
        <div class="jobs">
            <p class="jobs-banner">"Schedules fire only while Vortex is running. Ticks missed while it is closed follow each job's catch-up policy."</p>
            <InboxSection detail />
            {move || {
                if ctx.selected_job.get().is_some() {
                    view! { <JobDetailSection detail last_occ /> }.into_any()
                } else {
                    view! { <JobListSection last_occ /> }.into_any()
                }
            }}
            <RoutineSection />
        </div>
    }
}

// ---- inbox -------------------------------------------------------------------

#[component]
fn InboxSection(detail: RwSignal<Option<JobDetail>>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <section class="panel">
            <h3>"Approval inbox"
                <span class="panel-note">"job runs park here until you decide — no auto-deny"</span>
            </h3>
            <Show when=move || ctx.jobs_inbox.get().is_empty()>
                <p class="hint">"No parked approvals."</p>
            </Show>
            <For
                each=move || ctx.jobs_inbox.get()
                key=|p| p.approval.id.clone()
                let:parked
            >
                <ParkedCard parked detail />
            </For>
        </section>
    }
}

#[component]
fn ParkedCard(parked: ParkedApproval, detail: RwSignal<Option<JobDetail>>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let id_approve = parked.approval.id.clone();
    let id_deny = parked.approval.id.clone();
    let job_approve = parked.job_id.clone();
    let job_deny = parked.job_id.clone();
    let title = parked.approval.title.clone();
    let payload = parked.approval.payload.clone();
    let job_name = parked.job_name.clone();
    let agent = parked.approval.agent.clone();
    view! {
        <div class="jobs-inbox-card">
            <div class="approval-head">
                <strong>{title}</strong>
                <span class="badge">{format!("job: {job_name}")}</span>
            </div>
            <p class="hint">{format!("requested by {agent}")}</p>
            <pre class="approval-payload">{payload}</pre>
            <div class="approval-actions">
                <button class="btn danger"
                    on:click=move |_| {
                        let id = id_deny.clone();
                        let job_id = job_deny.clone();
                        spawn_local(async move {
                            decide_parked(ctx, &id, false, &job_id, detail).await;
                        });
                    }>"Deny"</button>
                <button class="btn primary"
                    on:click=move |_| {
                        let id = id_approve.clone();
                        let job_id = job_approve.clone();
                        spawn_local(async move {
                            decide_parked(ctx, &id, true, &job_id, detail).await;
                        });
                    }>"Approve"</button>
            </div>
        </div>
    }
}

async fn decide_parked(
    ctx: AppCtx,
    id: &str,
    approve: bool,
    job_id: &str,
    detail: RwSignal<Option<JobDetail>>,
) {
    match api::decide_approval(id, approve).await {
        Ok(()) => {
            // Same session-resolved guard as the run SSE path: a late poll
            // must not resurrect this approval in the Tasks panel.
            crate::sse::note_approval_resolved(id);
            ctx.jobs_inbox
                .update(|list| list.retain(|p| p.approval.id != id));
            ctx.approvals.update(|list| list.retain(|a| a.id != id));
            ctx.set_toast(if approve {
                "Approved — the job run resumes"
            } else {
                "Denied"
            });
            spawn_refresh_inbox(ctx);
            // The parked occurrence changes state on decision; if its job is
            // open, refresh the history (REST decide emits no JobEvent).
            if detail.get_untracked().is_some_and(|d| d.job.id == job_id) {
                refresh_detail(ctx, detail, job_id);
            }
        }
        Err(e) => ctx.set_toast(format!("decision failed: {e}")),
    }
}

// ---- job list + wizard ---------------------------------------------------------

#[component]
fn JobListSection(last_occ: RwSignal<HashMap<String, Occurrence>>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let wizard_open = RwSignal::new(false);
    // Zero-config friendly: with no jobs yet, the wizard is already open.
    Effect::new(move |_| {
        if ctx.jobs_loaded.get() && ctx.jobs.get().is_empty() {
            wizard_open.set(true);
        }
    });
    view! {
        <section class="panel">
            <h3>"Jobs" <span class="panel-note">"scheduled autonomous work"</span></h3>
            <div class="jobs-actions">
                <button class="btn primary"
                    on:click=move |_| wizard_open.update(|o| *o = !*o)>
                    {move || if wizard_open.get() { "Close wizard" } else { "New job" }}
                </button>
            </div>
            <Show when=move || wizard_open.get()>
                <JobWizardForm wizard_open />
            </Show>
            <Show when=move || !ctx.jobs_loaded.get()>
                <p class="hint">"Loading jobs…"</p>
            </Show>
            <Show when=move || ctx.jobs_loaded.get() && ctx.jobs.get().is_empty()>
                <p class="hint">"No jobs yet. Create one with the wizard: a name, instructions, and a schedule preset are all you need."</p>
            </Show>
            <ul class="jobs-row-list">
                <For
                    each=move || ctx.jobs.get()
                    key=|j| (j.id.clone(), j.updated_at.clone())
                    let:job
                >
                    <JobRow job last_occ />
                </For>
            </ul>
        </section>
    }
}

#[component]
fn JobRow(job: Job, last_occ: RwSignal<HashMap<String, Occurrence>>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let id = job.id.clone();
    let job_id = job.id.clone();
    let name = job.name.clone();
    let status = job.status;
    let next = next_run_label(&job);
    view! {
        <li>
            <button class="jobs-row-open"
                title="Open job details"
                on:click=move |_| ctx.selected_job.set(Some(id.clone()))>
                <span class="jobs-row-name">{name}</span>
                <span class=job_badge_class(status)>{status.as_str()}</span>
                <span class="jobs-row-meta">{next}</span>
                <span class="jobs-row-meta">{move || match last_occ.get().get(&job_id) {
                    Some(occ) => format!("last: {}", occ.status.as_str()),
                    None => "last: no runs yet".to_string(),
                }}</span>
            </button>
        </li>
    }
}

#[component]
fn JobWizardForm(wizard_open: RwSignal<bool>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let name = RwSignal::new(String::new());
    let instructions = RwSignal::new(String::new());
    let preset = RwSignal::new("daily".to_string());
    let workspace = RwSignal::new(String::new());
    let missed = RwSignal::new("run_once_if_missed".to_string());
    let overlap = RwSignal::new("skip".to_string());
    let max_turns = RwSignal::new("12".to_string());
    let max_tokens = RwSignal::new("120000".to_string());
    let daily_cap = RwSignal::new("24".to_string());
    let timeout = RwSignal::new("600".to_string());
    let busy = RwSignal::new(false);

    let submit = move || {
        if busy.get_untracked() {
            return;
        }
        let name_val = name.get_untracked().trim().to_string();
        let instructions_val = instructions.get_untracked().trim().to_string();
        if name_val.is_empty() || instructions_val.is_empty() {
            ctx.set_toast("A job needs a name and instructions.");
            return;
        }
        let selected = preset.get_untracked();
        let cron = PRESETS
            .iter()
            .find(|(key, _)| *key == selected)
            .and_then(|(_, cron)| *cron);
        let missed_policy = if missed.get_untracked() == "skip" {
            MissedPolicy::Skip
        } else {
            MissedPolicy::RunOnceIfMissed
        };
        let schedule = cron.map(|cron| ScheduleSpec {
            cron: cron.to_string(),
            tz: "local".to_string(),
            missed_policy,
        });
        let overlap_policy = if overlap.get_untracked() == "queue_one" {
            OverlapPolicy::QueueOne
        } else {
            OverlapPolicy::Skip
        };
        // Invalid advanced input falls back to the conservative defaults
        // rather than blocking the three-input happy path.
        let budgets = JobBudgets {
            max_turns: max_turns.get_untracked().trim().parse().unwrap_or(12),
            max_tokens: max_tokens.get_untracked().trim().parse().unwrap_or(120_000),
            daily_occurrence_cap: daily_cap.get_untracked().trim().parse().unwrap_or(24),
            run_timeout_secs: timeout.get_untracked().trim().parse().unwrap_or(600),
        };
        let ws = workspace.get_untracked().trim().to_string();
        let wizard = JobWizard {
            name: name_val,
            instructions: instructions_val,
            mode: None,
            model: None,
            schedule,
            workspace: if ws.is_empty() { None } else { Some(ws) },
            budgets: Some(budgets),
            overlap: Some(overlap_policy),
        };
        busy.set(true);
        spawn_local(async move {
            let result = api::create_job(&wizard).await;
            busy.set(false);
            match result {
                Ok(job) => {
                    ctx.set_toast("Job created");
                    upsert_job(ctx, job.clone());
                    wizard_open.set(false);
                    ctx.selected_job.set(Some(job.id));
                    // The wizard's routine was created atomically with the
                    // job; REST mutations emit no JobEvent, so refetch.
                    spawn_refresh_routines(ctx);
                }
                Err(e) => ctx.set_toast(format!("could not create job: {e}")),
            }
        });
    };

    view! {
        <div class="jobs-form">
            <div class="field">
                <label for="job-name">"Name"</label>
                <input id="job-name" type="text" maxlength="120" placeholder="Morning research digest"
                    prop:value=move || name.get()
                    on:input=move |ev| name.set(event_target_value(&ev)) />
            </div>
            <div class="field">
                <label for="job-instructions">"Instructions"</label>
                <textarea id="job-instructions" rows="4"
                    placeholder="What should this job do each time it runs?"
                    prop:value=move || instructions.get()
                    on:input=move |ev| instructions.set(event_target_value(&ev))></textarea>
            </div>
            <div class="field">
                <label for="job-schedule">"Schedule"</label>
                <select id="job-schedule"
                    prop:value=move || preset.get()
                    on:change=move |ev| preset.set(event_target_value(&ev))>
                    <option value="daily">"Daily 9:00"</option>
                    <option value="hourly">"Hourly"</option>
                    <option value="weekdays">"Weekdays 9:00"</option>
                    <option value="manual">"Manual only"</option>
                </select>
            </div>
            <details class="jobs-advanced">
                <summary>"Advanced options"</summary>
                <div class="advanced-body">
                    <div class="field">
                        <label for="job-workspace">"Workspace (blank = per-job scratch directory)"</label>
                        <input id="job-workspace" type="text" placeholder="/home/you/projects/site"
                            prop:value=move || workspace.get()
                            on:input=move |ev| workspace.set(event_target_value(&ev)) />
                    </div>
                    <div class="field-row">
                        <div class="field">
                            <label for="job-missed">"Missed ticks"</label>
                            <select id="job-missed"
                                prop:value=move || missed.get()
                                on:change=move |ev| missed.set(event_target_value(&ev))>
                                <option value="run_once_if_missed">"Run once if missed"</option>
                                <option value="skip">"Skip"</option>
                            </select>
                        </div>
                        <div class="field">
                            <label for="job-overlap">"If a run is still live"</label>
                            <select id="job-overlap"
                                prop:value=move || overlap.get()
                                on:change=move |ev| overlap.set(event_target_value(&ev))>
                                <option value="skip">"Skip the new tick"</option>
                                <option value="queue_one">"Queue one"</option>
                            </select>
                        </div>
                    </div>
                    <div class="jobs-budget-grid">
                        <div class="field">
                            <label for="job-max-turns">"Max turns"</label>
                            <input id="job-max-turns" type="number" min="1"
                                prop:value=move || max_turns.get()
                                on:input=move |ev| max_turns.set(event_target_value(&ev)) />
                        </div>
                        <div class="field">
                            <label for="job-max-tokens">"Max tokens"</label>
                            <input id="job-max-tokens" type="number" min="1"
                                prop:value=move || max_tokens.get()
                                on:input=move |ev| max_tokens.set(event_target_value(&ev)) />
                        </div>
                        <div class="field">
                            <label for="job-daily-cap">"Daily occurrence cap"</label>
                            <input id="job-daily-cap" type="number" min="1"
                                prop:value=move || daily_cap.get()
                                on:input=move |ev| daily_cap.set(event_target_value(&ev)) />
                        </div>
                        <div class="field">
                            <label for="job-timeout">"Run timeout (seconds)"</label>
                            <input id="job-timeout" type="number" min="1"
                                prop:value=move || timeout.get()
                                on:input=move |ev| timeout.set(event_target_value(&ev)) />
                        </div>
                    </div>
                </div>
            </details>
            <div class="jobs-actions">
                <button class="btn primary" disabled=move || busy.get()
                    on:click=move |_| submit()>"Create job"</button>
            </div>
        </div>
    }
}

// ---- job detail ----------------------------------------------------------------

#[component]
fn JobDetailSection(
    detail: RwSignal<Option<JobDetail>>,
    last_occ: RwSignal<HashMap<String, Occurrence>>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <section class="panel">
            <div class="jobs-actions">
                <button class="btn" on:click=move |_| ctx.selected_job.set(None)>"← All jobs"</button>
            </div>
            <Show
                when=move || detail.get().is_some()
                fallback=move || view! { <p class="hint">"Loading job… If this persists, the job may have been deleted — go back to the list."</p> }
            >
                <JobDetailBody detail last_occ />
            </Show>
        </section>
    }
}

/// Editable budget fields, kept as strings so partial typing never snaps back.
#[derive(Clone, Default)]
struct BudgetDraft {
    max_turns: String,
    max_tokens: String,
    daily_cap: String,
    timeout: String,
}

impl From<JobBudgets> for BudgetDraft {
    fn from(b: JobBudgets) -> Self {
        Self {
            max_turns: b.max_turns.to_string(),
            max_tokens: b.max_tokens.to_string(),
            daily_cap: b.daily_occurrence_cap.to_string(),
            timeout: b.run_timeout_secs.to_string(),
        }
    }
}

#[component]
fn JobDetailBody(
    detail: RwSignal<Option<JobDetail>>,
    last_occ: RwSignal<HashMap<String, Occurrence>>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    // The body only renders once a detail is loaded; the conversation id is
    // immutable for the job's lifetime.
    let conv_id = detail
        .get_untracked()
        .map(|d| d.job.conversation_id)
        .unwrap_or_default();
    let steer_draft = RwSignal::new(String::new());
    let steer_for = RwSignal::new(None::<String>);
    let budget_draft = RwSignal::new(BudgetDraft::default());
    let busy = RwSignal::new(false);

    // Seed the editors when the loaded job changes — not on every live event,
    // so typing is never clobbered by occurrence updates.
    Effect::new(move |_| {
        let Some(d) = detail.get() else { return };
        if steer_for.get_untracked().as_deref() != Some(d.job.id.as_str()) {
            steer_for.set(Some(d.job.id.clone()));
            steer_draft.set(d.job.steer_note.clone().unwrap_or_default());
            budget_draft.set(BudgetDraft::from(d.job.budgets));
        }
    });

    let act = move |kind: &'static str| {
        if busy.get_untracked() {
            return;
        }
        let Some(d) = detail.get_untracked() else {
            return;
        };
        let id = d.job.id.clone();
        busy.set(true);
        spawn_local(async move {
            let result = match kind {
                "activate" => api::activate_job(&id).await,
                "pause" => api::pause_job(&id).await,
                "resume" => api::resume_job(&id).await,
                _ => Ok(()),
            };
            busy.set(false);
            match result {
                Ok(()) => {
                    ctx.set_toast(match kind {
                        "activate" => "Job activated — its schedule is live",
                        "pause" => "Job paused",
                        _ => "Job resumed",
                    });
                    refresh_detail(ctx, detail, &id);
                    // REST mutations emit no JobEvent: refetch the list so
                    // the status badge stays authoritative.
                    spawn_refresh_jobs(ctx, last_occ);
                }
                Err(e) => ctx.set_toast(format!("{kind} failed: {e}")),
            }
        });
    };

    let run_now = move || {
        if busy.get_untracked() {
            return;
        }
        let Some(d) = detail.get_untracked() else {
            return;
        };
        let id = d.job.id.clone();
        busy.set(true);
        spawn_local(async move {
            let result = api::run_job(&id).await;
            busy.set(false);
            match result {
                Ok(occ) => {
                    ctx.set_toast("Occurrence started");
                    remember_occurrence(last_occ, occ.clone());
                    detail.update(|d| {
                        if let Some(d) = d {
                            if d.job.id == occ.job_id {
                                upsert_occurrence(&mut d.occurrences, occ);
                            }
                        }
                    });
                }
                Err(e) => ctx.set_toast(format!("could not start run: {e}")),
            }
        });
    };

    let delete = move || {
        let Some(d) = detail.get_untracked() else {
            return;
        };
        let id = d.job.id.clone();
        let name = d.job.name.clone();
        let confirmed = web_sys::window()
            .and_then(|w| {
                w.confirm_with_message(&format!(
                    "Delete job \"{name}\"? Its occurrence history is removed; the routine is kept."
                ))
                .ok()
            })
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        spawn_local(async move {
            match api::delete_job(&id).await {
                Ok(()) => {
                    ctx.set_toast("Job deleted");
                    ctx.selected_job.set(None);
                    ctx.jobs.update(|list| list.retain(|j| j.id != id));
                    last_occ.update(|map| {
                        map.remove(&id);
                    });
                }
                Err(e) => ctx.set_toast(format!("delete failed: {e}")),
            }
        });
    };

    let save_steer = move || {
        let Some(d) = detail.get_untracked() else {
            return;
        };
        let id = d.job.id.clone();
        let note = steer_draft.get_untracked().trim().to_string();
        if note.is_empty() {
            ctx.set_toast("Write a note first, or use Clear.");
            return;
        }
        spawn_local(async move {
            match api::steer_job(&id, Some(&note)).await {
                Ok(()) => {
                    ctx.set_toast("Steer note saved — applied to the next occurrence");
                    refresh_detail(ctx, detail, &id);
                }
                Err(e) => ctx.set_toast(format!("could not save note: {e}")),
            }
        });
    };

    let clear_steer = move || {
        let Some(d) = detail.get_untracked() else {
            return;
        };
        let id = d.job.id.clone();
        spawn_local(async move {
            match api::steer_job(&id, None).await {
                Ok(()) => {
                    ctx.set_toast("Steer note cleared");
                    steer_draft.set(String::new());
                    refresh_detail(ctx, detail, &id);
                }
                Err(e) => ctx.set_toast(format!("could not clear note: {e}")),
            }
        });
    };

    let save_budgets = move || {
        let Some(d) = detail.get_untracked() else {
            return;
        };
        let id = d.job.id.clone();
        let draft = budget_draft.get_untracked();
        let (Ok(max_turns), Ok(max_tokens), Ok(daily_occurrence_cap), Ok(run_timeout_secs)) = (
            draft.max_turns.trim().parse::<u32>(),
            draft.max_tokens.trim().parse::<u64>(),
            draft.daily_cap.trim().parse::<u32>(),
            draft.timeout.trim().parse::<u64>(),
        ) else {
            ctx.set_toast("Budgets must be positive whole numbers.");
            return;
        };
        if max_turns == 0 || max_tokens == 0 || daily_occurrence_cap == 0 || run_timeout_secs == 0 {
            ctx.set_toast("Budgets must be positive whole numbers.");
            return;
        }
        let patch = JobPatch {
            budgets: Some(JobBudgets {
                max_turns,
                max_tokens,
                daily_occurrence_cap,
                run_timeout_secs,
            }),
            ..Default::default()
        };
        spawn_local(async move {
            match api::patch_job(&id, &patch).await {
                Ok(job) => {
                    ctx.set_toast("Budgets updated");
                    upsert_job(ctx, job.clone());
                    detail.update(|d| {
                        if let Some(d) = d {
                            if d.job.id == job.id {
                                d.job = job;
                            }
                        }
                    });
                }
                Err(e) => ctx.set_toast(format!("could not update budgets: {e}")),
            }
        });
    };

    view! {
        <div class="jobs-detail">
            <div class="jobs-detail-head">
                <h3>{move || detail.get().map(|d| d.job.name).unwrap_or_default()}</h3>
                <span class=move || detail.get().map(|d| job_badge_class(d.job.status)).unwrap_or("badge")>
                    {move || detail.get().map(|d| d.job.status.as_str()).unwrap_or("")}
                </span>
            </div>
            <Show when=move || detail.get().is_some_and(|d| d.job.status == JobStatus::CircuitBroken)>
                <p class="jobs-banner">{move || {
                    let failures = detail.get().map(|d| d.job.consecutive_failures).unwrap_or(0);
                    format!("Circuit breaker tripped after {failures} consecutive failed occurrences. Review the history below, then resume explicitly.")
                }}</p>
            </Show>
            <dl class="jobs-meta">
                <dt>"Schedule"</dt>
                <dd>{move || detail.get().map(|d| schedule_label(&d.job)).unwrap_or_default()}</dd>
                <dt>"Routine"</dt>
                <dd>{move || {
                    let Some(d) = detail.get() else { return String::new(); };
                    let name = ctx.routines.get().iter()
                        .find(|r| r.id == d.job.routine_id)
                        .map(|r| r.name.clone());
                    match name {
                        Some(n) => format!("{n} · v{}", d.job.routine_version),
                        None => format!("{} · v{}", d.job.routine_id, d.job.routine_version),
                    }
                }}</dd>
                <dt>"Workspace"</dt>
                <dd>{move || detail.get().and_then(|d| d.job.workspace).unwrap_or_else(|| "per-job scratch directory".into())}</dd>
                <dt>"Overlap"</dt>
                <dd>{move || detail.get().map(|d| match d.job.overlap {
                    OverlapPolicy::Skip => "skip new ticks while a run is live",
                    OverlapPolicy::QueueOne => "queue one occurrence behind the live run",
                }).unwrap_or_default()}</dd>
            </dl>
            <div class="jobs-actions">
                <Show when=move || detail.get().is_some_and(|d| d.job.status == JobStatus::Draft)>
                    <button class="btn primary" disabled=move || busy.get()
                        on:click=move |_| act("activate")>"Activate"</button>
                </Show>
                <Show when=move || detail.get().is_some_and(|d| d.job.status == JobStatus::Active)>
                    <button class="btn" disabled=move || busy.get()
                        on:click=move |_| act("pause")>"Pause"</button>
                    <button class="btn" disabled=move || busy.get()
                        on:click=move |_| run_now()>"Run now"</button>
                </Show>
                <Show when=move || detail.get().is_some_and(|d| matches!(d.job.status, JobStatus::Paused | JobStatus::CircuitBroken))>
                    <button class="btn primary" disabled=move || busy.get()
                        on:click=move |_| act("resume")>"Resume"</button>
                </Show>
                <button class="btn danger" disabled=move || busy.get()
                    on:click=move |_| delete()>"Delete"</button>
            </div>
            <div class="field">
                <label for="job-steer">"Steer note (applied to the next occurrence)"</label>
                <textarea id="job-steer" rows="2"
                    prop:value=move || steer_draft.get()
                    on:input=move |ev| steer_draft.set(event_target_value(&ev))></textarea>
                <div class="jobs-actions">
                    <button class="btn" on:click=move |_| save_steer()>"Save note"</button>
                    <button class="btn" on:click=move |_| clear_steer()>"Clear"</button>
                </div>
            </div>
            <details class="jobs-advanced">
                <summary>"Budgets (per-occurrence guardrails)"</summary>
                <div class="advanced-body">
                    <div class="jobs-budget-grid">
                        <div class="field">
                            <label for="job-detail-max-turns">"Max turns"</label>
                            <input id="job-detail-max-turns" type="number" min="1"
                                prop:value=move || budget_draft.get().max_turns
                                on:input=move |ev| budget_draft.update(|b| b.max_turns = event_target_value(&ev)) />
                        </div>
                        <div class="field">
                            <label for="job-detail-max-tokens">"Max tokens"</label>
                            <input id="job-detail-max-tokens" type="number" min="1"
                                prop:value=move || budget_draft.get().max_tokens
                                on:input=move |ev| budget_draft.update(|b| b.max_tokens = event_target_value(&ev)) />
                        </div>
                        <div class="field">
                            <label for="job-detail-daily-cap">"Daily occurrence cap"</label>
                            <input id="job-detail-daily-cap" type="number" min="1"
                                prop:value=move || budget_draft.get().daily_cap
                                on:input=move |ev| budget_draft.update(|b| b.daily_cap = event_target_value(&ev)) />
                        </div>
                        <div class="field">
                            <label for="job-detail-timeout">"Run timeout (seconds)"</label>
                            <input id="job-detail-timeout" type="number" min="1"
                                prop:value=move || budget_draft.get().timeout
                                on:input=move |ev| budget_draft.update(|b| b.timeout = event_target_value(&ev)) />
                        </div>
                    </div>
                    <div class="jobs-actions">
                        <button class="btn" on:click=move |_| save_budgets()>"Save budgets"</button>
                    </div>
                </div>
            </details>
            <h4 class="jobs-subhead">"Occurrence history"</h4>
            <Show when=move || detail.get().is_some_and(|d| d.occurrences.is_empty())>
                <p class="hint">"No occurrences yet. The first tick (or a manual run) will appear here."</p>
            </Show>
            <ul class="jobs-row-list jobs-scroll">
                <For
                    each=move || detail.get().map(|d| d.occurrences).unwrap_or_default()
                    key=|o| (o.id.clone(), o.updated_at.clone())
                    let:occ
                >
                    <OccurrenceRow occ conv_id=conv_id.clone() detail />
                </For>
            </ul>
        </div>
    }
}

#[component]
fn OccurrenceRow(
    occ: Occurrence,
    conv_id: String,
    detail: RwSignal<Option<JobDetail>>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let status = occ.status;
    let scheduled = local_time(&occ.scheduled_for);
    let reason = occ.reason.clone();
    let review = occ.needs_human_review;
    let run_id = occ.run_id.clone();
    let occ_continue = occ.id.clone();
    let occ_dismiss = occ.id.clone();
    view! {
        <li class="jobs-occ">
            <div class="jobs-occ-head">
                <span class=occ_badge_class(status)>{status.as_str()}</span>
                <span class="jobs-row-meta">{scheduled}</span>
                {review.then(|| view! { <span class="badge jobs-occ-wait">"needs human review"</span> })}
                {run_id.map(|rid| view! {
                    <button class="btn jobs-mini" title="Open this run in the Tasks view"
                        on:click=move |_| open_occurrence_run(ctx, conv_id.clone(), rid.clone())>"Open run"</button>
                })}
            </div>
            {reason.filter(|r| !r.is_empty()).map(|r| view! { <p class="jobs-occ-reason">{r}</p> })}
            {review.then(|| view! {
                <div class="jobs-occ-actions">
                    <button class="btn jobs-mini"
                        title="Mark reviewed and let the job continue"
                        on:click=move |_| {
                            let id = occ_continue.clone();
                            spawn_local(async move { decide_review(ctx, id, ReviewAction::Continue, detail).await; });
                        }>"Continue"</button>
                    <button class="btn jobs-mini danger"
                        title="Dismiss the review flag without continuing"
                        on:click=move |_| {
                            let id = occ_dismiss.clone();
                            spawn_local(async move { decide_review(ctx, id, ReviewAction::Dismiss, detail).await; });
                        }>"Dismiss"</button>
                </div>
            })}
        </li>
    }
}

async fn decide_review(
    ctx: AppCtx,
    occ_id: String,
    action: ReviewAction,
    detail: RwSignal<Option<JobDetail>>,
) {
    match api::resolve_review(&occ_id, action).await {
        Ok(()) => {
            ctx.set_toast(match action {
                ReviewAction::Continue => "Review resolved — the job may continue",
                ReviewAction::Dismiss => "Review dismissed",
            });
            if let Some(d) = detail.get_untracked() {
                refresh_detail(ctx, detail, &d.job.id);
            }
        }
        Err(e) => ctx.set_toast(format!("review resolution failed: {e}")),
    }
}

/// Open an occurrence's run in the existing Tasks run-tree view: select the
/// job's dedicated conversation, restore the panels from the run snapshot,
/// and re-subscribe the run SSE when the run is still live. Mirrors the
/// sidebar's conversation-open flow, targeted at a specific run.
fn open_occurrence_run(ctx: AppCtx, conv_id: String, run_id: String) {
    ctx.sidebar_open.set(false);
    ctx.select_attachment_draft(&conv_id);
    ctx.messages.set(Vec::new());
    ctx.current.set(Some(conv_id.clone()));
    ctx.stream_text.set(String::new());
    ctx.stream_display.set(String::new());
    ctx.plan.set(Vec::new());
    ctx.tool_calls.set(Vec::new());
    ctx.agents.set(Vec::new());
    ctx.run_status.set(None);
    ctx.error_banner.set(None);
    ctx.run_id.set(None);
    crate::sse::close_subscription();
    ctx.tab.set(Tab::Tasks);
    spawn_local(async move {
        if let Ok(messages) = api::messages(&conv_id).await {
            if ctx.current.get_untracked().as_deref() == Some(conv_id.as_str()) {
                ctx.messages.set(messages);
            }
        }
        let Ok(snap) = api::run_state(&run_id).await else {
            return;
        };
        // A late snapshot must not clobber live panels if the user moved on
        // or started a new turn meanwhile.
        if ctx.current.get_untracked().as_deref() != Some(conv_id.as_str())
            || ctx.run_id.get_untracked().is_some()
            || ctx.chat_sending.get_untracked()
        {
            return;
        }
        ctx.tool_calls.set(
            snap.tool_calls
                .into_iter()
                .map(|t| {
                    let running = !t.ok && t.summary == "running";
                    crate::state::ToolCallDisplay {
                        call_id: t.id,
                        tool: t.tool,
                        args: t.args,
                        status: if running {
                            RunStatus::Running
                        } else if t.ok {
                            RunStatus::Completed
                        } else {
                            RunStatus::Failed
                        },
                        summary: t.summary,
                        diff: None,
                        running,
                    }
                })
                .collect(),
        );
        ctx.agents.set(
            snap.agents
                .into_iter()
                .map(|r| crate::state::AgentDisplay {
                    run_id: r.id,
                    agent: r.agent,
                    task: r.task,
                    model: r.model,
                    status: r.status,
                    usage: r.usage,
                })
                .collect(),
        );
        if let Some(run) = snap.run {
            ctx.error_banner.set(run.error);
            ctx.run_status.set(Some(run.status));
            ctx.plan.set(run.plan);
            ctx.run_model.set(Some((run.id.clone(), run.model)));
            if matches!(
                run.status,
                RunStatus::Queued | RunStatus::Running | RunStatus::WaitingApproval
            ) {
                crate::chat::start_stream(ctx, run.id);
            } else {
                ctx.run_id.set(Some(run.id));
            }
        }
    });
}

// ---- routine library -----------------------------------------------------------

#[component]
fn RoutineSection() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let expanded: RwSignal<Option<String>> = RwSignal::new(None);
    let versions: RwSignal<Vec<RoutineVersion>> = RwSignal::new(Vec::new());
    let form_open = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let instructions = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    let create = move || {
        if busy.get_untracked() {
            return;
        }
        let name_val = name.get_untracked().trim().to_string();
        let instructions_val = instructions.get_untracked().trim().to_string();
        if name_val.is_empty() || instructions_val.is_empty() {
            ctx.set_toast("A routine needs a name and instructions.");
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let result = api::create_routine(&name_val, &instructions_val, None, None, None).await;
            busy.set(false);
            match result {
                Ok(routine) => {
                    ctx.set_toast("Routine saved as draft — activate it to use it");
                    upsert_routine(ctx, routine);
                    form_open.set(false);
                    spawn_refresh_routines(ctx);
                }
                Err(e) => ctx.set_toast(format!("could not create routine: {e}")),
            }
        });
    };

    view! {
        <section class="panel">
            <h3>"Routine library"
                <span class="panel-note">"versioned instruction templates — agents draft, you activate"</span>
            </h3>
            <div class="jobs-actions">
                <button class="btn" on:click=move |_| form_open.update(|o| *o = !*o)>
                    {move || if form_open.get() { "Close form" } else { "New routine" }}
                </button>
            </div>
            <Show when=move || form_open.get()>
                <div class="jobs-form">
                    <div class="field">
                        <label for="routine-name">"Name"</label>
                        <input id="routine-name" type="text" maxlength="120"
                            prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev)) />
                    </div>
                    <div class="field">
                        <label for="routine-instructions">"Instructions"</label>
                        <textarea id="routine-instructions" rows="4"
                            prop:value=move || instructions.get()
                            on:input=move |ev| instructions.set(event_target_value(&ev))></textarea>
                    </div>
                    <div class="jobs-actions">
                        <button class="btn primary" disabled=move || busy.get()
                            on:click=move |_| create()>"Save draft"</button>
                    </div>
                </div>
            </Show>
            <Show when=move || ctx.routines.get().is_empty()>
                <p class="hint">"No routines yet. Write one here, or let the assistant draft one in Agent mode."</p>
            </Show>
            <ul class="jobs-row-list">
                <For
                    each=move || ctx.routines.get()
                    key=|r| (r.id.clone(), r.updated_at.clone())
                    let:r
                >
                    <RoutineRow routine=r expanded versions />
                </For>
            </ul>
        </section>
    }
}

#[component]
fn RoutineRow(
    routine: Routine,
    expanded: RwSignal<Option<String>>,
    versions: RwSignal<Vec<RoutineVersion>>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let id_activate = routine.id.clone();
    let id_archive = routine.id.clone();
    let id_versions = routine.id.clone();
    let id_show = routine.id.clone();
    let name = routine.name.clone();
    let name_archive = routine.name.clone();
    let status = routine.status;
    let version = routine.current_version;
    let description = routine.description.clone();

    let toggle_versions = move || {
        if expanded.get_untracked().as_deref() == Some(id_versions.as_str()) {
            expanded.set(None);
            return;
        }
        expanded.set(Some(id_versions.clone()));
        versions.set(Vec::new());
        let id = id_versions.clone();
        spawn_local(async move {
            match api::routine_detail(&id).await {
                Ok(d) if expanded.get_untracked().as_deref() == Some(id.as_str()) => {
                    let mut list = d.versions;
                    list.sort_by(|a, b| b.version.cmp(&a.version));
                    versions.set(list);
                }
                Ok(_) => {}
                Err(e) => ctx.set_toast(format!("could not load versions: {e}")),
            }
        });
    };

    view! {
        <li class="jobs-occ">
            <div class="jobs-occ-head">
                <span class="jobs-row-name">{name}</span>
                <span class=routine_badge_class(status)>{status.as_str()}</span>
                <span class="jobs-row-meta">{format!("v{version}")}</span>
            </div>
            {(!description.is_empty()).then(|| view! { <p class="jobs-occ-reason">{description}</p> })}
            <div class="jobs-occ-actions">
                <Show when=move || status == RoutineStatus::Draft>
                    {
                        let id = id_activate.clone();
                        view! {
                            <button class="btn jobs-mini"
                                title="Human activation — drafts never run on their own"
                                on:click=move |_| {
                                    let id = id.clone();
                                    spawn_local(async move {
                                        match api::activate_routine(&id).await {
                                            Ok(()) => {
                                                ctx.set_toast("Routine activated");
                                                spawn_refresh_routines(ctx);
                                            }
                                            Err(e) => ctx.set_toast(format!("activation failed: {e}")),
                                        }
                                    });
                                }>"Activate"</button>
                        }
                    }
                </Show>
                <Show when=move || status == RoutineStatus::Active>
                    {
                        let id = id_archive.clone();
                        let name = name_archive.clone();
                        view! {
                            <button class="btn jobs-mini"
                                on:click=move |_| {
                                    let confirmed = web_sys::window()
                                        .and_then(|w| {
                                            w.confirm_with_message(&format!(
                                                "Archive routine \"{name}\"? Jobs keep their pinned versions."
                                            ))
                                            .ok()
                                        })
                                        .unwrap_or(false);
                                    if !confirmed {
                                        return;
                                    }
                                    let id = id.clone();
                                    spawn_local(async move {
                                        match api::archive_routine(&id).await {
                                            Ok(()) => {
                                                ctx.set_toast("Routine archived");
                                                spawn_refresh_routines(ctx);
                                            }
                                            Err(e) => ctx.set_toast(format!("archive failed: {e}")),
                                        }
                                    });
                                }>"Archive"</button>
                        }
                    }
                </Show>
                <button class="btn jobs-mini" on:click=move |_| toggle_versions()>"Versions"</button>
            </div>
            <Show when=move || expanded.get().as_deref() == Some(id_show.as_str())>
                <div class="jobs-scroll">
                    <Show when=move || versions.get().is_empty()>
                        <p class="hint">"Loading versions…"</p>
                    </Show>
                    <For
                        each=move || versions.get()
                        key=|v| v.version
                        let:v
                    >
                        <div class="jobs-version">
                            <div class="jobs-occ-head">
                                <span class="badge">{format!("v{}", v.version)}</span>
                                <span class="jobs-row-meta">{v.mode.as_str()}</span>
                                <span class="jobs-row-meta">{v.model.clone().unwrap_or_else(|| "Auto".into())}</span>
                                <span class="jobs-row-meta">{local_time(&v.created_at)}</span>
                            </div>
                            <pre>{v.instructions.clone()}</pre>
                        </div>
                    </For>
                </div>
            </Show>
        </li>
    }
}

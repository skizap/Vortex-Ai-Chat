//! SSE subscription: web_sys::EventSource driving AppCtx signal updates.
//! The closures are kept alive for the lifetime of the subscription and are
//! released when a new run replaces the source.
//!
//! Recovery: the active subscription lives in a thread-local here so views
//! can replace or close it. On transport errors the UI resyncs from
//! `/api/runs/{id}/state`; if the EventSource is CLOSED (no automatic
//! reconnect) and the run is still live, it re-subscribes after a short
//! delay, appending `?since=<last seq>` so the server replays everything
//! missed from its per-run event log. The server's `replay_gap` event (the
//! retained log window evicted what we missed, or the consumer lagged)
//! triggers the same snapshot resync. Browser-driven auto-reconnects carry
//! the cursor natively via `Last-Event-ID`.

use crate::state::AppCtx;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::VecDeque;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

pub struct RunSubscription {
    pub source: web_sys::EventSource,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: Closure<dyn FnMut()>,
    _on_lagged: Closure<dyn FnMut()>,
    _on_replay_gap: Closure<dyn FnMut()>,
}

thread_local! {
    static SUBSCRIPTION: std::cell::RefCell<Option<RunSubscription>> =
        const { std::cell::RefCell::new(None) };
    /// (run_id, last seen SSE seq) for the active stream. Server seqs are
    /// per-run, so a cursor is only valid for the run it was seen on.
    static LAST_STREAM: std::cell::RefCell<(String, u64)> =
        const { std::cell::RefCell::new((String::new(), 0)) };
    /// Approval ids resolved during this session (via SSE). The 5 s poll and
    /// the boot fetch filter the server list through this set, so an approval
    /// resolved moments ago cannot be briefly resurrected by an already
    /// in-flight poll response (P12). Bounded: oldest entries drop off.
    static RESOLVED_APPROVALS: std::cell::RefCell<VecDeque<String>> =
        const { std::cell::RefCell::new(VecDeque::new()) };
}

fn note_seq(run_id: &str, seq: u64) {
    LAST_STREAM.with(|c| {
        let mut b = c.borrow_mut();
        if b.0 != run_id {
            b.0 = run_id.to_string();
            b.1 = 0;
        }
        if seq > b.1 {
            b.1 = seq;
        }
    });
}

/// Replay cursor for a (re-)subscription to `run_id`: the last seq seen on
/// that run's stream, or 0 (full replay) for a different/new run.
fn cursor_for(run_id: &str) -> u64 {
    LAST_STREAM.with(|c| {
        let b = c.borrow();
        if b.0 == run_id {
            b.1
        } else {
            0
        }
    })
}

pub fn note_approval_resolved(id: &str) {
    RESOLVED_APPROVALS.with(|r| {
        let mut q = r.borrow_mut();
        if q.iter().any(|x| x == id) {
            return;
        }
        q.push_back(id.to_string());
        while q.len() > 512 {
            q.pop_front();
        }
    });
}

/// Drop approvals this session already saw resolved (see RESOLVED_APPROVALS).
pub fn filter_resolved_approvals(
    list: Vec<vortex_types::ApprovalInfo>,
) -> Vec<vortex_types::ApprovalInfo> {
    RESOLVED_APPROVALS.with(|r| {
        let q = r.borrow();
        if q.is_empty() {
            return list;
        }
        list.into_iter()
            .filter(|a| !q.iter().any(|x| *x == a.id))
            .collect()
    })
}

/// Replace the active subscription, closing the previous one.
pub fn set_new_subscription(sub: RunSubscription) {
    SUBSCRIPTION.with(|s| {
        if let Some(old) = s.borrow_mut().replace(sub) {
            old.source.close();
        }
    });
}

/// Close and drop the active subscription (conversation switch/delete).
pub fn close_subscription() {
    SUBSCRIPTION.with(|s| {
        if let Some(old) = s.borrow_mut().take() {
            old.source.close();
        }
    });
}

/// Open the event stream for a run. Construction can fail (e.g. browser
/// connection limits), so this returns a `Result` instead of trapping the
/// WASM module with `expect`.
pub fn subscribe_run(ctx: AppCtx, run_id: String) -> Result<RunSubscription, String> {
    // Manual re-subscriptions continue from the last seq seen on this run so
    // the server replays what was missed; a fresh run starts at 0 (the server
    // then replays its full retained log, covering events published between
    // run start and the EventSource actually opening).
    let since = cursor_for(&run_id);
    let url = if since > 0 {
        format!("/api/runs/{run_id}/events?since={since}")
    } else {
        format!("/api/runs/{run_id}/events")
    };
    let source = web_sys::EventSource::new(&url).map_err(|_| {
        crate::diagnostics::report_sse_failure(
            vortex_types::BrowserOperation::SseConnect,
            Some(&run_id),
        );
        "the browser refused to open the run event stream \
              (too many open connections?)"
            .to_string()
    })?;

    let ctx_msg = ctx;
    let run_id_msg = run_id.clone();
    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            // Track the server's sequence id for `?since=` re-subscriptions.
            if let Ok(seq) = ev.last_event_id().parse::<u64>() {
                note_seq(&run_id_msg, seq);
            }
            let Some(text) = ev.data().as_string() else {
                return;
            };
            let Ok(event) = serde_json::from_str::<vortex_types::RunEvent>(&text) else {
                crate::diagnostics::report_sse_failure(
                    vortex_types::BrowserOperation::SseDecode,
                    Some(&run_id_msg),
                );
                return;
            };
            handle_event(&ctx_msg, event);
        });

    // Server-side gap hint: either the consumer lagged behind the broadcast
    // buffer or our cursor predates the retained log window. Resync from the
    // run snapshot instead of showing stale (frozen) state; the live stream
    // continues afterwards.
    let on_gap = {
        let run_id = run_id.clone();
        move || {
            let run_id = run_id.clone();
            crate::diagnostics::report_sse_failure(
                vortex_types::BrowserOperation::SseGap,
                Some(&run_id),
            );
            spawn_local(async move {
                resync_from_server(&ctx, &run_id).await;
            });
        }
    };
    let on_lagged = Closure::<dyn FnMut()>::new(on_gap.clone());
    let on_replay_gap = Closure::<dyn FnMut()>::new(on_gap);

    let ctx_err = ctx;
    let run_id_err = run_id.clone();
    let source_err = source.clone();
    let on_error = Closure::<dyn FnMut()>::new(move || {
        let nonterminal = ctx_err
            .run_status
            .get_untracked()
            .map(|s| !s.is_terminal())
            .unwrap_or(false);
        if !nonterminal {
            return;
        }
        crate::diagnostics::report_sse_failure(
            vortex_types::BrowserOperation::SseReconnect,
            Some(&run_id_err),
        );
        let closed = source_err.ready_state() == web_sys::EventSource::CLOSED;
        let ctx = ctx_err;
        let run_id = run_id_err.clone();
        if closed {
            // The browser will not reconnect a CLOSED source on its own:
            // resync, then re-subscribe if the run is still live. The delay
            // keeps a server that closes instantly from being hammered.
            set_timeout(
                move || {
                    spawn_local(async move {
                        resync_from_server(&ctx, &run_id).await;
                        let still_live = ctx
                            .run_status
                            .get_untracked()
                            .map(|s| !s.is_terminal())
                            .unwrap_or(false);
                        if still_live {
                            match subscribe_run(ctx, run_id.clone()) {
                                Ok(sub) => set_new_subscription(sub),
                                Err(e) => {
                                    crate::diagnostics::report_sse_failure(
                                        vortex_types::BrowserOperation::SseReconnect,
                                        Some(&run_id),
                                    );
                                    ctx.set_toast(format!("run stream reconnect failed: {e}"))
                                }
                            }
                        }
                    });
                },
                std::time::Duration::from_secs(1),
            );
        } else {
            // CONNECTING: the EventSource reconnects by itself (carrying
            // Last-Event-ID); fetch what we missed while it was down.
            spawn_local(async move {
                resync_from_server(&ctx, &run_id).await;
            });
        }
    });

    source
        .add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref())
        .map_err(|_| {
            source.close();
            crate::diagnostics::report_sse_failure(
                vortex_types::BrowserOperation::SseConnect,
                Some(&run_id),
            );
            "could not attach to the run event stream".to_string()
        })?;
    source
        .add_event_listener_with_callback("error", on_error.as_ref().unchecked_ref())
        .map_err(|_| {
            source.close();
            crate::diagnostics::report_sse_failure(
                vortex_types::BrowserOperation::SseConnect,
                Some(&run_id),
            );
            "could not attach to the run event stream".to_string()
        })?;
    // Legacy name (older servers) — same resync semantics as `replay_gap`.
    source
        .add_event_listener_with_callback("lagged", on_lagged.as_ref().unchecked_ref())
        .map_err(|_| {
            source.close();
            crate::diagnostics::report_sse_failure(
                vortex_types::BrowserOperation::SseConnect,
                Some(&run_id),
            );
            "could not attach to the run event stream".to_string()
        })?;
    source
        .add_event_listener_with_callback("replay_gap", on_replay_gap.as_ref().unchecked_ref())
        .map_err(|_| {
            source.close();
            crate::diagnostics::report_sse_failure(
                vortex_types::BrowserOperation::SseConnect,
                Some(&run_id),
            );
            "could not attach to the run event stream".to_string()
        })?;

    Ok(RunSubscription {
        source,
        _on_message: on_message,
        _on_error: on_error,
        _on_lagged: on_lagged,
        _on_replay_gap: on_replay_gap,
    })
}

/// Pull the authoritative run snapshot and reconcile the panels with it.
async fn resync_from_server(ctx: &AppCtx, run_id: &str) {
    match crate::api::run_state(run_id).await {
        Ok(snap) => {
            if let Some(run) = snap.run {
                ctx.run_status.set(Some(run.status));
                ctx.plan.set(run.plan);
            }
            let calls: Vec<crate::state::ToolCallDisplay> = snap
                .tool_calls
                .into_iter()
                .map(|t| {
                    // The engine records in-flight calls with ok=false and
                    // summary "running"; don't paint those as failures.
                    let running = !t.ok && t.summary == "running";
                    crate::state::ToolCallDisplay {
                        call_id: t.id,
                        tool: t.tool,
                        args: t.args,
                        status: if running {
                            vortex_types::RunStatus::Running
                        } else if t.ok {
                            vortex_types::RunStatus::Completed
                        } else {
                            vortex_types::RunStatus::Failed
                        },
                        summary: t.summary,
                        diff: None,
                        running,
                    }
                })
                .collect();
            ctx.tool_calls.set(calls);
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
        }
        Err(e) => {
            crate::diagnostics::report_sse_failure(
                vortex_types::BrowserOperation::SseResync,
                Some(run_id),
            );
            ctx.set_toast(format!("lost connection to the run stream ({e})"))
        }
    }
}

fn handle_event(ctx: &AppCtx, event: vortex_types::RunEvent) {
    use vortex_types::RunEvent as E;
    match event {
        E::TextDelta { run_id, delta } => {
            // The root subscription also receives sub-agent deltas (events
            // fan out up the run tree); only the subscribed run's text
            // belongs in the streaming bubble — mixing children in used to
            // interleave unrelated streams.
            if ctx.run_id.get_untracked().as_deref() == Some(run_id.as_str()) {
                ctx.stream_text.update(|t| t.push_str(&delta));
            }
        }
        E::PlanUpdated { steps, .. } => {
            ctx.plan.set(steps);
        }
        E::RunStarted {
            run_id,
            agent,
            model,
            task,
            depth,
            ..
        } => {
            if depth > 0 {
                ctx.agents.update(|a| {
                    a.retain(|x| x.run_id != run_id);
                    a.push(crate::state::AgentDisplay {
                        run_id,
                        agent,
                        task,
                        model,
                        status: vortex_types::RunStatus::Running,
                        usage: None,
                    });
                });
            }
        }
        E::ToolCallStarted {
            call_id,
            tool,
            args,
            ..
        } => {
            ctx.tool_calls.update(|c| {
                c.retain(|x| x.call_id != call_id);
                c.push(crate::state::ToolCallDisplay {
                    call_id,
                    tool,
                    args,
                    status: vortex_types::RunStatus::Running,
                    summary: "running…".into(),
                    diff: None,
                    running: true,
                });
            });
        }
        E::ToolCallFinished {
            call_id,
            ok,
            summary,
            diff,
            ..
        } => {
            ctx.tool_calls.update(|c| {
                if let Some(tc) = c.iter_mut().find(|tc| tc.call_id == call_id) {
                    tc.running = false;
                    tc.summary = summary.clone();
                    tc.diff = diff.clone();
                    tc.status = if ok {
                        vortex_types::RunStatus::Completed
                    } else {
                        vortex_types::RunStatus::Failed
                    };
                } else {
                    // The Started event was missed (stream gap): insert the
                    // finished record instead of dropping it, so the call is
                    // at least visible and not stuck as "running…" elsewhere.
                    c.push(crate::state::ToolCallDisplay {
                        call_id,
                        tool: "(recovered)".into(),
                        args: serde_json::Value::Null,
                        status: if ok {
                            vortex_types::RunStatus::Completed
                        } else {
                            vortex_types::RunStatus::Failed
                        },
                        summary,
                        diff,
                        running: false,
                    });
                }
            });
        }
        E::ApprovalRequested {
            approval_id,
            agent,
            title,
            payload,
            ..
        } => {
            ctx.approvals.update(|a| {
                a.retain(|x| x.id != approval_id);
                a.push(vortex_types::ApprovalInfo {
                    id: approval_id,
                    run_id: String::new(),
                    agent,
                    title,
                    payload,
                    status: vortex_types::ApprovalStatus::Pending,
                    created_at: String::new(),
                });
            });
        }
        E::ApprovalResolved {
            approval_id,
            approved,
            ..
        } => {
            // Remember session-resolved ids so a late poll response cannot
            // resurrect this approval (P12).
            note_approval_resolved(&approval_id);
            ctx.approvals.update(|a| a.retain(|x| x.id != approval_id));
            ctx.set_toast(if approved {
                "Action approved by the user"
            } else {
                "Action declined by the user"
            });
        }
        E::StatusChanged { run_id, status, .. } => {
            // Only the *subscribed* run drives the global status/controls; a
            // sub-agent's status changes (fanned out to the root stream)
            // update the agent panel only. Applying child statuses globally
            // used to hide the Stop button and end the run's live state the
            // moment any sub-agent finished.
            let is_subscribed = ctx.run_id.get_untracked().as_deref() == Some(run_id.as_str());
            if is_subscribed {
                ctx.run_status.set(Some(status));
            }
            ctx.agents.update(|a| {
                for agent in a.iter_mut() {
                    if agent.run_id == run_id {
                        agent.status = status;
                    }
                }
            });
        }
        E::UsageUpdate { run_id, usage, .. } => {
            ctx.agents.update(|a| {
                for agent in a.iter_mut() {
                    if agent.run_id == run_id {
                        agent.usage = Some(usage);
                    }
                }
            });
        }
        E::PreviewStarted { url, .. } => {
            ctx.set_toast(format!("Preview started: {url}"));
        }
        E::RunFinished {
            run_id,
            status,
            error,
            ..
        } => {
            // Same boundary as StatusChanged: a sub-agent finishing must not
            // terminate the subscribed run's UI (clear its streaming bubble,
            // refetch messages, flip the global status).
            if ctx.run_id.get_untracked().as_deref() != Some(run_id.as_str()) {
                ctx.agents.update(|a| {
                    for agent in a.iter_mut() {
                        if agent.run_id == run_id {
                            agent.status = status;
                        }
                    }
                });
                return;
            }
            ctx.run_status.set(Some(status));
            if let Some(err) = error {
                ctx.error_banner.set(Some(err));
            } else {
                ctx.error_banner.set(None);
            }
            if status.is_terminal() {
                let ctx2 = *ctx;
                let current = ctx.current.get_untracked();
                if let Some(conv) = current {
                    spawn_local(async move {
                        if let Ok(msgs) = crate::api::messages(&conv).await {
                            ctx2.messages.set(msgs);
                        }
                        ctx2.stream_text.set(String::new());
                        ctx2.stream_display.set(String::new());
                    });
                } else {
                    ctx.stream_text.set(String::new());
                    ctx.stream_display.set(String::new());
                }
            }
        }
    }
}

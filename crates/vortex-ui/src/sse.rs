//! SSE subscription: web_sys::EventSource driving AppCtx signal updates.
//! The closures are kept alive for the lifetime of the subscription and are
//! released when a new run replaces the source.

use crate::state::AppCtx;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

pub struct RunSubscription {
    pub source: web_sys::EventSource,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: Closure<dyn FnMut()>,
}

pub fn subscribe_run(ctx: AppCtx, run_id: String) -> RunSubscription {
    let source =
        web_sys::EventSource::new(&format!("/api/runs/{run_id}/events")).expect("EventSource constructs");

    let ctx_msg = ctx.clone();
    let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
        move |ev: web_sys::MessageEvent| {
            let Some(text) = ev.data().as_string() else { return };
            let Ok(event) = serde_json::from_str::<vortex_types::RunEvent>(&text) else { return };
            handle_event(&ctx_msg, event);
        },
    );

    let ctx_err = ctx.clone();
    let run_id_err = run_id.clone();
    let on_error = Closure::<dyn FnMut()>::new(move || {
        if let Some(status) = ctx_err.run_status.get_untracked() {
            if !status.is_terminal() {
                let ctx = ctx_err.clone();
                let run_id = run_id_err.clone();
                spawn_local(async move {
                    match crate::api::run_state(&run_id).await {
                        Ok(snap) => {
                            if let Some(run) = snap.run {
                                ctx.run_status.set(Some(run.status));
                            }
                        }
                        Err(_) => ctx.set_toast("lost connection to the run stream"),
                    }
                });
            }
        }
    });

    source
        .add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref())
        .expect("add message listener");
    source
        .add_event_listener_with_callback("error", on_error.as_ref().unchecked_ref())
        .expect("add error listener");

    RunSubscription {
        source,
        _on_message: on_message,
        _on_error: on_error,
    }
}

fn handle_event(ctx: &AppCtx, event: vortex_types::RunEvent) {
    use vortex_types::RunEvent as E;
    match event {
        E::TextDelta { delta, .. } => {
            ctx.stream_text.update(|t| t.push_str(&delta));
        }
        E::PlanUpdated { steps, .. } => {
            ctx.plan.set(steps);
        }
        E::RunStarted { run_id, agent, model, task, depth, .. } => {
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
        E::ToolCallStarted { call_id, tool, args, .. } => {
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
        E::ToolCallFinished { call_id, ok, summary, diff, .. } => {
            ctx.tool_calls.update(|c| {
                for tc in c.iter_mut() {
                    if tc.call_id == call_id {
                        tc.running = false;
                        tc.summary = summary.clone();
                        tc.diff = diff.clone();
                        tc.status = if ok {
                            vortex_types::RunStatus::Completed
                        } else {
                            vortex_types::RunStatus::Failed
                        };
                    }
                }
            });
        }
        E::ApprovalRequested { approval_id, agent, title, payload, .. } => {
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
        E::ApprovalResolved { approval_id, approved, .. } => {
            ctx.approvals.update(|a| a.retain(|x| x.id != approval_id));
            ctx.set_toast(if approved {
                "Action approved by the user"
            } else {
                "Action declined by the user"
            });
        }
        E::StatusChanged { run_id, status, .. } => {
            ctx.run_status.set(Some(status));
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
        E::RunFinished { status, error, .. } => {
            ctx.run_status.set(Some(status));
            if let Some(err) = error {
                ctx.error_banner.set(Some(err));
            } else {
                ctx.error_banner.set(None);
            }
            if status.is_terminal() {
                let ctx2 = ctx.clone();
                let current = ctx.current.get_untracked();
                if let Some(conv) = current {
                    spawn_local(async move {
                        if let Ok(msgs) = crate::api::messages(&conv).await {
                            ctx2.messages.set(msgs);
                        }
                        ctx2.stream_text.set(String::new());
                    });
                } else {
                    ctx.stream_text.set(String::new());
                }
            }
        }
    }
}


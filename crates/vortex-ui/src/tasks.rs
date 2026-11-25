//! Tasks view: plan, tool activity with diffs, sub-agent panel, approvals,
//! previews, and file-checkpoint recovery.

use crate::api;
use crate::state::{AgentDisplay, AppCtx, ToolCallDisplay};
use leptos::prelude::*;
use leptos::task::spawn_local;
use vortex_types::{ApprovalInfo, CheckpointInfo, RunStatus, StepStatus};

#[component]
pub fn TasksView() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <div class="tasks">
            <section class="panel">
                <h3>"Approvals" <span class="panel-note">"consequential actions pause here"</span></h3>
                <Show when=move || ctx.approvals.get().is_empty()>
                    <p class="hint">"Nothing is waiting for your decision."</p>
                </Show>
                <For
                    each=move || ctx.approvals.get()
                    key=|a| a.id.clone()
                    let:approval
                >
                    <ApprovalCard approval />
                </For>
            </section>

            <section class="panel">
                <h3>"Plan" <span class="panel-note">"current steps of the active task"</span></h3>
                <Show when=move || ctx.plan.get().is_empty()>
                    <p class="hint">"No plan published. The assistant shares one via its update_plan tool."</p>
                </Show>
                <ol class="plan">
                    <For
                        each=move || ctx.plan.get()
                        key=|s| s.title.clone()
                        let:step
                    >
                        <li class=match step.status {
                            StepStatus::Done => "step done",
                            StepStatus::InProgress => "step in-progress",
                            StepStatus::Skipped => "step skipped",
                            StepStatus::Pending => "step pending",
                        }>
                            <span class="step-title">{step.title.clone()}</span>
                        </li>
                    </For>
                </ol>
            </section>

            <section class="panel">
                <h3>"Tool activity"</h3>
                <Show when=move || ctx.tool_calls.get().is_empty()>
                    <p class="hint">"No tool calls in this conversation yet."</p>
                </Show>
                <ul class="tool-list">
                    <For
                        each=move || ctx.tool_calls.get()
                        key=|t| t.call_id.clone()
                        let:tc
                    >
                        <ToolCallRow tc />
                    </For>
                </ul>
            </section>

            <section class="panel">
                <h3>"Sub-agents"
                    <span class="panel-note">{
                        move || {
                            let caps = ctx.capabilities.get();
                            format!("up to {} concurrent by default", caps.map(|c| c.max_concurrent_subagents).unwrap_or(6))
                        }
                    }</span>
                </h3>
                <Show when=move || ctx.agents.get().is_empty()>
                    <p class="hint">"No sub-agents have been used in this conversation."</p>
                </Show>
                <div class="agent-grid">
                    <For
                        each=move || ctx.agents.get()
                        key=|a| a.run_id.clone()
                        let:agent
                    >
                        <AgentCard agent />
                    </For>
                </div>
            </section>

            <section class="panel">
                <h3>"Previews"</h3>
                <PreviewList />
            </section>

            <section class="panel">
                <h3>"File checkpoints"
                    <span class="panel-note">"snapshots taken before agent edits — current conversation"</span>
                </h3>
                <CheckpointList />
            </section>
        </div>
    }
}

#[component]
fn ApprovalCard(approval: ApprovalInfo) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <div class="approval-card">
            <div class="approval-head">
                <strong>{approval.title.clone()}</strong>
                <span class="badge">{format!("requested by {}", approval.agent)}</span>
            </div>
            <pre class="approval-payload">{approval.payload.clone()}</pre>
            <div class="approval-actions">
                <button class="btn danger"
                    on:click={
                        let id = approval.id.clone();
                        move |_| {
                            let id = id.clone();
                            spawn_local(async move {
                                if api::decide_approval(&id, false).await.is_ok() {
                                    ctx.approvals.update(|a| a.retain(|x| x.id != id));
                                    ctx.set_toast("Declined");
                                }
                            });
                        }
                    }>"Deny"</button>
                <button class="btn primary"
                    on:click={
                        let id = approval.id.clone();
                        move |_| {
                            let id = id.clone();
                            spawn_local(async move {
                                if api::decide_approval(&id, true).await.is_ok() {
                                    ctx.approvals.update(|a| a.retain(|x| x.id != id));
                                    ctx.set_toast("Approved");
                                }
                            });
                        }
                    }>"Approve"</button>
            </div>
        </div>
    }
}

#[component]
fn ToolCallRow(tc: ToolCallDisplay) -> impl IntoView {
    let has_diff = tc.diff.as_deref().map(|d| !d.is_empty()).unwrap_or(false);
    view! {
        <li class=if tc.running {
            "tool-call running"
        } else if tc.status == RunStatus::Completed {
            "tool-call ok"
        } else {
            "tool-call failed"
        }>
            <details>
                <summary>
                    <span class="tool-name">{tc.tool.clone()}</span>
                    <span class="tool-summary">{tc.summary.clone()}</span>
                </summary>
                <pre class="tool-args">{serde_json::to_string_pretty(&tc.args).unwrap_or_default()}</pre>
                {has_diff.then(|| view! { <pre class="diff">{tc.diff.clone().unwrap_or_default()}</pre> })}
            </details>
        </li>
    }
}

#[component]
fn AgentCard(agent: AgentDisplay) -> impl IntoView {
    view! {
        <div class=match agent.status {
            RunStatus::Completed => "agent-card ok",
            RunStatus::Failed => "agent-card failed",
            RunStatus::Cancelled => "agent-card cancelled",
            _ => "agent-card running",
        }>
            <div class="agent-head">
                <strong>{agent.agent.clone()}</strong>
                <span class="badge status">{agent.status.to_string()}</span>
            </div>
            <p class="agent-task">{agent.task.clone()}</p>
            <p class="agent-meta">{format!("model: {}", agent.model)}</p>
            {agent.usage.map(|u| view! {
                <p class="agent-meta"
                    title=format!("{} input · {} output · {} total", u.prompt_tokens, u.completion_tokens, u.total_tokens)>
                    {format!("{} tokens", grouped(u.total_tokens))}
                </p>
            })}
        </div>
    }
}

/// Thousands-separated count for compact token labels.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[component]
fn PreviewList() -> impl IntoView {
    let previews = RwSignal::new(Vec::new());
    Effect::new(move |_| {
        let p = previews;
        spawn_local(async move {
            if let Ok(list) = api::previews().await {
                p.set(list);
            }
        });
    });
    view! {
        <Show when=move || previews.get().is_empty()>
            <p class="hint">"No previews are running."</p>
        </Show>
        <ul class="preview-list">
            <For
                each=move || previews.get()
                key=|p| p.id.clone()
                let:p
            >
                <li>
                    {
                        // Same scheme allowlist as markdown links: only render
                        // an href for http(s). Preview URLs are server-controlled
                        // today, so this is defense-in-depth.
                        let url = p.url.clone();
                        if url.starts_with("http://") || url.starts_with("https://") {
                            let href = url.clone();
                            view! {
                                <a href=href target="_blank" rel="noreferrer">{url}</a>
                            }.into_any()
                        } else {
                            view! { <span title="blocked link scheme">{url}</span> }.into_any()
                        }
                    }
                    <span class="badge">{if p.running { "running" } else { "stopped" }}</span>
                </li>
            </For>
        </ul>
    }
}

/// Page size for the checkpoint list; a full page enables "Load older".
const CKPT_PAGE: u32 = 20;

/// Load one page of checkpoints. `reset` starts from the newest again;
/// otherwise the stored cursor (`before`) walks one page further back.
async fn load_checkpoints(
    ctx: AppCtx,
    items: RwSignal<Vec<CheckpointInfo>>,
    cursor: RwSignal<Option<i64>>,
    has_more: RwSignal<bool>,
    reset: bool,
) {
    let Some(conv) = ctx.current.get_untracked() else {
        items.set(Vec::new());
        cursor.set(None);
        has_more.set(false);
        return;
    };
    let before = if reset { None } else { cursor.get_untracked() };
    match api::checkpoints(&conv, before, CKPT_PAGE).await {
        Ok(page) => {
            let full = page.len() >= CKPT_PAGE as usize;
            let next = page.last().map(|c| c.id);
            if reset {
                items.set(page);
            } else {
                items.update(|l| l.extend(page));
            }
            cursor.set(next);
            has_more.set(full && next.is_some());
        }
        Err(e) => ctx.set_toast(format!("could not load checkpoints: {e}")),
    }
}

#[component]
fn CheckpointList() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let items: RwSignal<Vec<CheckpointInfo>> = RwSignal::new(Vec::new());
    let cursor: RwSignal<Option<i64>> = RwSignal::new(None);
    let has_more: RwSignal<bool> = RwSignal::new(false);

    // Reload whenever the open conversation changes (RwSignals are Copy).
    Effect::new(move |_| {
        let _ = ctx.current.get();
        spawn_local(async move {
            load_checkpoints(ctx, items, cursor, has_more, true).await;
        });
    });

    view! {
        <Show when=move || ctx.current.get().is_none()>
            <p class="hint">"Open a conversation to see its file checkpoints."</p>
        </Show>
        <Show when=move || ctx.current.get().is_some()>
            <Show when=move || items.get().is_empty()>
                <p class="hint">"No file snapshots yet. Checkpoints are recorded automatically before the agent writes, edits, or deletes a file."</p>
            </Show>
            <ul class="checkpoint-list">
                <For
                    each=move || items.get()
                    key=|c| c.id
                    let:ckpt
                >
                    <CheckpointRow ckpt items cursor has_more />
                </For>
            </ul>
            <Show when=move || has_more.get()>
                <button class="btn"
                    on:click=move |_| {
                        spawn_local(async move {
                            load_checkpoints(ctx, items, cursor, has_more, false).await;
                        });
                    }>"Load older"</button>
            </Show>
        </Show>
    }
}

#[component]
fn CheckpointRow(
    ckpt: CheckpointInfo,
    items: RwSignal<Vec<CheckpointInfo>>,
    cursor: RwSignal<Option<i64>>,
    has_more: RwSignal<bool>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let id = ckpt.id;
    let path = ckpt.path.clone();
    let path_confirm = path.clone();
    view! {
        <li class="checkpoint-row">
            <div class="ckpt-main">
                <code class="ckpt-path" title=path.clone()>{path.clone()}</code>
                <span class="badge">
                    {if ckpt.has_original { "previous version" } else { "new file" }}
                </span>
                <span class="ckpt-time">{ckpt.created_at.clone()}</span>
            </div>
            <button class="btn"
                title="Put the file back to this snapshot (the current content is snapshotted first)"
                on:click=move |_| {
                    let confirmed = web_sys::window()
                        .and_then(|w| {
                            w.confirm_with_message(&format!(
                                "Restore {path_confirm}? The current content is snapshotted first."
                            ))
                            .ok()
                        })
                        .unwrap_or(false);
                    if !confirmed {
                        return;
                    }
                    spawn_local(async move {
                        match api::restore_checkpoint(id).await {
                            Ok(r) => {
                                ctx.set_toast(if r.action == "trashed" {
                                    format!("{} removed — the snapshot recorded that it did not exist", r.path)
                                } else {
                                    format!("Restored {}", r.path)
                                });
                                load_checkpoints(ctx, items, cursor, has_more, true).await;
                            }
                            Err(e) => ctx.set_toast(format!("restore failed: {e}")),
                        }
                    });
                }>"Restore"</button>
        </li>
    }
}

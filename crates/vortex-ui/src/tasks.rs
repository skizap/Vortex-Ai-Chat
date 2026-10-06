//! Tasks view: plan, tool activity with diffs, sub-agent panel, approvals.

use crate::api;
use crate::state::{AgentDisplay, AppCtx, ToolCallDisplay};
use leptos::prelude::*;
use leptos::task::spawn_local;
use vortex_types::{ApprovalInfo, RunStatus, StepStatus};

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
        </div>
    }
}

#[component]
fn ApprovalCard(approval: ApprovalInfo) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let ctx_deny = ctx.clone();
    let ctx_approve = ctx.clone();
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
                        let ctx = ctx.clone();
                        let id = approval.id.clone();
                        move |_| {
                            let ctx = ctx.clone();
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
                        let ctx = ctx.clone();
                        let id = approval.id.clone();
                        move |_| {
                            let ctx = ctx.clone();
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
                <p class="agent-meta">
                    {format!("tokens: {} (prompt {} / completion {})", u.total_tokens, u.prompt_tokens, u.completion_tokens)}
                </p>
            })}
        </div>
    }
}

#[component]
fn PreviewList() -> impl IntoView {
    let previews = RwSignal::new(Vec::new());
    Effect::new(move |_| {
        let p = previews.clone();
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
                    <a href=p.url.clone() target="_blank" rel="noreferrer">{p.url.clone()}</a>
                    <span class="badge">{if p.running { "running" } else { "stopped" }}</span>
                </li>
            </For>
        </ul>
    }
}


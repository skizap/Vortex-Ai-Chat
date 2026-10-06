//! Chat view: message list with safe markdown, streaming bubble,
//! composer with stop and retry.

use crate::api;
use crate::state::AppCtx;
use leptos::prelude::*;
use leptos::task::spawn_local;
use vortex_types::{ChatRequestDto, Role, RunStatus, StoredMessage};

#[component]
pub fn ChatView() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let input = RwSignal::new(String::new());

    let send = move || {
        let text = input.get_untracked();
        if !text.trim().is_empty() {
            input.set(String::new());
            let ctx = ctx;
            let mode = ctx.mode.get_untracked();
            spawn_local(async move {
                crate::chat::send_message(ctx, text, mode).await;
            });
        }
    };

    view! {
        <div class="chat">
            <Show when=move || ctx.current.get().is_none()>
                <EmptyState />
            </Show>
            <Show when=move || ctx.current.get().is_some()>
                <div class="messages">
                    <For
                        each=move || ctx.messages.get()
                        key=|m| m.id
                        let:msg
                    >
                        <MessageBubble msg />
                    </For>
                    <Show when=move || !ctx.stream_text.get().is_empty()>
                        <div class="msg assistant streaming">
                            <div class="msg-body markdown"
                                inner_html=move || crate::markdown::render_markdown(&ctx.stream_text.get())>
                            </div>
                            <span class="cursor">"▊"</span>
                        </div>
                    </Show>
                    <Show when=move || {
                        let running = matches!(ctx.run_status.get(),
                            Some(RunStatus::Running) | Some(RunStatus::Queued) | Some(RunStatus::WaitingApproval));
                        running && ctx.stream_text.get().is_empty()
                    }>
                        <div class="msg assistant">
                            <div class="msg-body"><em class="working">"working…"</em></div>
                        </div>
                    </Show>
                </div>
                <div class="composer">
                    <RunControls />
                    <textarea
                        rows="2"
                        placeholder="Message Vortex… (Enter to send, Shift+Enter for a new line)"
                        prop:value=move || input.get()
                        on:input=move |ev| input.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" && !ev.shift_key() {
                                ev.prevent_default();
                                let text = input.get_untracked();
                                if !text.trim().is_empty() {
                                    input.set(String::new());
                                    let ctx = ctx;
                                    let mode = ctx.mode.get_untracked();
                                    spawn_local(async move {
                                        crate::chat::send_message(ctx, text, mode).await;
                                    });
                                }
                            }
                        }
                    ></textarea>
                    <button class="btn primary" on:click=move |_| send()>"Send"</button>
                </div>
            </Show>
        </div>
    }
}

/// Send a message: optimistic user bubble + POST + SSE subscription.
pub async fn send_message(ctx: AppCtx, text: String, mode: vortex_types::Mode) {
    let Some(conv_id) = ctx.current.get_untracked() else {
        return;
    };
    // Optimistic user bubble.
    ctx.messages.update(|m| {
        m.push(StoredMessage {
            id: -1,
            role: Role::User,
            content: text.clone(),
            tool_calls: Vec::new(),
            created_at: String::new(),
        })
    });
    ctx.stream_text.set(String::new());
    ctx.plan.set(Vec::new());
    ctx.tool_calls.set(Vec::new());
    ctx.agents.set(Vec::new());
    ctx.error_banner.set(None);
    ctx.run_status.set(Some(RunStatus::Queued));
    let dto = ChatRequestDto {
        content: text,
        mode: Some(mode),
    };
    match api::send_chat(&conv_id, &dto).await {
        Ok(r) => start_stream(ctx, r.run_id),
        Err(e) => {
            ctx.run_status.set(None);
            ctx.messages.update(|m| m.retain(|x| x.id != -1));
            ctx.error_banner.set(Some(e));
        }
    }
}

/// Subscribe the global SSE stream to a run (replacing any previous one).
pub fn start_stream(ctx: AppCtx, run_id: String) {
    ctx.run_id.set(Some(run_id.clone()));
    ctx.run_status.set(Some(RunStatus::Running));
    set_new_subscription(crate::sse::subscribe_run(ctx, run_id));
}

thread_local! {
    static SUBSCRIPTION: std::cell::RefCell<Option<crate::sse::RunSubscription>> =
        const { std::cell::RefCell::new(None) };
}

fn set_new_subscription(sub: crate::sse::RunSubscription) {
    SUBSCRIPTION.with(|s| {
        if let Some(old) = s.borrow_mut().replace(sub) {
            old.source.close();
        }
    });
}

#[component]
fn RunControls() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <>
            <Show when=move || {
                matches!(ctx.run_status.get(),
                    Some(RunStatus::Running) | Some(RunStatus::Queued) | Some(RunStatus::WaitingApproval))
            }>
                <button class="btn danger"
                    on:click=move |_| {
                        let ctx = ctx;
                        spawn_local(async move {
                            if let Some(run) = ctx.run_id.get_untracked() {
                                if api::stop_run(&run).await.is_ok() {
                                    ctx.set_toast("Stopping task…");
                                }
                            }
                        });
                    }>"■ Stop"</button>
            </Show>
            <Show when=move || matches!(ctx.run_status.get(), Some(RunStatus::Failed))>
                <button class="btn"
                    on:click=move |_| {
                        let ctx = ctx;
                        spawn_local(async move {
                            if let Some(run) = ctx.run_id.get_untracked() {
                                match api::retry_run(&run).await {
                                    Ok(r) => {
                                        ctx.error_banner.set(None);
                                        ctx.set_toast("Retrying…");
                                        start_stream(ctx, r.run_id);
                                    }
                                    Err(e) => ctx.set_toast(format!("retry failed: {e}")),
                                }
                            }
                        });
                    }>"↻ Retry"</button>
            </Show>
        </>
    }
}

#[component]
fn MessageBubble(msg: StoredMessage) -> impl IntoView {
    let markdown = crate::markdown::render_markdown(&msg.content);
    let raw = msg.content.clone();
    let is_user = msg.role == Role::User;
    view! {
        <div class=if is_user { "msg user" } else { "msg assistant" }>
            <div class="msg-head">
                <span class="who">{if is_user { "You" } else { "Vortex" }}</span>
                <button class="icon-btn copy"
                    title="Copy message"
                    on:click=move |_| copy_text(&raw)>"⧉"</button>
            </div>
            <div class="msg-body markdown" inner_html=markdown></div>
            {(!msg.tool_calls.is_empty()).then(|| view! {
                <details class="tool-calls-in-msg">
                    <summary>{format!("{} tool call(s)", msg.tool_calls.len())}</summary>
                    <ul>
                        {msg.tool_calls.iter().map(|tc| view! {
                            <li><code>{tc.name.clone()}</code></li>
                        }).collect::<Vec<_>>()}
                    </ul>
                </details>
            })}
        </div>
    }
}

fn copy_text(text: &str) {
    if let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) {
        // Fire-and-forget promise; failures are silent (clipboard permission).
        let _ = clipboard.write_text(text);
    }
    if let Some(ctx) = try_use_ctx() {
        ctx.set_toast("Copied");
    }
}

fn try_use_ctx() -> Option<AppCtx> {
    use_context::<AppCtx>()
}

#[component]
fn EmptyState() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let caps = ctx.capabilities; // RwSignal is Copy: avoids moving the struct into the closure
    view! {
        <div class="empty-state">
            <h2>"Welcome to Vortex"</h2>
            <p>"A local, private AI assistant with research, coding, and browser tools — running entirely on your Linux machine."</p>
            <p class="hint">
                {move || {
                    match caps.get() {
                        Some(c) if !c.openrouter => "No OPENROUTER_API_KEY configured yet — see Settings → setup instructions, or start with a normal chat after adding the key.".to_string(),
                        Some(_) => "Create a chat with \u{201c}+ New chat\u{201d} to begin.".to_string(),
                        None => "Connecting to server…".to_string(),
                    }
                }}
            </p>
        </div>
    }
}

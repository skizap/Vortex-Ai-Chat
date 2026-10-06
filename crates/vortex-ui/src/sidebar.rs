//! Sidebar: conversation list with search/rename/delete, new chat, workspace picker.

use crate::api;
use crate::state::AppCtx;
use leptos::prelude::*;
use leptos::task::spawn_local;

#[component]
pub fn Sidebar() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");

    view! {
        <aside class="sidebar">
            <div class="sidebar-head">
                <h1 class="brand">"Vortex"</h1>
                <button class="btn primary"
                    on:click=move |_| {
                        let ctx = ctx;
                        spawn_local(async move {
                            let mode = ctx.mode.get_untracked();
                            match api::create_conversation("New chat", mode).await {
                                Ok(c) => {
                                    ctx.conversations.update(|l| l.insert(0, c.clone()));
                                    ctx.current.set(Some(c.id));
                                    ctx.messages.set(Vec::new());
                                    ctx.tab.set(crate::state::Tab::Chat);
                                }
                                Err(e) => ctx.set_toast(format!("could not create chat: {e}")),
                            }
                        });
                    }>
                    "+ New chat"
                </button>
            </div>
            <input class="search"
                type="search"
                placeholder="Search conversations…"
                prop:value=move || ctx.search.get()
                on:input=move |ev| {
                    let q = event_target_value(&ev);
                    ctx.search.set(q.clone());
                    let ctx = ctx;
                    spawn_local(async move {
                        if let Ok(list) = api::conversations(&q).await {
                            ctx.conversations.set(list);
                        }
                    });
                }
            />
            <ul class="conv-list" role="listbox">
                <For
                    each=move || ctx.conversations.get()
                    key=|c| c.id.clone()
                    let:conv
                >
                    {move || {
                        let selected = ctx.current.get().as_deref() == Some(conv.id.as_str());
                        view! {
                            <li class=if selected { "conv selected" } else { "conv" }>
                                <ConvButton conv_id=conv.id.clone() title=conv.title.clone() mode=conv.mode.as_str() />
                                <button class="icon-btn conv-rename" title="Rename"
                                    on:click={
                                        let ctx = ctx.clone();
                                        let id = conv.id.clone();
                                        let title = conv.title.clone();
                                        move |_| {
                                            let new = web_sys::window()
                                                .and_then(|w| {
                                                    w.prompt_with_message_and_default(
                                                        "Rename conversation",
                                                        &title,
                                                    )
                                                    .ok()
                                                    .flatten()
                                                });
                                            if let Some(new) = new.filter(|s| !s.trim().is_empty()) {
                                                let ctx = ctx.clone();
                                                let id = id.clone();
                                                spawn_local(async move {
                                                    if api::rename_conversation(&id, &new).await.is_ok() {
                                                        ctx.conversations.update(|l| {
                                                            for c in l.iter_mut() {
                                                                if c.id == id { c.title = new.clone(); }
                                                            }
                                                        });
                                                    }
                                                });
                                            }
                                        }
                                    }>"✎"</button>
                                <button class="icon-btn conv-delete" title="Delete"
                                    on:click={
                                        let ctx = ctx.clone();
                                        let id = conv.id.clone();
                                        move |_| {
                                            let ctx = ctx.clone();
                                            let id = id.clone();
                                            spawn_local(async move {
                                                if api::delete_conversation(&id).await.is_ok() {
                                                    ctx.conversations.update(|l| l.retain(|c| c.id != id));
                                                    if ctx.current.get_untracked().as_deref() == Some(id.as_str()) {
                                                        ctx.current.set(None);
                                                        ctx.messages.set(Vec::new());
                                                    }
                                                }
                                            });
                                        }
                                    }>"🗑"</button>
                            </li>
                        }
                    }}
                </For>
            </ul>
            <WorkspacePicker />
        </aside>
    }
}

#[component]
fn ConvButton(conv_id: String, title: String, mode: &'static str) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <button class="conv-open" title=title.clone()
            on:click=move |_| {
                let ctx = ctx;
                ctx.current.set(Some(conv_id.clone()));
                ctx.stream_text.set(String::new());
                ctx.plan.set(Vec::new());
                ctx.tool_calls.set(Vec::new());
                ctx.agents.set(Vec::new());
                ctx.run_status.set(None);
                ctx.error_banner.set(None);
                let ctx = ctx.clone();
                let id = conv_id.clone();
                spawn_local(async move {
                    match api::messages(&id).await {
                        Ok(m) => ctx.messages.set(m),
                        Err(e) => ctx.set_toast(format!("could not load messages: {e}")),
                    }
                    if let Ok(runs) = api::runs_for_conversation(&id).await {
                        if let Some(latest) = runs.first() {
                            if let Ok(snap) = api::run_state(&latest.id).await {
                                let agents = snap.agents;
                                let calls: Vec<crate::state::ToolCallDisplay> = snap
                                    .tool_calls
                                    .into_iter()
                                    .map(|t| crate::state::ToolCallDisplay {
                                        call_id: t.id,
                                        tool: t.tool,
                                        args: t.args,
                                        status: if t.ok {
                                            vortex_types::RunStatus::Completed
                                        } else {
                                            vortex_types::RunStatus::Failed
                                        },
                                        summary: t.summary,
                                        diff: None,
                                        running: false,
                                    })
                                    .collect();
                                ctx.tool_calls.set(calls);
                                ctx.agents.set(agents.into_iter().map(|r| crate::state::AgentDisplay {
                                    run_id: r.id,
                                    agent: r.agent,
                                    task: r.task,
                                    model: r.model,
                                    status: r.status,
                                    usage: r.usage,
                                }).collect());
                                if let Some(run) = snap.run {
                                    ctx.plan.set(run.plan);
                                }
                            }
                        }
                    }
                });
            }>
            <span class="conv-title">{title.clone()}</span>
            <span class="conv-mode">{mode}</span>
        </button>
    }
}

#[component]
fn WorkspacePicker() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <div class="workspace-picker">
            <label for="workspace">"Workspace"</label>
            <input id="workspace"
                type="text"
                placeholder="/home/you/projects/my-site"
                prop:value=move || ctx.settings.get().and_then(|s| s.workspace.clone()).unwrap_or_default()
                on:change=move |ev| {
                    let path = event_target_value(&ev);

                    spawn_local(async move {
                        match api::check_workspace(&path).await {
                            Ok(root) => {
                                if let Some(mut s) = ctx.settings.get_untracked() {
                                    s.workspace = Some(root);
                                    let ctx = ctx.clone();
                                    spawn_local(async move {
                                        match api::save_settings(&s).await {
                                            Ok(saved) => {
                                                ctx.settings.set(Some(saved));
                                                ctx.set_toast("Workspace selected");
                                            }
                                            Err(e) => ctx.set_toast(format!("save failed: {e}")),
                                        }
                                    });
                                }
                            }
                            Err(e) => ctx.set_toast(format!("invalid workspace: {e}")),
                        }
                    });
                }
            />
            <Show when=move || {
                ctx.settings.get().and_then(|s| s.workspace.clone()).is_none()
                    && (ctx.mode.get() == vortex_types::Mode::Coding || ctx.mode.get() == vortex_types::Mode::Browser)
            }>
                <p class="hint">"File tools need a workspace root."</p>
            </Show>
        </div>
    }
}

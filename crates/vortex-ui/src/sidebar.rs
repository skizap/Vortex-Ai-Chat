//! Sidebar: conversation list with search/rename/delete, new chat, workspace picker.

use crate::api;
use crate::state::AppCtx;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashMap;
use vortex_types::RunStatus;

/// Mirror of the server-side cap (`vortex_core::title::MAX_TITLE_CHARS`);
/// the server rejects longer titles, the input enforces it up front.
const MAX_TITLE_CHARS: u32 = 200;

/// Sidebar-local coordination state.
///
/// Titles are read reactively from `ctx.conversations` (rows are keyed by
/// stable id), so a rename — local, from a search refresh, or an automatic
/// server-side rename — never shows a stale captured snapshot. The seq/clock
/// guards keep late-arriving list responses from undoing confirmed renames.
#[derive(Clone, Copy)]
struct SidebarShared {
    /// Id of the conversation whose inline rename editor is open (one at a time).
    editing: RwSignal<Option<String>>,
    /// Editor draft; survives re-renders and failed saves.
    draft: RwSignal<String>,
    /// Sequence of the latest issued list request; only its response applies.
    list_seq: RwSignal<u64>,
    /// Ticks once per confirmed rename.
    rename_clock: RwSignal<u64>,
    /// Confirmed renames: id → (rename_clock tick, canonical title).
    renames: RwSignal<HashMap<String, (u64, String)>>,
}

impl SidebarShared {
    fn new() -> Self {
        Self {
            editing: RwSignal::new(None),
            draft: RwSignal::new(String::new()),
            list_seq: RwSignal::new(0),
            rename_clock: RwSignal::new(0),
            renames: RwSignal::new(HashMap::new()),
        }
    }

    fn is_editing(&self, id: &str) -> bool {
        self.editing.get().as_deref() == Some(id)
    }
}

/// Issue a conversation-list request and apply the response unless a newer
/// request has since been issued. Renames confirmed after this request was
/// issued override the (stale) titles it carries, so a slow search response
/// can never undo a rename. Selection is never touched.
fn fetch_list(ctx: AppCtx, shared: SidebarShared, q: String) {
    shared.list_seq.update(|s| *s += 1);
    let seq = shared.list_seq.get_untracked();
    let issued_at = shared.rename_clock.get_untracked();
    spawn_local(async move {
        let Ok(list) = api::conversations(&q).await else {
            // Keep the current list on failure; background refreshes stay quiet.
            return;
        };
        if shared.list_seq.get_untracked() != seq {
            return;
        }
        let mut list = list;
        if shared.rename_clock.get_untracked() != issued_at {
            let renames = shared.renames.get_untracked();
            for row in list.iter_mut() {
                if let Some((tick, title)) = renames.get(&row.id) {
                    if *tick > issued_at {
                        row.title = title.clone();
                    }
                }
            }
        }
        ctx.conversations.set(list);
    });
}

#[component]
pub fn Sidebar() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let shared = SidebarShared::new();
    provide_context(shared);

    // Summary refresh without changing the selected chat: a run starting
    // means the first turn was just accepted, which may have auto-titled
    // the conversation server-side (the chat POST commits the title before
    // run_id reaches the UI). Silently re-fetch the list; `ctx.current`
    // is untouched.
    let had_run = RwSignal::new(false);
    Effect::new(move || {
        let has_run = ctx.run_id.get().is_some();
        let had = had_run.get_untracked();
        had_run.set(has_run);
        if has_run && !had {
            fetch_list(ctx, shared, ctx.search.get_untracked());
        }
    });

    view! {
        <aside class="sidebar">
            <div class="sidebar-head">
                <h1 class="brand">"Vortex"</h1>
                <button class="btn primary"
                    on:click=move |_| {
                        spawn_local(async move {
                            let mode = ctx.mode.get_untracked();
                            match api::create_conversation("New chat", mode).await {
                                Ok(c) => {
                                    ctx.conversations.update(|l| l.insert(0, c.clone()));
                                    ctx.select_attachment_draft(&c.id);
                                    ctx.run_id.set(None);
                                    ctx.run_status.set(None);
                                    crate::sse::close_subscription();
                                    ctx.stream_text.set(String::new());
                                    ctx.stream_display.set(String::new());
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
                    fetch_list(ctx, shared, q);
                }
            />
            <ul class="conv-list" role="listbox">
                <For
                    each=move || ctx.conversations.get()
                    key=|c| c.id.clone()
                    let:conv
                >
                    {move || {
                        let id = conv.id.clone();
                        let mode = conv.mode.as_str();
                        let selected = ctx.current.get().as_deref() == Some(id.as_str());
                        let rename_btn: NodeRef<leptos::html::Button> = NodeRef::new();
                        // Reactive title: read from the list by id at render
                        // time, never captured from the (possibly stale) row
                        // snapshot the For iteration handed us.
                        let title_now = {
                            let id = id.clone();
                            Signal::derive(move || {
                                ctx.conversations
                                    .get()
                                    .into_iter()
                                    .find(|c| c.id == id)
                                    .map(|c| c.title)
                                    .unwrap_or_default()
                            })
                        };
                        view! {
                            <li class=if selected { "conv selected" } else { "conv" }>
                                {
                                    let id = id.clone();
                                    move || {
                                        if shared.is_editing(&id) {
                                            view! {
                                                <RenameEditor conv_id=id.clone() restore_focus=rename_btn />
                                            }.into_any()
                                        } else {
                                            view! {
                                                <ConvButton conv_id=id.clone() title=title_now mode=mode />
                                            }.into_any()
                                        }
                                    }
                                }
                                <button class="icon-btn conv-rename" node_ref=rename_btn
                                    title="Rename" aria-label="Rename conversation"
                                    on:click={
                                        let id = id.clone();
                                        move |_| {
                                            if shared.is_editing(&id) {
                                                return;
                                            }
                                            // Seed the draft with the CURRENT
                                            // title, read reactively by id.
                                            let current = ctx
                                                .conversations
                                                .get_untracked()
                                                .into_iter()
                                                .find(|c| c.id == id)
                                                .map(|c| c.title)
                                                .unwrap_or_default();
                                            shared.draft.set(current);
                                            shared.editing.set(Some(id.clone()));
                                        }
                                    }>"✎"</button>
                                <button class="icon-btn conv-delete" title="Delete"
                                    on:click={
                                        let id = id.clone();
                                        move |_| {
                                            let id = id.clone();
                                            spawn_local(async move {
                                                match api::delete_conversation(&id).await {
                                                    Ok(()) => {
                                                        ctx.conversations.update(|l| l.retain(|c| c.id != id));
                                                        shared.editing.update(|e| {
                                                            if e.as_deref() == Some(id.as_str()) { *e = None; }
                                                        });
                                                        ctx.clear_attachment_draft(&id);
                                                        ctx.chat_drafts.update(|drafts| { drafts.remove(&id); });
                                                        if ctx.current.get_untracked().as_deref() == Some(id.as_str()) {
                                                            ctx.current.set(None);
                                                            ctx.messages.set(Vec::new());
                                                            ctx.run_id.set(None);
                                                            crate::sse::close_subscription();
                                                        }
                                                    }
                                                    Err(e) => ctx.set_toast(format!("delete failed: {e}")),
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

/// Accessible inline rename editor replacing the native `window.prompt`.
/// Enter saves, Escape cancels, focus returns to the row's ✎ button on
/// close. On failure the draft and editor stay open. On success only the
/// matching row is updated, using the canonical title returned by the
/// server.
#[component]
fn RenameEditor(
    conv_id: String,
    /// The row's ✎ button; receives focus back when the editor closes.
    restore_focus: NodeRef<leptos::html::Button>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let shared = use_context::<SidebarShared>().expect("sidebar shared provided");
    let input_ref: NodeRef<leptos::html::Input> = NodeRef::new();

    // Focus and select the draft once, after the input is mounted (effects
    // run after DOM insertion; nothing tracked here, so this runs once).
    Effect::new(move || {
        if let Some(el) = input_ref.get_untracked() {
            let _ = el.focus();
            el.select();
        }
    });

    let close_and_restore = move || {
        shared.editing.set(None);
        if let Some(el) = restore_focus.get_untracked() {
            let _ = el.focus();
        }
    };

    let save = {
        let conv_id = conv_id.clone();
        Callback::new(move |_| {
            let normalized: String = shared
                .draft
                .get_untracked()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if normalized.is_empty() {
                ctx.set_toast("title must not be empty");
                return; // keep the draft and the editor open
            }
            if normalized.chars().count() > MAX_TITLE_CHARS as usize {
                ctx.set_toast(format!(
                    "title is too long (max {MAX_TITLE_CHARS} characters)"
                ));
                return;
            }
            let id = conv_id.clone();
            spawn_local(async move {
                match api::rename_conversation(&id, &normalized).await {
                    Ok(canonical) => {
                        // Record the confirmed rename BEFORE touching the row,
                        // so list responses issued earlier can never undo it.
                        shared.rename_clock.update(|c| *c += 1);
                        let tick = shared.rename_clock.get_untracked();
                        shared.renames.update(|m| {
                            m.insert(id.clone(), (tick, canonical.clone()));
                        });
                        // Update only the matching row.
                        ctx.conversations.update(|l| {
                            for c in l.iter_mut() {
                                if c.id == id {
                                    c.title = canonical.clone();
                                }
                            }
                        });
                        close_and_restore();
                    }
                    Err(e) => {
                        // Keep draft and editor open so nothing is lost.
                        ctx.set_toast(format!("rename failed: {e}"));
                        if let Some(el) = input_ref.get_untracked() {
                            let _ = el.focus();
                        }
                    }
                }
            });
        })
    };

    let cancel = Callback::new(move |_| close_and_restore());

    view! {
        <div class="conv-rename-editor" role="group" aria-label="Rename conversation">
            <input class="conv-rename-input"
                type="text"
                node_ref=input_ref
                maxlength="200"
                aria-label="Conversation title"
                prop:value=move || shared.draft.get()
                on:input=move |ev| shared.draft.set(event_target_value(&ev))
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    match ev.key().as_str() {
                        "Enter" => {
                            ev.prevent_default();
                            save.run(());
                        }
                        "Escape" => {
                            ev.prevent_default();
                            cancel.run(());
                        }
                        _ => {}
                    }
                }
            />
            <button class="conv-rename-save" type="button"
                title="Save" aria-label="Save title"
                on:click=move |_| save.run(())>"Save"</button>
            <button class="conv-rename-cancel" type="button"
                title="Cancel" aria-label="Cancel rename"
                on:click=move |_| cancel.run(())>"Cancel"</button>
        </div>
    }
}

#[component]
fn ConvButton(conv_id: String, title: Signal<String>, mode: &'static str) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <button class="conv-open" title=move || title.get()
            on:click=move |_| {
                if ctx.current.get_untracked().as_deref() == Some(conv_id.as_str()) {
                    return;
                }
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
                // Clear the active run and close its SSE subscription: Stop/
                // Retry read run_id, and without this they would act on the
                // previous conversation's run (streaming its events into the
                // newly opened conversation's panels).
                ctx.run_id.set(None);
                crate::sse::close_subscription();
                let id = conv_id.clone();
                spawn_local(async move {
                    match api::messages(&id).await {
                        Ok(m) if ctx.current.get_untracked().as_deref() == Some(id.as_str()) => ctx.messages.set(m),
                        Ok(_) => return,
                        Err(e) => ctx.set_toast(format!("could not load messages: {e}")),
                    }
                    if let Ok(runs) = api::runs_for_conversation(&id).await {
                        // Runs come back oldest-first; the latest is last.
                        // Restore the panels from the latest COORDINATOR run
                        // (parent_run_id None) — a newer delegate sub-agent
                        // run must not shadow the conversation's own run.
                        let Some(latest) = runs.iter().rev().find(|r| r.parent_run_id.is_none()) else {
                            return;
                        };
                        let Ok(snap) = api::run_state(&latest.id).await else {
                            return;
                        };
                        // Apply only when this conversation is still selected
                        // and no new run started streaming meanwhile; a late
                        // snapshot must not clobber live panels.
                        if ctx.current.get_untracked().as_deref() != Some(id.as_str()) {
                            return;
                        }
                        if ctx.run_id.get_untracked().is_some() || ctx.chat_sending.get_untracked() {
                            return;
                        }
                        let status = snap.run.as_ref().map(|r| r.status).unwrap_or(latest.status);
                        let live_run_id = snap
                            .run
                            .as_ref()
                            .map(|r| r.id.clone())
                            .unwrap_or_else(|| latest.id.clone());
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
                            ctx.error_banner.set(run.error);
                            ctx.plan.set(run.plan);
                        }
                        // Reopening a conversation whose coordinator run is
                        // still live: resubscribe SSE so streaming and live
                        // session usage resume instead of showing a dead panel.
                        if matches!(
                            status,
                            RunStatus::Queued | RunStatus::Running | RunStatus::WaitingApproval
                        ) {
                            crate::chat::start_stream(ctx, live_run_id);
                        } else {
                            ctx.run_status.set(Some(status));
                            ctx.run_id.set(Some(live_run_id));
                        }
                    }
                });
            }>
            <span class="conv-title">{move || title.get()}</span>
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

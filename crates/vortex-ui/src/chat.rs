//! Chat view: message list with safe markdown, streaming bubble,
//! composer with attachments, stop and retry.

use crate::api;
use crate::state::{AppCtx, UploadChip, UploadState};
use leptos::prelude::*;
use leptos::task::spawn_local;
use vortex_types::chat::MAX_ATTACHMENT_UPLOAD_BYTES;
use vortex_types::dto::ReplyFinish;
use vortex_types::{
    AttachmentInfo, AttachmentKind, AttachmentRef, AttachmentStatus, ChatRequestDto, Role,
    RunStatus, StoredMessage,
};

#[component]
pub fn ChatView() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let input = move || {
        ctx.current
            .get()
            .and_then(|id| ctx.chat_drafts.with(|drafts| drafts.get(&id).cloned()))
            .unwrap_or_default()
    };
    let blocked = move || {
        ctx.chat_sending.get() || ctx.model_saving.get() || turn_active(ctx.run_status.get())
    };
    let file_input: NodeRef<leptos::html::Input> = NodeRef::new();

    // Shared submit path for the Send button and Enter key.
    let submit = move || {
        if untrack(blocked) {
            return;
        }
        let Some(id) = ctx.current.get_untracked() else {
            return;
        };
        let text = untrack(input);
        let chips = ctx.pending_attachments.get_untracked();
        if chips
            .iter()
            .any(|c| matches!(c.state, UploadState::Uploading))
        {
            ctx.set_toast("Waiting for uploads to finish…");
            return;
        }
        let ready: Vec<AttachmentRef> = chips
            .iter()
            .filter_map(|c| match &c.state {
                UploadState::Ready(info) => Some(AttachmentRef {
                    id: info.id.clone(),
                    kind: info.kind,
                    filename: info.filename.clone(),
                    media_type: info.media_type.clone(),
                    status: info.status,
                }),
                _ => None,
            })
            .collect();
        if text.trim().is_empty() && ready.is_empty() {
            return;
        }
        ctx.chat_sending.set(true);
        let mode = ctx.mode.get_untracked();
        spawn_local(async move {
            let accepted = send_message(ctx, &id, text.clone(), mode, ready).await;
            if accepted {
                ctx.chat_drafts.update(|drafts| {
                    if drafts.get(&id) == Some(&text) {
                        drafts.remove(&id);
                    }
                });
                ctx.update_attachment_draft(&id, |pending| {
                    pending.retain(|chip| {
                        !chips.iter().any(|sent| {
                            sent.local_id == chip.local_id
                                && matches!(sent.state, UploadState::Ready(_))
                        })
                    });
                });
            }
            ctx.chat_sending.set(false);
        });
    };

    let submit_key = submit;
    let attach_input = file_input;

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
                    <Show when=move || !ctx.stream_display.get().is_empty()>
                        <div class="msg assistant streaming">
                            <span class="message-model" title="Requested model; serving model not reported yet">{move || {
                                let run = ctx.run_id.get();
                                ctx.run_model.get().filter(|(id, _)| Some(id) == run.as_ref())
                                    .map(|(_, model)| model)
                            }}</span>
                            <div class="msg-body markdown"
                                inner_html=move || crate::markdown::render_markdown(&ctx.stream_display.get())>
                            </div>
                            <span class="cursor">"▊"</span>
                        </div>
                    </Show>
                    <Show when=move || {
                        let running = matches!(ctx.run_status.get(),
                            Some(RunStatus::Running) | Some(RunStatus::Queued) | Some(RunStatus::WaitingApproval));
                        running && ctx.stream_display.get().is_empty()
                    }>
                        <div class="msg assistant">
                            <div class="msg-body"><em class="working">"working…"</em></div>
                        </div>
                    </Show>
                </div>
                <div class="composer"
                    on:dragover=move |ev| { ev.prevent_default(); }
                    on:drop=move |ev| {
                        ev.prevent_default();
                        if let Some(files) = ev.data_transfer().and_then(|dt| dt.files()) {
                            start_uploads(ctx, files);
                        }
                    }
                >
                    <div class="composer-options">
                      <div class="composer-meta">
                        <crate::model_picker::ModelPicker />
                        <crate::session_usage::UsageBadge />
                        <RunControls />
                      </div>
                      <Show when=move || !ctx.pending_attachments.get().is_empty()>
                        <div class="composer-chips">
                            <For
                                each=move || ctx.pending_attachments.get()
                                key=|c| (c.local_id, matches!(c.state, UploadState::Uploading))
                                let:chip
                            >
                                <ChipView chip />
                            </For>
                        </div>
                        <p class="attachment-notice">"Uploads stay local until you send. Sending shares eligible files with the configured model provider."</p>
                      </Show>
                    </div>
                    <div class="composer-row">
                        <button class="icon-btn attach" aria-label="Attach files" title="Attach images, videos, PDFs, or text files"
                            on:click=move |_| {
                                if let Some(el) = attach_input.get() { el.click(); }
                            }>"📎"</button>
                        <input node_ref=file_input type="file" multiple
                            accept=".png,.jpg,.jpeg,.jpe,.jfif,.webp,.gif,.bmp,.tiff,.tif,.ico,.mp4,.m4v,.webm,.mov,.mpeg,.mpg,.mpe,.pdf,.txt,.md,.csv,.json,.xml,.yaml,.yml,.toml,.log,image/png,image/jpeg,image/webp,image/gif,image/bmp,image/tiff,image/vnd.microsoft.icon,video/mp4,video/webm,video/quicktime,video/mpeg,application/pdf,text/*"
                            style="display:none"
                            on:change=move |ev| {
                                let target = event_target::<web_sys::HtmlInputElement>(&ev);
                                if let Some(files) = target.files() {
                                    start_uploads(ctx, files);
                                }
                                if let Some(el) = file_input.get() { el.set_value(""); }
                            }
                        />
                        <textarea
                            rows="2"
                            placeholder="Message Vortex… (Enter to send, Shift+Enter for a new line)"
                            prop:value=input
                            on:input=move |ev| {
                                if let Some(id) = ctx.current.get_untracked() {
                                    ctx.chat_drafts.update(|drafts| { drafts.insert(id, event_target_value(&ev)); });
                                }
                            }
                            on:keydown=move |ev| {
                                if ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing() {
                                    ev.prevent_default();
                                    submit_key();
                                }
                            }
                        ></textarea>
                        <button class="btn primary" disabled=blocked on:click=move |_| submit()>"Send"</button>
                    </div>
                </div>
            </Show>
        </div>
    }
}

fn turn_active(status: Option<RunStatus>) -> bool {
    matches!(
        status,
        Some(RunStatus::Running | RunStatus::Queued | RunStatus::WaitingApproval)
    )
}

thread_local! {
    static CHIP_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn next_chip_id() -> u64 {
    CHIP_ID.with(|c| {
        let next = c.get() + 1;
        c.set(next);
        next
    })
}

/// Leave format validation and per-kind size limits to the upload endpoint.
fn precheck(size: u64) -> Result<(), String> {
    if size == 0 {
        return Err("File is empty.".into());
    }
    if size > MAX_ATTACHMENT_UPLOAD_BYTES as u64 {
        let mib = MAX_ATTACHMENT_UPLOAD_BYTES / (1024 * 1024);
        return Err(format!(
            "Files must be at most {mib} MiB. Trim or compress large videos."
        ));
    }
    Ok(())
}

/// Stage files as chips. Separate requests keep each file within the upload
/// body limit even when their combined size exceeds it.
fn start_uploads(ctx: AppCtx, files: web_sys::FileList) {
    let Some(conv) = ctx.current.get_untracked() else {
        return;
    };
    let mut chips = ctx.pending_attachments.get_untracked();
    let mut accepted: Vec<(u64, web_sys::File)> = Vec::new();
    for i in 0..files.length() {
        if chips.len() >= 8 {
            ctx.set_toast("At most eight files per message");
            break;
        }
        let Some(file) = files.get(i) else { continue };
        let name = file.name();
        let size = file.size() as u64;
        let local_id = next_chip_id();
        match precheck(size) {
            Err(msg) => chips.push(UploadChip {
                local_id,
                filename: name,
                size_bytes: size,
                state: UploadState::Error(msg),
            }),
            Ok(()) => {
                chips.push(UploadChip {
                    local_id,
                    filename: name.clone(),
                    size_bytes: size,
                    state: UploadState::Uploading,
                });
                accepted.push((local_id, file));
            }
        }
    }
    ctx.pending_attachments.set(chips);
    spawn_local(async move {
        for (id, file) in accepted {
            if !ctx.has_attachment_chip(&conv, id) {
                continue;
            }
            let result = api::upload_attachment(&conv, &file).await;
            ctx.update_attachment_draft(&conv, |chips| {
                if let Some(c) = chips.iter_mut().find(|c| c.local_id == id) {
                    c.state = match result {
                        Ok(info) => {
                            c.filename.clone_from(&info.filename);
                            c.size_bytes = info.size_bytes;
                            UploadState::Ready(info)
                        }
                        Err(error) => UploadState::Error(error),
                    };
                }
            });
        }
    });
}

#[component]
fn ChipView(chip: UploadChip) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let local_id = chip.local_id;
    let filename = chip.filename.clone();
    let state = chip.state.clone();
    let status_icon = match &state {
        UploadState::Uploading => "…",
        UploadState::Ready(_) => "✓",
        UploadState::Error(_) => "⚠",
    };
    let title = match &state {
        UploadState::Error(e) => e.clone(),
        _ => format!("{} ({})", filename, human_size(chip.size_bytes)),
    };
    let class = match &state {
        UploadState::Error(_) => "chip error",
        UploadState::Uploading => "chip uploading",
        UploadState::Ready(_) => "chip ready",
    };
    let error = match state {
        UploadState::Error(error) => Some(error),
        _ => None,
    };
    view! {
        <span class=class title=title>
            <span class="chip-status">{status_icon}</span>
            <span class="chip-name">{filename}</span>
            {error.map(|error| view! { <span class="chip-error-message" role="status">{error}</span> })}
            <button class="chip-x" title="Remove"
                on:click=move |_| {
                    ctx.pending_attachments.update(|v| v.retain(|c| c.local_id != local_id));
                }>"×"</button>
        </span>
    }
}

// Optimistic user bubbles need locally-unique negative ids: `<For>` keys on
// `id`, so a shared `-1` sentinel made two rapid sends collide (duplicate
// keys) and error rollback removed *all* optimistic bubbles, not just the
// failed one. Ids decrement per send (WASM is single-threaded, so a
// thread-local counter is enough).
thread_local! {
    static OPTIMISTIC_ID: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

fn next_optimistic_id() -> i64 {
    OPTIMISTIC_ID.with(|c| {
        let next = c.get() - 1;
        c.set(next);
        next
    })
}

/// Send a message: optimistic user bubble + POST + SSE subscription.
async fn send_message(
    ctx: AppCtx,
    conv_id: &str,
    text: String,
    mode: vortex_types::Mode,
    attachments: Vec<AttachmentRef>,
) -> bool {
    if ctx.current.get_untracked().as_deref() != Some(conv_id) {
        return false;
    }
    let attachment_ids: Vec<String> = attachments.iter().map(|a| a.id.clone()).collect();
    ctx.run_id.set(None);
    crate::sse::close_subscription();
    // Optimistic user bubble.
    let optimistic_id = next_optimistic_id();
    ctx.messages.update(|m| {
        m.push(StoredMessage {
            id: optimistic_id,
            role: Role::User,
            content: text.clone(),
            tool_calls: Vec::new(),
            attachments: attachments.clone(),
            created_at: String::new(),
            run_id: None,
            requested_model: None,
            serving_model: None,
            reply_summary: None,
        })
    });
    ctx.stream_text.set(String::new());
    ctx.stream_display.set(String::new());
    ctx.plan.set(Vec::new());
    ctx.tool_calls.set(Vec::new());
    ctx.agents.set(Vec::new());
    ctx.error_banner.set(None);
    ctx.run_status.set(Some(RunStatus::Queued));
    let dto = ChatRequestDto {
        content: text,
        mode: Some(mode),
        attachment_ids,
    };
    let result = api::send_chat(conv_id, &dto).await;
    if ctx.current.get_untracked().as_deref() != Some(conv_id) {
        return result.is_ok();
    }
    match result {
        Ok(r) => {
            start_stream(ctx, r.run_id);
            true
        }
        Err(e) => {
            ctx.run_status.set(None);
            // Roll back only *this* send's bubble.
            ctx.messages.update(|m| m.retain(|x| x.id != optimistic_id));
            ctx.error_banner.set(Some(e));
            false
        }
    }
}

/// Subscribe the global SSE stream to a run (replacing any previous one).
pub fn start_stream(ctx: AppCtx, run_id: String) {
    ctx.run_id.set(Some(run_id.clone()));
    ctx.run_status.set(Some(RunStatus::Running));
    match crate::sse::subscribe_run(ctx, run_id) {
        Ok(sub) => crate::sse::set_new_subscription(sub),
        Err(e) => {
            // Environmental failure (e.g. browser connection limits): show a
            // banner instead of trapping the WASM module.
            ctx.run_status.set(None);
            ctx.error_banner
                .set(Some(format!("could not subscribe to run events: {e}")));
        }
    }
}

#[component]
fn RunControls() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let can_continue = move || {
        !turn_active(ctx.run_status.get())
            && ctx.messages.with(|messages| {
                messages.last().is_some_and(|message| {
                    message.role == Role::Assistant
                        && !message.content.is_empty()
                        && message.reply_summary.as_ref().is_none_or(|summary| {
                            matches!(
                                summary.finish,
                                ReplyFinish::Stop | ReplyFinish::Length | ReplyFinish::NotReported
                            )
                        })
                })
            })
    };
    let last_hit_limit = move || {
        ctx.messages.with(|messages| {
            messages
                .last()
                .and_then(|message| message.reply_summary.as_ref())
                .is_some_and(|summary| summary.finish == ReplyFinish::Length)
        })
    };
    view! {
        <>
            <Show when=can_continue>
                <button class="btn" title="Send a new turn asking the model to continue"
                    disabled=move || ctx.model_saving.get() || ctx.chat_sending.get()
                    on:click=move |_| {
                        if !untrack(can_continue) || ctx.chat_sending.get_untracked() || ctx.model_saving.get_untracked() {
                            return;
                        }
                        let Some(id) = ctx.current.get_untracked() else { return; };
                        let mode = ctx.mode.get_untracked();
                        ctx.chat_sending.set(true);
                        spawn_local(async move {
                            send_message(ctx, &id, "Continue from where you left off without repeating the previous content.".into(), mode, Vec::new()).await;
                            ctx.chat_sending.set(false);
                        });
                    }>"Continue"</button>
            </Show>
            {move || match ctx.run_status.get() {
                Some(RunStatus::Failed) if !last_hit_limit() => Some("Run failed"),
                Some(RunStatus::Cancelled) => Some("Run cancelled"),
                Some(RunStatus::Interrupted) => Some("Run interrupted"),
                _ => None,
            }.map(|status| view! { <span role="status">{status}</span> })}
            <Show when=move || {
                matches!(ctx.run_status.get(),
                    Some(RunStatus::Running) | Some(RunStatus::Queued) | Some(RunStatus::WaitingApproval))
            }>
                <button class="btn danger"
                    on:click=move |_| {
                        let ctx = ctx;
                        spawn_local(async move {
                            if let Some(run) = ctx.run_id.get_untracked() {
                                match api::stop_run(&run).await {
                                    Ok(()) => ctx.set_toast("Stopping task…"),
                                    Err(e) => ctx.set_toast(format!("stop failed: {e}")),
                                }
                            }
                        });
                    }>"■ Stop"</button>
            </Show>
            <Show when=move || matches!(ctx.run_status.get(), Some(RunStatus::Failed)) && !last_hit_limit() && ctx.run_id.get().is_some()>
                <button class="btn"
                    disabled=move || ctx.model_saving.get() || ctx.chat_sending.get()
                    on:click=move |_| {
                        spawn_local(async move {
                            if let Some(run) = ctx.run_id.get_untracked() {
                                let conversation = ctx.current.get_untracked();
                                ctx.chat_sending.set(true);
                                let result = api::retry_run(&run).await;
                                ctx.chat_sending.set(false);
                                if ctx.current.get_untracked() != conversation || ctx.run_id.get_untracked().as_ref() != Some(&run) {
                                    return;
                                }
                                match result {
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
    let raw_is_empty = raw.is_empty();
    let is_user = msg.role == Role::User;
    let attachments = msg.attachments.clone();
    let model = if is_user {
        None
    } else {
        match (msg.serving_model, msg.requested_model) {
            (Some(serving), _) => Some((serving, String::new())),
            (None, Some(requested)) => Some((
                requested,
                "Requested model; serving model not reported".into(),
            )),
            (None, None) => None,
        }
    };
    view! {
        <div class=if is_user { "msg user" } else { "msg assistant" }>
            <div class="msg-head">
                <span class="who">{if is_user { "You" } else { "Vortex" }}</span>
                {model.map(|(label, tip)| view! { <span class="message-model" title=tip>{label}</span> })}
                <button class="icon-btn copy"
                    title="Copy message"
                    on:click=move |_| copy_text(&raw)>"⧉"</button>
            </div>
            <div class="msg-body markdown" inner_html=markdown></div>
            {msg.reply_summary.as_ref().filter(|_| !is_user).and_then(|summary| {
                let (label, class) = match summary.finish {
                    ReplyFinish::Length if raw_is_empty => ("Output limit reached — no text returned", "msg-flag warn"),
                    ReplyFinish::Length => ("Output limit reached — partial reply", "msg-flag warn"),
                    ReplyFinish::ContentFilter => ("Stopped by content filter", "msg-flag warn"),
                    ReplyFinish::Error => ("Provider generation error", "msg-flag error"),
                    _ => return None,
                };
                let usage = summary.completion_tokens.map(|count| count.to_string()).unwrap_or_else(|| "not reported".into());
                let tip = format!("Requested output allowance: {} tokens · Reported completion tokens: {usage}", summary.requested_output_tokens);
                Some(view! { <span class=class role="status" title=tip>{label}</span> })
            })}
            {(!attachments.is_empty()).then(|| view! {
                <div class="attachments">
                    {attachments.iter().map(|a| view! { <AttachmentView att=a.clone() /> }).collect::<Vec<_>>()}
                </div>
            })}
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

/// Renders one attachment by kind, fetching its metadata to detect deletion
/// (tombstone), size, and video job status.
#[component]
fn AttachmentView(att: AttachmentRef) -> impl IntoView {
    let state = RwSignal::new(None::<Result<Option<AttachmentInfo>, String>>);
    let loading = RwSignal::new(false);
    let url = api::attachment_url(&att.id);
    let kind = att.kind;
    let fetch = move || {
        if loading.get_untracked() {
            return;
        }
        loading.set(true);
        let id = att.id.clone();
        spawn_local(async move {
            let result = api::attachment_meta(&id).await;
            let _ = state.try_set(Some(result));
            let _ = loading.try_set(false);
        });
    };
    fetch();
    if kind == AttachmentKind::Video {
        if let Ok(handle) = set_interval_with_handle(
            move || {
                if !matches!(state.get_untracked(), Some(Ok(None)))
                    && !matches!(state.get_untracked(), Some(Ok(Some(m))) if m.status != AttachmentStatus::Pending)
                {
                    fetch();
                }
            },
            std::time::Duration::from_secs(15),
        ) {
            on_cleanup(move || handle.clear());
        }
    }
    move || match state.get() {
        None => {
            view! { <span class="att-chip" role="status">"Loading attachment…"</span> }.into_any()
        }
        Some(Ok(None)) => {
            view! { <span class="att-tombstone">"Attachment removed"</span> }.into_any()
        }
        Some(Err(_)) => {
            view! { <span class="att-tombstone">"Could not load attachment"</span> }.into_any()
        }
        Some(Ok(Some(meta))) => {
            let url = url.clone();
            match meta.status {
                AttachmentStatus::Pending => view! { <div class="att-video pending" role="status"><span class="spinner" aria-hidden="true"></span>" Video pending"</div> }.into_any(),
                AttachmentStatus::Failed => view! { <div class="att-video failed">{meta.error.unwrap_or_else(|| "Attachment generation failed".into())}</div> }.into_any(),
                AttachmentStatus::Ready => match kind {
                    AttachmentKind::Image => view! { <ImageThumb url filename=meta.filename /> }.into_any(),
                    AttachmentKind::Video => view! {
                        <div class="att-video-wrap">
                            <video class="att-video" controls preload="metadata" src=url.clone()></video>
                            <FileChip meta url />
                        </div>
                    }.into_any(),
                    _ => view! { <FileChip meta url /> }.into_any(),
                },
            }
        }
    }
}

#[component]
fn ImageThumb(url: String, filename: String) -> impl IntoView {
    let open = RwSignal::new(false);
    let failed = RwSignal::new(false);
    let full = url.clone();
    let dl = url.clone();
    let alt = filename.clone();
    view! {
        <Show when=move || !failed.get()>
            <button class="att-thumb-button" title="Open image" on:click=move |_| open.set(true)>
                <img class="att-thumb" src=url.clone() loading="lazy" alt=alt.clone()
                    on:error=move |_| failed.set(true) />
            </button>
        </Show>
        <Show when=move || failed.get()>
            <div class="att-tombstone">"image unavailable"</div>
        </Show>
        <Show when=move || open.get()>
            <div class="lightbox" on:click=move |_| open.set(false)>
                <div class="lightbox-inner" role="dialog" aria-modal="true" aria-label="Image preview"
                    on:click=move |ev| { ev.stop_propagation(); }>
                    <img src=full.clone() alt=filename.clone() />
                    <div class="lightbox-actions">
                        <a class="btn" href=dl.clone() download>"Download"</a>
                        <button class="btn" on:click=move |_| open.set(false)>"Close"</button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[component]
fn FileChip(meta: AttachmentInfo, url: String) -> impl IntoView {
    let icon = match meta.kind {
        AttachmentKind::Pdf => "PDF",
        AttachmentKind::Video => "Video",
        _ => "Text",
    };
    let dl_name = meta.filename.clone();
    view! {
        <div class="att-chip">
            <span class="att-icon">{icon}</span>
            <span class="att-name">{meta.filename}</span>
            <span class="att-size">{human_size(meta.size_bytes)}</span>
            <a class="att-dl" href=url.clone() download=dl_name title="Download">"⬇"</a>
        </div>
    }
}

fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let b = bytes as f64;
    if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn copy_text(text: &str) {
    let Some(ctx) = try_use_ctx() else {
        return;
    };
    let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) else {
        ctx.set_toast("Clipboard is not available in this browser");
        return;
    };
    // Await the promise: claiming "Copied" while the write was denied by
    // permissions policy is a lie.
    let promise = clipboard.write_text(text);
    spawn_local(async move {
        match wasm_bindgen_futures::JsFuture::from(promise).await {
            Ok(_) => ctx.set_toast("Copied"),
            Err(_) => ctx.set_toast("Clipboard write was blocked by the browser"),
        }
    });
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

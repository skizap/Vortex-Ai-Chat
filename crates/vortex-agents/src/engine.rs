//! The Vortex engine: agent loop, tool dispatch, delegation, budgets.
//!
//! One `execute_run` drives a single run (coordinator or sub-agent):
//! loop { model call → text deltas → tool calls (validated, permission-checked,
//! executed concurrently) → tool results } under iteration/time/token budgets
//! with cancellation, honest statuses, and full event streaming.

use crate::approvals::ApprovalManager;
use crate::browser::BrowserManager;
use crate::errors::ToolError;
use crate::file_state::FileState;
use crate::hub::Hub;
use crate::media_context::{self, MediaContext, MediaSupport, ModelCatalog, PdfReplay};
use crate::preview::PreviewRegistry;
use crate::registry::{ToolContext, ToolRegistry};
use crate::safe_http::SafeHttp;
use crate::scheduler::Scheduler;
use crate::search::SearchProvider;
use futures::future::join_all;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vortex_core::diag;
use vortex_core::{AttachmentStore, Db};
use vortex_llm::llm::PdfEngine;
use vortex_llm::{CompletionRequest, LlmClient, LlmEvent};
use vortex_types::{
    ChatMessage, Mode, RunEvent, RunInfo, RunStatus, Settings, ToolCallRecord, Usage,
};

pub struct Engine {
    pub llm: Arc<dyn LlmClient>,
    pub registry: Arc<ToolRegistry>,
    pub hub: Arc<Hub>,
    pub scheduler: Arc<Scheduler>,
    pub approvals: Arc<ApprovalManager>,
    pub db: Db,
    pub browser: Arc<BrowserManager>,
    pub previews: Arc<PreviewRegistry>,
    pub search: Option<Arc<dyn SearchProvider>>,
    pub http: SafeHttp,
    pub file_state: Arc<FileState>,
    attachments: Option<Arc<AttachmentStore>>,
    catalog: ModelCatalog,
    settings: tokio::sync::RwLock<Settings>,
    /// Engine self-reference for the delegate tool (set once after Arc::new).
    pub self_ref: Arc<OnceLock<Arc<Engine>>>,
}

/// Which tools a mode exposes, after applying the user's toggles and the
/// runtime capability detection. The registry still validates everything.
pub fn allowed_for_mode(mode: Mode, settings: &Settings, browser_ok: bool) -> HashSet<String> {
    let mut set = HashSet::new();
    set.insert("update_plan".to_string());
    set.insert("request_approval".to_string());
    let delegation = settings.tools.delegation;
    let add = |set: &mut HashSet<String>, name: &str| {
        set.insert(name.to_string());
    };
    match mode {
        Mode::Chat => {}
        Mode::Research => {
            if settings.tools.search {
                add(&mut set, "web_search");
                add(&mut set, "fetch_url");
            }
            if delegation {
                add(&mut set, "delegate_subtask");
            }
        }
        Mode::Coding => {
            if settings.tools.files {
                for t in [
                    "list_files",
                    "read_file",
                    "write_file",
                    "edit_file",
                    "search_files",
                    "delete_path",
                ] {
                    add(&mut set, t);
                }
            }
            if settings.tools.commands {
                add(&mut set, "run_command");
            }
            if settings.tools.previews {
                add(&mut set, "start_preview");
                add(&mut set, "stop_preview");
            }
            if delegation {
                add(&mut set, "delegate_subtask");
            }
        }
        Mode::Browser => {
            if settings.tools.browser && browser_ok {
                for t in [
                    "browser_navigate",
                    "browser_read",
                    "browser_click",
                    "browser_type",
                ] {
                    add(&mut set, t);
                }
            }
            if settings.tools.search {
                add(&mut set, "fetch_url");
            }
            if delegation {
                add(&mut set, "delegate_subtask");
            }
        }
    }
    set
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

pub struct RunSpec {
    pub run_id: String,
    pub root_run_id: String,
    pub conversation_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent: String,
    pub task: String,
    pub depth: u32,
    pub allowed: HashSet<String>,
    pub messages: Vec<ChatMessage>,
    pub model: String,
    pub settings: Settings,
    pub cancel: CancellationToken,
    pub is_root: bool,
    pub needs_slot: bool,
}

/// Settings and model input support captured before accepting a turn.
pub struct ChatPreflight {
    settings: Settings,
    support: Option<MediaSupport>,
}

#[derive(Default)]
struct ChatRunMedia {
    support: Option<MediaSupport>,
    assistant_ids: std::collections::HashMap<i64, usize>,
}

impl Engine {
    /// Composition root: every subsystem is a shared handle. Arguments are
    /// grouped into [`Engine`] by design (single composition point).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        llm: Arc<dyn LlmClient>,
        registry: Arc<ToolRegistry>,
        hub: Arc<Hub>,
        scheduler: Arc<Scheduler>,
        approvals: Arc<ApprovalManager>,
        db: Db,
        browser: Arc<BrowserManager>,
        previews: Arc<PreviewRegistry>,
        search: Option<Arc<dyn SearchProvider>>,
        http: SafeHttp,
        settings: Settings,
        attachments: Option<Arc<AttachmentStore>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            llm,
            registry,
            hub,
            scheduler,
            approvals,
            db,
            browser,
            previews,
            search,
            http,
            file_state: Arc::new(FileState::new()),
            attachments,
            catalog: ModelCatalog::default(),
            settings: tokio::sync::RwLock::new(settings),
            self_ref: Arc::new(OnceLock::new()),
        })
    }

    pub async fn current_settings(&self) -> Settings {
        self.settings.read().await.clone()
    }

    pub async fn models(&self) -> Result<Arc<Vec<vortex_types::ModelInfo>>, vortex_llm::LlmError> {
        self.catalog.get(self.llm.as_ref()).await
    }

    pub async fn preflight_chat(
        &self,
        conversation_id: &str,
        attachments: &[vortex_types::AttachmentRef],
    ) -> Result<ChatPreflight, &'static str> {
        let settings = self.current_settings().await;
        let support = match &self.attachments {
            Some(store) => {
                media_context::preflight(
                    store,
                    &self.catalog,
                    self.llm.as_ref(),
                    &settings.model,
                    conversation_id,
                    attachments,
                )
                .await?
            }
            None => None,
        };
        Ok(ChatPreflight { settings, support })
    }

    pub async fn update_settings(&self, settings: Settings) {
        self.scheduler.set_limit(settings.agents.max_concurrent);
        self.settings.write().await.clone_from(&settings);
    }

    fn build_context(&self, spec: &RunSpec) -> (ToolContext, mpsc::Receiver<RunEvent>) {
        let (tx, rx) = mpsc::channel::<RunEvent>(256);
        let workspace = spec
            .settings
            .workspace
            .as_deref()
            .and_then(|p| crate::workspace::WorkspaceRoots::new(std::path::Path::new(p)).ok())
            .map(Arc::new);
        let ctx = ToolContext {
            run_id: spec.run_id.clone(),
            root_run_id: spec.root_run_id.clone(),
            depth: spec.depth,
            settings: spec.settings.clone(),
            allowed: spec.allowed.clone(),
            events: tx,
            cancel: spec.cancel.clone(),
            approvals: self.approvals.clone(),
            workspace,
            db: Some(self.db.clone()),
            attachments: self.attachments.clone(),
            conversation_id: spec.conversation_id.clone(),
            file_state: self.file_state.clone(),
            http: self.http.clone(),
            search: self.search.clone(),
            browser: Some(self.browser.clone()),
            previews: self.previews.clone(),
        };
        (ctx, rx)
    }

    /// Drive one run to completion. Returns the final assistant text.
    pub async fn execute_run(self: &Arc<Self>, spec: RunSpec) -> Result<String, RunError> {
        self.execute_with_media(spec, ChatRunMedia::default()).await
    }

    async fn execute_with_media(
        self: &Arc<Self>,
        spec: RunSpec,
        media: ChatRunMedia,
    ) -> Result<String, RunError> {
        let run_id = spec.run_id.clone();
        let settings = spec.settings.clone();
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(settings.agents.run_timeout_secs.max(30));

        // Sub-agents queue for a scheduler slot; the cap is global and the
        // coordinator never consumes one.
        let slot = if spec.needs_slot {
            self.hub.publish(
                &run_id,
                RunEvent::StatusChanged {
                    run_id: run_id.clone(),
                    status: RunStatus::Queued,
                    detail: Some("waiting for a free sub-agent slot".into()),
                },
            );
            tokio::select! {
                slot = self.scheduler.acquire() => Some(slot),
                _ = spec.cancel.cancelled() => {
                    self.finish(&run_id, RunStatus::Cancelled, None).await;
                    return Err(RunError::Cancelled);
                }
            }
        } else {
            None
        };

        self.hub.publish(
            &run_id,
            RunEvent::RunStarted {
                run_id: run_id.clone(),
                parent_run_id: spec.parent_run_id.clone(),
                agent: spec.agent.clone(),
                model: spec.model.clone(),
                task: spec.task.clone(),
                depth: spec.depth,
            },
        );
        self.hub.set_status(&run_id, RunStatus::Running);
        self.persist_status(&run_id, RunStatus::Running, None).await;

        let (ctx, mut tool_events) = self.build_context(&spec);
        // Forward tool-emitted events (plan updates, previews) to subscribers.
        let fwd = {
            let hub = self.hub.clone();
            let run_id = run_id.clone();
            tokio::spawn(async move {
                while let Some(ev) = tool_events.recv().await {
                    hub.publish(&run_id, ev);
                }
            })
        };

        let result = self.run_loop(&spec, ctx, deadline, media).await;
        // Drain instead of abort: `run_loop` owns `ctx` and every tool task is
        // joined inside `execute_tool_calls`, so all `ctx.events` senders are
        // dropped by the time `run_loop` returns — the forwarder delivers
        // queued tool events and exits on its own, guaranteeing `finish()`
        // publishes `RunFinished` strictly last. The timeout bounds the drain
        // in case a future change ever lets a `ToolContext` clone outlive the
        // run loop (nothing does today).
        let mut fwd = fwd;
        if tokio::time::timeout(std::time::Duration::from_secs(2), &mut fwd)
            .await
            .is_err()
        {
            tracing::warn!("tool-event forwarder did not drain within 2s; aborting it");
            diag::event(
                tracing::Level::WARN,
                diag::RUN_EVENT_FORWARDER_TIMEOUT,
                &[("run_id", diag::Field::Uuid(run_id.as_str()))],
            );
            fwd.abort();
        }

        match &result {
            Ok(_) => self.finish(&run_id, RunStatus::Completed, None).await,
            Err(RunError::Cancelled) => self.finish(&run_id, RunStatus::Cancelled, None).await,
            Err(RunError::Failed(msg)) => {
                self.finish(&run_id, RunStatus::Failed, Some(msg.clone()))
                    .await
            }
        }
        // Cleanup owned resources regardless of outcome.
        self.previews.stop_for_run(&run_id);
        self.browser.cleanup_run(&run_id).await;
        self.file_state.cleanup_run(&run_id);
        drop(slot);
        result
    }
}

impl Engine {
    async fn persist_status(&self, run_id: &str, status: RunStatus, error: Option<String>) {
        let (usage, plan) = match self.hub.get(run_id) {
            Some(h) => {
                let m = h
                    .meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (Some(m.usage), Some(m.plan.clone()))
            }
            None => (None, None),
        };
        // Currently ignored persistence errors are surfaced only through the
        // gated diagnostics channel; normal behavior is unchanged.
        let persisted = self
            .db
            .update_run_status(run_id.to_string(), status, error, usage, plan)
            .await;
        if persisted.is_err() {
            diag::event(
                tracing::Level::ERROR,
                diag::DB_PERSIST_FAILED,
                &[
                    ("op", diag::Field::Static("run_status")),
                    ("run_id", diag::Field::Uuid(run_id)),
                ],
            );
        }
    }

    async fn finish(&self, run_id: &str, status: RunStatus, error: Option<String>) {
        self.hub.set_status(run_id, status);
        self.hub.publish(
            run_id,
            RunEvent::StatusChanged {
                run_id: run_id.to_string(),
                status,
                detail: error.clone(),
            },
        );
        self.hub.publish(
            run_id,
            RunEvent::RunFinished {
                run_id: run_id.to_string(),
                status,
                error,
            },
        );
        if status == RunStatus::Failed {
            // Final run failure boundary: status and id only — the error
            // detail can embed provider text and stays out of diagnostics.
            diag::event(
                tracing::Level::ERROR,
                diag::RUN_FAILED,
                &[("run_id", diag::Field::Uuid(run_id))],
            );
        }
        self.persist_status(run_id, status, None).await;
    }

    /// Start a new main (coordinator) run and return its id immediately.
    /// The run executes in the background; the UI follows via SSE.
    pub async fn start_chat_run(
        self: &Arc<Self>,
        conversation_id: &str,
        mode: Mode,
        history: Vec<ChatMessage>,
        user_message: &str,
        user_attachments: Vec<vortex_types::AttachmentRef>,
    ) -> String {
        let preflight = ChatPreflight {
            settings: self.current_settings().await,
            support: None,
        };
        let mut user = ChatMessage::user(user_message);
        user.attachments = user_attachments;
        self.start_chat_with_history(
            conversation_id,
            mode,
            history.into_iter().map(|m| (m, None)).collect(),
            user,
            preflight,
        )
        .await
    }

    pub async fn start_preflighted_chat_run(
        self: &Arc<Self>,
        conversation_id: &str,
        mode: Mode,
        history: Vec<vortex_types::StoredMessage>,
        user_message: &str,
        user_attachments: Vec<vortex_types::AttachmentRef>,
        preflight: ChatPreflight,
    ) -> String {
        let mut user = ChatMessage::user(user_message);
        user.attachments = user_attachments;
        let history = history
            .into_iter()
            .filter_map(|m| {
                let mut message = match m.role {
                    vortex_types::Role::User => ChatMessage::user(m.content),
                    vortex_types::Role::Assistant => ChatMessage::assistant(m.content),
                    _ => return None,
                };
                message.attachments = m.attachments;
                Some((message, Some(m.id)))
            })
            .collect();
        self.start_chat_with_history(conversation_id, mode, history, user, preflight)
            .await
    }

    async fn start_chat_with_history(
        self: &Arc<Self>,
        conversation_id: &str,
        mode: Mode,
        history: Vec<(ChatMessage, Option<i64>)>,
        user: ChatMessage,
        preflight: ChatPreflight,
    ) -> String {
        let ChatPreflight { settings, support } = preflight;
        let run_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        let allowed = allowed_for_mode(mode, &settings, self.browser.is_available());
        let system = crate::prompts::coordinator_system_prompt(mode, &settings);
        let mut messages = vec![ChatMessage::system(system)];
        // Keep context bounded: the most recent 40 turns.
        let skip = history.len().saturating_sub(40);
        let mut assistant_ids = std::collections::HashMap::new();
        for (message, id) in history.into_iter().skip(skip) {
            if let Some(id) = id.filter(|_| message.role == vortex_types::Role::Assistant) {
                assistant_ids.insert(id, messages.len());
            }
            messages.push(message);
        }
        let task = if user.content.trim().is_empty() {
            "[attached files]".to_string()
        } else {
            user.content.chars().take(120).collect()
        };
        messages.push(user);
        let spec = RunSpec {
            run_id: run_id.clone(),
            root_run_id: run_id.clone(),
            conversation_id: Some(conversation_id.to_string()),
            parent_run_id: None,
            agent: "coordinator".into(),
            task,
            depth: 0,
            allowed,
            messages,
            model: settings.model.clone(),
            settings,
            cancel,
            is_root: true,
            needs_slot: false,
        };
        self.hub.register(
            &run_id,
            None,
            Some(conversation_id.to_string()),
            "coordinator",
            &spec.task,
            &spec.model,
            0,
            spec.cancel.clone(),
        );
        let persisted = self
            .db
            .upsert_run(&RunInfo {
                id: run_id.clone(),
                parent_run_id: None,
                conversation_id: Some(conversation_id.to_string()),
                agent: "coordinator".into(),
                task: spec.task.clone(),
                model: spec.model.clone(),
                status: RunStatus::Running,
                error: None,
                usage: None,
                plan: Vec::new(),
                depth: 0,
                created_at: vortex_core::db::now(),
                updated_at: vortex_core::db::now(),
            })
            .await;
        if persisted.is_err() {
            diag::event(
                tracing::Level::ERROR,
                diag::DB_PERSIST_FAILED,
                &[
                    ("op", diag::Field::Static("run_upsert")),
                    ("run_id", diag::Field::Uuid(run_id.as_str())),
                ],
            );
        }
        let engine = self.clone();
        // The coordinator task is detached: without a panic boundary a panic
        // in `execute_run` would only surface as a dropped JoinHandle. We
        // record the structured fact — never the payload — and then resume
        // unwinding so the task still panics exactly as the pre
        // instrumentation code did: `hub.remove` is skipped on panic, which
        // preserves the pre-existing stuck-run recovery behavior unchanged.
        tokio::spawn(async move {
            let rid = spec.run_id.clone();
            let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                engine.execute_with_media(
                    spec,
                    ChatRunMedia {
                        support,
                        assistant_ids,
                    },
                ),
            ))
            .await;
            match outcome {
                Ok(text) => {
                    let _ = text;
                    engine.hub.remove(&rid);
                }
                Err(payload) => {
                    diag::event(
                        tracing::Level::ERROR,
                        diag::COORDINATOR_TASK_PANIC,
                        &[("run_id", diag::Field::Uuid(rid.as_str()))],
                    );
                    std::panic::resume_unwind(payload);
                }
            }
        });
        run_id
    }
}

impl Engine {
    /// The core agent loop for a single run.
    async fn run_loop(
        self: &Arc<Self>,
        spec: &RunSpec,
        ctx: ToolContext,
        deadline: tokio::time::Instant,
        chat_media: ChatRunMedia,
    ) -> Result<String, RunError> {
        let run_id = spec.run_id.clone();
        let settings = spec.settings.clone();
        let mut messages = spec.messages.clone();
        let mut total_usage = Usage::default();
        let tools_schema = self.registry.schemas(&spec.allowed);
        let mut media = MediaContext {
            store: ctx.attachments.as_deref(),
            conversation: ctx.conversation_id.as_deref(),
            catalog: &self.catalog,
            llm: self.llm.as_ref(),
            model: &spec.model,
            support: chat_media.support,
        };
        let mut pdfs = PdfReplay::new(
            PdfEngine::parse(&settings.media.pdf_engine).unwrap_or_default(),
            chat_media.assistant_ids,
        );

        for _iteration in 0..settings.agents.max_iterations.max(1) {
            // Token budget is enforced *before* each provider call, and the
            // request's own `max_tokens` is clamped to what remains, so a
            // single huge completion cannot overshoot the run budget by more
            // than one clamped completion. The post-tool check below stays as
            // a backstop.
            let remaining = settings
                .agents
                .max_total_tokens
                .saturating_sub(total_usage.total_tokens);
            if remaining == 0 {
                return Err(RunError::Failed(
                    "token budget exhausted before the next model call; summarize progress".into(),
                ));
            }
            let mut req = CompletionRequest {
                model: spec.model.clone(),
                messages: messages.clone(),
                tools: tools_schema.clone(),
                temperature: settings.temperature,
                max_tokens: settings
                    .max_tokens
                    .min(remaining.min(u64::from(u32::MAX)) as u32),
                pdf_engine: Some(pdfs.parser()),
                assistant_annotations: Default::default(),
            };
            let sent_pdfs = tokio::select! {
                biased;
                _ = spec.cancel.cancelled() => return Err(RunError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => return Err(RunError::Failed("run time budget exceeded".into())),
                prepared = async {
                    let remaining = media.hydrate(&mut req.messages).await?;
                    Ok(pdfs.prepare(&media, &mut req, remaining).await)
                } => prepared.map_err(|e: &'static str| RunError::Failed(e.into()))?,
            };
            if spec.cancel.is_cancelled() {
                return Err(RunError::Cancelled);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(RunError::Failed("run time budget exceeded".into()));
            }
            let (llm_tx, mut llm_rx) = mpsc::channel::<LlmEvent>(256);
            let fwd = {
                let hub = self.hub.clone();
                let run_id = run_id.clone();
                tokio::spawn(async move {
                    while let Some(LlmEvent::TextDelta(delta)) = llm_rx.recv().await {
                        hub.publish(
                            &run_id,
                            RunEvent::TextDelta {
                                run_id: run_id.clone(),
                                delta,
                            },
                        );
                    }
                })
            };
            let llm = self.llm.clone();
            let mut call = tokio::spawn(async move { llm.complete(&req, llm_tx).await });
            let resp = tokio::select! {
                r = &mut call => match r {
                    Ok(inner) => {
                        // Provider failure boundary: fixed category + optional
                        // HTTP status only; the error text stays out of
                        // diagnostics. Logging is a side effect only — the
                        // original structure is preserved: the `Result` is
                        // returned from the select, the delta forwarder below
                        // drains whatever deltas the provider already queued,
                        // and only then does the error propagate (`resp?`),
                        // keeping every delta strictly before the failure
                        // completion event.
                        if let Err(e) = &inner {
                            let mut fields = vec![
                                ("run_id", diag::Field::Uuid(run_id.as_str())),
                                ("reason", diag::Field::Static(e.category())),
                            ];
                            if let Some(code) = e.status() {
                                fields.push(("status", diag::Field::Num(u64::from(code))));
                            }
                            diag::event(tracing::Level::ERROR, diag::RUN_PROVIDER_FAILED, &fields);
                        }
                        inner.map_err(|e| RunError::Failed(e.to_string()))
                    }
                    Err(e) => {
                        // Join error: the original code propagated immediately
                        // with `?` (the provider task panicked, so no further
                        // deltas can arrive). Preserve that early-exit path
                        // exactly; record only the structured panic fact,
                        // never the payload.
                        diag::event(
                            tracing::Level::ERROR,
                            diag::RUN_PROVIDER_TASK_PANIC,
                            &[
                                ("run_id", diag::Field::Uuid(run_id.as_str())),
                                ("cancelled", diag::Field::Flag(e.is_cancelled())),
                            ],
                        );
                        return Err(RunError::Failed(format!("provider task failed: {e}")));
                    }
                },
                _ = spec.cancel.cancelled() => {
                    // Abort the provider task: merely dropping the JoinHandle
                    // would leave the completion running detached, consuming
                    // (and billing) the stream after cancellation.
                    call.abort();
                    fwd.abort();
                    return Err(RunError::Cancelled);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    call.abort();
                    fwd.abort();
                    diag::event(
                        tracing::Level::WARN,
                        diag::RUN_DEADLINE_EXCEEDED,
                        &[("run_id", diag::Field::Uuid(run_id.as_str()))],
                    );
                    return Err(RunError::Failed("run time budget exceeded".into()));
                }
            };
            // The provider task has finished and dropped its `llm_tx`, so the
            // delta forwarder drains whatever is queued and exits on its own:
            // no tail deltas are lost (previously `fwd.abort()` dropped them).
            // The provider error is propagated only after this drain, exactly
            // as the original code did.
            let _ = fwd.await;
            let resp = resp?;
            let annotations = sent_pdfs.match_annotations(resp.annotations);
            if let Some(u) = resp.usage {
                total_usage = total_usage + u;
                if let Some(h) = self.hub.get(&run_id) {
                    h.meta
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .usage = total_usage;
                }
                self.hub.publish(
                    &run_id,
                    RunEvent::UsageUpdate {
                        run_id: run_id.clone(),
                        usage: total_usage,
                    },
                );
                let persisted = self
                    .db
                    .update_run_status(
                        run_id.clone(),
                        RunStatus::Running,
                        None,
                        Some(total_usage),
                        None,
                    )
                    .await;
                if persisted.is_err() {
                    diag::event(
                        tracing::Level::ERROR,
                        diag::DB_PERSIST_FAILED,
                        &[
                            ("op", diag::Field::Static("run_usage")),
                            ("run_id", diag::Field::Uuid(run_id.as_str())),
                        ],
                    );
                }
            }

            // Normalize provider tool-call ids once, before anything consumes
            // them: strict OpenAI-compatible APIs reject an empty
            // `tool_call_id` on the follow-up request, and Started/Finished
            // events plus DB rows need one stable id per call (previously
            // Started used a generated UUID while Finished used "", leaving
            // UI tool rows stuck on "running…" forever).
            let mut tool_calls = resp.tool_calls;
            for tc in tool_calls.iter_mut() {
                if tc.id.trim().is_empty() {
                    tc.id = uuid::Uuid::new_v4().to_string();
                }
            }

            messages.push(ChatMessage {
                role: vortex_types::Role::Assistant,
                content: resp.content.clone(),
                tool_calls: tool_calls.clone(),
                tool_call_id: None,
                tool_name: None,
                attachments: Vec::new(),
                parts: None,
            });

            if tool_calls.is_empty() {
                if spec.is_root {
                    if let Some(conv) = &spec.conversation_id {
                        let persisted = self
                            .db
                            .add_message(conv.clone(), ChatMessage::assistant(resp.content.clone()))
                            .await;
                        if let Ok(assistant_id) = persisted {
                            let save = pdfs.persist(&media, assistant_id, annotations);
                            tokio::select! {
                                biased;
                                _ = spec.cancel.cancelled() => {},
                                _ = tokio::time::sleep_until(deadline) => {},
                                _ = save => {},
                            }
                        } else {
                            diag::event(
                                tracing::Level::ERROR,
                                diag::DB_PERSIST_FAILED,
                                &[
                                    ("op", diag::Field::Static("message_append")),
                                    ("run_id", diag::Field::Uuid(run_id.as_str())),
                                    ("conversation_id", diag::Field::Uuid(conv.as_str())),
                                ],
                            );
                        }
                        let touched = self.db.touch_conversation(conv.clone()).await;
                        if touched.is_err() {
                            diag::event(
                                tracing::Level::ERROR,
                                diag::DB_PERSIST_FAILED,
                                &[
                                    ("op", diag::Field::Static("conversation_touch")),
                                    ("run_id", diag::Field::Uuid(run_id.as_str())),
                                    ("conversation_id", diag::Field::Uuid(conv.as_str())),
                                ],
                            );
                        }
                    }
                }
                return Ok(resp.content);
            }

            pdfs.remember(messages.len() - 1, annotations);
            let outcomes = self.execute_tool_calls(&run_id, &ctx, &tool_calls).await;
            for (tc, outcome) in tool_calls.iter().zip(outcomes) {
                self.hub.publish(
                    &run_id,
                    RunEvent::ToolCallFinished {
                        run_id: run_id.clone(),
                        call_id: tc.id.clone(),
                        ok: outcome.ok,
                        summary: outcome.result.summary.clone(),
                        diff: outcome.result.diff.clone(),
                    },
                );
                messages.push(ChatMessage::tool_result(
                    tc.id.clone(),
                    tc.name.clone(),
                    outcome.result.text,
                ));
            }

            if total_usage.total_tokens > settings.agents.max_total_tokens {
                return Err(RunError::Failed(
                    "token budget for this run was exceeded; stop here and summarize progress"
                        .into(),
                ));
            }
        }
        Err(RunError::Failed(format!(
            "iteration limit ({}) reached before a final answer",
            settings.agents.max_iterations
        )))
    }
}

impl Engine {
    /// Validate, persist, and execute one turn's tool calls concurrently.
    /// Call ids are computed once so the Started event, the DB row and the
    /// finished-state update always refer to the same id.
    async fn execute_tool_calls(
        self: &Arc<Self>,
        run_id: &str,
        ctx: &ToolContext,
        tool_calls: &[vortex_types::ToolCall],
    ) -> Vec<crate::registry::ToolOutcome> {
        // Ids are normalized upstream in run_loop; this fallback keeps
        // Started/Finished consistent even if an empty id ever slips through.
        let ids: Vec<String> = tool_calls
            .iter()
            .map(|tc| {
                if tc.id.trim().is_empty() {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    tc.id.clone()
                }
            })
            .collect();
        let mut tasks = Vec::new();
        for (tc, call_id) in tool_calls.iter().zip(ids.iter()) {
            let args: serde_json::Value =
                serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);
            self.hub.publish(
                run_id,
                RunEvent::ToolCallStarted {
                    run_id: run_id.to_string(),
                    call_id: call_id.clone(),
                    tool: tc.name.clone(),
                    args: args.clone(),
                },
            );
            let _ = self
                .db
                .add_tool_call(&ToolCallRecord {
                    id: call_id.clone(),
                    run_id: run_id.to_string(),
                    tool: tc.name.clone(),
                    args: args.clone(),
                    ok: false,
                    summary: "running".into(),
                    created_at: vortex_core::db::now(),
                })
                .await
                .inspect_err(|_| {
                    diag::event(
                        tracing::Level::ERROR,
                        diag::DB_PERSIST_FAILED,
                        &[
                            ("op", diag::Field::Static("tool_call_record")),
                            ("run_id", diag::Field::Uuid(run_id)),
                            ("call_id", diag::Field::Uuid(call_id.as_str())),
                        ],
                    );
                });
            let registry = self.registry.clone();
            let ctx = ctx.clone();
            let name = tc.name.clone();
            tasks.push(tokio::spawn(async move {
                registry.execute(&ctx, &name, &args).await
            }));
        }
        let mut out = Vec::new();
        for (i, res) in join_all(tasks).await.into_iter().enumerate() {
            let outcome = match res {
                Ok(outcome) => outcome,
                Err(e) => {
                    // Tool task panic boundary: never capture the payload or
                    // message, only the structured flags. The tool name is
                    // model-chosen, so it is logged only when it matches a
                    // known registry entry.
                    if let (Some(tc), Some(call_id)) = (tool_calls.get(i), ids.get(i)) {
                        let mut fields = vec![
                            ("run_id", diag::Field::Uuid(run_id)),
                            ("call_id", diag::Field::Uuid(call_id.as_str())),
                            ("cancelled", diag::Field::Flag(e.is_cancelled())),
                        ];
                        if self.registry.get(&tc.name).is_some() {
                            fields.push(("tool", diag::Field::Id(tc.name.as_str())));
                        } else {
                            fields.push(("tool_known", diag::Field::Flag(false)));
                        }
                        diag::event(tracing::Level::ERROR, diag::TOOL_TASK_PANIC, &fields);
                    }
                    crate::registry::ToolOutcome {
                        ok: false,
                        result: crate::registry::ToolResult {
                            text: format!("error: tool task failed: {e}"),
                            summary: "tool task failed".into(),
                            diff: None,
                        },
                    }
                }
            };
            if let (Some(tc), Some(call_id)) = (tool_calls.get(i), ids.get(i)) {
                let persisted = self
                    .db
                    .add_tool_call(&ToolCallRecord {
                        id: call_id.clone(),
                        run_id: run_id.to_string(),
                        tool: tc.name.clone(),
                        args: serde_json::Value::Null,
                        ok: outcome.ok,
                        summary: outcome.result.summary.clone(),
                        created_at: vortex_core::db::now(),
                    })
                    .await;
                if persisted.is_err() {
                    diag::event(
                        tracing::Level::ERROR,
                        diag::DB_PERSIST_FAILED,
                        &[
                            ("op", diag::Field::Static("tool_call_record")),
                            ("run_id", diag::Field::Uuid(run_id)),
                            ("call_id", diag::Field::Uuid(call_id.as_str())),
                        ],
                    );
                }
                if !outcome.ok {
                    // Tool failure boundary. The tool name is model-chosen, so
                    // it is only captured when it matches a fixed registry
                    // entry; no error text, args, or results are captured.
                    let mut fields = vec![
                        ("run_id", diag::Field::Uuid(run_id)),
                        ("call_id", diag::Field::Uuid(call_id.as_str())),
                    ];
                    if self.registry.get(&tc.name).is_some() {
                        fields.push(("tool", diag::Field::Id(tc.name.as_str())));
                    } else {
                        fields.push(("tool_known", diag::Field::Flag(false)));
                    }
                    diag::event(tracing::Level::WARN, diag::TOOL_FAILED, &fields);
                }
            }
            out.push(outcome);
        }
        out
    }
}

impl Engine {
    /// Spawn one sub-agent on behalf of a coordinator (used by the delegate
    /// tool). Enforces recursion policy and permission inheritance; the
    /// scheduler enforces the global concurrency cap.
    pub async fn spawn_subagent(
        self: &Arc<Self>,
        parent: &ToolContext,
        task: &str,
        agent: &str,
        requested_tools: Option<Vec<String>>,
    ) -> Result<String, ToolError> {
        let settings = self.current_settings().await;
        // Recursion policy is enforced here — not by prompt.
        if parent.depth >= 1 && !settings.agents.recursive {
            return Err(ToolError::NotPermitted(
                "recursive delegation is disabled; only the coordinator can spawn sub-agents \
                 (enable it in Settings → Agents if you really need nesting)"
                    .into(),
            ));
        }
        if parent.depth + 1 > 3 {
            return Err(ToolError::NotPermitted(
                "sub-agent nesting is limited to depth 3".into(),
            ));
        }
        // Permission inheritance: children get a subset of the parent's
        // allow-list, never more, and never the delegate tool unless the user
        // explicitly enabled recursive delegation.
        let mut allowed: HashSet<String> = parent.allowed.clone();
        if let Some(requested) = requested_tools {
            let requested: HashSet<String> = requested.into_iter().collect();
            allowed = allowed.intersection(&requested).cloned().collect();
        }
        if !settings.agents.recursive {
            allowed.remove("delegate_subtask");
        }
        allowed.insert("update_plan".into());
        allowed.insert("request_approval".into());

        let run_id = uuid::Uuid::new_v4().to_string();
        let cancel = parent.cancel.child_token();
        let mut names: Vec<&str> = allowed.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        let allowed_summary = names.join(", ");
        let messages = vec![
            ChatMessage::system(crate::prompts::subagent_system_prompt(
                task,
                &allowed_summary,
            )),
            ChatMessage::user(task),
        ];
        let task_desc: String = task.chars().take(120).collect();
        let spec = RunSpec {
            run_id: run_id.clone(),
            root_run_id: parent.root_run_id.clone(),
            conversation_id: None,
            parent_run_id: Some(parent.run_id.clone()),
            agent: agent.to_string(),
            task: task_desc.clone(),
            depth: parent.depth + 1,
            allowed,
            messages,
            model: settings.model.clone(),
            settings,
            cancel,
            is_root: false,
            needs_slot: true,
        };
        self.hub.register(
            &run_id,
            Some(parent.run_id.clone()),
            None,
            agent,
            &task_desc,
            &spec.model,
            spec.depth,
            spec.cancel.clone(),
        );
        let persisted = self
            .db
            .upsert_run(&RunInfo {
                id: run_id.clone(),
                parent_run_id: Some(parent.run_id.clone()),
                conversation_id: None,
                agent: agent.to_string(),
                task: task_desc.clone(),
                model: spec.model.clone(),
                status: RunStatus::Queued,
                error: None,
                usage: None,
                plan: Vec::new(),
                depth: spec.depth,
                created_at: vortex_core::db::now(),
                updated_at: vortex_core::db::now(),
            })
            .await;
        if persisted.is_err() {
            diag::event(
                tracing::Level::ERROR,
                diag::DB_PERSIST_FAILED,
                &[
                    ("op", diag::Field::Static("run_upsert")),
                    ("run_id", diag::Field::Uuid(run_id.as_str())),
                ],
            );
        }
        let engine = self.clone();
        let rid = run_id.clone();
        let handle = tokio::spawn(async move { engine.execute_run(spec).await });
        match handle.await {
            Ok(Ok(text)) => {
                self.hub.remove(&rid);
                Ok(text)
            }
            Ok(Err(RunError::Cancelled)) => {
                self.hub.remove(&rid);
                Err(ToolError::Cancelled)
            }
            Ok(Err(RunError::Failed(msg))) => {
                self.hub.remove(&rid);
                Err(ToolError::Execution(format!("sub-agent failed: {msg}")))
            }
            Err(e) => {
                // Detached-work panic boundary for spawned sub-agents: the
                // JoinError display (which can embed a panic payload) stays in
                // the normal error path but never enters diagnostics.
                diag::event(
                    tracing::Level::ERROR,
                    diag::SUBAGENT_TASK_PANIC,
                    &[
                        ("run_id", diag::Field::Uuid(rid.as_str())),
                        ("cancelled", diag::Field::Flag(e.is_cancelled())),
                    ],
                );
                self.hub.remove(&rid);
                Err(ToolError::Execution(format!("sub-agent task crashed: {e}")))
            }
        }
    }
}

/// Build the full tool registry with every tool wired to its shared service.
pub fn build_registry(
    previews: Arc<PreviewRegistry>,
    engine_slot: Arc<OnceLock<Arc<Engine>>>,
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    macro_rules! reg {
        ($t:expr) => {
            registry.register(std::sync::Arc::new($t));
        };
    }
    reg!(crate::tools_files::ListFilesTool);
    reg!(crate::tools_files::ReadFileTool);
    reg!(crate::tools_files::SearchFilesTool);
    reg!(crate::tools_write::WriteFileTool);
    reg!(crate::tools_write::EditFileTool);
    reg!(crate::tools_write::DeletePathTool);
    reg!(crate::tools_search::WebSearchTool);
    reg!(crate::tools_search::FetchUrlTool);
    reg!(crate::tools_command::RunCommandTool);
    registry.register(std::sync::Arc::new(
        crate::tools_preview::StartPreviewTool {
            registry: previews.clone(),
        },
    ));
    registry.register(std::sync::Arc::new(
        crate::tools_preview_stop::StopPreviewTool { registry: previews },
    ));
    reg!(crate::tools_browser::BrowserNavigateTool);
    reg!(crate::tools_browser::BrowserReadTool);
    reg!(crate::tools_browser::BrowserClickTool);
    reg!(crate::tools_browser::BrowserTypeTool);
    reg!(crate::tools_meta::UpdatePlanTool);
    reg!(crate::tools_meta::RequestApprovalTool);
    registry.register(std::sync::Arc::new(crate::tools_delegate::DelegateTool {
        engine_slot,
    }));
    Arc::new(registry)
}

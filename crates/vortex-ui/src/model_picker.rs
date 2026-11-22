//! Shared model picker: one catalog browser used by the chat composer (pins
//! future sends in one conversation) and by Settings (sets the default model
//! for new chats). Display data only — catalog metadata is never instructions.

use crate::{
    api::{self, CatalogQuery},
    state::{AppCtx, UploadState},
};
use leptos::{prelude::*, task::spawn_local};
use vortex_types::{
    chat::VORTEX_AUTO_MODEL as AUTO, dto::ModelsResponse, AttachmentKind, ModelInfo, Settings as S,
};

/// Provider router alias: passed through unchanged; the provider picks the
/// eventual model. Distinct from [`AUTO`] (Vortex server-side cost-first
/// routing) and from pinning a concrete model.
pub(crate) const OPENROUTER_AUTO: &str = "openrouter/auto";

#[derive(serde::Deserialize)]
struct ModelPreference {
    model: Option<String>,
}

/// True for the two routing choices whose eventual model is decided per turn.
pub(crate) fn is_auto(id: &str) -> bool {
    id == AUTO || id == OPENROUTER_AUTO
}

/// Catalog fetch state shared between the picker surfaces of one view (e.g.
/// the Settings browser and its output-settings panel). Signals are `Copy`,
/// so the struct is `Copy` and can be handed to several components; all of
/// them observe the same fetch. Client-edited shortlist bounds live here too:
/// they survive picker recreation but are session-only (never persisted).
#[derive(Clone, Copy)]
pub(crate) struct CatalogState {
    catalog: RwSignal<Option<ModelsResponse>>,
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    bounds: RwSignal<CatalogQuery>,
}

impl Default for CatalogState {
    fn default() -> Self {
        Self::new()
    }
}

impl CatalogState {
    pub fn new() -> Self {
        Self {
            catalog: RwSignal::new(None),
            error: RwSignal::new(None),
            loading: RwSignal::new(false),
            bounds: RwSignal::new(CatalogQuery::default()),
        }
    }

    /// Force a fetch with the current client-edited bounds. The server keeps
    /// its own cache and clamps the bounds, echoing the effective values.
    /// Re-entrancy is ignored; async writes use `try_set` because the
    /// component may be gone by the time the fetch resolves.
    pub fn refresh(self) {
        if self.loading.get_untracked() {
            return;
        }
        self.loading.set(true);
        self.error.set(None);
        let catalog = self.catalog;
        let error = self.error;
        let loading = self.loading;
        let query = self.bounds.get_untracked();
        spawn_local(async move {
            match api::model_catalog(query).await {
                Ok(list) => {
                    let _ = catalog.try_set(Some(list));
                }
                Err(err) => {
                    let _ = error.try_set(Some(err));
                }
            }
            let _ = loading.try_set(false);
        });
    }

    /// Fetch only when nothing was ever loaded (Settings mounts with this;
    /// the composer refreshes explicitly when its panel opens).
    pub fn ensure(self) {
        if self.catalog.get_untracked().is_none() && !self.loading.get_untracked() {
            self.refresh();
        }
    }

    pub fn lookup(self, id: &str) -> Option<ModelInfo> {
        self.catalog.with(|list| {
            list.as_ref()
                .and_then(|c| c.models.iter().find(|m| m.id == id))
                .cloned()
        })
    }

    /// The recommended shortlist resolved to catalog entries, in server order.
    pub fn recommended(self) -> Vec<ModelInfo> {
        self.catalog.with(|c| match c {
            Some(resp) => resp
                .recommended
                .iter()
                .filter_map(|id| resp.models.iter().find(|m| &m.id == id).cloned())
                .collect(),
            None => Vec::new(),
        })
    }

    /// True when the catalog reports a weekly-usage ranking source; without
    /// it the UI must not make popularity claims.
    pub fn ranking_available(self) -> bool {
        self.catalog.with(|c| {
            c.as_ref()
                .and_then(|c| c.ranking_source.as_ref())
                .is_some_and(|s| !s.is_empty())
        })
    }
}

thread_local! {
    static PICKER_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn next_picker_id() -> u64 {
    PICKER_ID.with(|c| {
        let next = c.get() + 1;
        c.set(next);
        next
    })
}

/// The catalog browser shared by the composer and Settings.
///
/// Order follows the product decision: routing choice (Vortex Auto /
/// OpenRouter Auto / pin a concrete model) first, then the Recommended
/// shortlist, then All models, then advanced manual ID entry. `on_confirm`
/// is the only mutation path: the composer pins the conversation, Settings
/// updates the unsaved draft — this component itself never writes settings.
/// All function props are `Copy` closures over `Copy` signal state, so the
/// view can capture them repeatedly without move-order hazards.
#[component]
pub(crate) fn ModelBrowser(
    /// Reads the effective saved selection (chat preference or settings draft).
    get_current: impl Fn() -> String + Copy + Send + Sync + 'static,
    /// Disables the confirm action while a save or send is in flight.
    busy: impl Fn() -> bool + Copy + Send + Sync + 'static,
    confirm_label: &'static str,
    /// Applies the confirmed model id (pin chat / update draft).
    on_confirm: impl Fn(String) + Copy + Send + Sync + 'static,
    cat: CatalogState,
    /// When true, load the catalog once if still empty.
    load: RwSignal<bool>,
) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let candidate = RwSignal::new(None::<String>);
    let query = RwSignal::new(String::new());
    let sort = RwSignal::new(String::from("cost"));
    let manual = RwSignal::new(String::new());
    let radio_name = format!("model-choice-{}", next_picker_id());

    Effect::new(move || {
        if load.get() {
            cat.ensure();
        }
    });

    // candidate (explicit pick in this panel) or the saved selection.
    let eff = move || candidate.get().unwrap_or_else(&get_current);
    let confirm_disabled = move || {
        busy()
            || candidate.get().map(|m| m.trim().is_empty()).unwrap_or(true)
            || eff() == get_current()
    };
    let confirm = move |_| {
        let Some(model) = candidate.get_untracked() else {
            return;
        };
        let model = model.trim().to_owned();
        if model.is_empty() {
            return;
        }
        on_confirm(model);
    };

    let matches = move || {
        let mut models = cat
            .catalog
            .with(|list| list.as_ref().map(|c| c.models.clone()).unwrap_or_default());
        let query = query.get().trim().to_lowercase();
        models.retain(|m| {
            !is_auto(&m.id)
                && (m.id.to_lowercase().contains(&query) || m.name.to_lowercase().contains(&query))
        });
        match sort.get().as_str() {
            "newest" => {
                models.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
            }
            "weekly" => {
                models.sort_by(|a, b| {
                    a.weekly_rank
                        .unwrap_or(u32::MAX)
                        .cmp(&b.weekly_rank.unwrap_or(u32::MAX))
                        .then_with(|| a.id.cmp(&b.id))
                });
            }
            _ => {
                models.sort_by(|a, b| {
                    text_price(a)
                        .total_cmp(&text_price(b))
                        .then_with(|| a.id.cmp(&b.id))
                });
            }
        }
        models
    };

    // Recommended-filter editor state; seeded from the session bounds so a
    // picker recreation (conversation switch) keeps the applied values.
    let seed = cat.bounds.get_untracked();
    let bound_in = RwSignal::new(fmt_bound(seed.max_input_usd_per_mtok));
    let bound_out = RwSignal::new(fmt_bound(seed.max_output_usd_per_mtok));
    let bound_limit = RwSignal::new(
        seed.recommended_limit
            .map(|v| v.to_string())
            .unwrap_or_default(),
    );
    let apply_bounds = move |_| {
        cat.bounds.set(CatalogQuery {
            max_input_usd_per_mtok: parse_bound(&bound_in.get_untracked()),
            max_output_usd_per_mtok: parse_bound(&bound_out.get_untracked()),
            recommended_limit: bound_limit
                .get_untracked()
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|v| (1..=25).contains(v)),
        });
        cat.refresh();
    };

    let radio_auto = radio_name.clone();
    let radio_or = radio_name.clone();
    let radio_pin = radio_name.clone();
    // Two owned copies: the Show-children closure borrows `rec_for` per call
    // (label `for`), while the inner conditional closure owns `rec_id`
    // (select `id`) — a shared String would be moved and break `Fn`.
    let rec_for = format!("recommended-{radio_name}");
    let rec_id = rec_for.clone();

    view! {
        <fieldset class="field model-choice">
            <legend>"Model choice"</legend>
            <label>
                <input type="radio" name=radio_auto prop:checked=move || eff() == AUTO
                    on:change=move |_| candidate.set(Some(AUTO.into())) />
                "Vortex Auto — lowest estimated cost among eligible models; the server picks each turn"
            </label>
            <label>
                <input type="radio" name=radio_or prop:checked=move || eff() == OPENROUTER_AUTO
                    on:change=move |_| candidate.set(Some(OPENROUTER_AUTO.into())) />
                "OpenRouter Auto — the provider chooses; not limited to open weights or budget bounds"
            </label>
            <label>
                <input type="radio" name=radio_pin prop:checked=move || !is_auto(&eff())
                    on:change=move |_| {} />
                "Pin a specific model — choose from the lists below"
            </label>
        </fieldset>
        <Show when=move || cat.catalog.with(|c| c.is_some())>
            <div class="field">
                <label for=rec_for.clone()>"Recommended — verified open weights within budget"</label>
                <select id=rec_id.clone() aria-label="Choose a recommended model"
                    on:change=move |e| candidate.set(Some(event_target_value(&e)))>
                    <option value="" prop:selected=move || {
                        let e = eff();
                        e.is_empty() || is_auto(&e)
                    }>"Recommended — choose one"</option>
                    <For each=move || cat.recommended() key=|m| m.id.clone() let:model>
                        <option value=model.id.clone() prop:selected={
                            let id = model.id.clone();
                            move || eff() == id
                        }>
                            {recommended_label(&model)}
                        </option>
                    </For>
                </select>
                <Show when=move || {
                    cat.catalog.with(|c| c.as_ref().map(|c| c.recommended.is_empty()).unwrap_or(false))
                }>
                    <p class="hint">"No verified open-weights models within the current bounds are available in this catalog. Unknown prices or licenses never appear here; they stay in All models below. Widen the bounds to see more."</p>
                </Show>
                <p class="hint">{move || cat.catalog.with(|c| c.as_ref().map(bounds_text).unwrap_or_default())}</p>
                <details class="model-bounds">
                    <summary>"Adjust the recommended filter (this session only)"</summary>
                    <div class="model-picker-actions">
                        <label>"Max $/Mtok input"
                            <input type="number" min="0" step="0.1" aria-label="Maximum input USD per million tokens"
                                placeholder="1" prop:value=move || bound_in.get()
                                on:input=move |e| bound_in.set(event_target_value(&e)) />
                        </label>
                        <label>"Max $/Mtok output"
                            <input type="number" min="0" step="0.1" aria-label="Maximum output USD per million tokens"
                                placeholder="3" prop:value=move || bound_out.get()
                                on:input=move |e| bound_out.set(event_target_value(&e)) />
                        </label>
                        <label>"Shortlist size"
                            <input type="number" min="1" max="25" aria-label="Recommended shortlist size"
                                placeholder="5" prop:value=move || bound_limit.get()
                                on:input=move |e| bound_limit.set(event_target_value(&e)) />
                        </label>
                        <button class="btn" disabled=move || cat.loading.get() on:click=apply_bounds>"Apply"</button>
                    </div>
                    <p class="hint">"Bounds apply to the Recommended shortlist only; All models always shows the full catalog. The server clamps values and echoes what it applied. These edits are not persisted."</p>
                </details>
            </div>
        </Show>
        <div class="model-picker-actions">
            <input type="search" aria-label="Search models" placeholder="Search names and IDs"
                prop:value=move || query.get() on:input=move |e| {
                    candidate.set(None);
                    query.set(event_target_value(&e));
                } />
            <select aria-label="Sort models" on:change=move |e| sort.set(event_target_value(&e))>
                <option value="cost">"Lowest text rates"</option>
                <option value="newest">"Newest first"</option>
                {move || cat.ranking_available().then(|| view! {
                    <option value="weekly" prop:selected=move || sort.get() == "weekly">"Weekly token usage"</option>
                })}
            </select>
        </div>
        <div class="model-picker-actions">
            <select aria-label="All models" on:change=move |e| candidate.set(Some(event_target_value(&e)))>
                <option value="" prop:selected=move || {
                    let e = eff();
                    e.is_empty() || is_auto(&e)
                }>"All models — choose one to pin"</option>
                <For each=matches key=|m| m.id.clone() let:model>
                    <option value=model.id.clone() prop:selected={
                        let id = model.id.clone();
                        move || eff() == id
                    }>
                        {format!("{} ({})", model.name, model.id)}
                    </option>
                </For>
            </select>
            <button class="btn primary" disabled=confirm_disabled on:click=confirm>{confirm_label}</button>
        </div>
        <div class="model-picker-actions">
            <button class="btn" disabled=move || cat.loading.get()
                on:click=move |_| cat.refresh()>"Check for catalog updates"</button>
        </div>
        <p role="status">{move || {
            if cat.loading.get() { return "Loading model catalog...".into(); }
            if let Some(err) = cat.error.get() { return format!("Could not refresh models: {err}"); }
            cat.catalog.with(|list| match list {
                Some(c) if c.stale => {
                    let last = c.fetched_at.map(catalog_timestamp).unwrap_or_else(|| "unknown".into());
                    format!("Catalog refresh failed. Showing cached data from {last}. Auto requires a fresh catalog.")
                }
                Some(c) if c.models.is_empty() => "The provider returned no models.".into(),
                Some(c) => c.fetched_at.map(|ts| format!("Catalog updated: {}", catalog_timestamp(ts))).unwrap_or_default(),
                None => String::new(),
            })
        }}</p>
        {move || {
            let id = eff();
            let (ceiling, budget) = ctx.settings.get()
                .map(|s| (s.max_tokens, s.agents.max_total_tokens))
                .unwrap_or((16_384, 200_000));
            if is_auto(&id) {
                return view! {
                    <p>{auto_copy(&id)}</p>
                    <OutputFacts model=None router=true ceiling budget />
                }.into_any();
            }
            match cat.lookup(&id) {
                Some(model) => view! {
                    <Capabilities model=model.clone() />
                    <OutputFacts model=Some(model) router=false ceiling budget />
                }.into_any(),
                None => {
                    if cat.loading.get() && cat.catalog.with(|c| c.is_none()) {
                        return ().into_any();
                    }
                    view! {
                        <p>"The catalog does not report capabilities, pricing or limits for this model ID. You can still pin it; server-side validation stays authoritative."</p>
                        <OutputFacts model=None router=false ceiling budget />
                    }.into_any()
                }
            }
        }}
        <details>
            <summary>"Enter a model ID manually"</summary>
            <form class="model-picker-actions" on:submit=move |e| {
                e.prevent_default();
                let model = manual.get_untracked().trim().to_owned();
                if model.is_empty() { return; }
                candidate.set(Some(model.clone()));
                on_confirm(model);
            }>
                <input aria-label="Model ID" placeholder="provider/model" prop:value=move || manual.get()
                    on:input=move |e| manual.set(event_target_value(&e)) />
                <button class="btn" disabled=move || busy() || manual.get().trim().is_empty()>"Save model ID"</button>
            </form>
        </details>
    }
}

/// Settings surface: default model for NEW chats. Selection only updates the
/// unsaved draft (`Save settings` persists it); saving never repins an open
/// chat — conversations keep their own stored preference.
#[component]
pub(crate) fn DefaultModelPicker(draft: RwSignal<S>, cat: CatalogState) -> impl IntoView {
    let load = RwSignal::new(true);
    view! {
        <ModelBrowser
            get_current=move || draft.get().model
            busy=move || false
            confirm_label="Set as default for new chats"
            on_confirm=move |model| draft.update(|s| s.model = model)
            cat
            load
        />
    }
}

/// Step-5 output facts: requested ceiling, advertised provider maximum,
/// context capacity/headroom and the per-run budget as separate, honestly
/// labeled fields. Read-only display; editing stays in Settings, and actual
/// per-reply usage is rendered with each reply in the chat.
#[component]
pub(crate) fn OutputFacts(
    model: Option<ModelInfo>,
    router: bool,
    ceiling: u32,
    budget: u64,
) -> impl IntoView {
    let advertised = if router {
        "Unknown at request time — the router's eventual model decides".to_string()
    } else {
        advertised_text(
            model.as_ref().and_then(|m| m.max_completion_tokens),
            ceiling,
        )
    };
    let context = if router {
        "Depends on the eventual model (unknown at request time)".to_string()
    } else {
        match model
            .as_ref()
            .and_then(|m| m.context_length)
            .filter(|&c| c > 0)
        {
            Some(c) => format!("{c} tokens"),
            None => "Not reported".into(),
        }
    };
    let headroom = if router {
        "Unknown — depends on the eventual model".to_string()
    } else {
        headroom_text(model.as_ref().and_then(|m| m.context_length), ceiling)
    };
    view! {
        <dl class="model-capabilities">
            <dt>"Requested output ceiling"</dt>
            <dd>{format!("{ceiling} tokens per call — a ceiling, not a target: the model may stop earlier")}</dd>
            <dt>"Advertised max completion"</dt>
            <dd>{advertised}</dd>
            <dt>"Context capacity"</dt>
            <dd>{context}</dd>
            <dt>"Estimated context headroom"</dt>
            <dd>{headroom}</dd>
            <dt>"Per-run token budget"</dt>
            <dd>{format!("{budget} tokens — the engine also bounds each call's output allowance by the remaining budget")}</dd>
            <dt>"Actual reply usage"</dt>
            <dd>"Reported per reply in the chat when the provider reports it"</dd>
        </dl>
        <p class="hint">"A maximum is not a minimum. Reasoning models can consume completion tokens on reasoning before any visible text. With Auto or router endpoints the eventual model — and its true limits — can be unknown at request time."</p>
    }
}

/// The composer picker: pins the model for FUTURE sends in this chat only.
#[component]
pub(crate) fn ModelPicker() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let cat = CatalogState::new();
    let saving = ctx.model_saving;
    let load = RwSignal::new(false);
    let saved = RwSignal::new(None::<(String, String)>);
    let current_model = move || {
        let id = ctx.current.get();
        saved
            .get()
            .filter(|(chat, _)| Some(chat) == id.as_ref())
            .map(|(_, model)| model)
            .or_else(|| {
                ctx.conversations.with(|convs| {
                    convs
                        .iter()
                        .find(|c| Some(&c.id) == id.as_ref())
                        .and_then(|c| c.model.clone())
                })
            })
            .or_else(|| ctx.settings.get().map(|s| s.model))
            .unwrap_or_else(|| AUTO.into())
    };
    Effect::new(move || {
        let id = ctx.current.get();
        saved.set(None);
        if let Some(id) = id {
            spawn_local(async move {
                if let Ok(response) =
                    api::get_json::<ModelPreference>(&format!("/conversations/{id}/model")).await
                {
                    if ctx.current.get_untracked().as_deref() == Some(id.as_str())
                        && !saving.get_untracked()
                    {
                        let _ = saved.try_update(|current| {
                            if current.is_none() {
                                *current = response.model.map(|model| (id, model));
                            }
                        });
                    }
                }
            });
        }
    });
    let save = move |model: String| {
        let model = model.trim().to_owned();
        let Some(id) = ctx.current.get_untracked() else {
            return;
        };
        if model.is_empty() || saving.get_untracked() || ctx.chat_sending.get_untracked() {
            return;
        }
        saving.set(true);
        spawn_local(async move {
            let body = serde_json::json!({ "model": model }).to_string();
            match api::send_json::<ModelPreference>(
                "PATCH",
                &format!("/conversations/{id}/model"),
                &body,
            )
            .await
            {
                Ok(response) => {
                    let model = response.model.unwrap_or(model);
                    ctx.conversations.update(|list| {
                        if let Some(conversation) = list.iter_mut().find(|c| c.id == id) {
                            conversation.model = Some(model.clone());
                        }
                    });
                    if ctx.current.get_untracked().as_deref() == Some(id.as_str()) {
                        let _ = saved.try_set(Some((id, model)));
                    }
                }
                Err(err) => ctx.set_toast(format!("Could not switch model: {err}")),
            }
            saving.set(false);
        });
    };
    view! {
        <details class="model-picker" on:toggle=move |ev| {
            let target = event_target::<web_sys::HtmlElement>(&ev);
            if target.has_attribute("open") {
                load.set(true);
                cat.refresh();
            }
        }>
            <summary>{move || format!("Next message: {}", model_label(&current_model()))}</summary>
            <div class="model-panel">
                {move || {
                    // Recreate the browser per conversation so an in-progress
                    // pick never leaks into another chat (the old per-chat
                    // reset, expressed structurally). Message drafts and
                    // pending uploads live in AppCtx and are untouched.
                    ctx.current.track();
                    view! {
                        <ModelBrowser
                            get_current=current_model
                            busy=move || saving.get() || ctx.chat_sending.get()
                            confirm_label="Pin for this chat"
                            on_confirm=save
                            cat
                            load
                        />
                    }
                }}
            </div>
        </details>
        <p class="model-warning" role="status">{move || {
            let id = current_model();
            let needed = ctx.pending_attachments.with(|chips| chips.iter().filter_map(|chip| match &chip.state {
                UploadState::Ready(info) => match info.kind {
                    AttachmentKind::Image => Some("image"),
                    AttachmentKind::Video => Some("video"),
                    _ => None,
                },
                _ => None,
            }).collect::<std::collections::BTreeSet<_>>());
            if needed.is_empty() || id == AUTO { return String::new(); }
            match cat.lookup(&id) {
                Some(m) if !m.input_modalities.is_empty() => {
                    let missing = needed.into_iter().filter(|kind| !m.input_modalities.iter().any(|m| m == kind)).collect::<Vec<_>>();
                    if missing.is_empty() { String::new() } else { format!("Current model does not support {} input staged for this message.", missing.join(" or ")) }
                }
                _ => "Attachment support for the current model is not reported. Open the model picker to check.".into(),
            }
        }}</p>
    }
}

#[component]
fn Capabilities(model: ModelInfo) -> impl IntoView {
    let open_weights = match (&model.open_weights, &model.publisher) {
        (Some(true), Some(p)) => format!("Verified open weights — {p}"),
        (Some(true), None) => "Verified open weights".into(),
        _ => "Not verified (a family name, HF id or free price alone is not proof)".into(),
    };
    let license = match (&model.license, &model.license_url) {
        (Some(name), Some(url)) => {
            let name = name.clone();
            let url = url.clone();
            view! { <dd><a href=url target="_blank" rel="noopener noreferrer">{name}</a></dd> }
                .into_any()
        }
        (Some(name), None) => view! { <dd>{name.clone()}</dd> }.into_any(),
        (None, _) => view! { <dd>"Not in the reviewed mapping"</dd> }.into_any(),
    };
    let weights = model.weights_url.clone().map(|url| {
        view! { <dd><a href=url target="_blank" rel="noopener noreferrer">"Publisher weights page"</a></dd> }
    });
    let rank = model
        .weekly_rank
        .map(|r| format!("#{r}"))
        .unwrap_or_else(|| "Unavailable".into());
    view! {
        <dl class="model-capabilities">
            <dt>"Context capacity"</dt><dd>{model.context_length.filter(|&n| n > 0).map(|n| format!("{n} tokens")).unwrap_or_else(|| "Not reported".into())}</dd>
            <dt>"Advertised max completion"</dt><dd>{advertised_text(model.max_completion_tokens, 0)}</dd>
            <dt>"Tool calling"</dt><dd>{support(model.supports_tools)}</dd>
            <dt>"Text input / output"</dt><dd>{format!("{} / {}", modality(&model.input_modalities, "text"), modality(&model.output_modalities, "text"))}</dd>
            <dt>"Image input"</dt><dd>{modality(&model.input_modalities, "image")}</dd>
            <dt>"Video input"</dt><dd>{modality(&model.input_modalities, "video")}</dd>
            <dt>"Audio input / output"</dt><dd>{format!("{} / {}", modality(&model.input_modalities, "audio"), modality(&model.output_modalities, "audio"))}</dd>
            <dt>"Other output"</dt><dd>{format!("Images: {}; video: {}", modality(&model.output_modalities, "image"), modality(&model.output_modalities, "video"))}</dd>
            <dt>"USD per million tokens"</dt><dd>{format!("Input: {}; output: {}", price_label(model.pricing_prompt.as_deref()), price_label(model.pricing_completion.as_deref()))}</dd>
            <dt>"Additional fees (USD)"</dt><dd>{format!("Per request: {}; per image: {}", fee_label(model.pricing_request.as_deref()), fee_label(model.pricing_image.as_deref()))}</dd>
            <dt>"Weekly token usage rank"</dt><dd>{rank}" (usage, not quality)"</dd>
            <dt>"Open weights"</dt><dd>{open_weights}</dd>
            <dt>"License"</dt>{license}
            {weights.map(|w| view! { <dt>"Weights source"</dt>{w} })}
            {model.restriction.clone().map(|r| view! {
                <dt>"Custom terms"</dt><dd>{format!("{r} — kept out of Recommended")}</dd>
            })}
            {model.expires_on.clone().map(|d| view! {
                <dt>"Possible retirement"</dt><dd>{format!("Provider reports this model may be removed after {d}")}</dd>
            })}
        </dl>
        <p>"Capabilities are provider-reported. Vortex chat returns text and handles PDFs with its configured parser; audio and media generation are not available here."</p>
    }
}

fn auto_copy(id: &str) -> &'static str {
    match id {
        AUTO => "Vortex Auto routes each turn to the lowest estimated cost among eligible models and requires a fresh catalog. Each reply shows the model that actually served it. Estimates are ranking data; actual provider charges may differ.",
        _ => "OpenRouter Auto lets the provider choose the model per request. It is not restricted to open weights or budget bounds, and the eventual model can be unknown at request time; replies show the serving model when the provider reports it.",
    }
}

pub(crate) fn model_label(id: &str) -> &str {
    match id {
        AUTO => "Vortex Auto (lowest estimated cost)",
        OPENROUTER_AUTO => "OpenRouter Auto (provider decides)",
        id => id,
    }
}

/// Option label for the recommended shortlist: name, id, prices and weekly
/// usage position when ranked (never a quality claim).
fn recommended_label(m: &ModelInfo) -> String {
    let prices = format!(
        "{} in / {} out per Mtok",
        price_label(m.pricing_prompt.as_deref()),
        price_label(m.pricing_completion.as_deref())
    );
    let rank = match m.weekly_rank {
        Some(r) => format!("weekly usage #{r}"),
        None => "weekly rank unavailable".into(),
    };
    format!("{} ({}) — {prices}; {rank}", m.name, m.id)
}

/// Honest description of the applied shortlist bounds, size and ranking
/// provenance, from the server echo.
fn bounds_text(c: &ModelsResponse) -> String {
    let b = c.recommended_bounds.unwrap_or_default();
    let limit = c.recommended_limit.unwrap_or(5);
    let mut s = format!(
        "Verified open weights, at most ${}/Mtok input and ${}/Mtok output (published zero rates count, unknown prices never do), top {limit}.",
        fmt_usd(b.max_input_usd_per_mtok),
        fmt_usd(b.max_output_usd_per_mtok)
    );
    match c.ranking_source.as_deref().filter(|s| !s.is_empty()) {
        Some(src) => s.push_str(&format!(
            " Order: weekly token usage ({src}) — popularity, not quality."
        )),
        None => s.push_str(" Weekly ranking unavailable, so no popularity order is shown."),
    }
    if c.stale {
        s.push_str(" Showing last-known-good catalog data.");
    }
    s
}

/// Advertised `top_provider.max_completion_tokens`: provider-specific and
/// never a guaranteed limit. `ceiling` > 0 flags a requested ceiling above
/// the advertised maximum (known incompatibility).
fn advertised_text(advertised: Option<u64>, ceiling: u32) -> String {
    match advertised.filter(|&n| n > 0) {
        Some(n) => {
            let mut s = format!("{n} tokens (provider-specific, not a guaranteed limit)");
            if ceiling > 0 && ceiling as u64 > n {
                s.push_str(&format!(
                    " — the requested ceiling {ceiling} is higher; the provider may truncate or reject. Lower it or choose another model."
                ));
            }
            s
        }
        None => "Not reported".into(),
    }
}

fn headroom_text(context: Option<u64>, ceiling: u32) -> String {
    match context.filter(|&c| c > 0) {
        Some(c) => {
            let head = c as i64 - ceiling as i64;
            if head < 0 {
                "Requested ceiling exceeds this model's reported context capacity — lower the ceiling or choose another model".into()
            } else {
                format!("≈ {head} tokens for history plus reply (rough guide: history considers up to the last 40 stored messages)")
            }
        }
        None => "Unknown — context capacity not reported".into(),
    }
}

fn catalog_timestamp(ts: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts as f64 * 1_000.0));
    String::from(date.to_utc_string())
}

fn support(reported: Option<bool>) -> &'static str {
    match reported {
        Some(true) => "Supported",
        Some(false) => "Unsupported",
        None => "Not reported",
    }
}

fn modality(items: &[String], name: &str) -> &'static str {
    support((!items.is_empty()).then(|| items.iter().any(|item| item == name)))
}

fn price(rate: Option<&str>) -> Option<f64> {
    rate?
        .parse::<f64>()
        .ok()
        .filter(|p| p.is_finite() && *p >= 0.0)
}

fn text_price(model: &ModelInfo) -> f64 {
    price(model.pricing_prompt.as_deref()).unwrap_or(f64::INFINITY)
        + price(model.pricing_completion.as_deref()).unwrap_or(f64::INFINITY)
}

fn price_label(rate: Option<&str>) -> String {
    price(rate)
        .map(|p| format!("${}", trim_usd(p * 1_000_000.0)))
        .unwrap_or_else(|| "Not reported".into())
}

/// Per-Mtok display rounding: six decimals covers every human-meaningful
/// rate while hiding binary float artifacts (4e-7 * 1e6 != 0.4 exactly).
fn trim_usd(v: f64) -> String {
    let s = format!("{v:.6}");
    let trimmed = s.trim_end_matches('0');
    let trimmed = trimmed.trim_end_matches('.');
    if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.into()
    }
}

fn fee_label(rate: Option<&str>) -> String {
    price(rate)
        .map(|p| format!("${p}"))
        .unwrap_or_else(|| "Not reported".into())
}

fn fmt_usd(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

fn fmt_bound(v: Option<f64>) -> String {
    v.map(fmt_usd).unwrap_or_default()
}

fn parse_bound(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_labels_distinguish_missing_metadata_from_negative_capabilities() {
        assert_eq!(modality(&[], "video"), "Not reported");
        assert_eq!(modality(&["text".into()], "video"), "Unsupported");
        assert_eq!(modality(&["video".into()], "video"), "Supported");
        assert_eq!(price_label(Some("0")), "$0");
        assert_eq!(price_label(Some("0.0000025")), "$2.5");
        assert_eq!(price_label(Some("0.0000004")), "$0.4");
        assert_eq!(price_label(Some("0.00001")), "$10");
        assert_eq!(price_label(None), "Not reported");
        assert_eq!(price_label(Some("NaN")), "Not reported");
    }

    #[test]
    fn routing_choices_stay_distinct() {
        assert!(is_auto(AUTO));
        assert!(is_auto(OPENROUTER_AUTO));
        assert!(!is_auto("anthropic/claude-sonnet-4"));
        assert_ne!(AUTO, OPENROUTER_AUTO);
        assert_eq!(model_label(AUTO), "Vortex Auto (lowest estimated cost)");
        assert_eq!(
            model_label(OPENROUTER_AUTO),
            "OpenRouter Auto (provider decides)"
        );
        assert_eq!(model_label("a/b"), "a/b");
    }

    #[test]
    fn headroom_is_honest_about_limits() {
        assert_eq!(
            headroom_text(Some(128_000), 16_384),
            "≈ 111616 tokens for history plus reply (rough guide: history considers up to the last 40 stored messages)"
        );
        assert!(headroom_text(Some(4_096), 100_000).starts_with("Requested ceiling exceeds"));
        assert_eq!(
            headroom_text(None, 16_384),
            "Unknown — context capacity not reported"
        );
        assert_eq!(
            headroom_text(Some(0), 16_384),
            "Unknown — context capacity not reported"
        );
    }

    #[test]
    fn advertised_max_flags_known_incompatibility_only_when_exceeded() {
        assert_eq!(
            advertised_text(Some(8_192), 4_096),
            "8192 tokens (provider-specific, not a guaranteed limit)"
        );
        assert!(
            advertised_text(Some(8_192), 100_000).contains("requested ceiling 100000 is higher")
        );
        assert_eq!(advertised_text(None, 100_000), "Not reported");
        assert_eq!(advertised_text(Some(0), 4_096), "Not reported");
        // Capabilities panel passes ceiling 0: never warns there.
        assert_eq!(
            advertised_text(Some(8_192), 0),
            "8192 tokens (provider-specific, not a guaranteed limit)"
        );
    }

    #[test]
    fn recommended_label_never_claims_quality() {
        let m = ModelInfo {
            id: "qwen/qwen3-32b".into(),
            name: "Qwen3 32B".into(),
            pricing_prompt: Some("0.0000004".into()),
            pricing_completion: Some("0.0000016".into()),
            weekly_rank: Some(12),
            ..Default::default()
        };
        let label = recommended_label(&m);
        assert!(label.contains("weekly usage #12"));
        assert!(label.contains("$0.4 in / $1.6 out per Mtok"));
        assert!(!label.to_lowercase().contains("quality"));
        let unranked = ModelInfo {
            weekly_rank: None,
            ..m.clone()
        };
        assert!(recommended_label(&unranked).contains("weekly rank unavailable"));
    }

    #[test]
    fn bounds_text_omits_popularity_when_ranking_unavailable() {
        let base = ModelsResponse {
            models: Vec::new(),
            configured: true,
            fetched_at: None,
            stale: false,
            error: None,
            recommended: Vec::new(),
            recommended_bounds: None,
            recommended_limit: None,
            ranking_source: None,
        };
        let text = bounds_text(&base);
        assert!(text.contains("at most $1/Mtok input and $3/Mtok output"));
        assert!(text.contains("Weekly ranking unavailable"));
        assert!(!text.contains("top-weekly"));
        let ranked = ModelsResponse {
            recommended_bounds: Some(vortex_types::dto::RecommendedBounds {
                max_input_usd_per_mtok: 2.5,
                max_output_usd_per_mtok: 3.0,
            }),
            recommended_limit: Some(5),
            ranking_source: Some("openrouter sort=top-weekly".into()),
            stale: true,
            ..base
        };
        let text = bounds_text(&ranked);
        assert!(text.contains("$2.5/Mtok input"));
        assert!(text
            .contains("weekly token usage (openrouter sort=top-weekly) — popularity, not quality"));
        assert!(text.contains("last-known-good"));
    }

    #[test]
    fn bound_parsing_rejects_garbage() {
        assert_eq!(parse_bound(" 2.5 "), Some(2.5));
        assert_eq!(parse_bound("0"), Some(0.0));
        assert_eq!(parse_bound(""), None);
        assert_eq!(parse_bound("-1"), None);
        assert_eq!(parse_bound("abc"), None);
        assert_eq!(parse_bound("NaN"), None);
        assert_eq!(fmt_bound(None), "");
        assert_eq!(fmt_bound(Some(3.0)), "3");
        assert_eq!(fmt_bound(Some(2.5)), "2.5");
    }
}

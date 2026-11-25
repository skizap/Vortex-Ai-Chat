//! Settings view: model, generation, tools, permissions, search, agents.

use crate::api;
use crate::model_picker::{is_auto, model_label, CatalogState, DefaultModelPicker};
use crate::state::{search_key, AppCtx, SearchCheck, Tab};
use leptos::prelude::*;
use leptos::task::spawn_local;
use vortex_types::{PermissionProfile, SearchTestStatus, Settings as S};

#[component]
pub fn SettingsView() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <div class="settings">
            <Show when=move || ctx.settings.get().is_none()>
                <p class="hint">"Loading settings…"</p>
            </Show>
            {move || ctx.settings.get().map(|s| view! { <SettingsForm s /> }.into_any())}
        </div>
    }
}

#[component]
fn SettingsForm(s: S) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let draft = RwSignal::new(s.clone());
    // One catalog fetch shared by the default-model picker and the output
    // settings panel; the picker triggers the load on mount.
    let cat = CatalogState::new();
    let save = move || {
        let s = draft.get_untracked();
        spawn_local(async move {
            match api::save_settings(&s).await {
                Ok(saved) => {
                    crate::state::apply_theme(saved.theme);
                    ctx.replace_settings(saved);
                    ctx.set_toast("Settings saved");
                    if let Ok(caps) = api::health().await {
                        ctx.capabilities.set(Some(caps));
                    }
                }
                Err(e) => ctx.set_toast(format!("could not save settings: {e}")),
            }
        });
    };

    view! {
        <section class="panel">
            <h3>"Model & generation"</h3>
            <div class="field">
                <label>"Default model for new chats"</label>
                <p class="hint">{move || format!("Current default: {}", model_label(&draft.get().model))}</p>
                <DefaultModelPicker draft cat />
                <p class="hint">"Applies to new chats only; open chats keep the model pinned in their composer."</p>
            </div>
            <div class="field-row">
                <div class="field">
                    <label for="temperature">"Temperature"</label>
                    <input id="temperature" type="number" step="0.1" min="0.0" max="2.0"
                        prop:value=move || format!("{}", draft.get().temperature)
                        on:input=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<f32>() {
                                draft.update(|s| s.temperature = v);
                            }
                        }/>
                </div>
                <OutputSettings draft cat />
            </div>
        </section>
        <ToolsPanel draft />
        <AdvancedPanel draft />
        <SaveBar save />
    }
}

/// The requested output ceiling stays user-editable; full provider facts
/// live in the model picker's details disclosure. A known incompatibility
/// offers an explicit one-click lower value (draft-only — nothing is saved
/// silently).
#[component]
fn OutputSettings(draft: RwSignal<S>, cat: CatalogState) -> impl IntoView {
    view! {
        <div class="field">
            <label for="max-tokens">"Max output tokens (requested ceiling)"</label>
            <input id="max-tokens" type="number" min="64" max="100000"
                prop:value=move || format!("{}", draft.get().max_tokens)
                on:input=move |ev| {
                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                        draft.update(|s| s.max_tokens = v);
                    }
                }/>
            <p class="hint">"Per model call; the model may stop earlier. 16,384 suits most replies."</p>
            {move || {
                let s = draft.get();
                let router = is_auto(&s.model);
                let model = if router { None } else { cat.lookup(&s.model) };
                let unknown = !router && model.is_none();
                let over = model
                    .as_ref()
                    .and_then(|m| m.max_completion_tokens)
                    .filter(|&n| n > 0 && (s.max_tokens as u64) > n);
                view! {
                    {unknown.then(|| view! {
                        <p class="hint">"The catalog has no limits for this model ID; it can still be saved and server-side validation stays authoritative."</p>
                    })}
                    {over.map(|adv| {
                        let adv32 = adv.min(u32::MAX as u64) as u32;
                        let model_id = s.model.clone();
                        let requested = s.max_tokens;
                        view! {
                            <p class="model-warning" role="status">
                                {format!("Requested ceiling {requested} exceeds the advertised maximum completion {adv} of {model_id}. The provider may truncate or reject the call.")}
                                <button class="btn" on:click=move |_| draft.update(|st| st.max_tokens = adv32)>
                                    {format!("Use {adv32} instead")}
                                </button>
                                "Or pick a different model above. Nothing changes until you press Save settings."
                            </p>
                        }
                    })}
                }
            }}
        </div>
    }
}

#[component]
fn ToolsPanel(draft: RwSignal<S>) -> impl IntoView {
    view! {
        <section class="panel">
            <h3>"Tools & permissions"</h3>
            <div class="field-row">
                {toggle("Files", "workspace file read/write",
                    move || draft.get().tools.files,
                    move |v| draft.update(|s| s.tools.files = v))}
                {toggle("Web research", "search + fetch",
                    move || draft.get().tools.search,
                    move |v| draft.update(|s| s.tools.search = v))}
            </div>
            <div class="field-row">
                {toggle("Browser", "isolated Chromium automation",
                    move || draft.get().tools.browser,
                    move |v| draft.update(|s| s.tools.browser = v))}
                {toggle("Commands", "run executables (approval-gated)",
                    move || draft.get().tools.commands,
                    move |v| draft.update(|s| s.tools.commands = v))}
            </div>
            <div class="field-row">
                {toggle("Previews", "loopback site previews",
                    move || draft.get().tools.previews,
                    move |v| draft.update(|s| s.tools.previews = v))}
                {toggle("Sub-agents", "coordinator may delegate",
                    move || draft.get().tools.delegation,
                    move |v| draft.update(|s| s.tools.delegation = v))}
            </div>
            <div class="field-row">
                {toggle("Media", "image & video generation tools",
                    move || draft.get().tools.media,
                    move |v| draft.update(|s| s.tools.media = v))}
            </div>
            <div class="field">
                <label for="profile">"Permission profile"</label>
                <select id="profile"
                    prop:value=move || match draft.get().profile {
                        PermissionProfile::Restricted => "restricted".to_string(),
                        PermissionProfile::Standard => "standard".to_string(),
                        PermissionProfile::Trusted => "trusted".to_string(),
                    }
                    on:change=move |ev| {
                        let p = match event_target_value(&ev).as_str() {
                            "restricted" => PermissionProfile::Restricted,
                            "trusted" => PermissionProfile::Trusted,
                            _ => PermissionProfile::Standard,
                        };
                        draft.update(|s| s.profile = p);
                    }>
                    <option value="restricted">"Restricted — read-only tools"</option>
                    <option value="standard">"Standard — writes allowed; consequential actions need approval"</option>
                    <option value="trusted">"Trusted — allowlisted commands auto-run; consequential actions still need approval"</option>
                </select>
            </div>
            <SearchSetup draft />
        </section>
    }
}

fn saved_search_status(ctx: AppCtx) -> (String, bool) {
    let Some(saved) = ctx.settings.get() else {
        return ("Loading search settings…".into(), false);
    };
    let (enabled, url) = search_key(&saved);
    let mock = ctx
        .capabilities
        .with(|c| c.as_ref().is_some_and(|c| c.mock_llm));
    if !enabled {
        return (
            "Web research is disabled. Enable it in Settings and save.".into(),
            false,
        );
    }
    if !mock && url.is_empty() {
        return (
            "Search is not configured. Save a JSON-enabled SearXNG URL.".into(),
            false,
        );
    }
    match ctx.search_check.get() {
        SearchCheck::Untested => (
            if mock {
                "Mock search configured; not tested."
            } else {
                "SearXNG configured; not tested."
            }
            .into(),
            false,
        ),
        SearchCheck::Testing => ("Testing the saved search provider…".into(), false),
        SearchCheck::Finished {
            message,
            has_results,
        } => (message, has_results),
    }
}

#[component]
pub(crate) fn ResearchStatus(unsupported: Signal<bool>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <div class="research-status">
            <span>{move || saved_search_status(ctx).0}</span>
            <Show when=move || !saved_search_status(ctx).1>
                <button type="button" class="btn" on:click=move |_| {
                    ctx.tab.set(Tab::Settings);
                    request_animation_frame(move || {
                        use wasm_bindgen::JsCast;
                        if let Some(input) = document().get_element_by_id("search-url")
                            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
                        {
                            let _ = input.focus();
                        }
                    });
                }>"Search settings"</button>
            </Show>
            <Show when=move || unsupported.get()>
                <span>"This model does not support tool calling. Choose a tool-capable model above to use search."</span>
            </Show>
        </div>
    }
}

#[component]
fn SearchSetup(draft: RwSignal<S>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    let changed = move || {
        draft.with(|d| {
            ctx.settings
                .with(|saved| saved.as_ref().map(search_key) != Some(search_key(d)))
        })
    };
    view! {
        <div class="field">
            <label for="search-url">"Search provider (SearXNG URL)"</label>
            <input id="search-url" type="text" placeholder="http://127.0.0.1:8888"
                prop:value=move || draft.get().search.base_url
                on:input=move |ev| draft.update(|s| s.search.base_url = event_target_value(&ev))
            />
            <div class="search-actions">
                <button type="button" class="btn"
                    disabled=move || changed() || matches!(ctx.search_check.get(), SearchCheck::Testing)
                    on:click=move |_| test_search(ctx)>"Test search"</button>
                <p class="hint" role="status">{move || if changed() {
                    "Save your search changes before testing.".into()
                } else {
                    saved_search_status(ctx).0
                }}</p>
            </div>
            <p class="hint">"Tests the saved provider with a sample query, without calling a model."</p>
        </div>
    }
}

fn test_search(ctx: AppCtx) {
    if matches!(ctx.search_check.get_untracked(), SearchCheck::Testing) {
        return;
    }
    let revision = ctx.search_revision.get_untracked();
    ctx.search_check.set(SearchCheck::Testing);
    spawn_local(async move {
        let outcome = api::test_search().await;
        if revision != ctx.search_revision.get_untracked() {
            return;
        }
        let (message, has_results) = match outcome {
            Ok(result) => {
                let provider = if result.mock {
                    "Mock search"
                } else {
                    "SearXNG"
                };
                match result.status {
                    SearchTestStatus::Ok => (format!("{provider}: connected ({num} sample {noun}).",
                        num = result.result_count, noun = if result.result_count == 1 { "result" } else { "results" }), true),
                    SearchTestStatus::Empty => (format!("{provider}: connected, but the sample search returned no results. Check the provider's upstream search engines."), false),
                    SearchTestStatus::Disabled => ("Web research is disabled. Enable it in Settings and save.".into(), false),
                    SearchTestStatus::NotConfigured => ("Search is not configured. Save a JSON-enabled SearXNG URL.".into(), false),
                    SearchTestStatus::Error => (format!("{provider}: {}", result.detail.as_deref().unwrap_or("Search test failed. Check the provider and try again.")), false),
                }
            }
            Err(error) => (format!("Could not test search: {error}"), false),
        };
        ctx.search_check.set(SearchCheck::Finished {
            message,
            has_results,
        });
    });
}

#[component]
fn AdvancedPanel(draft: RwSignal<S>) -> impl IntoView {
    view! {
        <details class="panel advanced">
            <summary>"Advanced" <span class="panel-note">"agent limits, budgets & delegation"</span></summary>
            <div class="advanced-body">
                <div class="field-row">
                    <div class="field">
                        <label for="max-concurrent">"Concurrent sub-agents"</label>
                        <input id="max-concurrent" type="number" min="1" max="16"
                            prop:value=move || format!("{}", draft.get().agents.max_concurrent)
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<usize>() {
                                    draft.update(|s| s.agents.max_concurrent = v);
                                }
                            }/>
                        <p class="hint">{format!("Default 6; ceiling {}.", vortex_types::AgentSettings::MAX_LIMIT_CEILING)}</p>
                    </div>
                    <div class="field">
                        <label for="max-iterations">"Max iterations per run"</label>
                        <input id="max-iterations" type="number" min="1" max="64"
                            prop:value=move || format!("{}", draft.get().agents.max_iterations)
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                    draft.update(|s| s.agents.max_iterations = v);
                                }
                            }/>
                    </div>
                </div>
                <div class="field-row">
                    <div class="field">
                        <label for="run-timeout">"Run timeout (seconds)"</label>
                        <input id="run-timeout" type="number" min="30" max="3600"
                            prop:value=move || format!("{}", draft.get().agents.run_timeout_secs)
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<u64>() {
                                    draft.update(|s| s.agents.run_timeout_secs = v);
                                }
                            }/>
                    </div>
                    <div class="field">
                        <label for="token-budget">"Token budget per run"</label>
                        <input id="token-budget" type="number" min="1000" max="10000000" step="1000"
                            prop:value=move || format!("{}", draft.get().agents.max_total_tokens)
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<u64>() {
                                    draft.update(|s| s.agents.max_total_tokens = v);
                                }
                            }/>
                        <p class="hint">"Shared by all model calls in one run; each call's output allowance is bounded by what remains."</p>
                    </div>
                </div>
                {toggle("Recursive delegation", "allow sub-agents to spawn sub-agents (counted in the same limit)",
                    move || draft.get().agents.recursive,
                    move |v| draft.update(|s| s.agents.recursive = v))}
            </div>
        </details>
    }
}

#[component]
fn SaveBar(save: impl Fn() + Clone + Send + 'static) -> impl IntoView {
    view! {
        <div class="save-bar">
            <button class="btn primary" on:click=move |_| save()>"Save settings"</button>
        </div>
    }
}

fn toggle(
    label: &'static str,
    note: &'static str,
    get: impl Fn() -> bool + Send + 'static,
    set: impl Fn(bool) + Send + 'static,
) -> impl IntoView {
    view! {
        <div class="toggle-field">
            <label>
                <input type="checkbox" prop:checked=get
                    on:change=move |ev| set(event_target_checked(&ev))/>
                {label}
            </label>
            <p class="hint">{note}</p>
        </div>
    }
}

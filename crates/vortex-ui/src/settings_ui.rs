//! Settings view: model, generation, tools, permissions, search, agents.

use crate::api;
use crate::state::AppCtx;
use leptos::prelude::*;
use leptos::task::spawn_local;
use vortex_types::{PermissionProfile, Settings as S};

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
    let models = RwSignal::new(Vec::new());
    let saved_flash = RwSignal::new(false);

    Effect::new(move |_| {
        let m = models.clone();
        spawn_local(async move {
            if let Ok(list) = api::models().await {
                m.set(list);
            }
        });
    });

    let save = move || {
        let s = draft.get_untracked();
        let ctx = ctx.clone();
        let flash = saved_flash.clone();
        spawn_local(async move {
            match api::save_settings(&s).await {
                Ok(saved) => {
                    ctx.settings.set(Some(saved));
                    crate::state::apply_theme(s.theme);
                    flash.set(true);
                    set_timeout(
                        move || flash.set(false),
                        std::time::Duration::from_secs(2),
                    );
                }
                Err(e) => ctx.set_toast(format!("could not save settings: {e}")),
            }
        });
    };

    view! {
        <section class="panel">
            <h3>"Model & generation"</h3>
            <div class="field">
                <label for="model">"Model ID"</label>
                <input id="model" list="model-list"
                    prop:value=move || draft.get().model
                    on:input=move |ev| draft.update(|s| s.model = event_target_value(&ev))
                />
                <datalist id="model-list">
                    {move || models.get().into_iter().map(|m| view! {
                        <option value=m.id.clone()>{m.name.clone()}</option>
                    }).collect::<Vec<_>>()}
                </datalist>
                <p class="hint">"The catalog loads from OpenRouter when reachable; you can always type an ID manually (e.g. openrouter/auto)."</p>
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
                <div class="field">
                    <label for="max-tokens">"Max tokens"</label>
                    <input id="max-tokens" type="number" min="64" max="100000"
                        prop:value=move || format!("{}", draft.get().max_tokens)
                        on:input=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                draft.update(|s| s.max_tokens = v);
                            }
                        }/>
                </div>
            </div>
        </section>
        <ToolsPanel draft />
        <AgentsPanel draft />
        <SaveBar save />
    }
}

#[component]
fn ToolsPanel(draft: RwSignal<S>) -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
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
            <div class="field">
                <label for="search-url">"Search provider (SearXNG URL)"</label>
                <input id="search-url" type="text" placeholder="http://127.0.0.1:8888"
                    prop:value=move || draft.get().search.base_url
                    on:input=move |ev| draft.update(|s| s.search.base_url = event_target_value(&ev))
                />
                <p class="hint">
                    {move || {
                        let caps = ctx.capabilities.get();
                        if let Some(c) = caps {
                            if c.search { format!("search ready ({})", c.search_provider) }
                            else { format!("search not configured — {}", c.search_hint) }
                        } else { String::new() }
                    }}
                </p>
            </div>
        </section>
    }
}


#[component]
fn AgentsPanel(draft: RwSignal<S>) -> impl IntoView {
    view! {
        <section class="panel">
            <h3>"Agents, budgets & appearance"</h3>
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
                </div>
            </div>
            {toggle("Recursive delegation", "allow sub-agents to spawn sub-agents (counted in the same limit)",
                move || draft.get().agents.recursive,
                move |v| draft.update(|s| s.agents.recursive = v))}
            <div class="field">
                <label for="theme">"Theme"</label>
                <select id="theme"
                    prop:value=move || match draft.get().theme {
                        vortex_types::Theme::Dark => "dark".to_string(),
                        vortex_types::Theme::Light => "light".to_string(),
                    }
                    on:change=move |ev| {
                        let t = if event_target_value(&ev) == "light" {
                            vortex_types::Theme::Light
                        } else {
                            vortex_types::Theme::Dark
                        };
                        draft.update(|s| s.theme = t);
                    }>
                    <option value="dark">"Dark"</option>
                    <option value="light">"Light"</option>
                </select>
            </div>
        </section>
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
                <input type="checkbox" prop:checked=move || get()
                    on:change=move |ev| set(event_target_checked(&ev))/>
                {label}
            </label>
            <p class="hint">{note}</p>
        </div>
    }
}


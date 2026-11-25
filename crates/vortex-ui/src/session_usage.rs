//! Session totals are derived from cumulative per-run counters, never delta sums.

use crate::state::AppCtx;
use leptos::{prelude::*, task::spawn_local};
use std::collections::HashMap;
use vortex_types::Usage;

#[derive(Debug, Default)]
pub struct SessionUsage {
    conversation: String,
    runs: HashMap<String, RunUsage>,
    loaded: bool,
    load_failed: bool,
    request: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct RunUsage {
    usage: Option<Usage>,
    incomplete: bool,
}

impl SessionUsage {
    fn select(&mut self, conversation: &str) {
        if self.conversation != conversation {
            *self = Self {
                conversation: conversation.to_owned(),
                request: self.request,
                ..Self::default()
            };
        }
    }

    fn merge(&mut self, id: String, update: RunUsage) {
        let run = self.runs.entry(id).or_default();
        if let Some(update) = update.usage {
            let current = run.usage.unwrap_or_default();
            // A snapshot may be older than a cumulative event already received.
            run.usage = Some(Usage {
                prompt_tokens: current.prompt_tokens.max(update.prompt_tokens),
                completion_tokens: current.completion_tokens.max(update.completion_tokens),
                total_tokens: current.total_tokens.max(update.total_tokens),
            });
        }
        run.incomplete |= update.incomplete;
    }

    /// Cumulative totals across runs, with the incomplete marker; `None`
    /// until at least one run has reported usage.
    fn totals(&self) -> Option<(Usage, bool)> {
        let mut total = Usage::default();
        let mut has_report = false;
        let mut incomplete = self.load_failed || !self.loaded;
        for run in self.runs.values() {
            if let Some(usage) = run.usage {
                has_report = true;
                total.prompt_tokens = total.prompt_tokens.saturating_add(usage.prompt_tokens);
                total.completion_tokens = total
                    .completion_tokens
                    .saturating_add(usage.completion_tokens);
                total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
            }
            incomplete |= run.incomplete;
        }
        has_report.then_some((total, incomplete))
    }

    fn label(&self, id: &str) -> String {
        if self.conversation != id {
            return String::new();
        }
        match self.totals() {
            Some((total, incomplete)) => {
                let mut label = human_tokens(total.total_tokens);
                if incomplete {
                    label.push_str(" (partial)");
                }
                label
            }
            None => String::new(),
        }
    }

    fn tooltip(&self, id: &str) -> String {
        if self.conversation != id {
            return String::new();
        }
        match self.totals() {
            Some((total, incomplete)) => format!(
                "{} input · {} output · {} total{}",
                total.prompt_tokens,
                total.completion_tokens,
                total.total_tokens,
                if incomplete { " (incomplete)" } else { "" }
            ),
            None => String::new(),
        }
    }
}

/// Humanized token count: exact below 1k, one decimal in k/M above.
fn human_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M tokens", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k tokens", n as f64 / 1_000.0)
    } else {
        format!("{n} tokens")
    }
}

pub(crate) fn record(ctx: AppCtx, id: String, usage: Option<Usage>, incomplete: bool) {
    if let Some(conversation) = ctx.current.get_untracked() {
        ctx.session_usage.update(|session| {
            session.select(&conversation);
            session.merge(id, RunUsage { usage, incomplete });
        });
    }
}

pub(crate) fn reload(ctx: AppCtx) {
    let Some(id) = ctx.current.get_untracked() else {
        return;
    };
    let mut request = 0;
    ctx.session_usage.update(|s| {
        s.select(&id);
        s.request = s.request.wrapping_add(1);
        request = s.request;
    });
    spawn_local(async move {
        let result = crate::api::get_json::<vortex_types::ConversationUsage>(&format!(
            "/conversations/{id}/usage"
        ))
        .await;
        if ctx.current.get_untracked().as_deref() != Some(id.as_str()) {
            return;
        }
        ctx.session_usage.update(|session| {
            if session.request != request {
                return;
            }
            match result {
                Ok(snapshot) => {
                    session.loaded = true;
                    session.load_failed = false;
                    for run in snapshot.runs {
                        session.merge(
                            run.run_id,
                            RunUsage {
                                usage: run.usage,
                                incomplete: run.incomplete,
                            },
                        );
                    }
                }
                Err(_) => session.load_failed = true,
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_cumulative_reports_do_not_inflate_session_total() {
        let mut s = SessionUsage {
            loaded: true,
            conversation: "abc".into(),
            ..SessionUsage::default()
        };
        assert_eq!(s.label("abc"), "");
        for (id, input, output, incomplete) in [
            ("root", 50, 25, false),
            ("root", 5, 2, false),
            ("child", 20, 5, true),
            ("child", 20, 5, false),
        ] {
            s.merge(
                id.into(),
                RunUsage {
                    usage: Some(Usage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                    }),
                    incomplete,
                },
            );
        }
        assert_eq!(s.label("abc"), "100 tokens (partial)");
        assert_eq!(
            s.tooltip("abc"),
            "70 input · 30 output · 100 total (incomplete)"
        );
        s.select("other");
        assert_eq!(s.label("other"), "");
        assert_eq!(s.tooltip("other"), "");
    }

    #[test]
    fn humanized_token_boundaries() {
        assert_eq!(human_tokens(0), "0 tokens");
        assert_eq!(human_tokens(812), "812 tokens");
        assert_eq!(human_tokens(999), "999 tokens");
        assert_eq!(human_tokens(1_000), "1.0k tokens");
        assert_eq!(human_tokens(12_400), "12.4k tokens");
        assert_eq!(human_tokens(1_000_000), "1.0M tokens");
        assert_eq!(human_tokens(1_234_567), "1.2M tokens");
    }
}

#[component]
pub(crate) fn UsageBadge() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        {move || {
            let id = ctx.current.get().unwrap_or_default();
            ctx.session_usage.with(|s| {
                let label = s.label(&id);
                if !label.is_empty() {
                    let tip = s.tooltip(&id);
                    return view! {
                        <div class="session-usage" title=tip aria-label="Conversation token usage">{label}</div>
                    }.into_any();
                }
                if s.conversation == id && s.load_failed {
                    return view! {
                        <div class="session-usage dim" aria-label="Conversation token usage">"usage unavailable"</div>
                    }.into_any();
                }
                ().into_any()
            })
        }}
    }
}

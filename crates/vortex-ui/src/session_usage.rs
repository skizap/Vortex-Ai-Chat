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

    fn label(&self, id: &str) -> String {
        if self.conversation != id || (!self.loaded && !self.load_failed && self.runs.is_empty()) {
            return "Loading session usage...".into();
        }
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
        if !has_report {
            return if self.load_failed {
                "Session usage unavailable"
            } else {
                "No token usage reported yet"
            }
            .into();
        }
        format!(
            "Session reported tokens: {} input / {} output / {} total{}",
            total.prompt_tokens,
            total.completion_tokens,
            total.total_tokens,
            if incomplete { " (incomplete)" } else { "" }
        )
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
        assert_eq!(s.label("abc"), "No token usage reported yet");
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
        assert_eq!(
            s.label("abc"),
            "Session reported tokens: 70 input / 30 output / 100 total (incomplete)"
        );
        s.select("other");
        assert_eq!(s.label("other"), "Loading session usage...");
    }
}

#[component]
pub(crate) fn UsageBadge() -> impl IntoView {
    let ctx = use_context::<AppCtx>().expect("ctx provided");
    view! {
        <div class="session-usage" aria-label="Conversation token usage">{move || {
            let id = ctx.current.get().unwrap_or_default();
            ctx.session_usage.with(|s| s.label(&id))
        }}</div>
    }
}

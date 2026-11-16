//! Gated, leak-safe structured diagnostics for failure boundaries.
//!
//! Enabled **only** when the process environment contains
//! `VORTEX_DIAGNOSTICS=1` at call time; otherwise every event is a no-op and
//! behavior (including normal logging) is unchanged.
//!
//! Safety rules enforced by this module:
//! - Categories are fixed `&'static str` constants defined here — never
//!   arbitrary error text.
//! - Field values are restricted to numbers, booleans, fixed static labels,
//!   and validated identifiers (UUID format, or caller-validated names that
//!   must match a fixed in-binary allowlist, e.g. registry tool names).
//! - Identifiers are re-validated at emission: anything that is not a valid
//!   UUID or does not pass the identifier charset/length check is replaced
//!   with `<invalid>` so log injection (newlines, control bytes, prompt or
//!   path fragments) cannot reach the diagnostics output.
//! - There is deliberately no variant that accepts arbitrary `String` error
//!   text, payloads, prompts, tool arguments/results, paths, URLs, or tokens.

use tracing::Level;

/// Fixed event target so diagnostics can be routed/filtered independently.
pub const TARGET: &str = "vortex_diag";

// ---- fixed categories -------------------------------------------------------

pub const RUN_FAILED: &str = "run_failed";
pub const RUN_DEADLINE_EXCEEDED: &str = "run_deadline_exceeded";
pub const RUN_PROVIDER_FAILED: &str = "run_provider_failed";
pub const RUN_PROVIDER_TASK_PANIC: &str = "run_provider_task_panic";
pub const RUN_EVENT_FORWARDER_TIMEOUT: &str = "run_event_forwarder_timeout";
pub const COORDINATOR_TASK_PANIC: &str = "coordinator_task_panic";
pub const SUBAGENT_TASK_PANIC: &str = "subagent_task_panic";

pub const TOOL_FAILED: &str = "tool_failed";
pub const TOOL_TASK_PANIC: &str = "tool_task_panic";
pub const TOOL_UNKNOWN: &str = "tool_unknown";
pub const TOOL_NOT_PERMITTED: &str = "tool_not_permitted";
pub const TOOL_UNAVAILABLE: &str = "tool_unavailable";
pub const TOOL_INVALID_ARGS: &str = "tool_invalid_args";
pub const TOOL_DRAIN_TIMEOUT: &str = "tool_drain_timeout";
pub const TOOL_CHECKPOINT_SKIPPED: &str = "tool_checkpoint_skipped";

pub const APPROVAL_DENIED: &str = "approval_denied";
pub const APPROVAL_GATE_FAILED: &str = "approval_gate_failed";
pub const APPROVAL_PERSIST_FAILED: &str = "approval_persist_failed";
pub const APPROVAL_RESOLVE_FAILED: &str = "approval_resolve_failed";
pub const APPROVAL_TIMEOUT: &str = "approval_timeout";
pub const APPROVAL_WAITER_DROPPED: &str = "approval_waiter_dropped";

pub const DB_PERSIST_FAILED: &str = "db_persist_failed";
pub const DB_JOIN_FAILED: &str = "db_join_failed";
pub const SETTINGS_CORRUPT_DEFAULTS: &str = "settings_corrupt_defaults";

pub const PROVIDER_RETRY: &str = "provider_retry";
pub const PROVIDER_REQUEST_FAILED: &str = "provider_request_failed";
pub const PROVIDER_MIDSTREAM_ABORT: &str = "provider_midstream_abort";
pub const PROVIDER_MODELS_FAILED: &str = "provider_models_failed";
pub const SSE_OVERFLOW: &str = "sse_overflow";
pub const SSE_DECODE_FAILED: &str = "sse_decode_failed";
pub const SSE_READ_FAILED: &str = "sse_read_failed";

// ---- gate -------------------------------------------------------------------

fn from_env() -> bool {
    std::env::var("VORTEX_DIAGNOSTICS").is_ok_and(|v| v == "1")
}

/// Diagnostics are on only when `VORTEX_DIAGNOSTICS=1`. In production builds
/// (without the test-only `diag-test-gate` feature) this reads the
/// environment on every check and there is no other way to force the gate.
#[cfg(not(feature = "diag-test-gate"))]
pub fn enabled() -> bool {
    from_env()
}

/// Test-build flavor: the environment decides unless a test override is set.
#[cfg(feature = "diag-test-gate")]
pub fn enabled() -> bool {
    use std::sync::atomic::Ordering;
    match test_gate::GATE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => from_env(),
    }
}

#[cfg(feature = "diag-test-gate")]
mod test_gate {
    //! Override machinery, compiled out of production builds entirely: the
    //! `diag-test-gate` feature is only enabled by integration test
    //! dev-dependencies, so a release binary exposes no way to force the gate
    //! other than the `VORTEX_DIAGNOSTICS` environment variable.

    /// -1: unset (decide from the environment), 0: forced off, 1: forced on.
    pub static GATE: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

    /// Test-only override for the env gate. Must not be used outside test
    /// code; passing `None` restores normal env-based behavior. Tests that
    /// flip the gate must hold [`gate_lock_for_tests`] so concurrent tests in
    /// one process cannot race the flag.
    #[doc(hidden)]
    pub fn set_override_for_tests(value: Option<bool>) {
        use std::sync::atomic::Ordering;
        let v = match value {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        };
        GATE.store(v, Ordering::Relaxed);
    }

    /// Serializes gate flips across concurrent tests in one process.
    #[doc(hidden)]
    pub fn gate_lock_for_tests() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// RAII helper for integration tests: holds the gate lock, forces the gate
    /// on (or off with [`force_disabled_for_tests`]), and restores env-based
    /// behavior when dropped.
    #[doc(hidden)]
    pub struct TestGateGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for TestGateGuard {
        fn drop(&mut self) {
            set_override_for_tests(None);
        }
    }

    #[doc(hidden)]
    pub fn force_enabled_for_tests() -> TestGateGuard {
        let lock = gate_lock_for_tests()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_override_for_tests(Some(true));
        TestGateGuard { _lock: lock }
    }

    #[doc(hidden)]
    pub fn force_disabled_for_tests() -> TestGateGuard {
        let lock = gate_lock_for_tests()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_override_for_tests(Some(false));
        TestGateGuard { _lock: lock }
    }
}

#[cfg(feature = "diag-test-gate")]
pub use test_gate::{
    force_disabled_for_tests, force_enabled_for_tests, gate_lock_for_tests, set_override_for_tests,
    TestGateGuard,
};

// ---- validated fields -------------------------------------------------------

/// A safe field value. There is no arbitrary-string variant by design.
pub enum Field<'a> {
    /// Numeric counter or bounded code (attempt, HTTP status, sizes).
    Num(u64),
    /// Boolean flag.
    Flag(bool),
    /// Fixed label chosen from constants in this module or a fixed `op` name.
    Static(&'static str),
    /// Identifier validated here as a canonical UUID string; anything else is
    /// emitted as `<invalid>`.
    Uuid(&'a str),
    /// Identifier the caller has validated against a fixed in-binary set
    /// (e.g. a registry tool name). Re-checked here as a lowercase
    /// snake-case identifier (`[a-z0-9_]{1,64}`) so any other content —
    /// including secrets or injection text — is replaced with `<invalid>`.
    Id(&'a str),
}

fn is_valid_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

/// Identifiers are short, lowercase, snake-case, and injection-proof: only
/// ASCII lowercase letters, digits, and `_`, at most 64 characters.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn render(key: &str, value: &Field<'_>, out: &mut String) {
    out.push(' ');
    out.push_str(key);
    out.push('=');
    match value {
        Field::Num(n) => out.push_str(&n.to_string()),
        Field::Flag(b) => out.push_str(if *b { "true" } else { "false" }),
        Field::Static(s) => out.push_str(s),
        Field::Uuid(s) => {
            if is_valid_uuid(s) {
                out.push_str(s);
            } else {
                out.push_str("<invalid>");
            }
        }
        Field::Id(s) => {
            if is_safe_identifier(s) {
                out.push_str(s);
            } else {
                out.push_str("<invalid>");
            }
        }
    }
}

/// Format one event as `category=<c>[ key=value...]`. Exposed for tests.
pub fn format_event(category: &str, fields: &[(&'static str, Field<'_>)]) -> String {
    let mut msg = String::with_capacity(32 + fields.len() * 16);
    msg.push_str("category=");
    msg.push_str(category);
    for (key, value) in fields {
        render(key, value, &mut msg);
    }
    msg
}

/// Emit one diagnostics event; a no-op unless [`enabled`].
pub fn event(level: Level, category: &'static str, fields: &[(&'static str, Field<'_>)]) {
    if !enabled() {
        return;
    }
    let msg = format_event(category, fields);
    match level {
        Level::ERROR => tracing::error!(target: TARGET, "{}", msg),
        Level::WARN => tracing::warn!(target: TARGET, "{}", msg),
        Level::INFO => tracing::info!(target: TARGET, "{}", msg),
        Level::DEBUG => tracing::debug!(target: TARGET, "{}", msg),
        Level::TRACE => tracing::trace!(target: TARGET, "{}", msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Minimal capturing subscriber for offline assertions. Only the
    /// gate-override tests capture emitted events; the pure-function tests
    /// below exercise rendering without a subscriber.
    #[cfg_attr(not(feature = "diag-test-gate"), allow(dead_code))]
    #[derive(Default)]
    struct Capture {
        lines: Arc<Mutex<Vec<String>>>,
    }
    #[cfg_attr(not(feature = "diag-test-gate"), allow(dead_code))]
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::metadata::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut msg = String::new();
            event.record(&mut Visitor(&mut msg));
            self.lines.lock().unwrap().push(msg);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    #[cfg_attr(not(feature = "diag-test-gate"), allow(dead_code))]
    struct Visitor<'a>(&'a mut String);
    #[cfg_attr(not(feature = "diag-test-gate"), allow(dead_code))]
    impl tracing::field::Visit for Visitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            }
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.0.push_str(value);
            }
        }
    }

    const RUN: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    #[test]
    fn formats_fixed_categories_and_fields() {
        let msg = format_event(
            DB_PERSIST_FAILED,
            &[
                ("op", Field::Static("run_status")),
                ("run_id", Field::Uuid(RUN)),
                ("attempt", Field::Num(2)),
                ("ok", Field::Flag(false)),
            ],
        );
        assert_eq!(
            msg,
            "category=db_persist_failed op=run_status run_id=0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0 attempt=2 ok=false"
        );
    }

    #[test]
    fn invalid_uuids_become_invalid() {
        let msg = format_event(
            RUN_FAILED,
            &[
                ("run_id", Field::Uuid("not-a-uuid")),
                ("run_id", Field::Uuid("")),
            ],
        );
        assert_eq!(msg, "category=run_failed run_id=<invalid> run_id=<invalid>");
    }

    #[test]
    fn log_injection_attempts_are_neutralized() {
        let evil = "tool'; DROP\n2026-11-16T00:00:00Z injected=yes";
        let msg = format_event(TOOL_FAILED, &[("tool", Field::Id(evil))]);
        assert_eq!(msg, "category=tool_failed tool=<invalid>");
        assert!(!msg.contains('\n'));
        let spaces = format_event(TOOL_FAILED, &[("tool", Field::Id("a b c"))]);
        assert!(spaces.ends_with("<invalid>"));
        let too_long = format_event(TOOL_FAILED, &[("tool", Field::Id(&"x".repeat(65)))]);
        assert!(too_long.ends_with("<invalid>"));
        let ok = format_event(TOOL_FAILED, &[("tool", Field::Id("web_search"))]);
        assert!(ok.ends_with("web_search"));
    }

    // The two gate-behavior tests need the test-only override feature; they
    // also run end-to-end (gate on/off) in the vortex-agents integration
    // tests, which enable the feature via their dev-dependencies.
    #[cfg(all(test, feature = "diag-test-gate"))]
    #[test]
    fn events_are_silent_when_gate_is_off() {
        let _g = gate_lock_for_tests().lock().unwrap();
        set_override_for_tests(Some(false));
        let captured: Arc<Mutex<Vec<String>>> = Arc::default();
        let sub = Capture {
            lines: captured.clone(),
        };
        tracing::subscriber::with_default(sub, || {
            event(Level::ERROR, RUN_FAILED, &[("run_id", Field::Uuid(RUN))]);
        });
        assert!(captured.lock().unwrap().is_empty());
        set_override_for_tests(None);
    }

    #[cfg(all(test, feature = "diag-test-gate"))]
    #[test]
    fn events_emit_only_safe_rendered_fields_when_enabled() {
        let _g = gate_lock_for_tests().lock().unwrap();
        set_override_for_tests(Some(true));
        let captured: Arc<Mutex<Vec<String>>> = Arc::default();
        let sub = Capture {
            lines: captured.clone(),
        };
        let secret = "sk-SENTINEL-do-not-log";
        tracing::subscriber::with_default(sub, || {
            event(
                Level::ERROR,
                RUN_PROVIDER_FAILED,
                &[
                    ("run_id", Field::Uuid(RUN)),
                    ("reason", Field::Static("invalid_credentials")),
                    ("status", Field::Num(401)),
                ],
            );
            // A case that cannot occur in production (Static/Id values are
            // fixed labels or validated identifiers), proving the emit path
            // itself cannot leak arbitrary text such as a sentinel secret.
            event(Level::ERROR, TOOL_FAILED, &[("tool", Field::Id(secret))]);
        });
        let lines = captured.lock().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("category=run_provider_failed"));
        assert!(lines[0].contains("run_id=0f1e2d3c"));
        assert!(lines[0].contains("reason=invalid_credentials"));
        assert!(lines[0].contains("status=401"));
        assert!(!lines.iter().any(|l| l.contains("SENTINEL")));
        drop(lines);
        set_override_for_tests(None);
    }
}

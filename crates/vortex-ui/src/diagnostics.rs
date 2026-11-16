//! Session-gated, local-only best-effort browser diagnostics reporter.
//!
//! Activated only when the server reports `Capabilities.diagnostics` (the
//! env-only `VORTEX_DIAGNOSTICS=1` test-launch gate). Reports are the shared
//! [`vortex_types::BrowserReport`] shape: fixed enums, an optional HTTP
//! status, strict-UUID ids, and numeric-only source locations. There is no
//! field that can carry exception text, stack traces, URLs, header values,
//! conversations, or arbitrary error strings — the wire shape makes free
//! text impossible.
//!
//! Reporting is best-effort: a single send with no retry loop, console
//! fallback when the server cannot be reached, a 30-second dedup window for
//! repeated failures, and a 32-event pre-enable buffer. Diagnostic requests
//! are issued directly (never through the instrumented `api.rs`), so the
//! reporter cannot recurse through its own instrumentation.

use vortex_types::{BrowserEventKind, BrowserOperation, BrowserReport, BrowserSourceLocation};

/// How the response header carrying the server-generated request id is named
/// in diagnostics mode (`crates/vortex-server/src/diagnostics.rs`).
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Identical failures (same event/operation/status) report at most once
/// within this window.
pub const DEDUP_WINDOW_MS: u64 = 30_000;

/// Maximum events held while reporting is not yet enabled (pre-WASM
/// bootstrap events plus UI events raised before the health probe resolves).
pub const BUFFER_CAPACITY: usize = 32;

/// Maximum distinct dedup keys tracked (bounded, no unbounded growth).
pub const DEDUP_CAPACITY: usize = 64;

/// Server-accepted upper bound for location numbers.
pub const MAX_LOCATION_VALUE: u32 = 1_000_000;

// ---------------------------------------------------------------------------
// Pure helpers (exercised by native unit tests)
// ---------------------------------------------------------------------------

/// Accept only strict UUID-shaped ids: hexadecimal everywhere except
/// exactly the 8/13/18/23 hyphen positions. The server validates ids as
/// UUIDs, so injected text is rejected here first (client-side), keeping
/// malformed data off the wire entirely.
pub fn validate_uuid(value: &str) -> Option<String> {
    let v = value.trim();
    let bytes = v.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b != b'-' {
                return None;
            }
        } else if !b.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(v.to_string())
}

/// Numeric-only location metadata. Zero/oversized values are dropped and an
/// all-empty location collapses to `None` (the field is omitted entirely).
pub fn safe_location(line: Option<u32>, column: Option<u32>) -> Option<BrowserSourceLocation> {
    let in_range = |n: u32| (1..=MAX_LOCATION_VALUE).contains(&n);
    let line = line.filter(|l| in_range(*l));
    let column = column.filter(|c| in_range(*c));
    if line.is_none() && column.is_none() {
        None
    } else {
        Some(BrowserSourceLocation { line, column })
    }
}

/// Assemble a report. Ids and the status are validated here so a caller
/// cannot accidentally smuggle free text into the wire shape; anything not
/// matching the shared contract is silently dropped.
pub fn build_report(
    event: BrowserEventKind,
    operation: BrowserOperation,
    status: Option<u16>,
    request_id: Option<&str>,
    run_id: Option<&str>,
    location: Option<BrowserSourceLocation>,
) -> BrowserReport {
    BrowserReport {
        event,
        operation,
        status: status.filter(|s| *s <= 599),
        request_id: request_id.and_then(validate_uuid),
        run_id: run_id.and_then(validate_uuid),
        location,
    }
}

/// Extract the run id from an instrumented API path such as
/// `/runs/{uuid}/state`. Query strings are stripped before matching; only a
/// strict UUID in the run position is returned. The diagnostics endpoint
/// itself is never matched, so its own requests carry no run id.
pub fn extract_run_id(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or("");
    let mut segs = path.trim_start_matches('/').split('/');
    if segs.next() == Some("runs") {
        if let Some(id) = segs.next() {
            return validate_uuid(id);
        }
    }
    None
}

/// Outcome of the health probe shared between the bootstrap reporter (JS)
/// and the WASM reporter (Rust). Both sides publish/consume the same verdict
/// through `window.__vortexDiag`, so exactly one of them owns delivery for
/// each event origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Diagnostics are on: report.
    Enabled,
    /// Diagnostics are off (or the endpoint/flag is absent): stay silent.
    Disabled,
    /// Health could not be reached or understood (transport failure, guard
    /// denial, server error, malformed body): console fallback, never a
    /// confirmed "off".
    Unavailable,
}

/// Map a health response to a verdict. A guard denial (401/403/5xx) or an
/// unreadable body is `Unavailable`, not `Disabled`: only a definitive
/// 200-with-`diagnostics:false` or a 404 (feature absent) disables reporting.
pub fn classify_health(status: u16, diagnostics_flag: Option<bool>) -> Verdict {
    match status {
        200 => match diagnostics_flag {
            Some(true) => Verdict::Enabled,
            Some(false) => Verdict::Disabled,
            None => Verdict::Unavailable,
        },
        404 => Verdict::Disabled,
        _ => Verdict::Unavailable,
    }
}

type DedupKey = (BrowserEventKind, BrowserOperation, Option<u16>);

/// Bounded dedup of repeated failures. Fixed capacity: the oldest key is
/// evicted when full, so state can never grow with the number of failures.
pub struct Deduper {
    window_ms: u64,
    capacity: usize,
    last_seen: std::collections::VecDeque<(DedupKey, u64)>,
}

impl Deduper {
    pub fn new(window_ms: u64) -> Self {
        Self {
            window_ms,
            capacity: DEDUP_CAPACITY,
            last_seen: std::collections::VecDeque::new(),
        }
    }

    /// Emit at most once per window per key. Timestamps are supplied by the
    /// caller, keeping this pure and testable.
    pub fn should_emit(&mut self, key: DedupKey, now_ms: u64) -> bool {
        if let Some(entry) = self.last_seen.iter_mut().find(|(k, _)| *k == key) {
            if now_ms.saturating_sub(entry.1) < self.window_ms {
                return false;
            }
            entry.1 = now_ms;
            return true;
        }
        self.last_seen.push_back((key, now_ms));
        while self.last_seen.len() > self.capacity {
            self.last_seen.pop_front();
        }
        true
    }
}

/// Bounded pre-enable buffer (oldest dropped when full). Holds bootstrap and
/// early UI events until the health probe decides whether reporting is on.
pub struct EventBuffer {
    max: usize,
    items: std::collections::VecDeque<BrowserReport>,
}

impl EventBuffer {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            items: std::collections::VecDeque::new(),
        }
    }

    pub fn push(&mut self, report: BrowserReport) {
        if self.items.len() >= self.max {
            self.items.pop_front();
        }
        self.items.push_back(report);
    }

    pub fn drain(&mut self) -> Vec<BrowserReport> {
        self.items.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Public entry points (no-ops off WASM; real reporter under `imp`)
// ---------------------------------------------------------------------------

/// Initialize the reporter before any UI boot request runs. Installs the
/// WASM panic hook, consumes the bootstrap reporter's health verdict (if the
/// index.html probe already decided) and otherwise probes health itself,
/// publishing the verdict back for the bootstrap side. Bootstrap-origin
/// events stay owned by the JS reporter, so no event is delivered twice.
/// A no-op on non-WASM targets.
pub fn init() {
    imp::init();
}

/// Report an instrumented `api.rs` failure boundary (transport / status /
/// decode). Ids are validated before anything is sent.
pub fn report_api_failure(
    operation: BrowserOperation,
    status: Option<u16>,
    request_id: Option<&str>,
    run_id: Option<&str>,
) {
    imp::report(
        BrowserEventKind::Error,
        operation,
        status,
        request_id,
        run_id,
        None,
    );
}

/// Report an instrumented `sse.rs` boundary (connect / decode / reconnect /
/// gap / resync).
pub fn report_sse_failure(operation: BrowserOperation, run_id: Option<&str>) {
    imp::report(BrowserEventKind::Error, operation, None, None, run_id, None);
}

/// Record a panic location synchronously, before the WASM trap. The panic
/// payload is deliberately never read; only bounded numeric line/column is
/// forwarded.
pub fn report_panic(location: Option<&std::panic::Location<'_>>) {
    imp::report_panic(location);
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::BrowserOperation;
    use std::panic::Location;

    pub fn init() {}

    pub fn report(
        _event: vortex_types::BrowserEventKind,
        _operation: BrowserOperation,
        _status: Option<u16>,
        _request_id: Option<&str>,
        _run_id: Option<&str>,
        _location: Option<vortex_types::BrowserSourceLocation>,
    ) {
    }

    pub fn report_panic(_location: Option<&Location<'_>>) {}
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::{
        build_report, classify_health, safe_location, BrowserEventKind, BrowserOperation,
        BrowserReport, BrowserSourceLocation, Deduper, EventBuffer, Verdict, BUFFER_CAPACITY,
        DEDUP_WINDOW_MS,
    };
    use js_sys::Date;
    use std::cell::RefCell;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::spawn_local;

    const DIAGNOSTICS_PATH: &str = "/api/diagnostics";
    const HEALTH_PATH: &str = "/api/health";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Phase {
        /// Health verdict not yet known: buffer events.
        Probing,
        /// Diagnostics enabled: best-effort send.
        Enabled,
        /// Health unreachable/undecodable: console fallback only.
        ConsoleOnly,
        /// Diagnostics disabled: normal behavior, fully silent.
        Disabled,
    }

    struct Reporter {
        phase: Phase,
        buffer: EventBuffer,
        deduper: Deduper,
    }

    thread_local! {
        static REPORTER: RefCell<Reporter> = RefCell::new(Reporter {
            phase: Phase::Probing,
            buffer: EventBuffer::new(BUFFER_CAPACITY),
            deduper: Deduper::new(DEDUP_WINDOW_MS),
        });
    }

    pub fn init() {
        install_panic_hook();
        // The bootstrap reporter (index.html) owns delivery for
        // bootstrap-origin events; Rust never drains its buffer, so no event
        // is ever handed off twice. Rust only consumes the health verdict
        // (avoiding a second health request) and publishes its own probe
        // result back if the bootstrap probe had not completed.
        match read_bootstrap_verdict() {
            Some(Verdict::Enabled) => set_phase(Phase::Enabled),
            Some(Verdict::Disabled) => set_phase(Phase::Disabled),
            Some(Verdict::Unavailable) => {
                set_phase(Phase::ConsoleOnly);
            }
            None => spawn_local(async move { probe_and_enable().await }),
        }
    }

    pub fn report(
        event: BrowserEventKind,
        operation: BrowserOperation,
        status: Option<u16>,
        request_id: Option<&str>,
        run_id: Option<&str>,
        location: Option<BrowserSourceLocation>,
    ) {
        let report = build_report(event, operation, status, request_id, run_id, location);
        REPORTER.with(|r| {
            let mut r = r.borrow_mut();
            if !r
                .deduper
                .should_emit((event, operation, status), Date::now() as u64)
            {
                return;
            }
            match r.phase {
                Phase::Disabled => {}
                Phase::Probing => r.buffer.push(report),
                Phase::ConsoleOnly => console_fallback(&report),
                Phase::Enabled => {
                    spawn_local(async move {
                        if !send_report(&report).await {
                            console_fallback(&report);
                        }
                    });
                }
            }
        });
    }

    /// Record a panic location synchronously, before the WASM trap: an async
    /// (`spawn_local`) send could never run once the module aborts.
    /// Delivery is chosen by phase:
    /// - Enabled: `navigator.sendBeacon` (fire-and-forget, browser-owned).
    /// - Probing: hand the serialized report to the bootstrap reporter's
    ///   buffer, which survives the abort and is delivered (or consoled) by
    ///   the still-running JS side once its health probe resolves.
    /// - ConsoleOnly / Disabled: console fallback / silence.
    ///
    /// The payload is deliberately never read; only numeric line/column is
    /// forwarded, and no unrelated panic is suppressed.
    pub fn report_panic(location: Option<&std::panic::Location<'_>>) {
        let location = location
            .map(|l| safe_location(Some(l.line()), Some(l.column())))
            .unwrap_or(None);
        let report = build_report(
            BrowserEventKind::Panic,
            BrowserOperation::Panic,
            None,
            None,
            None,
            location,
        );
        match current_phase() {
            Phase::Enabled => {
                if !beacon_report(&report) {
                    console_fallback(&report);
                }
            }
            Phase::Probing => bootstrap_enqueue(&report),
            Phase::ConsoleOnly => console_fallback(&report),
            Phase::Disabled => {}
        }
    }

    fn current_phase() -> Phase {
        REPORTER.with(|r| r.try_borrow().map(|r| r.phase).unwrap_or(Phase::Probing))
    }

    fn set_phase(phase: Phase) {
        REPORTER.with(|r| r.borrow_mut().phase = phase);
    }

    fn phase_is_disabled() -> bool {
        REPORTER.with(|r| {
            r.try_borrow()
                .map(|r| r.phase == Phase::Disabled)
                .unwrap_or(false)
        })
    }

    /// Apply the health verdict: set the phase, then drain the wasm-origin
    /// pre-enable buffer appropriately (send when enabled, console fallback
    /// when unavailable, silence when disabled) and publish the verdict back
    /// to the bootstrap reporter so its pending flush starts.
    fn apply_verdict(verdict: Verdict) {
        let phase = match verdict {
            Verdict::Enabled => Phase::Enabled,
            Verdict::Disabled => Phase::Disabled,
            Verdict::Unavailable => Phase::ConsoleOnly,
        };
        let events = REPORTER.with(|r| {
            let mut r = r.borrow_mut();
            r.phase = phase;
            r.buffer.drain()
        });
        match verdict {
            Verdict::Enabled => spawn_local(async move {
                for report in events {
                    if !send_report(&report).await {
                        console_fallback(&report);
                    }
                }
            }),
            Verdict::Unavailable => {
                for report in events {
                    console_fallback(&report);
                }
            }
            Verdict::Disabled => drop(events), // confirmed off: silent
        }
        publish_verdict(verdict);
    }

    /// One health probe (uninstrumented, raw). Never retried.
    async fn probe_and_enable() {
        let verdict = match gloo_net::http::Request::get(HEALTH_PATH).send().await {
            Ok(resp) if resp.status() == 200 => {
                let flag = resp
                    .text()
                    .await
                    .ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                    .and_then(|v| v.get("diagnostics").and_then(|d| d.as_bool()));
                Some(classify_health(200, flag))
            }
            Ok(resp) => Some(classify_health(resp.status(), None)),
            Err(_) => None,
        };
        apply_verdict(verdict.unwrap_or(Verdict::Unavailable));
    }

    /// Consume the bootstrap health verdict stashed on `window.__vortexDiag`
    /// by index.html (`decided`/`enabled`/`fallback`). `None` = undecided.
    fn read_bootstrap_verdict() -> Option<Verdict> {
        let stash = bootstrap_stash()?;
        let decided = js_sys::Reflect::get(&stash, &"decided".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !decided {
            return None;
        }
        let enabled = js_sys::Reflect::get(&stash, &"enabled".into())
            .ok()
            .and_then(|v| v.as_bool());
        Some(match enabled {
            Some(true) => Verdict::Enabled,
            Some(false) => Verdict::Disabled,
            None => Verdict::Unavailable,
        })
    }

    /// Publish the Rust probe verdict to the bootstrap reporter via its
    /// `setVerdict(label)` function (an undecided JS side then flushes or
    /// falls back without another health request).
    fn publish_verdict(verdict: Verdict) {
        let Some(stash) = bootstrap_stash() else {
            return;
        };
        let Ok(fun) = js_sys::Reflect::get(&stash, &"setVerdict".into()) else {
            return;
        };
        let Ok(fun) = fun.dyn_into::<js_sys::Function>() else {
            return;
        };
        let label = match verdict {
            Verdict::Enabled => "enabled",
            Verdict::Disabled => "disabled",
            Verdict::Unavailable => "unavailable",
        };
        let _ = fun.call1(&stash, &label.into());
    }

    /// Hand a serialized report to the bootstrap reporter's buffer via its
    /// `enqueue(json)` function (used for panic reports while still probing;
    /// the buffer and the JS side survive a WASM abort).
    fn bootstrap_enqueue(report: &BrowserReport) {
        let Ok(body) = serde_json::to_string(report) else {
            return;
        };
        let Some(stash) = bootstrap_stash() else {
            return;
        };
        let Ok(fun) = js_sys::Reflect::get(&stash, &"enqueue".into()) else {
            return;
        };
        let Ok(fun) = fun.dyn_into::<js_sys::Function>() else {
            return;
        };
        let _ = fun.call1(&stash, &JsValue::from_str(&body));
    }

    fn bootstrap_stash() -> Option<JsValue> {
        let window = web_sys::window()?;
        let stash = js_sys::Reflect::get(&window, &"__vortexDiag".into()).ok()?;
        if stash.is_undefined() || stash.is_null() || !stash.is_object() {
            return None;
        }
        Some(stash)
    }

    /// Synchronous fire-and-forget delivery of a report via
    /// `navigator.sendBeacon`. `false` means the browser refused to queue it.
    fn beacon_report(report: &BrowserReport) -> bool {
        let Ok(body) = serde_json::to_string(report) else {
            return false;
        };
        let Some(window) = web_sys::window() else {
            return false;
        };
        window
            .navigator()
            .send_beacon_with_opt_str(DIAGNOSTICS_PATH, Some(&body))
            .unwrap_or(false)
    }

    /// One best-effort send. Never retries; the return value only decides
    /// between silence and the console fallback.
    async fn send_report(report: &BrowserReport) -> bool {
        let Ok(body) = serde_json::to_string(report) else {
            return false;
        };
        match gloo_net::http::Request::post(DIAGNOSTICS_PATH)
            .header("Content-Type", "application/json")
            .body(body)
        {
            Ok(req) => match req.send().await {
                Ok(resp) => resp.status() == 204,
                Err(_) => false,
            },
            Err(_) => false,
        }
    }

    /// Console fallback carries only the fixed enum labels, never payloads.
    fn console_fallback(report: &BrowserReport) {
        web_sys::console::warn_1(
            &format!(
                "[vortex diagnostics] event={} operation={} status={:?} (reporting unavailable)",
                report.event.as_str(),
                report.operation.as_str(),
                report.status
            )
            .into(),
        );
    }

    /// Panic hook: records the location only (line/column), never the
    /// payload or stack text. Also prints a fixed console line so the panic
    /// stays visible in developer tools. When diagnostics are disabled the
    /// default hook is forwarded unchanged (normal behavior is preserved).
    fn install_panic_hook() {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if phase_is_disabled() {
                default_hook(info);
                return;
            }
            web_sys::console::error_1(
                &"vortex: wasm panic (location recorded, payload withheld)".into(),
            );
            report_panic(info.location());
        }));
    }
}

// ---------------------------------------------------------------------------
// Tests (native, pure helpers only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::{BrowserEventKind as K, BrowserOperation as Op};

    const TOKEN_SENTINEL: &str = "token=sk-or-v1-sentinel-secret";

    #[test]
    fn uuid_validation_rejects_injected_text() {
        let good = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(validate_uuid(good).as_deref(), Some(good));
        let upper = good.to_uppercase();
        assert_eq!(validate_uuid(&upper).as_deref(), Some(upper.as_str()));
        assert_eq!(validate_uuid("Bearer abc123"), None);
        assert_eq!(validate_uuid("../etc/passwd"), None);
        assert_eq!(validate_uuid("run-1"), None);
        assert_eq!(validate_uuid(""), None);
        // 36 chars but wrong shape
        assert_eq!(validate_uuid("xxxx-xxxx-xxxx-xxxx-xxxx-xxxx-xxxx"), None);
        assert_eq!(validate_uuid("123e4567-e89b-12d3-a456-42661417400g"), None);
    }

    #[test]
    fn uuid_validation_requires_hyphens_at_fixed_positions() {
        // 36 hex characters with the required hyphens missing: hex characters
        // at the hyphen positions must not be accepted.
        let missing = "a".repeat(36);
        assert_eq!(missing.len(), 36);
        assert_eq!(validate_uuid(&missing), None);
        // Hex character substituted for a required hyphen.
        assert_eq!(validate_uuid("123e45678e9b-12d3-a456-426614174000"), None);
        assert_eq!(validate_uuid("123e4567-e89b-12d3-a456-42661417400-"), None);
        // Hyphen at a wrong position.
        assert_eq!(validate_uuid("123e4567-e89b-12d3-a456-4266141740-00"), None);
        // Extra hyphen where a hex digit belongs.
        assert_eq!(validate_uuid("123e4567--89b-12d3-a456-426614174000"), None);
    }

    #[test]
    fn health_classification_keeps_guard_failures_unavailable() {
        assert_eq!(classify_health(200, Some(true)), Verdict::Enabled);
        assert_eq!(classify_health(200, Some(false)), Verdict::Disabled);
        assert_eq!(classify_health(200, None), Verdict::Unavailable);
        assert_eq!(classify_health(404, None), Verdict::Disabled);
        // Guard denials / server errors are never "confirmed disabled".
        assert_eq!(classify_health(401, None), Verdict::Unavailable);
        assert_eq!(classify_health(403, None), Verdict::Unavailable);
        assert_eq!(classify_health(500, Some(true)), Verdict::Unavailable);
        assert_eq!(classify_health(502, None), Verdict::Unavailable);
    }

    #[test]
    fn location_metadata_is_numeric_and_bounded() {
        let loc = safe_location(Some(12), Some(34)).expect("in range");
        assert_eq!(loc.line, Some(12));
        assert_eq!(loc.column, Some(34));
        assert!(safe_location(Some(0), None).is_none());
        assert!(safe_location(None, Some(0)).is_none());
        assert!(safe_location(Some(MAX_LOCATION_VALUE + 1), None).is_none());
        assert!(safe_location(None, None).is_none());
    }

    #[test]
    fn build_report_drops_everything_but_the_contract() {
        let report = build_report(
            K::Error,
            Op::ApiStatus,
            Some(500),
            Some(TOKEN_SENTINEL),      // non-UUID: dropped
            Some("run 1; drop table"), // non-UUID: dropped
            safe_location(Some(7), None),
        );
        assert_eq!(report.request_id, None);
        assert_eq!(report.run_id, None);
        assert_eq!(report.status, Some(500));
        let body = serde_json::to_string(&report).expect("serializable");
        assert!(!body.contains(TOKEN_SENTINEL));
        assert!(!body.contains("drop table"));
        assert!(!body.contains("message"));
        assert!(!body.contains("stack"));
        assert!(!body.contains("url"));

        // Oversized status (outside 0..=599) is dropped too.
        let report = build_report(K::Error, Op::ApiStatus, Some(600), None, None, None);
        assert_eq!(report.status, None);
    }

    #[test]
    fn run_id_extraction_is_strict_and_query_safe() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(
            extract_run_id(&format!("/runs/{id}/state")).as_deref(),
            Some(id)
        );
        assert_eq!(
            extract_run_id(&format!("/runs/{id}/stop?x=1")).as_deref(),
            Some(id)
        );
        assert_eq!(extract_run_id("/runs/run-1/state"), None);
        assert_eq!(extract_run_id("/conversations/abc/chat"), None);
        assert_eq!(extract_run_id("/api/diagnostics"), None);
        // A query string (which can embed tokens/paths) is never examined.
        assert_eq!(extract_run_id("/settings"), None);
        assert_eq!(
            extract_run_id(&format!("/runs/{id}/state?{TOKEN_SENTINEL}")).as_deref(),
            Some(id)
        );
    }

    #[test]
    fn dedup_suppresses_repeats_within_the_window() {
        let key = (K::Error, Op::SseDecode, None);
        let mut d = Deduper::new(DEDUP_WINDOW_MS);
        assert!(d.should_emit(key, 1_000));
        assert!(!d.should_emit(key, 5_000));
        assert!(!d.should_emit(key, 30_999));
        assert!(d.should_emit(key, 31_000));
        // Different status means a different key: both are reported.
        let mut d2 = Deduper::new(DEDUP_WINDOW_MS);
        assert!(d2.should_emit((K::Error, Op::ApiStatus, Some(401)), 0));
        assert!(d2.should_emit((K::Error, Op::ApiStatus, Some(500)), 0));
        assert!(!d2.should_emit((K::Error, Op::ApiStatus, Some(401)), 1_000));
    }

    #[test]
    fn dedup_state_is_bounded() {
        let mut d = Deduper::new(DEDUP_WINDOW_MS);
        for i in 0..(DEDUP_CAPACITY * 4) {
            let status = 400u16 + i as u16; // distinct key per iteration
            assert!(d.should_emit((K::Error, Op::ApiStatus, Some(status)), i as u64));
        }
        assert!(d.last_seen.len() <= DEDUP_CAPACITY);
    }

    #[test]
    fn buffer_caps_at_32_dropping_oldest() {
        let mut b = EventBuffer::new(BUFFER_CAPACITY);
        for _ in 0..(BUFFER_CAPACITY + 8) {
            b.push(build_report(
                K::Error,
                Op::Bootstrap,
                None,
                None,
                None,
                None,
            ));
        }
        assert_eq!(b.len(), BUFFER_CAPACITY);
        let drained = b.drain();
        assert_eq!(drained.len(), BUFFER_CAPACITY);
        assert!(b.is_empty());
    }

    #[test]
    fn buffer_drop_oldest_keeps_most_recent() {
        let mut b = EventBuffer::new(2);
        b.push(build_report(
            K::Error,
            Op::ApiTransport,
            None,
            None,
            None,
            None,
        ));
        b.push(build_report(
            K::Error,
            Op::ApiStatus,
            Some(500),
            None,
            None,
            None,
        ));
        b.push(build_report(
            K::Error,
            Op::ApiDecode,
            None,
            None,
            None,
            None,
        ));
        let drained = b.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].operation, Op::ApiStatus);
        assert_eq!(drained[1].operation, Op::ApiDecode);
    }

    #[test]
    fn wire_body_has_no_free_text_fields() {
        let report = build_report(
            K::Panic,
            Op::Panic,
            None,
            Some("123e4567-e89b-12d3-a456-426614174000"),
            Some("123e4567-e89b-12d3-a456-426614174099"),
            safe_location(Some(1_000_000), Some(1)),
        );
        let body = serde_json::to_string(&report).expect("serializable");
        assert!(body.contains("\"event\":\"panic\""));
        assert!(body.contains("\"operation\":\"panic\""));
        assert!(body.contains("\"request_id\""));
        assert!(body.contains("\"run_id\""));
        assert!(body.contains("\"location\":{\"line\":1000000,\"column\":1}"));
        // No field exists that could carry text payloads.
        for forbidden in ["message", "error_text", "stack", "url", "query", "header"] {
            assert!(!body.contains(forbidden), "forbidden field {forbidden}");
        }
    }

    #[test]
    fn max_batch_body_stays_under_the_server_limit() {
        let mut b = EventBuffer::new(BUFFER_CAPACITY);
        for _ in 0..BUFFER_CAPACITY {
            b.push(build_report(
                K::Error,
                Op::ApiStatus,
                Some(503),
                Some("123e4567-e89b-12d3-a456-426614174000"),
                Some("123e4567-e89b-12d3-a456-426614174000"),
                safe_location(Some(9_999), Some(9_999)),
            ));
        }
        for report in b.drain() {
            let body = serde_json::to_string(&report).unwrap_or_default();
            assert!(body.len() < 8_192, "single report body exceeds 8 KiB");
        }
    }

    #[test]
    fn panic_fixture_captures_location_not_payload() {
        use std::sync::Mutex;

        let captured: std::sync::Arc<Mutex<Option<(u32, u32)>>> =
            std::sync::Arc::new(Mutex::new(None));
        let slot = std::sync::Arc::clone(&captured);
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(loc) = info.location() {
                *slot.lock().expect("mutex") = Some((loc.line(), loc.column()));
            }
            // Payload deliberately never read.
        }));

        let payload = "sentinel-panic-payload-must-not-escape";
        let _ = std::panic::catch_unwind(|| panic!("boom: {payload}"));
        std::panic::set_hook(prev);

        let (line, column) = captured
            .lock()
            .expect("mutex")
            .take()
            .expect("fixture panic must reach the hook");
        let location = safe_location(Some(line), Some(column)).expect("in-range location");
        let report = build_report(K::Panic, Op::Panic, None, None, None, Some(location));
        let body = serde_json::to_string(&report).expect("serializable");
        assert!(!body.contains(payload));
        assert!(!body.contains("boom"));
        assert!(body.contains("\"event\":\"panic\""));
        assert!(body.contains("\"line\""));
    }
}

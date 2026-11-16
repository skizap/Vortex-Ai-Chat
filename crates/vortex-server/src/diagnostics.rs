//! Session-scoped diagnostics: env-only gate, diagnostics-mode tracing,
//! panic capture, request correlation middleware, and the bounded
//! browser-report endpoint (`POST /api/diagnostics`).
//!
//! Privacy rules enforced here (not by convention):
//! - the gate is read-only environment state, never persisted and never part
//!   of any saved setting;
//! - logs in diagnostics mode carry only fixed labels, server-generated
//!   request ids, and strictly validated ids — never raw URIs, query
//!   strings, header values, bodies, error strings, panic payloads, or any
//!   user-controlled text;
//! - normal (non-diagnostics) launches keep the previous logging behavior
//!   byte-for-byte.

use crate::guard::DenialCategory;
use crate::state::AppState;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use vortex_types::BrowserReport;

/// Trace target for browser-sourced reports.
pub const TARGET: &str = "vortex_diagnostics";
/// Trace target for request correlation and startup/fatal events.
pub const HTTP_TARGET: &str = "vortex_http";

/// Maximum accepted report body (route-local body limit).
pub const BODY_LIMIT_BYTES: usize = 8 * 1024;
/// Global report budget per rolling one-minute window.
pub const RATE_LIMIT_PER_MINUTE: u32 = 120;
pub const REQUEST_ID_HEADER: &str = "x-request-id";
const REQUEST_ID_HEADER_NAME: HeaderName = HeaderName::from_static(REQUEST_ID_HEADER);

/// Fixed fallback for requests that matched no route (no template exists).
const UNMATCHED_ROUTE: &str = "unmatched";

/// Target only audited first-party events: dependency warnings may contain
/// URLs or other sensitive values. `RUST_LOG` always wins when set.
const DIAGNOSTICS_LOG_FILTER: &str = "off,vortex_server=debug,vortex_agents=debug,\
vortex_core=debug,vortex_llm=info,vortex_http=info,vortex_diagnostics=info,vortex_diag=info";

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

/// Diagnostics are on only when `VORTEX_DIAGNOSTICS=1`. The authoritative
/// gate lives in `vortex_core::diag` (single source of truth shared with the
/// engine/LLM failure-boundary events); it is read-only environment state,
/// never persisted and never part of any saved setting.
pub fn enabled() -> bool {
    vortex_core::diag::enabled()
}

/// Initialize tracing. Normal mode is unchanged; diagnostics mode emits
/// timestamped, levelled, target-labelled lines without ANSI escapes.
pub fn init_tracing(diagnostics: bool, config_log_level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if diagnostics {
            DIAGNOSTICS_LOG_FILTER.to_string()
        } else {
            config_log_level.to_string()
        })
    });
    if diagnostics {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_target(true)
            .init();
    } else {
        // Exactly the previous behavior.
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .compact()
            .init();
    }
}

/// Report a fatal startup error without copying the raw error text into any
/// output (config errors can embed file contents/paths). In diagnostics mode
/// a single fixed stderr line is emitted *directly* — the earliest stages
/// (paths/config) fail before the tracing subscriber exists, so tracing
/// alone would swallow them — and the process exits. Normal mode returns the
/// error unchanged so existing reporting behavior is preserved.
pub fn stage_fatal(stage: &'static str, error: anyhow::Error) -> anyhow::Error {
    if enabled() {
        eprintln!(
            "[vortex] startup failed at stage: {stage} (error details intentionally not logged)"
        );
        std::process::exit(1);
    }
    error
}

// ---------------------------------------------------------------------------
// Panic capture
// ---------------------------------------------------------------------------

thread_local! {
    /// Per-thread recursion guard: prevents infinite loops if tracing itself
    /// panics while we log, without dropping recordings from *other* threads
    /// panicking concurrently (a global guard would swallow those).
    static PANIC_RECURSION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
/// Count of recorded panics; exposed for test fixtures.
pub static PANIC_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Install the diagnostics panic hook. Records only the panic *location*
/// (a sanitized source filename) and the current span *name*
/// (static metadata from source code; span fields are never read); the
/// payload is never read or logged. The hook is process-global, so panics
/// inside detached coordinator tasks are recorded at the panic point
/// regardless of where the task was spawned, and concurrent panics on
/// different threads are each recorded once.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        // Extract compiler-provided location data before any processing; the
        // payload is never touched.
        let location = info
            .location()
            .map(|l| (l.file().to_owned(), l.line(), l.column()));
        record_panic(location);
    }));
}

fn safe_panic_file(file: &str) -> &str {
    let name = file.rsplit(['/', '\\']).next().unwrap_or_default();
    if !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
    {
        name
    } else {
        "<unknown>"
    }
}

fn record_panic(location: Option<(String, u32, u32)>) {
    // Never recurse if tracing itself panics while we log (per-thread).
    if PANIC_RECURSION.with(|c| c.replace(true)) {
        return;
    }
    PANIC_EVENTS.fetch_add(1, Ordering::SeqCst);
    let (file, line, col) = location.unwrap_or_default();
    let file = safe_panic_file(&file);
    let span = tracing::Span::current()
        .metadata()
        .map(|m| m.name())
        .unwrap_or("<none>");
    tracing::error!(
        target: HTTP_TARGET,
        panic = true,
        file,
        line,
        col,
        span,
        "panic captured (payload not recorded)"
    );
    PANIC_RECURSION.with(|c| c.set(false));
}

// ---------------------------------------------------------------------------
// Request correlation middleware (outside the guard)
// ---------------------------------------------------------------------------

/// Map the request method to a fixed allowlisted label. Non-standard method
/// strings are never echoed.
fn safe_method(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

/// Outermost middleware: adds a generated request id to every response and,
/// in diagnostics mode, logs a fully sanitized request line (allowlisted
/// method, matched route template or a fixed fallback, status, latency, and
/// a fixed denial category from the guard). Raw paths, queries, and header
/// values are never logged.
pub async fn correlation(req: Request, next: Next) -> Response {
    correlation_inner(req, next, enabled()).await
}

pub(crate) async fn correlation_inner(req: Request, next: Next, diagnostics: bool) -> Response {
    let start = std::time::Instant::now();
    let method = safe_method(req.method());
    // `MatchedPath` is the route *template* (e.g. `/api/runs/{id}/events`),
    // inserted by the router before layered middleware runs. No match (404
    // fallback or a guard denial before routing) uses the fixed label.
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| UNMATCHED_ROUTE.to_owned());

    let mut resp = next.run(req).await;
    if !diagnostics {
        return resp;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert(REQUEST_ID_HEADER_NAME, value);
    }
    let denial = resp.extensions().get::<DenialCategory>().copied();
    tracing::info!(
        target: HTTP_TARGET,
        request_id = request_id.as_str(),
        method,
        route = route.as_str(),
        status = resp.status().as_u16(),
        latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        denial = denial.map(|d| d.as_str()),
        "request handled"
    );
    resp
}

// ---------------------------------------------------------------------------
// Swallowed-failure events (fixed categories, validated ids only)
// ---------------------------------------------------------------------------

/// Log a database failure that a route intentionally swallows to keep its
/// response shape. Only active in diagnostics mode (ordinary launches keep
/// their existing silent behavior). `id` is logged only when it parses as a
/// UUID; otherwise a fixed `<invalid>` label is used. The error string
/// itself is never logged.
pub fn db_failure(op: &'static str, id: Option<&str>) {
    db_failure_gated(enabled(), op, id);
}

/// Gate-injectable core (testable without touching process env).
pub(crate) fn db_failure_gated(gate: bool, op: &'static str, id: Option<&str>) {
    if !gate {
        return;
    }
    let id = match id.and_then(|s| uuid::Uuid::parse_str(s).ok()) {
        Some(u) => u.to_string(),
        None => "<invalid>".to_string(),
    };
    tracing::warn!(
        target: HTTP_TARGET,
        category = "db",
        op,
        id = id.as_str(),
        "database operation failed (details not logged)"
    );
}

// ---------------------------------------------------------------------------
// Rate limiting (bounded, concurrency-safe rolling window)
// ---------------------------------------------------------------------------

/// Global rolling-window limiter: a single mutex-protected `VecDeque` of
/// accepted-timestamps. Strictly at most `limit` events in *any* rolling
/// `window_ms` window (no fixed-window boundary bursts), and the queue is
/// bounded to at most `limit` entries regardless of request volume — O(1)
/// memory and eviction per acquire. An expired-entry eviction before each
/// push keeps concurrent acquires serialized and exact.
pub struct RateLimiter {
    window_ms: u64,
    limit: usize,
    started: std::time::Instant,
    events: Mutex<VecDeque<u64>>,
}

impl RateLimiter {
    pub fn per_minute(limit: u32) -> Self {
        Self {
            window_ms: 60_000,
            limit: limit as usize,
            started: std::time::Instant::now(),
            events: Mutex::new(VecDeque::with_capacity(limit as usize)),
        }
    }

    /// Try to consume one slot at time `now_ms` (injectable for deterministic
    /// tests). Returns false when the rolling window is already full.
    pub fn try_acquire_at(&self, now_ms: u64) -> bool {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Evict timestamps that have fully left the rolling window.
        while events
            .front()
            .is_some_and(|&t| now_ms.saturating_sub(t) >= self.window_ms)
        {
            events.pop_front();
        }
        if events.len() < self.limit {
            events.push_back(now_ms);
            true
        } else {
            false
        }
    }

    /// Use monotonic elapsed time so wall-clock changes cannot reset the budget.
    pub fn try_acquire_now(&self) -> bool {
        let now = self.started.elapsed().as_millis();
        self.try_acquire_at(u64::try_from(now).unwrap_or(u64::MAX))
    }

    /// Current queue depth (test-only; proves the state bound).
    #[cfg(test)]
    fn window_len(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

// ---------------------------------------------------------------------------
// Browser report endpoint
// ---------------------------------------------------------------------------

/// Runtime state for the diagnostics endpoint (held by `AppState`).
pub struct DiagnosticsState {
    pub enabled: bool,
    limiter: RateLimiter,
}

impl DiagnosticsState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            limiter: RateLimiter::per_minute(RATE_LIMIT_PER_MINUTE),
        }
    }
}

/// Validate the already-deserialized report against the bounds serde cannot
/// express (uuid shape, status range, location magnitude).
fn validate_report(report: &BrowserReport) -> Result<(), ()> {
    if let Some(status) = report.status {
        if status > 599 {
            return Err(());
        }
    }
    if let Some(id) = &report.request_id {
        uuid::Uuid::parse_str(id).map_err(|_| ())?;
    }
    if let Some(id) = &report.run_id {
        uuid::Uuid::parse_str(id).map_err(|_| ())?;
    }
    if let Some(loc) = &report.location {
        if loc.line.unwrap_or(0) > 1_000_000 || loc.column.unwrap_or(0) > 1_000_000 {
            return Err(());
        }
    }
    Ok(())
}

/// Parse, validate, and trace one already-gated report: strict
/// deny-unknown-fields deserialization, validation, sanitized trace. Returns
/// fixed-status responses only; the request body is never echoed or logged.
fn parse_and_trace(body: &[u8]) -> Response {
    let report: BrowserReport = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "invalid diagnostic report\n").into_response();
        }
    };
    if validate_report(&report).is_err() {
        return (StatusCode::BAD_REQUEST, "invalid diagnostic report\n").into_response();
    }
    // Only fixed enums, validated uuids, and small numbers reach the log;
    // `source=browser` is applied by the server, never taken from input.
    tracing::info!(
        target: TARGET,
        source = "browser",
        event = report.event.as_str(),
        operation = report.operation.as_str(),
        status = report.status,
        request_id = report.request_id.as_deref(),
        run_id = report.run_id.as_deref(),
        line = report.location.as_ref().and_then(|l| l.line),
        column = report.location.as_ref().and_then(|l| l.column),
        "browser diagnostic report"
    );
    StatusCode::NO_CONTENT.into_response()
}

/// Outcome of reading the report body under the size cap.
enum BodyRead {
    Ok(Vec<u8>),
    TooLarge,
    Invalid,
}

/// Read the request body with a hard cap, rejecting oversize bodies with
/// `TooLarge` without buffering them. Used instead of a body *extractor* so
/// the disabled-gate (404) and rate limit are evaluated BEFORE any body is
/// pulled — a disabled endpoint must answer 404 even for oversized bodies.
async fn read_capped_body(body: axum::body::Body, cap: usize) -> BodyRead {
    use futures::StreamExt;
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return BodyRead::Invalid,
        };
        if buf.len() + chunk.len() > cap {
            return BodyRead::TooLarge;
        }
        buf.extend_from_slice(&chunk);
    }
    BodyRead::Ok(buf)
}

/// Post-gate flow shared by the real handler and tests: rate limit → size
/// checks → capped read → parse/trace.
pub(crate) async fn serve_gated_report(d: &DiagnosticsState, req: Request) -> Response {
    if !d.limiter.try_acquire_now() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many diagnostic reports\n",
        )
            .into_response();
    }
    if let Some(len) = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if len > BODY_LIMIT_BYTES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "diagnostic report too large\n",
            )
                .into_response();
        }
    }
    match read_capped_body(req.into_body(), BODY_LIMIT_BYTES).await {
        BodyRead::Ok(body) => parse_and_trace(&body),
        BodyRead::TooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "diagnostic report too large\n",
        )
            .into_response(),
        BodyRead::Invalid => {
            (StatusCode::BAD_REQUEST, "invalid diagnostic report\n").into_response()
        }
    }
}

/// Axum handler for `POST /api/diagnostics`. Gate → rate limit → size checks
/// happen before/instead of body extraction, so a disabled endpoint always
/// answers 404 regardless of body size, and oversized bodies always answer
/// 413. The `Content-Length` pre-check rejects large bodies without reading
/// them; the streaming cap covers chunked bodies. This cap is enforced by
/// the handler itself, so it holds on any router it is mounted on.
pub async fn report(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let d = &state.diagnostics;
    if !d.enabled {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }
    serve_gated_report(d, req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::{guard, GuardSettings};
    use axum::extract::DefaultBodyLimit;
    use axum::routing::{get, post};
    use axum::Router;

    // -- pure helpers -------------------------------------------------------

    #[test]
    fn methods_outside_allowlist_map_to_fixed_label() {
        assert_eq!(safe_method(&Method::GET), "GET");
        assert_eq!(safe_method(&Method::POST), "POST");
        // A valid but non-standard method string is never echoed.
        let custom = Method::from_bytes(b"TRACK").unwrap();
        assert_eq!(safe_method(&custom), "OTHER");
    }

    #[test]
    fn panic_filename_excludes_directories_and_untrusted_characters() {
        assert_eq!(safe_panic_file("/secret/path/engine.rs"), "engine.rs");
        assert_eq!(safe_panic_file("C:\\secret\\mod.rs"), "mod.rs");
        assert_eq!(safe_panic_file("injected\nline.rs"), "<unknown>");
        assert_eq!(safe_panic_file(&"a".repeat(101)), "<unknown>");
        assert_eq!(safe_panic_file(""), "<unknown>");
    }

    #[test]
    fn rolling_limiter_enforces_exact_limit_and_stays_bounded() {
        let rl = RateLimiter::per_minute(3);
        assert!(rl.try_acquire_at(0));
        assert!(rl.try_acquire_at(1));
        assert!(rl.try_acquire_at(59_999));
        assert!(!rl.try_acquire_at(59_999), "window full at limit");
        assert!(rl.window_len() == 3, "queue bounded at limit");
        // Rolling window: t=0 leaves exactly at t=60_000.
        assert!(rl.try_acquire_at(60_000), "oldest entry expired");
        assert!(rl.try_acquire_at(60_001), "t=1 expired now");
        assert!(!rl.try_acquire_at(60_002), "full again after 3 in window");
        // Many rejected attempts must not grow the queue.
        for _ in 0..1000 {
            assert!(!rl.try_acquire_at(60_003));
        }
        assert_eq!(rl.window_len(), 3, "state bound holds under rejection");
    }

    #[test]
    fn rolling_limiter_has_no_boundary_burst() {
        let rl = RateLimiter::per_minute(120);
        for _ in 0..120 {
            assert!(rl.try_acquire_at(0));
        }
        // Still full one millisecond before the (former) fixed-window edge.
        assert!(!rl.try_acquire_at(59_999));
        // And right after it, only what actually expired is available: the
        // whole t=0 batch leaves at once here, so exactly 120 more fit.
        for _ in 0..120 {
            assert!(rl.try_acquire_at(60_000));
        }
        assert!(!rl.try_acquire_at(60_001));
    }

    #[test]
    fn rolling_limiter_window_is_sliding_not_fixed() {
        let rl = RateLimiter::per_minute(3);
        assert!(rl.try_acquire_at(0));
        assert!(rl.try_acquire_at(30_000));
        // Only 2 in the window (nothing expired), 1 free slot.
        assert!(rl.try_acquire_at(45_000));
        assert!(!rl.try_acquire_at(45_001), "3 in the last 45 s: denied");
        // At 60_000+ the t=0 entry expires; t=30_000 still counts.
        assert!(rl.try_acquire_at(60_001));
        assert!(!rl.try_acquire_at(60_002), "t=30_000 is still in window");
        assert!(rl.try_acquire_at(90_001), "t=30_000 expires at 90_000");
    }

    #[test]
    fn rolling_limiter_is_exact_under_concurrency() {
        use std::sync::atomic::AtomicUsize;
        let rl = Arc::new(RateLimiter::per_minute(120));
        let accepted = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let rl = rl.clone();
            let accepted = accepted.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    if rl.try_acquire_at(42_000) {
                        accepted.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            120,
            "concurrent acquires must consume exactly the budget, no more"
        );
        assert!(rl.window_len() <= 120, "bounded state under concurrency");
    }

    // -- report endpoint ----------------------------------------------------

    fn valid_body() -> String {
        serde_json::json!({
            "event": "error",
            "operation": "api_status",
            "status": 500,
        })
        .to_string()
    }

    /// Test-facing report call: full gate (404/429) then parse/trace, the
    /// same flow the route handler performs.
    fn post_report(d: &DiagnosticsState, body: &str) -> Response {
        if !d.enabled {
            return (StatusCode::NOT_FOUND, "not found\n").into_response();
        }
        if !d.limiter.try_acquire_now() {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "too many diagnostic reports\n",
            )
                .into_response();
        }
        parse_and_trace(body.as_bytes())
    }

    #[test]
    fn disabled_gate_returns_404() {
        let d = DiagnosticsState::new(false);
        assert_eq!(
            post_report(&d, &valid_body()).status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn valid_report_is_accepted_with_204() {
        let d = DiagnosticsState::new(true);
        assert_eq!(
            post_report(&d, &valid_body()).status(),
            StatusCode::NO_CONTENT
        );
    }

    #[test]
    fn optional_fields_are_validated() {
        let d = DiagnosticsState::new(true);
        let ok = serde_json::json!({
            "event": "rejection",
            "operation": "sse_decode",
            "request_id": uuid::Uuid::new_v4().to_string(),
            "run_id": uuid::Uuid::new_v4().to_string(),
            "location": {"line": 12, "column": 34},
        });
        assert_eq!(
            post_report(&d, &ok.to_string()).status(),
            StatusCode::NO_CONTENT
        );

        for bad in [
            serde_json::json!({"event": "error", "operation": "api_status", "request_id": "not-a-uuid"}),
            serde_json::json!({"event": "error", "operation": "api_status", "run_id": "../../etc/passwd"}),
            serde_json::json!({"event": "error", "operation": "api_status", "status": 600}),
            serde_json::json!({"event": "error", "operation": "api_status", "status": 100_000}),
            serde_json::json!({"event": "error", "operation": "api_status", "location": {"line": 2_000_000}}),
            serde_json::json!({"event": "error", "operation": "api_status", "location": {"column": 2_000_000}}),
            serde_json::json!({"event": "bogus", "operation": "api_status"}),
            serde_json::json!({"event": "error", "operation": "bogus"}),
        ] {
            assert_eq!(
                post_report(&d, &bad.to_string()).status(),
                StatusCode::BAD_REQUEST,
                "must reject: {bad}"
            );
        }
    }

    #[test]
    fn unknown_fields_and_free_text_are_rejected() {
        let d = DiagnosticsState::new(true);
        let attempts = [
            // Unknown top-level field carrying injection text.
            serde_json::json!({
                "event": "error", "operation": "api_status",
                "message": "'; DROP TABLE runs; --"
            }),
            // Unknown nested field inside location.
            serde_json::json!({
                "event": "error", "operation": "api_status",
                "location": {"line": 1, "file": "/etc/passwd"}
            }),
            // Unrequested extra fields of any shape.
            serde_json::json!({
                "event": "error", "operation": "api_status", "url": "http://127.0.0.1/?token=x"
            }),
            serde_json::json!({
                "event": "error", "operation": "api_status", "stack": "at fn (x.js:1)"
            }),
            serde_json::json!({
                "event": "error", "operation": "api_status", "error": "provider said: bad key"
            }),
        ];
        for bad in attempts {
            assert_eq!(
                post_report(&d, &bad.to_string()).status(),
                StatusCode::BAD_REQUEST,
                "must reject: {bad}"
            );
        }
    }

    #[test]
    fn malformed_json_is_rejected() {
        let d = DiagnosticsState::new(true);
        for bad in ["", "{", "[]", "null", "\"str\"", "event=error"] {
            assert_eq!(
                post_report(&d, bad).status(),
                StatusCode::BAD_REQUEST,
                "must reject: {bad}"
            );
        }
    }

    #[test]
    fn flood_is_rate_limited() {
        let d = DiagnosticsState::new(true);
        for _ in 0..RATE_LIMIT_PER_MINUTE {
            assert_eq!(
                post_report(&d, &valid_body()).status(),
                StatusCode::NO_CONTENT
            );
        }
        assert_eq!(
            post_report(&d, &valid_body()).status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    // -- diagnostics-mode default filter ---------------------------------------

    #[test]
    fn diagnostics_default_filter_enables_the_explicit_targets() {
        let capture = Capture(Arc::new(Mutex::new(Vec::new())));
        let filter = tracing_subscriber::EnvFilter::new(DIAGNOSTICS_LOG_FILTER);
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info!(target: HTTP_TARGET, "corr probe");
        tracing::info!(target: TARGET, "browser probe");
        tracing::info!(target: "vortex_diag", "engine probe");
        let out = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
        assert!(out.contains("corr probe"), "vortex_http must pass: {out}");
        assert!(
            out.contains("browser probe"),
            "vortex_diagnostics must pass: {out}"
        );
        assert!(out.contains("engine probe"), "vortex_diag must pass: {out}");
        // Uncontrolled dependency warnings may contain credentials or URLs.
        tracing::warn!(target: "some_dependency", "quiet probe");
        let out = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
        assert!(
            !out.contains("quiet probe"),
            "dependency warnings must stay quiet: {out}"
        );
    }

    // -- swallowed-DB-failure event gating ------------------------------------

    /// Thread-local capture writer for asserting exactly what is logged.
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn db_failure_events_are_gated_and_sanitized() {
        // Disabled: ordinary launches keep their existing silent behavior.
        {
            let capture = Capture(Arc::new(Mutex::new(Vec::new())));
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(capture.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            db_failure_gated(false, "get_run", Some("'; DROP TABLE runs; --"));
            assert!(
                capture.0.lock().unwrap().is_empty(),
                "disabled mode must not emit db failure events"
            );
        }
        // Enabled: fixed fields only; the raw id and any injected text are
        // never echoed.
        {
            let capture = Capture(Arc::new(Mutex::new(Vec::new())));
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(capture.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            db_failure_gated(true, "get_run", Some("'; DROP TABLE runs; --"));
            let out = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
            assert!(out.contains(r#"category="db""#), "got: {out}");
            assert!(out.contains(r#"op="get_run""#), "got: {out}");
            assert!(out.contains(r#"id="<invalid>""#), "got: {out}");
            assert!(
                !out.contains("DROP"),
                "injection text must not appear: {out}"
            );
            // A valid UUID id is echoed in canonical form.
            let capture2 = Capture(Arc::new(Mutex::new(Vec::new())));
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(capture2.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            db_failure_gated(
                true,
                "get_run",
                Some("00000000-0000-4000-8000-000000000009"),
            );
            let out = String::from_utf8_lossy(&capture2.0.lock().unwrap()).to_string();
            assert!(
                out.contains("00000000-0000-4000-8000-000000000009"),
                "got: {out}"
            );
        }
    }

    // -- panic hook fixture ---------------------------------------------------

    /// Single panic-hook test (set_hook/take_hook are process-global, so all
    /// panic-fixture assertions live here to avoid cross-test races).
    #[test]
    fn panic_hook_records_concurrent_and_detached_panics_without_payload() {
        let previous_hook = std::panic::take_hook();
        install_panic_hook();
        let before = PANIC_EVENTS.load(Ordering::SeqCst);

        // Exercise the actual hook with a capturing subscriber on this thread.
        let capture = Capture(Arc::new(Mutex::new(Vec::new())));
        {
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(capture.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            let result = std::panic::catch_unwind(|| panic!("SENTINEL-SECRET payload"));
            assert!(result.is_err());
        }
        let output = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
        assert!(output.contains("panic captured"));
        assert!(!output.contains("SENTINEL-SECRET"));

        // Concurrent panics: every thread must be recorded exactly once
        // (per-thread recursion guard), payloads ("SENTINEL-SECRET") are
        // never read.
        let mut handles = Vec::new();
        for i in 0..8 {
            handles.push(std::thread::spawn(move || panic!("SENTINEL-SECRET {i}")));
        }
        for handle in handles {
            assert!(handle.join().is_err(), "thread must actually panic");
        }
        // A detached (never-joined) panicking thread signals after the hook
        // runs, avoiding a scheduler-dependent sleep in the fixture.
        struct NotifyOnDrop(std::sync::mpsc::Sender<()>);
        impl Drop for NotifyOnDrop {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        drop(std::thread::spawn(move || {
            let _notify = NotifyOnDrop(tx);
            panic!("SENTINEL-SECRET detached");
        }));
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let after = PANIC_EVENTS.load(Ordering::SeqCst);
        assert_eq!(
            after - before,
            10,
            "1 captured, 8 concurrent, and 1 detached panic must each be recorded once"
        );
        std::panic::set_hook(previous_hook);
    }

    // -- middleware + guard integration (routers mirroring build_router) ------

    /// Mirrors the real `build_router` layering (route → guard → correlation
    /// → global 2 MiB `DefaultBodyLimit`) so body-cap and gate behavior are
    /// exercised under the actual layer stack, not only a bare router.
    async fn spawn_test_server(diagnostics: bool) -> String {
        let dstate = Arc::new(DiagnosticsState::new(diagnostics));
        let guard_cfg = GuardSettings {
            lan_enabled: false,
            lan_token: Arc::from(""),
        };
        let app = Router::new()
            .route("/api/diagnostics", post(test_report))
            .route("/ping", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(guard_cfg, guard))
            .layer(axum::middleware::from_fn(
                move |req: Request, next: Next| async move {
                    correlation_inner(req, next, diagnostics).await
                },
            ))
            .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
            .with_state(dstate);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn test_report(State(ds): State<Arc<DiagnosticsState>>, req: Request) -> Response {
        // Same gate-before-extraction flow as the real handler.
        if !ds.enabled {
            return (StatusCode::NOT_FOUND, "not found\n").into_response();
        }
        serve_gated_report(&ds, req).await
    }

    #[tokio::test]
    async fn disabled_mode_serves_404_and_no_correlation_header() {
        let base = spawn_test_server(false).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/api/diagnostics"))
            .header("Host", "127.0.0.1")
            .body(valid_body())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        assert!(resp.headers().get(REQUEST_ID_HEADER).is_none());
        // Normal routes work and carry no header either.
        let resp = client.get(format!("{base}/ping")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get(REQUEST_ID_HEADER).is_none());
    }

    #[tokio::test]
    async fn disabled_gate_answers_404_even_for_oversized_bodies() {
        let base = spawn_test_server(false).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/diagnostics");
        // Just over the 8 KiB cap: the disabled gate must win over the size
        // check (404, never 413).
        let big = "x".repeat(BODY_LIMIT_BYTES + 1);
        let resp = client
            .post(&url)
            .header("Host", "127.0.0.1")
            .body(big)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // A body beyond even the global 2 MiB router limit must also be
        // gated first (404, not a body-limit rejection).
        let huge = "y".repeat(3 * 1024 * 1024);
        let resp = client
            .post(&url)
            .header("Host", "127.0.0.1")
            .body(huge)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn enabled_mode_adds_request_id_and_protects_route_with_guard() {
        let base = spawn_test_server(true).await;
        let client = reqwest::Client::new();

        // Guard denial (bad host) is still protected and still correlated.
        let resp = client
            .get(format!("{base}/ping"))
            .header("Host", "evil.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        let id = resp
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("denials carry the request id")
            .to_str()
            .unwrap()
            .to_string();
        uuid::Uuid::parse_str(&id).expect("request id is a uuid");

        // A valid report is accepted and carries a fresh request id.
        let resp = client
            .post(format!("{base}/api/diagnostics"))
            .header("Host", "127.0.0.1")
            .body(valid_body())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        let id2 = resp
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("reports carry the request id")
            .to_str()
            .unwrap()
            .to_string();
        uuid::Uuid::parse_str(&id2).expect("request id is a uuid");
        assert_ne!(id, id2, "ids are per-request");
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_with_413() {
        let base = spawn_test_server(true).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/diagnostics");
        let big = "x".repeat(BODY_LIMIT_BYTES + 1);
        let resp = client
            .post(&url)
            .header("Host", "127.0.0.1")
            .body(big)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
        // Exactly at the cap but invalid JSON → 400, not 413.
        let at_cap = "x".repeat(BODY_LIMIT_BYTES);
        let resp = client
            .post(&url)
            .header("Host", "127.0.0.1")
            .body(at_cap)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        // A body beyond even the global 2 MiB router limit still answers 413
        // via the content-length pre-check (rejected without reading).
        let huge = "z".repeat(3 * 1024 * 1024);
        let resp = client
            .post(&url)
            .header("Host", "127.0.0.1")
            .body(huge)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
    }

    #[tokio::test]
    async fn flood_over_http_hits_429() {
        let base = spawn_test_server(true).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/diagnostics");
        for _ in 0..RATE_LIMIT_PER_MINUTE {
            let resp = client
                .post(&url)
                .header("Host", "127.0.0.1")
                .body(valid_body())
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 204);
        }
        let resp = client
            .post(&url)
            .header("Host", "127.0.0.1")
            .body(valid_body())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 429);
    }

    // -- route template resolution --------------------------------------------

    #[tokio::test]
    async fn matched_path_template_is_visible_to_layer_middleware() {
        // The correlation middleware relies on `MatchedPath` being present in
        // `Router::layer` middleware (axum inserts it before layers run).
        let seen: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let seen_for_layer = seen.clone();
        let app = Router::new()
            .route("/api/runs/{id}/events", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(
                move |req: Request, next: Next| {
                    let seen = seen_for_layer.clone();
                    async move {
                        let path = req
                            .extensions()
                            .get::<MatchedPath>()
                            .map(|m| m.as_str().to_owned());
                        *seen.lock().unwrap() = path;
                        next.run(req).await
                    }
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        reqwest::Client::new()
            .get(format!("http://{addr}/api/runs/abc/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap().as_deref(),
            Some("/api/runs/{id}/events"),
            "layer middleware must see the route template, never the raw path"
        );
    }
}

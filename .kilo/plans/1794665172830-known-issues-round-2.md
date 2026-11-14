# KNOWN_ISSUES Round 2 — Fix Plan (C7, P1, P9, P12, P14, P15, P17 + N8)

Goal: close the remaining actionable OPEN items from `docs/KNOWN_ISSUES.md`
(full round, user-confirmed), plus one new finding (N8) discovered during
planning. All decisions below are settled — execute in order, verify per item,
then update docs.

Conventions: Rust 1.89 pin; `cargo fmt/clippy` clean native **and**
`-p vortex-ui --target wasm32-unknown-unknown`; tests offline-only; Linux-only
assumptions allowed (project invariant); no commits unless the user asks.

---

## Task 1 — C7 server core + N8: run event log, ordered drain, provider-task abort

**N8 (new finding, record in KNOWN_ISSUES):** in `engine.rs::run_loop`, the
cancel/deadline `tokio::select!` arms return without aborting the spawned
provider `call` task — dropping the JoinHandle leaves the completion running
detached (stream keeps being consumed/billed after cancellation). Also both
arms and the success path call `fwd.abort()`, dropping queued-but-unforwarded
deltas/events (part of C7).

Changes in `crates/vortex-agents/src/hub.rs`:
- Change broadcast payload from `RunEvent` to `(u64, RunEvent)` (seq-stamped).
- `RunHandle` gains `seq: AtomicU64` and `log: Mutex<VecDeque<(u64, RunEvent)>>`
  (cap `MAX_RUN_LOG = 8192` entries, pop_front on overflow). Log lives and dies
  with the handle (runs never survive restarts — they're marked interrupted —
  so persistence buys nothing).
- `Hub::publish`: `seq = fetch_add(1)`; push `(seq, event.clone())` to the log
  under the lock **before** `tx.send((seq, event))`, so a subscriber that
  registers + replays can never miss an event between replay and live stream
  (duplicates are possible instead — filtered per-connection in Task 5).
- Add `Hub::replay_from(run_id, since) -> (Vec<(u64, RunEvent)>, Option<u64>)`
  returning buffered events with `seq > since` plus `oldest_retained` (None if
  log empty).
- Poisoning-tolerant locks (match existing style).

Changes in `crates/vortex-agents/src/engine.rs`:
- `run_loop` success path: after `call` resolves (its `llm_tx` is dropped),
  replace `fwd.abort()` with `let _ = fwd.await;` — the delta forwarder drains
  the channel and exits naturally, so no tail deltas are lost.
- `run_loop` cancel/deadline arms: `call.abort();` **and** `fwd.abort();`
  (N8 fix), then return.
- `execute_run`: replace `fwd.abort()` after `run_loop` returns with
  `let _ = fwd.await;`. Safety: `run_loop` owns `ctx`; every tool task is
  joined inside `execute_tool_calls`, so all `ctx.events` senders are dropped
  by the time `run_loop` returns → forwarder drains queued tool events and
  exits → `finish()` publishes `RunFinished` strictly last. (Verify no other
  `ToolContext` clone can outlive `run_loop`; if the approval manager or
  previews hold one, bound the drain with a 2 s `tokio::time::timeout` and
  abort after.)

Tests (`crates/vortex-agents` unit/integration):
- Hub: publish N events → `replay_from(since)` returns exactly `seq > since`
  in order; overflow evicts oldest and `oldest_retained` advances.
- Engine (MockLlm): a run that emits tool events completes with all
  `ToolCallFinished` events observable from a root subscription **before**
  `RunFinished` (subscribe via hub, collect, assert ordering).
- Engine: cancelling mid-completion aborts the provider task — assert
  `MockLlm::calls()` stops growing / task finishes promptly (use
  `MockLlm::with_delay`).

## Task 2 — P15: token budget enforced before each provider call

`crates/vortex-agents/src/engine.rs::run_loop`:
- Before building `CompletionRequest`:
  `remaining = settings.agents.max_total_tokens.saturating_sub(total_usage.total_tokens)`;
  if `remaining == 0` → `RunError::Failed("token budget exhausted before the next model call; summarize progress")`.
- `req.max_tokens = settings.max_tokens.min(remaining.min(u32::MAX as u64) as u32)`
  (provider-side clamp bounds the single-iteration overshoot to one capped
  completion). Keep the existing post-tool check as a backstop.

Test (`tests/engine.rs`): scripted turn returning `usage.total_tokens` above
the configured `max_total_tokens` → run fails with the budget error and
`llm.calls() == 1` (no second call).

## Task 3 — P1: SSRF IP pinning (close the DNS-rebinding TOCTOU)

`crates/vortex-agents/src/safe_http.rs` + `html.rs`:
- Refactor `ensure_not_private` into `validated_addrs(host, port, resolver) -> Result<Vec<SocketAddr>>`
  with an injectable async resolver (default: `tokio::net::lookup_host`);
  keep the reject-if-**any**-address-is-private rule. Add `is_private_ip`
  unchanged.
- `check_url` (model-chosen URLs, `allow_private_hosts == false`) now returns
  the validated addr list; `fetch_text` picks `addrs[0]` and builds a
  per-request client:
  `reqwest::Client::builder().user_agent(..).connect_timeout(10s).timeout(20s).redirect(Policy::none()).resolve(host, port, pinned_addr).build()`.
  Request the **original** URL — `.resolve()` overrides DNS only; TLS SNI and
  certificate verification still use the hostname. Re-resolve + re-pin on
  every redirect hop (existing per-hop `check_url` loop).
- `allow_private_hosts == true` (SearXNG base, user-configured): keep the
  existing shared unpinned client.
- Document cost: one client build per hop (TLS config clone) — acceptable at
  tool-call frequency; note pooling as a future option.

Tests: stub-resolver unit tests — (a) resolver returns public then private on
second call (simulated rebinding): pin uses the validated addr, and a
re-validation failure aborts the hop; (b) `pick` = first validated addr;
(c) existing `MockSearchProvider`/local-server integration test still passes
via the allow-private path. Add `127.0.0.1`-resolving hostname fetch against
the local mock server **with** `allow_private_hosts=true` to keep it offline.

## Task 4 — P9: dev-server port detection verified by listener ownership

`crates/vortex-agents/src/tools_preview.rs`:
- `fn listening_ports() -> HashSet<u16>`: parse `/proc/net/tcp` and
  `/proc/net/tcp6` (skip header; field 1 = `hexip:hexport` local address,
  field 3 = state, `0A` = LISTEN).
- Snapshot before spawn in `run_process_preview`; pass to `wait_for_port`.
- `wait_for_port(child, before, deadline)` (parameterize the 20 s deadline for
  tests): accept a regex-matched port only if it appears in
  `listening_ports() - before`; if no regex match by deadline but exactly one
  new listener port exists, use it; keep the 8417 exclusion.

Tests: spawn `sh -c 'python3 -m http.server ...'`? No — stay dependency-free:
spawn a tiny tokio listener task in-process printing its URL (already the
pattern in `preview_server.rs` tests) and assert detection; negative test:
child prints a port it does **not** listen on → rejected (short deadline).

## Task 5 — C7 server route: SSE ids, Last-Event-ID/`since` replay, gap signal

`crates/vortex-server/src/routes_runs.rs::run_events`:
- Accept `Last-Event-ID` request header (browser auto-reconnect) **and**
  `?since=<seq>` query param (UI manual re-subscribe); `since = max(header, query)`.
- Live runs: `hub.subscribe()` first, then `hub.replay_from(id, since)`:
  - If `since` < `oldest_retained` → emit named event `replay_gap` (data =
    oldest retained seq) before continuing (client falls back to full resync).
  - Emit replayed events as `Event::default().id(seq.to_string()).json_data(ev)`.
  - Then stream live `(seq, ev)` from the broadcast, **skipping `seq <= last_sent`**
    (server-side per-connection dedup for the replay/live overlap). Keep the
    `lagged` → `replay_gap` mapping (client resyncs; with the log, a following
    reconnect can replay properly).
- Terminal/unknown runs: unchanged synthesized `RunFinished`.

Test: integration-style test with a Hub + axum route (existing local-server
pattern): publish events, connect with `?since=`, assert exact replay set,
id fields present, dedup (no event delivered twice), and `replay_gap` for an
ancient `since`.

## Task 6 — P17 backend: checkpoint pagination, DTO, recording flaw, routes

`crates/vortex-core/src/store_ckpts.rs`:
- `list_checkpoints(conversation_id, before: Option<i64>, limit: u32 /*clamp 1..=200*/)`
  → cursor query `WHERE conversation_id = ?1 AND (?2 IS NULL OR id < ?2) ORDER BY id DESC LIMIT ?3`.
- Fix recording flaw in `crates/vortex-agents/src/tools_write.rs::checkpoint`:
  distinguish *absent* (`tokio::fs::metadata` NotFound → `original = NULL`)
  from *present but non-UTF-8* (`read_to_string` fails while metadata says the
  file exists → **skip** the checkpoint row + `tracing::warn!`, since a NULL
  would wrongly mean "didn't exist" during restore).

`crates/vortex-types`: add
`CheckpointInfo { id: i64, run_id: String, path: String, has_original: bool, created_at: String }`
(never ship `original` content in lists — can be large; restore reads it
server-side).

`crates/vortex-server` — new `routes_ckpts.rs` (or extend `routes_misc.rs`),
registered in `build_router`:
- `GET /api/conversations/{id}/checkpoints?before=&limit=` → `Vec<CheckpointInfo>`.
- `POST /api/checkpoints/{id}/restore` (user-initiated — no approval gate,
  document why):
  1. Load row (`get_checkpoint`), 404 if unknown.
  2. Require a configured workspace (`engine.current_settings().workspace`),
     400 otherwise; resolve the stored absolute path through
     `WorkspaceRoots` — 400 if outside the current workspace (workspace may
     have changed since the snapshot).
  3. Snapshot the **current** file content as a new checkpoint first
     (same conversation, `run_id = "restore"`) so restore is reversible.
  4. `has_original == true` → write the original bytes back.
     `has_original == false` (file didn't exist) → move the current file to
     the trash dir (same mechanics as `delete_path`), never unlink.
  5. Respond `{"ok": true, "path": ..., "action": "restored" | "trashed"}`.
- Note in code: restore intentionally bypasses `file_state` run-ownership
  (it is a human recovery action outside any run).

Tests: store cursor pagination (create >1 page, walk with `before`);
restore round-trip at the store level; `checkpoint` flaw: binary file →
no row + warn (assert row absent); route-level test for outside-workspace
rejection if a route test harness exists, else cover via store + manual smoke.

## Task 7 — UI: C7 client, P12 remnants, P17 panel

`crates/vortex-ui/src/sse.rs`:
- Track last seen SSE id per stream (thread_local `Cell<u64>`, reset on new
  subscription): read `ev.last_event_id()` in `on_message`. Manual
  re-subscribe (CLOSED path) appends `?since=<last_seq>`; browser
  auto-reconnect sends `Last-Event-ID` natively. Add a `replay_gap` listener
  → `resync_from_server` (existing) and continue.
- Keep the existing error/lagged resync paths (idempotent, cheap).

`crates/vortex-ui/src/lib.rs` + `sse.rs` (P12, approvals resurrection):
- Thread-local set of session-resolved approval ids (insert on
  `ApprovalResolved`; cap ~512, drop-oldest). The 5 s poll and boot fetch
  filter the server list through it before `ctx.approvals.set(...)`.

`crates/vortex-ui/src/state.rs` + `chat.rs` (P12, per-delta re-render):
- Add `stream_display: RwSignal<String>` to `AppCtx`. `ChatView` runs a
  125 ms `set_interval` copying `stream_text` → `stream_display` only when
  changed; the streaming bubble's `Show` + `inner_html` read `stream_display`
  (markdown renders ≤8×/s instead of per delta). Reset both signals wherever
  `stream_text` is reset (send, conversation switch, `RunFinished`).

`crates/vortex-ui/src/api.rs` + `tasks.rs` (P17 UI):
- `api::checkpoints(conv_id, before, limit)` and `api::restore_checkpoint(id)`.
- New "File checkpoints" panel in TasksView (current conversation only):
  rows of `path` + `created_at` + badge (`previous version` / `new file`),
  Restore button with `window().confirm_with_message(...)` ("Restore <path>?
  The current content is snapshotted first."), toast on success/failure,
  list refresh after restore, "Load older" button using the cursor while a
  full page was returned.

`crates/vortex-ui/Cargo.toml`: confirm no new deps needed
(`wasm-bindgen-futures` already present from round 1).

Verification: `cargo clippy -p vortex-ui --target wasm32-unknown-unknown`
clean + `scripts/build-ui.sh` succeeds (browser-runtime behavior noted as
not exercised live, same caveat as round 1).

## Task 8 — P14: opt-in live OpenRouter smoke test

`crates/vortex-llm/tests/live_openrouter.rs`:
- `#[tokio::test] #[ignore = "costs API credits; run explicitly: cargo test -p vortex-llm --test live_openrouter -- --ignored (needs OPENROUTER_API_KEY)"]`
- Reads `OPENROUTER_API_KEY` from env; if absent, fail with a clear
  instruction (it's already `#[ignore]`, so no accidental runs). Model from
  `OPENROUTER_TEST_MODEL` (default `openrouter/auto`). Tiny prompt,
  `max_tokens: 16`, assert `Ok`, non-empty `content`, `finish_reason` set;
  log `describe()` + usage only (never the key). **Do not run it during
  implementation** — leave execution to the user; P14 stays OPEN until they do.

## Task 9 — Docs + final verification

Docs (all in one pass after code lands):
- `docs/KNOWN_ISSUES.md`: C7 → FIXED (server log + replay + drain; note the
  8192-event cap and gap-fallback semantics); P1 → FIXED (pinning; note
  per-hop client cost); P9 → FIXED; P12 → FIXED (both remnants); P15 →
  FIXED (bounded by one clamped completion — state residual honestly);
  P17 → FIXED (full recovery feature; document restore trust model); P14 →
  still OPEN, test now available (`#[ignore]`d); add **N8** entry (provider
  task leak on cancel) as FIXED.
- `docs/DEVELOPER_GUIDE.md`: SSE section (seq ids, replay, `replay_gap`,
  drain-not-abort ordering), API table (+2 checkpoint routes), Tasks-panel
  description, §12 test table/counts, live-test instructions.
- `AGENTS.md` + `README.md`: test count, one-line checkpoint-recovery feature
  mention in README.

Final gates (all must pass):
1. `cargo fmt --all -- --check`
2. `cargo test` (expect > 76; all new tests offline)
3. `cargo clippy --all-targets` and `cargo clippy -p vortex-ui --target wasm32-unknown-unknown` — zero warnings
4. `scripts/build-ui.sh`
5. Runtime smoke (mock provider, isolated `VORTEX_*_DIR`s under /tmp):
   start a run, `curl -N` the SSE stream with `?since=0` mid-run → replayed
   ids present, no duplicates; `?since=999999`-style ancient value on a
   fresh subscription → `replay_gap`; checkpoint flow via curl (write file →
   edit → list → restore → verify content, then restore the restore);
   cancel a run and confirm the provider task stops (log/`calls` heuristic).

## Risks / failure modes

- Broadcast payload change `(u64, RunEvent)` touches `Hub::subscribe`
  consumers — only `routes_runs.rs` (verify with grep before finalizing).
- Replay log memory: bounded at 8192 events/run; large `ToolCallStarted`
  args (schema caps string args at 200 KB) could make a run's log ~MiBs —
  acceptable, note it.
- `.resolve()` pinning: TLS hostname verification is unaffected (documented
  reqwest behavior); if a future reqwest changes semantics, the safe_http
  tests catch it.
- Throttled rendering adds ≤125 ms latency to the streaming bubble — intended.
- `/proc/net/tcp` parsing is Linux-only — matches the project's Linux-only
  invariant.
- `fwd.await` drain: if any `ToolContext` clone ever outlives `run_loop`
  (future change), the drain could hang → the 2 s timeout fallback in Task 1
  covers it.

## Explicitly out of scope

P2 (resolve→op TOCTOU; needs `openat2`/`O_NOFOLLOW` rework), C19 residual
(`use_context` expects — idiomatic Leptos), C22 residuals (push/PR/CI
verification — user actions), P11 boot/poll silence (by design), LAN TLS.

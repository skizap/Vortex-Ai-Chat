# Known issues

Findings from a full source analysis (Nov 2026, commit `3b58a76` on
`feat/rust-linux-ai-assistant`), plus a second review-and-fix pass (Nov 2026,
on top of `d6ff158`) and a third fix round (Nov 2026, closing C7, P1, P9,
P12, P15, P17 and new findings N8–N10). Two sections:

- **Confirmed issues** — provable from the code as written (exact trace or
  failing behavior identified by reading; runtime verification noted where
  not performed).
- **Possible concerns** — plausible but runtime-, provider-, or
  model-dependent; need live verification before treatment.

A third section records **new findings (N1–N10)** from the fix passes.

All paths are relative to the repo root. Each issue carries a **Status**
block: `FIXED`, `PARTIALLY FIXED`, or `OPEN`. Original descriptions and
evidence are preserved as the historical record even for fixed issues.

**Third-round verification summary.** `cargo test` (105 tests, up from 76;
all offline), `cargo fmt --all -- --check`, `cargo clippy --all-targets`,
and `cargo clippy -p vortex-ui --target wasm32-unknown-unknown` all clean;
`scripts/build-ui.sh` (Trunk release build) succeeds. Runtime smoke tests
against `VORTEX_LLM=mock` verified: SSE seq ids + `?since=` replay with no
duplicates + future-cursor clamping (C7), prompt cancel with the provider
stream stopped and `RunFinished` strictly last (N8), and the full checkpoint
flow — list → restore → verify → restore-the-restore, trash-on-"new file"
restore, outside-workspace rejection (P17). A live OpenRouter test now
exists but stays `#[ignore]`d until the user runs it (P14).

**Remaining open items** (quick index): C19 (residual `use_context`
expects), C22 (CI not yet exercised on Actions; `main` stale), P2
(resolve→op TOCTOU), P11 (boot/poll error surfacing), P14 (live provider
run pending).

---

## Confirmed issues

### C1. Chromium and dev-server previews are killed immediately after launch (critical) — **FIXED**

**Status.** Fixed: `BrowserProcess` and `PreviewHandle` now own the
`tokio::process::Child` (kept alive for the manager/registry lifetime,
killed on drop as intended), `attach_child` takes the handle, and the
`pid.unwrap()` panic paths were replaced with proper errors. Regression
test: `attached_child_survives_registration_and_dies_on_stop`
(`crates/vortex-agents/src/preview.rs`). Browser-side runtime behavior
still requires Chromium to verify live.

**Description.** Both process-spawning paths set `.kill_on_drop(true)` and then
drop the `tokio::process::Child` handle when the launching function returns.
With `kill_on_drop(true)`, dropping the handle sends SIGKILL to the child, so
the spawned process dies right after launch.

**Evidence.**
- Browser: `crates/vortex-agents/src/browser.rs:103` (`kill_on_drop(true)`);
  `child` is a local in `ensure_started` and only `pid` is stored in
  `BrowserProcess` (`browser.rs:19-22`, `109-127`). The `Child` drops at the
  end of `ensure_started`.
- Dev-server preview: `crates/vortex-agents/src/tools_preview.rs:123`
  (`kill_on_drop(true)`); `child` is a local in `run_process_preview` and only
  `child_pid: Option<u32>` is kept in `PreviewHandle`
  (`crates/vortex-agents/src/preview.rs:22-30`), so the `Child` drops when the
  tool returns (`tools_preview.rs:126-157`).
- Contrast: `run_command_scoped` consumes its `Child` to completion and is
  correct (`crates/vortex-agents/src/tools_command.rs:195-212`).

**Impact.** Browser automation and process-based previews cannot work at
runtime: the CDP websocket connects, then the browser is killed; the dev
server is killed after its port is detected. Not covered by tests (no test
spawns a long-lived child through these paths; browser tests are gated on
Chromium being installed).

**Recommended fix.** Store the `Child` in `BrowserProcess`/`PreviewHandle`
(or drop `kill_on_drop` and rely solely on the existing process-group kill
paths, which already exist for shutdown/cancel). — Done.

### C2. Workspace confinement: dangling-symlink write escape (security) — **FIXED**

**Status.** Fixed: `WorkspaceRoots::resolve` now rejects any component that
fails canonicalization but is itself a symlink (dangling or unresolvable),
while plain missing components remain allowed so write tools can create new
files (`crates/vortex-agents/src/workspace.rs`). Regression tests:
`rejects_dangling_symlink_escape`, `allows_missing_path_for_creation`.

**Description.** `WorkspaceRoots::resolve` canonicalizes each path component
only *if canonicalization succeeds*; a symlink whose target does not exist
yet fails `canonicalize`, the check is skipped, and the unresolved path
(inside the workspace) is returned. Subsequent file operations follow the
symlink and create/overwrite its target **outside** the workspace.

**Evidence.** `crates/vortex-agents/src/workspace.rs:79-87`
(`if let Ok(canonical) = std::fs::canonicalize(&current)` — the `Err` branch
silently keeps `current`). Write path: `tools_write.rs:88`
(`tokio::fs::write` on the resolved path follows symlinks); delete path:
`tools_write.rs:209` (rename through the link). Existing tests only cover
symlinks with existing targets (`workspace.rs:137-160`).

**Impact.** A dangling symlink inside the workspace (creatable by
`run_command`, an external process, or a checked-out repo) turns
`write_file`/`edit_file`/`delete_path` into arbitrary-location writes with the
user's privileges. Bypasses the confinement invariant documented in
`docs/SECURITY.md`.

**Recommended fix.** When `canonicalize` fails, use
`std::fs::symlink_metadata` / `read_link` on the component: refuse symlinks
whose resolved target leaves the root, or refuse dangling symlinks outright.
— Done (refuse outright).

### C3. OpenRouter retry re-delivers already-streamed text (duplicate output) — **FIXED**

**Status.** Fixed: `try_complete_once` tracks whether any delta was forwarded
(`streamed` flag); the retry loop in `complete` only retries while nothing has
been delivered to the consumer, and logs when a retryable mid-stream failure
is deliberately not retried (`crates/vortex-llm/src/openrouter.rs`). A
mid-stream failure after partial delivery now surfaces as an error instead of
duplicating text and double-billing. Regression test:
`no_retry_after_partial_stream_delivery` (server aborts the connection after
delivering a delta); `server_error_is_retried` and
`retries_rate_limit_then_succeeds` confirm pre-stream retries still happen.

**Description.** On a retryable mid-stream failure (network error/timeout
after deltas were streamed), `complete` retried the whole POST. Deltas from
attempt 1 were already sent to the caller's channel and appended to the UI;
attempt 2 streamed its full text again into the same channel. There was no
reset marker, so the UI showed duplicated/concatenated text, and the provider
call was billed twice.

**Evidence.** Streaming + accumulation:
`crates/vortex-llm/src/openrouter.rs` (`try_complete_once`); retry loop reused
the same request body and sender (`complete`); retryable classification
included `Network`/`Timeout` (`crates/vortex-llm/src/error.rs`). Tests only
covered failure *before* the stream started.

**Impact.** Corrupted assistant output and double billing on flaky
connections. Engine-side persisted final content came from the last attempt
only, so DB and UI could disagree.

### C4. Stale pending approvals survive server restarts (and can be "decided") — **FIXED**

**Status.** Fixed: `mark_interrupted_on_startup` now also sets
`status='expired'` (with `decided_at`) for pending approvals belonging to
interrupted runs, and returns `(runs, approvals)` counts which the server logs
(`crates/vortex-core/src/db.rs`, `crates/vortex-server/src/main.rs`).
Regression test: `startup_marks_runs_interrupted_and_expires_their_approvals`
(includes an approved-row-of-a-completed-run control that must not be
touched). Verified at runtime: after seeding a `running` run + `pending`
approval and restarting, `/api/approvals` returns `[]` and the DB row reads
`expired`.

**Description.** Startup marked live runs `interrupted` but did not expire
their pending `approvals` rows. After a restart, `GET /api/approvals`
(pending) surfaced approvals whose run was dead;
`POST /api/approvals/{id}/decide` resolved them with no linkage check —
though nothing executed because the in-memory waiter no longer existed.

**Evidence.** `crates/vortex-core/src/db.rs` (`mark_interrupted_on_startup`,
no approvals cleanup); `crates/vortex-core/src/store_appr.rs` (pending list);
`crates/vortex-server/src/routes_misc.rs` (decide by id only).

**Impact.** Confusing UI state (undismissable "ghost" approvals); DB status
inconsistency. No execution risk: the oneshot receiver was gone.

### C5. Approval timeout race can record the wrong DB status — **FIXED**

**Status.** Fixed: resolution is now ownership-based under the pending-map
lock. `decide` removes the sender and only writes `approved`/`denied` if the
decision was actually delivered; if the waiter had already timed out
(`send` fails), it writes `expired` instead of a phantom decision. The
timeout branch only writes `expired` if it still found (owned) the sender.
Exactly one writer per approval in every interleaving
(`crates/vortex-agents/src/approvals.rs`).

**Description.** On approval timeout, the pending sender was removed after
the timeout fired; if a user decision landed in the window between timeout
and removal, the DB row could be written as `expired` even though the user
decided (or as `approved` while the run auto-denied).

**Evidence.** `crates/vortex-agents/src/approvals.rs` (old timeout branch
removed the sender and wrote `Expired` without checking whether `decide`
had already resolved it).

**Impact.** State inconsistency between the run outcome and the approvals
table.

### C6. Byte-slicing panics on multi-byte model-provided strings — **FIXED**

**Status.** Fixed: both truncations now count characters.
`tools_delegate.rs` uses a `default_title` helper
(`task.chars().take(20).collect()`); `tools_search.rs` truncates `page.url`
with `chars().take(60)`. Regression test:
`default_title_never_splits_a_utf8_character` (CJK + emoji inputs). See also
N6 for a third instance of this bug class found in `html.rs`.

**Description.** Two truncations sliced raw bytes and panicked when the cut
landed inside a multi-byte UTF-8 character: `&task[..task.len().min(20)]` and
`&page.url[..page.url.len().min(60)]`.

**Evidence.** `crates/vortex-agents/src/tools_delegate.rs`;
`crates/vortex-agents/src/tools_search.rs`.

**Impact.** Panics occurred inside spawned tool tasks and were converted to
tool failures (`crates/vortex-agents/src/engine.rs`), so no crash — but
`delegate_subtask` failed for any task description whose 20th byte split a
character (e.g., CJK/emoji), and `fetch_url` could fail on such URLs.

### C7. SSE/broadcast event loss is silent (server-side) — **FIXED**

**Status.** Fixed (round 3): the server keeps a bounded, seq-stamped event
log per run and the SSE route replays it on (re-)subscription.
- `Hub` events are now `(seq, RunEvent)`; seq allocation, log insert and
  broadcast send happen under one lock per run handle, so the log and the
  live stream can never disagree on order (duplicates possible, gaps not).
  Ancestor channels get their own coherent seq namespace
  (`crates/vortex-agents/src/hub.rs`).
- Replay window: `MAX_RUN_LOG = 8192` entries per run, oldest evicted on
  overflow. The log lives and dies with the run handle — runs never survive
  restarts (they are marked interrupted), so persistence would buy nothing.
  Memory note: schema caps string args at 200 KB, so a pathological run's
  log can reach a few MiB; bounded and accepted.
- `GET /api/runs/{id}/events` accepts `Last-Event-ID` (browser
  auto-reconnect) and `?since=<seq>` (UI manual re-subscribe); the newer
  cursor wins. Every event carries `id: <seq>`; the replay/live overlap is
  deduplicated per connection. A cursor below the retained window emits a
  named `replay_gap` event (data = oldest retained seq) and the client
  falls back to a snapshot resync; a *future* cursor is clamped to the
  newest logged seq so the live tail keeps flowing
  (`crates/vortex-server/src/routes_runs.rs`).
- Drain, not abort: the tool-event forwarder in `execute_run` and the
  delta forwarder in `run_loop` are now awaited (bounded by a 2 s safety
  timeout in `execute_run`) instead of `fwd.abort()`, so queued events are
  delivered and `RunFinished` is published strictly last
  (`crates/vortex-agents/src/engine.rs`). Verified live: `ToolCallFinished`
  /`PlanUpdated` before `RunFinished` (test
  `tool_events_are_delivered_before_run_finished`), and in the mock-provider
  runtime smoke (stop mid-run → terminal event last).
- UI: tracks the last seen id per stream, re-subscribes with
  `?since=<last seq>` after a CLOSED source, listens for `replay_gap` (and
  legacy `lagged`) → snapshot resync
  (`crates/vortex-ui/src/sse.rs`).

Residual (honest): text deltas lost to a `replay_gap` (window overflow or
broadcast lag) are still not reconstructible from the snapshot endpoint —
`GET /api/runs/{id}/state` has no accumulated assistant text. With the 8192
-entry log this needs an extreme event burst; the bubble heals at run end
via the terminal message refetch.

**Description.** Per-run broadcast channels have capacity 512; `send` errors
and receiver lag are ignored, and the tool-event forwarder is aborted at run
end, dropping queued events. The SSE route translates lag into a `lagged`
hint event but nothing replayed the lost events.

**Evidence.** `crates/vortex-agents/src/hub.rs`;
`crates/vortex-agents/src/engine.rs` (`fwd.abort()`);
`crates/vortex-server/src/routes_runs.rs` (lag → `lagged` event, no replay).

**Impact.** Under load (fast streaming + slow client), the UI could miss
deltas/tool events mid-run; recovery only at terminal `RunFinished`. Now
mitigated by the `lagged`/error resync path.

### C8. Mock demo provider only performs the tool round-trip on the first run — **FIXED**

**Status.** Fixed: `DemoMockLlm` no longer keeps a process-global turn
counter; the "first turn" condition is derived from the request itself
(presence of a tool-role message), so every run gets the scripted
`web_search` round-trip. Mock tool-call ids are now unique per call
(`crates/vortex-server/src/mock_llm.rs`). Verified at runtime: two
consecutive chats in mock `tools` mode both produced a `web_search` tool
call and a citation-style answer.

**Description.** `DemoMockLlm` incremented a shared `turn_counter` per
completion and never reset it, despite the comment "every run starts fresh".
With `VORTEX_MOCK_SCRIPT=tools`, only the very first run of the server
process got the scripted `web_search` call; later runs skipped straight to
the text branch.

**Evidence.** `crates/vortex-server/src/mock_llm.rs` (comment, counter,
`script == "tools" && turn == 0`).

**Impact.** Offline demos/tests of the tool pipeline silently degraded after
the first run.

### C9. `VORTEX_SEARXNG_URL` is documented but never read — **FIXED**

**Status.** Fixed by removal: the line is gone from `.env.example`, replaced
with a note that the search provider URL is runtime state configured in
Settings → Search (SQLite), matching the documented split between boot config
and runtime settings. `docs/DEVELOPER_GUIDE.md` updated to match. Implementing
the override was rejected: it would bypass the settings-driven design and the
UI's honest capability reporting.

**Description.** `.env.example` advertised `VORTEX_SEARXNG_URL` as a testing
override, but no code read it; the search provider URL comes only from
runtime Settings (SQLite).

**Evidence.** `.env.example`; grep across `crates/**/*.rs` found zero
references; config env overrides are limited to `VORTEX_HOST`,
`VORTEX_PORT`, `VORTEX_OPENROUTER_BASE_URL`, `VORTEX_LLM`,
`VORTEX_MOCK_SCRIPT`, `RUST_LOG` (`crates/vortex-core/src/config.rs`).

**Impact.** Users following `.env.example` got a silently ignored setting.

### C10. XDG base directories are not actually honored — **FIXED**

**Status.** Fixed: `paths.rs` now consults `XDG_CONFIG_HOME` /
`XDG_DATA_HOME` / `XDG_CACHE_HOME` as a middle fallback between the
`VORTEX_*_DIR` overrides and the `$HOME` defaults. Per the XDG spec,
*relative* `XDG_*_HOME` values are ignored (fall through to the default).
Resolution is a pure function (`resolve_base`) with race-free unit tests
(see also C23). README and DEVELOPER_GUIDE storage tables updated
(`crates/vortex-core/src/paths.rs`).

**Description.** Docs (module header, README) described "XDG" storage, but
`paths.rs` never read `XDG_CONFIG_HOME`/`XDG_DATA_HOME`/`XDG_CACHE_HOME`; it
hardcoded `$HOME/.config`, `$HOME/.local/share`, `$HOME/.cache` unless the
`VORTEX_*_DIR` overrides were set.

**Evidence.** `crates/vortex-core/src/paths.rs` (doc claim vs. only
`VORTEX_*` and `HOME` consulted).

**Impact.** Users with relocated XDG dirs got data in the default locations.

### C11. `delete_conversation` is non-atomic and orphans related rows — **FIXED**

**Status.** Fixed: deletion runs in a single transaction
(`unchecked_transaction`) and now removes the whole run tree — the
conversation's runs *and* all descendant sub-agent runs (recursive CTE on
`parent_run_id`) — plus their `tool_calls`, `approvals`, `checkpoints` rows,
then messages, runs, and the conversation. Regression test:
`delete_conversation_cascades_all_related_rows` (asserts zero rows remain in
all six tables, including a sub-agent run's tool call). Verified at runtime:
after chat + retry + delete, no orphaned `tool_calls`/`approvals` rows
remained (`crates/vortex-core/src/store.rs`).

**Description.** Three sequential DELETEs (messages, runs, conversation) ran
without a transaction; a failure mid-way left partial deletion. It also never
deleted `tool_calls`, `approvals`, or `checkpoints` rows (no FK/cascade in
the schema), which accumulated forever.

**Evidence.** `crates/vortex-core/src/store.rs`; schema without foreign
keys: `crates/vortex-core/src/db.rs`.

**Impact.** DB growth over time; rare inconsistent state under BUSY.

### C12. Corrupt settings JSON silently resets all user settings — **FIXED**

**Status.** Fixed: a parse failure now logs `tracing::error!` with the parse
error, preserves the corrupt blob under settings key
`settings_corrupt_backup` (for manual recovery/inspection), and only then
falls back to defaults. Regression test:
`corrupt_settings_fall_back_to_defaults_and_keep_backup`
(`crates/vortex-core/src/store_msgs.rs`).

**Description.** `load_settings` fell back to `Settings::default()` on any
deserialization error, discarding every user preference without a warning.

**Evidence.** `crates/vortex-core/src/store_msgs.rs` (fallback).

**Impact.** Silent data loss (model, workspace, allowlists) after a schema
change or corruption.

### C13. Deterministic 4xx provider errors are retried — **FIXED**

**Status.** Fixed: `LlmError::is_retryable` now retries `Provider(code, _)`
only for `code >= 500` (429 maps to `RateLimited`, which stays retryable;
400/405/etc. are terminal). Regression tests: `retryable_classification`
(unit), `bad_request_is_not_retried` (400 → single call), 
`server_error_is_retried` (503 → retry then success)
(`crates/vortex-llm/src/error.rs`, `crates/vortex-llm/tests/openrouter.rs`).

**Description.** `map_status` funneled unmapped codes (including 400) into
`Provider`, and `is_retryable` retried all `Provider` errors — so a
deterministic bad request was retried twice with 1s/3s backoff before
failing.

**Evidence.** `crates/vortex-llm/src/error.rs`;
`crates/vortex-llm/src/openrouter.rs`.

**Impact.** Wasted latency and API calls on permanent errors.

### C14. TextDelta delivery is best-effort and can silently diverge from final content — **FIXED**

**Status.** Fixed: all three providers (`OpenRouterClient`, `MockLlm`,
`DemoMockLlm`) now `send().await` instead of fire-and-forget `try_send`, so
deltas are never silently dropped while the consumer is alive; the bounded
channel provides real backpressure. If the consumer is gone (send error),
delivery attempts stop but the final `CompletionResponse.content` remains
complete (`sink_alive` flag in `openrouter.rs`)
(`crates/vortex-llm/src/openrouter.rs`, `crates/vortex-llm/src/mock.rs`,
`crates/vortex-server/src/mock_llm.rs`).

**Description.** Providers used `try_send` on the delta channel and
discarded the result; when the consumer was slow/full, deltas were dropped
while the final content remained complete.

**Evidence.** `crates/vortex-llm/src/openrouter.rs`;
`crates/vortex-llm/src/mock.rs`.

**Impact.** Mid-run UI text could be missing chunks; only the terminal
message refetch healed it (see C7). Deadlock risk of awaited sends is nil:
the engine consumes the channel in a spawned forwarder whose only downstream
(broadcast publish) never blocks, and a dropped receiver makes `send` fail
immediately.

### C15. UI: no EventSource reconnect/recovery, and lost events are never reconciled mid-run — **FIXED**

**Status.** Fixed in `crates/vortex-ui/src/sse.rs`: (1) the active
subscription lives in a thread-local with `set_new_subscription` /
`close_subscription` helpers; (2) on `error`, if `readyState == CLOSED` and
the run is non-terminal, the UI resyncs from `/api/runs/{id}/state` and
re-subscribes after a 1 s delay (self-limiting: the resync sets a terminal
status when the run is actually finished, so no reconnect storm); if the
source is still `CONNECTING` (browser auto-reconnect), it only resyncs what
was missed; (3) the server's `lagged` event triggers the same resync; (4) an
orphan `ToolCallFinished` (missed `Started`) is inserted as a recovered row
instead of dropped; (5) resync maps in-flight DB rows (`ok=false`,
`summary="running"`) back to a running state instead of painting them
failed. Compiled clean for wasm and the Trunk release build succeeds;
browser-runtime behavior was not exercised live (no browser session in this
environment).

**Description.** The UI implemented no re-subscription logic: on `error` it
did a one-shot `run_state` fetch and showed a toast if that failed. Deltas
were blind appends with no ids/sequence numbers, so anything missed during a
gap was lost until the terminal `RunFinished` refetch.

**Evidence.** `crates/vortex-ui/src/sse.rs` (only error handler, blind
append, `ToolCallFinished` for an unseen call dropped, terminal refetch).

**Impact.** After transient network drops the run could appear frozen or
truncated; a stuck "running…" tool row never resolved.

### C16. UI: `run_id` is not cleared when switching conversations — **FIXED**

**Status.** Fixed: opening a conversation (`ConvButton` in
`crates/vortex-ui/src/sidebar.rs`) and deleting the currently-open
conversation now clear `ctx.run_id` and close the active SSE subscription
(`crate::sse::close_subscription()`), so Stop/Retry can no longer act on
the previous conversation's run.

**Description.** Loading a different conversation reset run visuals but not
the `run_id` signal, so Stop/Retry buttons could act on the previous
conversation's run (Retry would stream an old run's events into the newly
opened conversation's panels).

**Evidence.** `crates/vortex-ui/src/sidebar.rs` (reset without `run_id`);
`crates/vortex-ui/src/chat.rs` (Stop/Retry read `run_id`).

**Impact.** Wrong-target actions across conversations.

### C17. UI: optimistic-bubble sentinel `id: -1` collides across rapid sends — **FIXED**

**Status.** Fixed: optimistic bubbles take unique ids from a decrementing
thread-local counter (`-1, -2, -3, …`), and error rollback retains/removes
only the failed send's bubble. WASM is single-threaded, so a thread-local
`Cell<i64>` suffices (`crates/vortex-ui/src/chat.rs`).

**Description.** Optimistic user bubbles all used `id: -1` while `<For>`
keyed on `id`; two rapid sends created duplicate keys, and error rollback
removed *all* bubbles with `id == -1`, not just the failed one.

**Evidence.** `crates/vortex-ui/src/chat.rs` (sentinel, key, `retain`).

**Impact.** Rendering glitches/lost bubbles when sends overlapped.

### C18. `ammonia` is a declared but unused dependency; sanitization is hand-rolled — **FIXED**

**Status.** Fixed: `ammonia = "4"` removed from `crates/vortex-ui/Cargo.toml`.
The hand-rolled pulldown-cmark walker (which never emits raw HTML and
allowlists link schemes) remains the sanitizer, as before — it is sound, and
removing the dead dependency shrinks the wasm binary and stops the
footprint from misleading readers.

**Description.** `vortex-ui/Cargo.toml` listed `ammonia = "4"`, but no
source file used it. Sanitization is a manual pulldown-cmark walker that
never emits raw HTML (which is itself sound), with a strict link scheme
allowlist.

**Evidence.** `crates/vortex-ui/Cargo.toml`; no `use ammonia` in
`crates/vortex-ui/src/` (grep); walker: `crates/vortex-ui/src/markdown.rs`,
link policy therein.

**Impact.** Wasm binary bloat and a misleading dependency footprint.

### C19. `expect()` panics in WASM code paths — **PARTIALLY FIXED**

**Status.** Partially fixed: the environment-failure traps are gone —
`EventSource::new` and all listener registrations now return `Err` from
`subscribe_run`, which `start_stream` turns into a visible error banner
instead of a WASM trap; `apply_theme` no longer expects a document
(`crates/vortex-ui/src/sse.rs`, `chat.rs`, `state.rs`). The remaining
`use_context::<AppCtx>().expect("ctx provided")` calls are idiomatic Leptos:
they can only fire on a programming error (component mounted outside the
provider), never on environmental/runtime conditions, and converting them
would add noise to every component. Left OPEN by design; `markdown.rs`'s
`expect("themes exist")` on the syntect default theme set is likewise
infallible in practice.

**Description.** Several `expect` calls could trap the WASM module instead
of showing a user-visible error, e.g. `EventSource::new(...).expect(...)`,
listener registration, `use_context` lookups, and theme application.

**Evidence.** `crates/vortex-ui/src/sse.rs`; `state.rs`; `markdown.rs`;
`lib.rs`; `chat.rs`; `sidebar.rs`; `tasks.rs`; `settings_ui.rs`.

**Impact.** A blank/frozen UI on environmental failures (e.g., browser
connection limits) with no diagnostics beyond the console.

### C20. LAN token rewrite of `config.toml` drops user comments — **FIXED**

**Status.** Fixed: first LAN-mode boot now patches the existing file text
via `vortex_core::config::patch_lan_token` instead of regenerating it with
`toml::to_string_pretty`. The patcher preserves comments and unknown keys,
handles the `[server.lan]` section form (replacing or inserting the `token`
key) and the dotted-key form (`server.lan.token = …`), never touches
commented-out token lines, and appends a minimal section when none exists.
Unit tests: `patch_replaces_existing_token_and_keeps_comments`,
`patch_inserts_token_into_section_without_one`,
`patch_handles_dotted_key_form`, `patch_appends_section_when_missing`,
`patch_on_empty_file_creates_valid_section`,
`patch_never_touches_commented_token_lines`
(`crates/vortex-core/src/config.rs`, `crates/vortex-server/src/main.rs`).
The now-unused `toml` dependency was removed from `vortex-server`.

**Description.** First LAN-mode boot regenerated the whole config file via
`toml::to_string_pretty`, discarding comments and unknown keys.

**Evidence.** `crates/vortex-server/src/main.rs` (`handle_lan_token`).

**Impact.** One-time surprise loss of config annotations when enabling LAN
mode.

### C21. `retry_run` lacks the API-key pre-flight that `post_chat` has — **FIXED**

**Status.** Fixed: `POST /api/runs/{id}/retry` performs the same honest
pre-flight as `post_chat` and returns 503 with setup guidance when no key is
configured and the mock provider is not active. Verified at runtime in mock
mode: retry of a completed run starts and finishes normally
(`crates/vortex-server/src/routes_runs.rs`). See also N2 — the retry
history itself had two correctness bugs, fixed together.

**Description.** `POST /api/conversations/{id}/chat` returned 503 with setup
guidance when no key was configured, but `POST /api/runs/{id}/retry` started
a run that would fail inside the provider call.

**Evidence.** Pre-flight: `crates/vortex-server/src/routes_conv.rs`;
absent in `crates/vortex-server/src/routes_runs.rs`.

**Impact.** Confusing failed run instead of an actionable error.

### C22. Repo hygiene — **PARTIALLY FIXED**

**Status.** Partially fixed:
- Dangling `.gitkeep` exemption removed from `.gitignore`: Trunk empties
  `ui-dist/` on every build (verified — a created `.gitkeep` was deleted by
  `scripts/build-ui.sh`), so no sentinel file can be tracked inside it. The
  directory is now ignored wholesale with a comment explaining why;
  `build.rs` still creates the folder + placeholder `index.html` on fresh
  clones, so behavior is unchanged. AGENTS.md gotcha updated. — Done.
- `LICENSE` (MIT) added, matching `Cargo.toml`/README claims. — Done.
- CI added at `.github/workflows/ci.yml` (fmt, clippy native + wasm, tests,
  Trunk build, release server build; honors the `rust-toolchain.toml` pin).
  Not yet exercised on GitHub Actions (nothing was pushed in this pass) —
  the workflow is unverified until the first push. — Done locally, OPEN
  verification.
- `main` still stale / no PR — out of scope for local work. — OPEN.

**Description (original).**
- `.gitignore:6` exempts `crates/vortex-server/ui-dist/.gitkeep`, which did
  not exist; `ui-dist/` was entirely untracked.
- No `LICENSE` file although `Cargo.toml` and README claim MIT.
- No CI (`.github/` absent); all checks were manual.
- `main` is stale (Initial commit only); all work lives on
  `feat/rust-linux-ai-assistant`.

### C23. Test-suite hygiene (minor) — **FIXED**

**Status.** Fixed, all three items:
- `paths.rs` tests no longer mutate the process environment: resolution is
  a pure function (`resolve_base`) tested with explicit inputs; the racy
  set/remove dance and the never-read `VORTEX_TEST_HOME` are gone.
- Store tests (`store.rs`, `store_msgs.rs`, `store_calls.rs`,
  `store_ckpts.rs`) use `tempfile::tempdir()`; no real DB files are left in
  `/tmp`.
- `vortex_llm::MockLlm` can now script failures: `MockTurn::failure(msg)`
  (and a `fail_with: Option<String>` field) makes `complete` return
  `LlmError::Network(msg)`, matching the module doc.

**Description (original).**
- `paths.rs` test mutated process env, racy under parallel tests;
  `VORTEX_TEST_HOME` was set but never read.
- Store tests left real DB files in `/tmp`.
- `vortex-llm`'s `MockLlm` could not script failures despite the module doc
  claiming it.

### C24. Minor protocol/robustness deviations (vortex-llm and friends) — **FIXED (one residual)**

**Status.** Item-by-item:
- **SSE multi-line `data:` joining — FIXED.** The parser now follows the SSE
  spec: `data:` lines accumulate and dispatch on the blank line, joined with
  `\n` (only one leading space stripped per line); `finish()` flushes an
  unterminated trailing event at stream end. Tests:
  `joins_multiline_data_fields_per_spec`, `only_one_leading_space_is_stripped`,
  `finish_flushes_unterminated_event`, `comment_and_event_fields_are_ignored`,
  and end-to-end `multiline_data_events_are_joined`.
- **>1 MiB buffer silently cleared — FIXED.** Overflow now sets a flag the
  client checks after every push and fails the stream with
  `MalformedStream` instead of parsing a corrupted next payload; assembled
  events are additionally capped at 8 MiB. Test:
  `oversized_line_is_reported_not_silently_dropped`.
- **Timeouts hardcoded `Timeout(120)` — PARTIALLY FIXED.** The completion
  request phase now distinguishes connect timeouts (`Timeout(10)`, matching
  `connect_timeout`) from read timeouts (`Timeout(120)`), and `models()`
  reports `Timeout(20)`/`Network` instead of logging `is_timeout()` as the
  error text. Residual: reqwest does not always let the phase be inferred
  exactly, so the value is still an approximation.
- **Missing `tool_call_id` serialized as `""` — FIXED upstream.** The engine
  now normalizes empty provider ids to UUIDs before anything consumes them
  (see P5/N1), so the follow-up request never carries an empty id. The
  serializer's `unwrap_or_default` remains only as a defensive fallback.
- **`.env` parser — FIXED.** `export KEY=…` prefixes are stripped and
  inline comments (` # …`) are removed from unquoted values; quoted values
  are untouched.
- **LIKE wildcards not escaped — FIXED.** `%`, `_`, and `\` in search input
  are escaped with `ESCAPE '\'`. Tests: `escape_like_handles_all_wildcards`,
  `search_escapes_like_wildcards`.
- **`post_chat` O(n) conversation scan — FIXED.** New
  `Db::get_conversation(id)` is used by `post_chat` and `retry_run`. Test:
  `get_conversation_by_id`.

**Description (original).** See the item list above; all were confirmed in
the first analysis with file:line evidence in `crates/vortex-llm/src/sse.rs`,
`openrouter.rs`, `crates/vortex-core/src/config.rs`,
`crates/vortex-core/src/store.rs`, `crates/vortex-server/src/routes_conv.rs`.

---

## New findings (second review pass, Nov 2026)

Issues identified during the fix passes that were **not** in the original
analysis (N1–N7 from the second pass; N8–N10 from the third). All are
fixed; recorded here for traceability.

### N1. Tool-call Started/Finished event ids diverged for provider calls with empty ids — **FIXED**

**Description.** When a provider omitted tool-call ids, `execute_tool_calls`
generated a UUID for the `ToolCallStarted` event and the DB row, but the
`ToolCallFinished` event (emitted from `run_loop`) used the *empty* id, and
the finished-state DB update was skipped entirely (`if !tc.id.is_empty()`).
Result: UI tool rows stuck on "running…" forever and `tool_calls` rows frozen
at `ok=false, summary='running'` in the DB.

**Evidence (pre-fix).** `crates/vortex-agents/src/engine.rs` (`run_loop`
built `call_id = String::new()` for empty ids; `execute_tool_calls` used a
locally generated UUID for Started/insert and gated the update on
`!tc.id.is_empty()`).

**Fix.** `run_loop` now normalizes empty ids to UUIDs immediately after the
completion returns, before the assistant message, events, DB rows, and
provider-bound tool results are built — one stable id per call everywhere
(this is also the P5 fix). `execute_tool_calls` computes its id list once so
Started/insert/update always agree.

### N2. `retry_run` sent a corrupted history: duplicated last user message and orphan tool-role messages — **FIXED**

**Description.** Two bugs in the retry history rebuild
(`crates/vortex-server/src/routes_runs.rs`): (1) history was taken through
`last_user_idx + 1`, i.e. *including* the last user message, which
`start_chat_run` then appends again — the model saw the retried message
twice; (2) unlike `post_chat`, retry did not filter roles, so stored
`tool`-role messages were re-sent with `tool_call_id: None` and
`tool_calls: []`, which strict OpenAI-compatible APIs reject (an assistant
turn's tool_calls must be followed by matching tool results).

**Fix.** History is now `messages[..last_user_idx]` filtered to
`User | Assistant`, mirroring `post_chat`. Verified at runtime (mock mode):
retry completes and re-runs the tool round-trip.

### N3. Sub-agent text deltas leaked into the coordinator's streaming bubble — **FIXED**

**Description.** The hub fans every run's events out to all ancestors, so
the root subscription also receives sub-agent `TextDelta` events (each
carrying the child's `run_id`). The UI appended *all* deltas blindly,
interleaving sub-agent streaming text into the coordinator's answer bubble.

**Evidence (pre-fix).** `crates/vortex-ui/src/sse.rs` (`TextDelta` handler
appended unconditionally); `crates/vortex-agents/src/hub.rs` (`publish`
walks ancestors).

**Fix.** The handler now appends only when the delta's `run_id` matches the
subscribed run (`ctx.run_id`).

### N4. Opening a conversation loaded the *oldest* run's snapshot — **FIXED**

**Description.** `list_runs_for_conversation` orders by `created_at`
ascending; the sidebar took `runs.first()` as "latest", so opening a
conversation displayed the tool calls/agents/plan of its **first-ever** run.

**Evidence (pre-fix).** `crates/vortex-ui/src/sidebar.rs` (`runs.first()`);
`crates/vortex-core/src/store_runs.rs` (`ORDER BY created_at`).

**Fix.** Use `runs.last()`.

### N5. Mode selection reset to the default on every settings save — **FIXED**

**Description.** The `AppCtx::new` effect re-applied `settings.default_mode`
to the mode signal on *every* settings write — including the response of a
theme toggle's save — silently switching the user's selected mode (e.g.
Coding → Chat) mid-session.

**Evidence (pre-fix).** `crates/vortex-ui/src/state.rs` (unconditional
`mode_sig.set(s.default_mode)` inside the settings effect).

**Fix.** The effect applies the default only on the first settings load
(one-shot `Cell<bool>` guard).

### N6. `strip_html`'s 120 KB cap panicked on multi-byte pages — **FIXED**

**Description.** `final_text.truncate(final_text.len().min(120_000))`
panics when byte 120 000 lands inside a multi-byte UTF-8 character —
trivially reachable via `fetch_url` on any large non-ASCII page (CJK, emoji,
accented text). The panic surfaced as a tool failure ("tool task failed").
Same bug class as C6, in a third location.

**Evidence (pre-fix).** `crates/vortex-agents/src/html.rs`.

**Fix.** Back the cut down to the nearest char boundary before truncating.
Regression test: `size_cap_never_splits_a_utf8_character` (60 000 × 3-byte
chars).

### N7. IPv4-mapped IPv6 addresses bypassed the SSRF private-IP block — **FIXED**

**Description.** `is_private_ip` checked IPv6 loopback/unspecified/ULA/
link-local but not the `::ffff:0:0/96` mapped range: `::ffff:127.0.0.1` or
`::ffff:169.254.169.254` (cloud metadata) passed as "public". A DNS resolver
(or DNS64-style hostile server — see P1) returning mapped addresses could
slip past `ensure_not_private`.

**Evidence (pre-fix).** `crates/vortex-agents/src/html.rs`
(`is_private_ip`, V6 arm).

**Fix.** `v6.to_ipv4_mapped()` is unwrapped and re-checked through the IPv4
policy. Tests added to `private_ip_detection` (`::ffff:127.0.0.1`,
`::ffff:10.0.0.5`, `::ffff:169.254.169.254` private; `::ffff:1.1.1.1`
public).

### N8. Cancelling a run left the provider completion running detached — **FIXED**

**Description.** In `engine.rs::run_loop`, the cancel/deadline `tokio::select!`
arms returned without aborting the spawned provider `call` task — dropping a
`JoinHandle` does not stop the task, so the completion kept running detached:
the provider stream kept being consumed (and billed) after the user pressed
Stop, until it ended on its own. Found while fixing C7's abort-vs-drain
problem.

**Evidence (pre-fix).** `crates/vortex-agents/src/engine.rs` (`let call =
tokio::spawn(...)` moved into `select!`; cancel/deadline arms only called
`fwd.abort()` and returned).

**Fix.** The `JoinHandle` is kept outside the `select!` (`r = &mut call`) and
both abandonment arms call `call.abort()` before returning, dropping the
completion future (and with it the provider stream) promptly. Regression
test: `cancel_aborts_the_provider_task` (a probe provider whose future
reports when it is dropped; cancel must drop it within seconds, not after
the full simulated completion). Runtime smoke: stop mid-stream → run
terminal in ~43 ms, no deltas after `RunFinished`.

### N9. `fetch_url`'s 12 KB text cap panicked on multi-byte pages — **FIXED**

**Description.** `FetchUrlTool::execute` capped the page text with
`text.truncate(12_000)` — a raw byte cut that panics when byte 12 000 lands
inside a multi-byte UTF-8 character. Any large CJK/emoji/accented page
fetched through `fetch_url` failed the tool call. Same bug class as C6/N6,
in a fourth location missed by the second pass (C6 fixed only the URL
truncation in the same file).

**Evidence (pre-fix).** `crates/vortex-agents/src/tools_search.rs`
(`text.truncate(12_000)`).

**Fix.** A `cap_text` helper backs the cut down to the nearest char
boundary before truncating. Regression tests:
`cap_text_never_splits_a_utf8_character`, `cap_text_passes_short_text_through`.

### N10. Child run status events clobbered the subscribed run's UI state — **FIXED**

**Description.** The hub fans every run's events out to all ancestors, so
the root subscription also receives sub-agent `StatusChanged`/`RunFinished`
events (each carrying the child's `run_id`). The UI applied them
unconditionally to the global run state: the first sub-agent finishing set
the coordinator's status to a terminal one — hiding the Stop button mid-run,
clearing the streaming bubble, and refetching messages while the coordinator
was still working. Same boundary as N3 (which fixed only text deltas).
Surfaced by the handover review while extending replay/recovery (C7), where
replayed child events would otherwise re-apply the same clobber.

**Evidence (pre-fix).** `crates/vortex-ui/src/sse.rs` (`StatusChanged` and
`RunFinished` handlers ignored `run_id` for global state).

**Fix.** Both handlers now compare the event's `run_id` with the subscribed
run (`ctx.run_id`): only the subscribed run drives the global
status/controls; child events update only the sub-agent panel (same pattern
as the N3 delta fix).

---

## Possible concerns

### P1. SSRF via DNS rebinding (TOCTOU) — **FIXED**

**Status.** Fixed (round 3) with IP pinning for model-chosen URLs:
`ensure_not_private` became `validated_addrs(parsed, resolver)` — an
injectable async resolver (default `tokio::net::lookup_host`) returning the
full validated address list, still rejecting the URL if **any** resolved
address is private (`crates/vortex-agents/src/html.rs`). `check_url` now
returns the address to pin, and `fetch_text` builds a per-hop reqwest client
with `.resolve(host, pinned_addr)` and requests the **original** URL — DNS
is overridden only; TLS SNI and certificate verification still use the
hostname, so no verification is weakened. Every redirect hop re-resolves,
re-validates and re-pins, so a name that flips to loopback/metadata between
hops aborts the fetch. Cost note: one client build per hop (TLS config
clone, no cross-hop pooling) — acceptable at tool-call frequency; a
domain→addr client cache is a future option if it ever becomes hot. The
`allow_private_hosts` path (user-configured SearXNG base) keeps the shared
unpinned client by design. Tests: simulated rebinding (public first,
loopback on re-validation → hop aborts), reject-if-any, first-address pick,
end-to-end pinning against a `.invalid` hostname (RFC 2606 — cannot
resolve; success proves the pin), and localhost fetch via the allow-private
path (`crates/vortex-agents/src/safe_http.rs`). `docs/SECURITY.md` residual
risk note updated.

`check_url` resolves and validates every address, but reqwest re-resolves DNS
at connect time; no IP pinning between check and connect, at first hop or on
redirects. A rebinding DNS server could pass validation then resolve to
loopback/cloud metadata. Evidence:
`crates/vortex-agents/src/safe_http.rs`. Already acknowledged in
`docs/SECURITY.md` ("Residual risk"). Mitigation: resolve once, connect to
the validated IP with Host-header override, or use a custom resolver.
(N7 closed one adjacent hole — mapped IPv6 — but the rebinding window
itself remains.)

### P2. TOCTOU between workspace `resolve` and the file operation — **OPEN**

An out-of-band local process could swap a path component to a symlink
between validation and `write`/`rename` (`workspace.rs`, `tools_write.rs`).
In-process agents are serialized by the per-path mutex; external processes
are not. Inherent to the design without `O_NOFOLLOW`-style syscalls.

### P3. Provider-controlled tool-call `index` can allocate unboundedly — **FIXED**

**Status.** Fixed: slot vector growth is capped at `MAX_TOOL_CALL_SLOTS =
64`; deltas with an out-of-range `index` are ignored with a warning instead
of growing the vector. Regression test:
`out_of_range_tool_call_index_is_ignored` (index `u32::MAX`). Residual
note from the original finding: deltas *omitting* `index` still default to
slot 0 (matches OpenAI's own behavior for single-tool streams; parallel
calls without indexes are a protocol violation we cannot disambiguate).

**Description (original).** `openrouter.rs` grew the tool-call slot vector
to fit a provider-supplied `index` (u64 → usize) with no cap; a hostile
endpoint could trigger OOM. Also, deltas omitting `index` default to slot 0
and can silently merge two parallel tool calls.

### P4. `fetch_url` downloads the full body before applying the size cap — **FIXED**

**Status.** Fixed: `SafeHttp::fetch_text` streams `resp.bytes_stream()` and
stops reading as soon as `max_bytes` is reached (final chunk is trimmed to
the cap), so a hostile server can no longer make the client buffer gigabytes
within the 20 s timeout (`crates/vortex-agents/src/safe_http.rs`).

### P5. Empty tool-call ids from a provider break the next iteration — **FIXED**

**Status.** Fixed: `run_loop` normalizes empty ids to UUIDs before building
the assistant message, so the ids sent back to the provider (assistant
`tool_calls[].id` and the tool result's `tool_call_id`) are always non-empty
and mutually consistent — strict OpenAI-compatible APIs accept them. This
also fixes N1 (event/DB id divergence).
(`crates/vortex-agents/src/engine.rs`.)

### P6. Mutex-poisoning cascades — **FIXED**

**Status.** Fixed: `.expect(...)` on std-mutex locks replaced with
poisoning recovery (`unwrap_or_else(PoisonError::into_inner)`) in
`hub.rs`, `preview.rs`, `engine.rs`, `file_state.rs`
(`crates/vortex-agents`) and `db.rs` (`crates/vortex-core`). The guarded
data (registries/maps/connection) carries no panic-sensitive invariants, so
recovering keeps one bug from turning every later access into a panic and
killing the server. Rationale documented at the `Db::with_conn` site.

### P7. Chromium launch failure panics inside `page_for` — **FIXED**

**Status.** Fixed (remainder): `run_command_scoped` no longer indexes
`argv[0]` without a check — an empty argv returns
`ToolError::InvalidArgs` (`crates/vortex-agents/src/tools_command.rs`). The
`pid.unwrap()` paths were already fixed with C1; `.expect("stderr piped")`
remains and is safe (`Stdio::piped()` is set unconditionally).

### P8. `run_command` output drain has no deadline after normal exit — **FIXED**

**Status.** Fixed: the post-exit stdout/stderr drain is wrapped in a 5 s
`tokio::time::timeout` (with a warning log on expiry), so a grandchild
holding the pipes open can no longer stall the tool indefinitely after
normal exit (`crates/vortex-agents/src/tools_command.rs`).

### P9. Dev-server port detection can attach to the wrong service — **FIXED**

**Status.** Fixed (round 3) with a listener-ownership check
(`crates/vortex-agents/src/tools_preview.rs`): `listening_ports()` parses
`/proc/net/tcp` + `/proc/net/tcp6` (state `0A` = LISTEN; Linux-only, matching
the project invariant). `run_process_preview` snapshots the port set *before*
spawning; `wait_for_port` (deadline now parameterized) accepts a
regex-matched printed port only once it appears as a **new** listener
(present now, absent from the pre-spawn snapshot) — a dev server printing an
unrelated port can no longer attach the preview to somebody else's service.
If nothing parseable is printed by the deadline but exactly one new
listening port appeared, that port is used (quiet servers). The 8417
exclusion stays. Note: the old implementation also gave up after 2 s of
stdout silence despite its 20 s deadline; the rewrite polls until the
deadline. Tests: printed port accepted only with a real listener; printed
port without a listener rejected; single-new-listener fallback;
`/proc` parser sanity.

`wait_for_port` regex-matched any `host:port` printed on stdout
(`tools_preview.rs`, only 8417 excluded); a dev server printing an unrelated
port registers a wrong preview URL that browser tools would then drive. A
robust fix needs per-framework output contracts or port-ownership checks
(compare the listener set before/after spawn); left as a concern.

### P10. UI renders preview URLs into `href` without a scheme allowlist — **FIXED**

**Status.** Fixed: `PreviewList` only renders an `<a href>` for `http://` /
`https://` URLs; anything else is plain text with a "blocked link scheme"
title, matching the markdown link policy
(`crates/vortex-ui/src/tasks.rs`).

### P11. UI silently swallows many request errors — **PARTIALLY FIXED**

**Status.** Partially fixed: conversation rename, delete, Stop, and the
run-stream subscription now surface failures via toasts/banners
(`sidebar.rs`, `chat.rs`, `sse.rs`). Still silent by design: boot-time
conversations/approvals fetches and the 5 s approvals poll (a toast every
5 s during an outage would be worse than silence; the boot path already
banners on settings/health failures), theme save, previews/models fetches.
Listed as OPEN residual.

### P12. UI misc — **FIXED**

**Status.** Item-by-item:
- "Copied" toast shown even when the clipboard write rejects — **FIXED**:
  the promise is awaited via `wasm-bindgen-futures::JsFuture`; failure shows
  "Clipboard write was blocked by the browser" (`chat.rs`).
- Markdown render artifacts — **FIXED**: ordered lists starting at N≠1 now
  emit `<ol start="N">` (previously they fell through to *no wrapper at
  all*, emitting bare `<li>`s); blockquotes no longer inject an extra `<p>`
  that nested illegally with the quote's own paragraphs (`markdown.rs`).
- `urlencode` encoded space as `+` — **FIXED**: `%20` (`api.rs`).
- 3xx responses treated as success — **FIXED**: success is `200..300`
  (`api.rs`).
- Approvals 5 s poll can briefly resurrect an approval resolved via SSE —
  **FIXED** (round 3): the SSE handler records session-resolved approval ids
  in a bounded thread-local set (cap 512, drop-oldest), and both the boot
  fetch and the 5 s poll filter the server list through it before replacing
  `ctx.approvals` — an in-flight poll response captured before a resolution
  can no longer put the resolved card back (`crates/vortex-ui/src/sse.rs`,
  `lib.rs`).
- Full markdown re-render + syntax highlight on every streamed delta —
  **FIXED** (round 3) by throttling: `AppCtx` gained a `stream_display`
  signal; an app-level 125 ms interval mirrors `stream_text` into it only
  when changed, and the streaming bubble renders `stream_display`. Markdown
  + highlighting now re-run ≤8×/s instead of once per SSE delta, at the
  cost of ≤125 ms display latency (intended). Both signals are reset
  together on send, conversation switch, and `RunFinished`
  (`crates/vortex-ui/src/state.rs`, `lib.rs`, `chat.rs`, `sidebar.rs`,
  `sse.rs`).

### P13. `Trunk.toml` declares `public-dir = "public"` but no such directory exists — **FIXED**

**Status.** Fixed: the `public-dir` line is removed from
`crates/vortex-ui/Trunk.toml` (Trunk's default is the same nonexistent
`public/`, so behavior is unchanged; the trap for future asset work is
gone).

### P14. Live OpenRouter behavior is unverified end-to-end — **OPEN** (test now available)

**Status.** Still OPEN by design, but round 3 added the opt-in live smoke
test `crates/vortex-llm/tests/live_openrouter.rs`. It is `#[ignore]`d (it
costs API credits) and never runs in CI or a plain `cargo test`. To close
this item, run it yourself with a real key and mark this entry FIXED:

```sh
OPENROUTER_API_KEY=sk-or-... \
  cargo test -p vortex-llm --test live_openrouter -- --ignored --nocapture
```

Optional overrides: `OPENROUTER_TEST_MODEL` (default `openrouter/auto`),
`VORTEX_OPENROUTER_BASE_URL`. It sends a tiny prompt (`max_tokens: 16`),
asserts `Ok` + non-empty content + a set `finish_reason`, and prints only
`describe()` + usage — never the key. It was deliberately **not executed**
during implementation.

All other streaming/tool-call tests run against a protocol-faithful local
mock server (`crates/vortex-llm/tests/openrouter.rs`, extended in the second
pass with mid-stream abort, 4xx/5xx, and multi-line-data cases).
Real-provider framing differences would surface only in production use.

### P15. Token budget can be exceeded within one iteration — **FIXED**

**Status.** Fixed (round 3) in `crates/vortex-agents/src/engine.rs::run_loop`:
before *every* provider call the engine computes
`remaining = max_total_tokens − total_usage.total_tokens`; if nothing
remains the run fails honestly ("token budget exhausted before the next
model call; summarize progress") without dialing the provider, and the
request's own `max_tokens` is clamped to
`min(settings.max_tokens, remaining)` so the provider itself bounds the
completion. Residual (honest, unchanged): accounting is still post-hoc per
call, so a single completion can overshoot by at most one *clamped* request
— the previous unbounded overshoot within an iteration is gone. The
post-tool check remains as a backstop. Regression test:
`token_budget_is_enforced_before_the_next_call` (budget exactly consumed by
turn 1 → run fails pre-call, `llm.calls() == 1`).

Usage is only checked after tool execution each iteration (`engine.rs`); a
single huge completion can overshoot `max_total_tokens` before the run
stops. Inherent to post-hoc accounting; a hard fix needs provider-side
`max_tokens` budgeting per iteration.

### P16. SQLite has no `busy_timeout` — **FIXED**

**Status.** Fixed: `Db::open` sets `busy_timeout(5s)`, so a second process
touching `data/vortex.db` (sqlite3 CLI, a stray second instance) produces a
short wait instead of instant `SQLITE_BUSY`
(`crates/vortex-core/src/db.rs`).

### P17. Checkpoints list only the newest 200 — **FIXED**

**Status.** Fixed (round 3) as a full recovery feature, not just pagination:
- `Db::list_checkpoints(conversation_id, before, limit)` is cursor-paginated
  (`id < before ORDER BY id DESC`, `limit` clamped 1..=200) and returns the
  new `CheckpointInfo` DTO (`id`, `run_id`, `path`, `has_original`,
  `created_at`) — snapshot *content* is never shipped in lists
  (`crates/vortex-core/src/store_ckpts.rs`, `crates/vortex-types/src/dto.rs`).
- `Db::get_checkpoint` now also returns the conversation linkage, which the
  restore flow needs to record its own pre-restore snapshot in the same
  conversation.
- New routes (`crates/vortex-server/src/routes_ckpts.rs`):
  `GET /api/conversations/{id}/checkpoints?before=&limit=` and
  `POST /api/checkpoints/{id}/restore`. Restore snapshots the *current*
  content first (`run_id = "restore"`), so every restore is itself
  reversible from the same list; `has_original` rows are written back,
  "new file" rows move the current file to the trash (never unlink).
  Trust model: restore is a human recovery action behind the loopback/LAN
  guard and a UI confirm dialog — models have no path to it — so it
  deliberately bypasses the run approval gate and `file_state` run
  ownership, and is confined to the *currently* configured workspace
  (stored absolute paths outside it are rejected with 400).
- Recording flaw fixed in `tools_write.rs::checkpoint`: "file absent"
  (records NULL = didn't exist) is now distinguished from "present but not
  UTF-8" (records **nothing** + warns — a NULL would have meant "delete this
  file" to a later restore).
- UI: "File checkpoints" panel in TasksView (current conversation) with
  path/badge/time rows, confirm-guarded Restore, toast feedback, refresh
  after restore, and a "Load older" cursor button
  (`crates/vortex-ui/src/tasks.rs`, `api.rs`).
- Tests: cursor pagination walk, restore round-trip at store level,
  non-UTF-8 skip, absent-vs-text recording, outside-workspace rejection
  (route helper). Runtime smoke (mock provider): write → list → restore →
  verify → restore-the-restore → trash-on-"new file" → 400/404 paths, all
  verified via curl.

`store_ckpts.rs` (`LIMIT 200`, no pagination); older checkpoints exist in
the DB but are unreachable through the API. Needs an API/DTO change
(cursor or offset), deferred.

### P18. LAN token comparison is not constant-time — **FIXED**

**Status.** Fixed: the guard compares the provided token with a
constant-time byte equality helper (`ct_eq`, length mismatch still revealed
— unavoidable without a MAC); unit test
`constant_time_equality_matches_str_eq_semantics`
(`crates/vortex-server/src/guard.rs`). LAN mode still has no TLS (see
`docs/SECURITY.md`).

# Known issues

Findings from a full source analysis (Nov 2026, commit `3b58a76` on
`feat/rust-linux-ai-assistant`). Two sections:

- **Confirmed issues** — provable from the code as written (exact trace or
  failing behavior identified by reading; runtime verification noted where
  not performed).
- **Possible concerns** — plausible but runtime-, provider-, or
  model-dependent; need live verification before treatment.

All paths are relative to the repo root. This document records findings only;
no code was changed. Severity notes are informational.

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

**Reproduction (code-level).** Read the drop points above; tokio documents
`kill_on_drop`: "the child process will be sent a kill signal" when the
`Child` is dropped.

**Recommended fix.** Store the `Child` in `BrowserProcess`/`PreviewHandle`
(or drop `kill_on_drop` and rely solely on the existing process-group kill
paths, which already exist for shutdown/cancel).

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

**Reproduction.** In a workspace, `ln -s /tmp/pwn ws/evil` (target absent),
then have the agent `write_file("evil", ...)`: `/tmp/pwn` is created outside
the workspace.

**Recommended fix.** When `canonicalize` fails, use
`std::fs::symlink_metadata` / `read_link` on the component: refuse symlinks
whose resolved target leaves the root, or refuse dangling symlinks outright.

### C3. OpenRouter retry re-delivers already-streamed text (duplicate output)

**Description.** On a retryable mid-stream failure (network error/timeout
after deltas were streamed), `complete` retries the whole POST. Deltas from
attempt 1 were already sent to the caller's channel and appended to the UI;
attempt 2 streams its full text again into the same channel. There is no
reset marker, so the UI shows duplicated/concatenated text, and the provider
call is billed twice.

**Evidence.** Streaming + accumulation:
`crates/vortex-llm/src/openrouter.rs:198-206`; retry loop reuses the same
request body and sender: `openrouter.rs:270-289`; retryable classification
includes `Network`/`Timeout`: `crates/vortex-llm/src/error.rs:27-35`. Tests
only cover failure *before* the stream starts
(`crates/vortex-llm/tests/openrouter.rs:133-145`).

**Impact.** Corrupted assistant output and double billing on flaky
connections. Engine-side persisted final content comes from the last attempt
only, so DB and UI can disagree.

**Recommended fix.** Only retry when nothing has been streamed yet
(attempt produced zero deltas), or signal a "restart" to the consumer before
re-streaming.

### C4. Stale pending approvals survive server restarts (and can be "decided")

**Description.** Startup marks live runs `interrupted`
(`crates/vortex-core/src/db.rs:143-149`) but does not expire their pending
`approvals` rows. After a restart, `GET /api/approvals` (pending) surfaces
approvals whose run is dead; `POST /api/approvals/{id}/decide` resolves them
(`store_appr.rs:27-41` updates unconditionally) with no linkage check —
though nothing executes because the in-memory waiter no longer exists.

**Evidence.** `crates/vortex-core/src/db.rs:143-149` (no approvals cleanup);
`crates/vortex-core/src/store_appr.rs:61-77` (pending list);
`crates/vortex-server/src/routes_misc.rs:96-112` (decide by id only).

**Impact.** Confusing UI state (undismissable "ghost" approvals); DB status
inconsistency. No execution risk: the oneshot receiver is gone.

**Recommended fix.** In `mark_interrupted_on_startup`, also set
`status='expired'` for approvals of interrupted runs.

### C5. Approval timeout race can record the wrong DB status

**Description.** On approval timeout, the pending sender is removed after the
timeout fires; if a user decision lands in the window between timeout and
removal, the DB row may be written as `expired` even though the user decided
(and the oneshot outcome governs the run).

**Evidence.** `crates/vortex-agents/src/approvals.rs:87-104` (timeout branch
removes sender and writes `Expired` without checking whether `decide`
already resolved it, `approvals.rs:39-47`).

**Impact.** Cosmetic state inconsistency between the run outcome and the
approvals table.

**Recommended fix.** Make the timeout branch conditional on the sender still
being present (resolve-and-remove under the same lock, check the removed
value).

### C6. Byte-slicing panics on multi-byte model-provided strings

**Description.** Two truncations slice raw bytes and panic when the cut lands
inside a multi-byte UTF-8 character: `&task[..task.len().min(20)]` and
`&page.url[..page.url.len().min(60)]`.

**Evidence.** `crates/vortex-agents/src/tools_delegate.rs:66`;
`crates/vortex-agents/src/tools_search.rs:116`.

**Impact.** Panics occur inside spawned tool tasks and are converted to tool
failures (`crates/vortex-agents/src/engine.rs:582-589`), so no crash — but
`delegate_subtask` fails for any task description ≥ 20 bytes whose 20th byte
splits a character (e.g., CJK/emoji), and `fetch_url` can fail on such URLs.

**Reproduction.** Delegate with task `"日本語のタスク説明テキストです"` (byte 20
splits a 3-byte char) → tool panics → failure.

**Recommended fix.** Use `chars().take(n).collect::<String>()`.

### C7. SSE/broadcast event loss is silent (server-side)

**Description.** Per-run broadcast channels have capacity 512; `send` errors
and receiver lag are ignored, and the tool-event forwarder is aborted at run
end, dropping queued events. The SSE route translates lag into a `lagged`
hint event but nothing replays the lost events.

**Evidence.** `crates/vortex-agents/src/hub.rs:66,110-111`;
`crates/vortex-agents/src/engine.rs:266` (`fwd.abort()`);
`crates/vortex-server/src/routes_runs.rs:44-52` (lag → `lagged` event, no
replay).

**Impact.** Under load (fast streaming + slow client), the UI can miss
deltas/tool events mid-run; recovery only at terminal `RunFinished` (UI
refetches messages, `crates/vortex-ui/src/sse.rs:201-210`).

**Recommended fix.** On `lagged`, have the UI refetch `/api/runs/{id}/state`;
server-side, consider a per-run event log with `Last-Event-ID` replay.

### C8. Mock demo provider only performs the tool round-trip on the first run

**Description.** `DemoMockLlm` increments a shared `turn_counter` per
completion and never resets it, despite the comment "every run starts
fresh". With `VORTEX_MOCK_SCRIPT=tools`, only the very first run of the
server process gets the scripted `web_search` call; later runs skip straight
to the text branch.

**Evidence.** `crates/vortex-server/src/mock_llm.rs:19-20` (comment),
`61-66` (counter), `73` (`script == "tools" && turn == 0`).

**Impact.** Offline demos/tests of the tool pipeline silently degrade after
the first run.

**Recommended fix.** Reset or derive the turn from the request (e.g., count
tool-role messages in `req.messages`) instead of a process-global counter.

### C9. `VORTEX_SEARXNG_URL` is documented but never read

**Description.** `.env.example` advertises `VORTEX_SEARXNG_URL` as a testing
override, but no code reads it; the search provider URL comes only from
runtime Settings (SQLite).

**Evidence.** `.env.example:27`; grep across `crates/**/*.rs` finds zero
references; config env overrides are limited to `VORTEX_HOST`,
`VORTEX_PORT`, `VORTEX_OPENROUTER_BASE_URL`, `VORTEX_LLM`,
`VORTEX_MOCK_SCRIPT`, `RUST_LOG` (`crates/vortex-core/src/config.rs:102-129`).

**Impact.** Users following `.env.example` get a silently ignored setting.

**Recommended fix.** Either implement the override or remove the line from
`.env.example`.

### C10. XDG base directories are not actually honored

**Description.** Docs (module header, README) describe "XDG" storage, but
`paths.rs` never reads `XDG_CONFIG_HOME`/`XDG_DATA_HOME`/`XDG_CACHE_HOME`;
it hardcodes `$HOME/.config`, `$HOME/.local/share`, `$HOME/.cache` unless the
`VORTEX_*_DIR` overrides are set.

**Evidence.** `crates/vortex-core/src/paths.rs:1-9` (doc claim), `54-70`
(only `VORTEX_*` and `HOME` are consulted).

**Impact.** Users with relocated XDG dirs get data in the default locations.

**Recommended fix.** Read the XDG variables as a middle fallback, or correct
the docs to say "XDG-style defaults".

### C11. `delete_conversation` is non-atomic and orphans related rows

**Description.** Three sequential DELETEs (messages, runs, conversation) run
without a transaction; a failure mid-way leaves partial deletion. It also
never deletes `tool_calls`, `approvals`, or `checkpoints` rows (no FK/cascade
in the schema), which accumulate forever.

**Evidence.** `crates/vortex-core/src/store.rs:95-111`; schema without
foreign keys: `crates/vortex-core/src/db.rs:64-139`.

**Impact.** DB growth over time; rare inconsistent state under BUSY.

**Recommended fix.** Wrap in a transaction; delete related rows by `run_id`.

### C12. Corrupt settings JSON silently resets all user settings

**Description.** `load_settings` falls back to `Settings::default()` on any
deserialization error, discarding every user preference without a warning.

**Evidence.** `crates/vortex-core/src/store_msgs.rs:56-78` (fallback at 73).

**Impact.** Silent data loss (model, workspace, allowlists) after a schema
change or corruption.

**Recommended fix.** Log a warning; consider storing settings as columns or
versioning the blob.

### C13. Deterministic 4xx provider errors are retried

**Description.** `map_status` funnels unmapped codes (including 400) into
`Provider`, and `is_retryable` retries all `Provider` errors — so a
deterministic bad request is retried twice with 1s/3s backoff before failing.

**Evidence.** `crates/vortex-llm/src/error.rs:27-35`;
`crates/vortex-llm/src/openrouter.rs:54-65`.

**Impact.** Wasted latency and API calls on permanent errors.

**Recommended fix.** Only retry 5xx/429/timeout/network; treat 4xx (except
429) as terminal.

### C14. TextDelta delivery is best-effort and can silently diverge from final content

**Description.** Providers use `try_send` on the delta channel and discard
the result; when the consumer is slow/full, deltas are dropped while the
final `CompletionResponse.content` remains complete.

**Evidence.** `crates/vortex-llm/src/openrouter.rs:205`;
`crates/vortex-llm/src/mock.rs:98`.

**Impact.** Mid-run UI text can be missing chunks; only the terminal message
refetch heals it (see C7).

**Recommended fix.** Await the send (bounded backpressure) or emit an
explicit "delta dropped" marker.

### C15. UI: no EventSource reconnect/recovery, and lost events are never reconciled mid-run

**Description.** The UI implements no re-subscription logic: on `error` it
does a one-shot `run_state` fetch and shows a toast if that fails. Deltas are
blind appends with no ids/sequence numbers, so anything missed during a gap
is lost until the terminal `RunFinished` refetch.

**Evidence.** `crates/vortex-ui/src/sse.rs:35-52` (only error handler),
`71-73` (blind append), `125-138` (`ToolCallFinished` for an unseen call is
dropped), `201-210` (terminal refetch).

**Impact.** After transient network drops the run can appear frozen or
truncated; a stuck "running…" tool row never resolves.

**Recommended fix.** Detect `readyState == CLOSED` and re-subscribe with a
state refetch; handle orphan `ToolCallFinished` by inserting a record.

### C16. UI: `run_id` is not cleared when switching conversations

**Description.** Loading a different conversation resets run visuals but not
the `run_id` signal, so Stop/Retry buttons can act on the previous
conversation's run (Retry would stream an old run's events into the newly
opened conversation's panels).

**Evidence.** `crates/vortex-ui/src/sidebar.rs:119-125` (reset without
`run_id`); `crates/vortex-ui/src/chat.rs:156,169` (Stop/Retry read `run_id`).

**Impact.** Wrong-target actions across conversations.

**Recommended fix.** Clear `run_id` (and close the subscription) on
conversation switch.

### C17. UI: optimistic-bubble sentinel `id: -1` collides across rapid sends

**Description.** Optimistic user bubbles all use `id: -1` while `<For>` keys
on `id`; two rapid sends create duplicate keys, and error rollback removes
*all* bubbles with `id == -1`, not just the failed one.

**Evidence.** `crates/vortex-ui/src/chat.rs:96-101` (sentinel),
`chat.rs:36` (key), `chat.rs:117` (`retain(|x| x.id != -1)`).

**Impact.** Rendering glitches/lost bubbles when sends overlap.

**Recommended fix.** Use unique negative ids (decrementing counter).

### C18. `ammonia` is a declared but unused dependency; sanitization is hand-rolled

**Description.** `vortex-ui/Cargo.toml` lists `ammonia = "4"`, but no source
file uses it. Sanitization is a manual pulldown-cmark walker that never
emits raw HTML (which is itself sound), with a strict link scheme allowlist.

**Evidence.** `crates/vortex-ui/Cargo.toml:34`; no `use ammonia` in
`crates/vortex-ui/src/` (grep); walker: `crates/vortex-ui/src/markdown.rs:16-111`,
link policy `markdown.rs:83-96`.

**Impact.** Wasm binary bloat and a misleading dependency footprint; any
doc claiming "ammonia sanitization" is inaccurate.

**Recommended fix.** Remove the dependency (or switch to it deliberately).

### C19. `expect()` panics in WASM code paths

**Description.** Several `expect` calls can trap the WASM module instead of
showing a user-visible error, e.g. `EventSource::new(...).expect(...)`
(constructors can throw), listener registration, `use_context` lookups, and
theme application.

**Evidence.** `crates/vortex-ui/src/sse.rs:19,56,59`;
`crates/vortex-ui/src/state.rs:122`; `crates/vortex-ui/src/markdown.rs:133`;
`crates/vortex-ui/src/lib.rs:137`; `crates/vortex-ui/src/chat.rs:12,145`;
`crates/vortex-ui/src/sidebar.rs:10`; `crates/vortex-ui/src/tasks.rs:11`;
`crates/vortex-ui/src/settings_ui.rs:11`.

**Impact.** A blank/frozen UI on environmental failures (e.g., browser
connection limits) with no diagnostics beyond the console.

**Recommended fix.** Convert to `Result`/banner paths.

### C20. LAN token rewrite of `config.toml` drops user comments

**Description.** First LAN-mode boot regenerates the whole config file via
`toml::to_string_pretty`, discarding comments and unknown keys.

**Evidence.** `crates/vortex-server/src/main.rs:204-224` (write at 211-213).

**Impact.** One-time surprise loss of config annotations when enabling LAN
mode.

**Recommended fix.** Append/patch only the token key, or document the
behavior.

### C21. `retry_run` lacks the API-key pre-flight that `post_chat` has

**Description.** `POST /api/conversations/{id}/chat` returns 503 with setup
guidance when no key is configured, but `POST /api/runs/{id}/retry` starts a
run that will fail inside the provider call.

**Evidence.** Pre-flight: `crates/vortex-server/src/routes_conv.rs:126-132`;
absent in `crates/vortex-server/src/routes_runs.rs:118-166`.

**Impact.** Confusing failed run instead of an actionable error.

**Recommended fix.** Duplicate the pre-flight check in `retry_run`.

### C22. Repo hygiene

- `.gitignore:6` exempts `crates/vortex-server/ui-dist/.gitkeep`, which does
  not exist; `ui-dist/` is entirely untracked, so fresh clones serve the
  placeholder page until `scripts/build-ui.sh` runs (by design, but the
  dangling exception is misleading).
- No `LICENSE` file although `Cargo.toml:26` and README claim MIT.
- No CI (`.github/` absent); all checks are manual.
- `main` is stale (Initial commit only); all work lives on
  `feat/rust-linux-ai-assistant` (11 commits ahead), no PR.

### C23. Test-suite hygiene (minor)

- `paths.rs` test mutates process env (`crates/vortex-core/src/paths.rs:80-93`),
  racy under parallel tests; `VORTEX_TEST_HOME` is set but never read.
- Store tests leave real DB files in `/tmp`
  (`crates/vortex-core/src/store_msgs.rs:101`, `store_calls.rs:63`,
  `store_ckpts.rs:76,97`).
- `vortex-llm`'s `MockLlm` cannot script failures despite the module doc
  claiming it (`crates/vortex-llm/src/mock.rs:4,15-18`).

### C24. Minor protocol/robustness deviations (vortex-llm)

- SSE parser does not join multi-line `data:` fields per spec; a
  spec-compliant provider splitting a JSON payload across `data:` lines
  yields `MalformedStream` (non-retryable)
  (`crates/vortex-llm/src/sse.rs:32-37`, `openrouter.rs:186-187`).
- >1 MiB single-line buffers are silently cleared, corrupting the next
  payload (`sse.rs:39-42`).
- Timeout errors hardcode `Timeout(120)` regardless of the failing phase
  (`openrouter.rs:149,172`); `models()` logs `is_timeout()` instead of the
  error (`openrouter.rs:302`).
- Missing `tool_call_id` is serialized as `""`
  (`openrouter.rs:86-89`), which strict providers may reject.
- `.env` parser: inline comments become part of the value, `export KEY=…`
  lines are skipped (`crates/vortex-core/src/config.rs:177-184`).
- Conversation search does not escape LIKE wildcards `%`/`_`
  (`crates/vortex-core/src/store.rs:76`).
- `post_chat` finds the conversation via a full `list_conversations()` scan
  (`crates/vortex-server/src/routes_conv.rs:113-122`) — O(n) per message.

---

## Possible concerns

### P1. SSRF via DNS rebinding (TOCTOU)

`check_url` resolves and validates every address, but reqwest re-resolves DNS
at connect time; no IP pinning between check and connect, at first hop or on
redirects. A rebinding DNS server could pass validation then resolve to
loopback/cloud metadata. Evidence:
`crates/vortex-agents/src/safe_http.rs:74-88`. Already acknowledged in
`docs/SECURITY.md` ("Residual risk"). Mitigation: resolve once, connect to
the validated IP with Host-header override, or use a custom resolver.

### P2. TOCTOU between workspace `resolve` and the file operation

An out-of-band local process could swap a path component to a symlink
between validation and `write`/`rename` (`workspace.rs:74-93`,
`tools_write.rs:88,209`). In-process agents are serialized by the per-path
mutex; external processes are not.

### P3. Provider-controlled tool-call `index` can allocate unboundedly

`openrouter.rs:213-229` grows the tool-call slot vector to fit a
provider-supplied `index` (u64 → usize) with no cap; a hostile endpoint could
trigger OOM. Also, deltas omitting `index` default to slot 0 and can silently
merge two parallel tool calls (`openrouter.rs:213,230-237`).

### P4. `fetch_url` downloads the full body before applying the size cap

`resp.bytes()` buffers everything, then truncates
(`crates/vortex-agents/src/safe_http.rs:108-113`); a hostile server can stream
gigabytes within the 20s timeout. Mitigation: stream with a cap.

### P5. Empty tool-call ids from a provider break the next iteration

Defensive handling generates UUIDs for DB records but still sends empty
`tool_call_id` back to the provider (`engine.rs:498-503,544-549`);
OpenAI-compatible APIs reject empty ids, so the following request can fail.

### P6. Mutex-poisoning cascades

`hub.rs`, `preview.rs`, `engine.rs`, and `db.rs` use `.expect()` on std mutex
locks (e.g., `hub.rs:39,42,116,126,141,144,147`; `db.rs:45,54,58`). A panic
inside a critical section poisons the mutex and turns every later access into
a panic.

### P7. Chromium launch failure panics inside `page_for` — partially fixed

Fixed alongside C1: `pid.unwrap()` in `browser.rs` and `tools_preview.rs`
was replaced with proper `ToolError`s. `.expect("stderr piped")` remains
(safe: `Stdio::piped()` is set unconditionally); `tools_command.rs:176`
still indexes `argv[0]` without an empty check in a `pub fn`.

### P8. `run_command` output drain has no deadline after normal exit

`tools_command.rs:211-212` reads stdout/stderr to EOF after the child exits;
a grandchild holding the pipes open can stall the tool indefinitely
(cancel/timeout paths kill the group, but normal exit does not).

### P9. Dev-server port detection can attach to the wrong service

`wait_for_port` regex-matches any `host:port` printed on stdout
(`tools_preview.rs:165-193`, only 8417 excluded); a dev server printing an
unrelated port registers a wrong preview URL that browser tools would then
drive.

### P10. UI renders preview URLs into `href` without a scheme allowlist

`crates/vortex-ui/src/tasks.rs:209` — unlike markdown links
(`markdown.rs:83-96`), `p.url` is not scheme-checked. Server-controlled data
today, so this is defense-in-depth only.

### P11. UI silently swallows many request errors

Boot conversations/approvals (`lib.rs:42-47`), 5s approvals poll
(`lib.rs:56`), search (`sidebar.rs:42-44`), rename/delete
(`sidebar.rs:76,93`), theme save (`lib.rs:152-154`), Stop (`chat.rs:157`),
previews fetch (`tasks.rs:193`), models fetch (`settings_ui.rs:32`) — a
failed delete looks successful until refetch.

### P12. UI misc

- "Copied" toast is shown even when the clipboard write rejects
  (`chat.rs:214-222`).
- Markdown: ordered lists not starting at 1 emit `<li>` without a wrapping
  `<ol>` (`markdown.rs:60-63`); blockquotes emit an extra `<p>`
  (`markdown.rs:58`) — render artifacts, not injection.
- `urlencode` encodes space as `+` (form-style) (`api.rs:185`); breaks if a
  future endpoint decodes `+` literally.
- 3xx responses are treated as success (`api.rs:18,46`).
- Approvals 5s poll can briefly resurrect an approval resolved via SSE
  (`lib.rs:52-62` vs `sse.rs:140-171`).
- Full markdown re-render + syntax highlight on every streamed delta
  (`chat.rs:44`) — jank on long outputs.

### P13. `Trunk.toml` declares `public-dir = "public"` but no such directory exists

`crates/vortex-ui/Trunk.toml:4` — harmless today (Trunk skips it) but a trap
for future asset work.

### P14. Live OpenRouter behavior is unverified end-to-end

All streaming/tool-call tests run against a protocol-faithful local mock
server (`crates/vortex-llm/tests/openrouter.rs`). Real-provider framing
differences (e.g., multi-line `data:` events — see C24) would surface only
in production use. A key exists in `.env`; one live smoke test (opt-in, costs
credits) would move this out of the concerns list.

### P15. Token budget can be exceeded within one iteration

Usage is only checked after tool execution each iteration
(`engine.rs:521-526`); a single huge completion can overshoot
`max_total_tokens` before the run stops.

### P16. SQLite has no `busy_timeout`

Single in-process connection is mutex-serialized, but any second process
touching `data/vortex.db` (a second server instance, sqlite3 CLI) can
produce immediate `SQLITE_BUSY` errors (`crates/vortex-core/src/db.rs:24-27`).

### P17. Checkpoints list only the newest 200

`store_ckpts.rs:34` (`LIMIT 200`, no pagination); older checkpoints exist in
the DB but are unreachable through the API.

### P18. LAN token comparison is not constant-time

`guard.rs:73` (`t == *token`); acknowledged as acceptable for the home-LAN
threat model in `docs/SECURITY.md:92-94`, and LAN mode has no TLS.

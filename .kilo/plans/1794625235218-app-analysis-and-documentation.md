# Plan: Land pending UI fix, assess project, write developer/AI documentation

Repo: `/home/robotcowboy808/Documents/Vortex-Ai-Chat` · Branch: `feat/rust-linux-ai-assistant` (10 commits, pushed, up to date with origin) · ~11,300 LOC Rust across 6 crates.

## Context (verified facts)

- One uncommitted change: `crates/vortex-ui/index.html` — Trunk `data-target-name="vortex_ui"` → `"vortex-ui"`. Verified harmless: after the edit (Oct 5, 22:19) the UI was rebuilt and `ui-dist/` output was byte-identical to before; the release binary built at 22:20.
- `crates/vortex-server/ui-dist/*` is **gitignored and entirely untracked** (the `.gitkeep` referenced by `.gitignore` doesn't exist). A fresh clone serves a placeholder page until `scripts/build-ui.sh` runs. Release binaries embed `ui-dist/` via `rust-embed`; debug builds read it from disk.
- The WASM UI calls relative `/api` paths and is always served by the Axum server — there is no standalone `trunk serve` frontend flow. "Starting the website" always means: build UI assets → run `vortex-server` → open `http://127.0.0.1:8417`.
- Pinned toolchain: Rust **1.89.0** (`rust-toolchain.toml`), wasm-bindgen-cli **0.2.129** (matches `Cargo.lock`), Trunk for the WASM build. `vortex-ui` is excluded from workspace `default-members` (native `cargo build/test/clippy` never touches it).
- `.env` exists with a non-empty `OPENROUTER_API_KEY` (gitignored, never committed). No CI (`.github/` absent), no `LICENSE` file (README/Cargo claim MIT), `main` holds only the Initial commit.
- Current README has a "Testing without credentials" section implying the mock provider is a working no-key mode — the user requires docs to state a **real API key is required** and never imply mocks substitute for one.

## Decisions (flag any you disagree with)

1. **"Merge index.html" = commit the pending one-line fix** to `feat/rust-linux-ai-assistant`. Not a branch merge into `main` — branch/PR status is reported in the assessment, not acted on.
2. **Documentation deliverables**: `AGENTS.md` (repo root), `docs/DEVELOPER_GUIDE.md`, `docs/KNOWN_ISSUES.md`, plus targeted README corrections.
3. **Analysis identifies bugs/risks but does not fix them.** Fixes are follow-up work unless a finding blocks documentation accuracy.
4. **API-key stance in all docs**: the application requires a real OpenRouter API key to function. The mock provider is documented strictly as an offline test harness for the request/tool/SSE plumbing — never as a way to use the assistant.
5. **Out of scope**: merging to `main`, creating a PR, adding CI, packaging (.deb/.rpm), a `LICENSE` file (listed as recommendations only).

---

## Phase 1 — Commit the pending fix ("merge index.html")

1. Confirm working tree still shows only `crates/vortex-ui/index.html` modified (`git status`).
2. `git add crates/vortex-ui/index.html`
3. Commit (matches repo conventional-commit style):
   `fix(ui): align Trunk data-target-name with crate name (vortex-ui)`
   Body note: dist output was already `vortex-ui-*`; rename makes source match build output (verified byte-identical rebuild).
4. Do **not** push unless the user asks.

## Phase 2 — Assessment checkpoint ("what else needs attention")

Run and record results (expected baselines in parentheses):

- `cargo test` (43 tests, all offline)
- `cargo fmt --all -- --check` (clean)
- `cargo clippy --all-targets` (clean)
- `cargo build --release -p vortex-server` (succeeds; embeds existing `ui-dist/`)

Compile a short assessment list (seeded with already-verified findings):

- `main` stale / feature branch unmerged, no PR, `gh` not installed
- No CI workflows
- No `LICENSE` file despite MIT claims
- `.gitignore` references a `ui-dist/.gitkeep` that doesn't exist (fresh clones rely on the server's placeholder page)
- README mock wording contradicts the real-key requirement (fixed in Phase 4)
- Live OpenRouter streaming never verified against the real provider (only a protocol-faithful mock); a key is present in `.env`

This assessment is included in the final report before documentation work proceeds.

## Phase 3 — Full application analysis

Read every source file (~11.3k LOC), module by module, recording purpose, key types/functions, data flow, and edge cases. Per-crate focus:

- **vortex-types**: DTO/event/settings contracts shared by server and WASM.
- **vortex-core**: `paths.rs` XDG resolution; `config.rs` precedence (`.env` > env vars > `config.toml`) and loopback enforcement; `db.rs` SQLite/WAL + `spawn_blocking`; store modules (messages/runs/tool calls/approvals/checkpoints) and interrupted-run recovery at startup.
- **vortex-llm**: `openrouter.rs` streaming, tool-call accumulation across SSE deltas, bounded retries; `sse.rs` partial-event handling; `mock.rs` scripts (`echo`, `tools`).
- **vortex-agents** (largest surface): `engine.rs` run loop, budgets (iterations/deadline/tokens), cancellation tree; `registry.rs` risk classification + approval routing; `workspace.rs` path confinement/symlink handling/per-path mutex/ownership; `safe_http.rs` SSRF defenses; `approvals.rs` gate + timeout auto-deny; `scheduler.rs` slot acquisition/release under cancellation; `preview*.rs` process-group lifecycle; `browser.rs`/`cdp.rs` Chromium launch + DevTools protocol; `search.rs` SearXNG/mock providers; `hub.rs` event fan-out; file/write/command/delegate tools.
- **vortex-server**: `main.rs` startup/shutdown; `guard.rs` Host allow-list, Origin checks, LAN bearer-token mode; route modules (REST surface + SSE endpoints); `static_files.rs` embed vs. disk, traversal blocking, SPA fallback; `mock_llm.rs`.
- **vortex-ui**: Leptos CSR app structure (`lib.rs`, `state.rs`, `chat.rs`, `sidebar.rs`, `tasks.rs`, `settings_ui.rs`), `api.rs` client, `sse.rs` EventSource handling (incl. reconnect behavior), `markdown.rs` sanitization (`ammonia`) + `syntect` highlighting.

Cross-cutting risk review — each finding gets **file:line evidence** and is classified:

- **Confirmed issue**: provable from code paths (with a failing/passing test or exact trace).
- **Possible concern**: plausible but runtime- or model-dependent; needs live verification.

Target areas: approval timeout/deny paths; sub-agent self-approval impossibility; scheduler slot leaks on cancel/panic; workspace symlink & `..` escapes; SSRF via DNS rebinding/redirects; SSE broadcast lag → dropped events; streamed tool-call fragmentation; retry idempotency; preview/browser process cleanup on crash/shutdown; SQLite locking under concurrency; guard bypass vectors; UI EventSource reconnect/stale state; `index.html` target-name class of build-config drift.

## Phase 4 — Documentation deliverables

### 4.1 `AGENTS.md` (repo root) — orientation for AI agents
Project one-liner; crate map with dependency direction; exact commands (build, test, fmt, clippy, `scripts/build-ui.sh`, `scripts/build-release.sh`, run); toolchain pins (Rust 1.89.0, wasm-bindgen-cli 0.2.129, Trunk); invariants that must not break (loopback-only binding, workspace confinement, approval gating, sub-agent caps, `.env` never committed); gotchas (ui-dist untracked + placeholder page; `vortex-ui` outside `default-members`; debug reads dist from disk vs. release embeds at compile time); pointers into `docs/`.

### 4.2 `docs/DEVELOPER_GUIDE.md` — the thorough guide
1. **Overview & architecture** — expand the crate/data-flow model with module-level detail.
2. **Requirements** — Rust 1.89.0, cc/pkg-config/libsqlite3-dev/build-essential; Trunk + wasm-bindgen-cli 0.2.129 for UI builds; **a real OpenRouter API key (required — the app cannot function without one)**; optional Chromium (browser tools) and SearXNG (research).
3. **Running the website locally (dev)** — exact sequence from a fresh clone: `cp .env.example .env` + real key → `chmod 600 .env` → `scripts/build-ui.sh` → `cargo run -p vortex-server` → `http://127.0.0.1:8417`; explain debug-mode disk-read rebuild loop for UI iteration; explain the placeholder page when dist is missing.
4. **Release build & run** — `scripts/build-release.sh` → single embedded binary.
5. **Mock provider (test harness only)** — explicit statement: it exercises plumbing offline for development/tests; it does not provide real assistant functionality and is not a substitute for an API key.
6. **Configuration** — `config.toml` keys, env overrides, precedence, XDG storage table, LAN mode (off by default, token-gated).
7. **Component walkthroughs** — per crate/module: responsibility, key types, interactions, with file paths.
8. **Scripts explained** — step-by-step of both build scripts, including version-matching requirements.
9. **HTTP API reference** — REST endpoints + SSE event catalog (derived from routes and `vortex-types/events.rs`).
10. **Data model** — SQLite schema, run lifecycle (incl. interrupted-on-restart semantics), checkpoints/trash.
11. **Agent loop semantics** — budgets, approvals, scheduler caps, sub-agent inheritance rules.
12. **Testing** — how to run, what the 43 tests cover, how to add tests.
13. **Edge cases & known limitations** — consolidated from analysis + README.

### 4.3 `docs/KNOWN_ISSUES.md`
Findings from Phase 3, two sections — **Confirmed issues** and **Possible concerns** — each entry: description, evidence (`file:line`), impact, reproduction (if any), recommended fix. No code changes.

### 4.4 `README.md` corrections
- Rewrite the "Testing without credentials" section: lead with the real-API-key requirement; reposition the mock as an offline plumbing test harness only.
- Note the fresh-clone UI build requirement (placeholder page until `build-ui.sh`).
- Link `AGENTS.md`, `docs/DEVELOPER_GUIDE.md`, `docs/KNOWN_ISSUES.md`.
- Touch `docs/ARCHITECTURE.md`/`SECURITY.md` only where analysis proves them inaccurate.

## Phase 5 — Validation & final report

1. **Execute every command written into the docs** (fresh-shell sequence) and fix any doc that doesn't match reality; smoke-run the server and `curl /api/health`.
2. Re-run `cargo test`, `cargo fmt --all -- --check`, `cargo clippy --all-targets` to confirm the repo is still green after doc changes.
3. Commit docs in repo style (e.g., `docs: developer guide, AGENTS.md, known issues; README corrections`) — separate from the Phase 1 fix commit.
4. Final report: Phase 2 assessment, doc index, confirmed-vs-possible issue counts with the top findings.
5. **Optional, only with explicit user consent**: one live smoke test against OpenRouter using the key in `.env` (consumes credits) to move "live streaming unverified" out of the concerns list.

## Risks / mitigations

- **Doc drift**: every documented command is executed during validation (Phase 5.1).
- **Live test costs credits**: opt-in only, never automatic.
- **Git commits require permission prompts**: Phase 1 and Phase 5.3 each perform exactly one commit, no pushes.
- **Analysis false positives**: strict evidence rule (file:line + trace/test) separates confirmed issues from concerns.

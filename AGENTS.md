# AGENTS.md — orientation for AI agents working in this repo

Vortex-Ai-Chat is a local-first, Linux-only AI assistant written entirely in Rust: an Axum server (REST + SSE + embedded WASM UI), a multi-agent engine with tool/approval/scheduler enforcement, and a Leptos CSR UI compiled to WebAssembly. **The application requires a real OpenRouter API key to function**; the built-in mock provider is an offline test harness for the request/tool/SSE plumbing only — never a substitute for a key.

## Crate map (dependency direction: top depends on bottom)

```
vortex-server  (binary: Axum REST/SSE, guards, embedded UI assets)
vortex-ui      (WASM binary: Leptos CSR UI; NOT in workspace default-members)
      │
      ▼
vortex-agents  (engine, tool registry, workspace confinement, approvals,
      │         scheduler, previews, browser/CDP, search, hub)
      ▼
vortex-llm     (LlmClient trait, OpenRouter SSE client, scripted mock)
      │
      ▼
vortex-core    (paths, boot config, SQLite store)
      │
      ▼
vortex-types   (shared serde DTOs: chat, events, settings; compiles to WASM)
```

`vortex-ui` depends on `vortex-types` only (shared contract) and talks to the server over relative `/api` paths — it must always be served by the Axum server; there is no standalone `trunk serve` workflow.

## Commands

```bash
cargo test                          # 105 tests, all offline (mocks + local test servers; +1 #[ignore]d live-provider smoke)
cargo fmt --all -- --check          # must stay clean
cargo clippy --all-targets          # must stay clean
scripts/build-ui.sh                 # Trunk → crates/vortex-server/ui-dist (needs trunk + wasm-bindgen-cli)
cargo build --release -p vortex-server
scripts/build-release.sh            # build-ui.sh + release server (single embedded binary)
./target/release/vortex-server      # run; serves http://127.0.0.1:8417
```

Native `cargo build/test/clippy` from the workspace root **never touches `vortex-ui`** (it is excluded from `default-members` in the root `Cargo.toml` because it targets `wasm32-unknown-unknown`). To lint the UI: `cargo clippy -p vortex-ui --target wasm32-unknown-unknown`.

## Toolchain pins (must match)

- Rust **1.89.0** — pinned by `rust-toolchain.toml`; rustup enforces it.
- `wasm-bindgen-cli` **0.2.129** — must equal the `wasm-bindgen` version in `Cargo.lock` or the Trunk build fails.
- Trunk **0.21.14** — `cargo install trunk --version 0.21.14 --locked` (Trunk 0.22+ requires rustc ≥ 1.96, incompatible with the 1.89.0 pin; CI is pinned the same way).
- System deps: `build-essential pkg-config libsqlite3-dev` (SQLite is bundled via rusqlite `bundled` feature; cc is still required).

## Invariants — do not break these

1. **Loopback-only binding.** `AppConfig::load` hard-fails on a non-loopback host unless `[server.lan] enabled = true` (`crates/vortex-core/src/config.rs`). The request guard rejects non-loopback `Host` headers and cross-origin writes (`crates/vortex-server/src/guard.rs`). LAN mode is opt-in and token-gated.
2. **Workspace confinement.** File tools are refused until a workspace root is set; paths are resolved/canonicalized inside it; `..` and symlink escapes are rejected; writes are mutex-serialized with cross-run ownership and checkpoints (`crates/vortex-agents/src/workspace.rs`).
3. **Approval gating.** `Risk::Consequential` actions always route through the human approval gate regardless of permission profile or agent depth; the only resolution path is `POST /api/approvals/{id}/decide`; timeout auto-denies (`crates/vortex-agents/src/registry.rs`, `approvals.rs`). Models have no path to approval.
4. **Sub-agent caps.** Concurrency is enforced server-side by the scheduler semaphore (default 6, ceiling `AgentSettings::MAX_LIMIT_CEILING = 16`), never by prompts; nested agents share the same global gate; sub-agents inherit at most the coordinator's tool set.
5. **SSRF defenses.** Model-chosen URLs: http/https only; loopback/private/link-local refused after DNS resolution and on every redirect hop; connections are IP-pinned to a validated address per hop (`crates/vortex-agents/src/safe_http.rs`).
6. **Secrets.** `.env` is gitignored — never commit it, never print `OPENROUTER_API_KEY`, never log request headers/bodies, never put keys in error messages or docs.
7. **Interrupted-run honesty.** Runs live at process exit are marked `interrupted` at startup; nothing consequential is ever replayed automatically (`crates/vortex-core/src/db.rs`).

## Gotchas

- **`crates/vortex-server/ui-dist/` is gitignored and untracked.** A fresh clone has no UI assets; the server still compiles (build.rs writes a placeholder `index.html`) and serves an instructional placeholder page until `scripts/build-ui.sh` runs. The directory is ignored wholesale — Trunk empties it on every build, so no sentinel file can be tracked inside it.
- **Debug vs release asset serving.** `rust-embed` reads `ui-dist/` **from disk** in debug builds (UI rebuilds appear on refresh without recompiling the server) but **embeds at compile time** in release builds (must rebuild the server after UI changes).
- **`vortex-ui` is outside `default-members`** — workspace-wide cargo commands silently skip it; CI (`.github/workflows/ci.yml`) lints both targets, but run UI builds/lints explicitly when iterating locally.
- **wasm-bindgen version drift** breaks `scripts/build-ui.sh` — check `Cargo.lock` after any dependency bump.
- **Mock LLM is opt-in** via `VORTEX_LLM=mock` (`VORTEX_MOCK_SCRIPT=echo|tools`). Without a key and without the mock, the server still starts (to show setup guidance) but `POST /api/conversations/{id}/chat` returns 503.
- **Settings live in SQLite** (`~/.local/share/vortex/data/vortex.db`); boot config lives in `~/.config/vortex/config.toml` + `.env`/env vars. Precedence: env vars > `.env` > `config.toml`.
- CI lives at `.github/workflows/ci.yml` (fmt, clippy native + wasm, tests, Trunk build); a `LICENSE` (MIT) file now matches README/Cargo.toml. `main` branch is still stale — work happens on `feat/rust-linux-ai-assistant`.

## Documentation pointers

- `docs/DEVELOPER_GUIDE.md` — full developer guide: setup, running locally, architecture walkthroughs, HTTP API reference, data model, agent-loop semantics, testing.
- `docs/KNOWN_ISSUES.md` — confirmed issues and possible concerns with file:line evidence.
- `docs/ARCHITECTURE.md` — crate layout and data-flow overview.
- `docs/SECURITY.md` — threat model, permission model, honest limitations.
- `README.md` — user-facing setup and feature overview.

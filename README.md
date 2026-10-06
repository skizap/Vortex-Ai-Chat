# Vortex-Ai-Chat

A local-first, privacy-respecting AI assistant for Linux — chat, internet research with citations, workspace coding, controlled website previews, and isolated browser automation, with a coordinator that can delegate to **up to six concurrent sub-agents**. The entire application — backend, agent orchestration, tools, storage, and the browser UI — is written in **Rust** (the UI compiles to WebAssembly).

> Linux only (tested baseline: Linux Mint 22.3 / Ubuntu 24.04, x86_64). Windows/macOS are out of scope.

## What it does

- **Chat** — persistent conversations with streamed responses, safe Markdown, syntax-highlighted code, copy buttons, search/rename/delete.
- **Research** — pluggable search (SearXNG or built-in mock) with source links; SSRF-protected page fetching.
- **Coding** — file tools confined to a user-selected workspace, exact-match edits with unified diffs and automatic checkpoints, optional approval-gated command execution (no shell), loopback-only website previews.
- **Browser** — isolated Chromium instance (separate profile, never yours) driven over the DevTools Protocol from Rust; login/CAPTCHA handoff to the human.
- **Multi-agent** — the coordinator delegates self-contained subtasks to sub-agents; **6 run concurrently by default** (configurable 1–16, enforced server-side by a scheduler, not by prompts), nested agents share the same cap, and results are tracked in the Tasks panel.
- **Approvals** — consequential actions (deletions, sensitive commands, purchases, publishing, …) always pause for explicit human approval; sub-agents cannot self-approve.

## Requirements (tested baseline: Linux Mint 22.3 / Ubuntu 24.04)

- Rust **1.89.0** (pinned by `rust-toolchain.toml`; rustup installs it automatically)
- C compiler (cc) — for the bundled SQLite
- `libsqlite3-dev`, `pkg-config`, `build-essential` — `sudo apt install build-essential pkg-config libsqlite3-dev` (Debian/Ubuntu)
- **Optional:** Chromium for browser tools (`sudo apt install chromium`), a SearXNG instance for research
- **Optional, UI builds only:** `cargo install trunk --locked` and a matching `cargo install wasm-bindgen-cli --version <version in Cargo.lock>` (currently 0.2.129). Not required to run a release binary that already embeds the UI.

No Python and no Node.js are used or required by the assistant itself.

## Setup

```bash
git clone https://github.com/skizap/Vortex-Ai-Chat.git
cd Vortex-Ai-Chat
cp .env.example .env          # then edit: put your key in OPENROUTER_API_KEY
chmod 600 .env                # keep secrets private
scripts/build-release.sh      # builds WASM UI + native release binary
./target/release/vortex-server
# open http://127.0.0.1:8417
```

`.env` is loaded from the working directory at startup; environment variables
take precedence over `config.toml`, which lives under `~/.config/vortex/`.

### Testing without credentials

```bash
VORTEX_LLM=mock ./target/release/vortex-server
```

runs against a built-in mock provider (no network, no key): `VORTEX_MOCK_SCRIPT=echo`
echoes your message; `VORTEX_MOCK_SCRIPT=tools` performs a real web_search tool
round-trip so the whole tool pipeline can be exercised offline.

## Storage locations (XDG)

| Purpose | Default | Override |
| --- | --- | --- |
| Config (`config.toml`) | `~/.config/vortex/` | `VORTEX_CONFIG_DIR` |
| Data (SQLite DB, trash) | `~/.local/share/vortex/` | `VORTEX_DATA_DIR` |
| Cache (isolated browser profile) | `~/.cache/vortex/` | `VORTEX_CACHE_DIR` |

All paths are created on first run. The SQLite database is `data/vortex.db`
(WAL mode); deleted files go to `data/trash/` (recoverable).

## Configuration

Runtime settings (model, temperature, tools, permissions, agent limits,
theme, workspace) live in the app's **Settings tab** and persist in SQLite.

Boot-level settings live in `~/.config/vortex/config.toml` (all optional):

```toml
[server]
host = "127.0.0.1"   # loopback enforced; non-loopback requires [server.lan]
port = 8417

[server.lan]         # OFF by default; opt-in LAN exposure requires a token
enabled = false
token = ""

[llm]
provider = "openrouter"            # or "mock"
base_url = "https://openrouter.ai/api/v1"
# openrouter_api_key = "sk-or-..." # or set OPENROUTER_API_KEY in the env
```

Environment overrides: `VORTEX_HOST`, `VORTEX_PORT`,
`VORTEX_OPENROUTER_BASE_URL`, `VORTEX_LLM`, `VORTEX_MOCK_SCRIPT`,
`VORTEX_CHROME_PATH`, `RUST_LOG`.

## Search setup (research mode)

OpenRouter does **not** search the web; Vortex integrates search itself.
Point Settings → Search at a SearXNG instance you control, e.g.:

```bash
docker run --rm -p 127.0.0.1:8888:8080 searxng/searxng
# then enable JSON output: settings.yml → search.formats: [html, json]
```

and set `http://127.0.0.1:8888` in Settings → Search. When no provider is
configured, research tools say so honestly instead of pretending.

## Sub-agents (six concurrent by default)

`Settings → Agents` exposes the concurrency limit (default **6**, ceiling 16),
run timeout, iteration/token budgets, and optional recursive delegation
(disabled by default). The cap is enforced server-side by a scheduler
semaphore — the 7th sub-agent waits in the queue; nested agents count against
the same global limit; the coordinator itself never consumes a slot.
Sub-agents inherit at most the coordinator's tool set and cannot widen it.

## Architecture & security

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the crate layout, data
flow, and the agent loop; see [`docs/SECURITY.md`](docs/SECURITY.md) for the
threat model, permission model, and known limitations.

## Development

```bash
cargo test                       # 43 tests (all offline: mocks + local servers)
cargo fmt --all -- --check
cargo clippy --all-targets       # clean
scripts/build-ui.sh              # WASM UI only
cargo build --release -p vortex-server
```

## Known limitations

- Chromium must be installed for browser tools; the UI shows setup steps
  until it is present (the health endpoint reports it honestly).
- Wayland/X11 desktop automation is not implemented (browser and workspace
  tools are unaffected).
- Model/tool support depends on the chosen model — the catalog's
  `supported_parameters` flags tool capability, but behavior varies by model.
- Live OpenRouter streaming was verified against a protocol-faithful local
  mock server; end-to-end live-provider behavior requires your own API key.
- Packaging (.deb/.rpm) and an install wizard are not implemented; run the
  binary directly as your own user (no root, no services).

## License

MIT — see `Cargo.toml`.

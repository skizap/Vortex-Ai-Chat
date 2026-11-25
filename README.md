# Vortex-Ai-Chat

[![CI](https://github.com/skizap/Vortex-Ai-Chat/actions/workflows/ci.yml/badge.svg)](https://github.com/skizap/Vortex-Ai-Chat/actions/workflows/ci.yml)

A local-first, privacy-respecting AI assistant for Linux — chat, internet research with citations, workspace coding, controlled website previews, isolated browser automation, and durable scheduled jobs, with a coordinator that can delegate to **up to six concurrent sub-agents**. The entire application — backend, agent orchestration, tools, storage, and the browser UI — is written in **Rust** (the UI compiles to WebAssembly).

> Linux only (tested baseline: Linux Mint 22.3 / Ubuntu 24.04, x86_64). Windows/macOS are out of scope.

## What it does

- **Chat** — persistent conversations with streamed responses, safe Markdown, syntax-highlighted code, copy buttons, search/rename/delete.
- **Research** — pluggable search (SearXNG or built-in mock) with source links; SSRF-protected page fetching.
- **Coding** — file tools confined to a user-selected workspace, exact-match edits with unified diffs and automatic checkpoints, optional approval-gated command execution (no shell), loopback-only website previews.
- **File checkpoints & recovery** — every agent write/edit/delete snapshots the previous content; the Tasks panel lists them per conversation (paginated) and one confirmed click restores a file — the current content is snapshotted first, so restores are reversible too.
- **Browser** — isolated Chromium instance (separate profile, never yours) driven over the DevTools Protocol from Rust; login/CAPTCHA handoff to the human.
- **Agent mode** — plan-first autonomous runs combining the research, coding, and browser toolsets (subject to your tool toggles), with routine/job authoring tools that can only ever create **drafts** — a human activates them.
- **Jobs & routines** — durable, versioned instruction templates (routines) pinned to cron-scheduled or manual jobs with per-job workspaces, budgets, and a circuit breaker; each job keeps its own conversation transcript. **Schedules fire only while Vortex is running** (no OS timers in v1).
- **Multi-agent** — the coordinator delegates self-contained subtasks to sub-agents; **6 run concurrently by default** (configurable 1–16, enforced server-side by a scheduler, not by prompts), nested agents share the same cap, and results are tracked in the Tasks panel.
- **Approvals** — consequential actions (deletions, sensitive commands, purchases, publishing, …) always pause for explicit human approval; sub-agents cannot self-approve. Job runs **park and notify** (approval inbox) instead of timing out.

## Requirements (tested baseline: Linux Mint 22.3 / Ubuntu 24.04)

- Rust **1.89.0** (pinned by `rust-toolchain.toml`; rustup installs it automatically)
- C compiler (cc) — for the bundled SQLite
- `libsqlite3-dev`, `pkg-config`, `build-essential` — `sudo apt install build-essential pkg-config libsqlite3-dev` (Debian/Ubuntu)
- **Optional:** Chromium for browser tools (`sudo apt install chromium`), a SearXNG instance for research (a local one can be started with Docker — see [Search setup](#search-setup-research-mode))
- **Optional, UI builds only:** `cargo install trunk --version 0.21.14 --locked` (Trunk 0.22+ needs a newer Rust than the pinned 1.89.0) and a matching `cargo install wasm-bindgen-cli --version <version in Cargo.lock>` (currently 0.2.129). Not required to run a release binary that already embeds the UI.

No Python and no Node.js are used or required by the assistant itself.

## Setup

**A real OpenRouter API key is required** — the assistant cannot function
without one. Create an account at <https://openrouter.ai/keys>, generate a
key, and put it in `.env` as shown below.

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

> **Fresh clone?** The built UI assets (`crates/vortex-server/ui-dist/`) are
> not tracked by Git. Until you run `scripts/build-ui.sh` (included in
> `build-release.sh`), the server serves a placeholder page explaining this.
> For a dev workflow, see `docs/DEVELOPER_GUIDE.md` §3.

### Offline test harness (not a way to use the assistant)

```bash
VORTEX_LLM=mock ./target/release/vortex-server
```

runs against a built-in mock provider (no network, no key). It exists solely
to exercise the request/tool/SSE plumbing offline for development and tests:
`VORTEX_MOCK_SCRIPT=echo` echoes your message; `VORTEX_MOCK_SCRIPT=tools`
performs a scripted web_search tool round-trip. The mock produces canned
text with no model behind it — it does **not** provide real chat, research,
or coding functionality and is **not** a substitute for an API key.

## Storage locations (XDG)

| Purpose | Default | Override |
| --- | --- | --- |
| Config (`config.toml`) | `$XDG_CONFIG_HOME/vortex/` (fallback `~/.config/vortex/`) | `VORTEX_CONFIG_DIR` |
| Data (SQLite DB, trash) | `$XDG_DATA_HOME/vortex/` (fallback `~/.local/share/vortex/`) | `VORTEX_DATA_DIR` |
| Cache (isolated browser profile) | `$XDG_CACHE_HOME/vortex/` (fallback `~/.cache/vortex/`) | `VORTEX_CACHE_DIR` |

Precedence: `VORTEX_*_DIR` (must be absolute) > absolute `XDG_*_HOME` >
`$HOME` defaults (relative `XDG_*_HOME` values are ignored per the XDG spec).
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

OpenRouter does **not** search the web; Vortex integrates search itself via a
SearXNG instance you control. `deploy/searxng/` ships an opt-in Compose setup
(pinned official image, JSON output enabled, published on loopback only).

**Prerequisites:** Docker daemon access for your user (`docker compose
version` works), and port `127.0.0.1:8888` free.

```bash
# from the repository root; create the untracked secret file once
(umask 077; printf 'SEARXNG_SECRET=%s\n' "$(openssl rand -hex 32)" > deploy/searxng/.env)

# start
docker compose --env-file deploy/searxng/.env -f deploy/searxng/compose.yaml up -d

# test: must return JSON with real result URLs
curl -s 'http://127.0.0.1:8888/search?q=rust+programming+language&format=json' | head -c 400

# stop (config is a bind mount; nothing else persists, no volumes to delete)
docker compose --env-file deploy/searxng/.env -f deploy/searxng/compose.yaml down
```

In Vortex, open **Settings → Tools & permissions**, enable **Web research**,
enter `http://127.0.0.1:8888` as the **SearXNG URL**, and save. Click **Test search**
to check the saved provider without calling a model. For a sourced answer,
select **Research** mode and use a model that supports tool calling.

Common failures:

- **HTTP 403 on `format=json`** — JSON output is disabled on the instance.
  The shipped `deploy/searxng/config/settings.yml` sets
  `search.formats: [html, json]`; for other instances add it to their
  `settings.yml` and restart them.
- **Connection refused** — the container is not running or is published on a
  different port. Use the Compose command above with `ps` or `logs searxng`
  instead of `up -d`.
- **`SEARXNG_SECRET` error on `up`** — you skipped the `.env` step above.
- **Connected but zero results / engines timing out** — upstream search
  engines rate-limit or block some networks. Check
  `unresponsive_engines` in the JSON response and the container logs;
  retry later or enable different engines in `settings.yml`. This is an
  upstream/network problem, not a Vortex one.

Security notes: the instance is private and single-user (rate limiter off) and
must stay loopback-published — do not change the port mapping to expose it to
the LAN. The Compose setup never sees Vortex's `.env` or OpenRouter key.

## Sub-agents (six concurrent by default)

`Settings → Agents` exposes the concurrency limit (default **6**, ceiling 16),
run timeout, iteration/token budgets, and optional recursive delegation
(disabled by default). The cap is enforced server-side by a scheduler
semaphore — the 7th sub-agent waits in the queue; nested agents count against
the same global limit; the coordinator itself never consumes a slot.
Sub-agents inherit at most the coordinator's tool set and cannot widen it.

## Jobs & routines (autonomous work)

The **Jobs** tab schedules durable, unattended work. A **routine** is a
versioned instruction template; a **job** pins one routine version plus a cron
schedule (or manual-only), a workspace, and guardrails. Each execution is an
**occurrence** with its own run, transcript, and outcome.

- **Schedules fire only while Vortex is running.** The scheduler is an
  in-process loop, not an OS timer — there is no systemd/cron integration in
  v1. Missed ticks are handled at startup by a per-job policy:
  `run_once_if_missed` (default, one catch-up run for the most recent missed
  tick) or `skip`.
- **Per-job workspace.** A job either declares a workspace or gets a scratch
  directory at `<data dir>/jobs/<job id>/workspace`. The same confinement,
  write mutex, and checkpoints as interactive coding apply. This is **not** an
  OS sandbox (see `docs/SECURITY.md`).
- **Conservative budgets + circuit breaker.** Each occurrence is capped by
  turns, tokens, and wall-clock time; each job has a daily occurrence cap and
  single-flight (an occurrence never overlaps itself). After **3 consecutive
  failed/parked-expired occurrences** the job is auto-paused (`circuit_broken`)
  until a human resumes it. All caps are visible and editable per job.
- **Park & notify approvals.** Consequential actions in a job run park
  indefinitely in `waiting_approval` and surface in the approval **inbox**
  (Jobs tab) for a human decision — they never auto-deny on a timeout and never
  self-approve.
- **Draft-only agent authoring, human activation.** Agents can draft routines
  and propose schedules, but everything lands as a **draft**; only a human
  endpoint activates it. Job-origin runs never receive authoring tools, so a
  prompt injection cannot schedule itself. Three starter templates (research
  digest, repository health check, inbox triage) are seeded as drafts on first
  run.

See `docs/DEVELOPER_GUIDE.md` §9 (Jobs & routines API) and §11 (lifecycle) and
`docs/SECURITY.md` (jobs threat model) for the full contract.

## Architecture & security

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the crate layout, data
flow, and the agent loop; see [`docs/SECURITY.md`](docs/SECURITY.md) for the
threat model, permission model, and known limitations.

## Documentation

- [`docs/DEVELOPER_GUIDE.md`](docs/DEVELOPER_GUIDE.md) — full developer
  guide: setup, running locally, component walkthroughs, HTTP API reference,
  data model, agent-loop semantics, testing.
- [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) — confirmed issues and
  possible concerns from the latest source analysis, with file:line evidence.
- [`AGENTS.md`](AGENTS.md) — orientation for AI agents working in this repo
  (commands, toolchain pins, invariants, gotchas).

## Development

```bash
cargo test                       # 414 tests (all offline: mocks + local test servers; +2 #[ignore]d live-provider smokes)
cargo fmt --all -- --check
cargo clippy --all-targets       # clean
scripts/build-ui.sh              # WASM UI only (debug server reads ui-dist/ from disk)
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

MIT — see `LICENSE`.

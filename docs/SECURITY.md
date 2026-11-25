# Security notes

## Trust boundaries

- **The model is not trusted.** Every capability decision is made by the
  server: the tool allow-list per run, typed argument validation, risk
  classification, and approval gates. Prompts instruct the model, but nothing
  relies on instructions for enforcement.
- **File/web/tool content is untrusted data.** The system prompt marks it as
  such; more importantly, prompt injection cannot widen permissions, approve
  actions, escape the workspace, or reach the loopback network.

## Network posture

- Binds `127.0.0.1` by default and refuses to start on a non-loopback address
  without an explicit `[server.lan] enabled = true` in config.toml.
- **DNS rebinding:** the `Host` header must be `127.0.0.1`/`localhost`/`::1`
  unless LAN mode is on (verified: a spoofed Host gets HTTP 403).
- **CSRF:** writes require a same-loopback `Origin` when present and JSON
  content types; EventSource is read-only (GET) so it cannot mutate state.
- **LAN mode (opt-in):** generates a 32-byte token into config.toml
  (0600 perms) and requires `Authorization: Bearer <token>` (or `?token=`
  for SSE) on every request. A warning is logged at startup.
- **SSRF:** model-chosen URLs may only use http/https; loopback, private
  (RFC1918), link-local, and unique-local addresses are refused — including
  IPv4-mapped IPv6 forms, after DNS resolution, and on every redirect hop.
  Connections are **IP-pinned**: DNS is resolved once per hop, every
  resolved address is validated, and the request connects to a validated
  address via a per-request resolver override (TLS SNI/certificate checks
  still use the hostname), closing the TOCTOU DNS-rebinding window between
  validation and connect. Only the user-configured search provider may live
  on the local network (it keeps a shared unpinned client by design).

## Secrets

- `OPENROUTER_API_KEY` is read from the environment or config.toml only. It
  is never sent to the browser, never logged (headers/bodies are never
  logged at all), and never included in error messages.
- `.env` should be `chmod 600`; config.toml permissions are auto-tightened
  if they are looser.
- History, settings, and run metadata are stored locally in SQLite; no
  telemetry, no third-party calls except the configured LLM/search providers
  needed for the requested task.

## Permissions & approvals

Profiles (`restricted` / `standard` / `trusted`) only reduce tool availability.
Some actions are unconditional:

- `run_command` requires approval for anything sensitive (package managers,
  git push, systemctl, …) or not on the user's allowlist — in every profile,
  for any agent depth.
- `delete_path` requires approval; deletions move files to a local trash
  directory instead of unlinking.
- `request_approval` lets the model pause for any consequential web or
  external action; only a human can approve, and the exact payload is shown
  first.

## Jobs & routines (autonomous work)

Unattended runs raise the stakes: a prompt injection that can schedule itself
becomes a persistent threat, and a crash must never replay a half-executed
consequential action. The defenses are server-enforced, never prompt-based
(proven by the offline failure-injection matrix in
`crates/vortex-server/src/job_hardening_tests.rs`):

- **Injection self-propagation is blocked.** Agent authoring tools
  (`save_routine`, `propose_schedule`) always land **drafts**: no parameter
  list declares status/activation fields, and the schema filter strips
  injected keys (`status`, `activate`, …) before execution. Activation is a
  distinct **human-only** server endpoint (`POST /api/routines/{id}/activate`,
  `POST /api/jobs/{id}/activate`); no model-reachable code path performs an
  activation transition. Job-origin runs — and their delegates, which inherit
  the origin — never receive authoring tools at all (allow-list stripped at
  admission; attempted calls are refused by the registry, and the tools are
  absent from the provider schemas). A non-existent tool such as
  `activate_routine` is refused outright.
- **Approvals stay human-only, with park & notify.** Job runs use
  `ParkUntilDecided`: consequential actions park in `waiting_approval`
  indefinitely (no auto-deny timeout) and surface in the inbox
  (`GET /api/jobs/inbox`); the only resolution path is the existing
  `POST /api/approvals/{id}/decide`. A single-owner protocol guarantees
  exactly one of decide/timeout/cancel writes the outcome, so a late decision
  can never overwrite an expired approval or vice versa. Shutdown cancels
  parked runs and expires their approvals; on restart, parked-expired
  occurrences count toward the circuit breaker and are **never**
  auto-continued — a human must re-decide.
- **Double execution is guarded.** `UNIQUE(job_id, scheduled_for)` makes
  concurrent inserts (catch-up + live tick, manual + scheduled) resolve to
  exactly one occurrence; single-flight per job means an occurrence never
  overlaps itself; the supervisor is started exactly once. Clock jumps
  backward never re-fire an already-fired tick.
- **Workspace confinement applies per job.** Every job declares a workspace
  (default: a per-job scratch dir `<data dir>/jobs/<job id>/workspace`); the
  same canonicalized-root confinement, `..`/symlink-escape rejection, write
  mutex with cross-run ownership, and checkpoints apply to job runs exactly
  as to interactive runs.
- **Budgets and the circuit breaker bound cost runaway.** Per-occurrence
  turn/token/wall-clock caps, a per-job daily occurrence cap, and a breaker
  that pauses a job after 3 consecutive failed/parked-expired occurrences
  (human resume resets the counter). User cancellation and human dismissal of
  a review never count toward the breaker.
- **Interrupted-run honesty is preserved.** Startup sweeps mark live runs and
  occurrences `interrupted` and expire their pending approvals. Continuation
  happens only at step boundaries, seeded from the persisted transcript +
  plan; an unconfirmed consequential step (a tool-call row still `running`,
  or an unknown tool) is flagged `needs_human_review` and never auto-rerun —
  a human continues or dismisses it via `resolve-review`. Nothing
  consequential is ever replayed automatically.
- **Network posture is unchanged for job runs**: the same loopback-only
  server, request guard, and SSRF/IP-pinning defenses apply to every
  model-chosen URL a job fetches.

## Workspace & processes

- File tools are refused until a workspace root is selected; paths are then
  confined (see ARCHITECTURE.md).
- Commands run through argv arrays (no shell), binaries resolved from PATH
  only, in their own process groups, with output caps, timeouts (max 600s),
  and group-kill on timeout/cancel/shutdown — no orphaned children
  (verified by tests using pgrep).
- Forbidden patterns (sudo/su, `rm -rf /`, disk writers, power management,
  …) can never be run, even allowlisted or approved.
- A selected working directory is **not** a sandbox: commands run with your
  normal user rights inside the workspace. Isolation beyond process-group
  hygiene (containers, namespaces) is not implemented; this is stated plainly
  rather than implied. **Job workspaces are no different** — a job's scratch
  dir confines file tools, but any command a job runs (when command execution
  is enabled and approved) still runs with your user rights. There is no OS
  sandboxing around jobs in v1.

## Browser automation

- Uses a separate Chromium profile under the XDG cache — never your personal
  browser profile; no access to saved passwords or sessions.
- Only http/https navigation; `file://` and `chrome://` are refused.
- Typing into password fields or CAPTCHA widgets is refused; page reads
  report login-form/CAPTCHA presence so the agent hands off to the human.
- The browser process group is killed at shutdown.

## Reporting / disclosure scope

This is a personal, localhost application. If you find a vulnerability,
please open a private issue; do not publish against instances you do not own.

## Known limitations (honest list)

- No OS sandboxing around spawned commands/previews beyond process-group
  hygiene (see above) — including job-origin runs.
- Jobs fire only while Vortex is running (in-process scheduler, no OS
  timers); schedules use the server's local timezone only in v1. A parked
  job approval waits for a human indefinitely — until then, that occurrence
  makes no progress (by design).
- Live OpenRouter endpoints were exercised against a protocol-faithful local
  mock; behavior differences of real providers (e.g., unusual SSE framing)
  may surface and would be surfaced as clear errors, not silent failures.
- The LAN mode token is compared in constant time (byte-wise XOR fold;
  length mismatch is inherently revealed); it is still not a substitute for
  TLS, which LAN mode does not provide.
- Wayland/X11 desktop automation is not implemented.
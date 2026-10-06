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
  after DNS resolution and on every redirect hop. Only the user-configured
  search provider may live on the local network. Residual risk: TOCTOU DNS
  rebinding between check and connect (standard for proxy-less fetchers;
  documented, not hidden).

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
  rather than implied.

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
  hygiene (see above).
- Live OpenRouter endpoints were exercised against a protocol-faithful local
  mock; behavior differences of real providers (e.g., unusual SSE framing)
  may surface and would be surfaced as clear errors, not silent failures.
- The LAN mode token is compared with constant-time-adjacent string
  comparison; for a home-LAN threat model this is acceptable, but it is not a
  substitute for TLS, which LAN mode does not provide.
- Wayland/X11 desktop automation is not implemented.
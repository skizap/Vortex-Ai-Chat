//! `start_preview` tool (static + process modes).

use crate::errors::{Risk, ToolError};
use crate::preview::PreviewRegistry;
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct StartPreviewTool {
    pub registry: std::sync::Arc<PreviewRegistry>,
}

const START_DEF: ToolDef = ToolDef {
    name: "start_preview",
    description: "Start a local preview for a website project in the workspace. Static projects are served by the built-in loopback server; if `cmd` is given, that dev-server command is run instead (requires the project runtime and command execution to be enabled).",
    params: &[
        ParamSpec { name: "path", ty: ParamType::Str, required: true, description: "Project directory inside the workspace." },
        ParamSpec { name: "cmd", ty: ParamType::StrArray, required: false, description: "Optional dev-server argv (e.g. [\"npm\",\"run\",\"dev\"]). If omitted, the directory is served statically." },
    ],
};

#[async_trait]
impl Tool for StartPreviewTool {
    fn def(&self) -> &'static ToolDef {
        &START_DEF
    }
    fn risk(&self) -> Risk {
        Risk::Medium
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.previews {
            return Err(ToolError::Unavailable(
                "previews are disabled in Settings (Tools).".into(),
            ));
        }
        if ctx.workspace.is_none() {
            return Err(ToolError::Unavailable(
                "no workspace is selected. Pick one in the UI sidebar first.".into(),
            ));
        }
        Ok(())
    }
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let ws = ctx.require_workspace()?;
        let rel = args["path"].as_str().expect("validated");
        let root = ws.resolve(rel)?;
        if !root.is_dir() {
            return Err(ToolError::InvalidArgs(
                "path".into(),
                "project directory not found".into(),
            ));
        }
        let cmd: Option<Vec<String>> = args.get("cmd").and_then(|c| c.as_array()).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        });
        match cmd {
            None => {
                let url = self.registry.start_static(&ctx.run_id, root)?;
                Ok(ToolResult {
                    summary: format!("static preview at {url}"),
                    text: format!(
                        "Preview running at {url}\nIt is bound to 127.0.0.1 and is served by \
                         Vortex. Open the URL in your browser to review the site."
                    ),
                    diff: None,
                })
            }
            Some(cmd) => run_process_preview(self.registry.clone(), ctx, root, cmd).await,
        }
    }
}

/// Dev-server previews are command execution: approval is required, and the
/// child is spawned in its own process group so cancellation can clean it up.
async fn run_process_preview(
    registry: std::sync::Arc<PreviewRegistry>,
    ctx: &ToolContext,
    root: PathBuf,
    cmd: Vec<String>,
) -> Result<ToolResult, ToolError> {
    use crate::tools_command::{kill_process_group, resolve_binary_pub};
    if cmd.is_empty() {
        return Err(ToolError::InvalidArgs(
            "cmd".into(),
            "argv must not be empty".into(),
        ));
    }
    if !ctx.settings.tools.commands {
        return Err(ToolError::Unavailable(
            "process-based previews require command execution, which is disabled in Settings \
             (Tools). Static previews still work without it."
                .into(),
        ));
    }
    let payload = serde_json::to_string_pretty(&serde_json::json!({
        "cmd": cmd,
        "cwd": root.display().to_string(),
    }))
    .unwrap_or_default();
    let approved = ctx
        .approvals
        .request(&ctx.run_id, "Approve dev-server preview", &payload)
        .await?;
    if !approved {
        return Err(ToolError::Denied(
            "the user declined to start this dev server".into(),
        ));
    }
    let binary = resolve_binary_pub(&cmd[0])?;
    // Snapshot the machine's listening ports *before* spawning the child: a
    // printed port is only trusted if it appears as a NEW listener afterwards
    // (port-ownership check — see `wait_for_port`).
    let ports_before = listening_ports();
    let mut proc = tokio::process::Command::new(&binary);
    proc.args(&cmd[1..])
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    proc.process_group(0);
    let mut child = proc
        .spawn()
        .map_err(|e| ToolError::Execution(format!("starting dev server: {e}")))?;
    let pid = child
        .id()
        .ok_or_else(|| ToolError::Execution("dev server exited immediately after launch".into()))?;
    let port = wait_for_port(&mut child, ports_before, PORT_DETECT_TIMEOUT).await;
    let Some(port) = port else {
        kill_process_group(pid as i32).await;
        return Err(ToolError::Execution(
            "could not detect the dev server port. Ensure the command binds to 127.0.0.1 and \
             prints its URL/port."
                .into(),
        ));
    };
    let url = format!("http://127.0.0.1:{port}");
    // The registry takes ownership of the child handle: dropping it here
    // would kill the server (kill_on_drop), which used to happen the moment
    // this function returned.
    registry.attach_child(
        &ctx.run_id,
        root,
        url.clone(),
        pid,
        child,
        ctx.cancel.clone(),
    );
    Ok(ToolResult {
        summary: format!("dev-server preview at {url}"),
        text: format!(
            "Dev server running at {url} (bound to loopback). It will be stopped when this \
             task ends or is stopped."
        ),
        diff: None,
    })
}

/// Dev-server detection window (parameterized so tests can shorten it).
const PORT_DETECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Ports that currently have a socket in LISTEN state, read from the kernel
/// (`/proc/net/tcp` + `/proc/net/tcp6`; Linux-only, matching the project's
/// Linux-only invariant). Field layout: `sl local_address rem_address st …`
/// — the local address is `HEXIP:HEXPORT` and state `0A` means LISTEN.
fn listening_ports() -> std::collections::HashSet<u16> {
    let mut ports = std::collections::HashSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let _slot = fields.next();
            let Some(local) = fields.next() else {
                continue;
            };
            let _remote = fields.next();
            let Some(state) = fields.next() else {
                continue;
            };
            if !state.eq_ignore_ascii_case("0A") {
                continue; // not LISTEN
            }
            // IPv6 addresses contain colons, so split off the *last* one.
            let Some(hex_port) = local.rsplit_once(':').map(|(_, p)| p) else {
                continue;
            };
            if let Ok(port) = u16::from_str_radix(hex_port, 16) {
                if port != 0 {
                    ports.insert(port);
                }
            }
        }
    }
    ports
}

/// Read the child's stdout looking for its bound port, bounded by `timeout`.
///
/// A regex-matched port is only accepted once it shows up as a NEW listening
/// port (present now, absent from the pre-spawn `before` snapshot): a dev
/// server printing an unrelated port must not attach the preview to somebody
/// else's service. If nothing parseable is printed by the deadline but
/// exactly one new listening port appeared after the spawn, that port is
/// used. The Vortex server's own port (8417) is always excluded.
async fn wait_for_port(
    child: &mut tokio::process::Child,
    before: std::collections::HashSet<u16>,
    timeout: std::time::Duration,
) -> Option<u16> {
    use tokio::io::BufReader;
    let stdout = child.stdout.take()?;
    wait_for_port_on(BufReader::new(stdout), before, timeout).await
}

/// Reader-based core of [`wait_for_port`] (tests drive it without a child).
async fn wait_for_port_on<R>(
    reader: R,
    before: std::collections::HashSet<u16>,
    timeout: std::time::Duration,
) -> Option<u16>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut reader = reader;
    let mut line = String::new();
    let deadline = std::time::Instant::now() + timeout;
    let re = regex::Regex::new(r"(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d{2,5})|port\s+(\d{2,5})")
        .ok()?;
    // Ports the child printed, in order; confirmed only via listener ownership.
    let mut candidates: Vec<u16> = Vec::new();
    let mut saw_match = false;
    let mut eof = false;
    loop {
        if !eof {
            line.clear();
            match tokio::time::timeout(
                std::time::Duration::from_millis(250),
                reader.read_line(&mut line),
            )
            .await
            {
                Ok(Ok(0)) => eof = true,
                Ok(Ok(_)) => {
                    let lower = line.to_lowercase();
                    if let Some(caps) = re.captures(&lower) {
                        if let Some(port) = caps
                            .get(1)
                            .or_else(|| caps.get(2))
                            .and_then(|m| m.as_str().parse::<u16>().ok())
                        {
                            if port != 8417 && !candidates.contains(&port) {
                                saw_match = true;
                                candidates.push(port);
                            }
                        }
                    }
                }
                Ok(Err(_)) => eof = true,
                Err(_) => {} // read timed out: keep polling until the deadline
            }
        }
        // Confirm printed candidates against actual listener ownership: the
        // port must be listening now and must NOT have been listening before
        // the child was spawned.
        if !candidates.is_empty() {
            let now = listening_ports();
            if let Some(port) = candidates
                .iter()
                .copied()
                .find(|p| now.contains(p) && !before.contains(p))
            {
                return Some(port);
            }
        }
        if eof || std::time::Instant::now() >= deadline {
            // Fallback: nothing parseable was printed, but exactly one new
            // listening port appeared since the spawn — that is the server.
            if !saw_match {
                let now = listening_ports();
                let new: Vec<u16> = now
                    .difference(&before)
                    .copied()
                    .filter(|p| *p != 8417)
                    .collect();
                if new.len() == 1 {
                    return Some(new[0]);
                }
            }
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn listening_ports_reports_a_new_listener() {
        let before = listening_ports();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let now = listening_ports();
        assert!(
            now.contains(&port) && !before.contains(&port),
            "kernel listener table must show the new port {port}"
        );
    }

    /// A printed port is accepted once it shows up as a new listener.
    #[tokio::test]
    async fn detects_printed_port_only_when_actually_listening() {
        let before = listening_ports();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let output = format!("  VITE v5 ready — local: http://localhost:{port}/\n");
        let reader = tokio::io::BufReader::new(output.as_bytes());
        let detected = wait_for_port_on(reader, before, Duration::from_secs(5)).await;
        assert_eq!(detected, Some(port));
    }

    /// Negative case: the child prints a port it does NOT listen on — the
    /// preview must not attach to whatever else owns (or later grabs) it.
    #[tokio::test]
    async fn rejects_printed_port_that_is_not_listening() {
        // Grab a port and release it again so it is (almost certainly) free.
        let unused = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let before = listening_ports();
        assert!(!before.contains(&unused), "test port should be free");
        let output = format!("Server running at http://localhost:{unused}\n");
        let reader = tokio::io::BufReader::new(output.as_bytes());
        let detected = wait_for_port_on(reader, before, Duration::from_millis(500)).await;
        assert_eq!(
            detected, None,
            "a printed port with no listener behind it must be rejected"
        );
    }

    /// Fallback: a server that prints nothing parseable is still detected
    /// when exactly one new listening port appeared after the snapshot.
    #[tokio::test]
    async fn falls_back_to_the_single_new_listener() {
        // Other tests in this binary bind ephemeral ports concurrently, so
        // retry a few times if an unrelated listener polluted the window.
        for _ in 0..5 {
            let before = listening_ports();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let reader = tokio::io::BufReader::new("starting up...\n".as_bytes());
            let detected = wait_for_port_on(reader, before, Duration::from_millis(300)).await;
            if detected == Some(port) {
                return; // success
            }
            assert_eq!(
                detected, None,
                "fallback must never return a foreign port (got {detected:?}, expected {port})"
            );
        }
        panic!("fallback detection never saw a clean single-listener window");
    }
}

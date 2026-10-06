//! `run_command`: structured argv execution inside the workspace.
//!
//! Policy (all enforced server-side):
//! - Disabled entirely unless the user enabled commands in Settings.
//! - No shell: argv is a JSON array; shell metacharacters have no meaning.
//! - Binary must be a bare name resolved via PATH (no `./x`, no paths).
//! - FORBIDDEN patterns (sudo, su, rm -rf /, mkfs, dd to devices, ...) always
//!   fail, even if allowlisted or approved.
//! - Commands not on the user's allowlist are Consequential: they require
//!   explicit human approval every time, regardless of profile, including
//!   when a sub-agent asks.
//! - Own process group; cancellation/timeout kills the whole group.

use crate::errors::{Risk, ToolError};
use crate::registry::{Tool, ToolContext, ToolResult};
use crate::schema::{ParamSpec, ParamType, ToolDef};
use async_trait::async_trait;
use std::time::Duration;

pub struct RunCommandTool;

const CMD_DEF: ToolDef = ToolDef {
    name: "run_command",
    description: "Run an executable with arguments inside the workspace (no shell). Requires user approval unless the command is allowlisted.",
    params: &[
        ParamSpec { name: "cmd", ty: ParamType::StrArray, required: true, description: "Argv array, e.g. [\"cargo\", \"test\"]. First element is the binary name (resolved via PATH)." },
        ParamSpec { name: "cwd", ty: ParamType::Str, required: false, description: "Working directory inside the workspace (defaults to the root)." },
        ParamSpec { name: "timeout_secs", ty: ParamType::Int, required: false, description: "Timeout in seconds (default 60, max 600)." },
    ],
};

/// Hard-forbidden patterns. Matched against the joined argv, lowercased.
const FORBIDDEN: &[&str] = &[
    "sudo",
    "su ",
    "doas ",
    " pkexec",
    "rm -rf /",
    "rm -fr /",
    "rm -rf /*",
    "mkfs",
    "dd if=",
    "dd of=/dev/",
    "fdisk",
    "parted",
    "wipefs",
    "shred /dev/",
    "shutdown",
    "reboot",
    "poweroff",
    " halt",
    "init 0",
    "init 6",
    "chmod -r 777 /",
    "chown -r ",
    "> /dev/sd",
    "mv / ",
];

/// Approval-gated patterns even when allowlisting would otherwise match.
const SENSITIVE: &[&str] = &[
    "apt ",
    "apt-get",
    " apt",
    "dpkg",
    "dnf",
    "yum",
    "pacman",
    "zypper",
    "snap",
    "flatpak",
    "pip install",
    "pip3 install",
    "npm install",
    "npm publish",
    "yarn install",
    "cargo install",
    "cargo publish",
    "git push",
    "git reset --hard",
    "make install",
    "systemctl",
    "mount ",
    "umount",
    "crontab",
    "useradd",
    "userdel",
    "chsh",
];

fn classify(cmd: &[String]) -> Result<Risk, ToolError> {
    let joined = format!(" {} ", cmd.join(" ")).to_lowercase();
    for pat in FORBIDDEN {
        if joined.contains(pat) {
            return Err(ToolError::NotPermitted(format!(
                "command matches the forbidden pattern '{pat}' and can never be run by the assistant; \
                 run it yourself in a terminal if it is really intended"
            )));
        }
    }
    let sensitive = SENSITIVE.iter().any(|p| joined.contains(p));
    Ok(if sensitive {
        Risk::Consequential
    } else {
        Risk::Medium
    })
}

/// Does the argv match a user allowlist prefix rule?
fn is_allowlisted(allowlist: &[Vec<String>], cmd: &[String]) -> bool {
    allowlist
        .iter()
        .any(|prefix| cmd.len() >= prefix.len() && cmd[..prefix.len()] == prefix[..])
}

/// Resolve a bare binary name through PATH.
fn resolve_binary(name: &str) -> Result<std::path::PathBuf, ToolError> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return Err(ToolError::InvalidArgs(
            "cmd[0]".into(),
            "binary must be a bare name resolved via PATH (no slashes)".into(),
        ));
    }
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.permissions().mode() & 0o111 != 0 {
                    return Ok(candidate);
                }
            }
        }
    }
    Err(ToolError::Execution(format!(
        "executable '{name}' was not found in PATH"
    )))
}

/// Public wrapper used by the preview tool.
pub fn resolve_binary_pub(name: &str) -> Result<std::path::PathBuf, ToolError> {
    resolve_binary(name)
}

/// Kill an entire process group with SIGTERM, then SIGKILL after a grace
/// period. Public so previews reuse it.
pub async fn kill_process_group(pid: i32) {
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if unsafe { libc::kill(-pid, 0) } != 0 {
            return; // group gone
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Shared spawn helper: runs an argv program with a new process group in the
/// given cwd, capturing combined output up to a cap, with a timeout.
/// Returns (exit status string, output). The process group is always
/// terminated when the future is cancelled or times out.
pub async fn run_command_scoped(
    argv: &[String],
    cwd: &std::path::Path,
    timeout: Duration,
    cancel: tokio_util::sync::CancellationToken,
    max_output: usize,
) -> Result<(String, String), ToolError> {
    use tokio::io::AsyncReadExt;
    let binary = resolve_binary(&argv[0])?;
    let mut cmd = tokio::process::Command::new(&binary);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawning {}: {e}", argv[0])))?;
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let mut out_buf: Vec<u8> = Vec::new();
    let mut err_buf: Vec<u8> = Vec::new();

    let status = tokio::select! {
        s = child.wait() => s,
        _ = cancel.cancelled() => {
            if let Some(pid) = pid {
                kill_process_group(pid as i32).await;
            }
            return Err(ToolError::Cancelled);
        }
        _ = tokio::time::sleep(timeout) => {
            if let Some(pid) = pid {
                kill_process_group(pid as i32).await;
            }
            return Err(ToolError::Timeout);
        }
    };
    // Drain remaining output after exit (bounded).
    let _ = stdout.read_to_end(&mut out_buf).await;
    let _ = stderr.read_to_end(&mut err_buf).await;
    if out_buf.len() > max_output {
        out_buf.truncate(max_output);
    }
    if err_buf.len() > max_output {
        err_buf.truncate(max_output);
    }
    let status = status.map_err(|e| ToolError::Execution(format!("waiting for process: {e}")))?;
    let mut output = String::new();
    if !out_buf.is_empty() {
        output.push_str(&String::from_utf8_lossy(&out_buf));
    }
    if !err_buf.is_empty() {
        if !output.is_empty() {
            output.push_str("\n--- stderr ---\n");
        }
        output.push_str(&String::from_utf8_lossy(&err_buf));
    }
    Ok((format!("exit status: {status}"), output))
}

#[async_trait]
impl Tool for RunCommandTool {
    fn def(&self) -> &'static ToolDef {
        &CMD_DEF
    }
    fn available(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        if !ctx.settings.tools.commands {
            return Err(ToolError::Unavailable(
                "command execution is disabled. Enable it in Settings → Tools (and understand \
                 the implications first; see docs/SECURITY.md)."
                    .into(),
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
        let cmd: Vec<String> = args["cmd"]
            .as_array()
            .expect("validated")
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if cmd.is_empty() {
            return Err(ToolError::InvalidArgs(
                "cmd".into(),
                "argv must not be empty".into(),
            ));
        }
        let risk = classify(&cmd)?;
        let allowlisted = is_allowlisted(&ctx.settings.command_allowlist, &cmd);
        if risk == Risk::Consequential || !allowlisted {
            // Human approval for anything sensitive or not explicitly
            // allowlisted — regardless of profile, including sub-agents.
            let payload = serde_json::to_string_pretty(&serde_json::json!({
                "cmd": cmd,
                "cwd": args.get("cwd").and_then(|c| c.as_str()).unwrap_or("."),
            }))
            .unwrap_or_default();
            let approved = ctx
                .approvals
                .request(&ctx.run_id, "Approve command execution", &payload)
                .await?;
            if !approved {
                return Err(ToolError::Denied(
                    "the user declined to run this command".into(),
                ));
            }
        }
        let cwd_rel = args.get("cwd").and_then(|c| c.as_str()).unwrap_or(".");
        let cwd = ws.resolve(cwd_rel)?;
        if !cwd.is_dir() {
            return Err(ToolError::InvalidArgs(
                "cwd".into(),
                "not a directory".into(),
            ));
        }
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|t| t.as_u64())
            .unwrap_or(60)
            .clamp(1, 600);
        let (status, output) = run_command_scoped(
            &cmd,
            &cwd,
            Duration::from_secs(timeout_secs),
            ctx.cancel.clone(),
            200_000,
        )
        .await?;
        let mut text = format!("{status}\n");
        text.push_str(&output);
        let summary = format!("{} ({})", cmd.join(" "), status);
        Ok(ToolResult {
            text,
            summary,
            diff: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_patterns_are_refused() {
        for cmd in [
            vec!["sudo", "ls"],
            vec!["rm", "-rf", "/"],
            vec!["dd", "if=x", "of=/dev/sda"],
            vec!["shutdown", "now"],
        ] {
            let cmd: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
            assert!(classify(&cmd).is_err(), "{cmd:?} should be forbidden");
        }
        assert_eq!(
            classify(&["cargo".to_string(), "test".to_string()]).unwrap(),
            Risk::Medium
        );
        assert_eq!(
            classify(&["apt".to_string(), "install".to_string(), "x".to_string()]).unwrap(),
            Risk::Consequential
        );
    }

    #[test]
    fn allowlist_prefix_matching() {
        let allow = vec![vec!["cargo".to_string(), "test".to_string()]];
        assert!(is_allowlisted(
            &allow,
            &["cargo".to_string(), "test".to_string()]
        ));
        assert!(!is_allowlisted(
            &allow,
            &["cargo".to_string(), "build".to_string()]
        ));
        assert!(is_allowlisted(
            &allow,
            &["cargo".to_string(), "test".to_string(), "--".to_string()]
        ));
    }

    #[tokio::test]
    async fn run_scoped_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        let argv = vec!["echo".to_string(), "hello".to_string()];
        let (status, output) = run_command_scoped(
            &argv,
            dir.path(),
            Duration::from_secs(5),
            tokio_util::sync::CancellationToken::new(),
            1000,
        )
        .await
        .unwrap();
        assert!(status.contains("exit status: 0"), "{status}");
        assert!(output.contains("hello"));
    }

    #[tokio::test]
    async fn disabled_by_default() {
        let (ctx, _rx, _c) =
            crate::registry::test_context(vortex_types::Settings::default(), &["run_command"]);
        assert!(RunCommandTool.available(&ctx).is_err());
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let argv = vec!["sleep".to_string(), "30".to_string()];
        let started = std::time::Instant::now();
        let res = run_command_scoped(
            &argv,
            dir.path(),
            Duration::from_secs(1),
            tokio_util::sync::CancellationToken::new(),
            1000,
        )
        .await;
        assert!(matches!(res.unwrap_err(), ToolError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(5));
        let out = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("sleep 30")
            .output()
            .expect("pgrep works");
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "orphaned sleep process found"
        );
    }

    #[tokio::test]
    async fn cancellation_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let argv = vec!["sleep".to_string(), "30".to_string()];
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            let dir = dir.path().to_path_buf();
            async move { run_command_scoped(&argv, &dir, Duration::from_secs(60), cancel, 1000).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        assert!(matches!(
            handle.await.unwrap().unwrap_err(),
            ToolError::Cancelled
        ));
        let out = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("sleep 30")
            .output()
            .expect("pgrep works");
        assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
    }
}

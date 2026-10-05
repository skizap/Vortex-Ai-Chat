//! Chromium browser manager: launches an isolated Chromium/Chrome instance
//! (separate profile, never the user's personal browser profile) and owns
//! its lifecycle. The instance is killed via its process group on shutdown.

use crate::cdp::{CdpConnection, CdpPage};
use crate::errors::ToolError;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct BrowserManager {
    binary: Option<PathBuf>,
    profile_dir: PathBuf,
    headless: bool,
    inner: Mutex<Option<BrowserProcess>>,
    pages: Mutex<std::collections::HashMap<String, CdpPage>>,
}

struct BrowserProcess {
    conn: Arc<CdpConnection>,
    pid: u32,
}

/// Common binary names checked in PATH, in order of preference.
const CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
];

/// Detect the browser binary at startup so capabilities are honest.
pub fn detect_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("VORTEX_CHROME_PATH") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for name in CANDIDATES {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

impl BrowserManager {
    pub fn new(profile_dir: PathBuf, headless: bool) -> Self {
        let binary = detect_binary();
        Self {
            binary,
            profile_dir,
            headless,
            inner: Mutex::new(None),
            pages: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn is_available(&self) -> bool {
        self.binary.is_some()
    }

    /// Setup instructions surfaced in the UI when Chromium is missing.
    pub fn unavailable_hint() -> &'static str {
        "Install Chromium (e.g. `sudo apt install chromium` on Debian/Ubuntu or your \
         distribution's equivalent), or set VORTEX_CHROME_PATH to a Chromium/Chrome binary. \
         Then restart Vortex."
    }
}

impl BrowserManager {
    async fn ensure_started(&self) -> Result<Arc<CdpConnection>, ToolError> {
        let mut guard = self.inner.lock().await;
        if let Some(existing) = guard.as_ref() {
            return Ok(existing.conn.clone());
        }
        let binary = self.binary.as_ref().ok_or_else(|| {
            ToolError::Unavailable(format!(
                "browser automation is unavailable: no Chromium/Chrome binary was found. {}",
                Self::unavailable_hint()
            ))
        })?;
        let _ = std::fs::create_dir_all(&self.profile_dir);
        let mut cmd = tokio::process::Command::new(binary);
        cmd.arg("--remote-debugging-port=0")
            .arg("--user-data-dir")
            .arg(&self.profile_dir)
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-extensions")
            .arg("--disable-component-update")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if self.headless {
            cmd.arg("--headless=new");
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("launching Chromium: {e}")))?;
        let pid = child.id();
        let stderr = child.stderr.take().expect("stderr piped");
        let ws_url = read_devtools_url(stderr).await.ok_or_else(|| {
            ToolError::Execution(
                "Chromium did not report a DevTools websocket. The installed browser may be \
                 outdated; try a recent Chromium."
                    .into(),
            )
        })?;
        let conn = Arc::new(CdpConnection::connect(&ws_url).await?);
        *guard = Some(BrowserProcess {
            conn: conn.clone(),
            pid: pid.unwrap(),
        });
        Ok(conn)
    }

    /// Get or create the page tab for a run.
    pub async fn page_for(&self, run_id: &str) -> Result<(Arc<CdpConnection>, CdpPage), ToolError> {
        let conn = self.ensure_started().await?;
        let mut pages = self.pages.lock().await;
        if let Some(page) = pages.get(run_id) {
            return Ok((conn, page.clone()));
        }
        let page = CdpPage::create(&conn, "about:blank").await?;
        pages.insert(run_id.to_string(), page.clone());
        Ok((conn, page))
    }

    /// Close and forget a run's page (called when the run ends).
    pub async fn cleanup_run(&self, run_id: &str) {
        let page = self.pages.lock().await.remove(run_id);
        if let (Some(page), Some(process)) = (page, self.inner.lock().await.as_ref()) {
            page.close(&process.conn).await;
        }
    }

    /// Kill the browser process group (server shutdown).
    pub async fn shutdown(&self) {
        if let Some(process) = self.inner.lock().await.as_ref() {
            crate::tools_command::kill_process_group(process.pid as i32).await;
        }
        self.pages.lock().await.clear();
    }
}

/// Chromium prints `DevTools listening on ws://127.0.0.1:PORT/devtools/...`
/// on stderr; wait for it, bounded to 15 seconds.
async fn read_devtools_url(stderr: tokio::process::ChildStderr) -> Option<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if std::time::Instant::now() > deadline {
            return None;
        }
        line.clear();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_line(&mut line),
        )
        .await
        .ok()?
        .ok()?;
        if n == 0 {
            return None;
        }
        if let Some(idx) = line.find("ws://") {
            let url = line[idx..].trim().to_string();
            if url.starts_with("ws://127.0.0.1") || url.starts_with("ws://[::1]") {
                return Some(url);
            }
        }
    }
}

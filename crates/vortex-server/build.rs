//! Ensure `ui-dist/` exists at compile time so `rust-embed` has a folder to
//! embed. The real UI is built into it by `scripts/build-ui.sh` (Trunk).

use std::path::Path;

fn main() {
    let dir = Path::new("ui-dist");
    if !dir.exists() {
        let _ = std::fs::create_dir_all(dir);
    }
    let index = dir.join("index.html");
    if !index.exists() {
        let placeholder = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Vortex-Ai-Chat</title></head>
<body><h1>Vortex-Ai-Chat</h1><p>UI not built — run scripts/build-ui.sh</p></body></html>"#;
        let _ = std::fs::write(index, placeholder);
    }
    println!("cargo:rerun-if-changed=ui-dist/index.html");
}

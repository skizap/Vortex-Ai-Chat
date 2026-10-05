//! HTML → text extraction (dependency-free) and private-IP policy helpers.

use std::net::IpAddr;

use crate::errors::ToolError;

/// Minimal HTML → (title, plain text) extraction.
/// Strips script/style content, converts structural tags to newlines,
/// decodes common entities, collapses whitespace, and caps size.
pub fn strip_html(html: &str) -> (Option<String>, String) {
    let lower = html.to_ascii_lowercase();
    let title = lower.find("<title>").and_then(|s| {
        lower[s + 7..]
            .find("</title>")
            .map(|e| html[s + 7..s + 7 + e].trim().to_string())
    });

    let mut out = String::with_capacity(html.len() / 2);
    let mut rest: &str = html;
    loop {
        match rest.find('<') {
            None => {
                out.push_str(rest);
                break;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                let after = &rest[i..];
                let Some(end) = after.find('>') else { break };
                let tag = after[1..end].trim().to_ascii_lowercase();
                let skip_content = tag == "script" || tag == "style" || tag == "noscript";
                if skip_content {
                    let close = format!("</{tag}");
                    if let Some(c) = rest[end + i + 1..].to_ascii_lowercase().find(&close) {
                        let close_len = rest[end + i + 1 + c..]
                            .find('>')
                            .map(|g| end + i + 1 + c + g + 1)
                            .unwrap_or(rest.len());
                        rest = &rest[close_len..];
                        continue;
                    }
                    break;
                }
                if tag.starts_with("br")
                    || tag.starts_with("p")
                    || tag.starts_with("div")
                    || tag.starts_with("li")
                {
                    out.push('\n');
                }
                rest = &rest[i + end + 1..];
            }
        }
    }
    let text = decode_entities(&out);
    let mut collapsed = String::with_capacity(text.len());
    let mut last_ws = false;
    for ch in text.chars() {
        if ch == '\n' {
            if !last_ws {
                collapsed.push('\n');
            }
            last_ws = true;
        } else if ch.is_whitespace() {
            if !last_ws {
                collapsed.push(' ');
            }
            last_ws = true;
        } else {
            collapsed.push(ch);
            last_ws = false;
        }
    }
    let mut final_text = collapsed.trim().to_string();
    final_text.truncate(final_text.len().min(120_000));
    (title, final_text)
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

pub async fn ensure_not_private(parsed: &url::Url) -> Result<(), ToolError> {
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::InvalidArgs("url".into(), "URL has no host".into()))?;
    // IP-literal fast path.
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(ToolError::NotPermitted(format!(
                "refusing to fetch '{host}': private/local network addresses are blocked"
            )));
        }
        return Ok(());
    }
    // Resolve and check every address (blocks DNS-based bypasses).
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs: Vec<std::net::SocketAddr> = match tokio::net::lookup_host((host, port)).await {
        Ok(it) => it.collect(),
        Err(e) => {
            return Err(ToolError::Execution(format!(
                "DNS lookup failed for '{host}': {e}"
            )))
        }
    };
    for a in addrs {
        if is_private_ip(a.ip()) {
            return Err(ToolError::NotPermitted(format!(
                "refusing to fetch '{host}': it resolves to a private/local address ({})",
                a.ip()
            )));
        }
    }
    Ok(())
}

pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00  // unique local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_and_tags() {
        let html = "<html><head><title>My Page</title><style>.x{}</style></head><body><script>alert(1)</script><h1>Hello</h1><p>World &amp; more</p></body></html>";
        let (title, text) = strip_html(html);
        assert_eq!(title.as_deref(), Some("My Page"));
        assert!(text.contains("Hello"));
        assert!(text.contains("World & more"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("<h1>"));
    }

    #[test]
    fn private_ip_detection() {
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700::1"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }
}

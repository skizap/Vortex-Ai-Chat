//! HTML → text extraction (dependency-free) and private-IP policy helpers.

use std::net::{IpAddr, SocketAddr};

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
    // Cap by bytes but never split a UTF-8 character: `truncate` panics on a
    // non-char boundary, and fetched pages are full of multi-byte text.
    if final_text.len() > 120_000 {
        let mut cut = 120_000;
        while !final_text.is_char_boundary(cut) {
            cut -= 1;
        }
        final_text.truncate(cut);
    }
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

/// Production resolver: OS DNS via tokio.
pub async fn default_resolve(host: String, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host((host, port)).await?.collect())
}

/// Resolve a URL's host and validate *every* returned address against the
/// private-network policy: if any address is private, the whole URL is
/// rejected. Returns the full validated address list (port included) so the
/// caller can pin the actual connection to one of these exact addresses —
/// closing the DNS-rebinding TOCTOU window between validation and connect.
///
/// The resolver is injectable so tests can simulate rebinding without DNS.
pub async fn validated_addrs<F, Fut>(
    parsed: &url::Url,
    resolver: F,
) -> Result<Vec<SocketAddr>, ToolError>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::InvalidArgs("url".into(), "URL has no host".into()))?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    // IP-literal fast path: no DNS involved, nothing can rebind.
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(ToolError::NotPermitted(format!(
                "refusing to fetch '{host}': private/local network addresses are blocked"
            )));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs = resolver(host.to_string(), port)
        .await
        .map_err(|e| ToolError::Execution(format!("DNS lookup failed for '{host}': {e}")))?;
    if addrs.is_empty() {
        return Err(ToolError::Execution(format!(
            "DNS lookup for '{host}' returned no addresses"
        )));
    }
    for a in &addrs {
        if is_private_ip(a.ip()) {
            return Err(ToolError::NotPermitted(format!(
                "refusing to fetch '{host}': it resolves to a private/local address ({})",
                a.ip()
            )));
        }
    }
    Ok(addrs)
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
            // IPv4-mapped IPv6 addresses (::ffff:a.b.c.d) must be judged by
            // their IPv4 half; otherwise e.g. ::ffff:127.0.0.1 slips past
            // every check below and defeats the loopback/private block.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
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
            // IPv4-mapped IPv6 must inherit the IPv4 policy.
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.5",
            "::ffff:169.254.169.254",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700::1", "::ffff:1.1.1.1"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[test]
    fn size_cap_never_splits_a_utf8_character() {
        // Each char is 3 bytes; 120_000 is not a multiple of 3, so the old
        // `truncate(120_000)` panicked on this input.
        let big = "あ".repeat(60_000);
        let html = format!("<p>{big}</p>");
        let (_, text) = strip_html(&html);
        assert!(text.len() <= 120_000);
        assert!(text.chars().all(|c| c == 'あ'));
    }
}

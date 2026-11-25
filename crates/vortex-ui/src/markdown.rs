//! Safe Markdown rendering: pulldown-cmark event walk → escaped HTML,
//! syntect highlighting for code blocks, strict link policy (http/https
//! only). No raw HTML passes through, ever.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

const THEME: &str = "base16-ocean.dark";

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn render_markdown(md: &str) -> String {
    let parser = Parser::new_ext(md, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES);
    let mut out = String::with_capacity(md.len() * 2);
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_text = String::new();
    let mut ordered: Vec<bool> = Vec::new();
    // Image handling: only relative attachment URLs are allowed; the alt text
    // is collected from the inner events and everything else is neutralized.
    let mut image_depth = 0;
    let mut image_src: Option<String> = None;
    let mut image_alt = String::new();

    for ev in parser {
        if image_depth > 0 {
            match ev {
                Event::Start(Tag::Image { .. }) => image_depth += 1,
                Event::End(TagEnd::Image) => {
                    image_depth -= 1;
                    if image_depth == 0 {
                        if let Some(src) = image_src.take() {
                            out.push_str(&format!(
                                "<img src=\"{}\" alt=\"{}\" loading=\"lazy\" class=\"md-image\">",
                                esc(&src),
                                esc(&image_alt)
                            ));
                        }
                    }
                }
                Event::Text(t) | Event::Code(t) | Event::InlineHtml(t) => image_alt.push_str(&t),
                Event::SoftBreak | Event::HardBreak => image_alt.push(' '),
                _ => {}
            }
            continue;
        }
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                code_text.clear();
                code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => info
                        .split([' ', '\t', ','])
                        .next()
                        .unwrap_or("")
                        .to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                out.push_str(&highlighted_code(&code_lang, &code_text));
                in_code = false;
            }
            Event::Text(t) if in_code => code_text.push_str(&t),
            Event::Text(t) => out.push_str(&esc(&t)),
            Event::Code(t) => {
                out.push_str("<code>");
                out.push_str(&esc(&t));
                out.push_str("</code>");
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Start(Tag::Paragraph) => out.push_str("<p>"),
            Event::End(TagEnd::Paragraph) => out.push_str("</p>"),
            Event::Start(Tag::Heading { level, .. }) => {
                out.push_str(&format!("<{}>", heading_tag(level)));
            }
            Event::End(TagEnd::Heading(level, ..)) => {
                out.push_str(&format!("</{}>", heading_tag(level)));
            }
            Event::Start(Tag::BlockQuote(_)) => out.push_str("<blockquote>"),
            Event::End(TagEnd::BlockQuote(_)) => out.push_str("</blockquote>"),
            Event::Start(Tag::List(Some(start))) => {
                ordered.push(true);
                if start == 1 {
                    out.push_str("<ol>");
                } else {
                    // Preserve the author's numbering: a list starting at N
                    // used to emit bare <li> elements with no wrapper at all.
                    out.push_str(&format!("<ol start=\"{start}\">"));
                }
            }
            Event::Start(Tag::List(None)) => {
                ordered.push(false);
                out.push_str("<ul>");
            }
            Event::End(TagEnd::List(_)) => {
                if ordered.pop().unwrap_or(false) {
                    out.push_str("</ol>");
                } else {
                    out.push_str("</ul>");
                }
            }
            Event::Start(Tag::Item) => out.push_str("<li>"),
            Event::End(TagEnd::Item) => out.push_str("</li>"),
            Event::Start(Tag::Emphasis) => out.push_str("<em>"),
            Event::End(TagEnd::Emphasis) => out.push_str("</em>"),
            Event::Start(Tag::Strong) => out.push_str("<strong>"),
            Event::End(TagEnd::Strong) => out.push_str("</strong>"),
            Event::Start(Tag::Strikethrough) => out.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => out.push_str("</s>"),
            Event::Start(Tag::Image { dest_url, .. }) => {
                image_depth = 1;
                image_alt.clear();
                let dest = dest_url.as_ref();
                image_src = is_allowed_image_url(dest).then(|| {
                    if dest.contains('?') {
                        dest.to_owned()
                    } else {
                        crate::api::with_lan_token(dest)
                    }
                });
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let dest = dest_url.as_ref();
                if dest.starts_with("http://")
                    || dest.starts_with("https://")
                    || dest.starts_with('#')
                {
                    out.push_str(&format!(
                        "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">",
                        esc(dest)
                    ));
                } else {
                    // Neutralize anything else (javascript:, data:, ...).
                    out.push_str("<a href=\"#\" title=\"blocked link scheme\">");
                }
            }
            Event::End(TagEnd::Link) => out.push_str("</a>"),
            Event::Start(Tag::Table(_)) => out.push_str(
                "<div class=\"table-scroll\" tabindex=\"0\" role=\"region\" aria-label=\"Markdown table\"><table>",
            ),
            Event::End(TagEnd::Table) => out.push_str("</table></div>"),
            Event::Start(Tag::TableHead) => out.push_str("<thead><tr>"),
            Event::End(TagEnd::TableHead) => out.push_str("</tr></thead>"),
            Event::Start(Tag::TableRow) => out.push_str("<tr>"),
            Event::End(TagEnd::TableRow) => out.push_str("</tr>"),
            Event::Start(Tag::TableCell) => out.push_str("<td>"),
            Event::End(TagEnd::TableCell) => out.push_str("</td>"),
            _ => {}
        }
    }
    out
}

fn heading_tag(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "h1",
        HeadingLevel::H2 => "h2",
        HeadingLevel::H3 => "h3",
        HeadingLevel::H4 => "h4",
        HeadingLevel::H5 => "h5",
        HeadingLevel::H6 => "h6",
    }
}

/// Only relative attachment URLs are renderable as images:
/// `^/api/attachments/[0-9a-f-]{36}(\?token=[A-Za-z0-9]+)?$`.
/// External image URLs stay blocked (deliberate — no remote tracking pixels).
fn is_allowed_image_url(url: &str) -> bool {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    };
    let Some(rest) = path.strip_prefix("/api/attachments/") else {
        return false;
    };
    if rest.len() != 36
        || !rest
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'a'..='f' | '-'))
    {
        return false;
    }
    if let Some(q) = query {
        let Some(tok) = q.strip_prefix("token=") else {
            return false;
        };
        if tok.is_empty() || !tok.chars().all(|c| c.is_ascii_alphanumeric()) {
            return false;
        }
    }
    true
}

fn highlighted_code(lang: &str, code: &str) -> String {
    let ss = syntax_set();
    let syntax = ss
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let ts = theme_set();
    let theme = ts
        .themes
        .get(THEME)
        .unwrap_or_else(|| ts.themes.values().next().expect("themes exist"));
    let mut html = String::from("<pre class=\"code-block\" tabindex=\"0\"><code>");
    match syntect::html::highlighted_html_for_string(code, ss, syntax, theme) {
        Ok(highlighted) => {
            // Strip syntect's own <pre>/<code> wrapper; we provide ours.
            let inner = highlighted
                .replace("<pre style=\"background-color:#2b303b;\">\n", "")
                .replace("<pre style=\"background-color:#2b303b;\">", "")
                .replace("</pre>", "")
                .replace("<code>", "")
                .replace("</code>", "");
            html.push_str(&inner);
        }
        Err(_) => html.push_str(&esc(code)),
    }
    html.push_str("</code></pre>");
    html
}

fn syntax_set() -> &'static syntect::parsing::SyntaxSet {
    use std::sync::OnceLock;
    static SS: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
    SS.get_or_init(syntect::parsing::SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static syntect::highlighting::ThemeSet {
    use std::sync::OnceLock;
    static TS: OnceLock<syntect::highlighting::ThemeSet> = OnceLock::new();
    TS.get_or_init(syntect::highlighting::ThemeSet::load_defaults)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "00000000-0000-4000-8000-000000000001";

    #[test]
    fn allowed_image_urls() {
        assert!(is_allowed_image_url(&format!("/api/attachments/{UUID}")));
        assert!(is_allowed_image_url(&format!(
            "/api/attachments/{UUID}?token=abc123"
        )));
    }

    #[test]
    fn blocked_image_urls() {
        assert!(!is_allowed_image_url("https://evil.example/x.png"));
        assert!(!is_allowed_image_url("http://127.0.0.1/api/attachments/x"));
        assert!(!is_allowed_image_url("data:image/png;base64,AAAA"));
        assert!(!is_allowed_image_url("javascript:alert(1)"));
        assert!(!is_allowed_image_url("/api/attachments/../../etc/passwd"));
        assert!(!is_allowed_image_url("/api/attachments/not-a-uuid"));
        assert!(!is_allowed_image_url(&format!(
            "/api/attachments/{UUID}?token=a b"
        )));
        assert!(!is_allowed_image_url(&format!(
            "/api/attachments/{UUID}?redirect=evil"
        )));
        // Uppercase hex is outside the policy class.
        assert!(!is_allowed_image_url(
            "/api/attachments/00000000-0000-4000-8000-00000000000A"
        ));
    }

    #[test]
    fn renders_attachment_image_with_alt() {
        let md = format!("![a cat](/api/attachments/{UUID})");
        let html = render_markdown(&md);
        assert!(html.contains(&format!("src=\"/api/attachments/{UUID}\"")));
        assert!(html.contains("alt=\"a cat\""));
        assert!(html.contains("loading=\"lazy\""));
    }

    #[test]
    fn external_image_is_neutralized() {
        let html = render_markdown("![x](https://evil.example/pixel.png)");
        assert!(!html.contains("<img"));
        assert!(!html.contains("evil.example"));
    }

    #[test]
    fn image_alt_is_escaped() {
        let md = format!("![\"><script>](/api/attachments/{UUID})");
        let html = render_markdown(&md);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&quot;&gt;&lt;script&gt;"));
    }

    #[test]
    fn nested_image_in_blocked_alt_cannot_emit_markup() {
        let md = format!("![outer ![inner](/api/attachments/{UUID})](https://example.test/image)");
        assert_eq!(render_markdown(&md), "<p></p>");
    }
}

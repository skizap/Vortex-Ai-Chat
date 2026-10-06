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

    for ev in parser {
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
            Event::Start(Tag::BlockQuote(_)) => out.push_str("<blockquote><p>"),
            Event::End(TagEnd::BlockQuote(_)) => out.push_str("</p></blockquote>"),
            Event::Start(Tag::List(Some(1))) => {
                ordered.push(true);
                out.push_str("<ol>");
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
            Event::Start(Tag::Table(_)) => out.push_str("<table>"),
            Event::End(TagEnd::Table) => out.push_str("</table>"),
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
    let mut html = String::from("<pre class=\"code-block\"><code>");
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

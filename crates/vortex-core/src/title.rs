//! Local conversation-title derivation. Pure functions, no provider calls:
//! automatic titles come from the first user request (or a local label for
//! attachment-only turns), and manual titles are canonicalized here so the
//! API can echo back exactly what was stored.

use vortex_types::{AttachmentKind, AttachmentRef};

/// Hard cap for manually entered titles (rejected, not truncated).
pub const MAX_TITLE_CHARS: usize = 200;
/// Automatic titles stay short enough for one sidebar row.
const AUTO_TITLE_CHARS: usize = 60;
/// The default title the UI gives a brand-new conversation.
pub const PLACEHOLDER_TITLE: &str = "New chat";

/// Reasons a manual title can be rejected by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleRejection {
    Empty,
    TooLong,
}

impl TitleRejection {
    pub fn message(self) -> &'static str {
        match self {
            TitleRejection::Empty => "title must not be empty",
            TitleRejection::TooLong => "title is too long (max 200 characters)",
        }
    }
}

/// Canonical form of a manual title: trimmed, internal whitespace collapsed
/// to single spaces (sidebar rows are single-line). Validation is separate so
/// the API can distinguish "empty" from "too long".
pub fn normalize_title(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn validate_title(normalized: &str) -> Result<(), TitleRejection> {
    if normalized.trim().is_empty() {
        return Err(TitleRejection::Empty);
    }
    if normalized.chars().count() > MAX_TITLE_CHARS {
        return Err(TitleRejection::TooLong);
    }
    Ok(())
}

/// Derive a short automatic title from first-turn request text: strip
/// markup, take the first meaningful clause, normalize whitespace and
/// truncate on char (Unicode) boundaries, preferring a word boundary.
/// Returns `None` when nothing meaningful remains.
pub fn derive_title(text: &str) -> Option<String> {
    let stripped = strip_markup(text);
    let clause = first_clause(&stripped);
    let normalized = normalize_title(clause);
    if normalized.is_empty() {
        return None;
    }
    Some(truncate_chars(&normalized, AUTO_TITLE_CHARS))
}

/// Local label for an attachment-only first turn, e.g. `Review 2 videos`.
pub fn attachments_label(refs: &[AttachmentRef]) -> Option<String> {
    if refs.is_empty() {
        return None;
    }
    let n = refs.len();
    let uniform = |k: AttachmentKind| refs.iter().all(|r| r.kind == k);
    let (noun, plural) = if uniform(AttachmentKind::Video) {
        ("video", "videos")
    } else if uniform(AttachmentKind::Image) {
        ("image", "images")
    } else if uniform(AttachmentKind::Pdf) {
        ("PDF", "PDFs")
    } else {
        ("attachment", "attachments")
    };
    let word = if n == 1 { noun } else { plural };
    Some(format!("Review {n} {word}"))
}

/// Title for a chat's first accepted turn: text wins; attachment-only turns
/// get a local label.
pub fn title_for_first_turn(content: &str, refs: &[AttachmentRef]) -> Option<String> {
    derive_title(content).or_else(|| attachments_label(refs))
}

/// Remove common markdown/HTML markup so the title is plain text.
fn strip_markup(text: &str) -> String {
    // Cut everything from an opening code fence; fenced blocks are content,
    // not a request clause.
    let before_fence = match text.find("```") {
        Some(idx) => &text[..idx],
        None => text,
    };
    let mut out = String::with_capacity(before_fence.len());
    let bytes: Vec<char> = before_fence.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            // Markdown links [label](url) -> label
            '[' => {
                if let Some(close) = find_char(&bytes, i + 1, ']') {
                    let label: String = bytes[i + 1..close].iter().collect();
                    if bytes.get(close + 1) == Some(&'(') {
                        if let Some(paren) = find_char(&bytes, close + 2, ')') {
                            out.push_str(&label);
                            i = paren + 1;
                            continue;
                        }
                    }
                    out.push_str(&label);
                    i = close + 1;
                    continue;
                }
                i += 1;
            }
            // HTML tags: only when '<' starts a tag name (avoid eating
            // comparisons like "a < b and c > d").
            '<' if matches!(bytes.get(i + 1), Some(ch) if ch.is_ascii_alphabetic() || *ch == '/') =>
            {
                if let Some(close) = find_char(&bytes, i + 1, '>') {
                    i = close + 1;
                    continue;
                }
                out.push(c);
                i += 1;
            }
            // Inline code: drop backticks, keep content.
            '`' => i += 1,
            // Emphasis/strikethrough markers.
            '*' | '~' => i += 1,
            // Heading/blockquote markers only at line start.
            '#' | '>' => {
                if at_line_start(&out) {
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    // Underscores: only drop bold-italic emphasis pairs, keep single
    // underscores so snake_case identifiers survive.
    out.replace("__", "")
}

fn find_char(hay: &[char], from: usize, needle: char) -> Option<usize> {
    (from..hay.len()).find(|&i| hay[i] == needle)
}

fn at_line_start(out: &str) -> bool {
    match out.chars().next_back() {
        None => true,
        Some('\n') => true,
        Some(' ') | Some('\t') => {
            // Spaces so far on this line only (list bullets like "- # x").
            let line = out.rsplit('\n').next().unwrap_or("");
            line.chars()
                .all(|c| c == ' ' || c == '\t' || c == '-' || c == '+')
        }
        _ => false,
    }
}

/// First meaningful clause: first non-empty line, cut at the first sentence
/// terminator or semicolon. A '.' only terminates a sentence at a word
/// boundary, so identifiers like `main.rs` survive.
fn first_clause(text: &str) -> &str {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let end = chars
        .iter()
        .enumerate()
        .find_map(|(idx, &(i, c))| match c {
            ';' => Some(i),
            '.' | '!' | '?' => match chars.get(idx + 1) {
                // Terminator only when followed by whitespace or line end.
                None => Some(i),
                Some(&(_, next)) if next.is_whitespace() => Some(i),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or(line.len());
    // A terminator mid-identifier (e.g. "file.rs") shouldn't leave a stub of
    // one or two characters; fall back to the whole line in that case.
    let clause = &line[..end];
    if clause.trim().chars().count() < 3 {
        line
    } else {
        clause
    }
}

/// Truncate to `max` chars (Unicode-safe), preferring the last word boundary
/// and marking elision with '…'.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(max).collect();
    if let Some(last_space) = cut.rfind(' ') {
        // Only back off to a word boundary when it keeps most of the title.
        if last_space * 2 > cut.len() {
            cut.truncate(last_space);
        }
    }
    let cut = cut.trim_end();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::AttachmentStatus;

    fn aref(kind: AttachmentKind) -> AttachmentRef {
        AttachmentRef {
            id: "a".into(),
            kind,
            filename: "f".into(),
            media_type: "application/octet-stream".into(),
            status: AttachmentStatus::Ready,
        }
    }

    #[test]
    fn normalize_and_validate() {
        assert_eq!(normalize_title("  hi \n\t there  "), "hi there");
        assert_eq!(validate_title(""), Err(TitleRejection::Empty));
        assert_eq!(validate_title("   "), Err(TitleRejection::Empty));
        let long = "é".repeat(MAX_TITLE_CHARS + 1);
        assert_eq!(validate_title(&long), Err(TitleRejection::TooLong));
        assert_eq!(validate_title(&"é".repeat(MAX_TITLE_CHARS)), Ok(()));
    }

    #[test]
    fn derive_takes_first_clause_and_strips_markup() {
        assert_eq!(
            derive_title("## Fix the **login** bug. Then redeploy.").as_deref(),
            Some("Fix the login bug")
        );
        assert_eq!(
            derive_title("Please `refactor` [the docs](https://x.dev/d); they are stale")
                .as_deref(),
            Some("Please refactor the docs")
        );
        assert_eq!(
            derive_title("line one\nline two").as_deref(),
            Some("line one")
        );
        assert_eq!(
            derive_title("Multi-line request:\n* do a\n* do b").as_deref(),
            Some("Multi-line request:")
        );
    }

    #[test]
    fn derive_strips_code_fences() {
        assert_eq!(
            derive_title("Explain this:\n```rust\nfn main() {}\n```").as_deref(),
            Some("Explain this:")
        );
        assert_eq!(derive_title("```rust\nfn main() {}\n```"), None);
    }

    #[test]
    fn derive_normalizes_whitespace() {
        assert_eq!(
            derive_title("  fix   the\t bug  ").as_deref(),
            Some("fix the bug")
        );
    }

    #[test]
    fn derive_keeps_file_names_with_dots() {
        // "main.rs" must not collapse to "ma" at the '.' terminator.
        assert_eq!(
            derive_title("Update main.rs please").as_deref(),
            Some("Update main.rs please")
        );
    }

    #[test]
    fn derive_handles_empty_and_markup_only() {
        assert_eq!(derive_title(""), None);
        assert_eq!(derive_title("   \n\t  "), None);
        assert_eq!(derive_title("###"), None);
        assert_eq!(derive_title("<div><span></span></div>"), None);
    }

    #[test]
    fn derive_truncates_on_unicode_boundaries() {
        let long = "日本語のタイトル".repeat(20); // 160 chars, no spaces
        let t = derive_title(&long).unwrap();
        assert!(t.chars().count() <= AUTO_TITLE_CHARS + 1);
        assert!(t.ends_with('…'));
        assert!(long.starts_with(&t[..t.len() - '…'.len_utf8()]));

        let words = "word ".repeat(40); // 200 chars with spaces
        let t = derive_title(words.trim()).unwrap();
        assert!(t.chars().count() <= AUTO_TITLE_CHARS + 1);
        assert!(!t.contains("  "));
        assert!(!t.ends_with(" …"), "no space before the ellipsis");
        assert!(t.ends_with('…'));

        let emoji = "🚀 launch ".repeat(20);
        let t = derive_title(emoji.trim()).unwrap();
        assert!(t.chars().count() <= AUTO_TITLE_CHARS + 1);
        // No split surrogate/char: the result is a valid String by
        // construction; check it does not end mid-word with a stray space.
        assert!(!t.ends_with(" …"));
    }

    #[test]
    fn attachment_labels() {
        assert_eq!(attachments_label(&[]), None);
        assert_eq!(
            attachments_label(&[aref(AttachmentKind::Video)]).as_deref(),
            Some("Review 1 video")
        );
        assert_eq!(
            attachments_label(&[aref(AttachmentKind::Video), aref(AttachmentKind::Video)])
                .as_deref(),
            Some("Review 2 videos")
        );
        assert_eq!(
            attachments_label(&[aref(AttachmentKind::Image), aref(AttachmentKind::Image)])
                .as_deref(),
            Some("Review 2 images")
        );
        assert_eq!(
            attachments_label(&[aref(AttachmentKind::Pdf), aref(AttachmentKind::Pdf)]).as_deref(),
            Some("Review 2 PDFs")
        );
        assert_eq!(
            attachments_label(&[aref(AttachmentKind::Image), aref(AttachmentKind::Video)])
                .as_deref(),
            Some("Review 2 attachments")
        );
    }

    #[test]
    fn first_turn_prefers_text_then_attachments() {
        assert_eq!(
            title_for_first_turn("Summarize this", &[aref(AttachmentKind::Pdf)]).as_deref(),
            Some("Summarize this")
        );
        assert_eq!(
            title_for_first_turn(
                "  ",
                &[aref(AttachmentKind::Video), aref(AttachmentKind::Video)]
            )
            .as_deref(),
            Some("Review 2 videos")
        );
        assert_eq!(title_for_first_turn("", &[]), None);
    }
}

//! Minimal, allocation-light SSE line parser for provider streams.
//!
//! Follows the SSE spec for event dispatch: `data:` lines accumulate and the
//! event is emitted when a blank line is seen (multiple `data:` lines are
//! joined with `\n`). [`SseParser::finish`] flushes a trailing event that was
//! never terminated by a blank line. Oversized buffers are reported via
//! [`SseParser::take_overflow`] instead of being silently dropped.

/// Hard cap for one buffered line and for one assembled event payload.
const MAX_BUFFER: usize = 1 << 20; // 1 MiB per line
const MAX_EVENT: usize = 8 << 20; // 8 MiB per event

pub struct SseParser {
    buf: Vec<u8>,
    data_lines: Vec<String>,
    event_bytes: usize,
    overflowed: bool,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(8 * 1024),
            data_lines: Vec::new(),
            event_bytes: 0,
            overflowed: false,
        }
    }

    /// Feed raw bytes; returns completed event payload strings (dispatched on
    /// blank lines, per the SSE spec).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = &line[..line.len() - 1]; // strip \n
            let line = if line.last() == Some(&b'\r') {
                &line[..line.len() - 1]
            } else {
                line
            };
            if line.is_empty() {
                if let Some(payload) = self.dispatch() {
                    out.push(payload);
                }
            } else if let Some(rest) = line.strip_prefix(b"data:") {
                // Spec: strip at most one leading space after the colon.
                let value = rest.strip_prefix(&b" "[..]).unwrap_or(rest);
                let value = String::from_utf8_lossy(value).into_owned();
                self.event_bytes += value.len() + 1;
                self.data_lines.push(value);
                if self.event_bytes > MAX_EVENT {
                    self.overflowed = true;
                    self.data_lines.clear();
                    self.event_bytes = 0;
                }
            }
            // Other fields (event:, id:, retry:, comments) are ignored:
            // the transport only consumes `data:` payloads.
        }
        // Bound the buffer so a hostile stream cannot grow memory forever.
        // The overflow is reported (take_overflow) rather than silently
        // discarded, so the caller can fail the stream instead of parsing a
        // corrupted next payload.
        if self.buf.len() > MAX_BUFFER {
            self.buf.clear();
            self.overflowed = true;
        }
        out
    }

    /// Flush a pending event that was not terminated by a blank line
    /// (called when the stream ends). Also processes a final `data:` line
    /// that arrived without a trailing newline.
    pub fn finish(&mut self) -> Option<String> {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            let mut slice = &line[..];
            if slice.last() == Some(&b'\r') {
                slice = &slice[..slice.len() - 1];
            }
            if let Some(rest) = slice.strip_prefix(b"data:") {
                let value = rest.strip_prefix(&b" "[..]).unwrap_or(rest);
                let value = String::from_utf8_lossy(value).into_owned();
                self.event_bytes += value.len();
                self.data_lines.push(value);
            }
        }
        self.dispatch()
    }

    /// Reports (and clears) whether the parser dropped oversized data.
    pub fn take_overflow(&mut self) -> bool {
        std::mem::take(&mut self.overflowed)
    }

    fn dispatch(&mut self) -> Option<String> {
        if self.data_lines.is_empty() {
            self.event_bytes = 0;
            return None;
        }
        let payload = self.data_lines.join("\n");
        self.data_lines.clear();
        self.event_bytes = 0;
        if payload.is_empty() {
            None
        } else {
            Some(payload)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chunked_sse_lines() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"a\":").is_empty());
        let events = p.push(b"1}\n\n");
        assert_eq!(events, vec!["{\"a\":1}".to_string()]);
        let events = p.push(b"data: [DONE]\r\n\r\n");
        assert_eq!(events, vec!["[DONE]".to_string()]);
    }

    #[test]
    fn joins_multiline_data_fields_per_spec() {
        let mut p = SseParser::new();
        let events = p.push(b"data: {\"a\":\ndata: 1}\n\n");
        assert_eq!(events, vec!["{\"a\":\n1}".to_string()]);
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        let mut p = SseParser::new();
        let events = p.push(b"data:  spaced\n\n");
        assert_eq!(events, vec![" spaced".to_string()]);
        let events = p.push(b"data:tight\n\n");
        assert_eq!(events, vec!["tight".to_string()]);
    }

    #[test]
    fn finish_flushes_unterminated_event() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: tail").is_empty());
        assert_eq!(p.finish(), Some("tail".to_string()));
        assert_eq!(p.finish(), None);
    }

    #[test]
    fn oversized_line_is_reported_not_silently_dropped() {
        let mut p = SseParser::new();
        let big = vec![b'x'; (1 << 20) + 10];
        p.push(&big);
        assert!(p.take_overflow());
        assert!(!p.take_overflow()); // flag is cleared once taken
    }

    #[test]
    fn comment_and_event_fields_are_ignored() {
        let mut p = SseParser::new();
        let events = p.push(b": keepalive\nevent: message\ndata: payload\n\n");
        assert_eq!(events, vec!["payload".to_string()]);
    }
}

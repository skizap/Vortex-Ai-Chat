//! Minimal, allocation-light SSE line parser for provider streams.

pub struct SseParser {
    buf: Vec<u8>,
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
        }
    }

    /// Feed raw bytes; returns completed `data:` payload strings.
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
            if let Some(rest) = line.strip_prefix(b"data:") {
                let payload = String::from_utf8_lossy(rest).trim().to_string();
                if !payload.is_empty() {
                    out.push(payload);
                }
            }
        }
        // Bound the buffer so a hostile stream cannot grow memory forever.
        if self.buf.len() > 1 << 20 {
            self.buf.clear();
        }
        out
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
        let events = p.push(b"data: [DONE]\r\n");
        assert_eq!(events, vec!["[DONE]".to_string()]);
    }
}

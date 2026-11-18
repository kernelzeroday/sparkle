//! Incremental server-sent-events decoder. Works on bytes so that a multi-byte
//! character split across two network reads is never corrupted.

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl SseDecoder {
    /// Feeds bytes in and returns the data payload of every completed event.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                self.data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
            }
        }
        events
    }

    /// Returns a final event when the stream ended without a trailing blank line.
    pub fn finish(&mut self) -> Option<String> {
        let mut tail = self.push(b"\n\n");
        tail.pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_events_split_across_reads() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: {\"a\"").is_empty());
        assert_eq!(decoder.push(b":1}\n\ndata: two\r\n\r\n"), vec!["{\"a\":1}", "two"]);
    }

    #[test]
    fn keeps_multibyte_characters_split_between_reads() {
        let mut decoder = SseDecoder::default();
        let bytes = "data: héllo\n\n".as_bytes();
        assert!(decoder.push(&bytes[..8]).is_empty());
        assert_eq!(decoder.push(&bytes[8..]), vec!["héllo"]);
    }

    #[test]
    fn ignores_comments_and_flushes_unterminated_event() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b": keep-alive\n\ndata: tail").is_empty());
        assert_eq!(decoder.finish().as_deref(), Some("tail"));
    }
}

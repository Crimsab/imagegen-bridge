//! Incremental, bounded Server-Sent Events decoding.

use bytes::BytesMut;
use imagegen_bridge_core::{BridgeError, ErrorCode};

/// Limits applied before JSON or base64 parsing.
#[derive(Debug, Clone, Copy)]
pub struct SseLimits {
    /// Maximum aggregate bytes received across the complete response stream.
    pub max_stream_bytes: usize,
    /// Maximum bytes in one physical SSE line.
    pub max_line_bytes: usize,
    /// Maximum decoded events in one response.
    pub max_events: usize,
    /// Maximum aggregate bytes across `data:` lines for one event.
    pub max_event_bytes: usize,
}

impl Default for SseLimits {
    fn default() -> Self {
        Self {
            max_stream_bytes: 256 * 1024 * 1024,
            max_line_bytes: 128 * 1024 * 1024,
            max_events: 10_000,
            max_event_bytes: 128 * 1024 * 1024,
        }
    }
}

/// Stateful SSE decoder tolerant of arbitrary network chunk boundaries.
#[derive(Debug)]
pub struct SseDecoder {
    buffer: BytesMut,
    data: String,
    events: usize,
    stream_bytes: usize,
    limits: SseLimits,
}

impl SseDecoder {
    /// Creates a bounded decoder.
    #[must_use]
    pub fn new(limits: SseLimits) -> Self {
        Self {
            buffer: BytesMut::new(),
            data: String::new(),
            events: 0,
            stream_bytes: 0,
            limits,
        }
    }

    /// Pushes a network chunk and returns every completed event data payload.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, BridgeError> {
        self.stream_bytes = self
            .stream_bytes
            .checked_add(chunk.len())
            .ok_or_else(|| sse_error("SSE stream byte count overflowed"))?;
        if self.stream_bytes > self.limits.max_stream_bytes {
            return Err(sse_error("SSE stream exceeds the configured limit"));
        }
        if self.buffer.len().saturating_add(chunk.len()) > self.limits.max_line_bytes {
            return Err(sse_error("SSE line exceeds the configured limit"));
        }
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.split_to(position + 1);
            line.truncate(position);
            if line.last() == Some(&b'\r') {
                line.truncate(line.len() - 1);
            }
            let line = std::str::from_utf8(&line)
                .map_err(|_| sse_error("SSE response is not valid UTF-8"))?;
            self.process_line(line, &mut events)?;
        }
        Ok(events)
    }

    /// Flushes a final unterminated line/event at end of stream.
    pub fn finish(mut self) -> Result<Vec<String>, BridgeError> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let remaining = self.buffer.split().freeze();
            let line = std::str::from_utf8(&remaining)
                .map_err(|_| sse_error("SSE response is not valid UTF-8"))?;
            self.process_line(line.trim_end_matches('\r'), &mut events)?;
        }
        self.flush_event(&mut events)?;
        Ok(events)
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<String>) -> Result<(), BridgeError> {
        if line.is_empty() {
            return self.flush_event(events);
        }
        if line.starts_with(':') {
            return Ok(());
        }
        if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            let additional = value.len() + usize::from(!self.data.is_empty());
            if self.data.len().saturating_add(additional) > self.limits.max_event_bytes {
                return Err(sse_error("SSE event exceeds the configured limit"));
            }
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(value);
        }
        Ok(())
    }

    fn flush_event(&mut self, events: &mut Vec<String>) -> Result<(), BridgeError> {
        if self.data.is_empty() {
            return Ok(());
        }
        self.events = self.events.saturating_add(1);
        if self.events > self.limits.max_events {
            return Err(sse_error("SSE response contains too many events"));
        }
        events.push(std::mem::take(&mut self.data));
        Ok(())
    }
}

fn sse_error(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message).with_provider("codex-responses")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn handles_fragmented_crlf_multiline_and_heartbeats() {
        let mut decoder = SseDecoder::new(SseLimits::default());
        assert!(decoder.push(b": ping\r\nda").unwrap().is_empty());
        assert!(decoder.push(b"ta: {\"a\":\r\n").unwrap().is_empty());
        let events = decoder.push(b"data: 1}\r\n\r\n").unwrap();
        assert_eq!(events, ["{\"a\":\n1}"]);
    }

    #[test]
    fn flushes_unterminated_final_event() {
        let mut decoder = SseDecoder::new(SseLimits::default());
        assert!(decoder.push(b"data: [DONE]").unwrap().is_empty());
        assert_eq!(decoder.finish().unwrap(), ["[DONE]"]);
    }

    #[test]
    fn rejects_oversized_line_before_unbounded_growth() {
        let mut decoder = SseDecoder::new(SseLimits {
            max_line_bytes: 8,
            ..SseLimits::default()
        });
        assert!(decoder.push(b"123456789").is_err());
    }

    #[test]
    fn rejects_aggregate_stream_growth_across_small_chunks() {
        let mut decoder = SseDecoder::new(SseLimits {
            max_stream_bytes: 12,
            ..SseLimits::default()
        });
        assert!(decoder.push(b": ping\n").unwrap().is_empty());
        assert!(decoder.push(b": pong").is_err());
    }
}

//! JSON-line encoding/decoding for the protocol.

use std::marker::PhantomData;

use serde::{Serialize, de::DeserializeOwned};

/// Serialize a message to a JSON line (with trailing newline).
pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("serialization should not fail");
    s.push('\n');
    s
}

/// Parse a single JSON line.
///
/// Returns `Ok(None)` for empty/whitespace-only lines, `Ok(Some(T))` on
/// successful decode, or `Err` when the line contains invalid JSON.
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<Option<T>, serde_json::Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed).map(Some)
}

/// Line-buffered reader that yields complete messages.
///
/// Buffers partial lines until a newline is received.
pub struct LineReader<T, F> {
    buffer: String,
    callback: F,
    _marker: PhantomData<T>,
}

impl<T, F> LineReader<T, F>
where
    T: DeserializeOwned,
    F: FnMut(T),
{
    pub fn new(callback: F) -> Self {
        Self {
            buffer: String::new(),
            callback,
            _marker: PhantomData,
        }
    }

    /// Feed raw data from a stream. Complete lines are decoded and dispatched.
    pub fn push(&mut self, chunk: &str) {
        self.buffer.push_str(chunk);

        while let Some(newline_pos) = self.buffer.find('\n') {
            let line = self.buffer[..newline_pos].to_string();
            self.buffer = self.buffer[newline_pos + 1..].to_string();

            match decode_line(&line) {
                Ok(Some(msg)) => (self.callback)(msg),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, line = %line, "malformed JSON line, discarding");
                }
            }
        }
    }

    /// Flush any remaining data in the buffer.
    pub fn flush(&mut self) {
        if !self.buffer.trim().is_empty() {
            match decode_line(&self.buffer) {
                Ok(Some(msg)) => (self.callback)(msg),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, line = %self.buffer, "malformed JSON in buffer on flush, discarding");
                }
            }
        }
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Command, Event};
    use std::cell::RefCell;

    #[test]
    fn encode_command() {
        let cmd = Command::stop();
        let line = encode(&cmd);
        assert_eq!(line, "{\"cmd\":\"stop\"}\n");
    }

    #[test]
    fn decode_event() {
        let line = r#"{"ev":"system:ready"}"#;
        let event: Result<Option<Event>, _> = decode_line(line);
        let event = event.expect("should parse").expect("should not be empty");
        assert!(event.is_ready());
    }

    #[test]
    fn decode_empty() {
        let result: Result<Option<Event>, _> = decode_line("");
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn decode_malformed() {
        let result: Result<Option<Event>, _> = decode_line("{bad json}");
        assert!(result.is_err());
    }

    #[test]
    fn line_reader_complete_lines() {
        let events = RefCell::new(Vec::new());
        let mut reader: LineReader<Event, _> = LineReader::new(|e| events.borrow_mut().push(e));

        reader.push(r#"{"ev":"system:ready"}"#);
        reader.push("\n");

        assert_eq!(events.borrow().len(), 1);
        assert!(events.borrow()[0].is_ready());
    }

    #[test]
    fn line_reader_partial_lines() {
        let events = RefCell::new(Vec::new());
        let mut reader: LineReader<Event, _> = LineReader::new(|e| events.borrow_mut().push(e));

        reader.push(r#"{"ev":"agent:"#);
        assert_eq!(events.borrow().len(), 0);

        reader.push(r#"stdout","data":"hi"}"#);
        assert_eq!(events.borrow().len(), 0);

        reader.push("\n");
        assert_eq!(events.borrow().len(), 1);
    }

    #[test]
    fn line_reader_multiple_lines() {
        let events = RefCell::new(Vec::new());
        let mut reader: LineReader<Event, _> = LineReader::new(|e| events.borrow_mut().push(e));

        reader.push(
            r#"{"ev":"system:ready"}
{"ev":"agent:started","pid":123}
"#,
        );

        assert_eq!(events.borrow().len(), 2);
    }
}

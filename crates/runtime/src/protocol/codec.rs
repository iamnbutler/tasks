//! JSON-line encoding/decoding for the protocol.

use std::marker::PhantomData;

use serde::{Serialize, de::DeserializeOwned};

/// Serialize a message to a JSON line (with trailing newline).
pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("serialization should not fail");
    s.push('\n');
    s
}

/// Parse a single JSON line. Returns None on empty or invalid input.
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Option<T> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
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

            if let Some(msg) = decode_line(&line) {
                (self.callback)(msg);
            }
        }
    }

    /// Flush any remaining data in the buffer.
    pub fn flush(&mut self) {
        if !self.buffer.trim().is_empty() {
            if let Some(msg) = decode_line(&self.buffer) {
                (self.callback)(msg);
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
        let event: Option<Event> = decode_line(line);
        assert!(event.is_some());
        assert!(event.unwrap().is_ready());
    }

    #[test]
    fn decode_empty() {
        let event: Option<Event> = decode_line("");
        assert!(event.is_none());
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

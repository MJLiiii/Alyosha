//! An incremental parser for server-sent events.

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub name: Option<String>,
    pub data: String,
}

/// Turns arbitrary byte chunks into whole events.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    pending: Vec<u8>,
    name: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds bytes and returns the events they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(SseEvent {
                        name: self.name.take(),
                        data: self.data.join("\n"),
                    });
                }
                self.name = None;
                self.data.clear();
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line.as_ref(), ""),
            };
            match field {
                "event" => self.name = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_survive_any_chunking() {
        let stream = b"event: a\r\ndata: {\"x\":\ndata: 1}\r\n\r\n: comment\n\ndata: two\n\n";
        for split in 0..stream.len() {
            let mut parser = SseParser::default();
            let mut events = parser.feed(&stream[..split]);
            events.extend(parser.feed(&stream[split..]));
            assert_eq!(
                events,
                [
                    SseEvent {
                        name: Some("a".into()),
                        data: "{\"x\":\n1}".into()
                    },
                    SseEvent {
                        name: None,
                        data: "two".into()
                    },
                ]
            );
        }
    }
}

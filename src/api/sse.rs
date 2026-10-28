//! Server-sent events decoder: byte chunks in, complete events out.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Line endings are `\r\n`, `\n` or `\r`; a blank line ends an event. Bytes
/// are held until a whole line has arrived.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk and returns every event it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some((line_end, next)) = self.next_line_end(start) {
            let line = String::from_utf8_lossy(&self.buf[start..line_end]).into_owned();
            start = next;
            if let Some(event) = self.line(&line) {
                out.push(event);
            }
        }
        self.buf.drain(..start);
        out
    }

    /// Ends the stream: a held trailing `\r` ends its line. An event with no
    /// closing blank line is dropped.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.buf.last() == Some(&b'\r') {
            self.buf.pop();
            self.buf.push(b'\n');
            return self.push(&[]);
        }
        Vec::new()
    }

    /// The end of the first complete line at or after `start`, and the start
    /// of the line after it. `None` while the line is incomplete, including a
    /// `\r` that is the last byte held.
    fn next_line_end(&self, start: usize) -> Option<(usize, usize)> {
        let i = start
            + self.buf[start..]
                .iter()
                .position(|&b| b == b'\n' || b == b'\r')?;
        if self.buf[i] == b'\n' {
            return Some((i, i + 1));
        }
        match self.buf.get(i + 1) {
            Some(b'\n') => Some((i, i + 2)),
            Some(_) => Some((i, i + 1)),
            None => None,
        }
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            if self.event.is_none() && self.data.is_empty() {
                return None;
            }
            return Some(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data).join("\n"),
            });
        }
        // A comment line (`: ...`) has an empty field name and falls to `_`.
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: Some(event.to_string()),
            data: data.to_string(),
        }
    }

    fn decode_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        let mut events: Vec<SseEvent> = chunks.iter().flat_map(|c| decoder.push(c)).collect();
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn decodes_events_separated_by_blank_lines() {
        let stream = b"event: ping\ndata: {\"type\": \"ping\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        assert_eq!(
            decode_all(&[stream]),
            vec![
                ev("ping", "{\"type\": \"ping\"}"),
                ev("message_stop", "{\"type\":\"message_stop\"}"),
            ]
        );
    }

    #[test]
    fn a_split_at_any_byte_gives_the_same_events() {
        let stream = "event: content_block_delta\r\ndata: {\"text\":\"안녕\"}\r\n\r\nevent: message_stop\ndata: {}\n\n".as_bytes();
        let expected = vec![
            ev("content_block_delta", "{\"text\":\"안녕\"}"),
            ev("message_stop", "{}"),
        ];
        for cut in 1..stream.len() {
            let (a, b) = stream.split_at(cut);
            assert_eq!(decode_all(&[a, b]), expected, "split at byte {cut}");
        }
    }

    #[test]
    fn a_lone_carriage_return_ends_a_line() {
        assert_eq!(
            decode_all(&[b"event: ping\rdata: {}\r\r"]),
            vec![ev("ping", "{}")]
        );
    }

    #[test]
    fn data_lines_are_joined_with_newlines_and_comments_are_skipped() {
        assert_eq!(
            decode_all(&[b": keep-alive\nevent: error\ndata: line one\ndata:line two\n\n"]),
            vec![ev("error", "line one\nline two")]
        );
    }
}

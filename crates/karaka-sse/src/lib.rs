//! Server-sent events, read out of an HTTP body.
//!
//! Model providers stream their answers as server-sent events. This crate
//! does only the framing, the same for every provider: what each event's
//! JSON means is the provider crate's job. It has no dependencies, so it
//! works with any HTTP client: push the body's bytes in as they arrive and
//! take complete events out.
//!
//! The body arrives in chunks that can end anywhere: halfway through a line,
//! or even halfway through one UTF-8 character. So bytes are buffered until a
//! whole line is there, and only whole lines are turned into text.
//!
//! An event is a few `field: value` lines followed by a blank line. For the
//! model providers only the `data` field matters: Anthropic repeats the
//! event's type inside its JSON, and OpenAI-style servers send no `event`
//! field at all. [`Sse::next_event`] gives just that. MCP's Streamable HTTP
//! needs the rest too: an event's name, its id for resuming a stream, and
//! the server's `retry` delay. [`Sse::next_message`] gives those.

/// Turns chunks of bytes into the `data` of each complete event.
///
/// ```
/// let mut sse = karaka_sse::Sse::default();
/// sse.push(b"data: {\"a\":1}\n");
/// // The blank line that ends the event hasn't arrived yet.
/// assert_eq!(sse.next_event(), None);
/// sse.push(b"\n");
/// assert_eq!(sse.next_event().as_deref(), Some("{\"a\":1}"));
/// ```
#[derive(Debug, Default)]
pub struct Sse {
    /// Bytes received but not yet read as lines.
    buffer: Vec<u8>,
    /// How far into `buffer` lines have been read. Read lines are dropped in
    /// one go when more bytes are pushed, rather than one at a time.
    start: usize,
    /// The `data` lines of the event being read, joined with `\n`.
    data: Option<String>,
    /// The event being read's `event` field.
    event: Option<String>,
    /// The event being read's `id` field.
    id: Option<String>,
    /// The latest `retry` field, in milliseconds, until taken.
    retry: Option<u64>,
}

/// One whole event, with the fields beside its data. (The `EventSource`
/// spec's event, as `eventsource-parser` gives it to dsh's MCP SDK.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseMessage {
    /// The `event` field; `None` for an unnamed event, which a reader takes
    /// as `message`.
    pub event: Option<String>,
    /// The `data` lines, joined with `\n`.
    pub data: String,
    /// The `id` field, if the event had one.
    pub id: Option<String>,
}

impl Sse {
    /// Adds the next chunk of the body.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.drain(..self.start);
        self.start = 0;
        self.buffer.extend_from_slice(bytes);
    }

    /// The `data` of the next complete event, or `None` until more bytes
    /// arrive.
    pub fn next_event(&mut self) -> Option<String> {
        self.next_message().map(|message| message.data)
    }

    /// The next complete event, with its name and id, or `None` until more
    /// bytes arrive.
    pub fn next_message(&mut self) -> Option<SseMessage> {
        while let Some(line) = self.next_line() {
            if line.is_empty() {
                // A blank line ends the event. One with no data, such as a
                // run of comments, dispatches nothing.
                let event = self.event.take();
                let id = self.id.take();
                if let Some(data) = self.data.take() {
                    return Some(SseMessage { event, data, id });
                }
                continue;
            }
            // Comments start with `:` and so have an empty name; they and
            // unknown fields are ignored.
            let (field, value) = line.split_once(':').unwrap_or((&line, ""));
            // One space after the colon is part of the format, not the
            // value.
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => match &mut self.data {
                    Some(data) => {
                        data.push('\n');
                        data.push_str(value);
                    }
                    None => self.data = Some(value.to_owned()),
                },
                "event" => self.event = Some(value.to_owned()),
                // An id with a NUL is ignored, as the spec says.
                "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
                "retry" => {
                    if let Ok(retry) = value.parse() {
                        self.retry = Some(retry);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The latest `retry` the server sent, in milliseconds, once.
    pub fn take_retry(&mut self) -> Option<u64> {
        self.retry.take()
    }

    /// The next whole line, without its line ending, or `None` if the line
    /// hasn't fully arrived.
    fn next_line(&mut self) -> Option<String> {
        let rest = &self.buffer[self.start..];
        let end = rest.iter().position(|&byte| byte == b'\n')?;
        self.start += end + 1;
        let line = &rest[..end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // A whole line is whole UTF-8 too, as long as the server sent UTF-8.
        // If it didn't, the bad bytes become `�` and the provider's
        // JSON decoder reports the event.
        Some(String::from_utf8_lossy(line).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{Sse, SseMessage};

    #[test]
    fn a_message_keeps_its_name_id_and_the_retry() {
        let mut sse = Sse::default();
        sse.push(b"retry: 2500\nevent: message\nid: 7\ndata: {}\n\ndata: plain\n\n");
        assert_eq!(
            sse.next_message(),
            Some(SseMessage {
                event: Some("message".into()),
                data: "{}".into(),
                id: Some("7".into()),
            })
        );
        assert_eq!(
            sse.next_message(),
            Some(SseMessage {
                event: None,
                data: "plain".into(),
                id: None,
            })
        );
        assert_eq!(sse.take_retry(), Some(2500));
        assert_eq!(sse.take_retry(), None);
    }

    /// Pushes every chunk and keeps each event that comes out.
    fn events(chunks: &[&[u8]]) -> Vec<String> {
        let mut sse = Sse::default();
        let mut out = Vec::new();
        for chunk in chunks {
            sse.push(chunk);
            while let Some(data) = sse.next_event() {
                out.push(data);
            }
        }
        out
    }

    #[test]
    fn an_event_split_across_chunks() {
        let body = "event: content_block_delta\ndata: {\"text\":\"héllo\"}\n\n".as_bytes();
        // `é` is two bytes; cut between them, as well as mid-field and
        // between the two newlines.
        let accent = body.iter().position(|&byte| byte == 0xC3).expect("é");
        let cuts = [3, 20, accent + 1, body.len() - 1];

        let mut chunks = Vec::new();
        let mut from = 0;
        for cut in cuts {
            chunks.push(&body[from..cut]);
            from = cut;
        }
        chunks.push(&body[from..]);

        assert_eq!(events(&chunks), ["{\"text\":\"héllo\"}"]);
    }

    #[test]
    fn crlf_line_endings() {
        assert_eq!(
            events(&[b"event: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\n"]),
            ["{\"type\":\"ping\"}"]
        );
    }

    #[test]
    fn data_over_several_lines() {
        assert_eq!(
            events(&[b"data: first\ndata:second\ndata:  third\n\n"]),
            ["first\nsecond\n third"]
        );
    }

    #[test]
    fn comments_and_other_fields_are_ignored() {
        assert_eq!(
            events(&[b": keep-alive\nid: 7\nretry: 1000\nevent: x\ndata: a\n\n"]),
            ["a"]
        );
    }

    #[test]
    fn a_blank_line_with_no_data_dispatches_nothing() {
        assert_eq!(events(&[b": comment\n\n\nevent: x\n\ndata: b\n\n"]), ["b"]);
    }

    #[test]
    fn an_unfinished_event_waits_for_its_blank_line() {
        let mut sse = Sse::default();
        sse.push(b"data: a\n");
        assert_eq!(sse.next_event(), None);
        sse.push(b"\n");
        assert_eq!(sse.next_event().as_deref(), Some("a"));
    }
}

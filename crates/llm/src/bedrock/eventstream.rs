//! ```text
//! ┌────────────┬──────────────┬─────────────┬─────────┬─────────┬─────────────┐
//! │ total_len  │ headers_len  │ prelude_crc │ headers │ payload │ message_crc │
//! └────────────┴──────────────┴─────────────┴─────────┴─────────┴─────────────┘
//! ```

use zlogic_protocol::llm::LlmError;

use crate::error;

const PRELUDE_LEN: usize = 12;
const TRAILER_LEN: usize = 4;
const MAX_FRAME: u32 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventMessage {
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl EventMessage {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn event_type(&self) -> Option<&str> {
        self.header(":event-type")
    }

    pub fn message_type(&self) -> Option<&str> {
        self.header(":message-type")
    }

    pub fn is_exception(&self) -> bool {
        self.message_type() == Some("exception") || self.header(":exception-type").is_some()
    }
}

#[derive(Debug, Default)]
pub struct EventStreamDecoder {
    buf: Vec<u8>,
}

impl EventStreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<EventMessage>, LlmError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();

        loop {
            if self.buf.len() < PRELUDE_LEN {
                break;
            }
            let total = u32::from_be_bytes(self.buf[0..4].try_into().unwrap());
            let headers_len = u32::from_be_bytes(self.buf[4..8].try_into().unwrap());

            if total > MAX_FRAME
                || (total as usize) < PRELUDE_LEN + TRAILER_LEN
                || headers_len as usize > total as usize - PRELUDE_LEN - TRAILER_LEN
            {
                return Err(error::protocol(format!(
                    "malformed eventstream prelude (total={total}, headers={headers_len})"
                )));
            }
            if self.buf.len() < total as usize {
                break;
            }

            let frame: Vec<u8> = self.buf.drain(..total as usize).collect();
            let headers_end = PRELUDE_LEN + headers_len as usize;
            let headers = parse_headers(&frame[PRELUDE_LEN..headers_end])?;
            let payload = frame[headers_end..frame.len() - TRAILER_LEN].to_vec();
            out.push(EventMessage { headers, payload });
        }

        Ok(out)
    }

    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }
}

fn parse_headers(mut b: &[u8]) -> Result<Vec<(String, String)>, LlmError> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let name_len = b[0] as usize;
        if b.len() < 1 + name_len + 1 {
            return Err(error::protocol("truncated eventstream header"));
        }
        let name = String::from_utf8_lossy(&b[1..1 + name_len]).into_owned();
        let value_type = b[1 + name_len];
        b = &b[1 + name_len + 1..];

        let (value, consumed): (Option<String>, usize) = match value_type {
            0 | 1 => (None, 0), // bool true / false, no body
            2 => (None, 1),     // byte
            3 => (None, 2),     // short
            4 => (None, 4),     // integer
            5 => (None, 8),     // long
            6 | 7 => {
                if b.len() < 2 {
                    return Err(error::protocol("truncated eventstream header value"));
                }
                let len = u16::from_be_bytes([b[0], b[1]]) as usize;
                if b.len() < 2 + len {
                    return Err(error::protocol("truncated eventstream header value"));
                }
                let v = if value_type == 7 {
                    Some(String::from_utf8_lossy(&b[2..2 + len]).into_owned())
                } else {
                    None
                };
                (v, 2 + len)
            }
            8 => (None, 8),  // timestamp
            9 => (None, 16), // uuid
            other => {
                return Err(error::protocol(format!(
                    "unknown eventstream header value type {other}"
                )));
            }
        };

        if b.len() < consumed {
            return Err(error::protocol("truncated eventstream header value"));
        }
        b = &b[consumed..];
        if let Some(v) = value {
            out.push((name, v));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":event-type", event_type), (":message-type", "event")] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7); // string
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }

        let total = (PRELUDE_LEN + headers.len() + payload.len() + TRAILER_LEN) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // prelude crc
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload);
        out.extend_from_slice(&0u32.to_be_bytes()); // message crc
        out
    }

    #[test]
    fn decodes_a_single_frame() {
        let mut d = EventStreamDecoder::new();
        let msgs = d.push(&frame("contentBlockDelta", b"{\"x\":1}")).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].event_type(), Some("contentBlockDelta"));
        assert_eq!(msgs[0].payload, b"{\"x\":1}");
        assert!(!d.has_partial());
    }

    #[test]
    fn same_result_at_every_chunk_size() {
        let mut bytes = frame("messageStart", b"{}");
        bytes.extend_from_slice(&frame("contentBlockDelta", b"{\"a\":1}"));
        bytes.extend_from_slice(&frame("messageStop", b"{}"));

        for size in [1, 2, 3, 7, 16, 4096] {
            let mut d = EventStreamDecoder::new();
            let mut got = Vec::new();
            for c in bytes.chunks(size) {
                got.extend(d.push(c).unwrap());
            }
            assert_eq!(got.len(), 3, "chunk size {size}");
            assert_eq!(got[1].payload, b"{\"a\":1}");
            assert!(
                !d.has_partial(),
                "chunk size {size} left a partial frame behind"
            );
        }
    }

    #[test]
    fn truncated_frame_is_reported_as_partial() {
        let bytes = frame("messageStart", b"{}");
        let mut d = EventStreamDecoder::new();
        let got = d.push(&bytes[..bytes.len() - 3]).unwrap();
        assert!(got.is_empty());
        assert!(d.has_partial());
    }

    #[test]
    fn malformed_prelude_errors_instead_of_hanging() {
        let mut d = EventStreamDecoder::new();
        let bogus = [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(d.push(&bogus).is_err());
    }

    #[test]
    fn non_string_headers_are_skipped_not_fatal() {
        // [name_len][name][type=4 integer][4 bytes]
        let mut headers = vec![3u8];
        headers.extend_from_slice(b"num");
        headers.push(4);
        headers.extend_from_slice(&7i32.to_be_bytes());
        headers.push(11);
        headers.extend_from_slice(b":event-type");
        headers.push(7);
        headers.extend_from_slice(&2u16.to_be_bytes());
        headers.extend_from_slice(b"ok");

        let payload = b"{}";
        let total = (PRELUDE_LEN + headers.len() + payload.len() + TRAILER_LEN) as u32;
        let mut f = Vec::new();
        f.extend_from_slice(&total.to_be_bytes());
        f.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(&headers);
        f.extend_from_slice(payload);
        f.extend_from_slice(&0u32.to_be_bytes());

        let mut d = EventStreamDecoder::new();
        let msgs = d.push(&f).unwrap();
        assert_eq!(msgs[0].event_type(), Some("ok"));
    }
}

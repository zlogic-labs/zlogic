use eventsource_stream::{Event as RawEvent, Eventsource};
use futures_core::Stream;
use futures_util::StreamExt;

use crate::error;
use crate::transport::ByteStream;
use zlogic_protocol::llm::LlmError;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

impl From<RawEvent> for SseEvent {
    fn from(raw: RawEvent) -> Self {
        Self {
            event: (raw.event != "message").then_some(raw.event),
            data: raw.data,
        }
    }
}

pub fn events(bytes: ByteStream) -> impl Stream<Item = Result<SseEvent, LlmError>> {
    with_trailing_boundary(bytes)
        .eventsource()
        .map(|r| match r {
            Ok(ev) => Ok(SseEvent::from(ev)),
            Err(eventsource_stream::EventStreamError::Transport(e)) => Err(e),
            Err(other) => Err(error::protocol(format!("SSE parse failure: {other}"))),
        })
}

fn with_trailing_boundary(bytes: ByteStream) -> impl Stream<Item = Result<bytes::Bytes, LlmError>> {
    bytes.chain(futures_util::stream::iter([Ok(bytes::Bytes::from_static(
        b"\n\n",
    ))]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn collect(input: &str, chunk: usize) -> Vec<SseEvent> {
        let chunks: Vec<Result<bytes::Bytes, LlmError>> = input
            .as_bytes()
            .chunks(chunk.max(1))
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        let s = events(Box::pin(stream::iter(chunks)));
        futures_executor::block_on(s.map(|r| r.unwrap()).collect())
    }

    const SAMPLE: &str = "data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n";

    #[test]
    fn same_result_at_every_chunk_size() {
        let expect = ["{\"a\":1}", "{\"b\":2}", "[DONE]"];
        for size in [1, 2, 3, 5, 7, 13, 1000] {
            let got: Vec<String> = collect(SAMPLE, size).into_iter().map(|e| e.data).collect();
            assert_eq!(
                got, expect,
                "chunk size {size} parsed to a different result"
            );
        }
    }

    #[test]
    fn crlf_is_one_newline_at_every_chunk_size() {
        for size in [1, 2, 3, 4, 1000] {
            let got = collect("data: x\r\n\r\ndata: y\r\n\r\n", size);
            assert_eq!(got.len(), 2, "chunk size {size}");
            assert_eq!(got[0].data, "x");
            assert_eq!(got[1].data, "y");
        }
    }

    #[test]
    fn bare_cr_also_terminates_a_line() {
        let got = collect("data: x\r\rdata: y\r\r", 1);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, "x");
    }

    #[test]
    fn event_field_is_captured() {
        let got = collect("event: content_block_delta\ndata: {}\n\n", 3);
        assert_eq!(got[0].event.as_deref(), Some("content_block_delta"));
        assert_eq!(got[0].data, "{}");
    }

    #[test]
    fn a_frame_without_an_event_field_stays_none() {
        let got = collect("data: {}\n\n", 1);
        assert_eq!(got[0].event, None);
    }

    #[test]
    fn multiline_data_joins_with_newline() {
        let got = collect("data: line1\ndata: line2\n\n", 4);
        assert_eq!(got[0].data, "line1\nline2");
    }

    #[test]
    fn comments_are_ignored() {
        let got = collect(": ping\n\ndata: real\n\n", 2);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, "real");
    }

    #[test]
    fn trailing_event_without_blank_line_is_still_delivered() {
        let got = collect("data: tail", 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, "tail");
    }

    #[test]
    fn the_appended_boundary_does_not_invent_an_event() {
        assert_eq!(collect("data: x\n\n", 1).len(), 1);
        assert_eq!(collect("", 1).len(), 0);
        assert_eq!(collect("\n\n\n\n", 1).len(), 0);
    }

    #[test]
    fn value_keeps_inner_colons_and_spaces() {
        let got = collect("data: {\"url\": \"https://x.test/a\"}\n\n", 1);
        assert_eq!(got[0].data, "{\"url\": \"https://x.test/a\"}");
    }

    #[test]
    fn done_sentinel() {
        assert!(
            SseEvent {
                event: None,
                data: "[DONE]".into()
            }
            .is_done()
        );
        assert!(
            SseEvent {
                event: None,
                data: " [DONE] ".into()
            }
            .is_done()
        );
    }
}

//! Turning a pipe's bytes into text when nobody said what they are.
//! Two pipes come out of a process and they do not agree on what a byte string means. A
//! Unix-like tool writes UTF-8; a Windows console program writes its localized messages in the
//! console code page, which on a Chinese system is 936 (GBK). Nothing on the wire announces
//! which, and reading the stream as UTF-8 anyway is not a cosmetic error: GBK's lead bytes start
//! at `0x81` and UTF-8's two-byte sequences start at `0xC2`, so the ranges overlap from `C2` to
//! `DF` and a GBK character in that range is *also* a valid UTF-8 sequence. The reader cannot
//! tell "wrong bytes" from "different bytes" one character at a time, and it will confidently
//! print the wrong characters if it guesses.
//! So the encoding is **judged from the bytes, once per stream, and then held to**. Judging
//! needs a window rather than the first byte that fails, for exactly that reason: a prefix that
//! happens to be valid UTF-8 says nothing about what follows it. The window closes on whichever
//! comes first — enough bytes, a quiet moment, or end of input — because a three-line error
//! message never fills a window, and a command that printed its banner and is now waiting on
//! something must not sit in the buffer.
//! The verdict is [`decode_text`]'s, the same one a file gets, so a `.log` and a pipe carrying
//! the same bytes land on the same answer. What this adds is the part a file never had to deal
//! with: **a read can stop anywhere**, including inside a character, and decoding half a
//! character prints a replacement character into output that was entirely valid. That is not a
//! Windows problem — it has been here for UTF-8 all along, and it shows up a handful of times in
//! a hundred megabytes of output.

use std::time::Duration;

use encoding_rs::GB18030;
use tokio::time::Instant;

use crate::file::convert::decode_text;

/// How much an undecided stream collects before it must reach a verdict.
const PROBE_BYTES: usize = 8 * 1024;

/// How long an undecided stream may sit on what it has before it must reach a verdict.
const PROBE_IDLE: Duration = Duration::from_millis(250);

/// The encoding a stream's bytes are read as, fixed once the bytes have been judged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Gb18030,
    /// Nothing claimed these bytes as text. Kept because a stream is not a file: the alternative
    /// is showing no output at all, and replacement characters around whatever ASCII structure
    /// is in there is the more useful of the two.
    Lossy,
}

/// One pipe's bytes, on their way to text.
pub(crate) struct StreamDecoder {
    /// Bytes not yet emitted: either the whole window, while the encoding is undecided, or the
    /// tail of a character the next read completes.
    pending: Vec<u8>,
    encoding: Option<Encoding>,
    /// When the last bytes arrived. An undecided stream is released after [`PROBE_IDLE`] of
    /// quiet rather than only when its window fills, so a command that prints a line and then
    /// waits is not held behind a buffer meant for a long log.
    arrived: Option<Instant>,
}

impl StreamDecoder {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
            encoding: None,
            arrived: None,
        }
    }

    /// When an undecided stream has to be forced to a verdict, or `None` when it is holding
    /// nothing or already has one.
    pub(crate) fn probe_deadline(&self) -> Option<Instant> {
        if self.encoding.is_some() || self.pending.is_empty() {
            return None;
        }
        self.arrived.map(|at| at + PROBE_IDLE)
    }

    /// Releases what is held, if it has been quiet long enough. Driven by the caller's tick
    /// rather than a timer of its own, so the read loop keeps a single clock.
    pub(crate) fn flush_if_idle(&mut self, now: Instant) -> String {
        match self.probe_deadline() {
            Some(at) if now >= at => self.flush(),
            _ => String::new(),
        }
    }

    /// Feeds bytes read from the pipe. Returns the text to show, which is empty while the
    /// encoding is still being judged and while only a partial character is in hand.
    pub(crate) fn push(&mut self, bytes: &[u8], now: Instant) -> String {
        self.pending.extend_from_slice(bytes);
        if self.encoding.is_none() {
            self.arrived = Some(now);
            if self.pending.len() < PROBE_BYTES {
                return String::new();
            }
        }
        self.drain(false)
    }

    /// Releases everything, judging whatever was still undecided. The last call: end of input is
    /// evidence too, and a three-line error message never reaches [`PROBE_BYTES`].
    pub(crate) fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        self.drain(true)
    }

    fn drain(&mut self, last: bool) -> String {
        if self.encoding.is_none() {
            self.encoding = Some(judge(&self.pending));
        }
        let encoding = self.encoding.unwrap_or(Encoding::Lossy);
        let keep = held(encoding, &self.pending, last);
        let cut = self.pending.len() - keep;
        let text = decode_with(encoding, &self.pending[..cut]);
        if encoding == Encoding::Utf8 && std::str::from_utf8(&self.pending[..cut]).is_err() {
            // A stream that was valid UTF-8 for its whole window and is not any more. The
            // encoding does not change mid-stream — text already on the console would mean
            // something different under a second reading — so the rest is decoded the way it
            // was before this existed.
            self.encoding = Some(Encoding::Lossy);
        }
        self.pending.drain(..cut);
        text
    }
}

/// The encoding for a stream, from the same verdict a file gets: UTF-8 if the bytes are UTF-8, a
/// BOM'd UTF-16 if that is what they are, GB18030 if they are valid Chinese text, lossy UTF-8
/// for anything else.
///
/// The **whole** window is judged at once, never up to the first byte that fails. A GBK
/// character whose lead byte falls in `C2..DF` is a legal two-byte UTF-8 sequence, so a reader
/// that commits at the first hard error has already printed the characters before it under the
/// wrong reading and cannot take them back.
fn judge(bytes: &[u8]) -> Encoding {
    match decode_text(bytes) {
        Some(d) if d.converted && d.encoding == "UTF-16LE" => Encoding::Utf16Le,
        Some(d) if d.converted && d.encoding == "UTF-16BE" => Encoding::Utf16Be,
        // Whatever else counts as converted is the legacy Chinese encoding; keying on the flag
        // rather than the name means a fourth such encoding still lands somewhere true.
        Some(d) if d.converted => Encoding::Gb18030,
        Some(_) => Encoding::Utf8,
        None => Encoding::Lossy,
    }
}

/// Bytes at the end of `buf` that start a character the next read will finish, and so must not be
/// decoded yet. A read stops wherever the pipe stops, and half a character decoded eagerly
/// becomes a replacement character in output that was entirely valid.
fn held(encoding: Encoding, buf: &[u8], last: bool) -> usize {
    if last {
        return 0;
    }
    match encoding {
        // `error_len() == None` is UTF-8's own statement that the input ended inside a character
        // rather than at something unreadable — the only case where waiting helps.
        Encoding::Utf8 => match std::str::from_utf8(buf) {
            Ok(_) => 0,
            Err(e) if e.error_len().is_none() => buf.len() - e.valid_up_to(),
            Err(_) => 0,
        },
        Encoding::Utf16Le | Encoding::Utf16Be => buf.len() % 2,
        Encoding::Gb18030 => gb18030_held(buf),
        // Nothing is ever half-finished under a decoder that substitutes rather than fails.
        Encoding::Lossy => 0,
    }
}

/// The GB18030 equivalent. A character is one byte below `0x81`, two bytes up to `0xFE`, or four
/// when the second byte is a digit — the only form with a three-byte prefix. Walking from the
/// front is what makes this exact; reading the last byte cannot, because `0x81` is the first byte
/// of all three lengths.
fn gb18030_held(buf: &[u8]) -> usize {
    let mut at = 0;
    while at < buf.len() {
        // A byte up to `0x80` stands alone: ASCII, and `0x80` itself, which GB18030 maps to the
        // euro sign.
        let len = if buf[at] <= 0x80 {
            1
        } else if buf.get(at + 1).is_some_and(|b| (0x30..=0x39).contains(b)) {
            4
        } else {
            2
        };
        if at + len > buf.len() {
            return buf.len() - at;
        }
        at += len;
    }
    0
}

fn decode_with(encoding: Encoding, bytes: &[u8]) -> String {
    match encoding {
        Encoding::Utf8 | Encoding::Lossy => String::from_utf8_lossy(bytes).into_owned(),
        Encoding::Utf16Le => utf16(bytes, u16::from_le_bytes),
        Encoding::Utf16Be => utf16(bytes, u16::from_be_bytes),
        // `decode` is the substituting one: what is malformed in a stream that is otherwise
        // text should show as a replacement, not stop the rest of the line from being read.
        Encoding::Gb18030 => GB18030.decode(bytes).0.into_owned(),
    }
}

fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| unit([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gbk(text: &str) -> Vec<u8> {
        GB18030.encode(text).0.into_owned()
    }

    /// One byte per read, which is the harshest thing a pipe can do to a decoder.
    fn feed(decoder: &mut StreamDecoder, bytes: &[u8]) -> String {
        let now = Instant::now();
        let mut text = String::new();
        for byte in bytes {
            text.push_str(&decoder.push(&[*byte], now));
        }
        text.push_str(&decoder.flush());
        text
    }

    #[test]
    fn reads_a_gbk_stream_one_byte_at_a_time() {
        let text = "错误: 只有加上 /F 才能强制终止。\n";
        let mut decoder = StreamDecoder::new();
        assert_eq!(feed(&mut decoder, &gbk(text)), text);
    }

    #[test]
    fn holds_a_character_split_across_two_reads() {
        let mut decoder = StreamDecoder::new();
        let now = Instant::now();
        // The window closes here, so everything after it is decoded under a fixed verdict.
        assert_eq!(
            decoder.push(&vec![b'a'; PROBE_BYTES], now).len(),
            PROBE_BYTES
        );
        // "→" is three bytes and the read stopped two of them in.
        assert_eq!(decoder.push(b"\xe2\x86", now), "");
        assert_eq!(decoder.push(b"\x92b", now), "→b");
    }

    #[test]
    fn judges_a_window_that_a_single_split_could_hide() {
        // 0xC4 0xA1 is a GBK character *and* the UTF-8 for "С". Split between them, a reader
        // that judged up to the first bad byte would have committed to UTF-8 and lost the text.
        let mut raw = vec![b'a'; 1024];
        raw.extend(gbk("错误"));
        raw.extend(vec![b'a'; PROBE_BYTES]);
        let mut decoder = StreamDecoder::new();
        let now = Instant::now();
        let mut text = String::new();
        for chunk in [&raw[..1025], &raw[1025..]] {
            text.push_str(&decoder.push(chunk, now));
        }
        text.push_str(&decoder.flush());
        assert!(text.contains("错误"), "got {text:?}");
    }

    #[test]
    fn falls_back_when_the_bytes_are_not_text() {
        let mut raw = vec![b'a'; PROBE_BYTES];
        raw.extend_from_slice(&[0x00, 0xff, 0xfe, 0x00]);
        let text = feed(&mut StreamDecoder::new(), &raw);
        assert!(
            text.contains('\u{fffd}'),
            "expected replacements, got {text:?}"
        );
    }
}

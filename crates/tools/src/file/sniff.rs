//! What a file's leading bytes say it is, next to what its name says.
//!
//! The name is the cheap answer, and the only one available for formats that share a container
//! (`.xlsx` / `.docx` / `.ods` are all `PK\x03\x04`; a remote attachment's bytes are on another
//! machine). But names lie often enough that the bytes get the last word whenever they
//! identify something: a `.log` that is really a PNG, a `Dockerfile` with no extension at all,
//! a GB18030 log that no UTF-8 decoder will accept.
//!
//! One implementation, shared by every caller that has to answer "what is this file":
//! `read_file`'s classification, the workspace file API's MIME, and the desktop shell's
//! attachment previews. Each of those used to carry its own extension table, and a table cannot
//! see the bytes.

use std::io::Read;
use std::path::Path;

use crate::display::ToolDisplay;
use crate::file::convert::decode_text;

/// How many leading bytes a verdict may rest on.
///
/// Long enough for every signature below and for the encoding heuristics to have something to
/// chew on; short enough that identifying a file for its preview costs nothing next to opening it.
pub const SNIFF_BYTES: usize = 4096;

/// What the leading bytes say the file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    /// Readable as text, in this encoding. The text itself is not here: this is a verdict about
    /// the kind of file, not a reading of it. The label is an IANA charset name, the form
    /// `TextDecoder` takes.
    Text(&'static str),
    /// A format the bytes identify. The MIME wins over whatever the name claims.
    Binary(&'static str),
    /// Neither text nor a signature anyone knows. A name may still identify the file, but
    /// nothing may call it text: a binary shown as text is a screen of control characters.
    Unknown,
}

/// The first [`SNIFF_BYTES`] bytes of a file, or as many as it has.
pub fn read_head(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut head = Vec::new();
    file.take(SNIFF_BYTES as u64).read_to_end(&mut head)?;
    Ok(head)
}

/// Identifies a file from its first bytes.
pub fn sniff(head: &[u8]) -> Content {
    let head = head.get(..head.len().min(SNIFF_BYTES)).unwrap_or(&[]);
    if let Some(mime) = binary_mime(head) {
        return Content::Binary(mime);
    }
    match text_encoding(head) {
        Some(encoding) => Content::Text(encoding),
        None => Content::Unknown,
    }
}

/// The MIME to show the UI for this file, or `None` when neither the name nor the bytes say.
///
/// The name goes first for everything the bytes cannot separate; the bytes go first wherever
/// they can. `Dockerfile`, `.log` and a mislabelled `.png` are the cases this exists for.
pub fn detect_mime(name: &str, head: &[u8]) -> Option<&'static str> {
    let by_name = ToolDisplay::guess_mime(name);
    match sniff(head) {
        Content::Binary(mime) => Some(match by_name {
            Some(named) if refines(mime, named) => named,
            _ => mime,
        }),
        // A `.txt` that is really a workbook's zip would render as text if the name won here.
        Content::Text(_) => Some(match by_name {
            Some(named) if is_text(named) => named,
            _ => "text/plain",
        }),
        Content::Unknown => by_name.filter(|named| !is_text(named)),
    }
}

/// Names that only make sense as a label for text.
///
/// `image/svg+xml` is text by its bytes but must keep the image label: the file server
/// hands this verdict to the browser as `Content-Type`, and a `text/plain` on an `.svg`
/// is exactly what makes every renderer refuse it.
fn is_text(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/toml"
                | "application/sql"
                | "application/javascript"
                | "application/x-ndjson"
                | "image/svg+xml"
        )
}

/// The cases where the name is more precise than the bytes: containers that hold several
/// formats, and containers used by both audio and video.
fn refines(magic: &str, named: &str) -> bool {
    // `PK\x03\x04` is a workbook, a document and a plain archive alike; only the name can say
    // which member this is.
    (magic == "application/zip" && is_zip_member(named))
        // Ogg and Matroska carry audio or video depending on the codecs, not on the container.
        || ((magic == "audio/ogg" || magic == "video/x-matroska")
            && (named.starts_with("audio/") || named.starts_with("video/")))
}

fn is_zip_member(mime: &str) -> bool {
    mime.contains("openxmlformats")
        || mime.contains("opendocument")
        || mime.contains("msword")
        || mime.contains("ms-excel")
        || mime.contains("ms-powerpoint")
}

/// The encoding the bytes read as, when they read as text at all.
fn text_encoding(head: &[u8]) -> Option<&'static str> {
    let probe = trim_partial_tail(head);
    // The BOM comes first: UTF-16 ASCII is nothing but NUL bytes between ASCII ones, and
    // only the BOM says that is text rather than a binary. Same order as `decode_text`,
    // which reads the same way.
    if let Some(encoding) = bom_encoding(probe) {
        return Some(encoding);
    }
    // Past that point a NUL byte is the signal `grep` and `git` use for "not text", and the
    // one `decode_text` applies before its GB18030 fallback. `"\0\0\0\0"` decodes as UTF-8
    // perfectly well, which is exactly why it is not a verdict.
    if probe.contains(&0) {
        return None;
    }
    let decoded = decode_text(probe)?;
    Some(match decoded.encoding {
        "UTF-16LE" => "utf-16le",
        "UTF-16BE" => "utf-16be",
        "GB18030" => "gb18030",
        _ => "utf-8",
    })
}

fn bom_encoding(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\xef\xbb\xbf") {
        Some("utf-8")
    } else if head.starts_with(b"\xff\xfe") {
        Some("utf-16le")
    } else if head.starts_with(b"\xfe\xff") {
        Some("utf-16be")
    } else {
        None
    }
}

/// Drops a multi-byte sequence the probe boundary cut in half.
///
/// The probe is a window, not the file: without this, the lead byte of a 3-byte character
/// left alone at the end turns a UTF-8 file into something only GB18030 will accept, and the
/// reported encoding would be wrong for a file that is not wrong at all.
fn trim_partial_tail(head: &[u8]) -> &[u8] {
    if head.len() < SNIFF_BYTES {
        return head;
    }
    let mut lead = head.len() - 1;
    while lead > 0 && head[lead] & 0b1100_0000 == 0b1000_0000 {
        lead -= 1;
    }
    if head[lead] >= 0b1100_0000 {
        return &head[..lead];
    }
    head
}

/// The MIME of a format the bytes identify, or `None` for text and for signatures nobody here
/// knows. Order matters only where a prefix is a prefix of another (`ID3`, `OggS`).
fn binary_mime(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if head.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if head.starts_with(b"RIFF") {
        return match head.get(8..12) {
            Some(b"WEBP") => Some("image/webp"),
            Some(b"WAVE") => Some("audio/wav"),
            Some(b"AVI ") => Some("video/x-msvideo"),
            _ => None,
        };
    }
    // `BM` alone is not a signature — every English text file that opens with two capital
    // letters has it. The DIB header size right after the file header is what actually holds.
    if head.starts_with(b"BM")
        && let Some(dib) = head.get(14..18)
        && [12u32, 40, 52, 56, 64, 108, 124]
            .contains(&u32::from_le_bytes(dib.try_into().expect("four bytes")))
    {
        return Some("image/bmp");
    }
    if head.starts_with(b"\x00\x00\x01\x00") {
        return Some("image/x-icon");
    }
    if head.get(4..8) == Some(b"ftyp") {
        return Some(match head.get(8..12) {
            Some(b"avif") | Some(b"avis") => "image/avif",
            Some(b"heic") | Some(b"heix") | Some(b"hevc") | Some(b"mif1") => "image/heic",
            Some(b"qt  ") => "video/quicktime",
            _ => "video/mp4",
        });
    }
    if head.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    if head.starts_with(b"PK\x03\x04")
        || head.starts_with(b"PK\x05\x06")
        || head.starts_with(b"PK\x07\x08")
    {
        return Some("application/zip");
    }
    if head.starts_with(b"\x1f\x8b") {
        return Some("application/gzip");
    }
    if head.starts_with(b"BZh") {
        return Some("application/x-bzip2");
    }
    if head.starts_with(b"\xfd7zXZ\x00") {
        return Some("application/x-xz");
    }
    if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Some("application/x-7z-compressed");
    }
    if head.starts_with(b"Rar!\x1a\x07") {
        return Some("application/vnd.rar");
    }
    if head.starts_with(b"\x7fELF") {
        return Some("application/x-executable");
    }
    if head.starts_with(b"MZ") {
        return Some("application/vnd.microsoft.portable-executable");
    }
    if head.starts_with(b"fLaC") {
        return Some("audio/flac");
    }
    if head.starts_with(b"ID3") || head.starts_with(b"\xff\xfb") || head.starts_with(b"\xff\xf3") {
        return Some("audio/mpeg");
    }
    if head.starts_with(b"OggS") {
        return Some("audio/ogg");
    }
    if head.starts_with(b"\x1a\xe4\xdf\xa3") {
        return Some("video/x-matroska");
    }
    if head.starts_with(b"FLV\x01") {
        return Some("video/x-flv");
    }
    if head.starts_with(b"\x00\x00\x01\xba") || head.starts_with(b"\x00\x00\x01\xb3") {
        return Some("video/mpeg");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::convert::test_support::PNG_1X1;

    #[test]
    fn signatures_beat_names() {
        assert_eq!(detect_mime("notes.txt", PNG_1X1), Some("image/png"));
        assert_eq!(
            detect_mime("capture", &[0x00, 0x00, 0x01, 0xba, 0, 0, 0, 0]),
            Some("video/mpeg")
        );
    }

    #[test]
    fn the_name_breaks_ties_the_bytes_cannot() {
        // Every OOXML document is the same zip; only the name says which one this is.
        assert_eq!(
            detect_mime("book.xlsx", b"PK\x03\x04rest"),
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        );
        assert_eq!(
            detect_mime("bundle.zip", b"PK\x03\x04rest"),
            Some("application/zip")
        );
    }

    #[test]
    fn text_carries_its_encoding_and_survives_a_probe_boundary() {
        assert_eq!(sniff(b"INFO started\n"), Content::Text("utf-8"));
        assert_eq!(
            sniff(b"\xff\xfeh\x00i\x00"),
            Content::Text("utf-16le"),
            "UTF-16 ASCII is full of NUL bytes, and the BOM check runs before the NUL scan"
        );
        // A 3-byte character split by the probe window must not demote the file to GB18030.
        let mut head = vec![b'a'; SNIFF_BYTES - 1];
        head.extend_from_slice("é".as_bytes());
        assert_eq!(sniff(&head), Content::Text("utf-8"));
        assert_eq!(sniff(&[0xff, 0x00, 0xfe, 0x01]), Content::Unknown);
    }

    #[test]
    fn svg_keeps_the_image_label_its_bytes_cannot_give() {
        // The bytes are XML, so the name is the only place `image/svg+xml` survives — and
        // the file server turns this verdict into the browser's `Content-Type`.
        assert_eq!(
            detect_mime("icon.svg", b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            Some("image/svg+xml")
        );
        // Bytes still win wherever they can tell the two apart.
        assert_eq!(detect_mime("icon.svg", PNG_1X1), Some("image/png"));
    }

    #[test]
    fn a_binary_never_becomes_text_because_of_its_name() {
        assert_eq!(detect_mime("app.log", &[0x00, 0x01, 0x02, 0x03]), None);
        assert_eq!(detect_mime("archive.bin", &[0x00, 0x01, 0x02, 0x03]), None);
    }
}

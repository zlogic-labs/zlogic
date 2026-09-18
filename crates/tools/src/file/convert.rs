//! File-to-text conversion for `read_file`.
//! `read_file`'s contract: UTF-8 text comes back as-is, images are attached as media, and
//! everything else that is *representable as text* is converted. This module owns the
//! classification and the conversions, so the tool itself stays a thin dispatcher.
//! Every conversion here is **deterministic**: the same file and the same `ranges` always
//! produce the same lines, which is what lets `read_file`'s paging contract work on converted
//! text (a "line" is a table row or a docx paragraph, not a byte offset).

use std::io::Read;
use std::path::Path;

use calamine::{Data, Reader, open_workbook_auto};
use encoding_rs::{GB18030, UTF_16BE, UTF_16LE};
use quick_xml::Reader as XmlReader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use zip::ZipArchive;

/// Spreadsheet and delimited formats `read_file` converts to rows.
const TABLE_EXTENSIONS: &[&str] = &["xlsx", "xls", "xlsb", "ods", "csv", "tsv", "tab"];

/// Audio / video formats `read_file` captures for the UI instead of trying to read as text.
/// Extension only — like images, the bytes verify nothing further here; the UI preview (native
/// `<video>` / `<audio>` on the client) resolves the container from the data itself. `.mkv` /
/// `.mov` are codec roulette in a webview and often do not play, but listing them keeps the
/// capture path honest and the "open with system app" fallback reachable from the file card.
const MEDIA_EXTENSIONS: &[&str] = &[
    "mp3", "wav", "aiff", "aif", "aac", "flac", "ogg", "opus", "m4a", "mp4", "m4v", "webm", "mov",
    "mkv",
];

/// Input ceiling for converted files, in bytes.
/// A workbook cannot be streamed — calamine decodes whole worksheets — and a docx must be
/// unzipped first, so the on-disk size is the only honest proxy for the memory the conversion
/// takes.
pub(crate) const MAX_CONVERT_BYTES: u64 = 128 * 1024 * 1024;

/// Decompression ceiling for one docx part.
/// A tiny package can hide a huge XML payload (zip bomb), so `document.xml` is capped
/// separately from the package size.
const MAX_DOCX_PART_BYTES: u64 = 64 * 1024 * 1024;

/// How `read_file` should treat a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Plain text, possibly after transcoding (UTF-16 BOM / GB18030 → UTF-8).
    Text,
    /// Spreadsheet: cells converted to tab-separated rows, one row per line.
    Table,
    /// `.docx`: body paragraphs extracted, one paragraph per line.
    Docx,
    /// An image: attach as media instead of converting to text.
    Image(&'static str),
    /// An audio / video file: captured for the UI, never decoded to text.
    Media(&'static str),
    /// Not representable as text: refuse.
    Binary,
}

/// A plain-text file after decoding.
pub(crate) struct DecodedText {
    pub text: String,
    /// True when the bytes were re-encoded (UTF-16 BOM or GB18030) rather than already UTF-8.
    /// Such text is NOT byte-exact with the file — an `edit` on it will not round-trip.
    pub converted: bool,
    /// The encoding the bytes were read as; meaningful only when `converted`.
    pub encoding: &'static str,
}

/// Chooses the read strategy from the file's extension and its first bytes.
/// The extension drives the choice (the contract is "read by suffix"), and the magic bytes
/// verify the image case: a `.png` that is really a text file is read as text rather than
/// attached as media and then refused by the wire gate's declared-vs-actual MIME check.
pub(crate) fn classify(path: &Path, raw: &[u8]) -> Kind {
    let ext = extension(path);
    if let Some(mime) = detect_image_mime(raw) {
        return Kind::Image(mime);
    }
    match ext.as_str() {
        e if TABLE_EXTENSIONS.contains(&e) => Kind::Table,
        "docx" => Kind::Docx,
        // Audio / video: captured whole for the UI preview, never decoded to text. Checked
        // after the image-magic path so a misnamed image still reads as an image.
        e if MEDIA_EXTENSIONS.contains(&e) => Kind::Media(
            crate::ToolDisplay::guess_mime(&path.to_string_lossy())
                .unwrap_or("application/octet-stream"),
        ),
        // Image extensions whose bytes were not an image fall through here, like any other
        // unknown suffix: try plain text, and refuse only when no encoding decodes it.
        _ => match decode_text(raw) {
            Some(_) => Kind::Text,
            None => Kind::Binary,
        },
    }
}

/// Decodes bytes as plain text.
/// Order matters:
/// 1. strict UTF-8 (byte-exact, a UTF-8 BOM is preserved as today);
/// 2. UTF-16 with a BOM — checked **before** the NUL scan, because UTF-16 ASCII text is full
///    of NUL bytes by construction;
/// 3. GB18030 — covers GBK/GB2312, the common legacy encoding for Chinese text files. A NUL
///    byte marks real binary (no readable text encoding emits one), where a lucky GB18030
///    decode would be mojibake, not a file.
pub(crate) fn decode_text(raw: &[u8]) -> Option<DecodedText> {
    if let Ok(text) = std::str::from_utf8(raw) {
        return Some(DecodedText {
            text: text.to_owned(),
            converted: false,
            encoding: "UTF-8",
        });
    }
    if let Some(rest) = raw.strip_prefix(b"\xff\xfe") {
        return decode_utf16(UTF_16LE, rest, "UTF-16LE");
    }
    if let Some(rest) = raw.strip_prefix(b"\xfe\xff") {
        return decode_utf16(UTF_16BE, rest, "UTF-16BE");
    }
    if raw.contains(&0) {
        return None;
    }
    let (text, _, had_errors) = GB18030.decode(raw);
    (!had_errors).then(|| DecodedText {
        text: text.into_owned(),
        converted: true,
        encoding: "GB18030",
    })
}

fn decode_utf16(
    encoding: &'static encoding_rs::Encoding,
    bytes: &[u8],
    name: &'static str,
) -> Option<DecodedText> {
    let (text, _, had_errors) = encoding.decode(bytes);
    (!had_errors).then(|| DecodedText {
        text: text.into_owned(),
        converted: true,
        encoding: name,
    })
}

/// The visual MIME of `raw`, from its magic bytes. `None` when the bytes are not a
/// PNG/JPEG/GIF/WebP image.
pub(crate) fn detect_image_mime(raw: &[u8]) -> Option<&'static str> {
    if raw.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if raw.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if raw.starts_with(b"GIF87a") || raw.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if raw.len() >= 12 && &raw[..4] == b"RIFF" && &raw[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// One sheet of a table file: the header row plus the data rows below it.
pub(crate) struct SheetDump {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// A whole table file. Delimited files have exactly one sheet; workbooks have every sheet
/// calamine reports.
pub(crate) struct TableDump {
    pub sheets: Vec<SheetDump>,
}

/// Decodes a table file into cells.
/// `raw` is needed only for delimited files, whose bytes may be in a legacy encoding; workbooks
/// are opened by path through calamine.
pub(crate) fn table_dump(path: &Path, raw: &[u8]) -> Result<TableDump, String> {
    match extension(path).as_str() {
        "csv" => csv_dump(raw, b',', "csv"),
        "tsv" | "tab" => csv_dump(raw, b'\t', "tsv"),
        _ => workbook_dump(path),
    }
}

/// A delimited file parsed with the real CSV rules (quotes, embedded delimiters), so the header
/// and rows agree with what a spreadsheet program would show.
fn csv_dump(raw: &[u8], delimiter: u8, name: &'static str) -> Result<TableDump, String> {
    let decoded = decode_text(raw)
        .ok_or_else(|| "file is not readable text in any supported encoding".to_string())?;
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(decoded.text.as_bytes());
    let mut all: Vec<Vec<String>> = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|error| format!("cannot parse delimited row: {error}"))?;
        let cells: Vec<String> = record.iter().map(clean_cell).collect();
        // Rows that are entirely empty add nothing but shape noise to a preview.
        if cells.iter().any(|cell| !cell.is_empty()) {
            all.push(cells);
        }
    }
    let mut all = all.into_iter();
    let columns = all.next().unwrap_or_default();
    Ok(TableDump {
        sheets: vec![SheetDump {
            name: name.to_string(),
            columns,
            rows: all.collect(),
        }],
    })
}

fn workbook_dump(path: &Path) -> Result<TableDump, String> {
    let size = file_size(path)?;
    if size > MAX_CONVERT_BYTES {
        return Err(format!(
            "workbook is {size} bytes; the conversion limit is {MAX_CONVERT_BYTES}"
        ));
    }
    let mut workbook = open_workbook_auto(path)
        .map_err(|error| format!("cannot open workbook {}: {error}", path.display()))?;
    let sheets = workbook.sheet_names();
    if sheets.is_empty() {
        return Err("workbook contains no worksheets".to_string());
    }
    let mut out = Vec::new();
    for sheet in sheets {
        let range = workbook
            .worksheet_range(&sheet)
            .map_err(|error| format!("cannot read worksheet {sheet:?}: {error}"))?;
        let mut all: Vec<Vec<String>> = Vec::new();
        for row_index in 0..range.height() {
            let mut cells = Vec::new();
            let mut non_empty = false;
            for column in 0..range.width() {
                let text = cell_text(range.get((row_index, column)));
                non_empty |= !text.is_empty();
                cells.push(text);
            }
            if non_empty {
                all.push(cells);
            }
        }
        let mut all = all.into_iter();
        let columns = all.next().unwrap_or_default();
        out.push(SheetDump {
            name: sheet,
            columns,
            rows: all.collect(),
        });
    }
    Ok(TableDump { sheets: out })
}

/// A delimited-file cell as readable text. Newlines and tabs are flattened to spaces, the same
/// rule as workbook cells, so one row keeps fitting on one line.
fn clean_cell(cell: &str) -> String {
    cell.replace(['\n', '\r', '\t'], " ")
}

/// One cell of a workbook as readable text.
/// Newlines and tabs inside a cell are flattened to spaces: the conversion's shape contract is
/// one row per line, and a cell that smuggles in a newline would fake a row boundary.
fn cell_text(cell: Option<&Data>) -> String {
    match cell {
        None | Some(Data::Empty) => String::new(),
        Some(Data::Bool(value)) => value.to_string(),
        Some(Data::Int(value)) => value.to_string(),
        Some(Data::Float(value)) => format_float(*value),
        Some(Data::String(value))
        | Some(Data::DateTimeIso(value))
        | Some(Data::DurationIso(value)) => value.replace(['\n', '\r', '\t'], " "),
        Some(Data::DateTime(value)) => value.to_string(),
        Some(Data::Error(value)) => format!("#ERROR:{value:?}"),
    }
}

/// A float that Excel stored as a whole number renders without the decimal point — `100` reads
/// better than the `100.0` typing noise the model would otherwise copy into an output table.
fn format_float(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

/// Extracts the body paragraphs of a `.docx` package, one paragraph per line.
/// Headers, footers, footnotes and images are deliberately not walked: the body text is what a
/// model can act on, and the package's other parts are where bombs and noise hide.
pub(crate) fn docx_to_text(path: &Path) -> Result<String, String> {
    let size = file_size(path)?;
    if size > MAX_CONVERT_BYTES {
        return Err(format!(
            "docx is {size} bytes; the conversion limit is {MAX_CONVERT_BYTES}"
        ));
    }
    let file = std::fs::File::open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| format!("cannot open docx package {}: {error}", path.display()))?;
    let entry = archive
        .by_name("word/document.xml")
        .map_err(|error| format!("not a docx (word/document.xml is missing): {error}"))?;
    let mut xml = Vec::new();
    entry
        .take(MAX_DOCX_PART_BYTES + 1)
        .read_to_end(&mut xml)
        .map_err(|error| format!("cannot read document.xml: {error}"))?;
    if xml.len() as u64 > MAX_DOCX_PART_BYTES {
        return Err(format!(
            "document.xml exceeds the {} byte decompression limit",
            MAX_DOCX_PART_BYTES
        ));
    }
    extract_docx_paragraphs(&xml)
}

fn extract_docx_paragraphs(xml: &[u8]) -> Result<String, String> {
    let mut reader = XmlReader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => match local_name(event.name().as_ref()) {
                "p" => current.clear(),
                "t" => in_text = true,
                "tab" => current.push('\t'),
                "br" | "cr" => current.push('\n'),
                _ => {}
            },
            Ok(Event::Empty(event)) => match local_name(event.name().as_ref()) {
                "tab" => current.push('\t'),
                "br" | "cr" => current.push('\n'),
                _ => {}
            },
            Ok(Event::End(event)) => match local_name(event.name().as_ref()) {
                "p" => {
                    paragraphs.push(std::mem::take(&mut current));
                }
                "t" => in_text = false,
                _ => {}
            },
            Ok(Event::Text(text)) if in_text => {
                // Reader-produced text events are already unescaped; `into_inner` hands the
                // content over without a copy.
                current.push_str(text.into_inner().as_ref());
            }
            Ok(Event::GeneralRef(reference)) if in_text => {
                // `&amp;` and friends arrive as their own event, `&`- and `;`-stripped.
                if reference.is_char_ref() {
                    if let Ok(Some(character)) = reference.resolve_char_ref() {
                        current.push(character);
                    }
                } else if let Some(entity) = resolve_predefined_entity(reference.as_ref()) {
                    current.push_str(entity);
                } else {
                    // Unknown entity: keep it visible rather than silently dropping content.
                    current.push('&');
                    current.push_str(reference.as_ref());
                    current.push(';');
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(format!("cannot parse document.xml: {error}"));
            }
        }
    }
    Ok(paragraphs.join("\n") + "\n")
}

/// The local part of an element name: `w:p` and `p` both classify as paragraph.
fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn file_size(path: &Path) -> Result<u64, String> {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::GB18030;

    use test_support::{PNG_1X1, write_docx, write_xlsx};

    #[test]
    fn utf8_is_byte_exact_and_not_marked_converted() {
        let decoded = decode_text("héllo\n".as_bytes()).unwrap();
        assert_eq!(decoded.text, "héllo\n");
        assert!(!decoded.converted);
    }

    #[test]
    fn utf16_with_bom_decodes_even_though_it_is_full_of_nuls() {
        // "hi\n" as UTF-16LE, BOM included — every ASCII byte is followed by a NUL.
        let bytes = [0xff, 0xfe, b'h', 0x00, b'i', 0x00, b'\n', 0x00];
        let decoded = decode_text(&bytes).unwrap();
        assert_eq!(decoded.text, "hi\n");
        assert!(decoded.converted);
        assert_eq!(decoded.encoding, "UTF-16LE");
    }

    #[test]
    fn gb18030_text_decodes() {
        let (bytes, _, _) = GB18030.encode("名称,价格\n苹果,3\n");
        let decoded = decode_text(&bytes).unwrap();
        assert!(decoded.converted);
        assert!(decoded.text.contains("名称,价格"));
        assert!(decoded.text.contains("苹果,3"));
    }

    /// The NUL scan must come after the UTF-16 BOM check (see decode_text ordering), and binary
    /// without a NUL but with invalid GB18030 is still refused.
    #[test]
    fn binary_is_refused() {
        assert!(decode_text(&[0xff, 0x00, 0xfe, 0x01]).is_none());
    }

    #[test]
    fn image_magic_beats_extension_lies() {
        let path = Path::new("logo.png");
        assert_eq!(classify(path, PNG_1X1), Kind::Image("image/png"));
        // A "png" that is really text is read as text, not attached as a broken image.
        assert_eq!(classify(path, b"<svg>not actually png</svg>"), Kind::Text);
    }

    #[test]
    fn extensions_drive_table_and_docx_classification() {
        assert_eq!(classify(Path::new("a.xlsx"), b"PK\x03\x04"), Kind::Table);
        assert_eq!(classify(Path::new("a.xls"), b"PK\x03\x04"), Kind::Table);
        assert_eq!(classify(Path::new("a.ods"), b"PK\x03\x04"), Kind::Table);
        // Delimited files are tables too: they share the preview contract (header + rows).
        assert_eq!(classify(Path::new("a.csv"), b"a,b\n"), Kind::Table);
        assert_eq!(classify(Path::new("a.tsv"), b"a\tb\n"), Kind::Table);
        assert_eq!(classify(Path::new("a.docx"), b"PK\x03\x04"), Kind::Docx);
        // Invalid UTF-8, no BOM prefix, and a NUL byte: nothing decodes it as text.
        assert_eq!(
            classify(Path::new("b.bin"), &[0xff, 0x00, 0xfe, 0x01]),
            Kind::Binary
        );
    }

    #[test]
    fn a_workbook_becomes_rows_with_a_sheet_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        write_xlsx(&path, "Data", &[&["name", "price"], &["apple", "3"]]);
        let raw = std::fs::read(&path).unwrap();

        // Small workbook without ranges is returned whole, inline, under a one-line header and
        // with the sheet name in the range marker; the header row comes first.
        let out =
            crate::file::parser::render(&path, "book.xlsx", &raw, None, 100_000, true).unwrap();
        assert!(
            out.content.starts_with("book.xlsx (1 sheet, "),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("--- Data!A1:B2 ---"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("name | price\napple | 3\n"),
            "{}",
            out.content
        );
    }

    /// csv is parsed with the real CSV rules (quotes, embedded delimiters), and its first row
    /// is the header, exactly like a one-sheet workbook.
    #[test]
    fn a_csv_becomes_one_sheet_with_a_header() {
        let raw = b"name,price\napple,3\n\"quoted, cell\",x\n";
        let dump = table_dump(Path::new("a.csv"), raw).unwrap();
        assert_eq!(dump.sheets.len(), 1);
        assert_eq!(dump.sheets[0].name, "csv");
        assert_eq!(dump.sheets[0].columns, vec!["name", "price"]);
        assert_eq!(
            dump.sheets[0].rows,
            vec![vec!["apple", "3"], vec!["quoted, cell", "x"]]
        );
        assert_eq!(dump.sheets[0].rows.len(), 2);
    }

    #[test]
    fn a_docx_becomes_one_line_per_paragraph() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memo.docx");
        write_docx(
            &path,
            "<w:p><w:r><w:t>First paragraph</w:t></w:r></w:p>\
             <w:p><w:r><w:t>Second &amp; line</w:t></w:r></w:p>",
        );

        let text = docx_to_text(&path).unwrap();
        assert_eq!(text, "First paragraph\nSecond & line\n");
    }
}

/// Test-only fixture builders, shared with `read_file`'s tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Write;
    use std::path::Path;

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    /// A 1x1 PNG, enough for magic-byte detection and header reads.
    pub const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
        0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE,
    ];

    /// Minimal xlsx package: inline strings, one sheet.
    pub fn write_xlsx(path: &Path, sheet: &str, rows: &[&[&str]]) {
        let file = std::fs::File::create(path).unwrap();
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut part = |name: &str, content: &str| {
            archive.start_file(name, options).unwrap();
            archive.write_all(content.as_bytes()).unwrap();
        };
        part(
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
        );
        part(
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
        );
        part(
            "xl/workbook.xml",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="{sheet}" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#
            ),
        );
        part(
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
        );
        let mut sheet_xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
        );
        for (row_index, row) in rows.iter().enumerate() {
            sheet_xml.push_str(&format!("<row r=\"{}\">", row_index + 1));
            for (column, value) in row.iter().enumerate() {
                let reference = format!("{}{}", xlsx_column_name(column), row_index + 1);
                sheet_xml.push_str(&format!(
                    "<c r=\"{reference}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{value}</t></is></c>"
                ));
            }
            sheet_xml.push_str("</row>");
        }
        sheet_xml.push_str("</sheetData></worksheet>");
        part("xl/worksheets/sheet1.xml", &sheet_xml);
        archive.finish().unwrap();
    }

    fn xlsx_column_name(mut index: usize) -> String {
        let mut reversed = Vec::new();
        index += 1;
        while index > 0 {
            index -= 1;
            reversed.push((b'A' + (index % 26) as u8) as char);
            index /= 26;
        }
        reversed.iter().rev().collect()
    }

    /// Minimal docx package: just the body part our extractor reads.
    pub fn write_docx(path: &Path, body: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        archive.start_file("word/document.xml", options).unwrap();
        archive
            .write_all(
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
                )
                .as_bytes(),
            )
            .unwrap();
        archive.finish().unwrap();
    }
}

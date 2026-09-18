//! Parser layer behind `read_file`.
//! Each file kind owns its range syntax and renders plain text: a one-line header naming the file
//! and its shape, then the content, with `--- … ---` markers where a piece needs a label and
//! `[ … ]` for everything the tool has to say *about* the content (a cut, a conversion, where to
//! continue). Nothing is wrapped in markup — a read is something to read, and a tag soup competes
//! with the file's own text for the model's attention.
//! | kind        | range example   | renderer                              |
//! |-------------|-----------------|----------------------------------------|
//! | text/code   | `1:100`         | lines, byte-exact                      |
//! | spreadsheet | `Sheet1!A1:F50` | sheet skeleton, or rows of cells       |
//! | docx        | `1:20`          | paragraphs (one per body paragraph)    |
//! A parser decodes the file **once** and renders every requested range against that decode;
//! `read_file` never re-parses per range. The character budget and the UI file card stay in
//! `read_file`; a parser turns file + how it should be named + ranges + a budget into content.

use std::path::Path;

use super::convert::{self, Kind};

/// What a parser hands back to `read_file`.
pub(crate) struct ParserOutput {
    /// The rendered model-facing text: a one-line header, then content under `--- … ---` markers
    /// with bracketed notes. No markup — see the module doc.
    pub content: String,
}

impl ParserOutput {
    fn new(content: String) -> Self {
        ParserOutput { content }
    }
}

/// A tiny character budget: `try_take` succeeds while the unit fits, then spends it.
struct Budget {
    left: usize,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Budget { left: limit }
    }
    fn try_take(&mut self, cost: usize) -> bool {
        if cost <= self.left {
            self.left -= cost;
            true
        } else {
            false
        }
    }
    /// Puts characters back (used when a block that reserved them up front is abandoned).
    fn refund(&mut self, cost: usize) {
        self.left = self.left.saturating_add(cost);
    }
}

/// Parses and renders one file into model-facing text.
/// `display_path` is how the file is named in the header — the caller decides (workspace-relative
/// when the file is inside the working directory, absolute when it is not), so that a path the
/// model reads is a path it can pass straight back in.
/// `ranges` are the raw strings from the tool arguments, in order; `None`/empty means "no range
/// requested" and every renderer answers with its structural summary (spreadsheet sheet
/// skeleton, whole small text). `budget` is the character budget for this read.
pub(crate) fn render(
    path: &Path,
    display_path: &str,
    raw: &[u8],
    ranges: Option<&[String]>,
    budget: usize,
    show_metadata: bool,
) -> Result<ParserOutput, String> {
    match convert::classify(path, raw) {
        Kind::Text => render_text(path, display_path, raw, ranges, budget, show_metadata),
        Kind::Table => render_spreadsheet(path, display_path, raw, ranges, budget, show_metadata),
        Kind::Docx => render_docx(path, display_path, raw, ranges, budget, show_metadata),
        Kind::Image(_) | Kind::Media(_) | Kind::Binary => {
            Err("not a textual file; images, media and binary files are not parsed".to_string())
        }
    }
}

/// The one-line header that opens every read: what was read, how big it is, and — for the kinds
/// whose shape is not line count — how many pieces it has. `unit` is that piece word
/// (`lines` / `paragraphs` / `sheets`); one of them is singular.
fn header(display_path: &str, unit: &str, count: usize, bytes: u64) -> String {
    let unit = if count == 1 {
        unit.trim_end_matches('s')
    } else {
        unit
    };
    format!("{display_path} ({count} {unit}, {})\n\n", human_size(bytes))
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// The bracketed note that stands where the cut content would have been: what was shown, and the
/// exact argument that continues the read.
/// It deliberately says nothing about how many characters of the budget were left — the call it
/// belonged to is over by the time the model reads this, so a remaining-budget figure is a number
/// it can do nothing with. What it can do something with is the next `start:end`.
pub(crate) fn truncated_note(next: &str, detail: &str) -> String {
    format!("[cut at the read limit: {detail}. Continue with {{\"ranges\":[\"{next}\"]}}.]\n")
}

/// Whether a whole-file read plausibly fits `budget` chars (line text + header
/// and markers). Below this the file is read whole; above it a big code file gets
/// the outline view first.
fn whole_text_fits_budget(lines: &[&str], budget: usize) -> bool {
    let text: usize = lines.iter().map(|l| l.chars().count()).sum();
    text + 512 <= budget
}

/// Structural view of a big code file: every definition in document order,
/// one per line `kind qualified start:end` (indented by nesting depth), under
/// a `--- definitions ---` marker. Spans are 1-based inclusive — pasteable
/// into `{"ranges":["a:b"]}` on the next call. Stops listing when the budget
/// runs out and says how many definitions were left out.
fn render_outline(out: &mut String, budget: &mut Budget, items: &[code_sitter::DefEntry]) {
    let open = "--- definitions ---\n".to_string();
    let tags = open.chars().count();

    let mut body = String::new();
    let mut rest = items.len();
    for (i, item) in items.iter().enumerate() {
        let rendered = format!(
            "{indent}{kind} {qualified} {start}:{end}\n",
            indent = "  ".repeat(item.depth),
            kind = item.symbol.kind,
            qualified = item.symbol.qualified,
            start = item.start_line,
            end = item.end_line,
        );
        let cost = rendered.chars().count() + if i == 0 { tags } else { 0 };
        if !budget.try_take(cost) {
            rest = items.len() - i;
            break;
        }
        body.push_str(&rendered);
        rest = items.len() - i - 1;
    }

    let example = items
        .iter()
        .find(|item| item.start_line != item.end_line)
        .or_else(|| items.first());
    let mut note = match example {
        Some(item) => format!(
            "[each line is \"kind qualified start:end\" (1-based, indented by nesting); read one \
span next, e.g. {{\"ranges\":[\"{}:{}\"]}} for {} {}",
            item.start_line, item.end_line, item.symbol.kind, item.symbol.qualified,
        ),
        None => "[each line is \"kind qualified start:end\" (1-based, indented by nesting); read \
one span next, or any other line range"
            .to_string(),
    };
    if rest > 0 {
        note.push_str(&format!(" — {rest} more definitions not shown (budget)"));
    }
    note.push_str("]\n");

    out.push_str(&open);
    out.push_str(&body);
    out.push_str(&note);
}

fn conversion_note(encoding: &str) -> String {
    format!(
        "[read as {encoding} and converted to UTF-8; the text is not byte-exact with the file]\n"
    )
}

/// Parses `start:end`, 1-based inclusive. `end` may exceed the file; the caller clamps.
fn parse_line_range(s: &str) -> Result<(usize, usize), String> {
    let (a, b) = s
        .split_once(':')
        .ok_or_else(|| format!("range {s:?} must look like 1:20 (1-based, inclusive)"))?;
    let start = a
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("range {s:?}: start is not a positive integer"))?;
    let end = b
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("range {s:?}: end is not a positive integer"))?;
    if start == 0 || end == 0 || start > end {
        return Err(format!("range {s:?} must be 1-based with start <= end"));
    }
    Ok((start, end))
}

fn parse_text(raw: &[u8]) -> Result<(String, Option<&'static str>), String> {
    let decoded = convert::decode_text(raw)
        .ok_or_else(|| "file is not readable text in any supported encoding".to_string())?;
    Ok((decoded.text, decoded.converted.then_some(decoded.encoding)))
}

/// Text renderer. Whole-file read when no range is given; otherwise each `start:end` selects
/// whole lines, byte-exact. The budget cuts between lines; a truncated range names the exact
/// next `start:end`.
fn render_text(
    path: &Path,
    display_path: &str,
    raw: &[u8],
    ranges: Option<&[String]>,
    budget: usize,
    show_metadata: bool,
) -> Result<ParserOutput, String> {
    let (text, converted) = parse_text(raw)?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = if text.is_empty() { 0 } else { lines.len() };
    let size = raw.len() as u64;

    let mut out = String::new();
    if show_metadata {
        out.push_str(&header(display_path, "lines", total_lines, size));
    }
    if let Some(enc) = converted {
        out.push_str(&conversion_note(enc));
    }

    let labelled = matches!(ranges, Some(list) if !list.is_empty());
    let range_specs: Vec<(usize, usize)> = match ranges {
        None | Some([]) => {
            if total_lines == 0 {
                Vec::new()
            } else {
                vec![(1, total_lines)]
            }
        }
        Some(list) => list
            .iter()
            .map(|r| parse_line_range(r))
            .collect::<Result<_, _>>()?,
    };

    let mut budget = Budget::new(budget);

    let outline_items = if total_lines > 0
        && matches!(ranges, None | Some([]))
        && !whole_text_fits_budget(&lines, budget.left)
    {
        code_sitter::outline(&path.to_path_buf()).unwrap_or_default()
    } else {
        Vec::new()
    };
    if !outline_items.is_empty() {
        render_outline(&mut out, &mut budget, &outline_items);
        return Ok(ParserOutput::new(out));
    }

    for (start, end) in range_specs {
        if start > total_lines {
            continue; // entirely past EOF
        }
        let end = end.min(total_lines);

        // The `--- lines a-b ---` marker is charged against the budget like anything else: a
        // marker that arrives without the lines it promises would be a lie about what was shown.
        let open = if labelled {
            format!("--- lines {start}-{end} ---\n")
        } else {
            String::new()
        };
        let close = String::new();
        let tags_cost = open.chars().count() + close.chars().count();

        // Emit whole lines until the budget runs out.
        let mut piece = String::new();
        let mut last_line = start.saturating_sub(1); // 0 = none shown yet
        let mut cut = false;
        for (i, line) in lines[start - 1..end].iter().enumerate() {
            let line_no = start + i;
            let cost = line.chars().count();
            if piece.is_empty() {
                // First line must also pay for the tags.
                if !budget.try_take(tags_cost + cost) {
                    cut = true;
                    break;
                }
            } else if !budget.try_take(cost) {
                cut = true;
                break;
            }
            piece.push_str(line);
            last_line = line_no;
        }
        if cut {
            if !piece.is_empty() {
                out.push_str(&open);
                out.push_str(&piece);
                out.push_str(&close);
            }
            let next_start = last_line + 1;
            let next_end = (next_start + 2000 - 1).min(total_lines);
            let detail = format!("file has {total_lines} lines, shown through {last_line}");
            out.push_str(&truncated_note(
                &format!("{next_start}:{next_end}"),
                &detail,
            ));
            break;
        }
        out.push_str(&open);
        out.push_str(&piece);
        out.push_str(&close);
    }

    Ok(ParserOutput::new(out))
}

/// Spreadsheet renderer.
/// With no range the answer is a structural summary — one line per sheet, `name: rows x columns`
/// — so the model can choose a `Sheet!A1:F50` range instead of pulling the whole file.
/// A range names a sheet and a cell rectangle: `Sheet1!A1:F50` (columns A..F, rows 1..=50) or a
/// bare row range `Sheet1!1:10`. Cell text is rendered as compact `|`-joined rows.
fn render_spreadsheet(
    path: &Path,
    display_path: &str,
    raw: &[u8],
    ranges: Option<&[String]>,
    budget: usize,
    show_metadata: bool,
) -> Result<ParserOutput, String> {
    let dump = convert::table_dump(path, raw)?;
    if dump.sheets.is_empty() {
        return Err("spreadsheet contains no worksheets".to_string());
    }
    let size = raw.len() as u64;

    let mut out = if show_metadata {
        header(display_path, "sheets", dump.sheets.len(), size)
    } else {
        String::new()
    };

    let mut budget = Budget::new(budget);

    match ranges {
        None | Some([]) => {
            // No range: a small workbook is returned whole (sheet rows inline); a large one
            // returns the skeleton plus the first rows of the first sheet and a truncated note.
            let total_rows: usize = dump.sheets.iter().map(|s| s.rows.len()).sum();
            const SMALL_SHEET_ROWS: usize = 100;

            if total_rows <= SMALL_SHEET_ROWS {
                for sheet in &dump.sheets {
                    let cols = sheet_col_count(sheet) as u32;
                    let rows = sheet_row_count(sheet) as u32;
                    let full = sheet_ref(sheet, 1, 1, cols, rows);
                    if let Some(block) = sheet_block(sheet, 1, 1, cols, rows, &full, &mut budget) {
                        out.push_str(&block);
                    } else {
                        let detail = format!("sheet has {rows} rows x {cols} columns");
                        out.push_str(&truncated_note(&full, &detail));
                        break;
                    }
                }
            } else {
                out.push_str("--- sheets ---\n");
                for sheet in &dump.sheets {
                    out.push_str(&format!(
                        "{}: {} rows x {} columns\n",
                        sheet.name,
                        sheet.rows.len() + 1,
                        sheet.columns.len(),
                    ));
                }
                // Preview: header + first 5 data rows of the first sheet.
                let first = &dump.sheets[0];
                let cols = sheet_col_count(first) as u32;
                let total_rows = sheet_row_count(first) as u32;
                let preview_rows = (first.rows.len().min(5) + 1) as u32;
                let preview_ref = sheet_ref(first, 1, 1, cols, preview_rows);
                if let Some(block) =
                    sheet_block(first, 1, 1, cols, preview_rows, &preview_ref, &mut budget)
                {
                    out.push_str(&block);
                }
                let detail = format!("first sheet has {total_rows} rows");
                out.push_str(&truncated_note(
                    &sheet_ref(first, 1, 1, cols, total_rows),
                    &detail,
                ));
            }
        }
        Some(list) => {
            for raw_range in list {
                let (sheet_idx, mut c1, r1, mut c2, r2) = parse_sheet_range(&dump, raw_range)?;
                let sheet = &dump.sheets[sheet_idx];
                let rows = sheet_row_count(sheet);
                let cols = sheet_col_count(sheet);
                // Bare row range (no column letters): expand to all columns of the sheet.
                if c1 == 0 {
                    c1 = 1;
                }
                if c2 == 0 {
                    c2 = cols.max(1) as u32;
                }
                let c2 = c2.min(cols.max(1) as u32);
                let r2 = r2.min(rows.max(1) as u32);
                if c1 > c2 || r1 > r2 {
                    continue; // past the used area
                }
                match sheet_block(sheet, c1, r1, c2, r2, raw_range, &mut budget) {
                    Some(block) => out.push_str(&block),
                    None => {
                        let next_row = r2 + 1;
                        let next = sheet_ref(sheet, c1, next_row, c2, r2.max(next_row));
                        let detail = format!("{raw_range} was cut short");
                        out.push_str(&truncated_note(&next, &detail));
                        break;
                    }
                }
            }
        }
    }

    Ok(ParserOutput::new(out))
}

/// Renders `Sheet!<c1><r1>:<c2><r2>` rows (1-based inclusive) as one `--- label ---` block,
/// charging the marker and rows against `budget`. Returns `None` when the block does not fit
/// whole — the caller then emits a truncated note.
fn sheet_block(
    sheet: &convert::SheetDump,
    c1: u32,
    r1: u32,
    c2: u32,
    r2: u32,
    range_label: &str,
    budget: &mut Budget,
) -> Option<String> {
    let open = format!("--- {range_label} ---\n");
    let close = String::new();
    let tags_cost = open.chars().count() + close.chars().count();
    // Charge tags up front; if even they do not fit, nothing can.
    if !budget.try_take(tags_cost) {
        return None;
    }
    let mut data = String::new();
    for row in r1..=r2 {
        let cells = (c1..=c2)
            .map(|c| cell_at(sheet, row, c))
            .collect::<Vec<String>>();
        let line = format!("{}\n", trim_trailing_empty(cells).join(" | "));
        let cost = line.chars().count();
        if !budget.try_take(cost) {
            // Roll back the tags charge: nothing (or not all) of this block was emitted, so the
            // caller's truncated note must fit the remaining budget.
            budget.refund(tags_cost);
            return None;
        }
        data.push_str(&line);
    }
    Some(format!("{open}{data}{close}"))
}

/// Builds a `Sheet!A1:F50`-style label from 1-based inclusive bounds. Column index past Z uses
/// Excel letters (A..Z, AA..).
fn sheet_ref(sheet: &convert::SheetDump, c1: u32, r1: u32, c2: u32, r2: u32) -> String {
    format!(
        "{}!{}{}:{}{}",
        sheet.name,
        column_letter(c1.saturating_sub(1) as u64),
        r1,
        column_letter(c2.saturating_sub(1) as u64),
        r2,
    )
}

fn sheet_row_count(sheet: &convert::SheetDump) -> usize {
    sheet.rows.len() + 1 // header + data
}

fn sheet_col_count(sheet: &convert::SheetDump) -> usize {
    sheet.columns.len()
}

fn trim_trailing_empty(mut cells: Vec<String>) -> Vec<String> {
    while cells.last().is_some_and(|s| s.is_empty()) {
        cells.pop();
    }
    cells
}

/// Reads one cell: row 1 is the header row, rows ≥ 2 are data rows. Out-of-bounds → empty.
fn cell_at(sheet: &convert::SheetDump, row: u32, col: u32) -> String {
    let (row_data, col_idx) = if row == 1 {
        (&sheet.columns, (col - 1) as usize)
    } else {
        let idx = (row - 2) as usize;
        if idx >= sheet.rows.len() {
            return String::new();
        }
        (&sheet.rows[idx], (col - 1) as usize)
    };
    row_data.get(col_idx).cloned().unwrap_or_default()
}

/// Parses `Sheet!A1:F50` or `Sheet!1:10` or `A1:F50` (single-sheet shorthand) into
/// `(sheet_idx, c1, r1, c2, r2)` with 1-based inclusive coordinates.
fn parse_sheet_range(
    dump: &convert::TableDump,
    range: &str,
) -> Result<(usize, u32, u32, u32, u32), String> {
    let (sheet_part, cells) = match range.split_once('!') {
        Some((s, c)) => (Some(s.trim()), c.trim()),
        None => (None, range.trim()),
    };
    let sheet_idx = match sheet_part {
        Some(name) => dump
            .sheets
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| format!("no sheet named {name:?} in this workbook"))?,
        None => {
            if dump.sheets.len() == 1 {
                0
            } else {
                return Err(format!(
                    "range {range:?} must name a sheet: SheetName!A1:F50 (sheets: {})",
                    dump.sheets
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    };

    // Cell rectangle `A1:F50` or bare rows `1:10` (all columns).
    let (r1, c1, r2, c2) = if cells.contains(':') {
        let (a, b) = cells.split_once(':').unwrap();
        let (p1, p2) = (parse_cell_ref(a.trim())?, parse_cell_ref(b.trim())?);
        (p1.0, p1.1, p2.0, p2.1)
    } else {
        let p = parse_cell_ref(cells)?;
        (p.0, p.1, p.0, p.1)
    };
    Ok((sheet_idx, c1, r1, c2, r2))
}

/// Parses `A1` → `(row, col)` 1-based. A bare row `5` (no column letters) means "all columns",
/// encoded as col = 0 so the caller expands it to the sheet width.
fn parse_cell_ref(s: &str) -> Result<(u32, u32), String> {
    let s = s.trim();
    let letters: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits: String = s.chars().skip(letters.len()).collect();
    if letters.is_empty() && digits.is_empty() {
        return Err(format!("cell reference {s:?} is empty"));
    }
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("cell reference {s:?} must look like A1 or 5"));
    }
    let row: u32 = digits
        .parse()
        .map_err(|_| format!("cell reference {s:?} has an invalid row"))?;
    if row == 0 {
        return Err(format!("cell reference {s:?} is 1-based"));
    }
    // Column letters → 1-based column; no letters → 0 meaning "all columns".
    let mut col: u32 = 0;
    for ch in letters.chars() {
        let v = ch.to_ascii_uppercase() as u32 - 'A' as u32 + 1;
        col = col.saturating_mul(26).saturating_add(v);
    }
    Ok((row, col))
}

/// Column index (0-based) → Excel letters: 0→A, 25→Z, 26→AA.
fn column_letter(mut index: u64) -> String {
    let mut out = Vec::new();
    index += 1;
    while index > 0 {
        index -= 1;
        out.push((b'A' + (index % 26) as u8) as char);
        index /= 26;
    }
    out.iter().rev().collect()
}

/// Docx renderer: one line per body paragraph (see `convert::docx_to_text`), so `1:10` selects
/// paragraphs 1-10 and each paragraph is simply its own line.
fn render_docx(
    path: &Path,
    display_path: &str,
    raw: &[u8],
    ranges: Option<&[String]>,
    budget: usize,
    show_metadata: bool,
) -> Result<ParserOutput, String> {
    // If the docx cannot be opened as a package but the bytes are plain text, fall back.
    let text = match convert::docx_to_text(path) {
        Ok(t) => t,
        Err(conversion_error) => {
            if let Some(decoded) = convert::decode_text(raw) {
                decoded.text
            } else {
                return Err(conversion_error);
            }
        }
    };
    let paragraphs: Vec<&str> = text.split_inclusive('\n').collect();
    let total = if text.is_empty() { 0 } else { paragraphs.len() };
    let size = raw.len() as u64;

    let mut out = if show_metadata {
        header(display_path, "paragraphs", total, size)
    } else {
        String::new()
    };

    let labelled = matches!(ranges, Some(list) if !list.is_empty());
    let range_specs: Vec<(usize, usize)> = match ranges {
        None | Some([]) => {
            if total == 0 {
                Vec::new()
            } else {
                vec![(1, total)]
            }
        }
        Some(list) => list
            .iter()
            .map(|r| parse_line_range(r))
            .collect::<Result<_, _>>()?,
    };

    let mut budget = Budget::new(budget);

    for (start, end) in range_specs {
        if start > total {
            continue;
        }
        let end = end.min(total);
        let open = if labelled {
            format!("--- paragraphs {start}-{end} ---\n")
        } else {
            String::new()
        };
        let close = String::new();
        let tags_cost = open.chars().count() + close.chars().count();

        let mut piece = String::new();
        let mut last = start.saturating_sub(1);
        let mut cut = false;
        for (i, para) in paragraphs[start - 1..end].iter().enumerate() {
            let para_no = start + i;
            // A paragraph is a line: the newline is the boundary, so nothing needs to wrap it.
            let rendered = format!("{}\n", para.trim_end());
            let cost = rendered.chars().count();
            if piece.is_empty() {
                if !budget.try_take(tags_cost + cost) {
                    cut = true;
                    break;
                }
            } else if !budget.try_take(cost) {
                cut = true;
                break;
            }
            piece.push_str(&rendered);
            last = para_no;
        }
        if cut {
            if !piece.is_empty() {
                out.push_str(&open);
                out.push_str(&piece);
                out.push_str(&close);
            }
            let next_start = last + 1;
            let next_end = (next_start + 2000 - 1).min(total);
            let detail = format!("document has {total} paragraphs, shown through {last}");
            out.push_str(&truncated_note(
                &format!("{next_start}:{next_end}"),
                &detail,
            ));
            break;
        }
        out.push_str(&open);
        out.push_str(&piece);
        out.push_str(&close);
    }

    Ok(ParserOutput::new(out))
}

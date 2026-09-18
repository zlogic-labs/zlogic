use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::glyph::{Glyph, IconTier};
use crate::markdown::inline;
use crate::theme::{Sem, ThemeState};
use crate::widgets::rail;
use crate::widgets::table::{self, Align, Column};

/// Width of a rendered horizontal rule (`---`). Fixed: this module has no
/// terminal-width input, matching `widgets::rule`'s 80-col clamp philosophy.
const HR_WIDTH: usize = 40;

pub fn render_markdown(
    src: &str,
    theme: &ThemeState,
    icons: crate::glyph::IconTier,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut code = false;
    let mut code_lines: Vec<Line<'static>> = Vec::new();
    let mut code_lang = String::new();
    let mut table_lines: Vec<String> = Vec::new();

    for raw in src.lines() {
        if !code && is_table_line(raw) {
            table_lines.push(raw.to_string());
            continue;
        }
        if !table_lines.is_empty() {
            out.extend(render_table_block(&table_lines, theme, icons));
            table_lines.clear();
        }
        if raw.trim_start().starts_with("```") {
            if code {
                if code_lang == "diff" {
                    let src = code_lines
                        .iter()
                        .flat_map(|line| line.spans.iter())
                        .map(|span| span.content.as_ref())
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.extend(crate::markdown::diff::render_diff(&src, theme, icons));
                } else {
                    out.extend(rail::rail_lines(
                        &code_lines,
                        theme.style(Sem::Muted),
                        icons,
                    ));
                }
                code_lines = Vec::new();
                code_lang.clear();
            } else if let Some(lang) = raw.trim_start().strip_prefix("```") {
                code_lang = lang.trim().to_string();
                if !code_lang.is_empty() && code_lang != "diff" {
                    code_lines.push(Line::from(Span::styled(
                        code_lang.clone(),
                        theme.style(Sem::Muted),
                    )));
                }
            }
            code = !code;
            continue;
        }
        if code {
            code_lines.push(Line::from(raw.to_string()));
        } else {
            out.push(render_line(raw, theme, icons));
        }
    }

    if !table_lines.is_empty() {
        out.extend(render_table_block(&table_lines, theme, icons));
    }
    if !code_lines.is_empty() {
        out.extend(rail::rail_lines(
            &code_lines,
            theme.style(Sem::Muted),
            icons,
        ));
    }
    out
}

fn is_table_line(raw: &str) -> bool {
    let s = raw.trim();
    s.starts_with('|') && s.ends_with('|') && s.matches('|').count() >= 2
}

fn render_table_block(lines: &[String], theme: &ThemeState, icons: IconTier) -> Vec<Line<'static>> {
    // Cells go through `strip_inline` (markers dropped, plain text kept):
    // the table widget lays out plain `String` cells, so styled spans can't
    // ride through — but literal `**`/backticks in cells would be worse.
    let parsed: Vec<Vec<String>> = lines
        .iter()
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|s| inline::strip_inline(s.trim(), theme))
                .collect()
        })
        .collect();
    if parsed.is_empty() {
        return Vec::new();
    }
    let headers = parsed[0].clone();
    let rows: Vec<Vec<String>> = parsed
        .iter()
        .skip(if parsed.len() > 1 && is_separator_row(&parsed[1]) {
            2
        } else {
            1
        })
        .cloned()
        .collect();
    let columns: Vec<Column> = headers
        .iter()
        .map(|h| Column {
            header: h.clone(),
            min: 3,
            max: 28,
            priority: 1,
            align: if h.eq_ignore_ascii_case("price") || h.eq_ignore_ascii_case("key") {
                Align::Right
            } else {
                Align::Left
            },
        })
        .collect();
    table::render_table(
        &columns,
        &rows,
        100,
        theme.style(Sem::Muted),
        Style::default(),
        theme.style(Sem::Muted),
        icons,
    )
}

fn is_separator_row(row: &[String]) -> bool {
    row.iter().all(|cell| {
        cell.chars()
            .all(|c| c == '-' || c == ':' || c.is_whitespace())
    })
}

fn render_line(raw: &str, theme: &ThemeState, icons: IconTier) -> Line<'static> {
    let s = raw.trim_start();
    let indent = raw.len() - s.len();

    if is_hr(s) {
        return hr_line(theme, icons);
    }
    if let Some((level, text)) = heading_split(s) {
        return heading(text, level, theme);
    }
    if let Some(text) = s.strip_prefix("- ") {
        return list_item(text, indent, theme, icons);
    }
    if let Some(text) = s.strip_prefix("> ") {
        return Line::from(inline::parse_inline_styled(
            text,
            theme.style(Sem::Muted),
            theme,
        ));
    }
    if s == ">" {
        return Line::from(Span::styled(String::new(), theme.style(Sem::Muted)));
    }
    if let Some((marker, text)) = ordered_split(s) {
        let mut spans = pad_spans(indent);
        spans.push(Span::styled(format!("{marker} "), theme.style(Sem::Muted)));
        spans.extend(inline::parse_inline(text, theme));
        return Line::from(spans);
    }
    Line::from(inline::parse_inline(raw, theme))
}

/// `---` / `***` / `___` (3+ of one marker char, spaces allowed) on its own line.
fn is_hr(s: &str) -> bool {
    let s = s.trim_end();
    ['-', '*', '_'].iter().any(|ch| {
        let mut count = 0usize;
        for c in s.chars() {
            if c == *ch {
                count += 1;
            } else if c != ' ' {
                return false;
            }
        }
        count >= 3
    })
}

fn hr_line(theme: &ThemeState, icons: IconTier) -> Line<'static> {
    let ch = Glyph::RuleHorizontal.render(icons);
    Line::from(Span::styled(ch.repeat(HR_WIDTH), theme.style(Sem::Border)))
}

/// `#`..`######` followed by a space → (level, text).
fn heading_split(s: &str) -> Option<(usize, &str)> {
    let level = s.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    s[level..].strip_prefix(' ').map(|text| (level, text))
}

/// Heading ladder: h1/h3 bold Accent, h2/h4-h6 bold AccentSoft (no size
/// tricks in a terminal — colour + weight only).
fn heading(text: &str, level: usize, theme: &ThemeState) -> Line<'static> {
    let sem = match level {
        1 | 3 => Sem::Accent,
        _ => Sem::AccentSoft,
    };
    let base = Style::default()
        .fg(theme.color(sem))
        .add_modifier(Modifier::BOLD);
    Line::from(inline::parse_inline_styled(text, base, theme))
}

/// Bullet or task-list item; `indent` (leading spaces in the source) is kept
/// as left padding so 2-or-4-space nested lists sit deeper.
fn list_item(text: &str, indent: usize, theme: &ThemeState, icons: IconTier) -> Line<'static> {
    let mut spans = pad_spans(indent);
    if let Some(item) = text.strip_prefix("[ ] ") {
        spans.push(Span::styled("[ ] ", theme.style(Sem::Muted)));
        spans.extend(inline::parse_inline(item, theme));
        return Line::from(spans);
    }
    if let Some(item) = text
        .strip_prefix("[x] ")
        .or_else(|| text.strip_prefix("[X] "))
    {
        spans.push(Span::styled("[x] ", theme.style(Sem::Success)));
        spans.extend(inline::parse_inline(item, theme));
        return Line::from(spans);
    }
    spans.push(Span::styled(
        format!("{} ", Glyph::ListBullet.render(icons)),
        theme.style(Sem::AccentSoft),
    ));
    spans.extend(inline::parse_inline(text, theme));
    Line::from(spans)
}

/// `1. ` / `1) ` ordered-list marker (rendered as-is, no renumbering).
fn ordered_split(s: &str) -> Option<(&str, &str)> {
    let digit_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digit_end == 0 {
        return None;
    }
    let rest = &s[digit_end..];
    let punct = rest.chars().next()?;
    if punct != '.' && punct != ')' {
        return None;
    }
    let text = rest[1..].strip_prefix(' ')?;
    Some((&s[..digit_end + 1], text))
}

fn pad_spans(indent: usize) -> Vec<Span<'static>> {
    if indent == 0 {
        Vec::new()
    } else {
        vec![Span::raw(" ".repeat(indent))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{by_id, ColorTier};

    fn ts() -> ThemeState {
        ThemeState {
            theme: by_id("dark").expect("dark theme exists"),
            tier: ColorTier::Rich,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn render(src: &str) -> Vec<Line<'static>> {
        render_markdown(src, &ts(), IconTier::Unicode)
    }

    #[test]
    fn h1_h2_keep_accent_ladder() {
        let t = ts();
        let lines = render("# One\n## Two");
        assert_eq!(lines[0].spans[0].style.fg, Some(t.color(Sem::Accent)));
        assert_eq!(lines[1].spans[0].style.fg, Some(t.color(Sem::AccentSoft)));
        for line in &lines {
            assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn h3_through_h6_are_bold_headings() {
        let t = ts();
        let lines = render("### Three\n#### Four\n##### Five\n###### Six");
        assert_eq!(line_text(&lines[0]), "Three");
        assert_eq!(lines[0].spans[0].style.fg, Some(t.color(Sem::Accent)));
        for (line, text) in lines[1..].iter().zip(["Four", "Five", "Six"]) {
            assert_eq!(line_text(line), text);
            assert_eq!(line.spans[0].style.fg, Some(t.color(Sem::AccentSoft)));
            assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn seven_hashes_is_not_a_heading() {
        let lines = render("####### nope");
        assert_eq!(line_text(&lines[0]), "####### nope");
    }

    #[test]
    fn heading_text_gets_inline_styles() {
        let lines = render("## a `code` b");
        let t = ts();
        let code = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "code")
            .expect("code span");
        assert_eq!(code.style.fg, Some(t.color(Sem::AccentSoft)));
    }

    #[test]
    fn horizontal_rule_variants() {
        let t = ts();
        for src in ["---", "***", "___", "- - -", "-----"] {
            let lines = render(src);
            assert_eq!(line_text(&lines[0]), "─".repeat(HR_WIDTH), "src={src:?}");
            assert_eq!(lines[0].spans[0].style, t.style(Sem::Border), "src={src:?}");
        }
    }

    #[test]
    fn hr_needs_three_markers_and_nothing_else() {
        assert_eq!(line_text(&render("--")[0]), "--");
        assert_eq!(line_text(&render("--- x")[0]), "--- x");
        // an emphasis-only line is not a rule
        assert_eq!(line_text(&render("***x***")[0]), "x");
    }

    #[test]
    fn hr_respects_ascii_icon_tier() {
        let lines = render_markdown("---", &ts(), IconTier::Ascii);
        assert_eq!(line_text(&lines[0]), "-".repeat(HR_WIDTH));
    }

    #[test]
    fn bullet_item_with_inline_styles() {
        let lines = render("- **bold** item");
        assert_eq!(line_text(&lines[0]), "• bold item");
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn nested_list_keeps_indent() {
        let lines = render("- top\n  - two\n    - four");
        assert_eq!(line_text(&lines[0]), "• top");
        assert_eq!(line_text(&lines[1]), "  • two");
        assert_eq!(line_text(&lines[2]), "    • four");
    }

    #[test]
    fn list_and_table_rules_respect_ascii_icon_tier() {
        let lines = render_markdown("- item\n\n| A |\n| - |\n| x |", &ts(), IconTier::Ascii);
        assert_eq!(line_text(&lines[0]), "- item");
        assert!(line_text(&lines[3]).starts_with('-'));
    }

    #[test]
    fn ordered_list_marker_is_muted_and_verbatim() {
        let t = ts();
        let lines = render("1. first\n2) second\n  3. nested");
        assert_eq!(line_text(&lines[0]), "1. first");
        assert_eq!(lines[0].spans[0].style, t.style(Sem::Muted));
        assert_eq!(line_text(&lines[1]), "2) second");
        assert_eq!(line_text(&lines[2]), "  3. nested");
    }

    #[test]
    fn not_an_ordered_list_without_separator() {
        // "2024 was fine" must not become a list item
        let lines = render("2024 was fine");
        assert_eq!(line_text(&lines[0]), "2024 was fine");
    }

    #[test]
    fn task_list_checkboxes() {
        let t = ts();
        let lines = render("- [ ] todo\n- [x] done");
        assert_eq!(line_text(&lines[0]), "[ ] todo");
        assert_eq!(lines[0].spans[0].style, t.style(Sem::Muted));
        assert_eq!(line_text(&lines[1]), "[x] done");
        assert_eq!(lines[1].spans[0].style, t.style(Sem::Success));
    }

    #[test]
    fn blockquote_inline_keeps_muted_base() {
        let t = ts();
        let lines = render("> a *i* b");
        assert_eq!(line_text(&lines[0]), "a i b");
        for span in &lines[0].spans {
            assert_eq!(span.style.fg, t.style(Sem::Muted).fg, "{span:?}");
        }
        let italic = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "i")
            .expect("italic span");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn paragraph_gets_inline_styles() {
        let lines = render("hello **world**");
        assert_eq!(line_text(&lines[0]), "hello world");
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "world")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn table_cells_strip_inline_markers() {
        let lines = render("| Name | Desc |\n| --- | --- |\n| `run` | **fast** |");
        let body: Vec<String> = lines.iter().map(line_text).collect();
        let joined = body.join("\n");
        assert!(joined.contains("run"), "{joined}");
        assert!(!joined.contains('`'), "{joined}");
        assert!(joined.contains("fast"), "{joined}");
        assert!(!joined.contains("**"), "{joined}");
    }

    #[test]
    fn code_fence_content_is_not_inline_parsed() {
        let lines = render("```\n**not bold**\n```");
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("**not bold**"), "{joined}");
    }

    #[test]
    fn fenced_lang_label_still_renders() {
        let lines = render("```rust\nlet x = 1;\n```");
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("rust"), "{joined}");
        assert!(joined.contains("let x = 1;"), "{joined}");
    }

    #[test]
    fn diff_fence_routes_to_diff_renderer() {
        let t = ts();
        let lines = render("```diff\ndiff a b\n+added\n-removed\n```");
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("+added"), "{joined}");
        let plus = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "+added")
            .expect("+added span");
        assert_eq!(plus.style.fg, Some(t.color(Sem::Success)));
    }

    #[test]
    fn cjk_paragraph_with_styles_keeps_all_text() {
        let lines = render("这是**重点**：请看`示例`。");
        assert_eq!(line_text(&lines[0]), "这是重点：请看示例。");
    }
}

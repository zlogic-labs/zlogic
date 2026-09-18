//! Text received from an MCP server is untrusted.
//! Tool descriptions enter the model prompt, results enter both model context and the terminal,
//! and stderr may enter an error. Control sequences and invisible direction-changing characters
//! therefore have no legitimate place at this boundary. This deliberately is not Unicode
//! normalisation: identifiers, source code, and non-Latin text must keep their spelling.

/// Removes terminal control sequences and invisible formatting controls while preserving ordinary
/// Unicode, tabs, and line breaks. CRLF and lone CR are normalised to LF so a server cannot use a
/// carriage return to overwrite a terminal line.
pub(crate) fn sanitize_untrusted_text(input: &str) -> String {
    #[derive(Clone, Copy)]
    enum Escape {
        Normal,
        Esc,
        Csi,
        String,
        StringEsc,
    }

    let mut out = String::with_capacity(input.len());
    let mut state = Escape::Normal;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match state {
            Escape::Esc => {
                state = match ch {
                    '[' => Escape::Csi,
                    ']' | 'P' | '^' | '_' => Escape::String,
                    _ => Escape::Normal,
                };
                continue;
            }
            Escape::Csi => {
                if ('@'..='~').contains(&ch) {
                    state = Escape::Normal;
                }
                continue;
            }
            Escape::String => {
                state = match ch {
                    '\u{7}' => Escape::Normal,
                    '\u{1b}' => Escape::StringEsc,
                    _ => Escape::String,
                };
                continue;
            }
            Escape::StringEsc => {
                state = if ch == '\\' {
                    Escape::Normal
                } else {
                    Escape::String
                };
                continue;
            }
            Escape::Normal => {}
        }

        match ch {
            '\u{1b}' => state = Escape::Esc,
            // Eight-bit C1 forms of CSI, DCS, OSC, PM and APC.
            '\u{009b}' => state = Escape::Csi,
            '\u{0090}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => state = Escape::String,
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(ch),
            _ if is_invisible_format(ch) || ch.is_control() => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Unicode format controls that can make prompt text look different from its actual byte order.
fn is_invisible_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'
            | '\u{1160}'
            | '\u{17b4}'
            | '\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e0000}'..='\u{e007f}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

/// Sanitises every string value in schema JSON. Keys are protocol identifiers and stay byte-exact;
/// unsafe top-level parameter keys are removed separately by `tool::normalize_schema`.
pub(crate) fn sanitize_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = sanitize_untrusted_text(text),
        serde_json::Value::Array(values) => values.iter_mut().for_each(sanitize_json_strings),
        serde_json::Value::Object(object) => {
            object.values_mut().for_each(sanitize_json_strings);
        }
        _ => {}
    }
}

pub(crate) fn has_unsafe_text(text: &str) -> bool {
    sanitize_untrusted_text(text) != text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_terminal_and_direction_injection() {
        let raw = "\u{feff}safe\u{200b}\u{202e}txt\u{202c}\x1b[31m red\x1b[0m\
                   \x1b]8;;https://evil.test\x07link\x1b]8;;\x07";
        assert_eq!(sanitize_untrusted_text(raw), "safetxt redlink");
    }

    #[test]
    fn normalises_carriage_returns_and_preserves_real_text() {
        assert_eq!(
            sanitize_untrusted_text("text\tcode\r\nnext\rover"),
            "text\tcode\nnext\nover"
        );
    }

    #[test]
    fn strips_c0_and_c1_controls_without_leaving_csi_payloads() {
        assert_eq!(
            sanitize_untrusted_text("a\0b\u{009b}2Kc\u{009d}title\u{7}d"),
            "abcd"
        );
    }
}

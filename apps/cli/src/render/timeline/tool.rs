use ratatui::text::Line;

use crate::app::{AppState, ToolRow, ViewMode};
use crate::glyph::Glyph;
use crate::log::classify::{self, LogKind};

pub fn line(state: &AppState, spinner: &str, row: &ToolRow, mode: ViewMode) -> Line<'static> {
    let verbose = mode == ViewMode::Verbose;
    let params = if verbose {
        row.params.as_deref().unwrap_or(&row.arg)
    } else {
        &row.arg
    };
    let text = if let Some(ok) = row.ok {
        let mark = if ok { Glyph::Ok } else { Glyph::Fail };
        if verbose {
            match row.result.as_deref().filter(|text| !text.is_empty()) {
                Some(result) => {
                    format!(
                        "{}  {} {}  {}",
                        row.name,
                        mark.render(state.icons),
                        params,
                        result
                    )
                }
                None => format!("{}  {} {}", row.name, mark.render(state.icons), params),
            }
        } else {
            format!("{}  {} {}", row.name, mark.render(state.icons), params)
        }
    } else {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("spinner", spinner);
        format!(
            "{}({}) {}",
            row.name,
            params,
            state.i18n.format("live-tool-running", Some(&args))
        )
    };
    classify::line(LogKind::Tool, text, &state.theme, state.icons)
}

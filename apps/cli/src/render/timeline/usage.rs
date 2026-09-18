use ratatui::text::Line;

use crate::app::AppState;
use crate::log::classify::{self, LogKind};
use crate::session::dto::UsageSnapshot;

pub fn line(state: &AppState, usage: &UsageSnapshot) -> Line<'static> {
    classify::line(
        LogKind::Notice,
        format!("usage · {} tokens", usage.total_tokens),
        &state.theme,
        state.icons,
    )
}

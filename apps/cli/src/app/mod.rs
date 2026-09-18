//! App state (TEA) — the render thread owns this + the `Terminal`, exclusively.
//! `view` is a pure function of `AppState`.

pub mod loop_;
pub mod msg;
pub mod update;

pub use msg::Msg;

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::style::Style;
use ratatui::text::Line;
use serde::{Deserialize, Serialize};

use crate::glyph::IconTier;
use crate::i18n::{I18n, Locale};
use crate::render::history::{wrap_hist_line, HistAlign, HistLine, HistSpan};
use crate::session::dto::{
    CommandSpec, ConnResult, HistoryItem, KeyEntry, KeyStatus, MailboxEntry, Message, ModelEntry,
    ModelUsage, NotificationLevel, PermissionMode, ProviderEntry, Risk, SessionSummary,
    TurnSummary, TurnUsage, UsageSnapshot, WorkspaceSummary,
};
use crate::session::CoreSession;
use crate::splash;
use crate::term::caps::{detect_terminal_kind, TerminalKind};
use crate::theme::{Sem, ThemeState};

const TRANSCRIPT_LIMIT: usize = 2000;

/// Transient status line above the input auto-clears after this long
const STATUS_VISIBLE_FOR: Duration = Duration::from_secs(4);

/// Startup stage (splash). Boot animates loading; Ready is the normal app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Boot,
    Ready,
    Exit,
}

/// Max visible content lines in the input box; beyond this it scrolls internally.
/// The cap is 4.
pub const MAX_INPUT_LINES: u16 = 4;

/// Input-box mode (a small state machine). Normal and streaming are wired; god/plan are
/// carried but their full UX comes later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Streaming,
    God,
    Plan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    Minimal,
    #[default]
    Normal,
    Verbose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamingSendTarget {
    Mailbox,
    #[default]
    NextTurn,
}

pub struct QueuedTurn {
    pub messages: Vec<Message>,
    pub display: String,
}

/// A tool row in the live area.
pub struct ToolRow {
    pub id: String,
    pub name: String,
    pub arg: String,
    pub params: Option<String>,
    /// None = running (spinner); Some(ok) = settled.
    pub ok: Option<bool>,
    pub result: Option<String>,
}

pub struct ThinkingBlock {
    pub text: String,
    pub open: bool,
}

pub enum RoundBlock {
    Thinking(ThinkingBlock),
    Tool(ToolRow),
    Text(String),
    Note(String),
    Mailbox(MailboxEntry),
}

#[derive(Default)]
pub struct ReasoningRound {
    pub id: String,
    /// Stream blocks in arrival order. Never regroup by kind during rendering.
    pub blocks: Vec<RoundBlock>,
}

impl ReasoningRound {
    pub fn start_thinking(&mut self) {
        self.blocks.push(RoundBlock::Thinking(ThinkingBlock {
            text: String::new(),
            open: true,
        }));
    }

    pub fn thinking_mut(&mut self) -> &mut ThinkingBlock {
        let needs_block = !matches!(
            self.blocks.last(),
            Some(RoundBlock::Thinking(ThinkingBlock { open: true, .. }))
        );
        if needs_block {
            self.start_thinking();
        }
        match self.blocks.last_mut() {
            Some(RoundBlock::Thinking(block)) => block,
            _ => unreachable!("thinking block was just appended"),
        }
    }

    pub fn end_thinking(&mut self) {
        if let Some(block) = self.blocks.iter_mut().rev().find_map(|block| match block {
            RoundBlock::Thinking(thinking) if thinking.open => Some(thinking),
            _ => None,
        }) {
            block.open = false;
        }
    }

    pub fn append_text(&mut self, delta: String) {
        if let Some(RoundBlock::Text(text)) = self.blocks.last_mut() {
            text.push_str(&delta);
        } else {
            self.blocks.push(RoundBlock::Text(delta));
        }
    }
}

pub struct PasteDraft {
    pub text: String,
    pub display: String,
    pub lines: usize,
    pub chars: usize,
}

pub struct FileDraft {
    pub token: String,
    pub path: String,
    pub display: String,
    pub is_dir: bool,
}

/// One composer attachment (segmented composer). Chips are the SINGLE
/// source of truth for attachments: the input-box chip row renders them and the
/// submit path consumes them — there are no parallel draft lists and no inline
/// tokens in the text anymore.
pub struct Chip {
    pub kind: ChipKind,
    /// Cached token estimate of the chip's content (`estimate_tokens` at creation
    /// time), so the live composer estimate stays O(typed input) per render.
    pub tokens: u64,
}

pub enum ChipKind {
    Paste(PasteDraft),
    File(FileDraft),
}

impl Chip {
    pub fn paste(draft: PasteDraft) -> Self {
        let tokens = estimate_tokens(&draft.text);
        Self {
            kind: ChipKind::Paste(draft),
            tokens,
        }
    }

    /// `preview` is the (≤500-char) head that will accompany the FileRef message;
    /// the estimate covers path + preview, matching what actually ships.
    pub fn file(draft: FileDraft, preview: Option<&str>) -> Self {
        let tokens = estimate_tokens(&draft.path) + preview.map_or(0, estimate_tokens);
        Self {
            kind: ChipKind::File(draft),
            tokens,
        }
    }
}

/// Reference budget for the composer token warning when the current model's
/// context window is unknown (no `context_limit_tokens` yet) — a fixed 32k
/// stand-in, by design.
pub const FALLBACK_CONTEXT_TOKENS: u64 = 32_000;

/// Cheap token estimate — chars/4, CJK chars/1.7. This is a UI-side PLACEHOLDER
/// for the core estimator (which knows the real tokenizer); good enough for the
/// live `~1.2k` badge, never used for billing/limits.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    (other as f64 / 4.0 + cjk as f64 / 1.7).ceil() as u64
}

fn is_cjk(ch: char) -> bool {
    matches!(u32::from(ch),
        0x1100..=0x11FF      // Hangul Jamo
        | 0x2E80..=0x303F    // CJK radicals · Kangxi · CJK punctuation
        | 0x3040..=0x30FF    // Hiragana · Katakana
        | 0x3130..=0x318F    // Hangul compatibility Jamo
        | 0x3400..=0x4DBF    // CJK ext A
        | 0x4E00..=0x9FFF    // CJK unified
        | 0xAC00..=0xD7AF    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK compatibility
        | 0xFF00..=0xFFEF    // full/half-width forms
        | 0x20000..=0x2FA1F  // CJK ext B..F
    )
}

#[derive(Clone)]
pub struct FilePickerEntry {
    pub name: String,
    pub path: String,
    pub display: String,
    pub is_dir: bool,
    pub is_parent: bool,
}

pub struct FilePicker {
    pub cwd: std::path::PathBuf,
    pub prefix: String,
    pub query: String,
    pub entries: Vec<FilePickerEntry>,
    pub selected: usize,
}

pub struct FileScanRequest {
    pub generation: u64,
    pub query: String,
}

#[derive(Clone)]
pub enum SuggestItem {
    Command(CommandSpec),
    Argument {
        command: String,
        value: String,
        description: String,
    },
}

pub struct SuggestState {
    pub items: Vec<SuggestItem>,
    pub selected: usize,
    pub query: String,
}

pub enum CompletionState {
    Command(SuggestState),
    File(FilePicker),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatsRange {
    OneDay,
    SevenDays,
    ThirtyDays,
    Month,
}

impl StatsRange {
    pub fn days(self) -> Option<usize> {
        match self {
            StatsRange::OneDay => Some(1),
            StatsRange::SevenDays => Some(7),
            StatsRange::ThirtyDays => Some(30),
            // Core returns a current-calendar-month dataset. `Month` therefore
            // means all loaded rows rather than an unbounded all-time range.
            StatsRange::Month => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            StatsRange::OneDay => StatsRange::SevenDays,
            StatsRange::SevenDays => StatsRange::ThirtyDays,
            StatsRange::ThirtyDays => StatsRange::Month,
            StatsRange::Month => StatsRange::OneDay,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            StatsRange::OneDay => StatsRange::Month,
            StatsRange::SevenDays => StatsRange::OneDay,
            StatsRange::ThirtyDays => StatsRange::SevenDays,
            StatsRange::Month => StatsRange::ThirtyDays,
        }
    }
}

pub struct StatsOverlay {
    pub usage: UsageSnapshot,
    pub range: StatsRange,
    /// Per-model breakdown (`usage_by_model`), rendered as the "By model" table.
    pub by_model: Vec<ModelUsage>,
    /// Daily usage timeline (`usage_timeline`, oldest → newest), rendered as the
    /// daily bar chart; the range switcher filters it to the last N days.
    pub timeline: Vec<TurnUsage>,
    /// First visible row in the scrollable stats body.
    pub scroll: usize,
}

pub struct HelpOverlay {
    pub commands: Vec<CommandSpec>,
    pub selected: usize,
}

/// One row of the single-list model manager: a provider group header followed by its
/// model rows (design: master-detail, one flat cursor). Enter on a model switches the
/// session model; Enter on a provider header binds its API key. Provider/model
/// structure itself lives in `models.yaml` — the page only browses and manages keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRow {
    /// Index into [`ModelsOverlay::providers`].
    Provider(usize),
    /// (`provider` index, `model` index) into `providers` / `models`.
    Model { provider: usize, model: usize },
}

/// Which mode the models overlay is in. Provider/model structure is edited in
/// `models.yaml` — the page only browses and manages API keys.
pub enum ModelOverlayMode {
    Browse,
    SetKey {
        value: String,
        storage: String,
        field: usize,
    },
}

#[derive(Clone)]
pub struct ModelTest {
    pub model: String,
    pub result: Option<ConnResult>,
}

pub struct ModelsOverlay {
    /// Flat grouped list: one `Provider` row per provider, then its model rows.
    pub rows: Vec<ModelRow>,
    /// Cursor into `rows`.
    pub selected: usize,
    pub providers: Vec<ProviderEntry>,
    pub models: Vec<ModelEntry>,
    pub keys: Vec<KeyEntry>,
    pub test: Option<ModelTest>,
    pub detail_scroll: usize,
    pub mode: ModelOverlayMode,
    /// Validation error shown inside the active form (e.g. a bad key/storage).
    pub form_error: Option<String>,
    pub confirm: Option<ConfirmDialog>,
    pub config_path: Option<String>,
    pub filter: String,
    /// Set when the missing-key prompt was confirmed: the model to switch to as soon
    /// as the key form saves successfully.
    pub pending_switch: Option<String>,
}

impl ModelsOverlay {
    /// Row under the cursor (Copy — callers read the indices before mutating).
    pub(crate) fn selected_row(&self) -> Option<ModelRow> {
        self.rows.get(self.selected).copied()
    }
}

/// Flatten providers → their models into one selectable list (each provider header
/// first, then its model rows, in catalog order). Rows carry indices so lookup stays
/// O(1) and re-selection survives a reload by name.
pub fn model_rows(providers: &[ProviderEntry], models: &[ModelEntry]) -> Vec<ModelRow> {
    let mut rows = Vec::new();
    for (pi, provider) in providers.iter().enumerate() {
        rows.push(ModelRow::Provider(pi));
        for (mi, model) in models.iter().enumerate() {
            if model.provider == provider.name {
                rows.push(ModelRow::Model {
                    provider: pi,
                    model: mi,
                });
            }
        }
    }
    rows
}

pub fn sort_providers_by_key(providers: &mut [ProviderEntry]) {
    providers.sort_by_key(|provider| matches!(provider.key, KeyStatus::Missing));
}

pub fn filter_catalog(
    providers: &[ProviderEntry],
    models: &[ModelEntry],
    query: &str,
) -> (Vec<ProviderEntry>, Vec<ModelEntry>) {
    if query.trim().is_empty() {
        return (providers.to_vec(), models.to_vec());
    }
    let q = query.to_lowercase();
    let provider_has =
        |p: &ProviderEntry| p.name.to_lowercase().contains(&q) || p.sdk.to_lowercase().contains(&q);
    let kept: Vec<ProviderEntry> = providers
        .iter()
        .filter(|p| {
            provider_has(p)
                || models
                    .iter()
                    .any(|m| m.provider == p.name && m.name.to_lowercase().contains(&q))
        })
        .cloned()
        .collect();
    let kept_models: Vec<ModelEntry> = models
        .iter()
        .filter(|m| kept.iter().any(|p| p.name == m.provider) && m.name.to_lowercase().contains(&q))
        .cloned()
        .collect();
    (kept, kept_models)
}

pub struct SessionsOverlay {
    pub items: Vec<SessionSummary>,
    pub selected: usize,
    pub query: String,
    pub searching: bool,
    pub last_searched: Option<String>,
    /// Inline rename of the selected session: `Some(draft)` while editing (list pane
    /// `r`), rendered in the header where the search input lives. Enter commits,
    /// Esc cancels.
    pub renaming: Option<String>,
    /// History returned by `CoreSession::session_history` for the selected session.
    pub history: Vec<HistoryItem>,
    pub detail_mode: SessionDetailMode,
    pub history_selected: usize,
    pub selection_mode: bool,
    /// Session IDs staged for batch deletion from the left pane.
    pub selected_session_ids: BTreeSet<String>,
    pub confirm: Option<ConfirmDialog>,
}

pub struct WorkspacesOverlay {
    pub items: Vec<WorkspaceSummary>,
    pub selected: usize,
    pub current: String,
    pub sessions: Vec<SessionSummary>,
    pub focus: WorkspacePane,
    pub session_selected: usize,
    pub confirm: Option<ConfirmDialog>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacePane {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionDetailMode {
    Preview,
    HistoryPicker,
}

#[derive(Clone)]
pub struct ConfirmDialog {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub cancel_label: String,
    pub danger: bool,
    pub selected: usize,
    pub action: ConfirmAction,
}

#[derive(Clone)]
pub enum ConfirmAction {
    SwitchSession {
        id: String,
    },
    DeleteSessions {
        ids: Vec<String>,
    },
    ChooseHistoryAction {
        session_id: String,
        history_id: String,
    },
    ForkHistory {
        session_id: String,
        history_id: String,
    },
    RewindHistory {
        session_id: String,
        history_id: String,
    },
    /// Missing-key prompt: bind the key for `provider`, then switch to `model`.
    BindProviderKey {
        provider: String,
        model: String,
    },
    SwitchWorkspace {
        workspace_id: String,
        session_id: Option<String>,
    },
}

#[derive(Default)]
pub struct StatusSnapshot {
    pub cwd: String,
    pub session_id: Option<String>,
    /// Current session's display title (from `SessionTitleUpdate` / list lookup);
    /// the status line falls back to the short session id when unset.
    pub session_title: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub total_tokens: u64,
    pub context_used_tokens: u64,
    pub context_limit_tokens: u64,
    pub context_is_live: bool,
}

impl StatusSnapshot {
    pub fn model_identity(&self) -> Option<String> {
        let model = self.model.as_deref()?;
        let Some(provider) = self.provider.as_deref() else {
            return Some(model.to_string());
        };
        let model = model
            .strip_prefix(provider)
            .and_then(|tail| tail.strip_prefix('/'))
            .unwrap_or(model);
        Some(format!("{provider}:{model}"))
    }
}

#[derive(Clone)]
pub struct Notification {
    pub level: NotificationLevel,
    pub title: String,
    pub message: String,
    pub source: Option<String>,
    pub sticky: bool,
}

#[derive(Clone)]
pub enum PendingInteraction {
    Permission {
        id: String,
        action: String,
        target: String,
        reason: String,
        risk: Option<Risk>,
        selected: usize,
    },
    Confirmation {
        id: String,
        message: String,
        selected: usize,
    },
    Form {
        id: String,
        title: String,
        items: Vec<FormItem>,
        current: usize,
    },
}

#[derive(Clone)]
pub struct FormItem {
    pub title: String,
    pub fields: Vec<FormField>,
    pub focus: usize,
}

#[derive(Clone)]
pub struct FormField {
    pub name: String,
    pub label: String,
    pub value: FormValue,
    /// One-line description shown (dim) under the field while it is focused.
    pub hint: Option<String>,
    /// Advisory only: an empty required field (Text empty / MultiSelect none) surfaces a
    /// warning but NEVER blocks submit — the agent re-asks if it needs the value.
    pub required: bool,
}

/// Text field content constraint. Advisory: a mismatch shows a warning, never blocks.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum TextFormat {
    #[default]
    Any,
    Number,
    Integer,
}

/// Why a field is flagged. Rendered as a warning line; the classification is pure so it
/// can be unit-tested. Advisory — see `FormField::warning`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldWarning {
    Required,
    NotNumber,
    NotInteger,
}

impl FormField {
    /// Advisory warning for the field's current value, if any. Never affects submit.
    pub fn warning(&self) -> Option<FieldWarning> {
        match &self.value {
            FormValue::Text { value, format, .. } => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return self.required.then_some(FieldWarning::Required);
                }
                match format {
                    TextFormat::Any => None,
                    TextFormat::Number => {
                        (trimmed.parse::<f64>().is_err()).then_some(FieldWarning::NotNumber)
                    }
                    TextFormat::Integer => {
                        (trimmed.parse::<i64>().is_err()).then_some(FieldWarning::NotInteger)
                    }
                }
            }
            FormValue::MultiSelect { selected, .. } => {
                (self.required && selected.is_empty()).then_some(FieldWarning::Required)
            }
            // Checkbox / Select always carry a value, so "required" is a no-op there.
            _ => None,
        }
    }
}

#[derive(Clone)]
pub enum FormValue {
    Text {
        value: String,
        placeholder: String,
        /// Caret as a CHAR index into `value` (0..=char count).
        cursor: usize,
        format: TextFormat,
    },
    Checkbox {
        checked: bool,
    },
    Select {
        options: Vec<String>,
        selected: usize,
    },
    MultiSelect {
        options: Vec<String>,
        selected: Vec<usize>,
        cursor: usize,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InputHistoryEntry {
    pub messages: Vec<Message>,
    pub display: String,
}

pub struct InputHistory {
    pub entries: Vec<InputHistoryEntry>,
    pub cursor: Option<usize>,
    pub draft: Option<String>,
}

/// Fullscreen scrollable text viewer (zoom, simplified): pre-rendered
/// lines + a clamped scroll offset. Lines are wrapped at OPEN time against the
/// current terminal width (correct scroll/percent math beats live rewrap; reopen
/// after a resize to rewrap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomKind {
    Document,
    SessionInfo,
}

pub struct ZoomOverlay {
    pub title: String,
    pub lines: Vec<Line<'static>>,
    pub scroll: usize,
    pub kind: ZoomKind,
    /// Raw source copied by `c`. Rendered lines are deliberately not used because
    /// they contain wrapping and terminal-only styling decisions.
    pub copy_text: Option<String>,
    /// Immediate copy result shown in the fullscreen footer, where normal status
    /// notifications are hidden by the overlay.
    pub copy_feedback: Option<String>,
    /// Overlay to restore when the viewer closes. The single-overlay model has no
    /// stack: a viewer opened from Sessions REPLACES it and puts it back on Esc.
    pub restore: Option<Box<FullscreenOverlay>>,
}

/// Rows of zoom content visible at `term_rows`: full screen minus the card's
/// top/bottom border and the footer hint row. Shared by the key handler (scroll
/// clamping) and the renderer so both agree on page size.
pub fn zoom_content_rows(term_rows: u16) -> usize {
    const ZOOM_CHROME_ROWS: usize = 3; // top border + bottom border + footer
    (term_rows as usize).saturating_sub(ZOOM_CHROME_ROWS).max(1)
}

pub enum FullscreenOverlay {
    Help(HelpOverlay),
    Stats(StatsOverlay),
    Sessions(SessionsOverlay),
    Workspaces(WorkspacesOverlay),
    Models(ModelsOverlay),
    Zoom(ZoomOverlay),
    Replay(ReplayOverlay),
}

pub struct ReplayOverlay {
    pub turns: Vec<zlogic_protocol::query::TurnItem>,
    pub total: u64,
    pub selected: usize,
    pub scroll: usize,
    pub loading: bool,
    pub failed: bool,
    pub detail: Option<ReplayDetail>,
}

pub struct ReplayDetail {
    pub turn_seq: u32,
    pub rounds: Vec<ReplayRound>,
    pub lines: Vec<HistLine>,
    pub head_at: Vec<usize>,
    pub selected_round: usize,
    pub scroll: usize,
    pub failed: bool,
}

pub struct ReplayRound {
    pub head: HistLine,
    pub open: bool,
    pub body: Vec<HistLine>,
}

pub fn flatten_rounds(rounds: &[ReplayRound]) -> (Vec<HistLine>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut head_at = Vec::with_capacity(rounds.len());
    for round in rounds {
        head_at.push(lines.len());
        lines.push(round.head.clone());
        if round.open {
            lines.extend(round.body.iter().cloned());
        }
    }
    (lines, head_at)
}

#[allow(clippy::large_enum_variant)]
pub enum OverlayState {
    Floating(CompletionState),
    Fullscreen(FullscreenOverlay),
}

/// The in-flight round's live state (viewport only; settles into history on Done).
#[derive(Default)]
pub struct Live {
    pub active: bool,
    pub turn_id: Option<String>,
    pub assistant_header_emitted: bool,
    pub rounds: Vec<ReasoningRound>,
    pub current_round: Option<usize>,
    pub awaiting_summary: bool,
    pub summary: Option<TurnSummary>,
    /// API-refreshed, not-yet-consumed core mailbox entries.
    pub mailbox: Vec<MailboxEntry>,
}

impl Live {
    pub fn start_round(&mut self, id: String) -> &mut ReasoningRound {
        self.rounds.push(ReasoningRound {
            id,
            ..Default::default()
        });
        self.current_round = Some(self.rounds.len() - 1);
        self.rounds.last_mut().expect("round was just inserted")
    }

    pub fn current_round_mut(&mut self) -> &mut ReasoningRound {
        let index = self.current_round.unwrap_or_else(|| {
            self.rounds.push(ReasoningRound {
                id: "implicit".into(),
                ..Default::default()
            });
            self.rounds.len() - 1
        });
        self.current_round = Some(index);
        &mut self.rounds[index]
    }
}

pub struct AppState {
    pub i18n: I18n,
    pub theme: ThemeState,
    pub icons: IconTier,
    pub terminal_kind: TerminalKind,
    pub mode: Mode,
    /// Persistent startup/runtime flags. `mode` temporarily becomes Streaming,
    /// so turn options must not be inferred from that transient display state.
    pub plan_enabled: bool,
    pub god_enabled: bool,

    /// Finished lines waiting to be `insert_before`'d into scrollback this frame.
    pub history_outbox: Vec<Line<'static>>,
    /// Settled logical scrollback retained so resize can clear + rewrap + replay.
    transcript: VecDeque<HistLine>,
    /// Number of wrapped transcript rows already handed to the terminal scrollback.
    flushed_history_lines: usize,
    pub live: Live,
    /// Composer destination while a turn is streaming. Defaults to the safer
    /// follow-up queue; Ctrl+T switches to current-turn mailbox injection.
    pub streaming_send_target: StreamingSendTarget,
    /// CLI-owned follow-up turns. These never enter core's current-turn mailbox.
    pub next_turn_queue: VecDeque<QueuedTurn>,

    /// The input is a plain string for now; the segmented composer comes later.
    pub input: String,
    /// Caret position inside `input`, in CHAR index (0 = before the first char,
    /// `input.chars().count()` = end). ←/→/Home/End move it; edits land there.
    pub input_cursor: usize,
    pub input_history: InputHistory,
    pub input_before_interaction: Option<String>,
    pub chips: Vec<Chip>,
    /// Exact structured payload restored from input history. Any edit clears it;
    /// an unchanged resubmit sends these messages byte-for-byte.
    pub history_replay: Option<Vec<Message>>,
    pub file_scan_generation: u64,
    pub file_scan_request: Option<FileScanRequest>,
    pub test_request: Option<String>,
    pub overlay_char_guard_until: Instant,
    pub key_bursts: VecDeque<Instant>,
    pub esc_anchored_at: Option<Instant>,
    pub overlay: Option<OverlayState>,
    pub notifications: VecDeque<Notification>,
    notification_expires: VecDeque<Option<Instant>>,
    pub pending_interaction: Option<PendingInteraction>,

    pub spinner_frame: usize,
    pub dirty: bool,
    pub should_quit: bool,
    pub quit_armed_at: Option<Instant>,
    pub esc_count: u8,
    pub cancel_requested: bool,
    /// Set when the last state change was a stream delta (for 50ms throttle).
    pub pending_stream_only: bool,
    pub status: String,
    /// Deadline for the transient status line above the input (`notification_bar_line`):
    /// command feedback auto-clears after `STATUS_VISIBLE_FOR`; `None` = persists
    /// (zoom-hint / no-usable-model / boot messages that must stay until resolved).
    pub status_expires: Option<Instant>,
    pub status_snapshot: StatusSnapshot,
    pub permission_mode: PermissionMode,
    pub view_mode: ViewMode,
    /// Terminal width (for centering flushed splash lines; view uses live area width).
    pub width: u16,
    /// Terminal height in rows (updated on resize). Used by handlers that need a
    /// page size without a Frame (e.g. zoom overlay scroll clamping).
    pub term_rows: u16,
    /// Raw text of the last settled assistant reply — the fidelity source for the
    /// Ctrl+O zoom viewer (the transcript only retains rendered/wrapped lines).
    pub last_reply: Option<String>,

    // ── splash ──
    pub stage: Stage,
    pub boot_step: usize,
    pub boot_hold_ticks: usize,
    /// Static help block shown after boot until the first submission.
    pub help_shown: bool,
    /// Indices into `splash::TIPS`, chosen pseudo-randomly per launch.
    pub help: Vec<usize>,
    pub exit_resume_tip: Option<String>,
    pub anim_ticks: u64,
}

impl AppState {
    pub fn new(
        theme: ThemeState,
        icons: IconTier,
        width: u16,
        viewport_height: u16,
        term_rows: u16,
    ) -> Self {
        Self::with_locale(
            theme,
            icons,
            width,
            viewport_height,
            term_rows,
            Locale::default(),
        )
    }

    pub fn with_locale(
        theme: ThemeState,
        icons: IconTier,
        width: u16,
        viewport_height: u16,
        term_rows: u16,
        locale: Locale,
    ) -> Self {
        let mut s = Self {
            i18n: I18n::new(locale),
            theme,
            icons,
            terminal_kind: detect_terminal_kind(),
            mode: Mode::Normal,
            plan_enabled: false,
            god_enabled: false,
            history_outbox: Vec::new(),
            transcript: VecDeque::new(),
            flushed_history_lines: 0,
            live: Live::default(),
            streaming_send_target: StreamingSendTarget::default(),
            next_turn_queue: VecDeque::new(),
            input: String::new(),
            input_cursor: 0,
            input_history: load_input_history(),
            input_before_interaction: None,
            chips: Vec::new(),
            history_replay: None,
            file_scan_generation: 0,
            file_scan_request: None,
            test_request: None,
            overlay_char_guard_until: Instant::now(),
            key_bursts: VecDeque::new(),
            esc_anchored_at: None,
            overlay: None,
            notifications: VecDeque::new(),
            notification_expires: VecDeque::new(),
            pending_interaction: None,
            spinner_frame: 0,
            dirty: true,
            should_quit: false,
            quit_armed_at: None,
            esc_count: 0,
            cancel_requested: false,
            pending_stream_only: false,
            status: String::new(),
            status_expires: None,
            status_snapshot: StatusSnapshot {
                cwd: display_cwd(),
                ..Default::default()
            },
            permission_mode: PermissionMode::Auto,
            view_mode: load_view_mode(),
            width: width.max(1),
            term_rows: term_rows.max(1),
            last_reply: None,
            stage: Stage::Boot,
            boot_step: 0,
            boot_hold_ticks: 0,
            help_shown: false,
            help: Vec::new(),
            exit_resume_tip: None,
            anim_ticks: 0,
        };
        // Pick a few help tips at random for this launch (time-seeded; std only).
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize)
            .unwrap_or(0);
        s.help = splash::pick_tips(splash::tip_count_for_rows(term_rows), seed);
        s.seed_splash(term_rows, viewport_height);
        s
    }

    pub fn resize(&mut self, width: u16, rows: u16) {
        self.width = width.max(1);
        self.term_rows = rows.max(1);
        self.dirty = true;
    }

    pub fn refresh_status_snapshot(&mut self, session: &dyn CoreSession) {
        let usage = session.usage_overview();
        let configured_model = session.config_snapshot().current_model;
        let models = session.model_catalog();
        let current_model = models.iter().find(|model| model.is_current).or_else(|| {
            configured_model
                .as_ref()
                .and_then(|name| models.iter().find(|model| &model.name == name))
        });
        let model = current_model
            .map(|model| model.name.clone())
            .or(configured_model);
        self.status_snapshot.cwd = session
            .session_cwd()
            .map(|cwd| shorten_home(&cwd))
            .unwrap_or_else(display_cwd);
        self.status_snapshot.session_id = session.current_session_id();
        self.status_snapshot.session_title = self
            .status_snapshot
            .session_id
            .as_ref()
            .and_then(|id| {
                session
                    .list_sessions()
                    .into_iter()
                    .find(|summary| &summary.id == id)
            })
            .map(|summary| summary.title);
        self.status_snapshot.provider = current_model.map(|model| model.provider.clone());
        self.status_snapshot.model = model;
        self.status_snapshot.total_tokens = usage.total_tokens;
        if !self.status_snapshot.context_is_live {
            self.status_snapshot.context_used_tokens = usage.context_used_tokens;
            self.status_snapshot.context_limit_tokens = usage.context_limit_tokens;
        }
        self.dirty = true;
    }

    pub fn clear_overlay(&mut self) {
        if self.overlay.is_some() {
            self.overlay_char_guard_until = Instant::now() + Duration::from_millis(150);
        }
        self.overlay = None;
    }

    pub fn take_test_request(&mut self) -> Option<String> {
        self.test_request.take()
    }

    pub fn model_test_running(&self) -> bool {
        matches!(
            &self.overlay,
            Some(OverlayState::Fullscreen(FullscreenOverlay::Models(overlay)))
                if overlay.test.as_ref().is_some_and(|test| test.result.is_none())
        )
    }

    /// Reset the per-session view when the active session changes (`/new`, switch):
    /// retained transcript, pending outbox, and the live area. Rows already flushed
    /// into terminal scrollback stay there (that's real scrollback by design); this
    /// only clears what the viewport would replay.
    pub fn reset_conversation_view(&mut self) {
        self.transcript.clear();
        self.history_outbox.clear();
        self.flushed_history_lines = 0;
        self.live = Live::default();
        self.streaming_send_target = StreamingSendTarget::default();
        self.next_turn_queue.clear();
        self.last_reply = None;
        self.pending_stream_only = false;
        self.status_snapshot.context_is_live = false;
        self.status_snapshot.context_used_tokens = 0;
        self.status_snapshot.context_limit_tokens = 0;
        self.dirty = true;
    }

    pub fn push_notification(&mut self, notification: Notification) {
        const LIMIT: usize = 5;
        const VISIBLE_FOR: Duration = Duration::from_secs(4);
        if self.notifications.len() >= LIMIT {
            self.notifications.pop_front();
            self.notification_expires.pop_front();
        }
        let expires = (!notification.sticky).then(|| Instant::now() + VISIBLE_FOR);
        self.notifications.push_back(notification);
        self.notification_expires.push_back(expires);
        self.dirty = true;
    }

    pub fn expire_notifications(&mut self, now: Instant) {
        let mut changed = false;
        while self
            .notification_expires
            .back()
            .is_some_and(|deadline| deadline.is_some_and(|deadline| now >= deadline))
        {
            self.notifications.pop_back();
            self.notification_expires.pop_back();
            changed = true;
        }
        if changed {
            self.dirty = true;
        }
    }

    pub fn clear_notifications(&mut self) {
        self.notifications.clear();
        self.notification_expires.clear();
        self.dirty = true;
    }

    /// Transient status line (above input) with an auto-clear deadline.
    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.status_expires = Some(Instant::now() + STATUS_VISIBLE_FOR);
        self.dirty = true;
    }

    /// Persistent status (zoom-hint / no-usable-model): no deadline.
    pub fn set_status_persistent(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.status_expires = None;
        self.dirty = true;
    }

    pub fn expire_status(&mut self, now: Instant) {
        if self.status_expires.is_some_and(|deadline| now >= deadline) {
            self.status.clear();
            self.status_expires = None;
            self.dirty = true;
        }
    }

    pub fn clear_status(&mut self) {
        if self.status.is_empty() && self.status_expires.is_none() {
            return;
        }
        self.status.clear();
        self.status_expires = None;
        self.dirty = true;
    }

    pub fn show_exit_splash(&mut self, resume_tip: String) {
        self.stage = Stage::Exit;
        self.exit_resume_tip = Some(resume_tip);
        self.clear_overlay();
        self.input.clear();
        self.clear_status();
        self.live = Live::default();
        self.dirty = true;
    }

    pub fn set_command_suggest(&mut self, suggest: SuggestState) {
        self.overlay = Some(OverlayState::Floating(CompletionState::Command(suggest)));
    }

    pub fn set_file_picker(&mut self, picker: FilePicker) {
        self.overlay = Some(OverlayState::Floating(CompletionState::File(picker)));
    }

    pub fn set_stats_overlay(
        &mut self,
        usage: UsageSnapshot,
        by_model: Vec<ModelUsage>,
        timeline: Vec<TurnUsage>,
    ) {
        self.overlay = Some(OverlayState::Fullscreen(FullscreenOverlay::Stats(
            StatsOverlay {
                usage,
                range: StatsRange::SevenDays,
                by_model,
                timeline,
                scroll: 0,
            },
        )));
    }

    pub fn set_help_overlay(&mut self, commands: Vec<CommandSpec>) {
        self.overlay = Some(OverlayState::Fullscreen(FullscreenOverlay::Help(
            HelpOverlay {
                commands,
                selected: 0,
            },
        )));
    }

    pub fn set_replay_overlay(&mut self, turns: Vec<zlogic_protocol::query::TurnItem>, total: u64) {
        self.overlay = Some(OverlayState::Fullscreen(FullscreenOverlay::Replay(
            ReplayOverlay {
                turns,
                total,
                selected: 0,
                scroll: 0,
                loading: false,
                failed: false,
                detail: None,
            },
        )));
    }

    pub fn set_models_overlay(
        &mut self,
        mut providers: Vec<ProviderEntry>,
        models: Vec<ModelEntry>,
        keys: Vec<KeyEntry>,
        config_path: Option<String>,
        filter: String,
    ) {
        sort_providers_by_key(&mut providers);
        let rows = model_rows(&providers, &models);
        // Open with the cursor on the current model when it's in the list, else the
        // first row.
        let selected = rows
            .iter()
            .position(
                |row| matches!(row, ModelRow::Model { model, .. } if models[*model].is_current),
            )
            .unwrap_or(0);
        self.overlay = Some(OverlayState::Fullscreen(FullscreenOverlay::Models(
            ModelsOverlay {
                rows,
                selected,
                providers,
                models,
                keys,
                test: None,
                detail_scroll: 0,
                mode: ModelOverlayMode::Browse,
                form_error: None,
                confirm: None,
                config_path,
                filter,
                pending_switch: None,
            },
        )));
    }

    pub fn set_sessions_overlay(&mut self, overlay: SessionsOverlay) {
        self.overlay = Some(OverlayState::Fullscreen(FullscreenOverlay::Sessions(
            overlay,
        )));
    }

    pub fn set_workspaces_overlay(&mut self, overlay: WorkspacesOverlay) {
        self.overlay = Some(OverlayState::Fullscreen(FullscreenOverlay::Workspaces(
            overlay,
        )));
    }

    pub fn set_locale(&mut self, locale: Locale) {
        self.i18n.set_locale(locale);
        self.dirty = true;
    }

    pub fn command_suggest(&self) -> Option<&SuggestState> {
        match &self.overlay {
            Some(OverlayState::Floating(CompletionState::Command(suggest))) => Some(suggest),
            _ => None,
        }
    }

    pub fn command_suggest_mut(&mut self) -> Option<&mut SuggestState> {
        match &mut self.overlay {
            Some(OverlayState::Floating(CompletionState::Command(suggest))) => Some(suggest),
            _ => None,
        }
    }

    pub fn file_picker(&self) -> Option<&FilePicker> {
        match &self.overlay {
            Some(OverlayState::Floating(CompletionState::File(picker))) => Some(picker),
            _ => None,
        }
    }

    pub fn file_picker_mut(&mut self) -> Option<&mut FilePicker> {
        match &mut self.overlay {
            Some(OverlayState::Floating(CompletionState::File(picker))) => Some(picker),
            _ => None,
        }
    }

    pub fn fullscreen_overlay(&self) -> Option<&FullscreenOverlay> {
        match &self.overlay {
            Some(OverlayState::Fullscreen(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn fullscreen_overlay_mut(&mut self) -> Option<&mut FullscreenOverlay> {
        match &mut self.overlay {
            Some(OverlayState::Fullscreen(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn is_command_suggest_open(&self) -> bool {
        self.command_suggest().is_some()
    }

    pub fn is_file_picker_open(&self) -> bool {
        self.file_picker().is_some()
    }

    pub fn overlay_is_floating(&self) -> bool {
        matches!(self.overlay, Some(OverlayState::Floating(_)))
    }

    pub fn overlay_is_fullscreen(&self) -> bool {
        matches!(self.overlay, Some(OverlayState::Fullscreen(_)))
    }

    pub fn push_history_line(&mut self, line: HistLine) {
        if self.transcript.len() >= TRANSCRIPT_LIMIT {
            if let Some(old) = self.transcript.pop_front() {
                let removed_rows = self.wrap_history_line(&old).len();
                self.flushed_history_lines =
                    self.flushed_history_lines.saturating_sub(removed_rows);
            }
        }
        self.transcript.push_back(line);
    }

    pub fn reset_scrollback_replay(&mut self) {
        self.history_outbox.clear();
        self.flushed_history_lines = 0;
        self.dirty = true;
    }

    pub fn queue_scrollback_until_tail(&mut self, tail_rows: usize) {
        let wrapped = self.wrapped_history_lines();
        let target_flushed = wrapped.len().saturating_sub(tail_rows);
        if target_flushed <= self.flushed_history_lines {
            return;
        }
        self.history_outbox.extend(
            wrapped[self.flushed_history_lines..target_flushed]
                .iter()
                .cloned(),
        );
        self.flushed_history_lines = target_flushed;
    }

    pub fn history_tail_lines(&self, tail_rows: usize) -> Vec<Line<'static>> {
        if tail_rows == 0 {
            return Vec::new();
        }
        let wrapped = self.wrapped_history_lines();
        let start = wrapped
            .len()
            .saturating_sub(tail_rows)
            .max(self.flushed_history_lines.min(wrapped.len()));
        wrapped.into_iter().skip(start).collect()
    }

    fn history_width(&self) -> usize {
        usize::from(self.width).clamp(1, 120)
    }

    fn wrap_history_line(&self, line: &HistLine) -> Vec<Line<'static>> {
        let align_width = usize::from(self.width.max(1));
        let wrap_width = match line.align {
            HistAlign::Left => self.history_width(),
            HistAlign::CenterIn(_) => align_width,
        };
        wrap_hist_line(line, wrap_width, align_width)
    }

    fn wrapped_history_lines(&self) -> Vec<Line<'static>> {
        self.transcript
            .iter()
            .flat_map(|line| self.wrap_history_line(line))
            .collect()
    }

    /// Seed the initial visible transcript. The dynamic viewport shows these rows at
    /// startup; as real history arrives, they naturally slide into terminal scrollback.
    fn seed_splash(&mut self, term_rows: u16, viewport_height: u16) {
        let _ = term_rows;
        let idle_input_and_status = 4;
        let top_padding = splash::top_padding(
            viewport_height,
            self.help.len(),
            idle_input_and_status,
            self.icons,
        );
        for _ in 0..top_padding {
            self.push_history_line(HistLine::blank());
        }
        for row in 0..splash::logo_height(self.icons) as usize {
            let spans = splash::logo_row_chunks(&self.theme, self.icons, row)
                .into_iter()
                .map(|(text, color)| HistSpan::styled(text, Style::default().fg(color)))
                .collect();
            self.push_history_line(HistLine::centered_in(
                spans,
                splash::logo_width(self.icons) as usize,
            ));
        }
        self.push_history_line(HistLine::blank());

        for i in self.help.clone() {
            let (cmd, desc) = splash::TIPS[i % splash::TIPS.len()];
            self.push_history_line(HistLine::centered_in(
                vec![
                    HistSpan::raw(splash::tip_inner_padding(self.icons)),
                    HistSpan::styled(
                        splash::left_aligned_cmd(cmd),
                        Style::default().fg(self.theme.color(Sem::AccentSoft)),
                    ),
                    HistSpan::raw(splash::TIP_GAP),
                    HistSpan::styled(desc.to_string(), self.theme.style(Sem::Muted)),
                ],
                splash::tip_group_width(self.icons),
            ));
        }
    }

    /// Whether the loop should keep ticking. Transient notifications also need a
    /// short-lived clock while the app is otherwise idle so they can disappear.
    pub fn needs_animation(&self) -> bool {
        self.stage == Stage::Boot
            || (self.live.active && !self.live.awaiting_summary)
            || self.notification_expires.iter().any(Option::is_some)
            || self
                .status_expires
                .is_some_and(|deadline| Instant::now() < deadline)
            || self.model_test_running()
            || self
                .quit_armed_at
                .is_some_and(|t| Instant::now().duration_since(t) < Duration::from_secs(2))
    }
}

fn display_cwd() -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return ".".into();
    };
    shorten_home(&cwd.display().to_string())
}

fn shorten_home(path: &str) -> String {
    let home = std::env::var("HOME").ok();
    if let Some(home) = home {
        let home = std::path::PathBuf::from(home);
        if let Ok(stripped) = std::path::Path::new(path).strip_prefix(&home) {
            let rest = stripped.display().to_string();
            return if rest.is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest)
            };
        }
    }
    path.to_string()
}

pub fn input_history_path() -> PathBuf {
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join(".zlogic").join("input-history.json")
}

fn view_mode_path() -> PathBuf {
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join(".zlogic").join("view-mode.json")
}

#[cfg(not(test))]
fn load_view_mode() -> ViewMode {
    std::fs::read_to_string(view_mode_path())
        .ok()
        .and_then(|text| serde_json::from_str::<ViewMode>(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
fn load_view_mode() -> ViewMode {
    ViewMode::default()
}

// Test builds must not read the developer's real ~/.zlogic/input-history.json —
// mirrors the #[cfg(test)] no-op on save_input_history in update.rs.
#[cfg(not(test))]
fn load_input_history() -> InputHistory {
    let entries = std::fs::read_to_string(input_history_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<InputHistoryEntry>>(&text).ok())
        .unwrap_or_default();
    InputHistory {
        entries,
        cursor: None,
        draft: None,
    }
}

#[cfg(test)]
fn load_input_history() -> InputHistory {
    InputHistory {
        entries: Vec::new(),
        cursor: None,
        draft: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::dto::{KeyStatus, Tier};
    use crate::theme::{themes, ColorTier};

    fn state(width: u16) -> AppState {
        AppState {
            i18n: I18n::default(),
            theme: ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            icons: IconTier::Ascii,
            terminal_kind: TerminalKind::XtermLike,
            mode: Mode::Normal,
            plan_enabled: false,
            god_enabled: false,
            history_outbox: Vec::new(),
            transcript: VecDeque::new(),
            flushed_history_lines: 0,
            live: Live::default(),
            streaming_send_target: StreamingSendTarget::default(),
            next_turn_queue: VecDeque::new(),
            input: String::new(),
            input_cursor: 0,
            input_history: InputHistory {
                entries: Vec::new(),
                cursor: None,
                draft: None,
            },
            input_before_interaction: None,
            chips: Vec::new(),
            history_replay: None,
            file_scan_generation: 0,
            file_scan_request: None,
            test_request: None,
            overlay_char_guard_until: Instant::now(),
            key_bursts: VecDeque::new(),
            esc_anchored_at: None,
            overlay: None,
            notifications: VecDeque::new(),
            notification_expires: VecDeque::new(),
            pending_interaction: None,
            spinner_frame: 0,
            dirty: true,
            should_quit: false,
            quit_armed_at: None,
            esc_count: 0,
            cancel_requested: false,
            pending_stream_only: false,
            status: String::new(),
            status_expires: None,
            status_snapshot: StatusSnapshot::default(),
            permission_mode: PermissionMode::Auto,
            view_mode: ViewMode::default(),
            width,
            term_rows: 24,
            last_reply: None,
            stage: Stage::Ready,
            boot_step: 0,
            boot_hold_ticks: 0,
            help_shown: false,
            help: Vec::new(),
            exit_resume_tip: None,
            anim_ticks: 0,
        }
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn outbox_text(state: &AppState) -> Vec<String> {
        state.history_outbox.iter().map(line_text).collect()
    }

    fn tail_text(state: &AppState, rows: usize) -> Vec<String> {
        state
            .history_tail_lines(rows)
            .iter()
            .map(line_text)
            .collect()
    }

    #[test]
    fn transient_notifications_expire_but_sticky_interactions_remain() {
        let mut s = state(80);
        s.push_notification(Notification {
            level: NotificationLevel::Info,
            title: "temporary".into(),
            message: "done".into(),
            source: None,
            sticky: false,
        });
        let transient_deadline = s.notification_expires.back().unwrap().unwrap();
        s.expire_notifications(transient_deadline + Duration::from_millis(1));
        assert!(s.notifications.is_empty());

        s.push_notification(Notification {
            level: NotificationLevel::Info,
            title: "interaction".into(),
            message: "answer required".into(),
            source: None,
            sticky: true,
        });
        s.expire_notifications(Instant::now() + Duration::from_secs(60));
        assert_eq!(s.notifications.len(), 1);
    }

    #[test]
    fn transient_status_auto_clears_but_persistent_status_stays() {
        let mut s = state(80);
        s.set_status("model gpt-4");
        let deadline = s.status_expires.expect("transient status has a deadline");
        assert_eq!(s.status, "model gpt-4");
        s.expire_status(deadline - Duration::from_millis(1));
        assert_eq!(s.status, "model gpt-4");
        s.expire_status(deadline + Duration::from_millis(1));
        assert!(s.status.is_empty());
        assert!(s.status_expires.is_none());

        s.set_status_persistent("no usable model");
        s.expire_status(Instant::now() + Duration::from_secs(3600));
        assert_eq!(s.status, "no usable model");
    }

    #[test]
    fn startup_splash_remains_in_visible_tail() {
        let s = AppState::new(
            ThemeState {
                theme: themes::dark::theme(),
                tier: ColorTier::Rich,
            },
            IconTier::Ascii,
            120,
            24,
            24,
        );

        let tail = tail_text(&s, 20);
        assert!(tail.iter().any(|line| line.contains("_____")));
        assert!(tail.iter().any(|line| line.contains("/help")));
        assert!(s.history_outbox.is_empty());
    }

    #[test]
    fn scrollback_flush_keeps_recent_tail_in_viewport() {
        let mut s = state(80);
        for n in 1..=5 {
            s.push_history_line(HistLine::raw(format!("line {n}")));
        }

        s.queue_scrollback_until_tail(2);

        assert_eq!(outbox_text(&s), vec!["line 1", "line 2", "line 3"]);
        assert_eq!(tail_text(&s, 2), vec!["line 4", "line 5"]);
    }

    #[test]
    fn scrollback_flush_is_incremental() {
        let mut s = state(80);
        for n in 1..=4 {
            s.push_history_line(HistLine::raw(format!("line {n}")));
        }

        s.queue_scrollback_until_tail(2);
        s.history_outbox.clear();
        s.push_history_line(HistLine::raw("line 5"));
        s.queue_scrollback_until_tail(2);

        assert_eq!(outbox_text(&s), vec!["line 3"]);
        assert_eq!(tail_text(&s, 2), vec!["line 4", "line 5"]);
    }

    #[test]
    fn zoom_content_rows_reserves_chrome_and_never_hits_zero() {
        assert_eq!(zoom_content_rows(24), 21);
        assert_eq!(zoom_content_rows(4), 1);
        assert_eq!(zoom_content_rows(0), 1);
    }

    #[test]
    fn resize_replay_rewraps_from_transcript() {
        let mut s = state(20);
        s.push_history_line(HistLine::raw("abcdefghij"));
        s.queue_scrollback_until_tail(0);
        assert_eq!(outbox_text(&s), vec!["abcdefghij"]);

        s.width = 5;
        s.reset_scrollback_replay();
        s.queue_scrollback_until_tail(1);

        assert_eq!(outbox_text(&s), vec!["abcde"]);
        assert_eq!(tail_text(&s, 1), vec!["fghij"]);
    }

    #[test]
    fn model_rows_group_each_provider_before_its_models() {
        let providers = vec![
            ProviderEntry {
                name: "openai".into(),
                sdk: "openai".into(),
                base_url: None,
                key: KeyStatus::Missing,
            },
            ProviderEntry {
                name: "anthropic".into(),
                sdk: "anthropic".into(),
                base_url: None,
                key: KeyStatus::Present,
            },
        ];
        let models = vec![
            ModelEntry {
                name: "openai:gpt-4o".into(),
                provider: "openai".into(),
                tier: Tier::Main,
                vision: true,
                key: KeyStatus::Missing,
                price: "—".into(),
                context_window: 128_000,
                is_current: false,
            },
            ModelEntry {
                name: "anthropic:claude".into(),
                provider: "anthropic".into(),
                tier: Tier::Thinking,
                vision: false,
                key: KeyStatus::Present,
                price: "—".into(),
                context_window: 200_000,
                is_current: true,
            },
        ];
        let rows = model_rows(&providers, &models);
        assert_eq!(
            rows,
            vec![
                ModelRow::Provider(0),
                ModelRow::Model {
                    provider: 0,
                    model: 0
                },
                ModelRow::Provider(1),
                ModelRow::Model {
                    provider: 1,
                    model: 1
                },
            ]
        );
        // Opening the overlay puts the cursor on the current model (openai-gpt row is
        // skipped because the current model lives under anthropic).
        let overlay = ModelsOverlay {
            rows: rows.clone(),
            selected: rows
                .iter()
                .position(
                    |row| matches!(row, ModelRow::Model { model, .. } if models[*model].is_current),
                )
                .unwrap(),
            providers,
            models,
            keys: Vec::new(),
            test: None,
            detail_scroll: 0,
            mode: ModelOverlayMode::Browse,
            form_error: None,
            confirm: None,
            config_path: None,
            filter: String::new(),
            pending_switch: None,
        };
        assert_eq!(
            overlay.selected_row(),
            Some(ModelRow::Model {
                provider: 1,
                model: 1
            })
        );
    }

    #[test]
    fn sort_providers_by_key_puts_keyed_providers_first() {
        let mut providers = vec![
            ProviderEntry {
                name: "openai".into(),
                sdk: "openai".into(),
                base_url: None,
                key: KeyStatus::Missing,
            },
            ProviderEntry {
                name: "dashscope".into(),
                sdk: "dashscope".into(),
                base_url: None,
                key: KeyStatus::Present,
            },
            ProviderEntry {
                name: "deepseek".into(),
                sdk: "deepseek".into(),
                base_url: None,
                key: KeyStatus::Missing,
            },
            ProviderEntry {
                name: "anthropic".into(),
                sdk: "anthropic".into(),
                base_url: None,
                key: KeyStatus::Env,
            },
        ];
        sort_providers_by_key(&mut providers);
        let order: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
        // Keyed (Present / Env) first, stable within each group: dashscope, anthropic,
        // then the still-missing openai and deepseek in their original order.
        assert_eq!(order, vec!["dashscope", "anthropic", "openai", "deepseek"]);
    }

    #[test]
    fn filter_catalog_matches_names_case_insensitively() {
        let provider = |name: &str, key: KeyStatus| ProviderEntry {
            name: name.into(),
            sdk: name.into(),
            base_url: None,
            key,
        };
        let model = |name: &str, provider: &str| ModelEntry {
            name: name.into(),
            provider: provider.into(),
            tier: Tier::Main,
            vision: false,
            key: KeyStatus::Missing,
            price: "—".into(),
            context_window: 0,
            is_current: false,
        };
        let providers = vec![
            provider("openai", KeyStatus::Present),
            provider("anthropic", KeyStatus::Missing),
        ];
        let models = vec![
            model("openai:gpt-4o", "openai"),
            model("openai:o1", "openai"),
            model("anthropic:claude-sonnet-5", "anthropic"),
        ];

        let (p, m) = filter_catalog(&providers, &models, "o1");
        assert_eq!(
            p.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            vec!["openai"]
        );
        assert_eq!(
            m.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            vec!["openai:o1"]
        );

        let (p, m) = filter_catalog(&providers, &models, "ANTHROPIC");
        assert_eq!(
            p.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            vec!["anthropic"]
        );
        assert_eq!(m.len(), 1);

        let (p, m) = filter_catalog(&providers, &models, "");
        assert_eq!(p.len(), 2);
        assert_eq!(m.len(), 3);
        let (p, m) = filter_catalog(&providers, &models, "zzz");
        assert!(p.is_empty() && m.is_empty());
    }
}

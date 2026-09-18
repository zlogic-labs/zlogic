use serde::{Deserialize, Serialize};

use crate::error::{ApiError, LocalizedMessage};
use crate::interaction::{InteractionBody, InteractionDecision};
use crate::usage::{ContextUsage, CostTotal, CostView, TokenUsage};

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamEvent {
    pub seq: u64,
    pub session_id: String,
    pub turn_id: String,
    pub agent: AgentRef,
    pub payload: StreamPayload,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    pub name: String,
}

impl AgentRef {
    pub fn root() -> Self {
        Self {
            agent_id: None,
            parent_agent_id: None,
            name: "main".into(),
        }
    }

    pub fn is_root(&self) -> bool {
        self.agent_id.is_none()
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamPayload {
    // ───────────────────────── turn ─────────────────────────
    TurnStart {
        model: ModelRef,
        #[serde(default, skip_serializing_if = "is_false")]
        resumed: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        proactive: bool,
    },
    TurnEnd {
        status: TurnStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        stats: TurnStats,
    },

    // ───────────────────────── round ────────────────────────
    RoundStart {
        round_id: String,
        round_seq: u32,
        model: ModelRef,
    },
    RoundEnd {
        round_id: String,
        outcome: RoundOutcome,
        stats: RoundStats,
    },

    // ─────────────────── assistant blocks ───────────────────
    BlockStart {
        block_id: String,
        index: u32,
        kind: BlockKind,
    },
    BlockDelta {
        block_id: String,
        delta: String,
    },
    BlockEnd {
        block_id: String,
        block: BlockFinal,
    },

    // ───────────────────────── tools ────────────────────────
    ToolDetected {
        block_id: String,
        call_index: u32,
        name: String,
    },
    ToolExecStart {
        call_id: String,
        name: String,
    },
    ToolOutputDelta {
        call_id: String,
        stream: OutputStream,
        chunk: String,
    },
    /// A list, not one card: one call can produce several things worth showing — a build that
    /// wrote three files, or output plus a diff.
    ToolExecEnd {
        call_id: String,
        status: ToolStatus,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        display: Vec<ToolDisplay>,
        /// The exact model-facing result text (a "tool failed: …" line for failures). The live UI
        /// uses it so a failed step shows the concrete error without waiting for reopen.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        summary: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        duration_ms: u64,
    },

    // ───────────────────────── usage ────────────────────────
    Usage {
        round_id: String,
        round: TokenUsage,
        turn: TokenUsage,
        session: TokenUsage,
        context: ContextUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<CostView>,
    },

    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        replaces: (u32, u32),
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_tokens: Option<u64>,
    },
    MailboxConsumed {
        submission_ids: Vec<String>,
        via: Via,
    },
    SteeringInjected {
        entry_id: String,
        submission_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<SteeringContent>,
    },

    InteractionRequired {
        interaction_id: String,
        body: InteractionBody,
    },
    InteractionResolved {
        interaction_id: String,
        decision: InteractionDecision,
    },

    Notice {
        level: NoticeLevel,
        code: String,
        message: LocalizedMessage,
    },
    Error {
        error: ApiError,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SteeringContent {
    User {
        text: String,
    },
    TaskUpdate {
        task_id: String,
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session_id: Option<crate::SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_title: Option<String>,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Reasoning,
    Text,
    ToolCall,
}

/// Tool outcomes within some scope.
/// Counted rather than derived from the event stream: replay is only a bounded transport aid, and a
/// client that attached late may never have seen earlier `ToolExecEnd` events. It must not need the
/// complete render stream to show a total.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStats {
    pub total: u32,
    pub succeeded: u32,
    pub failed: u32,
    /// Refused by policy or by the user. Distinct from `failed`: nothing ran.
    pub denied: u32,
    pub timed_out: u32,
    /// The turn was cancelled before this call ran, or while it was running.
    /// Its own counter for the same reason `denied` has one: an audit view that lumps
    /// "you interrupted me" together with "the tool failed" is telling you something untrue.
    pub cancelled: u32,
    /// Tool calls rejected before execution because their arguments were malformed
    /// (unparseable JSON, or JSON that failed the tool's declared schema).
    /// Counted separately from `failed` so the turn loop can distinguish "the model wrote a bad
    /// call, retry" from "the tool genuinely errored". Every `precheck_failed` call is also
    /// counted in `failed` — it is a failed call, but the *fixable-by-the-model* kind.
    #[serde(default)]
    pub precheck_failed: u32,
}

impl ToolStats {
    pub fn record(&mut self, status: ToolStatus) {
        self.total += 1;
        match status {
            ToolStatus::Completed => self.succeeded += 1,
            ToolStatus::Error => self.failed += 1,
            ToolStatus::PrecheckFailed => {
                self.failed += 1;
                self.precheck_failed += 1;
            }
            ToolStatus::Denied => self.denied += 1,
            ToolStatus::Timeout => self.timed_out += 1,
            ToolStatus::Cancelled => self.cancelled += 1,
        }
    }

    pub fn add(&mut self, other: &ToolStats) {
        self.total += other.total;
        self.succeeded += other.succeeded;
        self.failed += other.failed;
        self.denied += other.denied;
        self.timed_out += other.timed_out;
        self.cancelled += other.cancelled;
        self.precheck_failed += other.precheck_failed;
    }
}

/// What one round cost and did.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoundStats {
    pub tools: ToolStats,
    pub usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostView>,
    /// Wall clock for the round.
    pub duration_ms: u64,
}

/// What a whole turn cost and did.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnStats {
    pub rounds: u32,
    pub tools: ToolStats,
    pub usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostTotal>,
    /// Execution time, **excluding** time spent waiting on the user.
    /// Split from `interaction_wait_ms` because a user thinking is not the agent being slow: a
    /// turn budget must not be consumed by a permission prompt left open over lunch.
    pub duration_ms: u64,
    pub interaction_wait_ms: u64,
    /// How many times the user was asked something.
    pub interactions: u32,
    /// How many times the context was compacted.
    pub compactions: u32,
}

impl TurnStats {
    /// Folds a finished round in.
    pub fn add_round(&mut self, round: &RoundStats) {
        self.rounds += 1;
        self.tools.add(&round.tools);
        self.usage.add(&round.usage);
        self.duration_ms += round.duration_ms;
    }

    /// Total elapsed, including the wait — what a clock on the wall would have shown.
    pub fn elapsed_ms(&self) -> u64 {
        self.duration_ms + self.interaction_wait_ms
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockFinal {
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    ToolCall {
        calls: Vec<UiToolCall>,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiToolCall {
    pub call_id: String,
    pub name: String,
    pub args: String,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Cancelled,
    Failed,
    Incomplete(IncompleteReason),
    LimitReached,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReason {
    MaxOutputTokens,
    ContentFilter,
    Refusal,
    Interrupted,
    UnknownStopReason,
    InvalidToolCalls,
    EmptyReply,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundOutcome {
    FinalAnswer,
    FinalAnswerWithMailbox,
    ToolCalls,
    Truncated,
    Paused,
    Failed,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
    Progress,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Completed,
    Error,
    Denied,
    Timeout,
    Cancelled,
    PrecheckFailed,
}

/// Line counts for a diff.
/// Lives here rather than in the tools crate because it crosses to the UI; `zlogic-tools`
/// re-exports it so there is one definition.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStat {
    pub added: u32,
    pub removed: u32,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Created,
    Modified,
    Deleted,
}

/// It mirrors `zlogic_tools::ToolDisplay` variant for variant, with one difference: an object is a
/// plain id string here. That is what the wire carries anyway (`ObjectId` serialises as
/// `<algo>:<hex>`), and it keeps this crate free of a dependency on the object store — a UI
/// rendering a card needs the id to fetch by, not the hashing machinery behind it.
/// Large payloads are **never inlined**: the card carries the id and the client fetches on demand,
/// which is what powers "view full output" without the transcript costing megabytes.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDisplay {
    /// `math` is an optional parallel rendering for clients that can typeset mathematics — the
    /// same content line by line, formulas as LaTeX. A terminal prints `text`; a GUI draws real
    /// fraction bars and integral signs instead of `/` and `^`.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        math: Vec<MathLine>,
    },
    /// Bounded rows for a graphical table. This projection never enters model context.
    /// `object` is the full table (all rows and cells, untruncated) captured as a text object when
    /// any cell was too large to inline; the UI fetches it on demand for a "view full table" modal.
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<serde_json::Value>>,
        total_rows: u64,
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object: Option<String>,
    },
    Diff {
        path: String,
        stat: DiffStat,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change: Option<FileChange>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object: Option<String>,
    },
    File {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_lines: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        object: Option<String>,
    },
    /// Captured output too large to hand the model in full: it saw head and tail, the user can
    /// see all of it.
    Output {
        object: String,
        total_chars: u64,
        truncated: bool,
    },
    /// A sub-agent run. `session_id` is what makes the child's transcript reachable.
    Agent { agent: String, session_id: String },
    Task {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    /// Sandboxed HTML component. The source lives in the object store and is loaded lazily.
    Widget {
        object: String,
        title: String,
        height: u16,
        libraries: Vec<String>,
    },
}

/// One line of a [`ToolDisplay::Text`] card's mathematical rendering.
/// Mirrors `zlogic_tools::MathLine`. Two fields because prose and mathematics are typeset
/// differently: `label` says what is happening, `latex` is what goes to a math renderer. Either
/// may be empty — a bare equation has no label, a step that only names data has no formula.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MathLine {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// LaTeX **without** delimiters: the client decides inline versus display math.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub latex: String,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Threshold,
    ContextOverflow,
    Manual,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Turn,
    Steer,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
}

/// Streams tool output to the UI as it is produced.
/// Lives here rather than in the tools crate for the same reason as `InteractionPort`: tools emit,
/// core forwards, and a definition in either place would have to be duplicated.
/// # Synchronous on purpose
/// A tool reading a subprocess pipe does so in a tight loop; making this `async` would put an
/// `await` in that loop and invites a backpressure deadlock — the tool waiting on the UI while the
/// UI waits on the turn. Implementations **must not block**: drop or coalesce instead. Losing a
/// chunk of progress output is a cosmetic problem; stalling the tool is not.
/// # These deltas are not persisted per chunk
/// Thousands of rows per build would swamp the database and the timeline. The full transcript goes
/// to the object store once, at the end, and the entry keeps a reference to it.
pub trait OutputSink: Send + Sync {
    fn emit(&self, stream: OutputStream, chunk: &str);
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutputDelta {
    pub task_id: String,
    pub stream: OutputStream,
    pub chunk: String,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateNotice {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub change: StateChange,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateChange {
    TurnStateChanged,
    InteractionPending,
    InteractionResolved,
    SessionMetaChanged,
    MailboxChanged,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInitProgress {
    pub workspace_id: String,
    pub root: String,
    pub phase: InitPhase,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum InitPhase {
    Started,
    Scanning {
        files: u64,
        bytes: u64,
    },
    Done {
        files: u64,
        bytes: u64,
        skipped: u32,
        elapsed_ms: u64,
        unchanged: bool,
    },
    Cancelled,
    Failed {
        message: String,
    },
    Disabled,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_stats_separate_refusal_from_failure() {
        let mut s = ToolStats::default();
        s.record(ToolStatus::Completed);
        s.record(ToolStatus::Error);
        s.record(ToolStatus::PrecheckFailed);
        s.record(ToolStatus::Denied);
        s.record(ToolStatus::Timeout);
        s.record(ToolStatus::Cancelled);

        assert_eq!(s.total, 6);
        assert_eq!(s.succeeded, 1);
        // A precheck failure is a failure: the call was malformed.
        assert_eq!(s.failed, 2);
        // A refusal is not a failure — nothing ran, and the audit view needs the difference.
        assert_eq!(s.denied, 1);
        assert_eq!(s.timed_out, 1);
        // Interrupting the agent is not the tool failing, and an audit view that says otherwise
        // is telling you something untrue.
        assert_eq!(s.cancelled, 1);
        assert_eq!(
            s.failed, 2,
            "cancellation must not have been counted as a failure"
        );
    }

    #[test]
    fn turn_stats_fold_rounds() {
        let mut turn = TurnStats::default();
        for _ in 0..3 {
            let mut round = RoundStats {
                duration_ms: 100,
                ..Default::default()
            };
            round.tools.record(ToolStatus::Completed);
            round.usage.input = 50;
            round.usage.output = 10;
            turn.add_round(&round);
        }
        assert_eq!(turn.rounds, 3);
        assert_eq!(turn.tools.total, 3);
        assert_eq!(turn.usage.input, 150);
        assert_eq!(turn.duration_ms, 300);
    }

    /// A permission prompt left open over lunch must not consume the turn's budget.
    #[test]
    fn waiting_on_the_user_is_not_execution_time() {
        let turn = TurnStats {
            duration_ms: 2_000,
            interaction_wait_ms: 600_000,
            interactions: 1,
            ..Default::default()
        };
        assert_eq!(
            turn.duration_ms, 2_000,
            "the budget only sees execution time"
        );
        assert_eq!(turn.elapsed_ms(), 602_000, "the wall clock sees both");
    }

    #[test]
    fn stats_ride_along_on_the_terminal_events() {
        let ev = StreamEvent {
            seq: 9,
            session_id: "s".into(),
            turn_id: "t".into(),
            agent: AgentRef::root(),
            payload: StreamPayload::TurnEnd {
                status: TurnStatus::Completed,
                reason: None,
                stats: TurnStats {
                    rounds: 2,
                    ..Default::default()
                },
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["payload"]["stats"]["rounds"], 2);
        assert_eq!(serde_json::from_value::<StreamEvent>(v).unwrap(), ev);
    }

    #[test]
    fn round_end_carries_its_own_stats() {
        let p = StreamPayload::RoundEnd {
            round_id: "r".into(),
            outcome: RoundOutcome::ToolCalls,
            stats: RoundStats {
                duration_ms: 42,
                ..Default::default()
            },
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["type"], "round_end");
        assert_eq!(v["stats"]["duration_ms"], 42);
        assert_eq!(serde_json::from_value::<StreamPayload>(v).unwrap(), p);
    }

    #[test]
    fn output_deltas_name_their_stream() {
        for stream in [
            OutputStream::Stdout,
            OutputStream::Stderr,
            OutputStream::Progress,
        ] {
            let p = StreamPayload::ToolOutputDelta {
                call_id: "c".into(),
                stream,
                chunk: "line\n".into(),
            };
            let v = serde_json::to_value(&p).unwrap();
            assert_eq!(v["type"], "tool_output_delta");
            assert_eq!(serde_json::from_value::<StreamPayload>(v).unwrap(), p);
        }
    }

    /// One call can be worth showing in several ways, and a large payload is referenced rather
    /// than inlined.
    #[test]
    fn a_tool_can_end_with_several_cards() {
        let p = StreamPayload::ToolExecEnd {
            call_id: "c1".into(),
            status: ToolStatus::Completed,
            display: vec![
                ToolDisplay::Diff {
                    path: "src/main.rs".into(),
                    stat: DiffStat {
                        added: 2,
                        removed: 1,
                    },
                    change: Some(FileChange::Modified),
                    object: None,
                },
                ToolDisplay::Output {
                    object: "sha256:abc".into(),
                    total_chars: 50_000,
                    truncated: true,
                },
            ],
            summary: "wrote src/main.rs".into(),
            duration_ms: 250,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["display"][0]["kind"], "diff");
        assert!(
            v["display"][0].get("unified").is_none(),
            "absent rather than null"
        );
        assert_eq!(
            v["display"][1]["object"], "sha256:abc",
            "the id, never the payload"
        );
        assert_eq!(serde_json::from_value::<StreamPayload>(v).unwrap(), p);
    }

    /// A call with nothing worth showing carries no card list at all.
    #[test]
    fn no_cards_means_the_field_is_absent() {
        let p = StreamPayload::ToolExecEnd {
            call_id: "c1".into(),
            status: ToolStatus::Denied,
            display: Vec::new(),
            summary: String::new(),
            duration_ms: 0,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("display").is_none());
        assert!(v.get("summary").is_none());
        assert!(v.get("duration_ms").is_none());
        assert_eq!(serde_json::from_value::<StreamPayload>(v).unwrap(), p);
    }

    /// `Error` is not terminal — `TurnEnd` is. A client must not tear the turn down on `Error`.
    #[test]
    fn error_is_not_a_terminal_payload() {
        let err = StreamPayload::Error {
            error: ApiError::unavailable("llm_server_unavailable", "boom"),
        };
        assert!(!matches!(err, StreamPayload::TurnEnd { .. }));
    }
}

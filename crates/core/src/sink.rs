//! The turn's render stream: core → engine → UI.
//! # Synchronous, and allowed to drop
//! a slow client — a UI that stopped reading would stall the agent.
//! Authoritative state lives in the database, and a client that missed events re-reads it.
//! # One sink for the whole tree
//! A sub-agent shares its parent's sink and is distinguished by [`AgentRef`], which is also why
//! blocks are identified by a `block_id` rather than by the LLM layer's bare `index` — indices
//! restart per response and would collide across concurrent agents.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use zlogic_protocol::error::{ApiError, ErrorCategory, LocalizedMessage};
use zlogic_protocol::stream::{
    AgentRef, NoticeLevel, OutputSink, OutputStream, StreamEvent, StreamPayload, ToolDisplay,
};
use zlogic_protocol::{SessionId, TurnId};
use zlogic_store::NewEntry;

use crate::CoreError;

pub trait EventSink: Send + Sync {
    fn emit(&self, event: StreamEvent);
}

/// Discards everything. What a batch run wants.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: StreamEvent) {}
}

/// Keeps every event, for tests and for engine-level assertions.
#[derive(Default)]
pub struct RecordingSink(std::sync::Mutex<Vec<StreamEvent>>);

impl RecordingSink {
    pub fn events(&self) -> Vec<StreamEvent> {
        self.0.lock().unwrap().clone()
    }

    /// The payloads only, which is what most assertions are about.
    pub fn payloads(&self) -> Vec<StreamPayload> {
        self.events().into_iter().map(|e| e.payload).collect()
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: StreamEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// Stamps payloads with the turn's identity and a monotonic sequence number.
pub struct TurnEmitter {
    sink: Arc<dyn EventSink>,
    session_id: String,
    turn_id: String,
    agent: AgentRef,
    seq: AtomicU64,
    recorder: Option<NoticeRecorder>,
}

struct NoticeRecorder {
    store: zlogic_store::SharedStore,
    objects: Arc<dyn zlogic_objects::ObjectStore>,
    session_id: SessionId,
    turn_id: TurnId,
    turn_seq: i64,
}

impl TurnEmitter {
    pub fn new(
        sink: Arc<dyn EventSink>,
        session_id: SessionId,
        turn_id: TurnId,
        agent: AgentRef,
        store: zlogic_store::SharedStore,
        objects: Arc<dyn zlogic_objects::ObjectStore>,
        turn_seq: i64,
    ) -> Self {
        Self {
            sink,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            agent,
            seq: AtomicU64::new(0),
            recorder: Some(NoticeRecorder {
                store,
                objects,
                session_id,
                turn_id,
                turn_seq,
            }),
        }
    }

    pub fn detached(
        sink: Arc<dyn EventSink>,
        session_id: SessionId,
        turn_id: TurnId,
        agent: AgentRef,
    ) -> Self {
        Self {
            sink,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            agent,
            seq: AtomicU64::new(0),
            recorder: None,
        }
    }

    pub fn send(&self, payload: StreamPayload) {
        self.sink.emit(StreamEvent {
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            agent: self.agent.clone(),
            payload,
        });
    }

    pub fn notice(
        &self,
        level: NoticeLevel,
        code: &str,
        message: impl Into<String>,
        args: std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        let message = Self::notice_message(code, message, args);
        if let Some(r) = &self.recorder {
            let entry = crate::entry_data::notice_entry(
                r.session_id,
                r.turn_id,
                r.turn_seq,
                level,
                code,
                &message,
            );
            let written = entry.and_then(|e| {
                r.store
                    .with(|db| db.entries().append_with_offload(e, r.objects.as_ref()))
                    .map_err(CoreError::from)
            });
            if let Err(e) = written {
                tracing::warn!(target: "zlogic::core", code, "notice could not be written to timeline: {e}");
            }
        }
        self.send(StreamPayload::Notice {
            level,
            code: code.to_string(),
            message,
        });
    }

    fn notice_message(
        code: &str,
        message: impl Into<String>,
        args: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> LocalizedMessage {
        let mut message = LocalizedMessage::new(format!("notice.{code}"), message.into());
        for (name, value) in args {
            message = message.arg(name, value);
        }
        message
    }

    pub fn notice_transient(
        &self,
        level: NoticeLevel,
        code: &str,
        message: impl Into<String>,
        args: std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        self.send(StreamPayload::Notice {
            level,
            code: code.to_string(),
            message: Self::notice_message(code, message, args),
        });
    }

    /// Reports a failure. **Not terminal** — the caller still sends `TurnEnd`.
    pub fn error(&self, err: &CoreError) {
        let error = api_error(err);
        let persist = match err {
            CoreError::Llm(e) => e.kind != zlogic_protocol::LlmErrorKind::Aborted,
            _ => false,
        };
        if persist && let Some(r) = &self.recorder {
            let entry = crate::entry_data::notice_entry(
                r.session_id,
                r.turn_id,
                r.turn_seq,
                zlogic_protocol::stream::NoticeLevel::Warn,
                &format!("turn_error.{}", error.code),
                &error_notice_message(&error),
            );
            let written = entry.and_then(|e| {
                r.store
                    .with(|db| db.entries().append_with_offload(e, r.objects.as_ref()))
                    .map_err(CoreError::from)
            });
            if let Err(e) = written {
                tracing::warn!(target: "zlogic::core", code = %error.code, "error could not be written to timeline: {e}");
            }
        }
        self.send(StreamPayload::Error { error });
    }

    pub fn turn_end(
        &self,
        status: &zlogic_protocol::stream::TurnStatus,
        reason: Option<&str>,
        stats: &zlogic_protocol::stream::TurnStats,
    ) {
        if let Some(r) = &self.recorder {
            let entry = NewEntry::new(
                r.session_id,
                r.turn_id,
                r.turn_seq,
                zlogic_store::EntryKind::Event,
                serde_json::json!({
                    "type": "turn_end",
                    "status": status,
                    "reason": reason,
                }),
            );
            let written = r
                .store
                .with(|db| db.entries().append_turn_end(entry, r.objects.as_ref()))
                .map_err(CoreError::from);
            if let Err(e) = written {
                tracing::warn!(target: "zlogic::core", status = ?status, "turn-end could not be written to timeline: {e}");
            }
        }
        self.send(StreamPayload::TurnEnd {
            status: status.clone(),
            reason: reason.map(String::from),
            stats: stats.clone(),
        });
    }

    /// A sink that turns a tool's incremental output into stream events.
    pub fn tool_output(self: &Arc<Self>, call_id: &str) -> Arc<dyn OutputSink> {
        Arc::new(ToolOutput {
            emitter: self.clone(),
            call_id: call_id.to_string(),
        })
    }
}

struct ToolOutput {
    emitter: Arc<TurnEmitter>,
    call_id: String,
}

impl OutputSink for ToolOutput {
    fn emit(&self, stream: OutputStream, chunk: &str) {
        self.emitter.send(StreamPayload::ToolOutputDelta {
            call_id: self.call_id.clone(),
            stream,
            chunk: chunk.to_string(),
        });
    }
}

/// Where a failure came from, and whether resending the same request could work.
/// `retryable` is taken from the LLM layer's own judgement rather than guessed from the message: it
/// is the difference between a rate limit (wait and resend) and a malformed request (resending
/// forever changes nothing).
fn api_error(err: &CoreError) -> ApiError {
    match err {
        CoreError::Llm(e) => zlogic_llm::to_api_error(e, e.to_string()),
        CoreError::Tool(error) => ApiError::new(
            "tool_execution_failed",
            ErrorCategory::Internal,
            LocalizedMessage::new("error.tool_execution_failed", "Tool execution failed"),
        )
        .with_diagnostic(error.to_string()),
        CoreError::Corrupt(error) => ApiError::new(
            "context_corrupt",
            ErrorCategory::Internal,
            LocalizedMessage::new(
                "error.context_corrupt",
                "Stored conversation context is corrupt",
            ),
        )
        .with_diagnostic(error.clone()),
        CoreError::Store(error) => ApiError::internal(error),
        CoreError::Object(error) => ApiError::internal(error),
        CoreError::Json(error) => ApiError::internal(error),
        CoreError::Invalid(error) => ApiError::invalid_code("core_invalid_state", error.clone()),
        CoreError::ContextUncompressible(error) => {
            ApiError::invalid_code("context_uncompressible", error.clone())
        }
    }
}

fn error_notice_message(error: &ApiError) -> LocalizedMessage {
    let mut message = LocalizedMessage::new(
        format!("turn_error.{}", error.code),
        error_detail_text(error),
    );
    if let Some(detail) = technical_detail(error) {
        message = message.arg("detail", detail);
    }
    message
}

fn error_detail_text(error: &ApiError) -> String {
    let mut text = error.message.fallback.clone();
    if let Some(detail) = technical_detail(error) {
        text.push('\n');
        text.push_str(&detail);
    }
    text
}

fn technical_detail(error: &ApiError) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(diagnostic) = error
        .details
        .get("diagnostic")
        .and_then(serde_json::Value::as_str)
    {
        parts.push(diagnostic.to_string());
    }
    if let Some(request_id) = error
        .details
        .get("request_id")
        .and_then(serde_json::Value::as_str)
    {
        parts.push(format!("request_id: {request_id}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// The UI projection of a tool's display card.
/// The only change is the object id: typed in the tools crate, an id string on the wire. Cards with
/// nothing to show are still forwarded — a `Text` card is how a tool says "render this as prose".
pub(crate) fn display_to_wire(d: &zlogic_tools::ToolDisplay) -> ToolDisplay {
    use zlogic_tools::ToolDisplay as T;
    match d {
        T::Text { text, math } => ToolDisplay::Text {
            text: text.clone(),
            math: math
                .iter()
                .map(|line| zlogic_protocol::stream::MathLine {
                    label: line.label.clone(),
                    latex: line.latex.clone(),
                })
                .collect(),
        },
        T::Table {
            columns,
            rows,
            total_rows,
            truncated,
            object_id,
        } => ToolDisplay::Table {
            columns: columns.clone(),
            rows: rows.clone(),
            total_rows: *total_rows,
            truncated: *truncated,
            object: object_id.as_ref().map(ToString::to_string),
        },
        T::Diff {
            path,
            stat,
            change,
            object_id,
        } => ToolDisplay::Diff {
            path: path.clone(),
            stat: *stat,
            change: *change,
            object: object_id.as_ref().map(ToString::to_string),
        },
        T::File {
            path,
            mime,
            bytes,
            total_lines,
            object_id,
        } => ToolDisplay::File {
            path: path.clone(),
            mime: mime.clone(),
            bytes: *bytes,
            total_lines: *total_lines,
            object: object_id.as_ref().map(ToString::to_string),
        },
        T::Output {
            object_id,
            total_chars,
            truncated,
        } => ToolDisplay::Output {
            object: object_id.to_string(),
            total_chars: *total_chars,
            truncated: *truncated,
        },
        T::Agent { agent, session_id } => ToolDisplay::Agent {
            agent: agent.clone(),
            session_id: session_id.clone(),
        },
        T::Task { task_id, command } => ToolDisplay::Task {
            task_id: task_id.clone(),
            command: command.clone(),
        },
        T::Widget {
            object_id,
            title,
            height,
            libraries,
        } => ToolDisplay::Widget {
            object: object_id.to_string(),
            title: title.clone(),
            height: *height,
            libraries: libraries.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use zlogic_objects::ObjectStore;
    use zlogic_protocol::llm::{LlmError, LlmErrorKind};
    use zlogic_protocol::stream::{DiffStat, FileChange, ModelRef};

    fn emitter(sink: Arc<RecordingSink>) -> Arc<TurnEmitter> {
        Arc::new(TurnEmitter::detached(
            sink,
            SessionId::new(),
            TurnId::new(),
            AgentRef::root(),
        ))
    }

    fn model() -> ModelRef {
        ModelRef {
            provider_id: "anthropic".into(),
            model_id: "claude-opus-5".into(),
            display_name: "Opus".into(),
        }
    }

    #[test]
    fn seq_starts_at_one_and_never_repeats() {
        let sink = Arc::new(RecordingSink::default());
        let em = emitter(sink.clone());
        for _ in 0..3 {
            em.send(StreamPayload::TurnStart {
                model: model(),
                resumed: false,
                proactive: false,
            });
        }
        let seqs: Vec<u64> = sink.events().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2, 3]);
    }

    /// Every event names its turn and its agent — sub-agents share this sink with the root.
    #[test]
    fn events_carry_their_identity() {
        let sink = Arc::new(RecordingSink::default());
        let session = SessionId::new();
        let turn = TurnId::new();
        let agent = AgentRef {
            agent_id: Some("child".into()),
            parent_agent_id: None,
            name: "researcher".into(),
        };
        let em = TurnEmitter::detached(sink.clone(), session, turn, agent.clone());
        em.notice(
            NoticeLevel::Info,
            "hello",
            "world",
            std::collections::BTreeMap::new(),
        );

        let ev = &sink.events()[0];
        assert_eq!(ev.session_id, session.to_string());
        assert_eq!(ev.turn_id, turn.to_string());
        assert_eq!(ev.agent, agent);
        assert!(!ev.agent.is_root());
    }

    #[test]
    fn tool_output_deltas_are_tagged_with_their_call_and_stream() {
        let sink = Arc::new(RecordingSink::default());
        let em = emitter(sink.clone());
        let out = em.tool_output("call_1");
        out.emit(OutputStream::Stdout, "compiling\n");
        out.emit(OutputStream::Stderr, "warning\n");

        match &sink.payloads()[0] {
            StreamPayload::ToolOutputDelta {
                call_id,
                stream,
                chunk,
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(*stream, OutputStream::Stdout);
                assert_eq!(chunk, "compiling\n");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            sink.payloads()[1],
            StreamPayload::ToolOutputDelta {
                stream: OutputStream::Stderr,
                ..
            }
        ));
    }

    /// Retryability comes from the LLM layer, not from inspecting the message.
    #[test]
    fn an_llm_error_keeps_its_own_retry_judgement() {
        let sink = Arc::new(RecordingSink::default());
        let em = emitter(sink.clone());
        em.error(&CoreError::Llm(LlmError {
            kind: LlmErrorKind::RateLimit,
            retryable: true,
            message: "slow down".into(),
            status: Some(429),
            request_id: None,
        }));
        em.error(&CoreError::Corrupt("no source stamp".into()));

        match &sink.payloads()[0] {
            StreamPayload::Error { error } => {
                assert_eq!(error.code, "llm_rate_limited");
                assert!(error.is_retryable());
                assert_eq!(
                    error.details.get("diagnostic").and_then(Value::as_str),
                    Some("RateLimit: slow down")
                );
            }
            other => panic!("{other:?}"),
        }
        match &sink.payloads()[1] {
            StreamPayload::Error { error } => {
                assert_eq!(error.code, "context_corrupt");
                assert!(
                    !error.is_retryable(),
                    "a broken history does not fix itself on a resend"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn errors_are_persisted_into_the_timeline_with_details() {
        let db = zlogic_store::Db::open_in_memory().unwrap();
        let session_id = db
            .sessions()
            .create(zlogic_store::NewSession::root(
                zlogic_protocol::WorkspaceId::new(),
            ))
            .unwrap()
            .session_id;
        let store = zlogic_store::SharedStore::new(db);
        let objects = Arc::new(zlogic_objects::MemoryObjectStore::new());
        let sink = Arc::new(RecordingSink::default());
        let em = TurnEmitter::new(
            sink.clone(),
            session_id,
            TurnId::new(),
            AgentRef::root(),
            store.clone(),
            objects,
            1,
        );

        em.error(&CoreError::Llm(LlmError {
            kind: LlmErrorKind::RateLimit,
            retryable: true,
            message: "HTTP 429: slow down".into(),
            status: Some(429),
            request_id: Some("req_abc".into()),
        }));

        assert!(matches!(sink.payloads()[0], StreamPayload::Error { .. }));

        let rows = store.with(|db| db.entries().list(session_id)).unwrap();
        assert_eq!(rows.len(), 1, "exactly one error notice must be persisted");
        let data = &rows[0].data;
        assert_eq!(data["code"], "turn_error.llm_rate_limited");
        assert_eq!(data["level"], "warn");
        assert_eq!(data["message"]["key"], "turn_error.llm_rate_limited");
        let fallback = data["message"]["fallback"].as_str().unwrap();
        assert!(
            fallback.contains("The model provider is rate limiting requests"),
            "{fallback}"
        );
        assert!(
            fallback.contains("RateLimit: HTTP 429: slow down"),
            "{fallback}"
        );
        assert!(fallback.contains("request_id: req_abc"), "{fallback}");
        let detail = data["message"]["args"]["detail"].as_str().unwrap();
        assert_eq!(
            detail,
            "RateLimit: HTTP 429: slow down\nrequest_id: req_abc"
        );
    }

    #[test]
    fn turn_end_is_persisted_with_status_and_reason() {
        use zlogic_protocol::stream::TurnStats;
        use zlogic_store::EntryKind;

        let db = zlogic_store::Db::open_in_memory().unwrap();
        let session_id = db
            .sessions()
            .create(zlogic_store::NewSession::root(
                zlogic_protocol::WorkspaceId::new(),
            ))
            .unwrap()
            .session_id;
        let store = zlogic_store::SharedStore::new(db);
        let objects = Arc::new(zlogic_objects::MemoryObjectStore::new());
        let sink = Arc::new(RecordingSink::default());
        let em = TurnEmitter::new(
            sink.clone(),
            session_id,
            TurnId::new(),
            AgentRef::root(),
            store.clone(),
            objects,
            1,
        );

        let status = zlogic_protocol::stream::TurnStatus::Completed;
        let stats = TurnStats::default();
        em.turn_end(&status, Some("hit the limit"), &stats);

        assert!(matches!(
            sink.payloads()[0],
            StreamPayload::TurnEnd { status: ref s, .. } if *s == status
        ));

        let rows = store.with(|db| db.entries().list(session_id)).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "exactly one terminal event must be persisted"
        );
        let rec = &rows[0];
        assert_eq!(rec.kind, EntryKind::Event);
        assert_eq!(rec.data["type"], "turn_end");
        assert_eq!(rec.data["status"], "completed");
        assert_eq!(rec.data["reason"], "hit the limit");
    }

    #[test]
    fn user_aborts_are_not_persisted_as_errors() {
        let db = zlogic_store::Db::open_in_memory().unwrap();
        let session_id = db
            .sessions()
            .create(zlogic_store::NewSession::root(
                zlogic_protocol::WorkspaceId::new(),
            ))
            .unwrap()
            .session_id;
        let store = zlogic_store::SharedStore::new(db);
        let objects = Arc::new(zlogic_objects::MemoryObjectStore::new());
        let sink = Arc::new(RecordingSink::default());
        let em = TurnEmitter::new(
            sink.clone(),
            session_id,
            TurnId::new(),
            AgentRef::root(),
            store.clone(),
            objects,
            1,
        );

        em.error(&CoreError::Llm(LlmError {
            kind: LlmErrorKind::Aborted,
            retryable: false,
            message: "request aborted".into(),
            status: None,
            request_id: None,
        }));

        assert!(matches!(sink.payloads()[0], StreamPayload::Error { .. }));
        let rows = store.with(|db| db.entries().list(session_id)).unwrap();
        assert!(
            rows.is_empty(),
            "a user abort must not leave an error record behind"
        );
    }

    #[test]
    fn tool_errors_are_not_persisted_as_errors() {
        let db = zlogic_store::Db::open_in_memory().unwrap();
        let session_id = db
            .sessions()
            .create(zlogic_store::NewSession::root(
                zlogic_protocol::WorkspaceId::new(),
            ))
            .unwrap()
            .session_id;
        let store = zlogic_store::SharedStore::new(db);
        let objects = Arc::new(zlogic_objects::MemoryObjectStore::new());
        let sink = Arc::new(RecordingSink::default());
        let em = TurnEmitter::new(
            sink.clone(),
            session_id,
            TurnId::new(),
            AgentRef::root(),
            store.clone(),
            objects,
            1,
        );

        em.error(&CoreError::Tool(zlogic_tools::ToolError::Failed(
            "shell exited with 127".into(),
        )));

        assert!(matches!(sink.payloads()[0], StreamPayload::Error { .. }));
        let rows = store.with(|db| db.entries().list(session_id)).unwrap();
        assert!(
            rows.is_empty(),
            "a failed tool is recorded by its tool_result"
        );
    }

    #[test]
    fn error_detail_text_joins_friendly_and_technical_parts() {
        let error = ApiError::new(
            "llm_rate_limited",
            ErrorCategory::Unavailable,
            LocalizedMessage::new("error.llm_rate_limited", "slow down please"),
        )
        .with_detail("diagnostic", "RateLimit: HTTP 429: slow down")
        .with_detail("request_id", "req_9");
        let text = error_detail_text(&error);
        assert!(text.starts_with("slow down please\n"), "{text}");
        assert!(text.contains("RateLimit: HTTP 429: slow down"), "{text}");
        assert!(text.ends_with("request_id: req_9"), "{text}");
    }

    /// The projection changes exactly one thing: the object id becomes its wire string.
    #[test]
    fn display_cards_project_with_the_object_id_as_a_string() {
        let id = zlogic_objects::MemoryObjectStore::new()
            .put(b"log")
            .unwrap();
        let wire = display_to_wire(&zlogic_tools::ToolDisplay::Output {
            object_id: id.clone(),
            total_chars: 3,
            truncated: false,
        });
        assert_eq!(
            wire,
            ToolDisplay::Output {
                object: id.to_string(),
                total_chars: 3,
                truncated: false
            }
        );

        let diff = display_to_wire(&zlogic_tools::ToolDisplay::Diff {
            path: "a.rs".into(),
            stat: DiffStat {
                added: 1,
                removed: 0,
            },
            change: Some(FileChange::Created),
            object_id: None,
        });
        match diff {
            ToolDisplay::Diff {
                path,
                stat,
                change,
                object,
                ..
            } => {
                assert_eq!(path, "a.rs");
                assert_eq!(
                    stat,
                    DiffStat {
                        added: 1,
                        removed: 0
                    }
                );
                assert_eq!(change, Some(FileChange::Created));
                assert_eq!(object, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_null_sink_accepts_everything_and_keeps_nothing() {
        let em = TurnEmitter::detached(
            Arc::new(NullSink),
            SessionId::new(),
            TurnId::new(),
            AgentRef::root(),
        );
        em.send(StreamPayload::TurnStart {
            model: model(),
            resumed: false,
            proactive: false,
        });
    }
}

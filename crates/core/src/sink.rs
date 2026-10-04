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

/// Enough bytes to tell a PNG from a mislabelled text file. Short enough that identifying a
/// gigabyte video nobody will open costs nothing; a file that cannot be read is described by its
/// name alone, which is the best that can be said about it.
fn head(path: &std::path::Path) -> Vec<u8> {
    const HEAD: usize = 4096;
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut buffer = vec![0u8; HEAD];
    match std::io::Read::read(&mut file, &mut buffer) {
        Ok(read) => {
            buffer.truncate(read);
            buffer
        }
        Err(_) => Vec::new(),
    }
}

/// The files in one folder, described the way the client will describe them.
fn listing(dir: &std::path::Path, since: Option<std::time::SystemTime>) -> Vec<zlogic_protocol::stream::TurnDeliverable> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            since.is_none_or(|since| {
                entry.metadata().is_ok_and(|m| m.modified().is_ok_and(|t| t >= since))
            })
        })
        .filter_map(|entry| {
            let path = entry.path();
            let meta = entry.metadata().ok()?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some(zlogic_protocol::stream::TurnDeliverable {
                mime: zlogic_tools::file::sniff::detect_mime(&name, &head(&path))
                    .map(str::to_string),
                path: path.to_string_lossy().into_owned(),
                bytes: meta.len(),
            })
        })
        .collect()
}

/// Whether a path the turn named is still there. A relative one is read against the workspace, the
/// way the shell that wrote it read it.
fn exists(root: &std::path::Path, path: &str) -> bool {
    let candidate = std::path::Path::new(path);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    candidate.symlink_metadata().is_ok()
}

/// The command in a shell tool call's raw arguments, whether it came as JSON or as a script under
/// another key. `None` when there is no `command`, and then the caller keeps the raw text — a
/// redirect can be read out of it either way.
fn shell_line(args: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
    ["command", "cmd"]
        .into_iter()
        .find_map(|key| parsed.get(key).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
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
    /// Where to look for what this turn produced. Absent in a turn that is not a user-facing one
    /// (compaction), which is also the turn with nothing to hand over.
    sources: Option<ArtifactSources>,
    /// The shell lines this turn ran, kept whole so that where a redirect wrote can be read off them
    /// at turn end rather than guessed from a name.
    shell_commands: std::sync::Mutex<Vec<String>>,
    /// Set when a `shell` call is dispatched, and the reason the workspace is walked at all.
    ///
    /// Every other tool that writes says what it wrote: `write_file` and `edit` carry a diff, an
    /// image or a document generator carries a file display. Those are entries, and entries are
    /// parsed — no inference, and a better answer than a walk would give. A shell command is the
    /// one tool that can leave anything anywhere (`python render.py --name out/icon` writes
    /// `out/icon-1024.png`, a path no entry mentions), so it is the one case where guessing is
    /// both necessary and worth its cost.
    ran_shell: std::sync::atomic::AtomicBool,
    /// Everything this turn has said and named, which is what decides a scanned file's owner when
    /// another session is running in the same workspace at the same time.
    own: crate::produced::Footprint,
}

/// The places a turn's output can be, and the window that tells them apart from yesterday's.
///
/// Read at turn end rather than watched: a file written seconds before the model answered has to
/// count like any other, and a watcher would have had to guess which turn it belonged to.
#[derive(Clone)]
pub struct ArtifactSources {
    /// The workspace to scan for anything written during the turn.
    pub root: std::path::PathBuf,
    /// This turn's delivery folder, read by path because it is under a dotfolder the scan skips.
    pub deliverables: std::path::PathBuf,
    /// The session's cache folder — the parent of every turn folder, and a dotfolder the scan cannot
    /// see. Searched for a *named* file and never listed: a model that redirects into it and keeps
    /// the name (`cd "<cache>/competitors" && … > logo.png`) wrote something real, and the numbered
    /// takes it iterates on sit in the same folder and are nobody's output.
    pub session_cache: std::path::PathBuf,
    /// When the turn began; files older than this are somebody else's.
    pub since: std::time::SystemTime,
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
            sources: None,
            shell_commands: std::sync::Mutex::new(Vec::new()),
            ran_shell: std::sync::atomic::AtomicBool::new(false),
            own: crate::produced::Footprint::default(),
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
            sources: None,
            shell_commands: std::sync::Mutex::new(Vec::new()),
            ran_shell: std::sync::atomic::AtomicBool::new(false),
            own: crate::produced::Footprint::default(),
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
        // Listed here rather than by the caller, so all six terminal paths report it — a turn that
        // failed after writing its chart still handed the user a chart.
        let deliverables = self.deliverables();
        let gone = self.gone(&deliverables);
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
                    "deliverables": deliverables,
                    "gone": gone,
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
            deliverables,
            gone,
        });
    }

    /// Points the emitter at the two places this turn's output can be. Without them a turn reports
    /// nothing, which is what a host that only runs turns internally wants.
    pub fn with_artifact_sources(mut self, sources: ArtifactSources) -> Self {
        self.sources = Some(sources);
        self
    }

    /// Records that this turn ran a shell command, which is what earns it a workspace walk. Set
    /// before the call rather than after a successful one: a command that failed halfway may still
    /// have written its output.
    ///
    /// The command itself is kept, because it is the strongest evidence of what the turn produced:
    /// `render.py --name out/icon` is how `out/icon-1024.png` came to exist, and no entry anywhere
    /// says so.
    pub fn note_shell_command(&self, args: &str) {
        self.ran_shell.store(true, Ordering::Relaxed);
        self.own.note(args);
        // The command itself, kept whole: where a redirect wrote is decided by reading it, not by
        // searching the text for a name (see `produced::redirect_outputs`).
        let command = shell_line(args).unwrap_or_else(|| args.to_owned());
        let mut commands = self.shell_commands.lock().unwrap_or_else(|error| error.into_inner());
        if commands.len() < 64 {
            commands.push(command);
        }
    }

    /// Every shell line this turn ran, in order.
    pub fn shell_commands(&self) -> Vec<String> {
        self.shell_commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// A path one of this turn's tool cards named, or a line of its own prose. Same purpose as the
    /// command: something the turn wrote down is something it can account for.
    pub fn note(&self, text: &str) {
        self.own.note(text);
    }

    /// A file one of this turn's cards named outright, kept so that the turn end can ask whether it
    /// is still there.
    pub fn note_path(&self, path: &std::path::Path) {
        self.own.note_path(path);
    }

    /// What the sessions running alongside this one in the same workspace have written down.
    ///
    /// Read from the database rather than from shared memory because that is what all the hosts
    /// (desktop, daemon) already share, and because a live turn's evidence is its own entries —
    /// there is no in-process object to ask. Only sessions holding a lock are consulted, which is
    /// the concurrent set and nothing larger: a session that ended yesterday cannot be competing
    /// for a file written in the last minute.
    fn concurrent_footprints(&self) -> crate::produced::Footprint {
        let others = crate::produced::Footprint::default();
        let (Some(_), Some(recorder)) = (&self.sources, &self.recorder) else {
            return others;
        };
        let store = &recorder.store;
        let me = recorder.session_id;
        let live = store.with(|db| {
            let session = db.sessions().get(me)?;
            Ok::<_, zlogic_store::StoreError>(
                db.locks()
                    .live_turns_in(session.workspace_id)?
                    .into_iter()
                    .filter(|(session_id, _)| *session_id != me)
                    .collect::<Vec<_>>(),
            )
        });
        let Ok(live) = live else {
            return others;
        };
        for (session_id, turn_id) in live {
            let Ok(entries) = store.with(|db| db.entries().list_turn_by_id(session_id, turn_id))
            else {
                continue;
            };
            for entry in entries {
                // The whole row as haystack: a command line lives inside a `tool_call`'s JSON and
                // a card's paths inside its display, and picking arguments out of either would be
                // parsing to no end for no gain — what is being looked for is a file *name*, which
                // the serialized text contains either way.
                if !entry.data.is_null() {
                    others.note(&entry.data.to_string());
                }
                if let Some(display) = &entry.display {
                    others.note(&display.to_string());
                }
                for object in &entry.objects {
                    if let Some(key) = &object.ref_key {
                        others.note(key);
                    }
                }
            }
        }
        others
    }

    /// What the turn left behind, from two places that are not the same kind of answer.
    ///
    /// **The delivery folder** is ours and its path is known, so reading it is not inference — one
    /// `read_dir`, no guessing, and whatever is in it was put there on purpose. Always read,
    /// whatever the turn ran, because a model that puts a file there is saying so in the one way we
    /// can check without interpreting anything. One level, files only: not recursive on purpose,
    /// since a script that wrote a tree wrote intermediates and the user is handed the file.
    ///
    /// **The session** cache folder is never listed, though the delivery folder sits inside it: that
    /// folder is where the environment tells the model to put its temporary files, and a model
    /// iterating on a design fills it with numbered takes (`v0.png`, `v1.png`, `v2_ws.png`) — the row
    /// is what the user is handed, so it gets the delivery folder or nothing. It is *searched*,
    /// though, for a file whose name a redirect named outright, which is a different question and has
    /// a different answer.
    ///
    /// **A redirect** is the one thing a tool card never carries. Where it wrote is read off the
    /// command — a relative target is relative to wherever the line had moved itself — and a target
    /// that does not exist where it says becomes a *name* to look up, across these same folders and
    /// the workspace, freshest first. Nothing found means nothing reported.
    ///
    /// **The workspace** is inference, and only a shell command earns it. Ownership is decided
    /// against the turns running alongside this one, because a shared workspace means two sessions
    /// share one clock.
    fn deliverables(&self) -> Vec<zlogic_protocol::stream::TurnDeliverable> {
        let Some(sources) = &self.sources else {
            return Vec::new();
        };
        let mut files = listing(&sources.deliverables, None);
        let scanned = if self.ran_shell.load(Ordering::Relaxed) {
            let others = self.concurrent_footprints();
            crate::produced::scan_workspace(&sources.root, sources.since, &self.own, &others)
        } else {
            Vec::new()
        };
        files.extend(scanned.iter().cloned());
        // A shell command that redirected is the one thing a tool card never carries, and where it
        // landed is only knowable by looking: the file is found by name and reported on its real
        // path, or not at all.
        if self.ran_shell.load(Ordering::Relaxed) {
            files.extend(crate::produced::redirect_outputs(
                &self.shell_commands(),
                &crate::produced::RedirectSources {
                    root: &sources.root,
                    deliverables: &sources.deliverables,
                    session_cache: &sources.session_cache,
                    since: sources.since,
                },
                &scanned,
            ));
        }
        // The same file can be in both — a model that wrote into the delivery folder gets it
        // counted once, and the projection unions by path anyway.
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files.dedup_by(|a, b| a.path == b.path);
        files.truncate(crate::produced::MAX_PRODUCED);
        files
    }

    /// The files this turn named that are not on disk any more, and so cannot be handed over.
    ///
    /// A turn that writes a scratch file and cleans up after itself — `rm probe.cmd`, a cache sweep,
    /// `git checkout --` — leaves an entry that says it wrote something the user can no longer open.
    /// Guessing at the cleanup from the command text misses every form but a literal `rm`, so the
    /// files are asked instead: one `stat` each, over the paths the turn's own cards named. A file
    /// the listing already found is not asked about, since it was there a moment ago by definition.
    fn gone(&self, listed: &[zlogic_protocol::stream::TurnDeliverable]) -> Vec<String> {
        let Some(root) = self.sources.as_ref().map(|sources| &sources.root) else {
            return Vec::new();
        };
        let listed: Vec<String> = listed.iter().map(|file| file.path.to_lowercase()).collect();
        self.own
            .paths()
            .into_iter()
            .filter(|path| {
                let path = path.to_lowercase();
                !listed.contains(&path) && !exists(root, &path)
            })
            .collect()
    }

    /// A sink that turns a tool's incremental output into stream events.
    pub fn tool_output(self: &Arc<Self>, call_id: &str) -> Arc<dyn OutputSink> {
        Arc::new(ToolOutput {
            emitter: self.clone(),
            call_id: call_id.to_string(),
        })
    }

    /// Where a tool attaches UI metadata to its own running call.
    pub fn tool_display(self: &Arc<Self>, call_id: &str) -> Arc<dyn zlogic_tools::DisplaySink> {
        Arc::new(ToolDisplaySink {
            emitter: self.clone(),
            call_id: call_id.to_string(),
        })
    }
}

struct ToolDisplaySink {
    emitter: Arc<TurnEmitter>,
    call_id: String,
}

impl zlogic_tools::DisplaySink for ToolDisplaySink {
    fn attach(&self, display: zlogic_tools::ToolDisplay) {
        self.emitter.send(StreamPayload::ToolDisplayAttached {
            call_id: self.call_id.clone(),
            display: display_to_wire(&display),
        });
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
            height,
            libraries,
        } => ToolDisplay::Widget {
            object: object_id.to_string(),
            height: *height,
            libraries: libraries.clone(),
        },
        T::Image {
            object_id,
            mime,
            width,
            height,
            bytes,
            label,
        } => ToolDisplay::Image {
            object: object_id.to_string(),
            mime: mime.clone(),
            width: *width,
            height: *height,
            bytes: *bytes,
            label: label.clone(),
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

    /// The turn's output list is built here rather than by the caller, so all six terminal paths
    /// carry it — and it has to reach the timeline, because the row the user reopens a session to
    /// see is built from what was persisted, not from the stream that has long since gone.
    ///
    /// The delivery folder is read without a shell command having run: its path is ours, so
    /// whatever is in it was put there on purpose and costs one `read_dir` to find.
    #[test]
    fn the_delivery_folder_is_reported_without_any_shell_command() {
        use zlogic_protocol::stream::TurnStats;

        let workspace = tempfile::tempdir().unwrap();
        let session_id = zlogic_protocol::SessionId::new();
        let turn_id = TurnId::new();
        let deliverables = crate::turndir::deliverables_dir(workspace.path(), session_id, turn_id);
        std::fs::create_dir_all(&deliverables).unwrap();
        std::fs::write(
            deliverables.join("icon-1024.png"),
            b"\x89PNG\r\n\x1a\n and the rest of a real png",
        )
        .unwrap();

        let db = zlogic_store::Db::open_in_memory().unwrap();
        let session_id = db
            .sessions()
            .create(zlogic_store::NewSession::root(zlogic_protocol::WorkspaceId::new()))
            .unwrap()
            .session_id;
        let store = zlogic_store::SharedStore::new(db);
        let sink = Arc::new(RecordingSink::default());
        let em = TurnEmitter::new(
            sink.clone(),
            session_id,
            turn_id,
            AgentRef::root(),
            store.clone(),
            Arc::new(zlogic_objects::MemoryObjectStore::new()),
            1,
        )
        .with_artifact_sources(crate::sink::ArtifactSources {
            root: workspace.path().to_path_buf(),
            deliverables: deliverables.clone(),
            // Where a redirect whose path was a variable landed.
            session_cache: workspace.path().join("no-session-folder"),
            since: std::time::SystemTime::now(),
        });

        em.turn_end(
            &zlogic_protocol::stream::TurnStatus::Completed,
            None,
            &TurnStats::default(),
        );

        let StreamPayload::TurnEnd { deliverables, .. } = &sink.payloads()[0] else {
            panic!("expected a turn-end payload");
        };
        assert_eq!(deliverables.len(), 1, "{deliverables:?}");
        assert!(
            deliverables[0].path.ends_with("icon-1024.png"),
            "{}",
            deliverables[0].path
        );
        assert_eq!(deliverables[0].mime.as_deref(), Some("image/png"));
        assert_eq!(deliverables[0].bytes, 35);

        let rows = store.with(|db| db.entries().list(session_id)).unwrap();
        let persisted = rows
            .iter()
            .find(|row| row.data["type"] == "turn_end")
            .expect("the terminal event");
        assert_eq!(persisted.data["deliverables"][0]["path"], deliverables[0].path);
        assert_eq!(persisted.data["deliverables"][0]["mime"], "image/png");
    }

    /// The gate, both ways. A file elsewhere in the workspace is only reported when a shell command
    /// ran, because that is the only tool whose output the entries cannot account for: a
    /// `write_file` diff or a generator's file display already says what it wrote, and reporting
    /// those a second time from a walk would only add a vaguer answer to a parsed one.
    #[test]
    fn a_file_written_anywhere_else_is_reported_only_after_a_shell_command() {
        use zlogic_protocol::stream::TurnStats;

        // The window opens with the turn, so the file has to be written after it: this is the whole
        // of what tells this turn's output from yesterday's.
        let since = std::time::SystemTime::now();
        let workspace = tempfile::tempdir().unwrap();
        // A root that is not itself a dotfolder: the walker's hidden filter takes the root with it,
        // which is its own blind spot and has a test of its own in `produced`.
        let root = workspace.path().join("project");
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/icon-1024.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let sink = Arc::new(RecordingSink::default());
        let emitter = || {
            TurnEmitter::new(
                sink.clone(),
                zlogic_protocol::SessionId::new(),
                TurnId::new(),
                AgentRef::root(),
                zlogic_store::SharedStore::new(zlogic_store::Db::open_in_memory().unwrap()),
                Arc::new(zlogic_objects::MemoryObjectStore::new()),
                1,
            )
            .with_artifact_sources(crate::sink::ArtifactSources {
                root: root.clone(),
                deliverables: workspace.path().join("never-created"),
                // Where a redirect whose path was a variable landed.
                session_cache: workspace.path().join("no-session-folder"),
                since,
            })
        };

        // Without a shell command: nothing. The file is right there, and stays unreported.
        let quiet = emitter();
        quiet.turn_end(
            &zlogic_protocol::stream::TurnStatus::Completed,
            None,
            &TurnStats::default(),
        );
        let StreamPayload::TurnEnd { deliverables, .. } = &sink.payloads()[0] else {
            panic!("expected a turn-end payload");
        };
        assert!(deliverables.is_empty(), "{deliverables:?}");

        // With one: the script's output, which no entry mentions.
        let ran = emitter();
        ran.note_shell_command(r#"{"command":"python tile.py --name out/icon"}"#);
        ran.turn_end(
            &zlogic_protocol::stream::TurnStatus::Completed,
            None,
            &TurnStats::default(),
        );
        let StreamPayload::TurnEnd { deliverables, .. } = &sink.payloads()[1] else {
            panic!("expected a second turn-end payload");
        };
        assert_eq!(deliverables.len(), 1, "{deliverables:?}");
        assert!(deliverables[0].path.ends_with("icon-1024.png"));
    }

    /// The session cache folder is where the environment tells the model to keep its temporary files, so
    /// the numbered takes a model iterating on a design leaves there are not what the user is
    /// handed. The delivery folder one level down is.
    #[test]
    fn a_temporary_file_in_the_session_folder_is_not_reported() {
        use zlogic_protocol::stream::TurnStats;

        let since = std::time::SystemTime::now();
        let workspace = tempfile::tempdir().unwrap();
        let session_id = zlogic_protocol::SessionId::new();
        let session_cache = crate::turndir::session_cache_dir(workspace.path(), Some(session_id));
        std::fs::create_dir_all(&session_cache).unwrap();
        for name in ["v0.png", "v1.png", "v4_ws_bottom.png"] {
            std::fs::write(session_cache.join(name), b"\x89PNG\r\n\x1a\n").unwrap();
        }

        let sink = Arc::new(RecordingSink::default());
        let em = TurnEmitter::new(
            sink.clone(),
            session_id,
            TurnId::new(),
            AgentRef::root(),
            zlogic_store::SharedStore::new(zlogic_store::Db::open_in_memory().unwrap()),
            Arc::new(zlogic_objects::MemoryObjectStore::new()),
            1,
        )
        .with_artifact_sources(crate::sink::ArtifactSources {
            root: workspace.path().to_path_buf(),
            deliverables: session_cache.join("no-turn/deliverables"),
            // Where a redirect whose path was a variable landed.
            session_cache: workspace.path().join("no-session-folder"),
            since,
        });

        em.turn_end(
            &zlogic_protocol::stream::TurnStatus::Completed,
            None,
            &TurnStats::default(),
        );

        let StreamPayload::TurnEnd { deliverables, .. } = &sink.payloads()[0] else {
            panic!("expected a turn-end payload");
        };
        assert!(deliverables.is_empty(), "{deliverables:?}");
    }

    /// A file the turn wrote and then cleaned up is reported as gone, so the client stops offering
    /// a row for something it cannot open.
    #[test]
    fn a_file_the_turn_cleaned_up_is_reported_as_gone() {
        use zlogic_protocol::stream::TurnStats;

        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().to_path_buf();
        let kept = root.join("kept.md");
        std::fs::write(&kept, b"# kept").unwrap();
        let removed = root.join("probe.cmd");
        std::fs::write(&removed, b"@echo off").unwrap();

        let sink = Arc::new(RecordingSink::default());
        let em = TurnEmitter::new(
            sink.clone(),
            zlogic_protocol::SessionId::new(),
            TurnId::new(),
            AgentRef::root(),
            zlogic_store::SharedStore::new(zlogic_store::Db::open_in_memory().unwrap()),
            Arc::new(zlogic_objects::MemoryObjectStore::new()),
            1,
        )
        .with_artifact_sources(crate::sink::ArtifactSources {
            root: root.clone(),
            deliverables: root.join("no-deliverables"),
            // Where a redirect whose path was a variable landed.
            session_cache: workspace.path().join("no-session-folder"),
            since: std::time::SystemTime::now(),
        });
        em.note_path(&kept);
        em.note_path(&removed);
        std::fs::remove_file(&removed).unwrap();

        em.turn_end(
            &zlogic_protocol::stream::TurnStatus::Completed,
            None,
            &TurnStats::default(),
        );

        let StreamPayload::TurnEnd { gone, .. } = &sink.payloads()[0] else {
            panic!("expected a turn-end payload");
        };
        assert_eq!(gone.len(), 1, "{gone:?}");
        assert!(gone[0].ends_with("probe.cmd"), "{gone:?}");
    }

    /// A turn with nothing to hand over reports nothing, and the wire stays as it was: the field is
    /// absent rather than an empty array, so an old client reading a new transcript sees the same
    /// shape it always did.
    #[test]
    fn a_turn_that_produced_nothing_reports_an_empty_list() {
        use zlogic_protocol::stream::TurnStats;

        let workspace = tempfile::tempdir().unwrap();
        let sink = Arc::new(RecordingSink::default());
        let em = TurnEmitter::new(
            sink.clone(),
            zlogic_protocol::SessionId::new(),
            TurnId::new(),
            AgentRef::root(),
            zlogic_store::SharedStore::new(zlogic_store::Db::open_in_memory().unwrap()),
            Arc::new(zlogic_objects::MemoryObjectStore::new()),
            1,
        )
        .with_artifact_sources(crate::sink::ArtifactSources {
            root: workspace.path().to_path_buf(),
            deliverables: workspace.path().join("deliverables"),
            // Where a redirect whose path was a variable landed.
            session_cache: workspace.path().join("no-session-folder"),
            since: std::time::SystemTime::now(),
        });
        em.note_shell_command(r#"{"command":"true"}"#);

        em.turn_end(
            &zlogic_protocol::stream::TurnStatus::Completed,
            None,
            &TurnStats::default(),
        );

        let StreamPayload::TurnEnd { deliverables, .. } = &sink.payloads()[0] else {
            panic!("expected a turn-end payload");
        };
        assert!(deliverables.is_empty());
        let value = serde_json::to_value(StreamPayload::TurnEnd {
            status: zlogic_protocol::stream::TurnStatus::Completed,
            reason: None,
            stats: TurnStats::default(),
            deliverables: Vec::new(),
            gone: Vec::new(),
        })
        .unwrap();
        assert!(
            value.get("deliverables").is_none(),
            "an empty list must not reach the wire: {value}"
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

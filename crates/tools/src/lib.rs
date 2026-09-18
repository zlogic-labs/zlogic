//! # zlogic-tools
//! Tool definitions, the registry, and the built-in tools.
//! Tools are grouped by domain — [`compute`], [`file`], [`search`], [`exec`], [`net`], [`ui`],
//! [`agent`], [`session`] — rather than by "is it built in". The distinction that matters when
//! reading the code is what a tool *does*; where it came from is one field on [`ToolMeta`].
//! The grouping is not filing for its own sake. Each domain has one question it has to keep
//! answering consistently, and that question is what the module's docs are about: for [`file`] it
//! is what happens to content that is too large or not text; for [`search`], what the caller does
//! *not* want to see; for [`exec`], how a child process is bounded and stopped; for [`net`], what
//! an untrusted remote is allowed to cost. A tool answering one of those differently from its
//! neighbours is a bug, and keeping them adjacent is what makes that visible.
//! # The minimal surface
//! ```ignore
//! async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult>
//! ```
//! `args` is the **verbatim JSON string**, not a parsed object — the same form
//! `protocol::ToolCall` stores, so there is no parse/serialize round trip here to disturb key
//! order (which would break prompt-cache stability).
//! # Tools report facts; policy decides
//! A tool does **not** make permission decisions. It works out facts — is this path inside the
//! workspace — and leaves approval to the policy service. Putting the judgement inside tools
//! means re-implementing the security boundary once per tool, and eventually missing one.
//! # A tool's own failure is not an `Err`
//! "File not found" comes back as [`ToolExecStatus::Failed`] so the model can correct itself.
//! `Err` is reserved for "this call cannot be executed at all" — unparseable arguments, a
//! capability this process does not have.
//! # A result has distinct audiences, and they have distinct fields
//! | Field | Audience | Constraint |
//! |---|---|---|
//! | `content` | the **model** | text plus persisted, MIME-described files |
//! | `display` | the **UI** | tables, diffs, file cards, agent cards; never costs a token |
//! | `objects` | **persistence** | object references; the payload itself is never in the row |
//! | `skill_load` | **session context state** | a loaded definition core persists separately |
//! | `loaded_tools` | **session context state** | deferred tools `load_tool` just made available |
//! They were one field with overloaded meaning before, and that forced compromises in both
//! directions: a diff had to be flattened into prose for the model, and a large output had to be
//! either truncated for everyone or held in memory for the UI's benefit. Splitting them means each
//! audience gets the form it can actually use, and only `content` costs tokens.
//! The three content/display/object lists remain parallel rather than grouped, for two reasons. The cardinalities
//! do not line up — a build that wrote three files produces three diff cards and *one* summary line
//! for the model, and a progress card has no model-facing text at all. And every consumer is
//! column-oriented: core wants all the object references to build an entry, all the content to
//! build a tool result, all the display to push to the UI. Grouping would make each of them
//! iterate and filter.
//! What grouping *would* have fixed is real, though: the same object appears in all three lists,
//! correlated only by its id. So capturing a payload goes through [`ToolCtx::capture`], which
//! writes all three coherently, and [`ToolExecResult::dangling_display_objects`] makes a mismatch
//! detectable instead of latent.

pub mod agent;
pub mod archive;
pub mod compute;
pub mod display;
pub mod exec;
pub mod file;
pub mod net;
pub mod registry;
pub mod resources;
pub mod runtime;
pub mod search;
pub mod sensitive;
pub mod session;
pub mod task;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
pub use zlogic_objects::{ObjectRef, ObjectRole};

use serde::{Deserialize, Serialize};
use zlogic_objects::{ObjectId, ObjectStore};
use zlogic_protocol::interaction::{InteractionBody, InteractionDecision};
use zlogic_protocol::llm::ToolDefinition;
use zlogic_protocol::{CallId, EntryId, SessionId, TurnId};
// Re-exported so a tool author does not have to name tokio-util, and so there is visibly one
// cancellation type in the system rather than a wrapper per layer.
pub use tokio_util::sync::CancellationToken;

pub use agent::{AgentMailboxGate, AgentOutcome, AgentRequest, AgentSpawner, CreateAgent};
pub use archive::{ArchiveInfo, ArchiveProcess};
pub use compute::Time;
pub use display::{DiffStat, FileChange, MathLine, ToolDisplay};
pub use exec::{Shell, ShellDialect, ShellPreference};
pub use file::{Edit, ReadFile, WriteFile};
pub use net::{SearchKeySource, SearchProvider, WebFetch, WebSearch, WebSearchSettings};
pub use registry::{Materialized, ToolRegistry, ToolSource};
pub use resources::{ManagedResourceConnection, ManagedResourceHost, ManagedResourceProvider};
pub use runtime::{executable_on_path, executable_on_path_in};
pub use search::{Glob, Grep, ListDir};
pub use sensitive::{
    ResourceAuthorization, SensitiveEnvironment, SensitiveResource, authorize_sensitive_resource,
};
pub use session::{
    AskUser, EnterWorktree, ExitAction, ExitWorktree, ExitedWorktree, LoadedSkill, MemoryHost,
    MemoryUpdate, Skill, SkillHost, WorktreeChanges, WorktreeHost, WorktreeState,
};
pub use task::{ProcessRequest, SpawnedProcess, TaskGet, TaskHost, TaskReport, TaskStop};
// Interaction lives in `protocol`, not here: core raises permission requests from the
// tool-execution gate and tools raise forms, so a definition in this crate would have to be
// duplicated — and duplicated definitions drift.
pub use zlogic_protocol::interaction::{
    Choice, Control, FieldValue, Form, FormAnswer, FormField, InteractionPort, InteractionRequest,
};
pub use zlogic_protocol::stream::{OutputSink, OutputStream};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    BadArgs(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Object(#[from] zlogic_objects::ObjectError),
    #[error("{0}")]
    Failed(String),
    #[error("not available in this run: {0}")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, ToolError>;

/// Static risk level. A **signal** for the approval pipeline, not a verdict — a `High` tool
/// such as `shell` spends most of its calls running `git status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    /// Read-only, no side effects.
    Read,
    /// Changes files or state, but reversibly.
    Write,
    /// Irreversible, or reaches the outside world (network, processes, publishing).
    High,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolMeta {
    pub name: String,
    /// builtin / skill / mcp / subagent / a2a — how the approval pipeline categorises it.
    pub source: &'static str,
    pub risk: ToolRisk,
}

/// A call that should not be made, with the reason it should not.
/// Two fields rather than one string because the two halves are rendered differently: the arguments
/// belong in a code span, the reason does not. Concatenated, the reason reads like part of the JSON —
/// which is how the first version of this looked, and it is the kind of confusion a model reproduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptExample {
    pub args: &'static str,
    pub why: &'static str,
}

/// Model-facing usage guidance owned by the tool that defines the input contract.
/// The full JSON schema still lives in [`Tool::definition`]. This is only the cross-call workflow
/// and canonicalization advice that belongs in the system prompt. Keeping it beside the tool
/// prevents a second name-to-prompt switch in the engine from drifting as tools are added,
/// removed, or replaced.
/// # Why it is split in two
/// The two halves answer different questions and are needed at different moments:
/// - [`when`](Self::when) — *should I reach for this at all?* One line, and the only part a model
///   needs before it has the tool's schema. Always injected while the tool is visible.
/// - [`contract`](Self::contract) plus the examples — *how do I call it correctly?* Useless until the
///   model can actually call the tool.
/// That split exists because of deferral. A deferred tool's schema is withheld until `load_tool`, but
/// the system prompt is assembled once at submit time, when nothing is loaded — so injecting the full
/// contract there spends the tokens deferral was meant to save, for a tool the turn may never touch.
/// Eager tools get both in the system prompt; deferred tools get `when` there and the contract in the
/// `load_tool` result, which is exactly when the tool becomes callable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPromptSpec {
    /// One line: when to reach for this tool rather than another. No input details.
    pub when: &'static str,
    /// The input and workflow contract. Paid for only when the tool is callable.
    pub contract: &'static str,
    pub positive_examples: &'static [&'static str],
    pub negative_examples: &'static [PromptExample],
}

impl ToolPromptSpec {
    /// The contract half as lines, without indentation or a leading tool name.
    /// Lines rather than a finished block because the two call sites — the system prompt (eager
    /// tools) and the `load_tool` result (deferred tools) — nest them differently. What must not
    /// differ is how an example is written: the reason belongs outside the code span, and two
    /// hand-rolled renderers would eventually disagree about that in a way nothing would catch.
    pub fn contract_lines(&self) -> Vec<String> {
        let mut lines = vec![self.contract.to_string()];
        lines.extend(
            self.positive_examples
                .iter()
                .map(|example| format!("- canonical: `{example}`")),
        );
        lines.extend(
            self.negative_examples
                .iter()
                .map(|example| format!("- avoid: `{}` — {}", example.args, example.why)),
        );
        lines
    }
}

/// A prompt specification paired with the effective registered tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPrompt {
    pub name: String,
    pub spec: ToolPromptSpec,
    /// Whether the model still has to `load_tool` this one.
    /// Decides where the contract goes; see [`ToolPromptSpec`]. Carried here because the registry
    /// knows a tool's exposure and the prompt assembler does not.
    pub deferred: bool,
}

/// How a tool call ended.
/// Replaces a bare `is_error: bool`, which conflated three situations the UI and the approval
/// audit need to tell apart: the tool ran and failed, the user refused, and the call never got
/// to run. All three are still reported to the model as an error result so it can adapt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecStatus {
    Success,
    /// The tool ran and reported a problem.
    Failed,
    /// Refused by policy or by the user.
    Denied,
    /// Exceeded its time budget.
    Timeout,
    /// Rejected before running: unknown tool, unparseable arguments, schema mismatch.
    /// Synthesised by the runtime, never by a tool.
    PrecheckFailed,
    /// The conversation was cancelled: the tool wound down early, or it never started.
    /// Distinct from `Denied` (somebody refused permission) and from `Failed` (it tried and could
    /// not): an audit view has to be able to say "you interrupted this", and a model reading the
    /// result needs to know the work is unfinished rather than impossible.
    Cancelled,
}

impl ToolExecStatus {
    /// Whether the model should see this as an error result.
    pub fn is_error(self) -> bool {
        !matches!(self, ToolExecStatus::Success)
    }
}

/// One piece of what the **model** sees.
/// Text plus files that may be projected into model context. A diff card or progress log has no
/// model representation and does not belong here — that is what `display` is for.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolContent {
    Text(String),
    /// A file whose bytes have already been persisted in the object store.
    /// This is generic rather than image-specific: context projection decides from `mime_type`
    /// whether to send media, bounded UTF-8 text, or a binary placeholder.
    File(ToolFile),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFile {
    name: String,
    mime_type: String,
    object_id: ObjectId,
    bytes: u64,
}

impl ToolFile {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn object_id(&self) -> &ObjectId {
        &self.object_id
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl ToolContent {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ToolContent::Text(t) => Some(t),
            ToolContent::File(_) => None,
        }
    }
}

/// What a tool produced, split by who consumes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolExecResult {
    pub status: ToolExecStatus,
    /// For the **model**. Text and images only.
    pub content: Vec<ToolContent>,
    /// For the **UI**. Never part of the model's context, so it costs no tokens.
    /// A list because one call can produce several things worth showing: a build that wrote three
    /// files, or output plus a diff.
    pub display: Vec<ToolDisplay>,
    /// For **persistence**. What the entry records as references.
    /// Also a list: a tool can capture a large stdout *and* a diff, and both must stay reachable
    /// and both must be kept alive by garbage collection.
    pub objects: Vec<ObjectRef>,
    /// Session state produced by the built-in skill loader. Core persists this beside the tool
    /// result so model-initiated and user-initiated loads share one source of truth.
    pub skill_load: Option<LoadedSkill>,
    /// Names of deferred tools the model loaded with `load_tool`. Core persists them as session
    /// state, so a loaded tool stays loaded on later turns instead of being re-requested.
    pub loaded_tools: Vec<String>,
}

impl ToolExecResult {
    pub fn new(status: ToolExecStatus) -> Self {
        Self {
            status,
            content: Vec::new(),
            display: Vec::new(),
            objects: Vec::new(),
            skill_load: None,
            loaded_tools: Vec::new(),
        }
    }

    pub fn success(text: impl Into<String>) -> Self {
        Self::new(ToolExecStatus::Success).with_text(text)
    }

    /// The tool ran and failed. Reported to the model, not raised as `Err`.
    pub fn failed(text: impl Into<String>) -> Self {
        Self::new(ToolExecStatus::Failed).with_text(text)
    }

    pub fn denied(text: impl Into<String>) -> Self {
        Self::new(ToolExecStatus::Denied).with_text(text)
    }

    pub fn timeout(text: impl Into<String>) -> Self {
        Self::new(ToolExecStatus::Timeout).with_text(text)
    }

    /// Interrupted, or never started. Still a **result**: a `tool_calls` group missing any of its
    /// results is a replay error on every provider, so a cancelled call is recorded, not skipped.
    pub fn cancelled(text: impl Into<String>) -> Self {
        Self::new(ToolExecStatus::Cancelled).with_text(text)
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.content.push(ToolContent::text(text));
        self
    }

    pub fn with_display(mut self, d: ToolDisplay) -> Self {
        self.display.push(d);
        self
    }

    pub fn with_object(mut self, o: ObjectRef) -> Self {
        self.objects.push(o);
        self
    }

    pub fn with_skill_load(mut self, loaded: LoadedSkill) -> Self {
        self.skill_load = Some(loaded);
        self
    }

    /// Names the `load_tool` built-in just made available. Core persists them as session state.
    pub fn with_loaded_tools(mut self, names: Vec<String>) -> Self {
        self.loaded_tools = names;
        self
    }

    pub fn is_error(&self) -> bool {
        self.status.is_error()
    }

    /// The model-facing content flattened to text.
    /// Files become explicit placeholders here; [`zlogic_core`] later projects their structured
    /// metadata according to MIME. Flattening is still used by transcript persistence and by
    /// callers that only have a text channel, so a file must never silently disappear.
    pub fn model_text(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                ToolContent::Text(t) => t.clone(),
                ToolContent::File(file) => format!(
                    "[file: {} ({}, {} bytes)]",
                    file.name, file.mime_type, file.bytes
                ),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn object_with_role(&self, role: ObjectRole) -> Option<&ObjectId> {
        self.objects
            .iter()
            .find(|o| o.role == role)
            .map(|o| &o.object_id)
    }

    /// Objects a display card points at but nothing in `objects` keeps alive.
    /// Empty is the only correct answer. A non-empty result means the UI would eventually render a
    /// broken link: garbage collection keeps an object because an entry references it, and a card
    /// is not a reference. Checked in tests rather than asserted at runtime — a tool returning a
    /// slightly wrong card should not take the turn down.
    pub fn dangling_display_objects(&self) -> Vec<&ObjectId> {
        let mut dangling: Vec<&ObjectId> = self
            .display
            .iter()
            .filter_map(|d| d.object_id())
            .filter(|id| !self.objects.iter().any(|o| &o.object_id == *id))
            .collect();
        dangling.extend(self.content.iter().filter_map(|content| {
            match content {
                ToolContent::File(file)
                    if !self
                        .objects
                        .iter()
                        .any(|object| object.object_id == file.object_id) =>
                {
                    Some(&file.object_id)
                }
                ToolContent::Text(_) | ToolContent::File(_) => None,
            }
        }));
        dangling
    }
}

/// The context of one tool call.
pub struct ToolCtx {
    /// Cancelled when the conversation is.
    /// **One token for the whole conversation**, created by the UI (or the CLI) and passed straight
    /// down through the engine — core, tools and sub-agents all hold the same one. Nothing derives a
    /// child token from it: "stop" means stop everywhere, and per-layer tokens would put the burden
    /// of forwarding on every layer, where one forgotten hop leaves work running after the user
    /// pressed Esc.
    /// **A tool is asked to stop, not killed.** The runtime does not race this against the tool's
    /// future: dropping a half-finished write leaves the filesystem in a state nothing recorded, and
    /// dropping a subprocess reader throws away the output that had already arrived. A long-running
    /// tool should poll [`ToolCtx::is_cancelled`] (or select on `cancel.cancelled()`) and return
    /// what it has — it is the only party that knows what "cleanly" means for it.
    /// A tool that never awaits — the built-in file tools — simply never observes it and runs to
    /// completion. That is correct: they finish in microseconds.
    pub cancel: CancellationToken,
    /// Where tools actually run. **Read fresh on every call** — it can migrate mid-session
    /// (into a worktree and back), so it is not a per-turn snapshot.
    pub exec_cwd: PathBuf,
    /// The workspace root.
    /// One directory, not a list. Every tool takes a single path and there is exactly one
    /// `exec_cwd`, so a list only ever raised questions with no principled answer — which root do
    /// relative paths resolve against, whose `AGENTS.md` wins. `inside_workspace` is also a
    /// sharper predicate against one root than "inside any of several".
    /// Injected per call rather than stored on the session: if the workspace is later pointed at a
    /// different directory, nothing has to be migrated.
    pub root: PathBuf,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub call_id: CallId,
    pub objects: Arc<dyn ObjectStore>,
    /// Present only when an agent runtime is wired in; `create_agent` fails closed without it.
    pub spawner: Option<Arc<dyn AgentSpawner>>,
    /// Present only when the process-wide task runtime is wired in. The host owns durable runs and
    /// process-local handles; this context only supplies the origin of an operation.
    pub tasks: Option<Arc<dyn TaskHost>>,
    /// Present only when a skill library is wired in; `skill` fails closed without it.
    /// Reading the library needs the data directory and the workspace root, so the implementation
    /// lives in engine — same boundary as [`AgentSpawner`] and [`WorktreeHost`].
    pub skills: Option<Arc<dyn SkillHost>>,
    /// Present only when worktrees are wired in; `enter_worktree` / `exit_worktree` fail closed
    /// without it.
    /// Bound to **this session**, which is what makes "the worktree I entered" unambiguous and
    /// keeps one session from moving another. Moving into one changes [`ToolCtx::exec_cwd`] for
    /// every call after it — including calls later in the same batch, since that field is read
    /// fresh per call rather than snapshotted per turn.
    pub worktree: Option<Arc<dyn WorktreeHost>>,
    /// Present only when there is a UI to ask. Tools that need a decision fail closed
    /// without it — see [`ToolCtx::ask`].
    pub interaction: Option<Arc<dyn InteractionPort>>,
    /// Where incremental output goes. `None` discards it, which is what a batch run wants.
    pub output: Option<Arc<dyn OutputSink>>,
    /// Output longer than this is offloaded to the object store, with head and tail kept.
    pub max_result_chars: usize,
    pub runtime_paths: Vec<PathBuf>,
}

pub trait RuntimePathProvider: Send + Sync {
    fn bin_dirs(&self) -> Vec<PathBuf>;
}

/// The outcome of resolving a path. **States facts; draws no conclusions.**
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPath {
    pub path: PathBuf,
    /// Whether it falls inside the workspace root **or the session's working directory**. The
    /// approval pipeline decides what to do about it.
    /// The second half matters once a session enters a worktree: the checkout lives outside the
    /// root (a sibling directory by default), so a root-only test would report every file the
    /// session is there to edit as "outside the workspace" — and a policy layer reading that would
    /// ask about each one.
    pub inside_workspace: bool,
}

impl ToolCtx {
    /// Resolves a required path argument after rejecting the empty string.
    /// Serde's `required` only proves that the key exists; without this check `{"path":""}`
    /// resolves to the current working directory, which is especially dangerous for destructive
    /// tools. Keep this at the shared boundary so every file tool applies the same rule.
    pub fn resolve_required_path(&self, field: &str, raw: &str) -> Result<ResolvedPath> {
        if raw.trim().is_empty() {
            return Err(ToolError::BadArgs(format!("{field} must not be empty")));
        }
        Ok(self.resolve_path(raw))
    }

    /// Expands a relative path against [`ToolCtx::exec_cwd`] and reports whether it lands
    /// inside the workspace.
    /// **Does not reject escapes** — that is policy's call. Reading `/etc/hosts` is a perfectly
    /// reasonable request; whether to allow it depends on the content (is it a credential path)
    /// and the current permission mode, not on which tool asked.
    pub fn resolve_path(&self, raw: &str) -> ResolvedPath {
        let p = Path::new(raw);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.exec_cwd.join(p)
        };
        let path = normalize(&joined);
        // Either boundary counts. `exec_cwd` equals `root` in the ordinary case, so this only
        // widens anything when the session has deliberately moved — into a worktree of the same
        // project, which is morally as much "the workspace" as the root is.
        let inside =
            path.starts_with(normalize(&self.root)) || path.starts_with(normalize(&self.exec_cwd));
        ResolvedPath {
            path,
            inside_workspace: inside,
        }
    }

    /// Whether the conversation has been cancelled. See [`ToolCtx::cancel`].
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Streams a chunk of tool output to the UI.
    /// Fire and forget: there is no error to handle and nothing to await. A long-running tool
    /// should call this as output arrives rather than accumulating and reporting at the end —
    /// waiting until a ten-minute build finishes means ten minutes of a blank screen.
    /// These chunks are **not persisted individually**. The full transcript goes to the object
    /// store once, and the entry keeps a reference; per-chunk rows would swamp the database.
    pub fn emit(&self, stream: OutputStream, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if let Some(sink) = &self.output {
            sink.emit(stream, chunk);
        }
    }

    pub fn stdout(&self, chunk: &str) {
        self.emit(OutputStream::Stdout, chunk);
    }

    pub fn stderr(&self, chunk: &str) {
        self.emit(OutputStream::Stderr, chunk);
    }

    /// Progress that is neither stdout nor stderr — "3 of 12 files", "connecting".
    pub fn progress(&self, chunk: &str) {
        self.emit(OutputStream::Progress, chunk);
    }

    /// Asks the user something.
    /// This is a tool *requesting input*, which is different from the permission gate the runtime
    /// applies before calling a tool: that one happens without the tool's involvement.
    /// Fails closed when no UI is attached. A headless run must not invent an answer: silently
    /// choosing "yes" would make a batch job destructive, and silently choosing "no" would look
    /// like the tool simply failed.
    pub async fn ask(&self, body: InteractionBody) -> Result<InteractionDecision> {
        let port = self.interaction.as_ref().ok_or(ToolError::Unsupported(
            "asking the user requires an attached UI",
        ))?;
        port.ask(InteractionRequest {
            interaction_id: EntryId::new().to_string(),
            session_id: self.session_id,
            turn_id: self.turn_id,
            call_id: Some(self.call_id.clone()),
            body,
        })
        .await
        .map_err(ToolError::Failed)
    }

    /// Asks a form and returns the answers, cleaned.
    /// Cleaning is not validation: nothing is rejected and the user is never sent back to fix
    /// something. It only removes what a tool could not safely act on — a `Select` value that was
    /// never offered, a key the form did not ask about. Length and range constraints pass straight
    /// through, because the model reads the answer and can cope.
    /// A dropped value leaves the field simply unanswered, which every caller has to handle anyway
    /// since `required` is a hint.
    pub async fn ask_form(&self, form: Form) -> Result<Option<FormAnswer>> {
        let spec = form.clone();
        match self.ask(InteractionBody::Form(form)).await? {
            InteractionDecision::Submitted(answer) => {
                let (clean, dropped) = spec.sanitize(answer);
                for note in dropped {
                    // A developer-facing signal that a client sent something odd. Never shown to
                    // the user as a failure.
                    tracing::warn!(target: "zlogic::tools", "form answer: {note}");
                }
                Ok(Some(clean))
            }
            // Cancelled or denied: the tool decides what to do, it is not an error.
            _ => Ok(None),
        }
    }

    /// Stores a payload and returns the three things that describe it, consistently.
    /// The single place a captured payload is turned into "what the model reads", "what the UI can
    /// expand" and "what keeps the object alive". Doing it by hand means writing the same object id
    /// into three lists and getting one of them wrong eventually — most likely the reference, which
    /// fails silently until garbage collection removes the object and the card goes dead.
    pub fn capture(
        &self,
        role: ObjectRole,
        ref_key: Option<&str>,
        full: &str,
        recovery: Recovery,
    ) -> Result<Capture> {
        let id = self.objects.put(full.as_bytes())?;
        let total_chars = full.chars().count();
        let truncated = total_chars > self.max_result_chars;

        let model_text = if truncated {
            let head_budget = self.max_result_chars / 3;
            let split = split_head_tail(full, head_budget, self.max_result_chars - head_budget);
            format!(
                "{}\n\n{}\n\n{}",
                split.head,
                split.note(recovery),
                split.tail
            )
        } else {
            full.to_string()
        };

        Ok(Capture {
            model_text,
            display: ToolDisplay::Output {
                object_id: id.clone(),
                total_chars: total_chars as u64,
                truncated,
            },
            object: match ref_key {
                Some(k) => ObjectRef::keyed(id, role, k),
                None => ObjectRef::new(id, role),
            },
        })
    }

    /// Stores arbitrary file bytes and constructs their model, UI and persistence projections
    /// together. The returned value must be added with [`CapturedFile::add_to`] or
    /// [`CapturedFile::into_result`], which prevents a media card from outliving its object.
    pub fn capture_file(
        &self,
        name: impl Into<String>,
        mime_type: impl Into<String>,
        bytes: &[u8],
    ) -> Result<CapturedFile> {
        let id = self.objects.put(bytes)?;
        Ok(captured_file(
            id,
            name.into(),
            mime_type.into(),
            bytes.len() as u64,
        ))
    }

    /// File-backed counterpart of [`ToolCtx::capture_file`]. `put_path` streams into the object
    /// store, so a large recording or archive is not first copied into one in-memory `Vec`.
    pub fn capture_file_path(
        &self,
        name: impl Into<String>,
        mime_type: impl Into<String>,
        path: &Path,
    ) -> Result<CapturedFile> {
        let id = self.objects.put_path(path)?;
        let bytes = self.objects.size(&id)?;
        Ok(captured_file(id, name.into(), mime_type.into(), bytes))
    }

    /// The shared treatment for large output: the whole thing goes to the object store, and
    /// the model gets head plus tail.
    /// Weighted towards the **tail** — build and test failures are almost always at the end.
    /// `recovery` is how the model gets at the part it did not receive. It has to come from the
    /// caller because only the tool knows: re-reading a file by line range, narrowing a pattern and
    /// filtering a command's output are three different answers, and a generic "some output was
    /// omitted" leaves the model to guess between them.
    pub fn offload_if_large(&self, full: &str, recovery: Recovery) -> Result<ToolExecResult> {
        let chars: Vec<char> = full.chars().collect();
        if chars.len() <= self.max_result_chars {
            return Ok(ToolExecResult::success(full));
        }
        Ok(self
            .capture(ObjectRole::Output, None, full, recovery)?
            .into_result())
    }

    pub fn result_with_full_output(
        &self,
        full: &str,
        recovery: Recovery,
    ) -> Result<ToolExecResult> {
        if full.chars().count() <= self.max_result_chars {
            Ok(self
                .capture(ObjectRole::Output, None, full, recovery)?
                .into_result())
        } else {
            self.offload_if_large(full, recovery)
        }
    }
}

/// How the model can reach the part of a payload it did not receive.
/// A closed set rather than a free-form string, for two reasons. The wording stays consistent
/// across tools, which matters because the model learns these sentences. And a new tool has to
/// *choose*, rather than inherit whatever the tool it was copied from happened to say.
/// Note what is deliberately absent: an object id. The model has no tool that takes one, so telling
/// it "the full output is at sha256:…" only invites it to invent a way to use that — most often by
/// passing the id to `read_file` as if it were a path. What it can act on is a line range and a
/// narrower request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// The content came from a file that is still on disk: read it again by line range.
    ReadFileRange,
    /// Ask for less — a tighter pattern, a smaller directory, a lower limit.
    Narrow,
    /// Re-run the command with the filtering done at the source, so only the wanted part comes
    /// back at all.
    FilterAtSource,
    /// Nothing useful to suggest. The omitted part is simply not retrievable through a tool.
    Unavailable,
}

impl Recovery {
    fn sentence(self) -> &'static str {
        match self {
            Recovery::ReadFileRange => {
                "Read the omitted lines with read_file's ranges, in pieces if it is long."
            }
            Recovery::Narrow => {
                "To see more, ask for less: a more specific pattern, a narrower path, or a smaller \
                 limit."
            }
            Recovery::FilterAtSource => {
                "To see the middle, re-run with the filtering at the source — pipe through grep, \
                 head or tail so only what you need comes back."
            }
            Recovery::Unavailable => "The omitted part is not retrievable.",
        }
    }
}

/// A head and a tail of a payload, cut at **line** boundaries.
/// # Why lines and not characters
/// A character offset lands wherever it lands, which means the last line of the head and the first
/// line of the tail are both fragments. That is worse than it sounds: half a line of code reads as
/// valid but different code, and a model that acts on `if (user.isAdmin` — the rest of the
/// condition having been cut — is reasoning about something the file does not say. A fragment is
/// not a smaller truth, it is a plausible falsehood.
/// It also makes the omitted **range** exact rather than approximate, and a line range is the one
/// thing a model can act on: it is what `read_file` takes.
pub(crate) struct HeadTailSplit {
    pub head: String,
    pub tail: String,
    /// 1-based inclusive range of the dropped lines. `None` only in the mid-line fallback below.
    dropped_lines: Option<(usize, usize)>,
    omitted_chars: usize,
    total_lines: usize,
}

impl HeadTailSplit {
    /// The marker that replaces what was cut, and what to do about it.
    pub fn note(&self, recovery: Recovery) -> String {
        let where_ = match self.dropped_lines {
            Some((first, last)) if first == last => {
                format!("line {first} of {}", self.total_lines)
            }
            Some((first, last)) => format!("lines {first}-{last} of {}", self.total_lines),
            // The fallback case: one line was longer than the whole budget, so there is no line
            // range to name. Saying so is better than naming a range that is not what was cut.
            None => "a single line too long to show whole was cut".to_string(),
        };
        format!(
            "… {where_} omitted here ({} characters; the start and the end are kept). {} …",
            self.omitted_chars,
            recovery.sentence()
        )
    }
}

/// Splits `full` into a head and a tail that together fit the budgets, cutting only between lines.
/// Each budget takes whole lines while they fit. The **tail** is normally given the larger budget by
/// the caller — build and test failures are at the end.
/// # The one case that cannot be line-aligned
/// A single line longer than both budgets: a minified bundle, a one-line JSON blob, a base64 payload.
/// Line alignment would return nothing at all there, so it falls back to a character cut — which is
/// correct, because for a file with no line structure there is no structure to preserve. The note
/// says that is what happened rather than claiming a line range.
pub(crate) fn split_head_tail(full: &str, head_budget: usize, tail_budget: usize) -> HeadTailSplit {
    // `split_inclusive` keeps each line's own newline, so head and tail concatenate back into
    // exact substrings of the original — no re-joining, and no invented or lost terminator.
    let lines: Vec<&str> = full.split_inclusive('\n').collect();
    let total_chars = full.chars().count();

    let mut head_lines = 0usize;
    let mut head_chars = 0usize;
    for line in &lines {
        let n = line.chars().count();
        if head_chars + n > head_budget {
            break;
        }
        head_chars += n;
        head_lines += 1;
    }

    let mut tail_start = lines.len();
    let mut tail_chars = 0usize;
    for i in (head_lines..lines.len()).rev() {
        let n = lines[i].chars().count();
        if tail_chars + n > tail_budget {
            break;
        }
        tail_chars += n;
        tail_start = i;
    }

    // Nothing survived line alignment: the first and last lines are each bigger than their budget.
    if head_lines == 0 && tail_start == lines.len() {
        let head: String = full.chars().take(head_budget).collect();
        let tail: String = {
            let skip = total_chars.saturating_sub(tail_budget);
            full.chars().skip(skip).collect()
        };
        let omitted = total_chars.saturating_sub(head.chars().count() + tail.chars().count());
        return HeadTailSplit {
            head,
            tail,
            dropped_lines: None,
            omitted_chars: omitted,
            total_lines: lines.len(),
        };
    }

    HeadTailSplit {
        head: lines[..head_lines].concat(),
        tail: lines[tail_start..].concat(),
        // Lines 1..=head_lines were kept and tail_start+1..=len were kept, so the gap is exactly
        // what lies between — an exact range, which is what line alignment bought.
        dropped_lines: Some((head_lines + 1, tail_start)),
        omitted_chars: total_chars.saturating_sub(head_chars + tail_chars),
        total_lines: lines.len(),
    }
}

/// A stored payload, described for all three audiences.
/// The three fields are produced together and are guaranteed to agree about the object — which is
/// the whole reason this type exists rather than three separate calls.
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    /// What the model reads: the whole thing when small, head and tail when not.
    pub model_text: String,
    /// An expandable card for the UI.
    pub display: ToolDisplay,
    /// The reference that keeps the object alive.
    pub object: ObjectRef,
}

impl Capture {
    /// A successful result made of just this capture.
    pub fn into_result(self) -> ToolExecResult {
        ToolExecResult::success(self.model_text)
            .with_display(self.display)
            .with_object(self.object)
    }

    /// Adds this capture to an existing result, all three facets at once.
    pub fn add_to(self, result: ToolExecResult) -> ToolExecResult {
        result
            .with_text(self.model_text)
            .with_display(self.display)
            .with_object(self.object)
    }

    pub fn object_id(&self) -> &ObjectId {
        &self.object.object_id
    }
}

/// One persisted file, already split for the three result audiences.
/// Constructed only by [`ToolCtx::capture_file`] and [`ToolCtx::capture_file_path`], so the
/// content descriptor, display card and object reference cannot disagree about the object id.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedFile {
    pub content: ToolFile,
    pub display: ToolDisplay,
    pub object: ObjectRef,
}

impl CapturedFile {
    pub fn into_result(self) -> ToolExecResult {
        self.add_to(ToolExecResult::new(ToolExecStatus::Success))
    }

    pub fn add_to(self, mut result: ToolExecResult) -> ToolExecResult {
        result.content.push(ToolContent::File(self.content));
        result.display.push(self.display);
        result.objects.push(self.object);
        result
    }

    pub fn object_id(&self) -> &ObjectId {
        &self.object.object_id
    }
}

fn captured_file(id: ObjectId, name: String, mime_type: String, bytes: u64) -> CapturedFile {
    let name = sanitize_file_name(&name);
    let mime_type = normalize_mime(&mime_type);
    CapturedFile {
        content: ToolFile {
            name: name.clone(),
            mime_type: mime_type.clone(),
            object_id: id.clone(),
            bytes,
        },
        display: ToolDisplay::File {
            path: name.clone(),
            mime: Some(mime_type),
            bytes,
            total_lines: None,
            object_id: Some(id.clone()),
        },
        object: ObjectRef::keyed(id, ObjectRole::Output, name),
    }
}

fn sanitize_file_name(name: &str) -> String {
    let clean = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let clean: String = clean.chars().take(240).collect();
    if clean.is_empty() {
        "tool-output".into()
    } else {
        clean
    }
}

/// Returns the lower-case MIME essence (`type/subtype`), discarding parameters.
/// Parameters such as `charset=utf-8` describe representation details, not which context/UI
/// projector should handle the file. Keeping three hand-written parsers in tools, core and engine
/// made a valid `text/plain; charset=utf-8` turn into an opaque binary at one boundary but not the
/// others, so this is the shared boundary for all three.
pub fn normalize_mime(value: &str) -> String {
    value
        .trim()
        .parse::<mime::Mime>()
        .map(|mime| mime.essence_str().to_ascii_lowercase())
        .unwrap_or_else(|_| "application/octet-stream".into())
}

/// Path normalisation that never touches the filesystem (`.`, `..`, redundant separators).
/// Deliberately **not** `canonicalize`: that requires the path to exist, which rules it out for
/// `write_file` creating a new file, and it resolves symlinks, which makes the path we judge
/// differ from the path the user typed.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn meta(&self) -> ToolMeta;

    /// Dynamic availability checked while materialising each turn.
    /// Optional-runtime tools remain in the catalog so a workspace can select them, but are not
    /// advertised to the model until their extension group and verified dependencies are ready.
    fn available(&self) -> bool {
        true
    }

    /// Whether only the root agent may see this tool.
    /// Filtering happens while materialising definitions, not when a sub-agent calls it: a tool
    /// the model is not allowed to use must not be advertised to it.
    fn root_only(&self) -> bool {
        false
    }

    /// Whether running this tool can change files in the workspace.
    /// Kept separate from [`ToolRisk`]: a durable state write may require approval without
    /// deserving a pair of filesystem snapshots.
    fn affects_workspace(&self) -> bool {
        self.meta().risk != ToolRisk::Read
    }

    /// Whether the full tool definition is sent immediately or only after `load_tool`.
    /// Deferred tools still belong to the registry and workspace allowlists. The system prompt
    /// exposes only their name and one-line description; loading makes their full schema visible
    /// for the remaining rounds of the current turn.
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Eager
    }

    /// The definition the model sees (goes into `LlmRequest.tools`).
    fn definition(&self) -> ToolDefinition;

    /// Canonical input/workflow guidance injected only when this exact tool is visible.
    /// Most tools need no system-prompt text: a good description and schema are sufficient.
    fn prompt_spec(&self) -> Option<ToolPromptSpec> {
        None
    }

    /// `args` is a verbatim JSON string.
    /// Note: the built-in file tools use **blocking IO**. They are short, but a host on a
    /// single-threaded executor should run them under `spawn_blocking`.
    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExposure {
    Eager,
    Deferred,
}

/// Parses arguments, mapping serde errors onto [`ToolError::BadArgs`].
/// An empty string is treated as `{}` — several providers send exactly that for a tool with no
/// required arguments.
pub fn parse_args<T: serde::de::DeserializeOwned>(args: &str) -> Result<T> {
    let args = if args.trim().is_empty() { "{}" } else { args };
    serde_json::from_str(args).map_err(|e| ToolError::BadArgs(e.to_string()))
}

/// Parses a guided tool's canonical JSON input and returns an actionable repair contract.
/// The schema in the model request remains authoritative. This repeats only the tool-owned
/// normalization guidance and examples because a parse failure occurs in a later model round,
/// where a concrete positive/negative pair is much easier to repair than a bare serde message.
pub fn parse_args_with_prompt<T: serde::de::DeserializeOwned>(
    args: &str,
    prompt: Option<ToolPromptSpec>,
) -> Result<T> {
    parse_args(args).map_err(|error| match (error, prompt) {
        (ToolError::BadArgs(error), Some(prompt)) => {
            let canonical = prompt
                .positive_examples
                .iter()
                .map(|example| format!("- {example}"))
                .collect::<Vec<_>>()
                .join("\n");
            let avoid = prompt
                .negative_examples
                .iter()
                .map(|example| format!("- {} — {}", example.args, example.why))
                .collect::<Vec<_>>()
                .join("\n");
            ToolError::BadArgs(format!(
                "{error}\nExpected workflow: {}\nCanonical examples:\n{}\nInvalid/non-canonical examples:\n{}",
                prompt.contract,
                if canonical.is_empty() { "- none" } else { &canonical },
                if avoid.is_empty() { "- none" } else { &avoid },
            ))
        }
        (error, _) => error,
    })
}

#[cfg(test)]
pub(crate) fn test_ctx(root: &Path) -> ToolCtx {
    ToolCtx {
        exec_cwd: root.to_path_buf(),
        root: root.to_path_buf(),
        session_id: SessionId::new(),
        turn_id: TurnId::new(),
        call_id: CallId::new("call_test"),
        objects: Arc::new(zlogic_objects::MemoryObjectStore::new()),
        spawner: None,
        tasks: None,
        skills: None,
        worktree: None,
        interaction: None,
        output: None,
        max_result_chars: 100,
        runtime_paths: Vec::new(),
        cancel: CancellationToken::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn relative_paths_resolve_against_exec_cwd() {
        let c = test_ctx(Path::new("/work"));
        let r = c.resolve_path("src/main.rs");
        assert_eq!(r.path, PathBuf::from("/work/src/main.rs"));
        assert!(r.inside_workspace);
    }

    #[test]
    fn guided_argument_errors_include_the_parser_error_and_repair_examples() {
        #[derive(Debug, Deserialize)]
        struct Input {
            _value: usize,
        }
        let error = parse_args_with_prompt::<Input>(
            r#"{"value":"wrong"}"#,
            Some(ToolPromptSpec {
                when: "Reach for it when normalizing.",
                contract: "Normalize the request.",
                positive_examples: &[r#"{"_value":1}"#],
                negative_examples: &[PromptExample {
                    args: r#"{"_value":"one"}"#,
                    why: "the value is a number",
                }],
            }),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("missing field `_value`"));
        // The repair contract, not the "when" line: the model already decided to call this tool.
        assert!(error.contains("Normalize the request."));
        assert!(!error.contains("Reach for it when"));
        assert!(error.contains(r#"{"_value":1}"#));
        assert!(error.contains(r#"{"_value":"one"}"#));
        assert!(error.contains("the value is a number"));
    }

    /// One renderer for the system prompt and for `load_tool`; both need the reason legible.
    #[test]
    fn the_contract_renders_examples_with_the_reason_outside_the_code_span() {
        let lines = ToolPromptSpec {
            when: "unused here",
            contract: "Inspect before you query.",
            positive_examples: &[r#"{"operation":"inspect"}"#],
            negative_examples: &[PromptExample {
                args: r#"{"operation":"query"}"#,
                why: "inspect first",
            }],
        }
        .contract_lines();

        assert_eq!(
            lines,
            vec![
                "Inspect before you query.".to_string(),
                "- canonical: `{\"operation\":\"inspect\"}`".to_string(),
                // The reason is outside the backticks, so it cannot read as part of the JSON.
                "- avoid: `{\"operation\":\"query\"}` — inspect first".to_string(),
            ]
        );
    }

    #[test]
    fn required_paths_reject_empty_and_whitespace_only_values() {
        let c = test_ctx(Path::new("/workspace"));
        for value in ["", "   ", "\t"] {
            let err = c.resolve_required_path("path", value).unwrap_err();
            assert!(err.to_string().contains("path must not be empty"));
        }
    }

    /// An escape is **reported**, not rejected here — rejection is policy's judgement.
    #[test]
    fn dotdot_is_normalised_and_escape_is_reported_not_blocked() {
        let c = test_ctx(Path::new("/work"));
        let r = c.resolve_path("../secrets/key.pem");
        assert_eq!(r.path, PathBuf::from("/secrets/key.pem"));
        assert!(!r.inside_workspace);
    }

    /// `/work-other` starts with `/work` as a string but is obviously not inside it.
    /// Comparing by path component is what makes this correct.
    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_inside() {
        let c = test_ctx(Path::new("/work"));
        assert!(!c.resolve_path("/work-other/f.txt").inside_workspace);
    }

    /// A session in a worktree edits files outside the root, and those are still the workspace's.
    /// Without this, entering a worktree would make every single edit look like an out-of-workspace
    /// write — which is precisely the class of call the approval pipeline stops to ask about.
    #[test]
    fn a_session_in_a_worktree_is_inside_its_own_working_directory() {
        let mut c = test_ctx(Path::new("/work"));
        c.exec_cwd = PathBuf::from("/work-worktrees/login");

        let relative = c.resolve_path("src/main.rs");
        assert_eq!(
            relative.path,
            PathBuf::from("/work-worktrees/login/src/main.rs")
        );
        assert!(
            relative.inside_workspace,
            "the checkout the session was moved into"
        );
        // The root it came from still counts — a session in a worktree can read the main tree.
        assert!(c.resolve_path("/work/README.md").inside_workspace);
        // And neither boundary is widened beyond itself.
        assert!(!c.resolve_path("/etc/hosts").inside_workspace);
        assert!(
            !c.resolve_path("/work-worktrees/other/f.rs")
                .inside_workspace
        );
    }

    #[test]
    fn status_distinguishes_the_ways_a_call_can_end() {
        assert!(!ToolExecStatus::Success.is_error());
        for s in [
            ToolExecStatus::Failed,
            ToolExecStatus::Denied,
            ToolExecStatus::Timeout,
            ToolExecStatus::PrecheckFailed,
            ToolExecStatus::Cancelled,
        ] {
            assert!(s.is_error(), "{s:?} must reach the model as an error");
        }
        // But they are distinct, which a bare bool could not express.
        assert_ne!(ToolExecStatus::Denied, ToolExecStatus::Failed);
        assert_ne!(ToolExecStatus::Cancelled, ToolExecStatus::Denied);
    }

    #[test]
    fn small_output_is_not_offloaded() {
        let c = test_ctx(Path::new("/work"));
        let out = c.offload_if_large("short", Recovery::Narrow).unwrap();
        assert_eq!(out.model_text(), "short");
        assert!(out.objects.is_empty());
        assert!(out.display.is_empty(), "nothing to expand");
    }

    /// Large output: the whole thing to the object store, head plus a **longer tail** to the
    /// model — failures are at the end.
    #[test]
    fn large_output_keeps_head_and_a_longer_tail() {
        let c = test_ctx(Path::new("/work"));
        let full: String = (0..1000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let out = c.offload_if_large(&full, Recovery::Narrow).unwrap();

        // Three audiences, three destinations.
        let model = out.model_text();
        assert!(
            model.chars().count() < full.chars().count(),
            "the model gets head and tail"
        );
        assert!(model.starts_with(&full[..10]));
        assert!(model.ends_with(&full[full.len() - 10..]));
        assert!(matches!(
            out.display[0],
            ToolDisplay::Output {
                truncated: true,
                ..
            }
        ));

        let id = out.object_with_role(ObjectRole::Output).unwrap();
        assert_eq!(
            String::from_utf8(c.objects.get(id).unwrap()).unwrap(),
            full,
            "and the store gets all of it"
        );
    }

    /// The model must never be handed an object id.
    /// It has no tool that takes one, so an id in the text is an invitation to invent a use for
    /// it — most often passing it to `read_file` as though it were a path. What it gets instead is
    /// the omitted **line range** and one sentence on how to reach it.
    #[test]
    fn the_omission_note_gives_a_line_range_and_never_an_object_id() {
        let c = test_ctx(Path::new("/work"));
        let full: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let cap = c
            .capture(ObjectRole::Output, None, &full, Recovery::ReadFileRange)
            .unwrap();

        assert!(
            !cap.model_text.contains(&cap.object_id().to_string()),
            "the model has no use for an object id: {}",
            cap.model_text
        );
        assert!(!cap.model_text.contains("sha256:"), "{}", cap.model_text);
        assert!(
            cap.model_text.contains(" of 200 omitted"),
            "{}",
            cap.model_text
        );
        assert!(
            cap.model_text.contains("read_file"),
            "and what to do next: {}",
            cap.model_text
        );
    }

    /// Each tool supplies its own next step, because they are genuinely different actions.
    #[test]
    fn the_recovery_sentence_is_the_callers_choice() {
        let c = test_ctx(Path::new("/work"));
        let full: String = (1..=200).map(|i| format!("line {i}\n")).collect();

        let narrow = c
            .capture(ObjectRole::Output, None, &full, Recovery::Narrow)
            .unwrap();
        assert!(
            narrow.model_text.contains("ask for less"),
            "{}",
            narrow.model_text
        );

        let filter = c
            .capture(ObjectRole::Output, None, &full, Recovery::FilterAtSource)
            .unwrap();
        assert!(
            filter.model_text.contains("pipe through grep"),
            "{}",
            filter.model_text
        );

        // Nothing to suggest says so, rather than suggesting something that will not work.
        let none = c
            .capture(ObjectRole::Output, None, &full, Recovery::Unavailable)
            .unwrap();
        assert!(
            none.model_text.contains("not retrievable"),
            "{}",
            none.model_text
        );
        assert!(
            !none.model_text.contains("read_file"),
            "{}",
            none.model_text
        );
    }

    /// The guarantee that matters: no line is handed over in fragments.
    /// Half a line of code reads as valid but *different* code — a model acting on a truncated
    /// condition is reasoning about something the file does not say.
    #[test]
    fn truncation_never_splits_a_line() {
        let full: String = (1..=200)
            .map(|i| format!("line {i} has some content here\n"))
            .collect();
        let split = split_head_tail(&full, 300, 600);

        // Every line shown, in both pieces, is a line that exists in the source.
        for piece in [&split.head, &split.tail] {
            for line in piece.lines() {
                assert!(
                    full.lines().any(|l| l == line),
                    "{line:?} is not a whole line of the source"
                );
            }
        }
        assert!(
            split.head.ends_with('\n'),
            "the head stops at a line boundary"
        );
        assert!(
            split.head.starts_with("line 1 "),
            "and starts at the beginning"
        );
        assert!(split.tail.ends_with("line 200 has some content here\n"));
    }

    /// The range is exact, not approximate — that is what line alignment buys.
    #[test]
    fn the_omitted_line_range_is_exact() {
        let full: String = (1..=100).map(|i| format!("{i}\n")).collect();
        let split = split_head_tail(&full, 10, 10);

        let (first, last) = split
            .dropped_lines
            .expect("a line-aligned cut names its range");
        let kept_head = split.head.lines().count();
        let kept_tail = split.tail.lines().count();
        assert_eq!(first, kept_head + 1, "the gap starts right after the head");
        assert_eq!(last, 100 - kept_tail, "and ends right before the tail");
        // No line is both kept and reported as dropped.
        assert!(kept_head + kept_tail + (last - first + 1) == 100);
    }

    /// Content with no line structure has no structure to preserve, so a character cut is right —
    /// and the note must not claim a line range it did not use.
    #[test]
    fn a_single_over_long_line_falls_back_to_a_character_cut() {
        let full = "x".repeat(1000);
        let split = split_head_tail(&full, 30, 60);

        assert_eq!(split.head.chars().count(), 30);
        assert_eq!(split.tail.chars().count(), 60);
        assert!(split.dropped_lines.is_none());
        let note = split.note(Recovery::FilterAtSource);
        assert!(note.contains("single line too long"), "{note}");
        assert!(!note.contains("lines 1-"), "no invented range: {note}");
    }

    /// Small enough to keep whole: nothing is dropped and no note is produced.
    #[test]
    fn a_payload_within_budget_is_not_split() {
        let full = "a\nb\nc\n";
        let split = split_head_tail(full, 100, 100);
        assert_eq!(split.head, full);
        assert_eq!(split.tail, "", "the head already took everything");
        assert_eq!(split.omitted_chars, 0);
    }

    /// A one-line note reads better than a range of one.
    #[test]
    fn a_single_dropped_line_is_named_in_the_singular() {
        let full = "aaaa\nbbbb\ncccc\n";
        let split = split_head_tail(full, 5, 5);
        assert_eq!(split.dropped_lines, Some((2, 2)));
        assert!(
            split.note(Recovery::Narrow).contains("line 2 of 3"),
            "{}",
            split.note(Recovery::Narrow)
        );
    }

    /// Multi-byte content must be cut on **character** boundaries, not byte offsets.
    #[test]
    fn truncation_is_char_safe() {
        let c = test_ctx(Path::new("/work"));
        let full: String = "中文内容".repeat(100);
        let out = c.offload_if_large(&full, Recovery::Narrow).unwrap();
        assert!(out.model_text().chars().count() > 0);
    }

    /// One call, three consistent facets. This is the path a tool should use, so it never has to
    /// write the same object id into three lists by hand.
    #[test]
    fn capture_describes_a_payload_for_all_three_audiences_at_once() {
        let c = test_ctx(Path::new("/work"));
        let full: String = (0..500)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();

        let cap = c
            .capture(ObjectRole::Output, None, &full, Recovery::ReadFileRange)
            .unwrap();
        // All three agree about the object — by construction, not by convention.
        assert_eq!(cap.display.object_id(), Some(cap.object_id()));

        let out = cap.into_result();
        assert!(out.dangling_display_objects().is_empty());
        let stored = c.objects.get(&out.objects[0].object_id).unwrap();
        assert_eq!(String::from_utf8(stored).unwrap(), full);
    }

    /// Small payloads are captured whole — `capture` is about coherence, not about truncation.
    #[test]
    fn a_small_capture_is_not_truncated() {
        let c = test_ctx(Path::new("/work"));
        let cap = c
            .capture(
                ObjectRole::Diff,
                Some("src/main.rs"),
                "@@ -1 +1 @@",
                Recovery::ReadFileRange,
            )
            .unwrap();
        assert_eq!(cap.model_text, "@@ -1 +1 @@");
        assert!(matches!(
            cap.display,
            ToolDisplay::Output {
                truncated: false,
                ..
            }
        ));
        assert_eq!(cap.object.ref_key.as_deref(), Some("src/main.rs"));
        assert_eq!(cap.object.role, ObjectRole::Diff);
    }

    /// Several captures compose, and each keeps its own object alive.
    #[test]
    fn captures_compose_without_losing_references() {
        let c = test_ctx(Path::new("/work"));
        let stdout = c
            .capture(
                ObjectRole::Output,
                None,
                "the build log",
                Recovery::FilterAtSource,
            )
            .unwrap();
        let diff = c
            .capture(
                ObjectRole::Diff,
                Some("src/a.rs"),
                "@@ …",
                Recovery::ReadFileRange,
            )
            .unwrap();

        let out = diff.add_to(stdout.into_result());
        assert_eq!(out.objects.len(), 2);
        assert_eq!(out.display.len(), 2);
        assert!(out.dangling_display_objects().is_empty());
    }

    /// The mismatch the coherence check exists to catch: a card pointing at an object that nothing
    /// keeps alive. Garbage collection keeps objects because *entries* reference them; a card is
    /// not a reference, so this would render as a broken link once GC ran.
    #[test]
    fn a_card_without_a_matching_reference_is_detectable() {
        let c = test_ctx(Path::new("/work"));
        let id = c.objects.put(b"orphan").unwrap();

        let bad = ToolExecResult::success("done").with_display(ToolDisplay::Output {
            object_id: id.clone(),
            total_chars: 6,
            truncated: false,
        });
        assert_eq!(bad.dangling_display_objects(), [&id]);

        let good = bad.with_object(ObjectRef::output(id));
        assert!(good.dangling_display_objects().is_empty());
    }

    /// The three audiences are independent: content costs tokens, display does not, and objects
    /// are what persistence keeps alive.
    #[test]
    fn a_result_can_address_all_three_audiences_at_once() {
        let c = test_ctx(Path::new("/work"));
        let id = c.objects.put(b"the full log").unwrap();

        let out = ToolExecResult::success("built 3 targets")
            .with_display(ToolDisplay::Diff {
                path: "src/main.rs".into(),
                stat: display::DiffStat {
                    added: 2,
                    removed: 1,
                },
                change: None,
                object_id: None,
            })
            .with_display(ToolDisplay::Output {
                object_id: id.clone(),
                total_chars: 12,
                truncated: false,
            })
            .with_object(ObjectRef::output(id.clone()))
            .with_object(ObjectRef::keyed(id, ObjectRole::Diff, "src/main.rs"));

        // Only `content` reaches the model.
        assert_eq!(out.model_text(), "built 3 targets");
        assert_eq!(out.display.len(), 2, "the UI gets both cards for free");
        assert_eq!(
            out.objects.len(),
            2,
            "both references keep their object alive"
        );
    }

    /// One API persists every MIME and keeps all three result audiences coherent.
    #[test]
    fn captured_files_are_persisted_and_flatten_to_a_visible_placeholder() {
        let c = test_ctx(Path::new("/work"));
        let captured = c
            .capture_file("chart.PNG\n", " IMAGE/PNG ", b"\x89PNG")
            .unwrap();
        let out = captured.add_to(ToolExecResult::success("here is the chart"));
        assert_eq!(out.content.len(), 2);
        assert!(
            out.model_text()
                .contains("[file: chart.PNG (image/png, 4 bytes)]")
        );
        assert_eq!(
            c.objects.get(&out.objects[0].object_id).unwrap(),
            b"\x89PNG"
        );
        assert!(out.dangling_display_objects().is_empty());
        match &out.content[1] {
            ToolContent::File(file) => {
                assert_eq!(file.name, "chart.PNG");
                assert_eq!(file.mime_type, "image/png");
                assert_eq!(file.object_id, out.objects[0].object_id);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mime_parameters_do_not_change_the_dispatch_essence() {
        assert_eq!(normalize_mime(" Text/Plain; Charset=UTF-8 "), "text/plain");
        assert_eq!(normalize_mime("not a mime"), "application/octet-stream");
    }

    #[tokio::test]
    async fn asking_without_a_ui_fails_closed() {
        let c = test_ctx(Path::new("/work"));
        let err = c
            .ask_form(Form::confirm("Proceed?", "40 files change."))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Unsupported(_)),
            "a headless run must not invent answers"
        );
    }

    /// A tool gets its answer through the port, and the request identifies the asking call so
    /// the UI can show the question in place.
    #[tokio::test]
    async fn a_tool_can_ask_and_the_request_identifies_the_call() {
        struct Capture {
            seen: std::sync::Mutex<Option<InteractionRequest>>,
            answer: InteractionDecision,
        }
        #[async_trait]
        impl InteractionPort for Capture {
            async fn ask(
                &self,
                req: InteractionRequest,
            ) -> std::result::Result<InteractionDecision, String> {
                *self.seen.lock().unwrap() = Some(req);
                Ok(self.answer.clone())
            }
        }

        let port = Arc::new(Capture {
            seen: std::sync::Mutex::new(None),
            answer: InteractionDecision::Submitted(
                FormAnswer::new().set("confirm", zlogic_protocol::FieldValue::Bool(true)),
            ),
        });
        let mut ctx = test_ctx(Path::new("/work"));
        ctx.interaction = Some(port.clone());
        let (sid, tid) = (ctx.session_id, ctx.turn_id);

        let answer = ctx
            .ask_form(Form::confirm("Proceed?", "40 files."))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answer.bool("confirm"), Some(true));

        let req = port.seen.lock().unwrap().clone().unwrap();
        assert_eq!(req.session_id, sid);
        assert_eq!(req.turn_id, tid);
        assert_eq!(req.call_id.unwrap().as_str(), "call_test");
    }

    /// A select answer goes to the **tool**, which reasonably assumes it is one of the options it
    /// offered — so a value that was never offered is removed rather than passed on.
    /// Removed, not rejected: no error dialog, no round trip. The tool sees an unanswered field,
    /// which it has to handle anyway.
    #[tokio::test]
    async fn an_answer_the_form_never_offered_is_dropped() {
        struct Rogue;
        #[async_trait]
        impl InteractionPort for Rogue {
            async fn ask(
                &self,
                _req: InteractionRequest,
            ) -> std::result::Result<InteractionDecision, String> {
                Ok(InteractionDecision::Submitted(FormAnswer::single_choice(
                    "file",
                    "/etc/shadow",
                )))
            }
        }
        let mut ctx = test_ctx(Path::new("/work"));
        ctx.interaction = Some(Arc::new(Rogue));

        let form = Form::single_choice(
            "Which file?",
            "file",
            vec![Choice::plain("src/a.rs"), Choice::plain("src/b.rs")],
        );
        let answer = ctx.ask_form(form).await.unwrap().unwrap();
        assert_eq!(
            answer.text("file"),
            None,
            "the tool must never see a path it did not offer"
        );
    }

    /// Everything that is only a hint passes straight through — the model reads what the user
    /// actually wrote.
    #[tokio::test]
    async fn hints_do_not_filter_the_answer() {
        struct Verbose;
        #[async_trait]
        impl InteractionPort for Verbose {
            async fn ask(
                &self,
                _req: InteractionRequest,
            ) -> std::result::Result<InteractionDecision, String> {
                Ok(InteractionDecision::Submitted(
                    FormAnswer::new().set("notes", FieldValue::Text("x".repeat(500))),
                ))
            }
        }
        let mut ctx = test_ctx(Path::new("/work"));
        ctx.interaction = Some(Arc::new(Verbose));

        let form = Form::new(
            "Notes",
            vec![FormField::new(
                "notes",
                "Notes",
                Control::Textarea {
                    default: None,
                    placeholder: None,
                    rows: None,
                    max_len: Some(10),
                },
            )],
        );
        let answer = ctx.ask_form(form).await.unwrap().unwrap();
        assert_eq!(
            answer.text("notes").unwrap().len(),
            500,
            "max_len is a hint, not a gate"
        );
    }

    /// Cancelling is not an error — the tool decides what to do about it.
    #[tokio::test]
    async fn cancelling_yields_no_answer_rather_than_an_error() {
        struct Cancels;
        #[async_trait]
        impl InteractionPort for Cancels {
            async fn ask(
                &self,
                _req: InteractionRequest,
            ) -> std::result::Result<InteractionDecision, String> {
                Ok(InteractionDecision::Cancelled)
            }
        }
        let mut ctx = test_ctx(Path::new("/work"));
        ctx.interaction = Some(Arc::new(Cancels));
        assert!(
            ctx.ask_form(Form::confirm("Proceed?", "…"))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A transport failure is a tool failure, not an implicit "no".
    #[tokio::test]
    async fn a_broken_port_is_an_error_not_an_implicit_deny() {
        struct Broken;
        #[async_trait]
        impl InteractionPort for Broken {
            async fn ask(
                &self,
                _req: InteractionRequest,
            ) -> std::result::Result<InteractionDecision, String> {
                Err("client disconnected".into())
            }
        }
        let mut ctx = test_ctx(Path::new("/work"));
        ctx.interaction = Some(Arc::new(Broken));
        match ctx.ask_form(Form::confirm("Proceed?", "…")).await {
            Err(ToolError::Failed(m)) => assert!(m.contains("disconnected")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// A long-running tool must be able to show progress before it finishes.
    #[test]
    fn output_deltas_reach_the_sink_tagged_by_stream() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<(OutputStream, String)>>);
        impl OutputSink for Recorder {
            fn emit(&self, stream: OutputStream, chunk: &str) {
                self.0.lock().unwrap().push((stream, chunk.to_string()));
            }
        }

        let sink = Arc::new(Recorder::default());
        let mut ctx = test_ctx(Path::new("/work"));
        ctx.output = Some(sink.clone());

        ctx.stdout("compiling\n");
        ctx.stderr("warning: unused\n");
        ctx.progress("3/12");
        // Empty chunks are dropped rather than producing empty frames.
        ctx.stdout("");

        let seen = sink.0.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0], (OutputStream::Stdout, "compiling\n".to_string()));
        assert_eq!(seen[1].0, OutputStream::Stderr);
        assert_eq!(seen[2].0, OutputStream::Progress);
    }

    /// A batch run has nowhere to send output; that must be silent, not an error.
    #[test]
    fn output_without_a_sink_is_discarded_not_an_error() {
        let ctx = test_ctx(Path::new("/work"));
        ctx.stdout("goes nowhere");
        ctx.progress("also nowhere");
    }

    #[test]
    fn empty_args_parse_as_an_empty_object() {
        #[derive(serde::Deserialize)]
        struct Empty {}
        assert!(parse_args::<Empty>("").is_ok());
        assert!(parse_args::<Empty>("   ").is_ok());
    }

    #[test]
    fn malformed_or_missing_args_are_bad_args_not_a_panic() {
        #[derive(serde::Deserialize)]
        struct A {
            #[allow(dead_code)]
            x: i32,
        }
        assert!(matches!(
            parse_args::<A>("{not json"),
            Err(ToolError::BadArgs(_))
        ));
        assert!(matches!(parse_args::<A>("{}"), Err(ToolError::BadArgs(_))));
    }
}

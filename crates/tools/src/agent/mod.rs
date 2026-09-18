//! Agent tools.
//! # This module is a dependency boundary
//! "Run an agent" is core's capability. If `create_agent` called into core from here, the
//! result would be a `tools → core → tools` cycle: core registers tools, and a tool runs core.
//! So this module holds only the [`AgentSpawner`] trait. The implementation lives in core and
//! is injected into [`crate::ToolCtx`] when it is assembled. The tool itself knows nothing
//! about how an agent actually starts.
//! With no implementation injected it **fails closed** — a clear "not available in this run"
//! rather than pretending to succeed or quietly returning nothing.

pub mod create_agent;

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use zlogic_protocol::{CallId, SessionId, TurnId};

pub use create_agent::CreateAgent;

use crate::Tool;

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CreateAgent)]
}

/// A request to run a sub-agent.
/// No `PartialEq`: it carries a cancellation token, which has no meaningful equality. Tests compare
/// the fields they care about.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// The sub-agent's name. It is the *identity*: the child session name, the usage attribution
    /// (`Purpose::Agent(name)`) and the UI card all carry it. Whether the name also selects a
    /// pre-registered profile (model / tools / system prompt) is a resolution detail in core —
    /// an unregistered name with explicit [`Self::system`] / [`Self::tools`] / [`Self::model`]
    /// runs on the caller's customisation over the parent's defaults instead.
    pub agent: String,
    pub task: String,
    /// Who asked, so core can establish the parent/child relationship.
    pub parent_session_id: SessionId,
    pub parent_turn_id: TurnId,
    /// Optional execution-directory override. Interactive sub-agents inherit the parent session;
    /// scheduled jobs use their immutable executor snapshot.
    pub exec_cwd: Option<String>,
    /// Present for background agents. The final mailbox checkpoint closes this gate atomically
    /// with observing an empty inbox, so a sender can never enqueue after the last drain.
    pub mailbox: Option<Arc<AgentMailboxGate>>,
    /// Which tool call it hangs under, so the UI can drill into it from the parent timeline.
    pub anchor_call_id: CallId,
    /// Unattended agents have no interaction port. A policy decision that needs a person therefore
    /// fails closed instead of leaving a background run waiting on a vanished turn.
    pub unattended: bool,
    /// The conversation's token — **the same one**, not a child. Cancelling stops the parent and
    /// every sub-agent under it at once; otherwise a cancelled turn would sit waiting on a
    /// sub-agent that has no idea anybody stopped caring.
    pub cancel: CancellationToken,
    /// Per-call system-prompt override supplied by `create_agent`. Appended after the profile's
    /// (or the parent's) system prompt, so it refines rather than replaces the environment and
    /// capability guidance the child needs to act. `None` = use the profile / parent default.
    pub system: Option<String>,
    /// Per-call tool allowlist supplied by `create_agent`. `Some` replaces the profile / parent
    /// allowlist entirely — it is the capability boundary, and mixing would make "what can this
    /// agent do" unanswerable. Applied at materialisation, so an excluded tool is invisible to the
    /// child rather than refused when called.
    pub tools: Option<Vec<String>>,
    /// Per-call model override supplied by `create_agent`. `None` = the profile's model, or the
    /// parent's model for a custom agent. Resolution happens in the engine (which owns routing);
    /// a name that cannot be resolved fails the spawn with the tried list, like any role.
    pub model: Option<String>,
}

impl AgentRequest {
    /// Whether the caller supplied any per-call customisation. This is what lets an unregistered
    /// agent name run at all — with no profile and no customisation the request is an error, not
    /// a silent fallback.
    pub fn is_customised(&self) -> bool {
        self.system.is_some() || self.tools.is_some() || self.model.is_some()
    }
}

#[derive(Debug, Clone, Copy)]
enum AgentMailboxState {
    Pending,
    Open(SessionId),
    /// Terminal. Retains the last activated session id (if any) so a failed run's persisted
    /// transcript stays discoverable after the gate closes.
    Closed(Option<SessionId>),
}

#[derive(Debug)]
pub struct AgentMailboxGate {
    state: tokio::sync::Mutex<AgentMailboxState>,
    changed: tokio::sync::Notify,
}

impl Default for AgentMailboxGate {
    fn default() -> Self {
        Self {
            state: tokio::sync::Mutex::new(AgentMailboxState::Pending),
            changed: tokio::sync::Notify::new(),
        }
    }
}

impl AgentMailboxGate {
    pub async fn activate(&self, session_id: SessionId) {
        let mut state = self.state.lock().await;
        if matches!(*state, AgentMailboxState::Pending) {
            *state = AgentMailboxState::Open(session_id);
            self.changed.notify_waiters();
        }
    }

    /// Waits for a starting agent to become addressable, then runs while the gate is locked.
    /// Returning `true` closes it before another sender can enter.
    pub async fn with_open_session<R, F, Fut>(&self, operation: F) -> Option<R>
    where
        F: FnOnce(SessionId) -> Fut,
        Fut: Future<Output = (R, bool)>,
    {
        let mut operation = Some(operation);
        loop {
            // Construct the waiter before inspecting state so activation cannot be lost between
            // the check and `.await`.
            let changed = self.changed.notified();
            let mut state = self.state.lock().await;
            match *state {
                AgentMailboxState::Pending => {
                    drop(state);
                    changed.await;
                }
                AgentMailboxState::Open(session_id) => {
                    let operation = operation.take().expect("mailbox operation runs once");
                    let (result, close) = operation(session_id).await;
                    if close {
                        *state = AgentMailboxState::Closed(Some(session_id));
                        self.changed.notify_waiters();
                    }
                    return Some(result);
                }
                AgentMailboxState::Closed(_) => return None,
            }
        }
    }

    pub async fn close(&self) {
        let mut state = self.state.lock().await;
        match *state {
            AgentMailboxState::Open(session_id) => {
                *state = AgentMailboxState::Closed(Some(session_id));
                self.changed.notify_waiters();
            }
            AgentMailboxState::Pending => {
                *state = AgentMailboxState::Closed(None);
                self.changed.notify_waiters();
            }
            // Already closed: keep the retained session id as-is.
            AgentMailboxState::Closed(_) => {}
        }
    }

    /// Reads the child session id without opening or closing the gate.
    /// `None` while the agent is still starting (Pending), or if it finished without ever
    /// creating a child session. After close the id is retained, so a finished run — successful or
    /// failed — still links to its persisted transcript.
    pub async fn session_id(&self) -> Option<SessionId> {
        match *self.state.lock().await {
            AgentMailboxState::Open(session_id) => Some(session_id),
            AgentMailboxState::Closed(session_id) => session_id,
            AgentMailboxState::Pending => None,
        }
    }
}

/// What a finished sub-agent produced.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// The child session the sub-agent ran in.
    /// Returned so the tool result can carry it, which is what lets the UI drill from the parent's
    /// tool card into the sub-agent's own transcript. Without it the child's history is persisted
    /// but unreachable.
    pub session_id: SessionId,
    /// The conclusion. Everything else stays in the child session's entries.
    pub answer: String,
}

/// Core's "run a sub-agent" capability.
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    /// Runs to completion and returns the sub-agent's conclusion.
    /// # The implementation owns the child session
    /// Before running anything it must create a child session — parent linkage, inherited root and
    /// agent path — and persist the sub-agent's entries there. Two reasons this is the spawner's
    /// job and not the tool's:
    /// - The transcript has to survive: a sub-agent that did fifteen tool calls and got it wrong is
    ///   exactly what someone will want to read afterwards.
    /// - Usage has to be attributable. Rolled into the parent session it would both distort the
    ///   parent's compaction signal and make "what did that sub-agent cost" unanswerable.
    /// Only the conclusion comes back here.
    async fn spawn(&self, req: AgentRequest) -> Result<AgentOutcome, String>;

    /// The available sub-agent profile names.
    /// Used to check the `agent` argument, so a name the model invented is answered with the real
    /// list instead of a confusing failure deeper in. The check is advisory only: core still
    /// resolves the request (a custom agent with explicit `system` / `tools` / `model` is allowed
    /// to use a name outside this list), but the model is steered towards the list before it goes
    /// that route.
    fn available(&self) -> Vec<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mailbox_sender_waits_until_the_child_session_is_ready() {
        let gate = Arc::new(AgentMailboxGate::default());
        let sender_gate = gate.clone();
        let sender = tokio::spawn(async move {
            sender_gate
                .with_open_session(|session_id| async move { (session_id, false) })
                .await
        });

        tokio::task::yield_now().await;
        assert!(!sender.is_finished());

        let session_id = SessionId::new();
        gate.activate(session_id).await;
        assert_eq!(sender.await.unwrap(), Some(session_id));

        gate.close().await;
        assert_eq!(
            gate.with_open_session(|open| async move { (open, false) })
                .await,
            None
        );
    }

    #[tokio::test]
    async fn closing_a_pending_mailbox_releases_waiting_senders() {
        let gate = Arc::new(AgentMailboxGate::default());
        let sender_gate = gate.clone();
        let sender = tokio::spawn(async move {
            sender_gate
                .with_open_session(|session_id| async move { (session_id, false) })
                .await
        });

        tokio::task::yield_now().await;
        gate.close().await;
        assert_eq!(sender.await.unwrap(), None);
    }
}

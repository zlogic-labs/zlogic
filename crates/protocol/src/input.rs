use serde::{Deserialize, Serialize};

use crate::error::LocalizedMessage;
use crate::llm::ThinkingIntent;

/// A durable notification injected when a background task reaches a terminal state.
/// This is deliberately not a `Text` part: replay and transcript rendering must not make a
/// machine-generated task result look like something the user typed.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUpdatePart {
    pub task_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Present for agent tasks so clients can open the child transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<crate::SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_title: Option<String>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillLoadSource {
    User,
    Model,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Submission {
    pub submission_id: String,
    pub session_id: String,
    pub client_request_id: String,
    pub parts: Vec<MessagePart>,
    pub delivery: Delivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingIntent>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text {
        text: String,
    },
    /// A user-selected skill. The engine resolves this reference immediately before the turn
    /// starts, so a renamed or removed skill fails visibly instead of becoming slash-prefixed
    /// prose.
    Skill {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    /// A durable skill definition loaded by either a user slash invocation or the model's `skill`
    /// tool. UI/API clients may not submit this variant directly.
    SkillLoad {
        name: String,
        revision: String,
        /// Content-addressed `SKILL.md` body. Keeping the body outside the entry payload lets
        /// context selection discard superseded revisions without reading them first.
        body_object: String,
        path: String,
        loaded_by: SkillLoadSource,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unsupported: Vec<String>,
    },
    /// One request to apply a skill. Kept separate from [`MessagePart::SkillLoad`] because
    /// invocations are chronological user intent while the definition is replaceable context
    /// state: only the newest load for a qualified name reaches the model.
    SkillInvocation {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    /// Explicitly removes a skill from active session state.
    SkillUnload {
        name: String,
    },
    File {
        path: String,
    },
    /// A remote attachment whose bytes already live in the content-addressed object store.
    /// Remote clients upload first and then submit this reference. The engine validates the id
    /// and materializes it as an application-managed local [`MessagePart::File`] before it enters
    /// the mailbox. Models and file tools therefore receive one path-based attachment contract.
    Attachment {
        object_id: String,
        name: String,
        mime_type: String,
        bytes: u64,
    },
    /// Engine-produced background-task completion. UI submissions must not manufacture this part.
    TaskUpdate {
        update: TaskUpdatePart,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Steer,
    Queue,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubmitAck {
    Accepted {
        submission_id: String,
        /// This Engine process acquired the session lock and started the turn. `None` means the
        /// submission remains queued or belongs to a turn owned elsewhere.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notice: Option<LocalizedMessage>,
    },
    Duplicate {
        submission_id: String,
    },
    UnsupportedInput {
        missing_capabilities: Vec<String>,
        suggested_models: Vec<String>,
    },
    Rejected {
        reason: LocalizedMessage,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    CancelTurn {
        turn_id: String,
    },
    /// Proactively summarise the oldest eligible context without sending a user message or
    /// requesting a normal assistant reply. The session must be idle when the command is handled.
    CompactContext {
        session_id: String,
    },
    AnswerInteraction {
        interaction_id: String,
        decision: crate::interaction::InteractionDecision,
    },
    RetargetSubmission {
        submission_id: String,
        delivery: Delivery,
    },
    CancelSubmission {
        submission_id: String,
    },
    Rewind {
        session_id: String,
        keep_through_turn: u32,
    },
    Fork {
        session_id: String,
        keep_through_turn: u32,
    },
}

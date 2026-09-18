//! Mid-run user input: the mailbox drain.
//! A message submitted while the agent is running does **not** abort it. It waits in the mailbox and
//! is injected at a safe checkpoint, and there are exactly two of those:
//! 1. **After a round's tool results are all in the history.** The turn was going to continue
//!    anyway, so the message simply joins the next request.
//! 2. **At the would-be end of the turn.** A pending message continues the *same* turn with another
//!    round rather than letting it finish — which is what makes "wait, also do X" feel like part of
//!    the same exchange instead of a new one.
//! # The invariant that dictates those two points
//! A `role: user` message must never land between an assistant `tool_calls` message and its
//! `role: tool` results. Several OpenAI-compatible providers reject that shape outright. Draining
//! per tool call, or the moment a submission arrives, would produce it. Draining only after the
//! whole batch is persisted cannot.
//! # Delivery is a move, and it is evaluated now rather than at submit time
//! An undelivered submission lives in the mailbox; a delivered one lives in `session_entry`. There is
//! no `delivered` flag, because a flag admits two broken states — written but not flagged (delivered
//! twice) and flagged but not written (lost). `MailboxStore::deliver` moves the row and appends the
//! entry in one transaction.
//! Only [`Delivery::Steer`] is drained here. A `Queue` row is left alone: it asked to start its own
//! turn, and that is the engine's business once this turn ends.

use std::collections::HashMap;

use zlogic_protocol::stream::{SteeringContent, StreamPayload, Via};
use zlogic_protocol::{MessagePart, SkillLoadSource};
use zlogic_protocol::{SubmissionId, TurnId};
use zlogic_store::Delivery;

use crate::{Core, CoreError, Result, TurnEmitter, TurnInput, context, entry_data};

pub(crate) type SkillLoadStamp = (String, String);

#[derive(Debug, Clone, PartialEq)]
pub enum DecodedPart {
    User(MessagePart),
    TaskUpdate(zlogic_protocol::TaskUpdatePart),
}

/// What one drain delivered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drained {
    pub submissions: Vec<SubmissionId>,
}

impl Drained {
    pub fn is_empty(&self) -> bool {
        self.submissions.is_empty()
    }
}

impl Core {
    /// Resolves skill references in a new turn before any input is persisted.
    pub(crate) async fn resolve_turn_skills(
        &self,
        input: Vec<TurnInput>,
    ) -> Result<Vec<TurnInput>> {
        let mut resolver = SkillResolver::new(self)?;
        let mut resolved = Vec::with_capacity(input.len());
        for item in input {
            match item {
                TurnInput::Submitted(part) => {
                    resolved.extend(
                        resolver
                            .resolve(part)
                            .await?
                            .into_iter()
                            .map(TurnInput::Submitted),
                    );
                }
                other => resolved.push(other),
            }
        }
        Ok(resolved)
    }

    /// Delivers every pending `Steer` submission into the running turn.
    /// Call only at the two checkpoints in the module docs.
    pub(crate) async fn drain_steering(
        &self,
        emitter: &TurnEmitter,
        turn_id: TurnId,
        turn_seq: i64,
    ) -> Result<Drained> {
        let session = self.session_id();
        let pending = self
            .services()
            .store
            .with(|db| db.mailbox().pending(session))?;
        let pending: Vec<_> = pending
            .into_iter()
            .filter(|m| m.delivery == Delivery::Steer)
            .collect();
        if pending.is_empty() {
            return Ok(Drained::default());
        }

        let mut drained = Drained::default();
        for record in pending {
            let parts = self
                .resolve_steering_skills(decode_parts(&record.parts))
                .await?;
            if parts.is_empty() {
                // Nothing to say. Take it out of the mailbox anyway, or it is retried on every
                // checkpoint for the rest of the turn.
                self.services()
                    .store
                    .with(|db| db.mailbox().cancel(record.submission_id))?;
                continue;
            }

            let content = steering_content(&parts);
            let entry_id = self.deliver(record.submission_id, turn_id, turn_seq, parts)?;

            // The delivery receipt. The UI holds submitted messages in a pending mirror and shifts
            // them off on this event, so nothing can look lost or appear twice. The content rides
            // along typed (`steering_content`): user messages are already on the client, but
            // machine injections such as task notifications are not — without them the live view
            // can only render a placeholder.
            emitter.send(StreamPayload::SteeringInjected {
                entry_id: entry_id.to_string(),
                submission_id: record.submission_id.to_string(),
                content,
            });
            drained.submissions.push(record.submission_id);
        }

        if !drained.is_empty() {
            emitter.send(StreamPayload::MailboxConsumed {
                submission_ids: drained
                    .submissions
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                via: Via::Steer,
            });
        }
        Ok(drained)
    }

    /// Final checkpoint for an addressable background agent. Empty closes the send gate while it
    /// is still locked; a sender therefore either lands before this drain or gets a terminal error.
    pub(crate) async fn drain_steering_or_close(
        &self,
        emitter: &TurnEmitter,
        turn_id: TurnId,
        turn_seq: i64,
    ) -> Result<Drained> {
        let Some(gate) = &self.agent_mailbox else {
            return self.drain_steering(emitter, turn_id, turn_seq).await;
        };
        gate.with_open_session(|_| async {
            let result = self.drain_steering(emitter, turn_id, turn_seq).await;
            let close = result.as_ref().is_ok_and(Drained::is_empty);
            (result, close)
        })
        .await
        .unwrap_or_else(|| Ok(Drained::default()))
    }

    async fn resolve_steering_skills(&self, parts: Vec<DecodedPart>) -> Result<Vec<DecodedPart>> {
        let mut resolver = SkillResolver::new(self)?;
        let mut resolved = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                DecodedPart::User(part) => {
                    resolved.extend(
                        resolver
                            .resolve(part)
                            .await?
                            .into_iter()
                            .map(DecodedPart::User),
                    );
                }
                other => resolved.push(other),
            }
        }
        Ok(resolved)
    }

    /// Moves one submission into the history. Returns the id of the entry that anchors it.
    /// A submission may hold several parts (text plus attachments) while `deliver` takes one entry,
    /// so the extra parts are appended inside the **same transaction**: either the whole message
    /// arrives or the row stays in the mailbox. Splitting it across two transactions would allow
    /// half a message in the history with nothing left to redeliver.
    fn deliver(
        &self,
        submission_id: SubmissionId,
        turn_id: TurnId,
        turn_seq: i64,
        parts: Vec<DecodedPart>,
    ) -> Result<zlogic_protocol::EntryId> {
        let session = self.session_id();
        let mut entries = Vec::with_capacity(parts.len());
        for part in parts {
            entries.push(match part {
                DecodedPart::User(MessagePart::SkillLoad {
                    name,
                    revision,
                    body_object,
                    path,
                    loaded_by,
                    unsupported,
                }) => entry_data::skill_load_entry(
                    session,
                    turn_id,
                    turn_seq,
                    name,
                    revision,
                    body_object,
                    path,
                    loaded_by,
                    unsupported,
                )?,
                DecodedPart::User(part) => entry_data::input_entry(
                    session,
                    turn_id,
                    turn_seq,
                    &entry_data::Author::Steering,
                    part,
                )?,
                DecodedPart::TaskUpdate(update) => {
                    entry_data::task_update_entry(session, turn_id, turn_seq, &update)?
                }
            });
        }

        let mut it = entries.into_iter();
        let first = it.next().expect("the caller checked for parts");
        let rest: Vec<_> = it.collect();

        let anchor = self
            .services()
            .store
            .with(|db| db.mailbox().deliver_all(submission_id, first, rest))?;
        Ok(anchor.entry_id)
    }
}

fn steering_content(parts: &[DecodedPart]) -> Option<SteeringContent> {
    let mut task_update: Option<SteeringContent> = None;
    let mut user_text: Vec<String> = Vec::new();
    for part in parts {
        match part {
            DecodedPart::TaskUpdate(update) => {
                task_update = Some(SteeringContent::TaskUpdate {
                    task_id: update.task_id.clone(),
                    state: update.state.clone(),
                    summary: update.summary.clone(),
                    child_session_id: update.child_session_id,
                    command: update.command.clone(),
                    cwd: update.cwd.clone(),
                    preview: update.preview.clone(),
                    agent: update.agent.clone(),
                    source: update.source.clone(),
                    job_title: update.job_title.clone(),
                });
            }
            DecodedPart::User(MessagePart::Text { text }) => user_text.push(text.clone()),
            DecodedPart::User(MessagePart::File { path }) => user_text.push(format!("@{path}")),
            DecodedPart::User(MessagePart::Skill { name, args }) => {
                user_text.push(format!(
                    "/{name}{}",
                    args.as_deref()
                        .map(|value| format!(" {value}"))
                        .unwrap_or_default()
                ));
            }
            DecodedPart::User(MessagePart::SkillUnload { name }) => {
                user_text.push(format!("/unload {name}"));
            }
            _ => {}
        }
    }
    if let Some(update) = task_update {
        return Some(update);
    }
    if !user_text.is_empty() {
        return Some(SteeringContent::User {
            text: user_text.join(" "),
        });
    }
    None
}

/// Decodes mailbox input without erasing whether a part came from the user or the task runtime.
pub fn decode_parts(parts: &serde_json::Value) -> Vec<DecodedPart> {
    let Ok(parts) =
        serde_json::from_value::<Vec<zlogic_protocol::input::MessagePart>>(parts.clone())
    else {
        tracing::warn!(target: "zlogic::core", "unreadable submission parts; skipped");
        return Vec::new();
    };

    parts
        .into_iter()
        .filter_map(|p| match p {
            zlogic_protocol::input::MessagePart::Text { text } if text.trim().is_empty() => None,
            part @ zlogic_protocol::input::MessagePart::Text { .. }
            | part @ zlogic_protocol::input::MessagePart::Skill { .. }
            | part @ zlogic_protocol::input::MessagePart::SkillUnload { .. }
            | part @ zlogic_protocol::input::MessagePart::File { .. } => {
                Some(DecodedPart::User(part))
            }
            zlogic_protocol::input::MessagePart::SkillLoad { .. }
            | zlogic_protocol::input::MessagePart::SkillInvocation { .. } => {
                tracing::warn!(
                    target: "zlogic::core",
                    "client-supplied internal skill state reached the mailbox; skipped"
                );
                None
            }
            zlogic_protocol::input::MessagePart::Attachment { .. } => {
                tracing::warn!(
                    target: "zlogic::core",
                    "unmaterialized attachment reached the mailbox; skipped"
                );
                None
            }
            zlogic_protocol::input::MessagePart::TaskUpdate { update } => {
                Some(DecodedPart::TaskUpdate(update))
            }
        })
        .collect()
}

struct SkillResolver<'a> {
    core: &'a Core,
    loaded: HashMap<String, SkillLoadStamp>,
}

impl<'a> SkillResolver<'a> {
    fn new(core: &'a Core) -> Result<Self> {
        Ok(Self {
            core,
            loaded: latest_skill_load_stamps(core)?,
        })
    }

    async fn resolve(&mut self, part: MessagePart) -> Result<Vec<MessagePart>> {
        if let MessagePart::SkillUnload { name } = &part {
            let name = name.trim().trim_start_matches('/').to_string();
            if name.is_empty() {
                return Err(CoreError::Invalid("skill name is required".into()));
            }
            self.loaded.remove(&name);
            return Ok(vec![MessagePart::SkillUnload { name }]);
        }
        let MessagePart::Skill { name, args } = part else {
            if matches!(
                part,
                MessagePart::SkillLoad { .. } | MessagePart::SkillInvocation { .. }
            ) {
                return Err(CoreError::Invalid(
                    "skill load state may only be produced by the engine".into(),
                ));
            }
            return Ok(vec![part]);
        };
        let name = name.trim().trim_start_matches('/').to_string();
        if name.is_empty() {
            return Err(CoreError::Invalid("skill name is required".into()));
        }
        let args = args
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let host = self
            .core
            .skills()
            .ok_or_else(|| CoreError::Invalid("skills are not available in this run".into()))?;
        let loaded = host.load(&name).await.map_err(CoreError::Invalid)?;
        let state = (loaded.revision.clone(), loaded.path.clone());
        let changed = self.loaded.get(&loaded.name) != Some(&state);
        let mut parts = Vec::with_capacity(if changed { 2 } else { 1 });
        if changed {
            self.loaded.insert(loaded.name.clone(), state);
            let body_object = self
                .core
                .services()
                .objects
                .put(loaded.raw_body.as_bytes())
                .map_err(CoreError::from)?;
            parts.push(MessagePart::SkillLoad {
                name: loaded.name.clone(),
                revision: loaded.revision,
                body_object: body_object.to_string(),
                path: loaded.path,
                loaded_by: SkillLoadSource::User,
                unsupported: loaded.unsupported,
            });
        }
        parts.push(MessagePart::SkillInvocation {
            name: loaded.name,
            args,
        });
        Ok(parts)
    }
}

/// The newest active revision for every qualified name.
/// Skill state is durable session state: compaction does not erase it, while an explicit unload
/// removes the corresponding stamp so the next invocation loads the definition again.
pub(crate) fn latest_skill_load_stamps(core: &Core) -> Result<HashMap<String, SkillLoadStamp>> {
    let session = core.session_id();
    let objects = core.services().objects.clone();
    core.services().store.with(|db| {
        let store = db.entries();
        let entries = store.list_skill_state(session)?;
        let load = context::store_loader(&store, objects.as_ref());
        let mut loaded = HashMap::new();
        for entry in &entries {
            let data = load(entry)?;
            let part = serde_json::from_value::<MessagePart>(data).map_err(|error| {
                CoreError::Corrupt(format!(
                    "skill state entry {} has unreadable data: {error}",
                    entry.entry_id
                ))
            })?;
            match part {
                MessagePart::SkillLoad {
                    name,
                    revision,
                    path,
                    ..
                } => {
                    loaded.insert(name, (revision, path));
                }
                MessagePart::SkillUnload { name } => {
                    loaded.remove(&name);
                }
                _ => {
                    return Err(CoreError::Corrupt(format!(
                        "skill state entry {} does not contain skill state data",
                        entry.entry_id
                    )));
                }
            }
        }
        Ok(loaded)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_parts_become_content_in_order() {
        let parts = json!([
            { "type": "text", "text": "first" },
            { "type": "text", "text": "second" }
        ]);
        let decoded = decode_parts(&parts);
        assert_eq!(decoded.len(), 2);
        match (&decoded[0], &decoded[1]) {
            (
                DecodedPart::User(MessagePart::Text { text: a }),
                DecodedPart::User(MessagePart::Text { text: b }),
            ) => {
                assert_eq!(a, "first");
                assert_eq!(b, "second");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Whitespace-only input is not a message. It would otherwise reach the model as an empty user
    /// turn, which some providers reject.
    #[test]
    fn blank_text_is_dropped() {
        assert!(decode_parts(&json!([{ "type": "text", "text": "   \n" }])).is_empty());
    }

    /// An attachment remains structured until the model-context boundary.
    #[test]
    fn a_file_part_remains_structured() {
        let decoded = decode_parts(&json!([{ "type": "file", "path": "/tmp/a.csv" }]));
        match &decoded[0] {
            DecodedPart::User(MessagePart::File { path }) => assert_eq!(path, "/tmp/a.csv"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_skill_unload_remains_a_user_state_transition() {
        let decoded = decode_parts(&json!([{
            "type": "skill_unload",
            "name": "acme:review"
        }]));
        assert!(matches!(
            decoded.as_slice(),
            [DecodedPart::User(MessagePart::SkillUnload { name })] if name == "acme:review"
        ));
    }

    #[test]
    fn an_unreadable_payload_yields_nothing_rather_than_an_error() {
        assert!(decode_parts(&json!({ "not": "an array" })).is_empty());
        assert!(decode_parts(&json!([{ "type": "from_the_future" }])).is_empty());
    }

    #[test]
    fn task_update_keeps_its_machine_generated_origin() {
        let decoded = decode_parts(&json!([{
            "type": "task_update",
            "update": {
                "task_id": "task-1",
                "state": "succeeded",
                "summary": "review complete"
            }
        }]));
        assert!(matches!(
            decoded.as_slice(),
            [DecodedPart::TaskUpdate(zlogic_protocol::TaskUpdatePart {
                task_id,
                state,
                summary: Some(summary),
                ..
            })] if task_id == "task-1" && state == "succeeded" && summary == "review complete"
        ));
    }

    #[test]
    fn steering_content_distinguishes_task_updates_from_user_text() {
        let task = decode_parts(&json!([{
            "type": "task_update",
            "update": {
                "task_id": "task-7",
                "state": "failed",
                "summary": "build broke",
                "command": "npm run build",
                "source": "tool",
                "job_title": "nightly build"
            }
        }]));
        assert_eq!(
            steering_content(&task),
            Some(SteeringContent::TaskUpdate {
                task_id: "task-7".into(),
                state: "failed".into(),
                summary: Some("build broke".into()),
                child_session_id: None,
                command: Some("npm run build".into()),
                cwd: None,
                preview: None,
                agent: None,
                source: Some("tool".into()),
                job_title: Some("nightly build".into()),
            })
        );

        let user = decode_parts(&json!([
            { "type": "text", "text": "do not merge yet" },
            { "type": "file", "path": "src/a.rs" }
        ]));
        assert_eq!(
            steering_content(&user),
            Some(SteeringContent::User {
                text: "do not merge yet @src/a.rs".into()
            })
        );
    }
}

//! The contract between what the round loop writes and what `build_context` reads.
//! Model-produced entries hold canonical [`ContentPart`] values. User entries hold the submitted
//! [`MessagePart`] unchanged after the engine has materialized uploaded objects as local
//! `File { path }` parts. [`from_entry`] is the model-context boundary that projects a stored file
//! reference into text.
//! # Why `raw` lives in its own column rather than inside `data`
//! `ContentPart::Reasoning` has a `raw` field, so it could all go in one place. It is split
//! because the column makes "which entries carry provider state" a **SQL question**: a future
//! maintenance pass that has to strip native payloads for one model can find them with a query
//! instead of parsing every row. Re-attaching on read is a few lines, in one place ([`from_entry`]).

use serde_json::Value;
use zlogic_protocol::message::{ContentPart, ReasoningPart, Source, TextPart};
use zlogic_protocol::{MessagePart, SkillLoadSource, TaskUpdatePart};
use zlogic_store::{EntryKind, EntryRecord, NewEntry};

use crate::{CoreError, Result};

/// Who produced a part.
/// Not inferable from the part itself: `Text` is the one shape both sides produce, and a user's
/// message and an assistant's reply are byte-for-byte identical payloads under different kinds.
/// Guessing gave the user's own input `assistant_text`, which then failed `build_context`'s
/// "an assistant entry must have a round" check — a confusing way to learn about a writer bug.
/// The source stamp rides on [`Author::Model`] because those are exactly the entries that have one:
/// a tool result is not a model's output, and the raw gate has nothing to compare for it.
#[derive(Debug, Clone, PartialEq)]
pub enum Author {
    User,
    /// The user again, but injected into a running turn.
    /// Identical to [`Author::User`] on the wire — `build_context` merges the two — and distinct in
    /// the timeline, which is where "this arrived mid-run" is worth seeing.
    Steering,
    /// One part of a model response, stamped with the model that produced it.
    Model(Source),
    /// A tool result.
    Tool,
}

impl Author {
    fn source(&self) -> Option<Source> {
        match self {
            Author::Model(s) => Some(s.clone()),
            Author::User | Author::Steering | Author::Tool => None,
        }
    }
}

/// The entry kind a part is stored under.
/// An impossible pairing is an error rather than a best guess: the combinations are fixed constants
/// at every call site, so this can only fire on a genuine mistake — and a silently mis-kinded entry
/// corrupts the history in a way that only shows up rounds later.
pub fn kind_of(part: &ContentPart, author: &Author) -> Result<EntryKind> {
    Ok(match (author, part) {
        // Images arrive here once attachment resolution exists.
        (Author::User, ContentPart::Text(_) | ContentPart::Image(_)) => EntryKind::User,
        (Author::Steering, ContentPart::Text(_) | ContentPart::Image(_)) => EntryKind::Steering,
        (Author::Model(_), ContentPart::Reasoning(_)) => EntryKind::Thinking,
        (Author::Model(_), ContentPart::Text(_)) => EntryKind::AssistantText,
        (Author::Model(_), ContentPart::ToolCall(_)) => EntryKind::ToolCall,
        (Author::Tool, ContentPart::ToolResult(_)) => EntryKind::ToolResult,
        (author, part) => {
            return Err(CoreError::Invalid(format!(
                "{author:?} cannot produce a {} part",
                part_name(part)
            )));
        }
    })
}

fn part_name(part: &ContentPart) -> &'static str {
    match part {
        ContentPart::Reasoning(_) => "reasoning",
        ContentPart::Text(_) => "text",
        ContentPart::ToolCall(_) => "tool_call",
        ContentPart::ToolResult(_) => "tool_result",
        ContentPart::Image(_) => "image",
    }
}

/// Splits a part into what goes in `data` and what goes in `native`.
/// The part written to `data` always has `raw: None`, so the two columns never disagree about
/// which one is authoritative.
pub fn split_raw(part: ContentPart) -> (ContentPart, Option<Value>) {
    match part {
        ContentPart::Reasoning(p) => (
            ContentPart::Reasoning(ReasoningPart {
                raw: None,
                ..p.clone()
            }),
            p.raw,
        ),
        ContentPart::Text(p) => (
            ContentPart::Text(TextPart {
                raw: None,
                ..p.clone()
            }),
            p.raw,
        ),
        // A tool-call group carries raw per call, which cannot be lifted into one column without
        // losing which call it belonged to. It stays inside `data`.
        other => (other, None),
    }
}

/// Builds the entry for one content part.
/// `round_id` is absent for the user's own parts: they belong to a turn, not to a response. Every
/// assistant part must carry one — `build_context` groups by it, and cannot group without it.
pub fn to_entry(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    round_id: Option<zlogic_protocol::RoundId>,
    author: &Author,
    part: ContentPart,
) -> Result<NewEntry> {
    let kind = kind_of(&part, author)?;
    let (data_part, native) = split_raw(part);
    let data = serde_json::to_value(&data_part).map_err(CoreError::from)?;

    let mut e = NewEntry::new(session_id, turn_id, turn_seq, kind, data);
    if let Some(r) = round_id {
        e = e.in_round(r);
    }
    if let Some(n) = native {
        e = e.with_native(n);
    }
    if let Some(s) = author.source() {
        e = e.from_model(s);
    }
    Ok(e)
}

/// Persists one submitted user part without translating it into a model-facing representation.
/// File references remain structured in `session_entry.data`. Uploaded attachment references must
/// already have been materialized into files by the engine.
pub fn input_entry(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    author: &Author,
    part: MessagePart,
) -> Result<NewEntry> {
    let kind = match (author, &part) {
        (Author::User | Author::Steering, MessagePart::SkillUnload { .. }) => {
            EntryKind::SkillUnload
        }
        (Author::User, _) => EntryKind::User,
        (Author::Steering, _) => EntryKind::Steering,
        other => {
            return Err(CoreError::Invalid(format!(
                "{:?} cannot produce a submitted input part",
                other.0
            )));
        }
    };
    if matches!(
        part,
        MessagePart::TaskUpdate { .. }
            | MessagePart::Attachment { .. }
            | MessagePart::Skill { .. }
            | MessagePart::SkillLoad { .. }
    ) {
        return Err(CoreError::Invalid(
            "task_update, attachment, skill references and skill loads must be normalized before persistence".into(),
        ));
    }
    let data = serde_json::to_value(part).map_err(CoreError::from)?;
    Ok(NewEntry::new(session_id, turn_id, turn_seq, kind, data))
}

/// Persists a loaded skill definition as replaceable context state.
pub fn skill_load_entry(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    name: String,
    revision: String,
    body_object: String,
    path: String,
    loaded_by: SkillLoadSource,
    unsupported: Vec<String>,
) -> Result<NewEntry> {
    let object_id = body_object.parse().map_err(CoreError::from)?;
    let data = serde_json::to_value(MessagePart::SkillLoad {
        name,
        revision,
        body_object,
        path,
        loaded_by,
        unsupported,
    })
    .map_err(CoreError::from)?;
    Ok(
        NewEntry::new(session_id, turn_id, turn_seq, EntryKind::SkillLoad, data).references(
            zlogic_objects::ObjectRef::new(object_id, zlogic_objects::ObjectRole::Skill),
        ),
    )
}

/// Stores a freshly loaded body and constructs its lightweight state entry.
pub fn skill_load_entry_from_body(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    name: String,
    revision: String,
    body: String,
    path: String,
    loaded_by: SkillLoadSource,
    unsupported: Vec<String>,
    objects: &dyn zlogic_objects::ObjectStore,
) -> Result<NewEntry> {
    let body_object = objects.put(body.as_bytes()).map_err(CoreError::from)?;
    skill_load_entry(
        session_id,
        turn_id,
        turn_seq,
        name,
        revision,
        body_object.to_string(),
        path,
        loaded_by,
        unsupported,
    )
}

/// Builds the durable entry for a machine-generated task completion.
/// Its data remains structured for transcript/UI readers. [`from_entry`] is the sole place that
/// projects the structure into the user-role text the model consumes.
pub fn task_update_entry(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    update: &TaskUpdatePart,
) -> Result<NewEntry> {
    let data = serde_json::to_value(update).map_err(CoreError::from)?;
    Ok(NewEntry::new(
        session_id,
        turn_id,
        turn_seq,
        EntryKind::TaskUpdate,
        data,
    ))
}

/// Records the deferred tools `load_tool` just made available, as replaceable session state.
/// Only the names are kept — the definitions themselves are reconstructed by name from the tool
/// registry on the next turn (see `Core::loaded_tool_names` / `Materialized::with_loaded`). The
/// entries never reach the model's context: the loaded definitions travel in the request's
/// `tools` array, and replaying this row as prose would be noise.
pub fn tool_load_entry(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    names: Vec<String>,
) -> Result<NewEntry> {
    let data = serde_json::json!({ "names": names });
    Ok(NewEntry::new(
        session_id,
        turn_id,
        turn_seq,
        EntryKind::ToolLoad,
        data,
    ))
}

/// The names recorded by one [`tool_load_entry`].
pub fn tool_load_names(rec: &EntryRecord, data: &Value) -> Result<Vec<String>> {
    data.get("names")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .map(|name| {
                    name.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        CoreError::Corrupt(format!(
                            "tool-load entry {} has a non-string name",
                            rec.entry_id
                        ))
                    })
                })
                .collect()
        })
        .unwrap_or_else(|| {
            Err(CoreError::Corrupt(format!(
                "tool-load entry {} has no `names` array",
                rec.entry_id
            )))
        })
}

/// The **UI projection** of a tool result: its outcome status and its display cards.
/// This is the single definition of the `display` column's shape. The reader is
/// `zlogic_engine::transcript`; keeping the writer's shape in one named function is what stops the two
/// sides from drifting into "works in this build only".
/// # Why not in `data`
/// Model-produced `data` holds a canonical [`ContentPart`]; submitted user `data` holds its
/// [`MessagePart`] unchanged. `ToolDisplay` and the fine-grained `ToolStatus` are UI types: the
/// model sees only `content` and `is_error`, and putting them in [`zlogic_protocol::message`] would
/// make the LLM wire model depend on the UI stream model. Same reasoning as `native` having its own
/// column.
/// # Written even when there are no cards
/// A result with no `display` still records its status and duration — that is the whole point for
/// `Denied` / `Timeout` / `Cancelled`, which carry no cards but must not read back as a plain error.
/// # `elapsed_ms` is wall time, gate included
/// It spans the whole call, so a permission prompt the user sat on for three minutes lands here
/// too. That is deliberate — it matches what the user actually waited — but it means the per-tool
/// figure in usage reports is "time until this call was answered", not "time this tool computed".
pub fn tool_display(result: &zlogic_tools::ToolExecResult, elapsed_ms: u64) -> Value {
    let cards: Vec<zlogic_protocol::stream::ToolDisplay> = result
        .display
        .iter()
        .map(crate::sink::display_to_wire)
        .collect();
    serde_json::json!({
        "status": crate::round::wire_status(result.status),
        "cards": cards,
        "duration_ms": elapsed_ms,
    })
}

/// A notice's entry. `kind: Event` — timeline-only, never replayed to the model.
/// Shape is fixed by its readers: `zlogic_engine::transcript` renders it, and nothing else looks at
/// it. Keeping the writer in a named function is what keeps the two ends from drifting.
/// # Why notices are persisted at all
/// They are the record of **what the agent told the user about its own behaviour** — a tool that
/// was not offered, a context that got compacted, a model that was swapped for one turn. Only
/// streaming them means reopening the session silently drops every one of those explanations, and
/// the user is left with a transcript that does not account for what happened.
pub fn notice_entry(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    level: zlogic_protocol::stream::NoticeLevel,
    code: &str,
    message: &zlogic_protocol::LocalizedMessage,
) -> Result<NewEntry> {
    let data = serde_json::json!({ "level": level, "code": code, "message": message });
    Ok(NewEntry::new(
        session_id,
        turn_id,
        turn_seq,
        EntryKind::Event,
        data,
    ))
}

/// A question put to the user. `interaction_id` must match the one on the stream, or the two
/// halves cannot be paired.
/// # Written **before** the question is asked
/// [`zlogic_store::EntryStore::pending_interactions`] is what a reconnecting client reads to learn
/// there is a dialog waiting (the stream has no replay). Writing the request after the answer
/// arrives would make that query blind for exactly the window it exists to cover.
pub fn interaction_request_entry(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    interaction_id: &str,
    call_id: Option<&zlogic_protocol::CallId>,
    body: &zlogic_protocol::interaction::InteractionBody,
) -> Result<NewEntry> {
    let mut data = serde_json::json!({
        "interaction_id": interaction_id,
        "body": serde_json::to_value(body).map_err(CoreError::from)?,
    });
    if let Some(call_id) = call_id {
        data["call_id"] = serde_json::Value::String(call_id.to_string());
    }
    Ok(NewEntry::new(
        session_id,
        turn_id,
        turn_seq,
        EntryKind::InteractionRequest,
        data,
    ))
}

/// The answer. **Always written**, including on cancellation — a request with no response is
/// what `pending_interactions` reports as still waiting, so an unanswered row would leave the
/// session looking permanently blocked on a dialog nobody will ever see again.
pub fn interaction_response_entry(
    session_id: zlogic_protocol::SessionId,
    turn_id: zlogic_protocol::TurnId,
    turn_seq: i64,
    interaction_id: &str,
    decision: &zlogic_protocol::interaction::InteractionDecision,
) -> Result<NewEntry> {
    let data = serde_json::json!({
        "interaction_id": interaction_id,
        "decision": serde_json::to_value(decision).map_err(CoreError::from)?,
    });
    Ok(NewEntry::new(
        session_id,
        turn_id,
        turn_seq,
        EntryKind::InteractionResponse,
        data,
    ))
}

/// Reads an entry back as a content part, re-attaching `native`.
/// `data` is passed in separately because an offloaded entry's payload comes from the object store
/// rather than from the row.
pub fn from_entry(rec: &EntryRecord, data: &Value) -> Result<ContentPart> {
    if rec.kind == EntryKind::TaskUpdate {
        let update: TaskUpdatePart = serde_json::from_value(data.clone()).map_err(|e| {
            CoreError::Corrupt(format!(
                "task update entry {} has unreadable data: {e}",
                rec.entry_id
            ))
        })?;
        let mut lines = vec![format!(
            "[background task update] task {}: {}",
            update.task_id, update.state
        )];
        if let Some(summary) = update.summary.as_deref().filter(|s| !s.is_empty()) {
            lines.push(summary.to_string());
        }
        if let Some(command) = update.command.as_deref() {
            lines.push(format!("command: {command}"));
        }
        if let Some(cwd) = update.cwd.as_deref() {
            lines.push(format!("cwd: {cwd}"));
        }
        if let Some(agent) = update.agent.as_deref() {
            lines.push(format!("agent: {agent}"));
        }
        if let Some(preview) = update.preview.as_deref().filter(|p| !p.is_empty()) {
            lines.push("--- output preview ---".into());
            lines.push(preview.to_string());
        }
        return Ok(ContentPart::Text(TextPart {
            text: lines.join("\n"),
            raw: None,
            truncated: false,
        }));
    }

    if rec.kind == EntryKind::SkillLoad {
        return Err(CoreError::Invalid(
            "skill loads require their referenced body object during context projection".into(),
        ));
    }

    if rec.kind == EntryKind::SkillUnload {
        let MessagePart::SkillUnload { name } = serde_json::from_value::<MessagePart>(data.clone())
            .map_err(|error| {
                CoreError::Corrupt(format!(
                    "skill unload entry {} has unreadable data: {error}",
                    rec.entry_id
                ))
            })?
        else {
            return Err(CoreError::Corrupt(format!(
                "skill unload entry {} does not contain skill_unload data",
                rec.entry_id
            )));
        };
        return Ok(ContentPart::Text(TextPart {
            text: format!(
                "<skill_unload name={name:?}>Stop applying this skill in subsequent work.</skill_unload>"
            ),
            raw: None,
            truncated: false,
        }));
    }

    if matches!(rec.kind, EntryKind::User | EntryKind::Steering) {
        match data.get("type").and_then(Value::as_str) {
            Some("skill") => {
                return Err(CoreError::Corrupt(format!(
                    "input entry {} contains an unresolved skill reference",
                    rec.entry_id
                )));
            }
            Some("skill_invocation") => {
                let MessagePart::SkillInvocation { name, args } =
                    serde_json::from_value::<MessagePart>(data.clone()).map_err(|e| {
                        CoreError::Corrupt(format!(
                            "input entry {} has unreadable skill invocation: {e}",
                            rec.entry_id
                        ))
                    })?
                else {
                    unreachable!("the serialized type was checked above");
                };
                return Ok(ContentPart::Text(TextPart {
                    text: skill_invocation_message(&name, args.as_deref()),
                    raw: None,
                    truncated: false,
                }));
            }
            _ => {}
        }
    }

    if matches!(rec.kind, EntryKind::User | EntryKind::Steering)
        && data.get("type").and_then(Value::as_str) == Some("file")
    {
        let MessagePart::File { path } = serde_json::from_value::<MessagePart>(data.clone())
            .map_err(|e| {
                CoreError::Corrupt(format!(
                    "input entry {} has unreadable file data: {e}",
                    rec.entry_id
                ))
            })?
        else {
            unreachable!("the serialized type was checked above");
        };
        return Ok(ContentPart::Text(TextPart {
            text: format!("<file path=\"{path}\" />"),
            raw: None,
            truncated: false,
        }));
    }

    let part: ContentPart = serde_json::from_value(data.clone()).map_err(|e| {
        CoreError::Corrupt(format!("entry {} has unreadable data: {e}", rec.entry_id))
    })?;

    let Some(native) = rec.native.clone() else {
        return Ok(part);
    };
    Ok(match part {
        ContentPart::Reasoning(p) => ContentPart::Reasoning(ReasoningPart {
            raw: Some(native),
            ..p
        }),
        ContentPart::Text(p) => ContentPart::Text(TextPart {
            raw: Some(native),
            ..p
        }),
        // Any other kind carrying a native column is a writer bug; the column is meaningless
        // there. Keep the part and say so rather than dropping data silently.
        other => {
            tracing::warn!(
                target: "zlogic::core",
                entry = %rec.entry_id,
                "native column on an entry kind that cannot hold one; ignored"
            );
            other
        }
    })
}

pub(crate) fn skill_load_part(
    name: &str,
    revision: &str,
    body: &str,
    path: &str,
    loaded_by: SkillLoadSource,
    unsupported: &[String],
) -> ContentPart {
    let loaded_by = match loaded_by {
        SkillLoadSource::User => "user",
        SkillLoadSource::Model => "model",
    };
    let mut message = format!(
        "<skill_load name={name:?} revision={revision:?} path={path:?} loaded_by={loaded_by:?}>\n\
         Loaded procedure for the skill {name:?}. It is more specific than general guidance. \
         Apply it whenever a subsequent invocation names this skill; stop and say so if it asks \
         for something you cannot do.\n\n{body}"
    );
    if !unsupported.is_empty() {
        message.push_str(&format!(
            "\n\n[This skill declares {}, which this build does not apply. Its steps still hold, \
             but nothing was pre-authorised — expect the usual approvals.]",
            unsupported.join(", ")
        ));
    }
    message.push_str("\n</skill_load>");
    ContentPart::Text(TextPart {
        text: message,
        raw: None,
        truncated: false,
    })
}

fn skill_invocation_message(name: &str, args: Option<&str>) -> String {
    match args {
        Some(args) => format!(
            "<skill_invocation name={name:?} arguments={args:?}>\n\
             Apply the loaded skill {name:?} now. Interpret `$ARGUMENTS` as {args:?} and `$1` \
             through `$9` as its whitespace-separated arguments.\n\
             </skill_invocation>"
        ),
        None => format!(
            "<skill_invocation name={name:?}>\n\
             Apply the loaded skill {name:?} now with no arguments.\n\
             </skill_invocation>"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zlogic_protocol::message::{Source, ToolCall, ToolCallPart};

    fn reasoning(raw: Option<Value>) -> ContentPart {
        ContentPart::Reasoning(ReasoningPart {
            text: "thinking".into(),
            raw,
            truncated: false,
        })
    }

    fn text(t: &str) -> ContentPart {
        ContentPart::Text(TextPart {
            text: t.into(),
            raw: None,
            truncated: false,
        })
    }

    fn model() -> Author {
        Author::Model(Source::new("anthropic", "claude-opus-5"))
    }

    #[test]
    fn a_model_part_maps_to_its_own_kind() {
        assert_eq!(
            kind_of(&reasoning(None), &model()).unwrap(),
            EntryKind::Thinking
        );
        assert_eq!(
            kind_of(&text("x"), &model()).unwrap(),
            EntryKind::AssistantText
        );
        assert_eq!(
            kind_of(
                &ContentPart::ToolCall(ToolCallPart { calls: vec![] }),
                &model()
            )
            .unwrap(),
            EntryKind::ToolCall
        );
    }

    /// The pairing that a previous version got wrong: identical payloads, different kinds.
    #[test]
    fn the_same_text_part_is_user_or_assistant_depending_on_who_wrote_it() {
        assert_eq!(
            kind_of(&text("hello"), &Author::User).unwrap(),
            EntryKind::User
        );
        assert_eq!(
            kind_of(&text("hello"), &model()).unwrap(),
            EntryKind::AssistantText
        );
    }

    /// A tool is not a model, so its result carries no source for the raw gate to compare.
    #[test]
    fn only_model_parts_are_stamped_with_a_source() {
        let result = ContentPart::ToolResult(zlogic_protocol::message::ToolResultPart {
            files: Vec::new(),
            call_id: "c1".into(),
            name: "t".into(),
            content: "ok".into(),
            is_error: false,
        });
        assert_eq!(
            kind_of(&result, &Author::Tool).unwrap(),
            EntryKind::ToolResult
        );
        assert!(Author::Tool.source().is_none());
        assert!(Author::User.source().is_none());
        assert!(model().source().is_some());
    }

    #[test]
    fn an_impossible_pairing_is_rejected_rather_than_guessed() {
        assert!(matches!(
            kind_of(&reasoning(None), &Author::User),
            Err(CoreError::Invalid(_))
        ));
        assert!(matches!(
            kind_of(&text("x"), &Author::Tool),
            Err(CoreError::Invalid(_))
        ));
    }

    /// `data` must never contain raw, or the two columns could disagree about which is
    /// authoritative.
    #[test]
    fn raw_is_lifted_out_of_data() {
        let (data, native) = split_raw(reasoning(Some(json!({ "signature": "sig" }))));
        assert_eq!(native, Some(json!({ "signature": "sig" })));
        match data {
            ContentPart::Reasoning(p) => assert!(p.raw.is_none(), "data must not keep a copy"),
            other => panic!("{other:?}"),
        }
    }

    /// A tool-call group's raw is per call, so lifting it into one column would lose which call it
    /// belonged to.
    #[test]
    fn tool_call_raw_stays_inside_data() {
        let part = ContentPart::ToolCall(ToolCallPart {
            calls: vec![ToolCall {
                id: "c1".into(),
                name: "t".into(),
                args: "{}".into(),
                raw: Some(json!({ "thoughtSignature": "sig" })),
            }],
        });
        let (data, native) = split_raw(part);
        assert!(native.is_none());
        match data {
            ContentPart::ToolCall(p) => assert!(p.calls[0].raw.is_some()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_part_round_trips_through_an_entry() {
        let db = zlogic_store::Db::open_in_memory().unwrap();
        let s = db
            .sessions()
            .create(zlogic_store::NewSession::root(
                zlogic_protocol::WorkspaceId::new(),
            ))
            .unwrap();

        let original = reasoning(Some(json!({ "type": "thinking", "signature": "EqoBCk" })));
        let e = to_entry(
            s.session_id,
            zlogic_protocol::TurnId::new(),
            1,
            Some(zlogic_protocol::RoundId::new()),
            &model(),
            original.clone(),
        )
        .unwrap();

        let rec = db.entries().append(e).unwrap();
        assert!(
            rec.round_id.is_some(),
            "an assistant part cannot be grouped without one"
        );
        assert_eq!(rec.kind, EntryKind::Thinking);
        assert!(rec.native.is_some(), "raw is findable with SQL");
        assert_eq!(rec.source, Some(Source::new("anthropic", "claude-opus-5")));

        let back = from_entry(&rec, &rec.data).unwrap();
        assert_eq!(back, original, "raw must come back byte for byte");
    }

    #[test]
    fn a_submitted_file_is_structured_in_storage_and_text_only_in_model_context() {
        let db = zlogic_store::Db::open_in_memory().unwrap();
        let s = db
            .sessions()
            .create(zlogic_store::NewSession::root(
                zlogic_protocol::WorkspaceId::new(),
            ))
            .unwrap();
        let e = input_entry(
            s.session_id,
            zlogic_protocol::TurnId::new(),
            1,
            &Author::User,
            MessagePart::File {
                path: "/tmp/report.csv".into(),
            },
        )
        .unwrap();

        let rec = db.entries().append(e).unwrap();
        assert_eq!(
            rec.data,
            json!({ "type": "file", "path": "/tmp/report.csv" }),
            "the durable row must retain the submitted part"
        );
        assert_eq!(
            from_entry(&rec, &rec.data).unwrap(),
            ContentPart::Text(TextPart {
                text: "<file path=\"/tmp/report.csv\" />".into(),
                raw: None,
                truncated: false,
            }),
            "only the model-context reader creates the placeholder"
        );
    }

    #[test]
    fn unreadable_data_is_a_typed_error() {
        let db = zlogic_store::Db::open_in_memory().unwrap();
        let s = db
            .sessions()
            .create(zlogic_store::NewSession::root(
                zlogic_protocol::WorkspaceId::new(),
            ))
            .unwrap();
        let rec = db
            .entries()
            .append(NewEntry::new(
                s.session_id,
                zlogic_protocol::TurnId::new(),
                1,
                EntryKind::AssistantText,
                json!({ "type": "not_a_part" }),
            ))
            .unwrap();
        assert!(matches!(
            from_entry(&rec, &rec.data),
            Err(CoreError::Corrupt(_))
        ));
    }
}

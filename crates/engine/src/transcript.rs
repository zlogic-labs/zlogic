use serde_json::Value;
use zlogic_protocol::MessagePart;
use zlogic_protocol::message::{ContentPart, ImageSource};
use zlogic_protocol::query::{TranscriptBody, TranscriptEntry, TranscriptPart, TranscriptToolCall};
use zlogic_protocol::stream::{ToolDisplay, ToolStatus};
use zlogic_store::{EntryKind, EntryRecord};

pub fn project(
    records: &[EntryRecord],
    load: &dyn Fn(&EntryRecord) -> Option<Value>,
) -> Vec<TranscriptEntry> {
    records
        .iter()
        .filter_map(|rec| {
            let data = load(rec)?;
            match body_of(rec, &data) {
                Some(body) => Some(TranscriptEntry {
                    entry_id: rec.entry_id.to_string(),
                    turn_id: rec.turn_id,
                    turn_seq: rec.turn_seq as u32,
                    round_id: rec.round_id,
                    round_seq: rec.round_seq,
                    at: rec.created_at,
                    agent: None,
                    is_final: rec.is_final,
                    body,
                }),
                None => {
                    tracing::debug!(
                        target: "zlogic::engine",
                        entry = %rec.entry_id,
                        kind = ?rec.kind,
                        "this line cannot be rendered; skipping"
                    );
                    None
                }
            }
        })
        .collect()
}

fn body_of(rec: &EntryRecord, data: &Value) -> Option<TranscriptBody> {
    match rec.kind {
        EntryKind::User => Some(TranscriptBody::User {
            parts: parts_of(data)?,
        }),
        EntryKind::Steering => Some(TranscriptBody::Steering {
            parts: parts_of(data)?,
        }),
        EntryKind::Thinking => match part_of(data)? {
            ContentPart::Reasoning(p) => Some(TranscriptBody::Reasoning {
                text: p.text,
                truncated: p.truncated,
            }),
            _ => None,
        },
        EntryKind::AssistantText => match part_of(data)? {
            ContentPart::Text(p) => Some(TranscriptBody::Text {
                text: p.text,
                truncated: p.truncated,
            }),
            _ => None,
        },
        EntryKind::ToolCall => match part_of(data)? {
            ContentPart::ToolCall(p) => Some(TranscriptBody::ToolCall {
                calls: p
                    .calls
                    .into_iter()
                    .map(|c| TranscriptToolCall {
                        call_id: c.id,
                        name: c.name,
                        args: c.args,
                    })
                    .collect(),
            }),
            _ => None,
        },
        EntryKind::ToolResult => match part_of(data)? {
            ContentPart::ToolResult(p) => {
                let projection = rec.display.as_ref().and_then(tool_projection);
                Some(TranscriptBody::ToolResult {
                    call_id: p.call_id,
                    name: p.name,
                    status: projection.as_ref().map(|(status, _, _)| *status).unwrap_or(
                        if p.is_error {
                            ToolStatus::Error
                        } else {
                            ToolStatus::Completed
                        },
                    ),
                    summary: summarise(&p.content),
                    display: projection
                        .as_ref()
                        .map(|(_, cards, _)| cards.clone())
                        .unwrap_or_default(),
                    duration_ms: projection
                        .as_ref()
                        .map(|(_, _, duration)| *duration)
                        .unwrap_or(0),
                })
            }
            _ => None,
        },
        // Loaded definitions are model context state, not an extra user bubble. The visible tool
        // call/result or slash invocation already explains how the state got there.
        EntryKind::SkillLoad => None,
        // Same for `load_tool`: the loaded tool's definition is not timeline content, and the
        // load_tool call/result that caused it is already rendered.
        EntryKind::ToolLoad => None,
        EntryKind::SkillUnload => Some(TranscriptBody::User {
            parts: parts_of(data)?,
        }),
        EntryKind::Compaction => {
            let from = data.get("from_turn")?.as_i64()?;
            let to = data.get("to_turn")?.as_i64()?;
            Some(TranscriptBody::Compaction {
                replaces: (from as u32, to as u32),
                summary: data
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                reason: data
                    .get("reason")
                    .and_then(|r| serde_json::from_value(r.clone()).ok())
                    .unwrap_or(zlogic_protocol::stream::CompactionReason::Threshold),
                summary_tokens: data.get("summary_tokens").and_then(Value::as_u64),
            })
        }
        EntryKind::Event if data.get("type").and_then(Value::as_str) == Some("turn_end") => {
            Some(TranscriptBody::TurnEnd {
                status: serde_json::from_value(data.get("status")?.clone()).ok()?,
                reason: data.get("reason").and_then(Value::as_str).map(String::from),
            })
        }
        EntryKind::Event => Some(TranscriptBody::Notice {
            level: serde_json::from_value(data.get("level")?.clone()).ok()?,
            code: data.get("code")?.as_str()?.to_string(),
            message: serde_json::from_value(data.get("message")?.clone()).ok()?,
        }),
        EntryKind::TaskUpdate => {
            let update: zlogic_protocol::TaskUpdatePart =
                serde_json::from_value(data.clone()).ok()?;
            Some(TranscriptBody::TaskUpdate {
                task_id: update.task_id,
                state: update.state,
                summary: update.summary,
                preview: update.preview,
                child_session_id: update.child_session_id,
                command: update.command,
                cwd: update.cwd,
                agent: update.agent,
                source: update.source,
                job_title: update.job_title,
            })
        }
        EntryKind::InteractionRequest => Some(TranscriptBody::InteractionRequest {
            interaction_id: data.get("interaction_id")?.as_str()?.to_string(),
            body: serde_json::from_value(data.get("body")?.clone()).ok()?,
        }),
        EntryKind::InteractionResponse => Some(TranscriptBody::InteractionResponse {
            interaction_id: data.get("interaction_id")?.as_str()?.to_string(),
            decision: serde_json::from_value(data.get("decision")?.clone()).ok()?,
        }),
    }
}

fn part_of(data: &Value) -> Option<ContentPart> {
    serde_json::from_value(data.clone()).ok()
}

fn tool_projection(display: &Value) -> Option<(ToolStatus, Vec<ToolDisplay>, u64)> {
    let status = serde_json::from_value(display.get("status")?.clone()).ok()?;
    let cards = display
        .get("cards")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| serde_json::from_value(c.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let duration_ms = display
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some((status, cards, duration_ms))
}

fn parts_of(data: &Value) -> Option<Vec<TranscriptPart>> {
    if data.get("type").and_then(Value::as_str) == Some("skill_unload") {
        let MessagePart::SkillUnload { name } =
            serde_json::from_value::<MessagePart>(data.clone()).ok()?
        else {
            return None;
        };
        return Some(vec![TranscriptPart::Text {
            text: format!("/unload {name}"),
        }]);
    }
    if matches!(
        data.get("type").and_then(Value::as_str),
        Some("skill" | "skill_invocation")
    ) {
        let part = serde_json::from_value::<MessagePart>(data.clone()).ok()?;
        let (name, args) = match part {
            MessagePart::Skill { name, args } | MessagePart::SkillInvocation { name, args } => {
                (name, args)
            }
            _ => return None,
        };
        return Some(vec![TranscriptPart::Text {
            text: format!(
                "/{name}{}",
                args.as_deref()
                    .map(|value| format!(" {value}"))
                    .unwrap_or_default()
            ),
        }]);
    }
    if data.get("type").and_then(Value::as_str) == Some("file") {
        let MessagePart::File { path } = serde_json::from_value(data.clone()).ok()? else {
            return None;
        };
        return Some(vec![TranscriptPart::File {
            display_name: file_name_of(&path),
            path,
            mime: None,
            bytes: None,
            preview: None,
            degraded: false,
        }]);
    }
    if data.get("type").and_then(Value::as_str) == Some("attachment") {
        let MessagePart::Attachment {
            object_id,
            name,
            mime_type,
            bytes,
        } = serde_json::from_value(data.clone()).ok()?
        else {
            return None;
        };
        return Some(vec![TranscriptPart::Attachment {
            object_id,
            display_name: name,
            mime: mime_type,
            bytes,
            preview: None,
            degraded: false,
        }]);
    }
    Some(match part_of(data)? {
        ContentPart::Text(p) => vec![TranscriptPart::Text { text: p.text }],
        ContentPart::Image(p) => vec![match p.source {
            ImageSource::Path { path } => TranscriptPart::File {
                display_name: file_name_of(&path),
                path,
                mime: Some(p.mime_type),
                bytes: None,
                preview: None,
                degraded: false,
            },
            ImageSource::Base64 { .. } => return None,
        }],
        _ => return None,
    })
}

fn file_name_of(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

fn summarise(content: &str) -> String {
    const MAX: usize = 200;
    let head: String = content
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(MAX)
        .collect();
    if head.chars().count() < content.chars().count() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::message::{
        ImagePart, ReasoningPart, TextPart, ToolCall, ToolCallPart, ToolResultPart,
    };
    use zlogic_protocol::{SessionId, TurnId};
    use zlogic_store::{Db, NewEntry, NewSession};

    struct Rig {
        db: Db,
        session: SessionId,
        turn: TurnId,
    }

    impl Rig {
        fn new() -> Self {
            let db = Db::open_in_memory().unwrap();
            let session = db
                .sessions()
                .create(NewSession::root(zlogic_protocol::WorkspaceId::new()))
                .unwrap()
                .session_id;
            Self {
                db,
                session,
                turn: TurnId::new(),
            }
        }

        fn push(&self, kind: EntryKind, data: Value) {
            self.db
                .entries()
                .append(NewEntry::new(self.session, self.turn, 1, kind, data))
                .unwrap();
        }

        fn push_in_round(&self, round: zlogic_protocol::RoundId, kind: EntryKind, data: Value) {
            self.db
                .entries()
                .append(NewEntry::new(self.session, self.turn, 1, kind, data).in_round(round))
                .unwrap();
        }

        fn push_part(&self, kind: EntryKind, part: ContentPart) {
            self.push(kind, serde_json::to_value(&part).unwrap());
        }

        fn push_tool_result(
            &self,
            name: &str,
            content: &str,
            is_error: bool,
            display: Option<Value>,
        ) {
            let part = ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "c1".into(),
                name: name.into(),
                content: content.into(),
                is_error,
            });
            let mut entry = NewEntry::new(
                self.session,
                self.turn,
                1,
                EntryKind::ToolResult,
                serde_json::to_value(&part).unwrap(),
            );
            if let Some(d) = display {
                entry = entry.with_display(d);
            }
            self.db.entries().append(entry).unwrap();
        }

        fn project(&self) -> Vec<TranscriptEntry> {
            let rows = self.db.entries().list(self.session).unwrap();
            project(&rows, &|rec| Some(rec.data.clone()))
        }

        fn bodies(&self) -> Vec<TranscriptBody> {
            self.project().into_iter().map(|e| e.body).collect()
        }
    }

    fn projection(status: &str, cards: &[ToolDisplay]) -> Value {
        serde_json::json!({
            "status": status,
            "cards": cards.iter().map(|c| serde_json::to_value(c).unwrap()).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn a_user_message_becomes_a_text_part() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::User,
            ContentPart::Text(TextPart {
                text: "hello".into(),
                raw: None,
                truncated: false,
            }),
        );

        assert_eq!(
            rig.bodies(),
            [TranscriptBody::User {
                parts: vec![TranscriptPart::Text {
                    text: "hello".into()
                }]
            }]
        );
    }

    #[test]
    fn a_structured_file_entry_remains_a_file_in_the_transcript() {
        let rig = Rig::new();
        rig.push(
            EntryKind::User,
            serde_json::json!({ "type": "file", "path": "/tmp/report.csv" }),
        );

        assert_eq!(
            rig.bodies(),
            [TranscriptBody::User {
                parts: vec![TranscriptPart::File {
                    path: "/tmp/report.csv".into(),
                    display_name: Some("report.csv".into()),
                    mime: None,
                    bytes: None,
                    preview: None,
                    degraded: false,
                }]
            }]
        );
    }

    #[test]
    fn stored_round_id_survives_transcript_projection() {
        let rig = Rig::new();
        let round = zlogic_protocol::RoundId::new();
        rig.push_in_round(
            round,
            EntryKind::AssistantText,
            serde_json::to_value(ContentPart::Text(TextPart {
                text: "second round".into(),
                raw: None,
                truncated: false,
            }))
            .unwrap(),
        );

        let projected = rig.project();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].round_id, Some(round));
        assert_eq!(
            projected[0].round_seq, None,
            "with no round_seq it is None — rows from before the migration must not read back a made-up number"
        );
    }

    #[test]
    fn stored_round_seq_survives_transcript_projection() {
        let rig = Rig::new();
        let round = zlogic_protocol::RoundId::new();
        rig.db
            .entries()
            .append(
                NewEntry::new(
                    rig.session,
                    rig.turn,
                    1,
                    EntryKind::AssistantText,
                    serde_json::to_value(ContentPart::Text(TextPart {
                        text: "second round".into(),
                        raw: None,
                        truncated: false,
                    }))
                    .unwrap(),
                )
                .in_round(round)
                .with_round_seq(2),
            )
            .unwrap();

        let projected = rig.project();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].round_id, Some(round));
        assert_eq!(projected[0].round_seq, Some(2));
    }

    #[test]
    fn steering_is_rendered_as_its_own_kind_not_as_a_user_message() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::Steering,
            ContentPart::Text(TextPart {
                text: "hold on".into(),
                raw: None,
                truncated: false,
            }),
        );

        assert!(matches!(rig.bodies()[0], TranscriptBody::Steering { .. }));
    }

    #[test]
    fn provider_native_payloads_never_reach_the_ui() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::Thinking,
            ContentPart::Reasoning(ReasoningPart {
                text: "thinking it over".into(),
                raw: Some(serde_json::json!({ "signature": "must never leak out" })),
                truncated: true,
            }),
        );

        assert_eq!(
            rig.bodies(),
            [TranscriptBody::Reasoning {
                text: "thinking it over".into(),
                truncated: true
            }]
        );
        let json = serde_json::to_string(&rig.project()).unwrap();
        assert!(!json.contains("must never leak out"), "{json}");
    }

    #[test]
    fn assistant_text_keeps_its_truncation_flag() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::AssistantText,
            ContentPart::Text(TextPart {
                text: "the answer".into(),
                raw: Some(serde_json::json!({ "thoughtSignature": "x" })),
                truncated: true,
            }),
        );

        assert_eq!(
            rig.bodies(),
            [TranscriptBody::Text {
                text: "the answer".into(),
                truncated: true
            }]
        );
    }

    #[test]
    fn a_whole_group_of_tool_calls_is_one_entry() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::ToolCall,
            ContentPart::ToolCall(ToolCallPart {
                calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        args: r#"{"path":"a.rs"}"#.into(),
                        raw: Some(serde_json::json!({ "itemId": "must not leak out" })),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "grep".into(),
                        args: r#"{"q":"fn"}"#.into(),
                        raw: None,
                    },
                ],
            }),
        );

        match &rig.bodies()[0] {
            TranscriptBody::ToolCall { calls } => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].call_id, "c1");
                assert_eq!(calls[0].args, r#"{"path":"a.rs"}"#);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            !serde_json::to_string(&rig.project())
                .unwrap()
                .contains("must not leak out")
        );
    }

    #[test]
    fn a_failed_tool_result_is_marked_as_an_error() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::ToolResult,
            ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "c1".into(),
                name: "shell".into(),
                content: "boom".into(),
                is_error: true,
            }),
        );

        match &rig.bodies()[0] {
            TranscriptBody::ToolResult {
                status, summary, ..
            } => {
                assert_eq!(*status, ToolStatus::Error);
                assert_eq!(summary, "boom");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_reloaded_tool_result_still_has_its_card() {
        let rig = Rig::new();
        let card = ToolDisplay::Diff {
            path: "a.rs".into(),
            stat: zlogic_protocol::stream::DiffStat {
                added: 3,
                removed: 1,
            },
            change: Some(zlogic_protocol::stream::FileChange::Modified),
            object: None,
        };
        rig.push_tool_result(
            "edit",
            "edited a.rs",
            false,
            Some(projection("completed", &[card])),
        );

        match &rig.bodies()[0] {
            TranscriptBody::ToolResult {
                status, display, ..
            } => {
                assert_eq!(*status, ToolStatus::Completed);
                assert_eq!(display.len(), 1, "the card must be restored");
                match &display[0] {
                    ToolDisplay::Diff { path, stat, .. } => {
                        assert_eq!(path, "a.rs");
                        assert_eq!(stat.added, 3);
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_reloaded_tool_result_still_has_its_duration() {
        let rig = Rig::new();
        rig.push_tool_result(
            "shell",
            "build finished",
            false,
            Some(serde_json::json!({
                "status": "completed",
                "cards": [],
                "duration_ms": 2750,
            })),
        );

        match &rig.bodies()[0] {
            TranscriptBody::ToolResult { duration_ms, .. } => assert_eq!(*duration_ms, 2750),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_fine_grained_status_survives_a_reload() {
        for (wire, expected) in [
            ("denied", ToolStatus::Denied),
            ("timeout", ToolStatus::Timeout),
            ("cancelled", ToolStatus::Cancelled),
            ("precheck_failed", ToolStatus::PrecheckFailed),
        ] {
            let rig = Rig::new();
            rig.push_tool_result(
                "shell",
                "nothing came of it",
                true,
                Some(serde_json::json!({ "status": wire, "cards": [] })),
            );

            match &rig.bodies()[0] {
                TranscriptBody::ToolResult { status, .. } => assert_eq!(*status, expected),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_result_with_no_cards_still_carries_its_status() {
        let rig = Rig::new();
        rig.push_tool_result(
            "write_file",
            "you denied this write",
            true,
            Some(serde_json::json!({ "status": "denied", "cards": [] })),
        );

        match &rig.bodies()[0] {
            TranscriptBody::ToolResult {
                status, display, ..
            } => {
                assert_eq!(*status, ToolStatus::Denied);
                assert!(display.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_row_written_before_the_column_existed_falls_back_to_is_error() {
        let rig = Rig::new();
        rig.push_tool_result("edit", "edited", false, None);
        rig.push_tool_result("shell", "blew up", true, None);

        let bodies = rig.bodies();
        assert!(matches!(
            bodies[0],
            TranscriptBody::ToolResult {
                status: ToolStatus::Completed,
                ..
            }
        ));
        assert!(matches!(
            bodies[1],
            TranscriptBody::ToolResult {
                status: ToolStatus::Error,
                ..
            }
        ));
    }

    #[test]
    fn a_corrupt_projection_degrades_instead_of_dropping_the_row() {
        let rig = Rig::new();
        rig.push_tool_result(
            "shell",
            "blew up",
            true,
            Some(serde_json::json!({ "status": "this is not a status" })),
        );

        match &rig.bodies()[0] {
            TranscriptBody::ToolResult {
                status, display, ..
            } => {
                assert_eq!(*status, ToolStatus::Error, "falls back to is_error");
                assert!(display.is_empty());
            }
            other => panic!("the whole result must not vanish, got {other:?}"),
        }
    }

    #[test]
    fn one_unreadable_card_does_not_drop_its_siblings() {
        let rig = Rig::new();
        let good = serde_json::to_value(ToolDisplay::Text {
            text: "this one is fine".into(),
            math: Vec::new(),
        })
        .unwrap();
        rig.push_tool_result(
            "edit",
            "edited two files",
            false,
            Some(serde_json::json!({
                "status": "completed",
                "cards": [ { "kind": "a card we have never seen" }, good ],
            })),
        );

        match &rig.bodies()[0] {
            TranscriptBody::ToolResult { display, .. } => {
                assert_eq!(display.len(), 1, "the one we recognise must be kept");
                assert!(matches!(display[0], ToolDisplay::Text { .. }));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_long_result_is_summarised_to_one_line() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::ToolResult,
            ContentPart::ToolResult(ToolResultPart {
                files: Vec::new(),
                call_id: "c1".into(),
                name: "shell".into(),
                content: format!("first line\n{}", "x".repeat(10_000)),
                is_error: false,
            }),
        );

        match &rig.bodies()[0] {
            TranscriptBody::ToolResult { summary, .. } => {
                assert_eq!(
                    summary, "first line…",
                    "only the first line, and mark that there is more"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn summarising_never_splits_a_multibyte_character() {
        let long = "é".repeat(500);
        let s = summarise(&long);
        assert!(s.chars().count() <= 201, "{}", s.chars().count());
        assert!(s.ends_with('…'));
        assert_eq!(
            serde_json::from_str::<String>(&serde_json::to_string(&s).unwrap()).unwrap(),
            s
        );
    }

    #[test]
    fn a_compaction_carries_the_summary_the_reason_and_its_cost() {
        let rig = Rig::new();
        rig.push(
            EntryKind::Compaction,
            serde_json::json!({
                "from_turn": 3,
                "to_turn": 9,
                "content": "The user is refactoring the snapshot subsystem and has settled on libgit2.",
                "reason": "context_overflow",
                "model_ref": "anthropic:haiku",
                "summary_tokens": 412,
            }),
        );

        assert_eq!(
            rig.bodies(),
            [TranscriptBody::Compaction {
                replaces: (3, 9),
                summary:
                    "The user is refactoring the snapshot subsystem and has settled on libgit2."
                        .into(),
                reason: zlogic_protocol::stream::CompactionReason::ContextOverflow,
                summary_tokens: Some(412),
            }]
        );
    }

    #[test]
    fn a_compaction_written_before_the_cost_was_recorded_still_renders() {
        let rig = Rig::new();
        rig.push(
            EntryKind::Compaction,
            serde_json::json!({ "from_turn": 1, "to_turn": 2, "content": "summary", "reason": "manual" }),
        );

        match &rig.bodies()[0] {
            TranscriptBody::Compaction {
                summary,
                reason,
                summary_tokens,
                ..
            } => {
                assert_eq!(summary, "summary");
                assert_eq!(*reason, zlogic_protocol::stream::CompactionReason::Manual);
                assert_eq!(*summary_tokens, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unrecognised_compaction_reason_falls_back_to_the_common_one() {
        let rig = Rig::new();
        rig.push(
            EntryKind::Compaction,
            serde_json::json!({ "from_turn": 1, "to_turn": 1, "content": "x", "reason": "this is not a reason" }),
        );

        match &rig.bodies()[0] {
            TranscriptBody::Compaction { reason, .. } => {
                assert_eq!(
                    *reason,
                    zlogic_protocol::stream::CompactionReason::Threshold
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_turn_end_event_carries_status_and_reason() {
        let rig = Rig::new();
        rig.push(
            EntryKind::Event,
            serde_json::json!({
                "type": "turn_end",
                "status": { "incomplete": "interrupted" },
                "reason": "the model was cut off mid-answer",
            }),
        );

        match &rig.bodies()[0] {
            TranscriptBody::TurnEnd { status, reason } => {
                assert_eq!(
                    *status,
                    zlogic_protocol::stream::TurnStatus::Incomplete(
                        zlogic_protocol::stream::IncompleteReason::Interrupted
                    )
                );
                assert_eq!(reason.as_deref(), Some("the model was cut off mid-answer"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_notice_carries_its_level_and_code() {
        let rig = Rig::new();
        rig.push(
            EntryKind::Event,
            serde_json::json!({
                "level": "warn",
                "code": "vision_reroute",
                "message": {
                    "key": "notice.vision_reroute",
                    "args": {},
                    "fallback": "switched models"
                }
            }),
        );

        match &rig.bodies()[0] {
            TranscriptBody::Notice {
                level,
                code,
                message,
            } => {
                assert_eq!(*level, zlogic_protocol::stream::NoticeLevel::Warn);
                assert_eq!(code, "vision_reroute");
                assert_eq!(message.fallback, "switched models");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn one_unreadable_row_does_not_sink_the_whole_transcript() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::User,
            ContentPart::Text(TextPart {
                text: "before".into(),
                raw: None,
                truncated: false,
            }),
        );
        rig.push(
            EntryKind::Thinking,
            serde_json::json!({ "not": "ContentPart" }),
        );
        rig.push_part(
            EntryKind::AssistantText,
            ContentPart::Text(TextPart {
                text: "after".into(),
                raw: None,
                truncated: false,
            }),
        );

        let bodies = rig.bodies();
        assert_eq!(
            bodies.len(),
            2,
            "the bad row is skipped; the other two must still be there: {bodies:?}"
        );
        assert!(matches!(bodies[0], TranscriptBody::User { .. }));
        assert!(matches!(bodies[1], TranscriptBody::Text { .. }));
    }

    #[test]
    fn a_lost_payload_object_skips_the_row() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::User,
            ContentPart::Text(TextPart {
                text: "here".into(),
                raw: None,
                truncated: false,
            }),
        );
        let rows = rig.db.entries().list(rig.session).unwrap();

        assert!(project(&rows, &|_| None).is_empty());
    }

    #[test]
    fn a_base64_image_in_storage_is_treated_as_corrupt() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::User,
            ContentPart::Image(ImagePart {
                mime_type: "image/png".into(),
                source: ImageSource::Base64 {
                    data: "AAAA".into(),
                },
            }),
        );

        assert!(rig.bodies().is_empty());
    }

    #[test]
    fn an_image_attachment_keeps_its_path_and_display_name() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::User,
            ContentPart::Image(ImagePart {
                mime_type: "image/png".into(),
                source: ImageSource::Path {
                    path: "/tmp/diagram.png".into(),
                },
            }),
        );

        match &rig.bodies()[0] {
            TranscriptBody::User { parts } => match &parts[0] {
                TranscriptPart::File {
                    path,
                    display_name,
                    mime,
                    ..
                } => {
                    assert_eq!(path, "/tmp/diagram.png");
                    assert_eq!(display_name.as_deref(), Some("diagram.png"));
                    assert_eq!(mime.as_deref(), Some("image/png"));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_projection_exposes_turn_seq_but_not_the_storage_seq() {
        let rig = Rig::new();
        rig.push_part(
            EntryKind::User,
            ContentPart::Text(TextPart {
                text: "x".into(),
                raw: None,
                truncated: false,
            }),
        );

        let entry = &rig.project()[0];
        assert_eq!(entry.turn_seq, 1);
        assert_eq!(entry.turn_id, rig.turn);
        let json = serde_json::to_value(entry).unwrap();
        assert!(json.get("seq").is_none(), "{json}");
    }
}

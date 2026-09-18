mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use zlogic_core::{Summary, TurnInput};
use zlogic_llm::mock::MockScript;
use zlogic_protocol::MessagePart;
use zlogic_protocol::TurnId;
use zlogic_protocol::message::ContentPart;
use zlogic_protocol::stream::CompactionReason;
use zlogic_store::{EntryKind, NewEntry};
use zlogic_tools::{LoadedSkill, SkillHost};

struct MutableSkill {
    revision: Mutex<String>,
    body: Mutex<String>,
    path: Mutex<String>,
}

impl MutableSkill {
    fn new(revision: &str, body: &str) -> Arc<Self> {
        Arc::new(Self {
            revision: Mutex::new(revision.into()),
            body: Mutex::new(body.into()),
            path: Mutex::new("/skills/review/SKILL.md".into()),
        })
    }

    fn update(&self, revision: &str, body: &str) {
        *self.revision.lock().unwrap() = revision.into();
        *self.body.lock().unwrap() = body.into();
    }

    fn move_to(&self, path: &str) {
        *self.path.lock().unwrap() = path.into();
    }
}

#[async_trait]
impl SkillHost for MutableSkill {
    async fn available(&self) -> Vec<String> {
        vec!["review".into()]
    }

    async fn load(&self, name: &str) -> std::result::Result<LoadedSkill, String> {
        let body = self.body.lock().unwrap().clone();
        Ok(LoadedSkill {
            name: name.into(),
            revision: self.revision.lock().unwrap().clone(),
            raw_body: body,
            path: self.path.lock().unwrap().clone(),
            unsupported: Vec::new(),
        })
    }
}

fn input() -> Vec<TurnInput> {
    vec![
        TurnInput::Submitted(MessagePart::Skill {
            name: "review".into(),
            args: None,
        }),
        TurnInput::Submitted(MessagePart::Text {
            text: "check this change".into(),
        }),
    ]
}

fn request_text(client: &Scripted, index: usize) -> String {
    client.requests()[index]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn direct_skill_input_snapshots_deduplicates_and_reloads_changed_content() {
    let h = Harness::new();
    let skill = MutableSkill::new("rev-1", "FIRST PROCEDURE");
    let client = Scripted::new(vec![
        MockScript::text("one"),
        MockScript::text("two"),
        MockScript::text("three"),
        MockScript::text("four"),
    ]);
    let core = h.core().with_skills(Some(skill.clone()));

    core.run_with_input(TurnId::new(), h.plan(client.clone()), input(), h.token())
        .await
        .unwrap();
    core.run_with_input(TurnId::new(), h.plan(client.clone()), input(), h.token())
        .await
        .unwrap();

    let second = request_text(&client, 1);
    assert_eq!(second.matches("FIRST PROCEDURE").count(), 1);
    assert!(second.contains("<skill_invocation"));

    h.store
        .with(|db| {
            db.entries().append(NewEntry::new(
                h.session,
                TurnId::new(),
                3,
                EntryKind::Compaction,
                serde_json::to_value(Summary {
                    from_turn: 1,
                    to_turn: 1,
                    content: "the first exchange".into(),
                    reason: CompactionReason::Manual,
                    model_ref: None,
                    summary_tokens: None,
                })
                .unwrap(),
            ))
        })
        .unwrap();
    core.run_with_input(TurnId::new(), h.plan(client.clone()), input(), h.token())
        .await
        .unwrap();
    let third = request_text(&client, 2);
    assert!(
        third.contains("FIRST PROCEDURE"),
        "compaction must not unload active session skill state"
    );

    skill.update("rev-2", "SECOND PROCEDURE");
    core.run_with_input(TurnId::new(), h.plan(client.clone()), input(), h.token())
        .await
        .unwrap();

    let fourth = request_text(&client, 3);
    assert!(!fourth.contains("FIRST PROCEDURE"));
    assert!(fourth.contains("SECOND PROCEDURE"));
    let loads: Vec<MessagePart> = h
        .entries()
        .iter()
        .filter_map(|entry| serde_json::from_value(entry.data.clone()).ok())
        .filter(|part| matches!(part, MessagePart::SkillLoad { .. }))
        .collect();
    assert_eq!(loads.len(), 2);
    assert!(matches!(
        &loads[0],
        MessagePart::SkillLoad {
            revision,
            ..
        } if revision == "rev-1"
    ));
    assert!(matches!(
        &loads[1],
        MessagePart::SkillLoad {
            revision,
            ..
        } if revision == "rev-2"
    ));
}

#[tokio::test]
async fn unload_removes_active_skill_state_and_the_next_invocation_loads_it_again() {
    let h = Harness::new();
    let skill = MutableSkill::new("rev-1", "ACTIVE PROCEDURE");
    let client = Scripted::new(vec![
        MockScript::text("loaded"),
        MockScript::text("unloaded"),
        MockScript::text("loaded again"),
    ]);
    let core = h.core().with_skills(Some(skill));

    core.run_with_input(TurnId::new(), h.plan(client.clone()), input(), h.token())
        .await
        .unwrap();
    core.run_with_input(
        TurnId::new(),
        h.plan(client.clone()),
        vec![
            TurnInput::Submitted(MessagePart::SkillUnload {
                name: "review".into(),
            }),
            TurnInput::Submitted(MessagePart::Text {
                text: "continue without it".into(),
            }),
        ],
        h.token(),
    )
    .await
    .unwrap();

    let unloaded = request_text(&client, 1);
    assert!(!unloaded.contains("ACTIVE PROCEDURE"));
    assert!(unloaded.contains("<skill_unload"));

    core.run_with_input(TurnId::new(), h.plan(client.clone()), input(), h.token())
        .await
        .unwrap();
    assert!(request_text(&client, 2).contains("ACTIVE PROCEDURE"));
    assert_eq!(
        h.kinds()
            .iter()
            .filter(|kind| **kind == EntryKind::SkillLoad)
            .count(),
        2,
        "an invocation after unload must create a fresh active-state load"
    );
    assert_eq!(
        h.kinds()
            .iter()
            .filter(|kind| **kind == EntryKind::SkillUnload)
            .count(),
        1
    );
}

#[tokio::test]
async fn repeated_model_skill_tool_loads_reuse_the_same_durable_state() {
    let h = Harness::new();
    let skill = MutableSkill::new("rev-tool", "TOOL PROCEDURE");
    let client = Scripted::new(vec![
        call_id(
            "skill-call-1",
            "skill",
            r#"{"name":"review","args":"strict"}"#,
        ),
        MockScript::text("first done"),
        call_id(
            "skill-call-2",
            "skill",
            r#"{"name":"review","args":"again"}"#,
        ),
        MockScript::text("second done"),
    ]);
    let core = h.core().with_skills(Some(skill));

    core.run_with_input(
        TurnId::new(),
        h.plan(client.clone()),
        vec![TurnInput::User(ContentPart::Text(
            zlogic_protocol::message::TextPart {
                text: "review this".into(),
                raw: None,
                truncated: false,
            },
        ))],
        h.token(),
    )
    .await
    .unwrap();
    core.run_with_input(
        TurnId::new(),
        h.plan(client.clone()),
        vec![TurnInput::User(ContentPart::Text(
            zlogic_protocol::message::TextPart {
                text: "review another".into(),
                raw: None,
                truncated: false,
            },
        ))],
        h.token(),
    )
    .await
    .unwrap();

    assert_eq!(
        h.kinds()
            .iter()
            .filter(|kind| **kind == EntryKind::SkillLoad)
            .count(),
        1,
        "the second call is audited by its tool call/result but must not duplicate unchanged state"
    );
    let load = h
        .entries()
        .into_iter()
        .find(|entry| entry.kind == EntryKind::SkillLoad)
        .unwrap();
    assert_eq!(load.data["loaded_by"], "model");
    let followup = request_text(&client, 1);
    assert_eq!(followup.matches("TOOL PROCEDURE").count(), 1);
    assert!(followup.contains("<skill_load"));
    assert_eq!(h.tool_results().len(), 2);
    assert!(h.tool_results()[0].1.contains("Loaded skill"));
    assert!(h.tool_results()[1].1.contains("Loaded skill"));
}

#[tokio::test]
async fn moving_an_unchanged_skill_reloads_its_path_dependent_state() {
    let h = Harness::new();
    let skill = MutableSkill::new("same-revision", "run ${SKILL_DIR}/check.sh");
    let client = Scripted::new(vec![MockScript::text("one"), MockScript::text("two")]);
    let core = h.core().with_skills(Some(skill.clone()));

    core.run_with_input(TurnId::new(), h.plan(client.clone()), input(), h.token())
        .await
        .unwrap();
    skill.move_to("/moved/review/SKILL.md");
    core.run_with_input(TurnId::new(), h.plan(client.clone()), input(), h.token())
        .await
        .unwrap();

    let second = request_text(&client, 1);
    assert!(second.contains("/moved/review/check.sh"));
    assert!(!second.contains("/skills/review/check.sh"));
    assert_eq!(
        h.kinds()
            .iter()
            .filter(|kind| **kind == EntryKind::SkillLoad)
            .count(),
        2
    );
}

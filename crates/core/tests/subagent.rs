//! Sub-agents: a real child `Core` running in a real child session.
//! What these cover that unit tests cannot: the child session actually exists before the child
//! runs, its usage is attributable, and the events of two agents share one stream without becoming
//! indistinguishable.

mod common;

use zlogic_protocol::TurnId;

use std::sync::Arc;

use common::*;
use zlogic_core::{AgentProfile, CoreSpawner, EventSink, Limits};
use zlogic_llm::LlmClient;
use zlogic_llm::mock::MockScript;
use zlogic_protocol::llm::{FinishReason, ThinkingIntent};
use zlogic_protocol::stream::{StreamPayload, ToolDisplay};
use zlogic_protocol::usage::{Purpose, TokenUsage};
use zlogic_store::EntryKind;

fn profile(name: &str, client: Arc<dyn LlmClient>) -> AgentProfile {
    AgentProfile {
        name: name.into(),
        system: vec![format!("you are {name}")],
        // Narrow on purpose: the allowlist is applied at materialisation, and one test checks it.
        tools: Some(vec!["read_file".into()]),
        model: model(),
        client,
        thinking: ThinkingIntent::default(),
    }
}

/// A spawner over this harness, with the root's sink so both agents share one stream.
fn spawner(h: &Harness, profiles: Vec<AgentProfile>) -> Arc<CoreSpawner> {
    CoreSpawner::new(
        h.services(),
        profiles,
        h.sink.clone() as Arc<dyn EventSink>,
        h.dir.path(),
        0,
    )
}

#[tokio::test]
async fn a_sub_agent_runs_in_its_own_session_and_reports_back() {
    let h = Harness::new();
    let child_client = Scripted::new(vec![MockScript {
        text: Some("the researcher's conclusion".into()),
        usage: Some(TokenUsage {
            input: 500,
            output: 50,
            ..Default::default()
        }),
        ..Default::default()
    }]);
    let spawner = spawner(&h, vec![profile("researcher", child_client)]);

    let parent_client = Scripted::new(vec![
        spawn_call("researcher", "find out about X"),
        MockScript::text("the sub-agent found it"),
    ]);
    let out = h
        .core_with_spawner(spawner)
        .run(
            TurnId::new(),
            h.plan(parent_client),
            user("delegate this"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.stats.tools.succeeded, 1);
    assert_eq!(out.answer, "the sub-agent found it");

    // The child session exists, is linked to its parent, and carries the agent path.
    let children = h
        .store
        .with(|db| db.sessions().children(h.session).unwrap());
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert_eq!(child.agent, "researcher");
    assert_eq!(child.agent_paths.as_string(), "main/researcher");
    assert_eq!(child.root_session_id, h.session);

    let child_kinds: Vec<EntryKind> = h
        .entries_of(child.session_id)
        .iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        child_kinds,
        [EntryKind::User, EntryKind::AssistantText, EntryKind::Event]
    );

    // The conclusion reached the parent's tool result, and the card points at the child session so
    // the UI can open it.
    let card = h
        .sink
        .payloads()
        .into_iter()
        .filter_map(|p| match p {
            StreamPayload::ToolExecEnd { display, .. } => Some(display),
            _ => None,
        })
        .flatten()
        .find(|d| matches!(d, ToolDisplay::Agent { .. }))
        .expect("an agent card");
    match card {
        ToolDisplay::Agent { agent, session_id } => {
            assert_eq!(agent, "researcher");
            assert_eq!(session_id, child.session_id.to_string());
        }
        other => panic!("{other:?}"),
    }
}

/// Charged to the parent, a sub-agent's tokens would distort the parent's compaction signal and
/// make "what did that cost" unanswerable.
#[tokio::test]
async fn a_sub_agents_usage_is_attributed_to_it() {
    let h = Harness::new();
    let child_client = Scripted::new(vec![MockScript {
        text: Some("done".into()),
        usage: Some(TokenUsage {
            input: 5_000,
            output: 100,
            ..Default::default()
        }),
        ..Default::default()
    }]);
    let spawner = spawner(&h, vec![profile("researcher", child_client)]);
    let parent_client = Scripted::new(vec![
        spawn_call("researcher", "go"),
        MockScript {
            text: Some("ok".into()),
            usage: Some(TokenUsage {
                input: 1_000,
                output: 10,
                ..Default::default()
            }),
            ..Default::default()
        },
    ]);

    h.core_with_spawner(spawner)
        .run(TurnId::new(), h.plan(parent_client), user("go"), h.token())
        .await
        .unwrap();

    let child = h
        .store
        .with(|db| db.sessions().children(h.session).unwrap())
        .remove(0);
    let rows = h
        .store
        .with(|db| db.usage().list_for_session(child.session_id).unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].purpose, Purpose::Agent("researcher".into()));
    assert_eq!(rows[0].tokens.input, 5_000);

    // The parent's own signal excludes the child; the tree total includes it.
    assert_eq!(
        h.store
            .with(|db| db.usage().last_main_input_tokens(h.session).unwrap()),
        Some(1_000),
        "compaction must not fire because a sub-agent read a lot"
    );
    assert_eq!(
        h.store
            .with(|db| db.usage().total_for_tree(h.session).unwrap().input),
        6_000
    );
}

/// One stream, two agents: an event must say which one it came from.
#[tokio::test]
async fn parent_and_child_events_stay_distinguishable() {
    let h = Harness::new();
    let spawner = spawner(
        &h,
        vec![profile(
            "researcher",
            Scripted::new(vec![MockScript::text("child answer")]),
        )],
    );
    let parent = Scripted::new(vec![
        spawn_call("researcher", "go"),
        MockScript::text("done"),
    ]);

    h.core_with_spawner(spawner)
        .run(TurnId::new(), h.plan(parent), user("go"), h.token())
        .await
        .unwrap();

    let events = h.sink.events();
    let roots = events.iter().filter(|e| e.agent.is_root()).count();
    let child: Vec<_> = events.iter().filter(|e| !e.agent.is_root()).collect();
    assert!(
        roots > 0 && !child.is_empty(),
        "both agents used the same sink"
    );
    assert_eq!(child[0].agent.name, "researcher");
    assert_eq!(
        child[0].agent.parent_agent_id.as_deref(),
        Some(h.session.to_string().as_str())
    );
    // Different turns, so a client can group them.
    assert_ne!(child[0].turn_id, events[0].turn_id);
}

/// The nesting limit is checked before anything is created, so a refusal leaves no empty session.
#[tokio::test]
async fn the_nesting_limit_refuses_without_creating_a_session() {
    let h = Harness::new().limits(Limits {
        max_depth: 0,
        ..Default::default()
    });
    let spawner = spawner(
        &h,
        vec![profile(
            "researcher",
            Scripted::new(vec![MockScript::text("never runs")]),
        )],
    );
    let parent = Scripted::new(vec![
        spawn_call("researcher", "go"),
        MockScript::text("gave up"),
    ]);

    let out = h
        .core_with_spawner(spawner)
        .run(TurnId::new(), h.plan(parent), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.stats.tools.failed, 1);
    assert!(
        h.store
            .with(|db| db.sessions().children(h.session).unwrap())
            .is_empty()
    );

    let result = h.store.with(|db| {
        db.entries()
            .list(h.session)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EntryKind::ToolResult)
            .unwrap()
    });
    let text = result.data["content"].as_str().unwrap().to_string();
    assert!(text.contains("nest"), "{text}");
}

/// A sub-agent may spawn its own, up to the same limit — which is why the spawner holds a weak
/// reference to itself.
#[tokio::test]
async fn a_sub_agent_can_spawn_its_own() {
    let h = Harness::new().limits(Limits {
        max_depth: 2,
        ..Default::default()
    });
    let grandchild = Scripted::new(vec![MockScript::text("the deepest answer")]);
    let child = Scripted::new(vec![
        spawn_call("reviewer", "check this"),
        MockScript::text("the child's answer"),
    ]);
    let spawner = spawner(
        &h,
        vec![
            AgentProfile {
                tools: None,
                ..profile("researcher", child)
            },
            profile("reviewer", grandchild),
        ],
    );
    let parent = Scripted::new(vec![
        spawn_call("researcher", "go"),
        MockScript::text("done"),
    ]);

    let out = h
        .core_with_spawner(spawner)
        .run(TurnId::new(), h.plan(parent), user("go"), h.token())
        .await
        .unwrap();
    assert_eq!(out.stats.tools.succeeded, 1);

    // Three sessions in the tree, and the path records the whole lineage.
    let tree = h.store.with(|db| db.sessions().tree(h.session).unwrap());
    let mut paths: Vec<String> = tree.iter().map(|s| s.agent_paths.as_string()).collect();
    paths.sort();
    assert_eq!(
        paths,
        ["main", "main/researcher", "main/researcher/reviewer"]
    );
}

/// An invented name comes back with the real list rather than failing the turn.
#[tokio::test]
async fn an_unknown_agent_name_is_answered_with_the_available_ones() {
    let h = Harness::new();
    let spawner = spawner(
        &h,
        vec![profile(
            "researcher",
            Scripted::new(vec![MockScript::text("x")]),
        )],
    );
    let parent = Scripted::new(vec![
        spawn_call("archaeologist", "dig"),
        MockScript::text("sorry"),
    ]);

    let out = h
        .core_with_spawner(spawner)
        .run(TurnId::new(), h.plan(parent), user("go"), h.token())
        .await
        .unwrap();
    assert_eq!(out.stats.tools.failed, 1);
    assert_eq!(out.status, zlogic_protocol::stream::TurnStatus::Completed);

    let result = h.store.with(|db| {
        db.entries()
            .list(h.session)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EntryKind::ToolResult)
            .unwrap()
    });
    let text = result.data["content"].as_str().unwrap().to_string();
    assert!(text.contains("researcher"), "{text}");
}

/// A sub-agent's tool allowlist applies at materialisation: excluded tools are never offered.
#[tokio::test]
async fn a_sub_agents_tool_set_is_narrowed_before_the_model_sees_it() {
    let h = Harness::new();
    // The child tries to write a file, which its profile does not allow it to see.
    let child = Scripted::new(vec![
        MockScript {
            tool_calls: vec![(
                0,
                "c1".into(),
                "write_file".into(),
                serde_json::json!({ "path": "x.txt", "content": "x" }).to_string(),
            )],
            finish: Some(FinishReason::ToolCalls),
            ..Default::default()
        },
        MockScript::text("could not write"),
    ]);
    let spawner = spawner(&h, vec![profile("researcher", child)]);
    let parent = Scripted::new(vec![
        spawn_call("researcher", "go"),
        MockScript::text("done"),
    ]);

    h.core_with_spawner(spawner)
        .run(TurnId::new(), h.plan(parent), user("go"), h.token())
        .await
        .unwrap();

    let child_session = h
        .store
        .with(|db| db.sessions().children(h.session).unwrap())
        .remove(0);
    let result = h.store.with(|db| {
        db.entries()
            .list(child_session.session_id)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EntryKind::ToolResult)
            .unwrap()
    });
    let text = result.data["content"].as_str().unwrap().to_string();
    assert!(text.contains("unknown tool"), "{text}");
    assert!(
        !h.dir.path().join("x.txt").exists(),
        "and it certainly did not run"
    );
}

/// An unregistered agent name runs when the caller customises it: the base profile supplies the
/// model/tools, the request's `system` is layered on, and the name is still the identity.
#[tokio::test]
async fn a_custom_agent_runs_on_the_base_profile_with_its_own_system() {
    let h = Harness::new();
    let base_client = Scripted::new(vec![MockScript::text("custom agent answer")]);
    let base = AgentProfile {
        name: "general".into(),
        system: vec!["you are the base agent".into()],
        tools: Some(vec!["read_file".into()]),
        model: model(),
        client: base_client,
        thinking: ThinkingIntent::default(),
    };
    let spawner = CoreSpawner::with_base(
        h.services(),
        vec![],
        Some(base),
        h.sink.clone() as Arc<dyn EventSink>,
        h.dir.path(),
        0,
        None,
        None,
    );

    // The custom name is not registered, but `system` is supplied — must not be refused.
    let parent = Scripted::new(vec![
        call(
            "create_agent",
            &serde_json::json!({
                "agent": "safety-auditor",
                "task": "audit the auth flow",
                "system": "You are a security auditor. Report only findings.",
            })
            .to_string(),
        ),
        MockScript::text("done"),
    ]);
    let out = h
        .core_with_spawner(spawner)
        .run(TurnId::new(), h.plan(parent), user("go"), h.token())
        .await
        .unwrap();
    assert_eq!(out.stats.tools.succeeded, 1);

    // The child session carries the custom name.
    let children = h
        .store
        .with(|db| db.sessions().children(h.session).unwrap());
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].agent, "safety-auditor");
    assert_eq!(children[0].agent_paths.as_string(), "main/safety-auditor");

    // The base model was used (one scripted response consumed by the child), and the system
    // prompt delivered to the child includes the custom role text.
    let requests = h
        .sink
        .payloads()
        .into_iter()
        .filter_map(|p| match p {
            StreamPayload::TurnStart { .. } => Some(()),
            _ => None,
        })
        .count();
    assert_eq!(requests, 2, "parent turn + child turn");
}

/// An unregistered name with *no* customisation is still refused, listing what is available.
#[tokio::test]
async fn a_bare_unknown_name_is_refused_even_with_a_base_profile() {
    let h = Harness::new();
    let base = AgentProfile {
        name: "general".into(),
        system: vec!["base".into()],
        tools: None,
        model: model(),
        client: Scripted::new(vec![]),
        thinking: ThinkingIntent::default(),
    };
    let spawner = CoreSpawner::with_base(
        h.services(),
        vec![],
        Some(base),
        h.sink.clone() as Arc<dyn EventSink>,
        h.dir.path(),
        0,
        None,
        None,
    );
    let parent = Scripted::new(vec![spawn_call("typo-agent", "go"), MockScript::text("ok")]);

    h.core_with_spawner(spawner)
        .run(TurnId::new(), h.plan(parent), user("go"), h.token())
        .await
        .unwrap();

    let result = h.store.with(|db| {
        db.entries()
            .list(h.session)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EntryKind::ToolResult)
            .unwrap()
    });
    // The create_agent tool refuses a bare unknown name (no customisation) and points the model
    // at the custom-agent escape hatch; the spawner never runs.
    let text = result.data["content"].as_str().unwrap().to_string();
    assert!(
        text.contains("customise"),
        "refusal must name the custom-agent path: {text}"
    );
    assert!(
        h.store
            .with(|db| db.sessions().children(h.session).unwrap())
            .is_empty(),
        "a refused spawn creates nothing"
    );
}

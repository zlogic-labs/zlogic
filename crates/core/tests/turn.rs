//! End-to-end turns, driven by a scripted client.
//! These are the tests that would catch the failures unit tests cannot see: an event emitted in the
//! wrong order, a tool result that never reached the database, a denied call that ran anyway.

mod common;

use zlogic_protocol::TurnId;

use std::sync::Arc;

use async_trait::async_trait;
use common::*;
use zlogic_core::{Limits, PlanNotice, PolicyDecision, PolicyGate, PolicyRequest};
use zlogic_llm::mock::MockScript;
use zlogic_protocol::interaction::{
    FieldValue, FormAnswer, GrantScope, InteractionBody, InteractionDecision, InteractionPort,
    InteractionRequest,
};
use zlogic_protocol::llm::{FinishReason, LlmError};
use zlogic_protocol::stream::{StreamPayload, ToolStatus, TurnStatus};
use zlogic_protocol::usage::{Purpose, TokenUsage};
use zlogic_store::{EntryKind, EntryRecord};

// ─────────────────────────── a plain turn ───────────────────────────

#[tokio::test]
async fn a_question_and_an_answer() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript {
        reasoning: Some("thinking".into()),
        text: Some("the answer".into()),
        usage: Some(TokenUsage {
            input: 1_000,
            output: 100,
            ..Default::default()
        }),
        ..Default::default()
    }]);

    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("a question"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.answer, "the answer");
    assert_eq!(out.stats.rounds, 1);
    assert_eq!(out.stats.usage.input, 1_000);
    assert_eq!(out.turn_seq, 1);

    // The user's message, then one entry per part of the response.
    assert_eq!(
        h.kinds(),
        [
            EntryKind::User,
            EntryKind::Thinking,
            EntryKind::AssistantText,
            EntryKind::Event
        ]
    );

    // The frame: a turn opens once, closes once, and closes last.
    let names = h.event_names();
    assert_eq!(names.first().unwrap(), "turn_start");
    assert_eq!(names.last().unwrap(), "turn_end");
    assert_eq!(names.iter().filter(|n| *n == "turn_end").count(), 1);
    assert!(
        names.iter().position(|n| n == "round_start").unwrap()
            < names.iter().position(|n| n == "round_end").unwrap()
    );

    // Two blocks, each a well-formed start → delta → end. Deliberately not asserted against a
    // fixed global sequence: where `usage` lands relative to the last block is the provider's
    // choice (a tail chunk after the text, or interleaved), and pinning it would make this test
    // fail on a perfectly valid stream.
    let blocks: Vec<&String> = names.iter().filter(|n| n.starts_with("block_")).collect();
    assert_eq!(
        blocks,
        [
            "block_start",
            "block_delta",
            "block_end",
            "block_start",
            "block_delta",
            "block_end"
        ]
    );
    // And the block ids are per round and per index, so a sub-agent's cannot collide.
    let ids: Vec<String> = h
        .sink
        .payloads()
        .iter()
        .filter_map(|p| match p {
            StreamPayload::BlockStart { block_id, .. } => Some(block_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert!(
        ids[0].contains(':'),
        "round-scoped, not a bare index: {}",
        ids[0]
    );

    // What the model was actually sent.
    let req = &client.requests()[0];
    assert_eq!(req.model, "m1");
    assert_eq!(req.system[0].text, "you are a test");
    assert!(
        req.system[0].cache,
        "one cache breakpoint covers the prefix"
    );
    assert_eq!(req.messages.len(), 1, "just the user's message");
    assert!(!req.tools.is_empty(), "the built-ins are offered");
    assert_eq!(req.meta.purpose, Purpose::Main);
}

/// The reply is priced, and the row is attributed to this round.
#[tokio::test]
async fn usage_is_normalised_priced_and_stored() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript {
        text: Some("ok".into()),
        usage: Some(TokenUsage {
            input: 1_000_000,
            output: 0,
            ..Default::default()
        }),
        ..Default::default()
    }]);

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("hi"), h.token())
        .await
        .unwrap();

    let rows = h
        .store
        .with(|db| db.usage().list_for_session(h.session).unwrap());
    assert_eq!(
        rows.len(),
        1,
        "one report per round, upserted not accumulated"
    );
    assert_eq!(rows[0].purpose, Purpose::Main);
    assert_eq!(rows[0].model_ref.as_deref(), Some("mock:m1"));
    assert_eq!(rows[0].tokens.input, 1_000_000);
    assert_eq!(rows[0].cost, Some(3.0), "1M input at 3.0 per million");
    assert_eq!(
        rows[0].cost_source,
        Some(zlogic_protocol::usage::CostSource::LocalPricing)
    );

    // And the same figures reached the UI.
    match h
        .sink
        .payloads()
        .iter()
        .find(|p| matches!(p, StreamPayload::Usage { .. }))
        .unwrap()
    {
        StreamPayload::Usage {
            round,
            session,
            context,
            cost,
            ..
        } => {
            assert_eq!(round.input, 1_000_000);
            assert_eq!(session.input, 1_000_000);
            assert_eq!(context.used, Some(1_000_000));
            assert_eq!(context.window, 100_000);
            assert_eq!(cost.as_ref().unwrap().amount, 3.0);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(out.stats.usage.input, 1_000_000);
}

// ─────────────────────────── tools ───────────────────────────

#[tokio::test]
async fn a_deferred_tool_schema_is_added_only_after_load_tool_runs() {
    let h = Harness::new();
    let client = Scripted::new(vec![
        call("load_tool", r#"{"names":["palette"]}"#),
        MockScript::text("ready"),
    ]);

    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("inspect the UI"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.answer, "ready");
    let requests = client.requests();
    assert_eq!(requests.len(), 2);

    let first_names = requests[0]
        .tools
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert!(first_names.contains(&"load_tool"));
    assert!(!first_names.contains(&"palette"));
    assert!(
        requests[0]
            .system
            .iter()
            .any(|block| block.text.contains("`palette`")),
        "the small deferred catalogue is present in the system prompt"
    );

    let second_names = requests[1]
        .tools
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert!(second_names.contains(&"load_tool"));
    assert!(
        second_names.contains(&"palette"),
        "load_tool must affect the next request in the same turn"
    );
}

#[tokio::test]
async fn a_deferred_tool_stays_loaded_across_turns() {
    let h = Harness::new();

    // Turn 1: the model loads `palette` mid-turn.
    let first = Scripted::new(vec![
        call("load_tool", r#"{"names":["palette"]}"#),
        MockScript::text("loaded"),
    ]);
    h.core()
        .run(
            TurnId::new(),
            h.plan(first.clone()),
            user("inspect the UI"),
            h.token(),
        )
        .await
        .unwrap();

    // The load is session state, not just this turn's in-memory set: a `tool_load` entry records
    // the names, and nothing else writes rows of that kind.
    let tool_loads: Vec<_> = h
        .entries()
        .into_iter()
        .filter(|e| e.kind == EntryKind::ToolLoad)
        .collect();
    assert_eq!(tool_loads.len(), 1);
    let names: Vec<String> = serde_json::from_value(tool_loads[0].data["names"].clone()).unwrap();
    assert_eq!(names, ["palette"]);

    // Turn 2, same session: the model never calls `load_tool` again, yet `palette`'s definition is
    // in the first request, and the catalogue no longer offers it as something to load.
    let second = Scripted::new(vec![MockScript::text("done")]);
    h.core()
        .run(
            TurnId::new(),
            h.plan(second.clone()),
            user("continue"),
            h.token(),
        )
        .await
        .unwrap();

    let requests = second.requests();
    assert_eq!(requests.len(), 1);
    let second_names = requests[0]
        .tools
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert!(
        second_names.contains(&"palette"),
        "a tool loaded last turn must be present without a new load_tool call"
    );
    assert!(
        !requests[0]
            .system
            .iter()
            .any(|block| block.text.contains("`palette`")),
        "an already-loaded tool must not be re-offered in the deferred catalogue"
    );

    // And the state did not leak a second row.
    let tool_loads: Vec<_> = h
        .entries()
        .into_iter()
        .filter(|e| e.kind == EntryKind::ToolLoad)
        .collect();
    assert_eq!(tool_loads.len(), 1);
}

#[tokio::test]
async fn a_tool_round_then_a_final_answer() {
    let h = Harness::new();
    let path = h.dir.path().join("note.txt");
    let args =
        serde_json::json!({ "path": path.to_string_lossy(), "content": "written\n" }).to_string();

    let client = Scripted::new(vec![call("write_file", &args), MockScript::text("done")]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("write a note"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.answer, "done");
    assert_eq!(out.stats.rounds, 2, "the tool round and the answer round");
    assert_eq!(out.stats.tools.total, 1);
    assert_eq!(out.stats.tools.succeeded, 1);

    // The tool really ran.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "written\n");

    assert_eq!(
        h.kinds(),
        [
            EntryKind::User,
            EntryKind::ToolCall,
            EntryKind::ToolResult,
            EntryKind::AssistantText,
            EntryKind::Event
        ]
    );
    assert!(!h.tool_results()[0].0, "not an error");

    // The second request replays the call and its result, so the model sees what happened.
    let second = &client.requests()[1];
    let roles: Vec<_> = second.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            zlogic_protocol::message::Role::User,
            zlogic_protocol::message::Role::Assistant,
            zlogic_protocol::message::Role::Tool
        ]
    );

    let names = h.event_names();
    assert!(
        names.contains(&"tool_detected".to_string()),
        "the tool name lights up early"
    );
    let start = names.iter().position(|n| n == "tool_exec_start").unwrap();
    let end = names.iter().position(|n| n == "tool_exec_end").unwrap();
    assert!(start < end);

    // The card the UI gets, with the object id as a string.
    match h
        .sink
        .payloads()
        .into_iter()
        .find(|p| matches!(p, StreamPayload::ToolExecEnd { .. }))
        .unwrap()
    {
        StreamPayload::ToolExecEnd {
            status, display, ..
        } => {
            assert_eq!(status, ToolStatus::Completed);
            assert!(matches!(
                display[0],
                zlogic_protocol::stream::ToolDisplay::Diff { .. }
            ));
        }
        other => panic!("{other:?}"),
    }

    let stored = h
        .entries()
        .into_iter()
        .find(|e| e.kind == EntryKind::ToolResult)
        .expect("the tool result must be persisted");
    let display = stored
        .display
        .expect("the UI projection must come along with it");
    assert_eq!(display["status"], "completed");
    assert_eq!(display["cards"][0]["kind"], "diff", "{display}");
}

#[tokio::test]
async fn a_denied_calls_status_is_recorded_not_just_its_error_flag() {
    let h = Harness::new().policy(Arc::new(DenyAll));
    let path = h.dir.path().join("nope.txt");
    let args = serde_json::json!({ "path": path.to_string_lossy(), "content": "x" }).to_string();

    let client = Scripted::new(vec![call("write_file", &args), MockScript::text("ok")]);
    h.core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("write something"),
            h.token(),
        )
        .await
        .unwrap();

    let stored = h
        .entries()
        .into_iter()
        .find(|e| e.kind == EntryKind::ToolResult)
        .expect("a denied call still has a result row");

    assert_eq!(stored.data["is_error"], true);
    assert_eq!(stored.display.expect("projection")["status"], "denied");
}

/// A refused call must not run, and the model must be told why.
#[tokio::test]
async fn a_denied_call_does_not_run() {
    let h = Harness::new().policy(Arc::new(DenyAll));
    let path = h.dir.path().join("forbidden.txt");
    let args = serde_json::json!({ "path": path.to_string_lossy(), "content": "nope" }).to_string();

    let client = Scripted::new(vec![
        call("write_file", &args),
        MockScript::text("understood"),
    ]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("write it"), h.token())
        .await
        .unwrap();

    assert!(!path.exists(), "the tool must never have run");
    assert_eq!(out.stats.tools.denied, 1);
    assert_eq!(out.stats.tools.failed, 0, "a refusal is not a failure");

    let (is_error, content) = h.tool_results().remove(0);
    assert!(is_error, "the model has to see it as an error to adapt");
    assert!(
        content.contains("not in this test"),
        "with the reason: {content}"
    );
}

/// The user is asked once, and a session grant means the second identical call is not asked about.
#[tokio::test]
async fn a_session_grant_stops_the_second_prompt() {
    let answers = Answers::new(InteractionDecision::Allow {
        scope: GrantScope::Session,
        source: None,
    });
    let h = Harness::new()
        .policy(Arc::new(AlwaysAsk))
        .interaction(answers.clone());
    let path = h.dir.path().join("twice.txt");
    let args = serde_json::json!({ "path": path.to_string_lossy(), "content": "x" }).to_string();

    let client = Scripted::new(vec![
        call("write_file", &args),
        call("write_file", &args),
        MockScript::text("done"),
    ]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("write it twice"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.stats.tools.succeeded, 2);
    assert_eq!(answers.count(), 1, "the grant covered the second call");
    assert_eq!(out.stats.interactions, 1);
    assert!(
        h.event_names()
            .contains(&"interaction_required".to_string())
    );
    assert!(
        h.event_names()
            .contains(&"interaction_resolved".to_string())
    );
}

/// A grant settles uncertainty; it is not a bypass around policy. If policy learns that the same
/// operation is dangerous, the old grant must not run it.
#[tokio::test]
async fn a_session_grant_cannot_punch_through_a_later_deny() {
    struct AskThenDeny(std::sync::Mutex<u32>);

    #[async_trait]
    impl PolicyGate for AskThenDeny {
        async fn evaluate(&self, r: &PolicyRequest) -> PolicyDecision {
            let mut calls = self.0.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                PolicyDecision::Ask {
                    body: InteractionBody::Permission {
                        tool: r.tool.name.clone(),
                        args_preview: r.args.clone(),
                        reason: "not yet classified".into(),
                        caveats: Vec::new(),
                        offered_scopes: vec![GrantScope::Once, GrantScope::Session],
                        grant_preview: None,
                    },
                }
            } else {
                PolicyDecision::Deny {
                    reason: "now known dangerous".into(),
                }
            }
        }
    }

    let answers = Answers::new(InteractionDecision::Allow {
        scope: GrantScope::Session,
        source: None,
    });
    let h = Harness::new()
        .policy(Arc::new(AskThenDeny(std::sync::Mutex::new(0))))
        .interaction(answers.clone());
    let path = h.dir.path().join("grant-floor.txt");
    let args = serde_json::json!({ "path": path.to_string_lossy(), "content": "x" }).to_string();

    let client = Scripted::new(vec![
        call("write_file", &args),
        call("write_file", &args),
        MockScript::text("done"),
    ]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("try it twice"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.stats.tools.succeeded, 1);
    assert_eq!(out.stats.tools.denied, 1);
    assert_eq!(answers.count(), 1);
}

/// Waiting on the user is not the agent being slow.
#[tokio::test]
async fn time_spent_waiting_is_tracked_apart_from_execution() {
    /// Long enough that "did the wait land in `duration_ms`" is not a timing coincidence.
    const WAIT_MS: u64 = 150;

    struct Slow;
    #[async_trait]
    impl InteractionPort for Slow {
        async fn ask(&self, _r: InteractionRequest) -> Result<InteractionDecision, String> {
            tokio::time::sleep(std::time::Duration::from_millis(WAIT_MS)).await;
            Ok(InteractionDecision::Deny {
                reason: Some("no".into()),
            })
        }
    }

    let h = Harness::new()
        .policy(Arc::new(AlwaysAsk))
        .interaction(Arc::new(Slow));
    let args = serde_json::json!({ "path": "x.txt", "content": "x" }).to_string();
    let client = Scripted::new(vec![call("write_file", &args), MockScript::text("ok")]);

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();
    assert!(
        out.stats.interaction_wait_ms >= WAIT_MS - 10,
        "got {}",
        out.stats.interaction_wait_ms
    );
    // The gate awaits the user *inside* the round, so this is the assertion that matters: the wait
    // must have been taken back out of the execution figure. Asserting `elapsed_ms() >= duration +
    // wait` proves nothing — `elapsed_ms` is defined as that sum.
    assert!(
        out.stats.duration_ms < WAIT_MS,
        "the approval wait is counted twice: duration_ms {} already contains the {}ms wait",
        out.stats.duration_ms,
        out.stats.interaction_wait_ms
    );
    assert_eq!(
        out.stats.elapsed_ms(),
        out.stats.duration_ms + out.stats.interaction_wait_ms,
        "the wall clock is execution plus the wait, each counted once"
    );
}

/// A headless run must refuse rather than invent an approval.
#[tokio::test]
async fn without_an_interface_an_approval_request_fails_closed() {
    let h = Harness::new().policy(Arc::new(AlwaysAsk));
    let path = h.dir.path().join("headless.txt");
    let args = serde_json::json!({ "path": path.to_string_lossy(), "content": "x" }).to_string();

    let client = Scripted::new(vec![call("write_file", &args), MockScript::text("ok")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert!(!path.exists());
    assert_eq!(out.stats.tools.denied, 1);
    assert!(h.tool_results()[0].1.contains("no interface"));
}

/// A name the model invented comes back with the real list, not a crash.
#[tokio::test]
async fn an_unknown_tool_is_a_precheck_failure() {
    let h = Harness::new();
    let client = Scripted::new(vec![call("teleport", "{}"), MockScript::text("sorry")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("teleport"), h.token())
        .await
        .unwrap();

    assert_eq!(
        out.stats.tools.failed, 1,
        "a malformed call counts as a failure"
    );
    assert_eq!(out.stats.tools.total, 1);
    let (is_error, content) = h.tool_results().remove(0);
    assert!(is_error);
    assert!(
        content.contains("read_file"),
        "the available tools are listed: {content}"
    );
    assert_eq!(out.status, TurnStatus::Completed, "the turn carries on");
}

#[tokio::test]
async fn unparseable_arguments_come_back_to_the_model() {
    let h = Harness::new();
    let client = Scripted::new(vec![
        call("read_file", "{not json"),
        MockScript::text("my mistake"),
    ]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("read something"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.stats.tools.failed, 1);
    assert!(h.tool_results()[0].1.contains("invalid arguments"));
    assert_eq!(out.answer, "my mistake");
}

/// A gate can reject a call outright (`PolicyDecision::PrecheckFailed`): the reason comes back
/// to the model as a precheck failure, the user is never asked, and the model retries.
#[tokio::test]
async fn gate_level_rejection_comes_back_to_the_model() {
    struct RejectArgs;

    #[async_trait]
    impl PolicyGate for RejectArgs {
        async fn evaluate(&self, _r: &PolicyRequest) -> PolicyDecision {
            PolicyDecision::PrecheckFailed {
                reason: "not acceptable here".into(),
            }
        }
    }

    let h = Harness::new().policy(Arc::new(RejectArgs));
    let args = serde_json::json!({ "path": "x.txt" }).to_string();
    let client = Scripted::new(vec![call("read_file", &args), MockScript::text("fixed it")]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("read something"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.stats.tools.failed, 1);
    assert_eq!(out.status, TurnStatus::Completed, "the turn carries on");
    let (is_error, content) = h.tool_results().remove(0);
    assert!(is_error);
    assert!(
        content.contains("invalid arguments for `read_file`"),
        "{content}"
    );
    assert!(content.contains("not acceptable here"), "{content}");
    assert_eq!(out.answer, "fixed it", "the model retries");
}

#[tokio::test]
async fn schema_invalid_arguments_come_back_to_the_model_before_the_gate() {
    let h = Harness::new().policy(Arc::new(AlwaysAsk));
    let client = Scripted::new(vec![
        call("read_file", r#"{"path":123}"#), // wrong type: path is declared as a string
        call("read_file", r#"{}"#),           // missing required field: path
        MockScript::text("fixed it"),
    ]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("read stuff"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed, "the turn carries on");
    let results = h.tool_results();
    assert_eq!(
        results.len(),
        2,
        "both malformed calls come back as results"
    );
    assert!(results[0].0, "type error is an error result");
    assert!(
        results[0].1.contains("invalid arguments for `read_file`"),
        "{}",
        results[0].1
    );
    assert!(results[0].1.contains("string"), "{}", results[0].1);
    assert!(results[1].0, "missing required is an error result");
    assert!(
        results[1].1.contains("invalid arguments for `read_file`"),
        "{}",
        results[1].1
    );
    assert!(results[1].1.contains("path"), "{}", results[1].1);
    assert_eq!(out.answer, "fixed it", "the model retries with fixed JSON");
}

/// A tool's own failure is reported, not raised: the model can correct itself.
#[tokio::test]
async fn a_missing_file_is_a_tool_failure_not_a_turn_failure() {
    let h = Harness::new();
    let args = serde_json::json!({ "path": "nope.txt" }).to_string();
    let client = Scripted::new(vec![
        call("read_file", &args),
        MockScript::text("it is missing"),
    ]);

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("read it"), h.token())
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.stats.tools.failed, 1);
    assert!(h.tool_results()[0].0);
}

#[tokio::test]
async fn a_slow_tool_is_stopped_at_its_budget() {
    struct Sleeper;
    #[async_trait]
    impl zlogic_tools::Tool for Sleeper {
        fn meta(&self) -> zlogic_tools::ToolMeta {
            zlogic_tools::ToolMeta {
                name: "sleep".into(),
                source: "test",
                risk: zlogic_tools::ToolRisk::Read,
            }
        }
        fn definition(&self) -> zlogic_protocol::llm::ToolDefinition {
            zlogic_protocol::llm::ToolDefinition {
                name: "sleep".into(),
                description: "sleeps".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }
        async fn execute(
            &self,
            _ctx: &zlogic_tools::ToolCtx,
            _args: &str,
        ) -> zlogic_tools::Result<zlogic_tools::ToolExecResult> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(zlogic_tools::ToolExecResult::success("finally"))
        }
    }

    let h = Harness::new().tool(Arc::new(Sleeper)).limits(Limits {
        tool_timeout_secs: 1,
        ..Default::default()
    });

    let client = Scripted::new(vec![call("sleep", "{}"), MockScript::text("gave up")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("sleep"), h.token())
        .await
        .unwrap();

    assert_eq!(out.stats.tools.timed_out, 1);
    assert_eq!(out.stats.tools.succeeded, 0);
    assert!(h.tool_results()[0].1.contains("budget"));
}

/// Large output goes to the object store, and the entry keeps a reference to it.
#[tokio::test]
async fn a_large_tool_result_is_offloaded_and_still_referenced() {
    let h = Harness::new().limits(Limits {
        max_result_chars: 200,
        ..Default::default()
    });
    let path = h.dir.path().join("big.txt");
    std::fs::write(&path, "line of text\n".repeat(500)).unwrap();
    let args = serde_json::json!({ "path": path.to_string_lossy() }).to_string();

    let client = Scripted::new(vec![call("read_file", &args), MockScript::text("read it")]);
    h.core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("read the big file"),
            h.token(),
        )
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
    assert!(!result.objects.is_empty(), "the full text is reachable");
    let (_, content) = h.tool_results().remove(0);
    assert!(
        content.len() < 6_500,
        "the model got head and tail, not all of it"
    );
}

// ─────────────────────── ending badly ───────────────────────

#[tokio::test]
async fn a_truncated_response_ends_the_turn_as_incomplete() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript {
        text: Some("half an ans".into()),
        finish: Some(FinishReason::Length),
        ..Default::default()
    }]);

    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("write an essay"),
            h.token(),
        )
        .await
        .unwrap();
    assert_eq!(
        out.status,
        TurnStatus::Incomplete(zlogic_protocol::stream::IncompleteReason::MaxOutputTokens)
    );
    assert_eq!(out.stats.rounds, 1);
}

/// A model that keeps calling tools is stopped, and told apart from a failure.
#[tokio::test]
async fn an_endless_tool_chain_hits_the_round_limit() {
    let h = Harness::new().limits(Limits {
        max_rounds: 3,
        ..Default::default()
    });
    let args = serde_json::json!({ "path": "x.txt" }).to_string();
    let client = Scripted::new(vec![
        call("read_file", &args),
        call("read_file", &args),
        call("read_file", &args),
        call("read_file", &args),
    ]);

    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("loop forever"),
            h.token(),
        )
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::LimitReached);
    assert_eq!(out.stats.rounds, 3);

    let reason = h
        .sink
        .payloads()
        .iter()
        .find_map(|p| match p {
            StreamPayload::TurnEnd { reason, .. } => reason.clone(),
            _ => None,
        })
        .expect("TurnEnd must carry a reason");
    assert!(reason.contains("round limit"), "{reason}");
    assert!(
        reason.contains("3"),
        "the limit value must be stated: {reason}"
    );

    let notices: Vec<EntryRecord> = h
        .entries()
        .into_iter()
        .filter(|e| e.data["code"] == "limit_reached")
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "hitting the limit must persist one notice"
    );
    let fallback = notices[0].data["message"]["fallback"].as_str().unwrap();
    assert!(fallback.contains("round limit"), "{fallback}");
}

/// A handshake failure ends the turn, and the reason goes out before the end event.
#[tokio::test]
async fn a_failed_request_reports_the_reason_then_ends_the_turn() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript {
        fail_with: Some(LlmError {
            kind: zlogic_protocol::llm::LlmErrorKind::Auth,
            retryable: false,
            message: "bad key".into(),
            status: Some(401),
            request_id: None,
        }),
        ..Default::default()
    }]);

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("hi"), h.token())
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Failed);

    let names = h.event_names();
    let err = names.iter().position(|n| n == "error").unwrap();
    let end = names.iter().position(|n| n == "turn_end").unwrap();
    assert!(err < end, "`error` is not terminal; `turn_end` is");

    // The user's message survives, so they can retry without retyping it. The failure itself is
    // persisted as a timeline notice (LLM errors must survive the live projection being replaced
    // by history — see `TurnEmitter::error`), carrying the technical detail.
    let kinds = h.kinds();
    assert_eq!(kinds, [EntryKind::User, EntryKind::Event, EntryKind::Event]);
    let entries = h.entries();
    let event = entries
        .iter()
        .rev()
        .find(|e| e.data.get("code").is_some())
        .unwrap();
    assert_eq!(event.data["code"], "turn_error.llm_auth_failed");
    assert_eq!(event.data["level"], "warn");
    let fallback = event.data["message"]["fallback"].as_str().unwrap();
    assert!(fallback.contains("bad key"), "{fallback}");
}

/// A stream that dies **after** text has visibly streamed must not lose that text: the user
/// watched it appear, so it has to survive a reload. It is stored truncated, with no `raw`.
#[tokio::test]
async fn text_streamed_before_a_mid_stream_error_survives_the_reload() {
    use zlogic_protocol::llm::{LlmEvent, PartKind};

    struct DiesMidStream;
    #[async_trait]
    impl zlogic_llm::LlmClient for DiesMidStream {
        async fn stream(
            &self,
            _req: zlogic_protocol::llm::LlmRequest,
        ) -> Result<zlogic_llm::EventStream, LlmError> {
            // Handshake succeeds, text streams, then the transport dies before any `PartEnd`.
            let events: Vec<Result<LlmEvent, LlmError>> = vec![
                Ok(LlmEvent::PartStart {
                    index: 0,
                    kind: PartKind::Text,
                }),
                Ok(LlmEvent::PartDelta {
                    index: 0,
                    delta: "half an ".into(),
                }),
                Ok(LlmEvent::PartDelta {
                    index: 0,
                    delta: "answer".into(),
                }),
                Err(LlmError {
                    kind: zlogic_protocol::llm::LlmErrorKind::Network,
                    retryable: false,
                    message: "connection reset".into(),
                    status: None,
                    request_id: None,
                }),
            ];
            Ok(Box::pin(futures_util::stream::iter(events)))
        }
    }

    let h = Harness::new();
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(Arc::new(DiesMidStream)),
            user("hello"),
            h.token(),
        )
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Failed);

    // A stream that dies mid-way is now retried as a continuation attempt. Each attempt flushes
    // what the user watched stream in as a truncated block (with no `raw`), announces the retry
    // with a persisted notice, and — once the retry budget (2) is exhausted — the round fails.
    // So there are (1 initial + 2 retries) attempts, each of the form [truncated reply, notice].
    let kinds = h.kinds();
    let assistant_count = kinds
        .iter()
        .filter(|k| **k == EntryKind::AssistantText)
        .count();
    assert_eq!(assistant_count, 3, "{kinds:?}");
    assert_eq!(kinds.first(), Some(&EntryKind::User));

    // Every attempt's streamed half-reply survives the reload, truncated and raw-free.
    for reply in h
        .entries()
        .iter()
        .filter(|e| e.kind == EntryKind::AssistantText)
    {
        assert_eq!(reply.data["text"], "half an answer");
        assert_eq!(reply.data["truncated"], true);
        assert!(
            reply.native.is_none(),
            "an incomplete part must not carry raw"
        );
    }

    // Each retry announced itself; the final entry is the failure that ended the turn.
    let entries = h.entries();
    let event_codes: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == EntryKind::Event)
        .map(|e| e.data["code"].as_str().unwrap_or("").to_string())
        .collect();
    let retry_events = event_codes
        .iter()
        .filter(|c| *c == "llm_interrupted_retry")
        .count();
    assert_eq!(retry_events, 2, "{event_codes:?}");
    let event = entries
        .iter()
        .rev()
        .find(|e| e.data["code"] == "turn_error.llm_network_failed")
        .expect("the failure notice is persisted");
    let fallback = event.data["message"]["fallback"].as_str().unwrap();
    assert!(fallback.contains("connection reset"), "{fallback}");
}

/// A mid-stream failure retries as a **continuation**: the second request's freshly rebuilt context
/// carries what the first (interrupted) attempt already committed, so the model continues rather
/// than repeating — the very reason the retry does not duplicate the visible reply.
#[tokio::test]
async fn a_mid_stream_failure_retries_with_continuation_context() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zlogic_protocol::llm::{LlmEvent, PartKind};

    struct RetriesOnce {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl zlogic_llm::LlmClient for RetriesOnce {
        async fn stream(
            &self,
            req: zlogic_protocol::llm::LlmRequest,
        ) -> Result<zlogic_llm::EventStream, LlmError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                // First attempt: handshake succeeds, streams a partial, then the transport dies.
                let events: Vec<Result<LlmEvent, LlmError>> = vec![
                    Ok(LlmEvent::PartStart {
                        index: 0,
                        kind: PartKind::Text,
                    }),
                    Ok(LlmEvent::PartDelta {
                        index: 0,
                        delta: "Starting to answer, ".into(),
                    }),
                    Ok(LlmEvent::PartDelta {
                        index: 0,
                        delta: "but it cut out. ".into(),
                    }),
                    Err(LlmError {
                        kind: zlogic_protocol::llm::LlmErrorKind::Network,
                        retryable: false,
                        message: "connection reset".into(),
                        status: None,
                        request_id: None,
                    }),
                ];
                return Ok(Box::pin(futures_util::stream::iter(events)));
            }

            // Second (continuation) attempt: assert the rebuilt context already contains the first
            // attempt's committed partial text, then provide the true continuation.
            let all_text: Vec<String> = req
                .messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter_map(|p| match p {
                    zlogic_protocol::message::ContentPart::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect();
            assert!(
                all_text
                    .iter()
                    .any(|t| t.contains("Starting to answer, but it cut out.")),
                "continuation request must carry the interrupted attempt's committed text: {all_text:?}"
            );

            let events: Vec<Result<LlmEvent, LlmError>> = vec![
                Ok(LlmEvent::PartStart {
                    index: 0,
                    kind: PartKind::Text,
                }),
                Ok(LlmEvent::PartDelta {
                    index: 0,
                    delta: "This is the continuation.".into(),
                }),
                Ok(LlmEvent::PartEnd {
                    index: 0,
                    part: zlogic_protocol::message::ContentPart::Text(
                        zlogic_protocol::message::TextPart {
                            text: "This is the continuation.".into(),
                            raw: None,
                            truncated: false,
                        },
                    ),
                }),
                Ok(LlmEvent::ResponseEnd {
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(futures_util::stream::iter(events)))
        }
    }

    let h = Harness::new();
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(Arc::new(RetriesOnce {
                calls: AtomicUsize::new(0),
            })),
            user("hello"),
            h.token(),
        )
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Completed, "{out:?}");
    // The continuation answer is complete: the interrupted partial plus the retried finish.
    assert_eq!(
        out.answer,
        "Starting to answer, but it cut out. This is the continuation."
    );

    // One persisted notice announced the retry.
    let event_codes: Vec<_> = h
        .entries()
        .iter()
        .filter(|e| e.kind == EntryKind::Event)
        .map(|e| e.data["code"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        event_codes
            .iter()
            .filter(|c| *c == "llm_interrupted_retry")
            .count(),
        1,
        "{event_codes:?}"
    );
}

/// A turn's history is what the next turn builds on.
#[tokio::test]
async fn a_second_turn_sees_the_first() {
    let h = Harness::new();
    let core = h.core();

    let first = Scripted::new(vec![MockScript::text("first answer")]);
    core.run(TurnId::new(), h.plan(first), user("one"), h.token())
        .await
        .unwrap();

    let second = Scripted::new(vec![MockScript::text("second answer")]);
    let out = core
        .run(
            TurnId::new(),
            h.plan(second.clone()),
            user("two"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.turn_seq, 2);
    let messages = &second.requests()[0].messages;
    assert_eq!(messages.len(), 3, "user, assistant, user");
    assert_eq!(messages[2].content.len(), 1);
}

/// A prompt with no input is legal: it continues from history rather than adding to it.
#[tokio::test]
async fn a_turn_with_no_input_adds_no_entry() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript::text("carrying on")]);
    h.core()
        .run(TurnId::new(), h.plan(client), Vec::new(), h.token())
        .await
        .unwrap();
    assert_eq!(h.kinds(), [EntryKind::AssistantText, EntryKind::Event]);
}

#[tokio::test]
async fn a_notice_is_written_into_the_transcript_not_only_streamed() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript::text("ok")]);
    let mut plan = h.plan(client);
    plan.notices.push(PlanNotice::warn(
        "test_notice",
        "the profile names a tool that is not registered, so it is not offered to the model",
    ));

    h.core()
        .run(TurnId::new(), plan, user("go"), h.token())
        .await
        .unwrap();

    let streamed: Vec<_> = h
        .sink
        .payloads()
        .into_iter()
        .filter_map(|p| match p {
            StreamPayload::Notice { code, .. } => Some(code),
            _ => None,
        })
        .collect();
    assert!(
        streamed.contains(&"test_notice".to_string()),
        "{streamed:?}"
    );

    let event = h
        .entries()
        .into_iter()
        .find(|e| e.kind == EntryKind::Event)
        .expect("the notice must land as one event entry");
    assert_eq!(event.data["code"], "test_notice");
    assert_eq!(event.data["level"], "warn");
    assert!(
        event.data["message"]
            .get("fallback")
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .contains("not registered")
    );
}

#[tokio::test]
async fn a_permission_prompt_and_its_answer_are_both_persisted() {
    let answers = Answers::new(InteractionDecision::Deny {
        reason: Some("not allowed".into()),
    });
    let h = Harness::new()
        .policy(Arc::new(AlwaysAsk))
        .interaction(answers);
    let args = serde_json::json!({ "path": "x.txt", "content": "x" }).to_string();
    let client = Scripted::new(vec![call("write_file", &args), MockScript::text("ok")]);

    h.core()
        .run(TurnId::new(), h.plan(client), user("write"), h.token())
        .await
        .unwrap();

    let entries = h.entries();
    let request = entries
        .iter()
        .find(|e| e.kind == EntryKind::InteractionRequest)
        .expect("a prompt must leave a request row");
    let response = entries
        .iter()
        .find(|e| e.kind == EntryKind::InteractionResponse)
        .expect("an answer must leave a response row");

    assert_eq!(
        request.data["interaction_id"],
        response.data["interaction_id"]
    );
    assert!(
        request.data["body"].is_object(),
        "the question itself must be kept: {}",
        request.data
    );
    assert_eq!(response.data["decision"]["type"], "deny");
    assert_eq!(
        response.data["decision"]["reason"], "not allowed",
        "the reason must be kept too"
    );
    assert!(request.seq < response.seq);

    let session = request.session_id;
    let turn = request.turn_id;
    assert!(
        h.store
            .with(|db| db.entries().pending_interactions(session, turn).unwrap())
            .is_empty(),
        "once answered it must no longer count as pending"
    );
}

/// Tool-internal forms use the same core-owned persistence wrapper as policy prompts. Removing
/// engine writes must not make `ask_user` or bulk-delete confirmations invisible to the UI.
#[tokio::test]
async fn a_tool_form_is_persisted_once_by_core() {
    let answers = Answers::new(InteractionDecision::Submitted(
        FormAnswer::new().set("answer", FieldValue::Text("continue".into())),
    ));
    let h = Harness::new().interaction(answers.clone());
    let args = serde_json::json!({ "question": "continue?" }).to_string();
    let client = Scripted::new(vec![call("ask_user", &args), MockScript::text("ok")]);

    h.core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("ask me when needed"),
            h.token(),
        )
        .await
        .unwrap();

    let entries = h.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::InteractionRequest)
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::InteractionResponse)
            .count(),
        1
    );
    assert_eq!(answers.count(), 1);
}

#[tokio::test]
async fn a_prompt_whose_transport_failed_is_still_closed_out() {
    struct Broken;
    #[async_trait]
    impl InteractionPort for Broken {
        async fn ask(&self, _r: InteractionRequest) -> Result<InteractionDecision, String> {
            Err("the UI connection dropped".into())
        }
    }

    let h = Harness::new()
        .policy(Arc::new(AlwaysAsk))
        .interaction(Arc::new(Broken));
    let args = serde_json::json!({ "path": "x.txt", "content": "x" }).to_string();
    let client = Scripted::new(vec![call("write_file", &args), MockScript::text("ok")]);

    h.core()
        .run(TurnId::new(), h.plan(client), user("write"), h.token())
        .await
        .unwrap();

    let entries = h.entries();
    let request = entries
        .iter()
        .find(|e| e.kind == EntryKind::InteractionRequest)
        .expect("asked");
    assert!(
        entries
            .iter()
            .any(|e| e.kind == EntryKind::InteractionResponse),
        "there must be a response to close it out"
    );
    assert!(
        h.store
            .with(|db| db
                .entries()
                .pending_interactions(request.session_id, request.turn_id)
                .unwrap())
            .is_empty(),
        "no confirmation box may be left pending forever"
    );
}

#[tokio::test]
async fn prompts_and_notices_never_reach_the_model() {
    let answers = Answers::new(InteractionDecision::Allow {
        scope: GrantScope::Once,
        source: None,
    });
    let h = Harness::new()
        .policy(Arc::new(AlwaysAsk))
        .interaction(answers);
    let path = h.dir.path().join("ok.txt");
    let args = serde_json::json!({ "path": path.to_string_lossy(), "content": "x" }).to_string();
    let client = Scripted::new(vec![call("write_file", &args), MockScript::text("ok")]);
    let mut plan = h
        .plan(client)
        .with_tools(Some(vec!["write_file".into(), "an absent tool".into()]));
    plan.notices.push(PlanNotice::warn(
        "test_notice",
        "a missing tool was not offered",
    ));

    h.core()
        .run(TurnId::new(), plan, user("write"), h.token())
        .await
        .unwrap();

    let kinds = h.kinds();
    assert!(kinds.contains(&EntryKind::Event));
    assert!(kinds.contains(&EntryKind::InteractionRequest));
    assert!(kinds.contains(&EntryKind::InteractionResponse));
    for kind in [
        EntryKind::Event,
        EntryKind::InteractionRequest,
        EntryKind::InteractionResponse,
    ] {
        assert!(!kind.goes_to_model(), "{kind:?} must not enter the context");
    }
}

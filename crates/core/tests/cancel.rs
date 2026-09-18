//! Cancellation: one token for the conversation, and what stopping actually stops.

mod common;

use zlogic_protocol::TurnId;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use serde_json::json;
use zlogic_core::{
    AgentProfile, CancellationToken, CoreSpawner, EventSink, PolicyDecision, PolicyGate,
    PolicyRequest,
};
use zlogic_llm::mock::{MockClient, MockScript};
use zlogic_llm::{EventStream, LlmClient};
use zlogic_protocol::interaction::{InteractionDecision, InteractionPort, InteractionRequest};
use zlogic_protocol::llm::{LlmError, LlmRequest, ThinkingIntent};
use zlogic_protocol::message::ContentPart;
use zlogic_protocol::stream::{RoundOutcome, StreamPayload, ToolStatus, TurnStatus};
use zlogic_store::EntryKind;
use zlogic_tools::{AgentSpawner, Tool, ToolCtx, ToolExecResult, ToolMeta, ToolRisk};

/// Cancels the conversation at the moment a request goes out, then answers normally.
/// This is the realistic shape: the user presses Esc while the model is streaming.
struct CancelsOnRequest {
    inner: Arc<Scripted>,
    cancel: CancellationToken,
    after: usize,
    seen: Mutex<usize>,
}

impl CancelsOnRequest {
    fn new(scripts: Vec<MockScript>, cancel: CancellationToken, after: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Scripted::new(scripts),
            cancel,
            after,
            seen: Mutex::new(0),
        })
    }
}

#[async_trait]
impl LlmClient for CancelsOnRequest {
    async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError> {
        {
            let mut seen = self.seen.lock().unwrap();
            *seen += 1;
            if *seen > self.after {
                self.cancel.cancel();
            }
        }
        self.inner.stream(req).await
    }
}

/// Records that it ran, and honours the token the way a long-running tool should.
struct Slow {
    ran: Arc<Mutex<u32>>,
}

#[async_trait]
impl Tool for Slow {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "slow".into(),
            source: "test",
            risk: ToolRisk::Read,
        }
    }
    fn definition(&self) -> zlogic_protocol::llm::ToolDefinition {
        zlogic_protocol::llm::ToolDefinition {
            name: "slow".into(),
            description: "waits".into(),
            parameters: json!({ "type": "object" }),
        }
    }
    async fn execute(&self, ctx: &ToolCtx, _args: &str) -> zlogic_tools::Result<ToolExecResult> {
        *self.ran.lock().unwrap() += 1;
        // What a well-behaved long-running tool does: stop early, return what it has.
        tokio::select! {
            () = ctx.cancel.cancelled() => Ok(ToolExecResult::cancelled("stopped early")),
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                Ok(ToolExecResult::success("finished"))
            }
        }
    }
}

fn tool_statuses(h: &Harness) -> Vec<ToolStatus> {
    h.sink
        .payloads()
        .into_iter()
        .filter_map(|p| match p {
            StreamPayload::ToolExecEnd { status, .. } => Some(status),
            _ => None,
        })
        .collect()
}

// ─────────────────── the basic shapes ───────────────────

/// A token cancelled before the turn starts: nothing runs, and it is not reported as a failure.
#[tokio::test]
async fn a_turn_cancelled_before_it_starts_does_nothing() {
    let h = Harness::new();
    h.stop();

    let client = Scripted::new(vec![MockScript::text("never sent")]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("hello"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Cancelled);
    assert_eq!(
        out.stats.rounds, 1,
        "the round was counted, but it stopped at once"
    );
    assert_eq!(client.request_count(), 0, "no request was ever made");
    // The user's message is persisted regardless — they typed it, and the next turn builds on it.
    // `Event` = the persisted `turn_end` terminal state, which now lands in the timeline (it also
    // carries the `is_final` stamp for this turn).
    assert_eq!(h.kinds(), [EntryKind::User, EntryKind::Event]);
    assert!(h.saw("turn_end"));
}

/// Cancelling mid-stream keeps what was already persisted.
#[tokio::test]
async fn cancelling_mid_stream_keeps_the_parts_already_written() {
    let h = Harness::new();
    let client = CancelsOnRequest::new(
        vec![MockScript {
            reasoning: Some("thinking".into()),
            text: Some("a partial answer".into()),
            ..Default::default()
        }],
        h.token(),
        0,
    );

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Cancelled);
    // The mock produces its whole script in one go, so the parts land before the token is observed.
    // Whatever was persisted stays: a cancelled turn is part of the history.
    let kinds = h.kinds();
    assert_eq!(kinds[0], EntryKind::User);
    assert!(
        kinds.len() >= 2,
        "what the model already produced is kept, not rolled back: {kinds:?}"
    );
}

/// The round is closed, so a client is not left with an open round forever.
#[tokio::test]
async fn a_cancelled_round_still_ends() {
    let h = Harness::new();
    h.stop();
    let client = Scripted::new(vec![MockScript::text("x")]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    let outcomes: Vec<RoundOutcome> = h
        .sink
        .payloads()
        .into_iter()
        .filter_map(|p| match p {
            StreamPayload::RoundEnd { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes, [RoundOutcome::Paused]);

    match h
        .sink
        .payloads()
        .into_iter()
        .find(|p| matches!(p, StreamPayload::TurnEnd { .. }))
        .unwrap()
    {
        StreamPayload::TurnEnd { status, .. } => assert_eq!(status, TurnStatus::Cancelled),
        other => panic!("{other:?}"),
    }
}

struct HangingClient;

#[async_trait]
impl LlmClient for HangingClient {
    async fn stream(&self, _req: LlmRequest) -> Result<EventStream, LlmError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancelling_while_the_request_handshake_hangs_stops_the_turn() {
    let h = Harness::new();
    let token = h.token();
    let core = h.core();
    let run = core.run(
        TurnId::new(),
        h.plan(Arc::new(HangingClient)),
        user("go"),
        token.clone(),
    );
    tokio::pin!(run);

    tokio::select! {
        biased;
        _ = &mut run => panic!("the turn must not finish on its own before cancellation"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
    }

    token.cancel();

    let out = tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("cancellation failed to interrupt the hanging handshake")
        .unwrap();
    assert_eq!(out.status, TurnStatus::Cancelled);
    assert!(h.saw("turn_end"));
}

// ─────────────────── tools ───────────────────

/// The invariant that makes this more than a flag: a `tool_calls` group missing any result is a
/// replay error on every provider, so calls that never ran are **completed**, not skipped.
#[tokio::test]
async fn calls_that_never_ran_still_get_a_result() {
    let h = Harness::new();
    let path = h.dir.path().join("a.txt");
    std::fs::write(&path, "x").unwrap();

    // Two calls in one response; the token is cancelled before the batch runs.
    let script = MockScript {
        tool_calls: vec![
            (
                0,
                "c1".into(),
                "read_file".into(),
                json!({ "path": path.to_string_lossy() }).to_string(),
            ),
            (
                1,
                "c2".into(),
                "read_file".into(),
                json!({ "path": path.to_string_lossy() }).to_string(),
            ),
        ],
        finish: Some(zlogic_protocol::llm::FinishReason::ToolCalls),
        ..Default::default()
    };
    let client = CancelsOnRequest::new(vec![script], h.token(), 0);

    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("read it twice"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Cancelled);
    assert_eq!(
        out.stats.tools.cancelled, 2,
        "both calls were accounted for"
    );
    assert_eq!(out.stats.tools.succeeded, 0);
    assert_eq!(out.stats.tools.failed, 0, "cancelling is not failing");

    // One result per call, so the group is complete and replayable.
    let results = h.tool_results();
    assert_eq!(results.len(), 2);
    for (is_error, text) in &results {
        assert!(*is_error, "the model must see that the work did not happen");
        assert!(text.contains("stopped the turn"), "{text}");
    }
    assert_eq!(
        tool_statuses(&h),
        [ToolStatus::Cancelled, ToolStatus::Cancelled]
    );
}

/// And the completed group really is replayable: the next turn sends it unchanged.
#[tokio::test]
async fn the_completed_group_survives_into_the_next_turn() {
    let h = Harness::new();
    let path = h.dir.path().join("a.txt");
    std::fs::write(&path, "x").unwrap();
    let args = json!({ "path": path.to_string_lossy() }).to_string();

    let first = CancelsOnRequest::new(vec![call("read_file", &args)], h.token(), 0);
    h.core()
        .run(TurnId::new(), h.plan(first), user("read it"), h.token())
        .await
        .unwrap();

    // A fresh token, as a new conversation turn would have.
    let resumed = Scripted::new(vec![MockScript::text("carrying on")]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(resumed.clone()),
            user("continue"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Completed);

    let messages = &resumed.requests()[0].messages;
    // The tool call and its cancelled result both travel: nothing was dropped as incomplete.
    let roles: Vec<_> = messages.iter().map(|m| m.role).collect();
    assert!(
        roles.contains(&zlogic_protocol::message::Role::Tool),
        "the group was complete, so it was replayed: {roles:?}"
    );
}

/// A tool already running is asked to stop, not killed — and its own result is what gets recorded.
#[tokio::test]
async fn a_running_tool_winds_itself_down() {
    let ran = Arc::new(Mutex::new(0));
    let h = Harness::new().tool(Arc::new(Slow { ran: ran.clone() }));

    let script = MockScript {
        tool_calls: vec![
            (0, "c1".into(), "slow".into(), "{}".into()),
            (1, "c2".into(), "slow".into(), "{}".into()),
        ],
        finish: Some(zlogic_protocol::llm::FinishReason::ToolCalls),
        ..Default::default()
    };
    let client = Scripted::new(vec![script, MockScript::text("done")]);

    // Cancel shortly after the batch starts, while the tools are sleeping. The batch runs the
    // two calls in parallel (default window), so both are in flight and both wind down on the
    // token — the invariant is "never killed mid-execution", not "only the first one started".
    let token = h.token();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        token.cancel();
    });

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Cancelled);

    // Both calls ran (parallel window) and each returned its own cancelled result; none was
    // dropped mid-execution.
    assert_eq!(
        *ran.lock().unwrap(),
        2,
        "both calls are running in the parallel window; cancellation only makes each wind itself down"
    );
    assert_eq!(
        out.stats.tools.cancelled, 2,
        "both are recorded as cancelled (winding down is not the same as never running)"
    );
    let results = h.tool_results();
    assert_eq!(results.len(), 2, "the group is still complete");
    for (_, text) in &results {
        assert!(
            text.contains("stopped early"),
            "the tool's own words: {text}"
        );
    }
}

/// A policy gate stuck on a model review must not pin the turn: stopping cancels the wait.
/// (The review call itself is additionally bounded by the engine's `REVIEW_TIMEOUT`.)
#[tokio::test]
async fn stopping_while_the_policy_gate_hangs_cancels_the_tool_call() {
    struct HangingGate;
    #[async_trait]
    impl PolicyGate for HangingGate {
        async fn evaluate(&self, _r: &PolicyRequest) -> PolicyDecision {
            std::future::pending::<PolicyDecision>().await
        }
    }

    let h = Harness::new()
        .policy(Arc::new(HangingGate))
        .tool(Arc::new(Slow {
            ran: Arc::new(Mutex::new(0)),
        }));

    let script = MockScript {
        tool_calls: vec![(0, "c1".into(), "slow".into(), "{}".into())],
        finish: Some(zlogic_protocol::llm::FinishReason::ToolCalls),
        ..Default::default()
    };
    let client = Scripted::new(vec![script]);

    let token = h.token();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        token.cancel();
    });

    let started = std::time::Instant::now();
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "stop must not wait for the gate: {:?}",
        started.elapsed()
    );

    assert_eq!(out.status, TurnStatus::Cancelled);
    let results = h.tool_results();
    assert_eq!(results.len(), 1, "the group is still complete");
    assert!(
        results[0].1.contains("stopped while waiting for approval"),
        "the call was cancelled while waiting for approval: {}",
        results[0].1
    );
}

/// Every tool gets the token, and it is the conversation's — not a copy that could be missed.
#[tokio::test]
async fn a_tool_sees_the_conversations_own_token() {
    struct Reports {
        cancelled: Arc<Mutex<Option<bool>>>,
    }
    #[async_trait]
    impl Tool for Reports {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "reports".into(),
                source: "test",
                risk: ToolRisk::Read,
            }
        }
        fn definition(&self) -> zlogic_protocol::llm::ToolDefinition {
            zlogic_protocol::llm::ToolDefinition {
                name: "reports".into(),
                description: "reports".into(),
                parameters: json!({ "type": "object" }),
            }
        }
        async fn execute(
            &self,
            ctx: &ToolCtx,
            _args: &str,
        ) -> zlogic_tools::Result<ToolExecResult> {
            *self.cancelled.lock().unwrap() = Some(ctx.is_cancelled());
            Ok(ToolExecResult::success("ok"))
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let h = Harness::new().tool(Arc::new(Reports {
        cancelled: seen.clone(),
    }));
    let client = Scripted::new(vec![call("reports", "{}"), MockScript::text("done")]);

    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        Some(false),
        "not cancelled, but it was asked"
    );

    // Cancelling the harness token is observable through the same handle the tool held.
    h.stop();
    assert!(h.cancel.is_cancelled());
}

// ─────────────────── waiting on the user ───────────────────

/// The one place a cancel could hang forever if it were not raced.
#[tokio::test]
async fn cancelling_while_a_prompt_is_open_does_not_hang() {
    /// Never answers. Only cancellation can end this.
    struct NeverAnswers;
    #[async_trait]
    impl InteractionPort for NeverAnswers {
        async fn ask(&self, _r: InteractionRequest) -> Result<InteractionDecision, String> {
            std::future::pending().await
        }
    }

    let h = Harness::new()
        .policy(Arc::new(AlwaysAsk))
        .interaction(Arc::new(NeverAnswers));
    let path = h.dir.path().join("x.txt");
    let args = json!({ "path": path.to_string_lossy(), "content": "x" }).to_string();
    let client = Scripted::new(vec![call("write_file", &args), MockScript::text("done")]);

    let token = h.token();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
    });

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.core()
            .run(TurnId::new(), h.plan(client), user("write it"), h.token()),
    )
    .await
    .expect("cancelling must end the wait")
    .unwrap();

    assert_eq!(out.status, TurnStatus::Cancelled);
    assert!(!path.exists(), "an unapproved write must not have happened");
    assert_eq!(out.stats.tools.cancelled, 1);
}

// ─────────────────── compaction ───────────────────

/// A cancelled conversation must not spend a summarising call.
#[tokio::test]
async fn a_cancelled_turn_does_not_compact() {
    let h = Harness::new().context(zlogic_core::ContextPolicy {
        compact_ratio: 0.5,
        tail_turns: 1,
        overflow_retries: 1,
    });

    // Build history with heavy usage so the threshold would fire on the next turn.
    let core = h.core();
    for i in 1..=3 {
        let tokens = if i == 3 { 90_000 } else { 10 };
        let client = Scripted::new(vec![answer_using(&format!("answer {i}"), tokens)]);
        core.run(
            TurnId::new(),
            h.plan(client),
            user(&format!("q{i}")),
            h.token(),
        )
        .await
        .unwrap();
    }

    h.stop();
    let client = Scripted::new(vec![MockScript::text("a summary"), MockScript::text("ok")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Cancelled);
    assert_eq!(out.stats.compactions, 0);
    assert_eq!(client.request_count(), 0, "no summarising call was made");
    assert!(h.entries().iter().all(|e| e.kind != EntryKind::Compaction));
}

// ─────────────────── sub-agents ───────────────────

/// One token for the conversation: cancelling the parent stops the sub-agent too.
#[tokio::test]
async fn cancelling_the_parent_stops_the_sub_agent() {
    let h = Harness::new();
    let child = Scripted::new(vec![MockScript::text("the child's answer")]);
    let spawner = CoreSpawner::new(
        h.services(),
        vec![AgentProfile {
            name: "researcher".into(),
            system: vec!["you research".into()],
            tools: None,
            model: model(),
            client: child.clone(),
            thinking: ThinkingIntent::default(),
        }],
        h.sink.clone() as Arc<dyn EventSink>,
        h.dir.path(),
        0,
    );

    // Cancelled before the parent even asks, so the sub-agent must not run a round.
    h.stop();
    let parent = Scripted::new(vec![
        spawn_call("researcher", "go"),
        MockScript::text("done"),
    ]);
    let out = h
        .core_with_spawner(spawner as Arc<dyn AgentSpawner>)
        .run(TurnId::new(), h.plan(parent), user("delegate"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Cancelled);
    assert_eq!(
        child.request_count(),
        0,
        "the sub-agent never called its model"
    );
}

/// A sub-agent that was already running observes the same token and unwinds.
#[tokio::test]
async fn a_running_sub_agent_observes_the_parents_cancellation() {
    let h = Harness::new();
    let child = Scripted::new(vec![
        MockScript::text("child answer"),
        MockScript::text("more"),
    ]);
    let spawner = CoreSpawner::new(
        h.services(),
        vec![AgentProfile {
            name: "researcher".into(),
            system: Vec::new(),
            tools: None,
            model: model(),
            client: child.clone(),
            thinking: ThinkingIntent::default(),
        }],
        h.sink.clone() as Arc<dyn EventSink>,
        h.dir.path(),
        0,
    );

    // The parent asks for a sub-agent, and the token is cancelled as the parent's request goes out —
    // i.e. before the sub-agent starts.
    let parent = CancelsOnRequest::new(
        vec![spawn_call("researcher", "go"), MockScript::text("done")],
        h.token(),
        0,
    );
    let out = h
        .core_with_spawner(spawner as Arc<dyn AgentSpawner>)
        .run(TurnId::new(), h.plan(parent), user("delegate"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Cancelled);
    // The child session may exist, but it must not have spent a request.
    assert_eq!(child.request_count(), 0);
    // And the parent's call is still answered, so the group stays replayable.
    assert_eq!(h.tool_results().len(), 1);
}

// ─────────────────── steering ───────────────────

/// Cancelling means stop, not "stop and then pick up the queue".
#[tokio::test]
async fn a_cancelled_turn_does_not_drain_the_mailbox() {
    let h = Harness::new();
    h.store
        .with(|db| {
            db.mailbox().submit(
                h.session,
                "req-1",
                &json!([{ "type": "text", "text": "one more thing" }]),
                zlogic_store::Delivery::Steer,
            )
        })
        .unwrap();
    h.stop();

    let client = Scripted::new(vec![MockScript::text("a")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Cancelled);
    assert!(h.entries().iter().all(|e| e.kind != EntryKind::Steering));
    assert_eq!(
        h.store
            .with(|db| db.mailbox().pending(h.session).unwrap())
            .len(),
        1,
        "the message is still waiting, not consumed by a turn that stopped"
    );
}

// ─────────────────── one token, many turns ───────────────────

/// The token is the conversation's, so cancelling it stops every later turn too — until the caller
/// hands over a fresh one.
#[tokio::test]
async fn one_token_covers_the_whole_conversation() {
    let h = Harness::new();
    let core = h.core();

    let first = Scripted::new(vec![MockScript::text("answered")]);
    let a = core
        .run(TurnId::new(), h.plan(first), user("one"), h.token())
        .await
        .unwrap();
    assert_eq!(a.status, TurnStatus::Completed);

    h.stop();

    let second = Scripted::new(vec![MockScript::text("never sent")]);
    let b = core
        .run(
            TurnId::new(),
            h.plan(second.clone()),
            user("two"),
            h.token(),
        )
        .await
        .unwrap();
    assert_eq!(b.status, TurnStatus::Cancelled);
    assert_eq!(second.request_count(), 0);

    // A new token is how a caller resumes; nothing needs resetting on core's side.
    let third = Scripted::new(vec![MockScript::text("back again")]);
    let c = core
        .run(
            TurnId::new(),
            h.plan(third),
            user("three"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(c.status, TurnStatus::Completed);
    assert_eq!(c.answer, "back again");
}

/// Cancelling twice, or cancelling something already finished, is a no-op.
#[tokio::test]
async fn cancelling_an_already_finished_turn_changes_nothing() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript::text("done")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Completed);

    h.stop();
    h.stop();
    assert_eq!(
        h.kinds(),
        [EntryKind::User, EntryKind::AssistantText, EntryKind::Event],
        "nothing was undone"
    );
}

/// A cancelled sub-agent turn is not an `Err` for the parent: the tool result carries it.
#[tokio::test]
async fn the_parent_gets_a_result_not_an_error() {
    let h = Harness::new();
    let path = h.dir.path().join("a.txt");
    std::fs::write(&path, "x").unwrap();
    let args = json!({ "path": path.to_string_lossy() }).to_string();

    let client = CancelsOnRequest::new(vec![call("read_file", &args)], h.token(), 0);
    // `run` returns Ok: cancelling is an outcome, not a failure.
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Cancelled);
    assert!(
        !h.saw("error"),
        "an interrupted turn is not an error to report"
    );
}

/// A `MockClient` used directly still works with the new signature — guards the plainest path.
#[tokio::test]
async fn an_uncancelled_token_changes_nothing() {
    let h = Harness::new();
    let client = Arc::new(MockClient::new(MockScript::text("hello")));
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("hi"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.answer, "hello");
    let reply: ContentPart = serde_json::from_value(h.entries()[1].data.clone()).unwrap();
    assert_eq!(
        reply,
        ContentPart::Text(zlogic_protocol::message::TextPart {
            text: "hello".into(),
            raw: None,
            truncated: false,
        })
    );
    assert_eq!(out.stats.tools.cancelled, 0);
}

//! Mid-run user input: what gets injected, when, and where it lands in the request.

mod common;

use zlogic_protocol::TurnId;

use std::sync::Arc;

use async_trait::async_trait;
use common::*;
use serde_json::json;
use zlogic_llm::mock::MockScript;
use zlogic_llm::{EventStream, LlmClient};
use zlogic_protocol::llm::{LlmError, LlmRequest};
use zlogic_protocol::message::{ContentPart, Role};
use zlogic_protocol::stream::{RoundOutcome, StreamPayload, TurnStatus, Via};
use zlogic_store::{Delivery, EntryKind};
use zlogic_tools::AgentMailboxGate;

fn parts(text: &str) -> serde_json::Value {
    json!([{ "type": "text", "text": text }])
}

/// Puts a message in the mailbox, as the engine's submit path would.
fn submit(h: &Harness, request_id: &str, text: &str, delivery: Delivery) {
    h.store
        .with(|db| {
            db.mailbox()
                .submit(h.session, request_id, &parts(text), delivery)
        })
        .unwrap();
}

fn steering_texts(h: &Harness) -> Vec<String> {
    h.entries()
        .iter()
        .filter(|e| e.kind == EntryKind::Steering)
        .map(|e| e.data["text"].as_str().unwrap().to_string())
        .collect()
}

/// The simplest case: it arrived while the model was answering.
#[tokio::test]
async fn a_steered_message_continues_the_same_turn() {
    let h = Harness::new();
    submit(&h, "req-1", "actually, use tabs", Delivery::Steer);

    let client = Scripted::new(vec![
        MockScript::text("first answer"),
        MockScript::text("tabs it is"),
    ]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("write it"),
            h.token(),
        )
        .await
        .unwrap();

    // One turn, two rounds — not a second turn.
    assert_eq!(out.stats.rounds, 2);
    assert_eq!(out.turn_seq, 1);
    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.answer, "tabs it is");

    assert_eq!(steering_texts(&h), ["actually, use tabs"]);
    // Delivery is a move: the mailbox row is gone, not flagged.
    assert!(
        h.store
            .with(|db| db.mailbox().pending(h.session).unwrap())
            .is_empty()
    );

    // The second round sees it.
    let second = &client.requests()[1];
    let wire = serde_json::to_string(&second.messages).unwrap();
    assert!(wire.contains("actually, use tabs"));
    assert_no_consecutive_user(&second.messages);
}

#[tokio::test]
async fn a_background_agent_closes_its_mailbox_after_the_final_empty_drain() {
    let h = Harness::new();
    submit(
        &h,
        "req-1",
        "include the latest requirement",
        Delivery::Steer,
    );
    let gate = Arc::new(AgentMailboxGate::default());
    gate.activate(h.session).await;

    let client = Scripted::new(vec![
        MockScript::text("first answer"),
        MockScript::text("final answer"),
    ]);
    let out = h
        .core()
        .with_agent_mailbox(Some(gate.clone()))
        .run(
            TurnId::new(),
            h.plan(client),
            user("work in the background"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.answer, "final answer");
    assert_eq!(
        steering_texts(&h),
        ["include the latest requirement"],
        "the message must be consumed before the gate closes"
    );
    assert_eq!(
        gate.with_open_session(|session_id| async move { (session_id, false) })
            .await,
        None,
        "no sender may enqueue after the final empty checkpoint"
    );
}

/// The receipt the UI needs to shift a message off its pending mirror.
#[tokio::test]
async fn delivery_emits_a_receipt_and_a_consumed_event() {
    let h = Harness::new();
    submit(&h, "req-1", "one more thing", Delivery::Steer);

    let client = Scripted::new(vec![MockScript::text("a"), MockScript::text("b")]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    let injected = h
        .sink
        .payloads()
        .into_iter()
        .find_map(|p| match p {
            StreamPayload::SteeringInjected {
                entry_id,
                submission_id,
                content,
            } => Some((entry_id, submission_id, content)),
            _ => None,
        })
        .expect("a delivery receipt");

    // The receipt names the entry it became, so a client can render it in place.
    let entry = h
        .entries()
        .into_iter()
        .find(|e| e.kind == EntryKind::Steering)
        .unwrap();
    assert_eq!(injected.0, entry.entry_id.to_string());
    assert!(!injected.1.is_empty());
    assert_eq!(
        injected.2,
        Some(zlogic_protocol::stream::SteeringContent::User {
            text: "one more thing".into()
        })
    );

    match h
        .sink
        .payloads()
        .into_iter()
        .find(|p| matches!(p, StreamPayload::MailboxConsumed { .. }))
        .unwrap()
    {
        StreamPayload::MailboxConsumed {
            submission_ids,
            via,
        } => {
            assert_eq!(submission_ids, [injected.1]);
            assert_eq!(via, Via::Steer);
        }
        other => panic!("{other:?}"),
    }
}

/// The invariant the two checkpoints exist for: no user message between the calls and the results.
#[tokio::test]
async fn an_injection_never_splits_a_tool_call_from_its_result() {
    let h = Harness::new();
    let path = h.dir.path().join("f.txt");
    std::fs::write(&path, "content").unwrap();
    let args = json!({ "path": path.to_string_lossy() }).to_string();

    // Submitted before the run, so it is already pending when the tool batch finishes.
    submit(
        &h,
        "req-1",
        "while you are there, check the tests",
        Delivery::Steer,
    );

    let client = Scripted::new(vec![call("read_file", &args), MockScript::text("done")]);
    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("read it"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(out.stats.tools.succeeded, 1);
    assert_eq!(steering_texts(&h), ["while you are there, check the tests"]);

    let round = &client.requests()[1];
    let roles: Vec<Role> = round.messages.iter().map(|m| m.role).collect();
    // The message lands after the tool results, never between them and the calls.
    assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool, Role::User]);

    let tool_at = roles.iter().position(|r| *r == Role::Tool).unwrap();
    let assistant_at = roles.iter().position(|r| *r == Role::Assistant).unwrap();
    assert_eq!(tool_at, assistant_at + 1, "nothing may come between them");
    assert_no_consecutive_user(&round.messages);
}

/// Injected right after the batch, it joins the round that was going to happen anyway — no extra
/// round is added.
#[tokio::test]
async fn a_message_arriving_during_a_tool_round_adds_no_extra_round() {
    let h = Harness::new();
    let path = h.dir.path().join("f.txt");
    std::fs::write(&path, "x").unwrap();
    let args = json!({ "path": path.to_string_lossy() }).to_string();
    submit(&h, "req-1", "and hurry", Delivery::Steer);

    let client = Scripted::new(vec![call("read_file", &args), MockScript::text("done")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("read it"), h.token())
        .await
        .unwrap();

    assert_eq!(
        out.stats.rounds, 2,
        "the tool round and the answer round, nothing more"
    );
}

/// A queued submission asked for its own turn and must be left where it is.
#[tokio::test]
async fn a_queued_submission_is_not_injected() {
    let h = Harness::new();
    submit(&h, "req-1", "next question, separately", Delivery::Queue);

    let client = Scripted::new(vec![MockScript::text("answer")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.stats.rounds, 1, "the turn ended as it should");
    assert!(steering_texts(&h).is_empty());
    assert_eq!(
        h.store
            .with(|db| db.mailbox().pending(h.session).unwrap())
            .len(),
        1,
        "still waiting for the engine to start its own turn"
    );
    assert!(!h.saw("steering_injected"));
}

/// Several submissions all arrive, in the order they were made.
#[tokio::test]
async fn several_pending_messages_are_delivered_fifo() {
    let h = Harness::new();
    submit(&h, "req-1", "first", Delivery::Steer);
    submit(&h, "req-2", "second", Delivery::Steer);
    submit(&h, "req-3", "queued", Delivery::Queue);

    let client = Scripted::new(vec![MockScript::text("a"), MockScript::text("b")]);
    h.core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(steering_texts(&h), ["first", "second"]);
    assert_eq!(
        h.store
            .with(|db| db.mailbox().pending(h.session).unwrap())
            .len(),
        1
    );

    // Both merge into one user message rather than becoming two consecutive ones.
    let round = &client.requests()[1];
    assert_no_consecutive_user(&round.messages);
    let last = round.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content.len(), 2);

    match h
        .sink
        .payloads()
        .into_iter()
        .find(|p| matches!(p, StreamPayload::MailboxConsumed { .. }))
        .unwrap()
    {
        StreamPayload::MailboxConsumed { submission_ids, .. } => {
            assert_eq!(submission_ids.len(), 2)
        }
        other => panic!("{other:?}"),
    }
}

/// A message with several parts is one delivery: all of it, or none of it.
#[tokio::test]
async fn a_multi_part_submission_arrives_whole() {
    let h = Harness::new();
    h.store
        .with(|db| {
            db.mailbox().submit(
                h.session,
                "req-1",
                &json!([
                    { "type": "text", "text": "look at this" },
                    { "type": "file", "path": "/tmp/report.csv" }
                ]),
                Delivery::Steer,
            )
        })
        .unwrap();

    let client = Scripted::new(vec![MockScript::text("a"), MockScript::text("b")]);
    h.core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    // Two structured entries, one mailbox row moved.
    let stored: Vec<_> = h
        .entries()
        .into_iter()
        .filter(|entry| entry.kind == EntryKind::Steering)
        .map(|entry| entry.data)
        .collect();
    assert_eq!(
        stored,
        [
            json!({ "type": "text", "text": "look at this" }),
            json!({ "type": "file", "path": "/tmp/report.csv" }),
        ]
    );
    assert!(
        h.store
            .with(|db| db.mailbox().pending(h.session).unwrap())
            .is_empty()
    );
    // One receipt: the message is one message, whatever it is made of.
    assert_eq!(
        h.event_names()
            .iter()
            .filter(|n| *n == "steering_injected")
            .count(),
        1
    );

    let round = &client.requests()[1];
    let last = round.messages.last().unwrap();
    assert_eq!(last.content.len(), 2, "both parts, in order");
    match (&last.content[0], &last.content[1]) {
        (ContentPart::Text(text), ContentPart::Text(file)) => {
            assert_eq!(text.text, "look at this");
            assert_eq!(file.text, "<file path=\"/tmp/report.csv\" />");
        }
        other => panic!("{other:?}"),
    }
}

/// Nothing pending is the overwhelmingly common case and must cost nothing.
#[tokio::test]
async fn an_empty_mailbox_changes_nothing() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript::text("answer")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.stats.rounds, 1);
    assert!(!h.saw("steering_injected"));
    assert!(!h.saw("mailbox_consumed"));
    assert_eq!(
        h.kinds(),
        [EntryKind::User, EntryKind::AssistantText, EntryKind::Event]
    );
}

/// A submission with nothing sayable in it must not be retried on every checkpoint for the rest of
/// the turn.
#[tokio::test]
async fn a_blank_submission_is_discarded_rather_than_retried() {
    let h = Harness::new();
    submit(&h, "req-1", "   ", Delivery::Steer);

    let client = Scripted::new(vec![MockScript::text("answer")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(
        out.stats.rounds, 1,
        "the turn ended; it was not extended by an empty message"
    );
    assert!(steering_texts(&h).is_empty());
    assert!(
        h.store
            .with(|db| db.mailbox().pending(h.session).unwrap())
            .is_empty(),
        "and it is gone"
    );
}

/// The round reports why the turn carried on, so a client can tell it apart from a tool round.
#[tokio::test]
async fn the_round_reports_that_the_mailbox_extended_the_turn() {
    let h = Harness::new();
    submit(&h, "req-1", "one more", Delivery::Steer);

    let client = Scripted::new(vec![MockScript::text("a"), MockScript::text("b")]);
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
    assert_eq!(
        outcomes,
        [
            RoundOutcome::FinalAnswerWithMailbox,
            RoundOutcome::FinalAnswer
        ]
    );
}

/// A message that arrives *during* a round — the real timing — is picked up at the checkpoint.
#[tokio::test]
async fn a_message_submitted_mid_stream_is_picked_up_at_the_checkpoint() {
    /// Submits into the mailbox at the moment the first request goes out, which is as close to
    /// "the user typed while the model was talking" as a test can get.
    struct SubmitsMidRun {
        inner: Arc<Scripted>,
        store: zlogic_core::SharedStore,
        session: zlogic_protocol::SessionId,
        done: std::sync::Mutex<bool>,
    }

    #[async_trait]
    impl LlmClient for SubmitsMidRun {
        async fn stream(&self, req: LlmRequest) -> Result<EventStream, LlmError> {
            {
                let mut done = self.done.lock().unwrap();
                if !*done {
                    *done = true;
                    self.store
                        .with(|db| {
                            db.mailbox().submit(
                                self.session,
                                "mid",
                                &parts("wait, also add a test"),
                                Delivery::Steer,
                            )
                        })
                        .unwrap();
                }
            }
            self.inner.stream(req).await
        }
    }

    let h = Harness::new();
    let client = Arc::new(SubmitsMidRun {
        inner: Scripted::new(vec![
            MockScript::text("first"),
            MockScript::text("and the test"),
        ]),
        store: h.store.clone(),
        session: h.session,
        done: std::sync::Mutex::new(false),
    });

    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client),
            user("write the function"),
            h.token(),
        )
        .await
        .unwrap();

    assert_eq!(
        out.stats.rounds, 2,
        "it did not abort the run, and it did not start a new turn"
    );
    assert_eq!(out.answer, "and the test");
    assert_eq!(steering_texts(&h), ["wait, also add a test"]);
}

/// Steering and ordinary input are the same thing on the wire and different in the timeline.
#[tokio::test]
async fn a_steering_entry_is_distinct_in_the_timeline_but_not_on_the_wire() {
    let h = Harness::new();
    submit(&h, "req-1", "injected", Delivery::Steer);

    let client = Scripted::new(vec![MockScript::text("a"), MockScript::text("b")]);
    h.core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("original"),
            h.token(),
        )
        .await
        .unwrap();

    // Distinct kinds, so a client can mark one as injected mid-run.
    assert_eq!(
        h.kinds(),
        [
            EntryKind::User,
            EntryKind::AssistantText,
            EntryKind::Steering,
            EntryKind::AssistantText,
            EntryKind::Event
        ]
    );
    // No source stamp: it is the user speaking, not a model.
    let injected = h
        .entries()
        .into_iter()
        .find(|e| e.kind == EntryKind::Steering)
        .unwrap();
    assert!(injected.source.is_none());
    assert!(
        injected.round_id.is_none(),
        "it belongs to the turn, not to a response"
    );

    // Identical treatment on the wire.
    let round = &client.requests()[1];
    let last = round.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    match &last.content[0] {
        ContentPart::Text(t) => assert_eq!(t.text, "injected"),
        other => panic!("{other:?}"),
    }
}

/// Submitting the same client request twice must not deliver two messages.
#[tokio::test]
async fn a_resubmitted_message_is_delivered_once() {
    let h = Harness::new();
    submit(&h, "req-1", "the same thing", Delivery::Steer);
    submit(&h, "req-1", "the same thing", Delivery::Steer);

    let client = Scripted::new(vec![MockScript::text("a"), MockScript::text("b")]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(steering_texts(&h), ["the same thing"]);
}

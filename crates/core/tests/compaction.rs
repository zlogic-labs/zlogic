//! Compaction end to end: what is written, what the model then sees, and what is left alone.

mod common;

use zlogic_protocol::TurnId;

use common::*;
use zlogic_core::{ContextPolicy, Summary, compact::SUMMARY_ATTEMPTS, compact::is_summary_message};
use zlogic_llm::mock::MockScript;
use zlogic_protocol::llm::{LlmError, LlmErrorKind};
use zlogic_protocol::message::{ContentPart, Role};
use zlogic_protocol::stream::{CompactionReason, StreamPayload};
use zlogic_protocol::usage::{Purpose, TokenUsage};
use zlogic_store::EntryKind;

/// Compacts as soon as there is anything old enough, so tests do not need long histories.
fn eager() -> ContextPolicy {
    ContextPolicy {
        compact_ratio: 0.5,
        tail_turns: 1,
        overflow_retries: 1,
    }
}

/// A threshold high enough that it never fires, for the tests that drive compaction purely from a
/// provider's `context_length_exceeded`.
fn overflow_only() -> ContextPolicy {
    ContextPolicy {
        compact_ratio: 0.99,
        tail_turns: 1,
        overflow_retries: 1,
    }
}

fn summaries(h: &Harness) -> Vec<Summary> {
    h.entries()
        .iter()
        .filter(|e| e.kind == EntryKind::Compaction)
        .map(|e| serde_json::from_value(e.data.clone()).unwrap())
        .collect()
}

/// Builds `n` completed turns.
/// Only the **last** reports heavy usage, so the next turn is the one that compacts. The trigger
/// reads the most recent main figure only, so making every turn heavy would compact during setup —
/// and each of those compactions would consume a script the test meant for the round.
async fn history(h: &Harness, n: usize) {
    let core = h.core();
    for i in 1..=n {
        let tokens = if i == n { 90_000 } else { 10 };
        let client = Scripted::new(vec![answer_using(&format!("answer {i}"), tokens)]);
        core.run(
            TurnId::new(),
            h.plan(client),
            user(&format!("question {i}")),
            h.token(),
        )
        .await
        .unwrap();
    }
}

/// Turns that leave the context comfortably small.
async fn light_history(h: &Harness, n: usize) {
    let core = h.core();
    for i in 1..=n {
        let client = Scripted::new(vec![answer_using(&format!("answer {i}"), 10)]);
        core.run(
            TurnId::new(),
            h.plan(client),
            user(&format!("question {i}")),
            h.token(),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn manual_compaction_is_a_non_conversational_turn() {
    let h = Harness::new().context(eager());
    light_history(&h, 3).await;
    let users_before = h
        .entries()
        .iter()
        .filter(|entry| entry.kind == EntryKind::User)
        .count();
    let client = Scripted::new(vec![MockScript::text("manual summary")]);

    let outcome = h
        .core()
        .compact_context(TurnId::new(), h.plan(client.clone()), h.token())
        .await
        .unwrap();

    assert_eq!(
        client.request_count(),
        1,
        "only the summary model is called"
    );
    assert_eq!(
        outcome.answer, "",
        "manual compaction has no assistant reply"
    );
    assert_eq!(outcome.stats.compactions, 1);
    assert_eq!(
        h.entries()
            .iter()
            .filter(|entry| entry.kind == EntryKind::User)
            .count(),
        users_before,
        "the command must not manufacture a user message"
    );
    let summary = summaries(&h).pop().expect("summary entry");
    assert_eq!(summary.content, "manual summary");
    assert_eq!(summary.reason, CompactionReason::Manual);
}

/// `/compact` on a session that has nothing to summarise has to say so.
/// The automatic trigger is silenced on purpose — a threshold a long session crosses many times
/// would otherwise narrate every short conversation. A typed command is the opposite case: writing
/// nothing and saying nothing is indistinguishable from a broken command.
#[tokio::test]
async fn manual_compaction_reports_when_there_is_nothing_to_do() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript::text("unused")]);

    let outcome = h
        .core()
        .compact_context(TurnId::new(), h.plan(client.clone()), h.token())
        .await
        .unwrap();

    assert_eq!(outcome.stats.compactions, 0);
    assert_eq!(client.request_count(), 0, "no model call to make");
    assert!(
        h.notices()
            .iter()
            .any(|(code, _)| code == "compaction_nothing_to_do"),
        "{:?}",
        h.notices()
    );

    // The same state reached by the threshold trigger stays quiet. Recorded usage puts the session
    // over the line, but the newest turn is still inside the protected tail, so `compact` returns
    // early — and that is the state a long session sits in round after round.
    let h = Harness::new();
    h.store
        .with(|db| {
            db.usage().record(zlogic_store::NewUsage::new(
                h.session,
                Purpose::Main,
                zlogic_protocol::usage::TokenUsage {
                    input: 120_000,
                    output: 10,
                    ..Default::default()
                },
            ))
        })
        .unwrap();
    let client = Scripted::new(vec![MockScript::text("ok")]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("hi"), h.token())
        .await
        .unwrap();
    assert!(summaries(&h).is_empty());
    assert!(
        !h.notices()
            .iter()
            .any(|(code, _)| code == "compaction_nothing_to_do"),
        "an ordinary short conversation must not be told about compaction: {:?}",
        h.notices()
    );
}

#[tokio::test]
async fn a_summary_is_appended_and_nothing_is_deleted() {
    let h = Harness::new().context(eager());
    // Turns under the threshold, so nothing compacts yet.
    light_history(&h, 2).await;
    assert!(summaries(&h).is_empty());
    let before = h.entries().len();

    // One more turn, reporting enough input to cross 50% of the 100k window.
    history(&h, 1).await;
    let compact_client = Scripted::new(vec![MockScript::text("the summary")]);
    h.core()
        .run(
            TurnId::new(),
            h.plan(compact_client),
            user("carry on"),
            h.token(),
        )
        .await
        .unwrap();

    let s = summaries(&h);
    assert_eq!(s.len(), 1, "one summary entry");
    assert_eq!(s[0].content, "the summary");
    assert_eq!(s[0].reason, CompactionReason::Threshold);
    assert_eq!(s[0].model_ref.as_deref(), Some("mock:m1"));
    assert_eq!(s[0].from_turn, 1, "starts at the oldest turn");

    // The point of the design: the turns it covers are still there, untouched.
    assert!(h.entries().len() > before);
    let users: Vec<String> = h
        .entries()
        .iter()
        .filter(|e| e.kind == EntryKind::User)
        .map(|e| e.data["text"].as_str().unwrap().to_string())
        .collect();
    assert!(
        users.contains(&"question 1".to_string()),
        "the real message survives verbatim"
    );

    assert!(h.saw("compaction_start"));
    assert!(h.saw("compaction_end"));
}

/// The covered turns are replaced *in the context*, in the position they occupied.
#[tokio::test]
async fn the_model_sees_the_summary_instead_of_the_covered_turns() {
    // tail_turns 2, so the protected tail holds a whole exchange and this can check that it is
    // still structured rather than flattened into the summary.
    let h = Harness::new().context(ContextPolicy {
        compact_ratio: 0.5,
        tail_turns: 2,
        overflow_retries: 1,
    });
    history(&h, 3).await;

    let client = Scripted::new(vec![
        MockScript::text("[the summary]"),
        MockScript::text("ok"),
    ]);
    h.core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("what now?"),
            h.token(),
        )
        .await
        .unwrap();

    // Two requests: the summarisation call, then the round itself.
    assert_eq!(client.request_count(), 2);

    let summary_request = &client.requests()[0];
    assert_eq!(summary_request.meta.purpose, Purpose::Compaction);
    assert!(!summary_request.messages.is_empty());

    // The round's message list is where the summary has to sit.
    let round = &client.requests()[1];

    // **The summarising call rides the conversation's own prefix.** Same system prompt, same tool
    // definitions, same opening messages — anything that differs here is a cache miss, i.e. the
    // whole range re-billed at full price instead of read at 0.1x.
    assert_eq!(
        summary_request.system, round.system,
        "the system prompt is the first thing in a cached prefix"
    );
    assert_eq!(
        summary_request.tools, round.tools,
        "the tool definitions are part of it"
    );

    // …and it is that prefix **cut off where the protected tail begins**: the covered turns are
    // there, the tail is not. Feeding the tail to the summariser would only invite it to restate
    // what stays verbatim, and the same recent facts would then sit in the context twice.
    let wire = serde_json::to_string(&summary_request.messages).unwrap();
    assert!(
        wire.contains("question 1") && wire.contains("answer 1") && wire.contains("question 2"),
        "the covered turns are the request: {wire}"
    );
    assert!(
        !wire.contains("question 3") && !wire.contains("answer 3"),
        "the protected tail must not be sent: {wire}"
    );
    assert!(
        !wire.contains("what now?"),
        "and neither must the turn in flight: {wire}"
    );

    // The instruction is a **user** message at the very end, and the last thing the model reads.
    let instruction = summary_request.messages.last().unwrap();
    assert_eq!(instruction.role, Role::User);
    let text = serde_json::to_string(&instruction.content).unwrap();
    assert!(text.contains("Summarise the conversation above"), "{text}");
    assert!(
        !text.contains("turn"),
        "no internal turn bookkeeping reaches the model: {text}"
    );
    assert!(
        text.contains("Do not call tools"),
        "the request carries the tool definitions, so it has to say not to use them: {text}"
    );
    assert_no_consecutive_user(&summary_request.messages);

    // The summary is first, then the protected tail, then this turn's question — and the summary
    // merged with nothing before it.
    assert!(is_summary_message(&round.messages[0]));
    match &round.messages[0].content[0] {
        ContentPart::Text(t) => {
            assert!(t.text.contains("[the summary]"));
            assert!(
                t.text.contains("turns=\"1-2\""),
                "the range travels with it: {}",
                t.text
            );
        }
        other => panic!("{other:?}"),
    }
    // The covered questions are gone from the wire, and the tail is still structured.
    let wire = serde_json::to_string(&round.messages).unwrap();
    assert!(!wire.contains("question 1"), "turn 1 was summarised away");
    assert!(
        wire.contains("question 3") && wire.contains("answer 3"),
        "the tail is intact"
    );
    assert!(wire.contains("what now?"), "and so is the current turn");
    assert!(
        round.messages.iter().any(|m| m.role == Role::Assistant),
        "the tail is structured, not flattened into prose"
    );
    assert_no_consecutive_user(&round.messages);
}

/// The tail is what keeps an in-flight tool chain replayable, so it is never covered.
#[tokio::test]
async fn the_protected_tail_is_never_covered() {
    let h = Harness::new().context(ContextPolicy {
        compact_ratio: 0.5,
        tail_turns: 3,
        overflow_retries: 1,
    });
    history(&h, 5).await;

    let client = Scripted::new(vec![MockScript::text("summary"), MockScript::text("ok")]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("next"), h.token())
        .await
        .unwrap();

    let s = &summaries(&h)[0];
    // Six turns exist (5 plus the current one); three are protected.
    assert_eq!((s.from_turn, s.to_turn), (1, 3));
}

/// A summary that came back empty would erase the covered turns and put nothing there.
#[tokio::test]
async fn an_empty_summary_is_discarded() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;

    let client = Scripted::new(vec![
        MockScript::text("   \n  "),
        // An empty reply is regenerated, so the second draw has to be empty as well.
        MockScript::text("   \n  "),
        MockScript::text("answered anyway"),
    ]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert!(summaries(&h).is_empty(), "nothing was written");
    assert_eq!(out.stats.compactions, 0);
    assert_eq!(out.answer, "answered anyway", "and the turn carried on");
    assert!(
        h.notices()
            .iter()
            .any(|(code, _)| code == "compaction_empty")
    );

    // The history the model gets is the real one, uncompacted.
    let wire = serde_json::to_string(&client.requests()[1].messages).unwrap();
    assert!(wire.contains("question 1"));
}

/// The riding shape sends the conversation's tool definitions, so a model can answer with a call
/// and no text at all. That is reported as what it is, not as an empty reply.
#[tokio::test]
async fn a_summary_call_that_answers_with_a_tool_call_is_reported_as_such() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;
    // `read` is a real built-in, so the call itself is well-formed — the point is the shape of the
    // reply, not the tool.
    let client = Scripted::new(vec![
        call("read", "{\"path\":\"/tmp/x\"}"),
        // The reply is regenerated, so the second draw has to be a tool call as well.
        call("read", "{\"path\":\"/tmp/y\"}"),
        MockScript::text("answered anyway"),
    ]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert!(summaries(&h).is_empty());
    assert_eq!(out.stats.compactions, 0);
    assert_eq!(out.answer, "answered anyway");
    assert!(
        h.notices()
            .iter()
            .any(|(code, m)| code == "compaction_tool_call" && m.contains("tool call")),
        "{:?}",
        h.notices()
    );
}

/// What the model writes around the summary is kept, but a wrapper over the **whole** reply is not
/// part of it: replayed verbatim it would be stray backticks in the middle of the conversation.
#[tokio::test]
async fn a_wrapper_around_the_whole_reply_is_not_stored_as_part_of_the_summary() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;

    let client = Scripted::new(vec![
        MockScript::text(
            "```markdown\n## Requirements\n- Fix the login\n\nDrop the branch that hard-codes the session.\n```",
        ),
        MockScript::text("ok"),
    ]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    let summary = summaries(&h).pop().expect("summary entry");
    assert_eq!(
        summary.content,
        "## Requirements\n- Fix the login\n\nDrop the branch that hard-codes the session."
    );
    assert!(
        !summary.content.contains("```"),
        "the wrapper is not part of what the model wrote: {}",
        summary.content
    );
}

/// A truncated reply is **regenerated**, not saved half-finished — and when the regeneration fails
/// too, the failure is reported instead of the conversation being replaced by something incomplete.
#[tokio::test]
async fn a_truncated_summary_is_regenerated_and_reported_if_that_fails_too() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;

    let cut_off = || MockScript {
        text: Some("turns 1-3 were about…".into()),
        // The provider stopped because it ran out of output room.
        finish: Some(zlogic_protocol::llm::FinishReason::Length),
        // Reported per attempt, so the retry is on the bill like the original.
        usage: Some(TokenUsage {
            input: 90_000,
            output: 900,
            ..Default::default()
        }),
        ..Default::default()
    };
    let client = Scripted::new(vec![
        cut_off(),
        cut_off(),
        MockScript::text("answered anyway"),
    ]);

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    // Two attempts at the summary (the retry is the second call), then the round itself.
    assert_eq!(client.request_count(), 3, "the summary was asked for again");
    let retry = &client.requests()[1];
    let first = &client.requests()[0];
    assert_eq!(retry.meta.purpose, Purpose::Compaction);
    assert_eq!(
        serde_json::to_string(&retry.messages).unwrap(),
        serde_json::to_string(&first.messages).unwrap(),
        "a regeneration is the same request, not a patched one"
    );
    assert_ne!(
        retry.meta.round_id, first.meta.round_id,
        "each attempt is its own round: two real requests, two usage rows"
    );

    assert!(
        summaries(&h).is_empty(),
        "nothing half-finished was written"
    );
    assert_eq!(out.stats.compactions, 0);
    assert_eq!(out.answer, "answered anyway", "the turn carried on");

    // Both attempts are on the bill.
    let rows = h
        .store
        .with(|db| db.usage().list_for_session(h.session).unwrap());
    assert_eq!(
        rows.iter()
            .filter(|r| r.purpose == Purpose::Compaction)
            .count(),
        SUMMARY_ATTEMPTS as usize
    );

    // The user is told, and told *why*, in one sentence that includes the retry.
    let (code, message) = h
        .notices()
        .into_iter()
        .find(|(code, _)| code.starts_with("compaction_"))
        .expect("a notice");
    assert_eq!(code, "compaction_truncated");
    assert!(
        message.contains("cut off") && message.contains("on all 2 attempts"),
        "{message}"
    );

    // And the history the model gets is the real one, uncompacted.
    let wire = serde_json::to_string(&client.requests()[2].messages).unwrap();
    assert!(wire.contains("question 1"));
}

/// A regeneration that works is enough: one bad draw does not cost the compaction.
#[tokio::test]
async fn a_retry_that_produces_a_real_summary_is_used() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;

    let client = Scripted::new(vec![
        // First draw: cut off by the output limit.
        MockScript {
            text: Some("half a summ".into()),
            finish: Some(zlogic_protocol::llm::FinishReason::Length),
            ..Default::default()
        },
        // Second draw: a complete one.
        MockScript::text("a real summary"),
        MockScript::text("ok"),
    ]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.stats.compactions, 1);
    let summary = summaries(&h).pop().expect("summary entry");
    assert_eq!(summary.content, "a real summary");
    assert!(
        !h.notices()
            .iter()
            .any(|(code, _)| code == "compaction_truncated"),
        "a recovered attempt is not a failure the user has to hear about: {:?}",
        h.notices()
    );
}

/// A threshold is a soft signal: a failed summary must not fail the turn.
#[tokio::test]
async fn a_failed_summary_does_not_fail_the_turn() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;

    let client = Scripted::new(vec![
        MockScript {
            fail_with: Some(LlmError {
                kind: LlmErrorKind::Server,
                retryable: true,
                message: "upstream broke".into(),
                status: Some(500),
                request_id: None,
            }),
            ..Default::default()
        },
        MockScript::text("still answered"),
    ]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, zlogic_protocol::stream::TurnStatus::Completed);
    assert_eq!(out.answer, "still answered");
    assert!(
        h.notices()
            .iter()
            .any(|(code, m)| code == "compaction_failed" && m.contains("broke"))
    );
}

/// The other trigger: the provider says the request is too long.
#[tokio::test]
async fn a_context_overflow_compacts_and_retries_once() {
    let h = Harness::new().context(overflow_only());
    // No threshold pressure at all — this must be driven purely by the provider's refusal.
    light_history(&h, 3).await;

    let overflow = || MockScript {
        fail_with: Some(LlmError {
            kind: LlmErrorKind::ContextLengthExceeded,
            retryable: false,
            message: "too long".into(),
            status: Some(400),
            request_id: None,
        }),
        ..Default::default()
    };
    let client = Scripted::new(vec![
        overflow(),                           // the round's first attempt
        MockScript::text("a forced summary"), // the summarising call
        MockScript::text("fits now"),         // the retry
    ]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, zlogic_protocol::stream::TurnStatus::Completed);
    assert_eq!(out.answer, "fits now");
    assert_eq!(out.stats.compactions, 1);

    let s = summaries(&h);
    assert_eq!(s[0].reason, CompactionReason::ContextOverflow);
    assert_eq!(client.request_count(), 3);

    // **The overflow retry cannot ride the conversation's prefix.** The request that just came
    // back "too long" is the one the summary would have to resend, plus one more message — so this
    // is the one trigger that builds a request of its own, covering the range alone.
    let failed = &client.requests()[0];
    let summary_request = &client.requests()[1];
    assert_eq!(summary_request.meta.purpose, Purpose::Compaction);
    assert!(
        summary_request.tools.is_empty(),
        "a request of its own carries no tool definitions"
    );
    assert!(
        summary_request
            .system
            .iter()
            .all(|p| !p.text.contains("you are a test")),
        "and not the conversation's system prompt either: {:?}",
        summary_request.system
    );
    assert!(
        summary_request.messages.len() < failed.messages.len(),
        "the summary call has to send strictly less than the request that did not fit ({} vs {})",
        summary_request.messages.len(),
        failed.messages.len()
    );
}

/// A second overflow is not retried again.
#[tokio::test]
async fn a_second_overflow_is_not_retried_again() {
    let h = Harness::new().context(overflow_only());
    light_history(&h, 3).await;

    let overflow = || MockScript {
        fail_with: Some(LlmError {
            kind: LlmErrorKind::ContextLengthExceeded,
            retryable: false,
            message: "too long".into(),
            status: Some(400),
            request_id: None,
        }),
        ..Default::default()
    };
    let client = Scripted::new(vec![overflow(), MockScript::text("a summary"), overflow()]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, zlogic_protocol::stream::TurnStatus::Failed);
    assert!(h.saw("error"));
}

/// A tail that itself exceeds the window is reported once at threshold time, before any
/// with a Completed status while the request never shrank.
#[tokio::test]
async fn threshold_compaction_that_cannot_fit_fails_the_turn_immediately() {
    let h = Harness::new().context(ContextPolicy {
        compact_ratio: 0.5,
        tail_turns: 1,
        overflow_retries: 1,
    });
    light_history(&h, 3).await;
    // Near-window signal: threshold (50k) fires, but even removing the light prefix cannot
    // bring the estimate below the 100k window — the protected tail is the whole problem.
    h.store
        .with(|db| {
            db.usage().record(zlogic_store::NewUsage::new(
                h.session,
                Purpose::Main,
                zlogic_protocol::usage::TokenUsage {
                    input: 120_000,
                    output: 10,
                    ..Default::default()
                },
            ))
        })
        .unwrap();

    // Only the summarisation call is consumed — the turn must fail at the post-condition,
    // never issuing the main request at all.
    let client = Scripted::new(vec![MockScript::text("a summary")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, zlogic_protocol::stream::TurnStatus::Failed);
    assert_eq!(
        client.request_count(),
        1,
        "no main request after the hard error"
    );
    assert_eq!(summaries(&h).len(), 1);
    let error = h
        .sink
        .payloads()
        .into_iter()
        .filter_map(|p| match p {
            StreamPayload::Error { error } => Some(error.to_string()),
            _ => None,
        })
        .next()
        .expect("an error was streamed");
    assert!(
        error.contains("cannot be compacted below the model window"),
        "{error}"
    );
}

/// With nothing old enough to summarise, retrying would send exactly the same request.
#[tokio::test]
async fn an_overflow_with_nothing_to_compact_fails_immediately() {
    let h = Harness::new();
    let client = Scripted::new(vec![MockScript {
        fail_with: Some(LlmError {
            kind: LlmErrorKind::ContextLengthExceeded,
            retryable: false,
            message: "too long".into(),
            status: Some(400),
            request_id: None,
        }),
        ..Default::default()
    }]);

    let out = h
        .core()
        .run(
            TurnId::new(),
            h.plan(client.clone()),
            user("a single enormous message"),
            h.token(),
        )
        .await
        .unwrap();
    assert_eq!(out.status, zlogic_protocol::stream::TurnStatus::Failed);
    assert_eq!(client.request_count(), 1, "no pointless retry");
    assert!(summaries(&h).is_empty());
}

/// A summary's own cost must never make the next turn compact again.
#[tokio::test]
async fn the_summary_call_is_attributed_to_compaction() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;

    let client = Scripted::new(vec![
        MockScript {
            text: Some("summary".into()),
            usage: Some(TokenUsage {
                input: 40_000,
                output: 500,
                ..Default::default()
            }),
            ..Default::default()
        },
        answer_using("ok", 2_000),
    ]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    let rows = h
        .store
        .with(|db| db.usage().list_for_session(h.session).unwrap());
    let compaction: Vec<_> = rows
        .iter()
        .filter(|r| r.purpose == Purpose::Compaction)
        .collect();
    assert_eq!(compaction.len(), 1);
    assert_eq!(compaction[0].tokens.input, 40_000);

    // The trigger reads only the main conversation, so the last figure is the round's, not the
    // summary's.
    assert_eq!(
        h.store
            .with(|db| db.usage().last_main_input_tokens(h.session).unwrap()),
        Some(2_000)
    );
}

/// Summaries chain: the second starts where the first stopped, and both are rendered.
#[tokio::test]
async fn a_second_compaction_starts_where_the_first_stopped() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;

    let first = Scripted::new(vec![
        MockScript::text("summary A"),
        answer_using("ok", 90_000),
    ]);
    h.core()
        .run(TurnId::new(), h.plan(first), user("go"), h.token())
        .await
        .unwrap();
    let a = summaries(&h);
    assert_eq!(a.len(), 1);

    let second = Scripted::new(vec![MockScript::text("summary B"), MockScript::text("ok")]);
    h.core()
        .run(
            TurnId::new(),
            h.plan(second.clone()),
            user("again"),
            h.token(),
        )
        .await
        .unwrap();

    let all = summaries(&h);
    assert_eq!(all.len(), 2, "notices: {:?}", h.notices());
    assert_eq!(
        all[1].from_turn,
        all[0].to_turn + 1,
        "disjoint and adjacent"
    );

    // Both reach the model, oldest first — as **one** user message with two parts, not two
    // messages. Every turn between them was summarised away, so there is no assistant message
    // separating them, and two consecutive user messages are exactly what several providers reject.
    let round = &second.requests()[1];
    assert_no_consecutive_user(&round.messages);

    let first_message = &round.messages[0];
    assert!(is_summary_message(first_message));
    let text = serde_json::to_string(&first_message.content).unwrap();
    assert!(text.contains("summary A") && text.contains("summary B"));
    assert!(
        text.find("summary A").unwrap() < text.find("summary B").unwrap(),
        "oldest first"
    );
}

/// The first round of a session has no usage figure, and `None` is not zero.
#[tokio::test]
async fn a_fresh_session_does_not_compact() {
    let h = Harness::new().context(ContextPolicy {
        compact_ratio: 0.1,
        ..eager()
    });
    let client = Scripted::new(vec![MockScript::text("hello")]);
    h.core()
        .run(TurnId::new(), h.plan(client.clone()), user("hi"), h.token())
        .await
        .unwrap();

    assert!(summaries(&h).is_empty());
    assert_eq!(
        client.request_count(),
        1,
        "no summarising call on the first round"
    );
}

/// Nothing old enough is an ordinary outcome, not a failure.
#[tokio::test]
async fn nothing_old_enough_is_skipped_silently() {
    let h = Harness::new().context(ContextPolicy {
        compact_ratio: 0.5,
        tail_turns: 4,
        overflow_retries: 1,
    });
    history(&h, 1).await;

    let client = Scripted::new(vec![MockScript::text("ok")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert!(summaries(&h).is_empty());
    assert_eq!(out.stats.compactions, 0);
    assert!(
        !h.saw("compaction_start"),
        "nothing started, so nothing is reported as started"
    );
    assert!(
        !h.notices()
            .iter()
            .any(|(code, _)| code == "compaction_skipped"),
        "a skipped compaction is ordinary and emits no notice"
    );
}

/// A dedicated aux model writes the summary, and the row says which one did.
#[tokio::test]
async fn a_separate_model_can_write_the_summary() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;

    let small = Scripted::new(vec![MockScript {
        text: Some("cheap summary".into()),
        usage: Some(TokenUsage {
            input: 30_000,
            output: 200,
            ..Default::default()
        }),
        ..Default::default()
    }]);
    let main = Scripted::new(vec![MockScript::text("ok")]);
    let plan = h
        .plan(main)
        .with_compaction_model(aux(other_model("m-small"), small.clone()));

    h.core()
        .run(TurnId::new(), plan, user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(summaries(&h)[0].model_ref.as_deref(), Some("mock:m-small"));
    assert_eq!(
        small.request_count(),
        1,
        "the aux model did the summarising"
    );
    // A different model has a different cache, so there is nothing to ride: the call is a request
    // of its own — no tool definitions, its own system prompt.
    let summary_request = &small.requests()[0];
    assert!(
        summary_request.tools.is_empty(),
        "another model cannot read this conversation's cache, so it is not sent its prefix"
    );
    assert!(
        summary_request
            .system
            .iter()
            .all(|p| !p.text.contains("you are a test"))
    );

    let rows = h
        .store
        .with(|db| db.usage().list_for_session(h.session).unwrap());
    assert!(rows.iter().any(
        |r| r.purpose == Purpose::Compaction && r.model_ref.as_deref() == Some("mock:m-small")
    ));
}

/// A compaction entry is timeline material; the summary reaching the model is a separate thing.
#[tokio::test]
async fn the_compaction_entry_is_visible_in_the_timeline() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;
    let client = Scripted::new(vec![MockScript::text("s"), MockScript::text("ok")]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    let entry = h
        .entries()
        .into_iter()
        .find(|e| e.kind == EntryKind::Compaction)
        .unwrap();
    assert_eq!(entry.data["from_turn"], 1);
    assert_eq!(entry.data["content"], "s");
    assert!(entry.source.is_none(), "not a model response");
    assert!(entry.round_id.is_none(), "not part of a conversation round");
}

/// The event carries the range so a client can show "turns 1-3 were summarised".
#[tokio::test]
async fn the_end_event_reports_the_range_it_replaced() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;
    let client = Scripted::new(vec![MockScript::text("s"), MockScript::text("ok")]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    match h
        .sink
        .payloads()
        .into_iter()
        .find(|p| matches!(p, StreamPayload::CompactionEnd { .. }))
        .unwrap()
    {
        StreamPayload::CompactionEnd { replaces, .. } => assert_eq!(replaces, (1, 3)),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_stream_and_the_stored_entry_agree_on_the_summary_cost() {
    let h = Harness::new().context(eager());
    history(&h, 3).await;
    let client = Scripted::new(vec![
        MockScript {
            text: Some("summary".into()),
            usage: Some(TokenUsage {
                input: 900,
                output: 42,
                ..Default::default()
            }),
            ..Default::default()
        },
        MockScript::text("ok"),
    ]);
    h.core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();

    let (streamed_tokens, streamed_summary) = match h
        .sink
        .payloads()
        .into_iter()
        .find(|p| matches!(p, StreamPayload::CompactionEnd { .. }))
        .unwrap()
    {
        StreamPayload::CompactionEnd {
            summary_tokens,
            summary,
            ..
        } => (summary_tokens, summary),
        other => panic!("{other:?}"),
    };

    let entry = h
        .entries()
        .into_iter()
        .find(|e| e.kind == EntryKind::Compaction)
        .unwrap();

    assert_eq!(streamed_tokens, Some(42));
    assert_eq!(entry.data["summary_tokens"], 42, "{}", entry.data);
    assert_eq!(streamed_summary, "summary");
    assert_eq!(
        entry.data["content"], "summary",
        "the same piece of text, not two copies"
    );
    assert_eq!(entry.data["reason"], "threshold");
}

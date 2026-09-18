mod common;

use std::sync::Arc;

use common::*;
use zlogic_core::budget::TurnBudget;
use zlogic_llm::mock::MockScript;
use zlogic_protocol::TurnId;
use zlogic_protocol::interaction::{FormAnswer, InteractionDecision};
use zlogic_protocol::llm::FinishReason;
use zlogic_protocol::settings::{BudgetAction, BudgetConfig, CostConfig};
use zlogic_protocol::stream::{NoticeLevel, StreamPayload, TurnStatus};
use zlogic_protocol::usage::TokenUsage;

fn priced_call(id: &str, path: &std::path::Path) -> MockScript {
    let args = serde_json::json!({ "path": path.to_string_lossy(), "content": "x\n" }).to_string();
    MockScript {
        tool_calls: vec![(0, id.into(), "write_file".into(), args)],
        finish: Some(FinishReason::ToolCalls),
        usage: Some(TokenUsage {
            input: 100_000,
            output: 10,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn budget(config: BudgetConfig) -> Arc<TurnBudget> {
    Arc::new(TurnBudget::new(config, CostConfig::default()))
}

fn per_turn(limit: f64, on_exceeded: BudgetAction) -> BudgetConfig {
    BudgetConfig {
        per_turn: Some(limit),
        on_exceeded,
        ..Default::default()
    }
}

#[tokio::test]
async fn an_exceeded_stop_budget_ends_the_turn_at_the_round_boundary() {
    let h = Harness::new();
    let client = Scripted::new(vec![
        priced_call("c1", &h.dir.path().join("a.txt")),
        MockScript::text("should never be asked for"),
    ]);
    let mut plan = h.plan(client.clone());
    plan.budget = Some(budget(per_turn(0.01, BudgetAction::Stop)));

    let out = h
        .core()
        .run(TurnId::new(), plan, user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::LimitReached);
    assert_eq!(out.stats.rounds, 1, "no second round may be sent");
    assert_eq!(client.request_count(), 1);

    let reason = h
        .sink
        .payloads()
        .iter()
        .find_map(|p| match p {
            StreamPayload::TurnEnd { reason, .. } => reason.clone(),
            _ => None,
        })
        .expect("TurnEnd must carry a reason");
    assert!(reason.contains("budget limit"), "{reason}");
    assert!(
        reason.contains("0.01"),
        "the limit must be stated: {reason}"
    );

    let notices: Vec<zlogic_store::EntryRecord> = h
        .entries()
        .into_iter()
        .filter(|e| e.data["code"] == "limit_reached")
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "hitting the budget must persist one notice"
    );
    assert!(
        notices[0].data["message"]["fallback"]
            .as_str()
            .unwrap()
            .contains("budget limit")
    );
}

#[tokio::test]
async fn choosing_continue_waives_that_budget_for_the_rest_of_the_turn() {
    let h_answers = Answers::new(InteractionDecision::Submitted(FormAnswer::single_choice(
        "action", "continue",
    )));
    let h = Harness::new().interaction(h_answers.clone());
    let client = Scripted::new(vec![
        priced_call("c1", &h.dir.path().join("a.txt")),
        priced_call("c2", &h.dir.path().join("b.txt")),
        MockScript::text("done"),
    ]);
    let mut plan = h.plan(client.clone());
    plan.budget = Some(budget(per_turn(0.01, BudgetAction::Ask)));

    let out = h
        .core()
        .run(TurnId::new(), plan, user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.answer, "done");
    assert_eq!(
        h_answers.count(),
        1,
        "one question per round is enough for one budget"
    );
    assert_eq!(out.stats.interactions, 1);
}

#[tokio::test]
async fn choosing_stop_ends_the_turn() {
    let answers = Answers::new(InteractionDecision::Submitted(FormAnswer::single_choice(
        "action", "stop",
    )));
    let h = Harness::new().interaction(answers.clone());
    let client = Scripted::new(vec![
        priced_call("c1", &h.dir.path().join("a.txt")),
        MockScript::text("unreachable"),
    ]);
    let mut plan = h.plan(client.clone());
    plan.budget = Some(budget(per_turn(0.01, BudgetAction::Ask)));

    let out = h
        .core()
        .run(TurnId::new(), plan, user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::LimitReached);
    assert_eq!(answers.count(), 1);
    assert_eq!(client.request_count(), 1);
}

#[tokio::test]
async fn ask_without_an_interaction_port_fails_closed_to_stop() {
    let h = Harness::new(); // no interaction
    let client = Scripted::new(vec![
        priced_call("c1", &h.dir.path().join("a.txt")),
        MockScript::text("unreachable"),
    ]);
    let mut plan = h.plan(client.clone());
    plan.budget = Some(budget(per_turn(0.01, BudgetAction::Ask)));

    let out = h
        .core()
        .run(TurnId::new(), plan, user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::LimitReached);
    assert_eq!(client.request_count(), 1);
}

#[tokio::test]
async fn warn_notifies_once_and_lets_the_turn_finish() {
    let h = Harness::new();
    let client = Scripted::new(vec![
        priced_call("c1", &h.dir.path().join("a.txt")),
        priced_call("c2", &h.dir.path().join("b.txt")),
        MockScript::text("done"),
    ]);
    let mut plan = h.plan(client.clone());
    plan.budget = Some(budget(per_turn(0.01, BudgetAction::Warn)));

    let out = h
        .core()
        .run(TurnId::new(), plan, user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    let warnings: Vec<String> = h
        .sink
        .payloads()
        .iter()
        .filter_map(|p| match p {
            StreamPayload::Notice {
                level,
                code,
                message,
            } if *level == NoticeLevel::Warn && code == "budget" => Some(message.fallback.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("budget"), "{}", warnings[0]);
}

#[tokio::test]
async fn turn_stats_accumulate_cost_across_rounds() {
    let h = Harness::new();
    let client = Scripted::new(vec![
        priced_call("c1", &h.dir.path().join("a.txt")),
        priced_call("c2", &h.dir.path().join("b.txt")),
        MockScript::text("done"),
    ]);
    let mut plan = h.plan(client.clone());
    plan.budget = Some(budget(BudgetConfig::default()));

    let out = h
        .core()
        .run(TurnId::new(), plan, user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    let cost = out
        .stats
        .cost
        .expect("this field must have a value now that it is filled in");
    assert!((cost.amount - 0.6003).abs() < 1e-9, "{}", cost.amount);
    assert_eq!(cost.currency, "USD");
    assert!(cost.unconverted.is_empty());
}

#[tokio::test]
async fn without_a_budget_nothing_changes() {
    let h = Harness::new();
    let client = Scripted::new(vec![
        priced_call("c1", &h.dir.path().join("a.txt")),
        MockScript::text("done"),
    ]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("go"), h.token())
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(
        out.stats.cost, None,
        "the test harness wires up no budget, so nothing is tallied"
    );
}

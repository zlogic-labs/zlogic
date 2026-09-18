//! A panicking tool must not kill the turn.
//! Tools process untrusted input (web pages, MCP payloads, command output), and a parsing panic
//! inside one used to unwind through the round into the turn task — the task died silently, no
//! `TurnEnd` ever arrived, and the UI was left with a turn that could not finish and could not
//! be cancelled. `round.rs` contains tool execution with `catch_unwind`: the panic becomes an
//! ordinary failed result, and the turn continues.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::*;
use serde_json::json;
use zlogic_llm::mock::MockScript;
use zlogic_protocol::TurnId;
use zlogic_protocol::stream::{StreamPayload, ToolStatus, TurnStatus};
use zlogic_tools::{Tool, ToolCtx, ToolExecResult, ToolMeta, ToolRisk};

/// The worst a tool can do: panic the moment it runs.
struct Panics;

#[async_trait]
impl Tool for Panics {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "panics".into(),
            source: "test",
            risk: ToolRisk::Read,
        }
    }
    fn definition(&self) -> zlogic_protocol::llm::ToolDefinition {
        zlogic_protocol::llm::ToolDefinition {
            name: "panics".into(),
            description: "panics".into(),
            parameters: json!({ "type": "object" }),
        }
    }
    async fn execute(&self, _ctx: &ToolCtx, _args: &str) -> zlogic_tools::Result<ToolExecResult> {
        panic!("probe: the tool panicked")
    }
}

#[tokio::test]
async fn a_panicking_tool_is_a_failed_result_not_a_dead_turn() {
    let h = Harness::new().tool(Arc::new(Panics));
    let client = Scripted::new(vec![call("panics", "{}"), MockScript::text("done")]);

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    // The turn ended — the panic did not unwind it, and the model got to answer.
    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.answer, "done");

    // The panic became a failure the model can read, with the panic message intact.
    let results = h.tool_results();
    assert_eq!(results.len(), 1);
    assert!(results[0].0, "a panic is an error result");
    assert!(
        results[0].1.contains("probe: the tool panicked"),
        "{}",
        results[0].1
    );
    assert!(
        results[0].1.contains("panicked while running"),
        "{}",
        results[0].1
    );

    // The UI saw a terminal tool event instead of a forever-running card.
    let ends: Vec<ToolStatus> = h
        .sink
        .payloads()
        .into_iter()
        .filter_map(|p| match p {
            StreamPayload::ToolExecEnd { status, .. } => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![ToolStatus::Error]);

    // The stats count it as a failed call, not a hole.
    assert_eq!(out.stats.tools.total, 1);
    assert_eq!(out.stats.tools.failed, 1);
    assert_eq!(out.stats.rounds, 2, "the tool round and the answer round");
}

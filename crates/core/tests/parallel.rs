mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use common::*;
use serde_json::json;
use zlogic_llm::mock::MockScript;
use zlogic_protocol::TurnId;
use zlogic_protocol::stream::TurnStatus;
use zlogic_tools::{Tool, ToolCtx, ToolExecResult, ToolMeta, ToolRisk};

struct Spanning {
    spans: Arc<Mutex<Vec<(Instant, Instant)>>>,
    sleep_ms: u64,
}

#[async_trait]
impl Tool for Spanning {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "span".into(),
            source: "test",
            risk: ToolRisk::Read,
        }
    }
    fn definition(&self) -> zlogic_protocol::llm::ToolDefinition {
        zlogic_protocol::llm::ToolDefinition {
            name: "span".into(),
            description: "records its run span".into(),
            parameters: json!({ "type": "object" }),
        }
    }
    async fn execute(&self, _ctx: &ToolCtx, args: &str) -> zlogic_tools::Result<ToolExecResult> {
        let n: usize = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| v.get("n").and_then(|n| n.as_u64()).map(|n| n as usize))
            .unwrap_or(0);
        let start = Instant::now();
        {
            let mut spans = self.spans.lock().unwrap();
            spans[n] = (start, start);
        }
        tokio::time::sleep(std::time::Duration::from_millis(self.sleep_ms)).await;
        let end = Instant::now();
        self.spans.lock().unwrap()[n].1 = end;
        Ok(ToolExecResult::success("ran"))
    }
}

fn four_calls_script() -> Vec<MockScript> {
    let mut script = MockScript {
        finish: Some(zlogic_protocol::llm::FinishReason::ToolCalls),
        ..Default::default()
    };
    for i in 0..4u32 {
        script.tool_calls.push((
            i,
            format!("c{i}"),
            "span".into(),
            json!({ "n": i }).to_string(),
        ));
    }
    vec![script, MockScript::text("done")]
}

async fn run_batch(max_parallel_tools: usize) -> Vec<(Instant, Instant)> {
    let spans = Arc::new(Mutex::new(vec![(Instant::now(), Instant::now()); 4]));
    let h = Harness::new()
        .tool(Arc::new(Spanning {
            spans: spans.clone(),
            sleep_ms: 150,
        }))
        .limits(zlogic_core::Limits {
            max_parallel_tools,
            ..Default::default()
        });
    let client = Scripted::new(four_calls_script());

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("run them"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.answer, "done");
    assert_eq!(out.stats.tools.succeeded, 4, "all four calls succeed");
    assert_eq!(out.stats.rounds, 2, "a tool round plus an answer round");
    spans.lock().unwrap().clone()
}

#[tokio::test]
async fn tool_calls_in_one_round_run_concurrently() {
    let spans = run_batch(4).await;
    for (start, end) in &spans {
        assert!(
            end.duration_since(*start) >= std::time::Duration::from_millis(150),
            "the span is the wrong length: {start:?}..{end:?}"
        );
    }
    assert!(
        spans[0].1 > spans[1].0,
        "call 0 ends at {:?} and call 1 starts at {:?} — no overlap, so they never ran concurrently",
        spans[0].1,
        spans[1].0
    );
    let first_start = spans.iter().map(|s| s.0).min().unwrap();
    let last_end = spans.iter().map(|s| s.1).max().unwrap();
    assert!(
        last_end.duration_since(first_start) < std::time::Duration::from_millis(450),
        "4 × 150ms run serially is at least 600ms, measured {}ms — nothing ran in parallel",
        last_end.duration_since(first_start).as_millis()
    );
}

#[tokio::test]
async fn max_parallel_tools_1_restores_sequential_execution() {
    let spans = run_batch(1).await;
    for i in 1..spans.len() {
        assert!(
            spans[i - 1].1 <= spans[i].0,
            "with a sequential window, call {i} overlaps call {}-1: {:?} .. {:?}",
            i,
            spans[i - 1],
            spans[i]
        );
    }
}

struct AskAlways;
#[async_trait]
impl zlogic_core::PolicyGate for AskAlways {
    async fn evaluate(&self, _r: &zlogic_core::PolicyRequest) -> zlogic_core::PolicyDecision {
        zlogic_core::PolicyDecision::Ask {
            body: zlogic_protocol::interaction::InteractionBody::Form(
                zlogic_protocol::interaction::Form::new("t", Vec::new()),
            ),
        }
    }
}

struct RecordingApprover {
    spans: Arc<Mutex<Vec<(Instant, Instant)>>>,
}

#[async_trait]
impl zlogic_protocol::interaction::InteractionPort for RecordingApprover {
    async fn ask(
        &self,
        _req: zlogic_protocol::interaction::InteractionRequest,
    ) -> Result<zlogic_protocol::interaction::InteractionDecision, String> {
        let start = Instant::now();
        {
            let mut spans = self.spans.lock().unwrap();
            spans.push((start, start));
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let end = Instant::now();
        self.spans.lock().unwrap().last_mut().unwrap().1 = end;
        Ok(zlogic_protocol::interaction::InteractionDecision::Allow {
            scope: zlogic_protocol::interaction::GrantScope::Once,
            source: None,
        })
    }
}

#[tokio::test]
async fn approvals_are_serialized_even_while_execution_is_parallel() {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let h = Harness::new()
        .policy(Arc::new(AskAlways))
        .interaction(Arc::new(RecordingApprover {
            spans: spans.clone(),
        }))
        .tool(Arc::new(Spanning {
            spans: Arc::new(Mutex::new(vec![(Instant::now(), Instant::now()); 2])),
            sleep_ms: 150,
        }))
        .limits(zlogic_core::Limits {
            max_parallel_tools: 4,
            ..Default::default()
        });
    let mut script = MockScript {
        finish: Some(zlogic_protocol::llm::FinishReason::ToolCalls),
        ..Default::default()
    };
    script.tool_calls = vec![
        (0, "a1".into(), "span".into(), json!({ "n": 0 }).to_string()),
        (1, "a2".into(), "span".into(), json!({ "n": 1 }).to_string()),
    ];
    let client = Scripted::new(vec![script, MockScript::text("done")]);

    let out = h
        .core()
        .run(TurnId::new(), h.plan(client), user("both"), h.token())
        .await
        .unwrap();
    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(out.stats.tools.succeeded, 2);

    let spans = spans.lock().unwrap().clone();
    assert_eq!(spans.len(), 2, "each call asks once");
    assert!(
        spans[0].1 <= spans[1].0,
        "the two approvals overlap: {:?} .. {:?} — the approval lock is not doing its job",
        spans[0],
        spans[1]
    );
}

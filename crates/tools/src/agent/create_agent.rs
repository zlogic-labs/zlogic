//! `create_agent`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zlogic_protocol::llm::ToolDefinition;

use crate::agent::AgentRequest;
use crate::{
    Recovery, Result, Tool, ToolCtx, ToolDisplay, ToolError, ToolExecResult, ToolMeta, ToolRisk,
    parse_args,
};

#[derive(Debug, Deserialize)]
struct Args {
    agent: String,
    task: String,
    #[serde(default)]
    background: bool,
    /// Per-call role customisation. `None` = use the named profile (or the parent's defaults
    /// when the name is new and customisation is present).
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    model: Option<String>,
}

impl Args {
    fn is_customised(&self) -> bool {
        self.system.is_some() || self.tools.is_some() || self.model.is_some()
    }
}

pub struct CreateAgent;

#[async_trait]
impl Tool for CreateAgent {
    fn meta(&self) -> ToolMeta {
        // A sub-agent calls tools of its own, so it can do no less than the main agent.
        ToolMeta {
            name: "create_agent".into(),
            source: "builtin",
            risk: ToolRisk::High,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "create_agent".into(),
            description: "Hand a self-contained sub-task to another agent and get its \
                          conclusion back. Suited to work that can proceed without your \
                          step-by-step involvement. The `agent` name selects a built-in profile \
                          (`general`, `researcher`, `reviewer`, `planner`, or one configured as \
                          `agent:<name>` in llm_roles); to shape an agent yourself, pass a new \
                          name together with `system` (role instructions), `tools` (an exact \
                          allowlist) and/or `model` (a `provider:model` ref)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "minLength": 1, "description": "The sub-agent's name: a built-in profile (general / researcher / reviewer / planner) or a custom name when system/tools/model are given" },
                    "task": { "type": "string", "minLength": 1, "description": "The task; must be self-contained" },
                    "background": {
                        "type": "boolean",
                        "description": "Start the agent concurrently and return a task id immediately"
                    },
                    "system": {
                        "type": "string",
                        "description": "Role instructions appended to the profile's system prompt. With a new agent name this defines the custom agent; with a profile it refines it."
                    },
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Exact tool allowlist for this run. Replaces the profile's list entirely — anything omitted is invisible to the agent."
                    },
                    "model": {
                        "type": "string",
                        "description": "Model override as `provider:model` (or a tier / `session`). Must be resolvable with the current credentials."
                    }
                },
                "required": ["agent", "task"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let mut a: Args = parse_args(args)?;
        a.agent = a.agent.trim().to_string();
        a.task = a.task.trim().to_string();
        if a.agent.is_empty() {
            return Ok(ToolExecResult::failed("agent is required"));
        }
        if a.task.is_empty() {
            return Ok(ToolExecResult::failed("task is required"));
        }

        // No spawner means this run has no agent runtime at all. Fail closed.
        let Some(spawner) = &ctx.spawner else {
            return Err(ToolError::Unsupported(
                "create_agent requires an agent runtime",
            ));
        };

        // A name outside the profile list is fine when the caller customises the agent (system /
        // tools / model) — core falls back to the parent's base then. Only a bare unknown name is
        // an error, answered with the real list so the model can fix it without ending the turn.
        let available = spawner.available();
        if !available.iter().any(|n| n == &a.agent) && !a.is_customised() {
            return Ok(ToolExecResult::failed(format!(
                "no sub-agent named {:?}. Available: {}{}",
                a.agent,
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                },
                "; or pick a new name and pass system/tools/model to customise it",
            )));
        }

        let req = AgentRequest {
            agent: a.agent.clone(),
            task: a.task,
            parent_session_id: ctx.session_id,
            parent_turn_id: ctx.turn_id,
            exec_cwd: None,
            mailbox: None,
            anchor_call_id: ctx.call_id.clone(),
            unattended: a.background,
            cancel: ctx.cancel.clone(),
            system: a.system,
            tools: a.tools,
            model: a.model,
        };

        if a.background {
            let Some(tasks) = &ctx.tasks else {
                return Err(ToolError::Unsupported(
                    "background agents require a task runtime",
                ));
            };
            let task_id = tasks
                .start_agent(req, spawner.clone())
                .await
                .map_err(ToolError::Failed)?;
            return Ok(ToolExecResult::success(format!(
                "Sub-agent {:?} started in background as task {task_id}. You will be notified \
                 when it finishes — its conclusion arrives with the notification; do not poll.",
                a.agent
            )));
        }

        match spawner.spawn(req).await {
            Ok(outcome) => {
                // Carry the child session id so the UI can open the sub-agent's own transcript.
                // Added alongside any offload card rather than replacing it: both are useful, and
                // neither costs the model anything.
                Ok(ctx
                    .offload_if_large(&outcome.answer, Recovery::Unavailable)?
                    .with_display(ToolDisplay::Agent {
                        agent: a.agent.clone(),
                        session_id: outcome.session_id.to_string(),
                    }))
            }
            // A sub-agent failing is also something the model can work with: rephrase the task,
            // or do it directly.
            Err(e) => Ok(ToolExecResult::failed(format!(
                "sub-agent {:?} failed: {e}",
                a.agent
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentSpawner;
    use crate::{TaskHost, ToolExecStatus, test_ctx};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zlogic_task::{TaskId, TaskRun};

    #[derive(Default)]
    struct FakeSpawner {
        calls: AtomicUsize,
        fail: bool,
        answer: String,
    }

    #[async_trait]
    impl AgentSpawner for FakeSpawner {
        async fn spawn(
            &self,
            req: AgentRequest,
        ) -> std::result::Result<crate::AgentOutcome, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err("upstream blew up".into());
            }
            Ok(crate::AgentOutcome {
                session_id: zlogic_protocol::SessionId::new(),
                answer: format!("{} did: {}|{}", req.agent, req.task, self.answer),
            })
        }
        fn available(&self) -> Vec<String> {
            vec!["researcher".into(), "reviewer".into()]
        }
    }

    fn ctx_with(spawner: Arc<dyn AgentSpawner>) -> ToolCtx {
        let mut c = test_ctx(std::path::Path::new("/work"));
        c.spawner = Some(spawner);
        c
    }

    #[tokio::test]
    async fn spawns_and_returns_the_answer() {
        let s = Arc::new(FakeSpawner::default());
        let ctx = ctx_with(s.clone());
        let out = CreateAgent
            .execute(&ctx, r#"{"agent":"researcher","task":"look into X"}"#)
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains("look into X"));
        assert_eq!(s.calls.load(Ordering::Relaxed), 1);
    }

    struct FixedTasks {
        id: TaskId,
        starts: AtomicUsize,
    }

    #[async_trait]
    impl TaskHost for FixedTasks {
        async fn start_process(
            &self,
            _request: crate::ProcessRequest,
            _process: crate::SpawnedProcess,
        ) -> std::result::Result<TaskId, String> {
            unreachable!()
        }

        async fn start_agent(
            &self,
            request: AgentRequest,
            _spawner: Arc<dyn AgentSpawner>,
        ) -> std::result::Result<TaskId, String> {
            assert!(request.unattended);
            self.starts.fetch_add(1, Ordering::Relaxed);
            Ok(self.id)
        }

        async fn get(
            &self,
            _session_id: zlogic_protocol::SessionId,
            _task_id: TaskId,
        ) -> std::result::Result<Option<TaskRun>, String> {
            Ok(None)
        }

        async fn report(
            &self,
            _session_id: zlogic_protocol::SessionId,
            _task_id: TaskId,
        ) -> std::result::Result<Option<crate::TaskReport>, String> {
            Ok(None)
        }

        async fn list(
            &self,
            _session_id: zlogic_protocol::SessionId,
        ) -> std::result::Result<Vec<TaskRun>, String> {
            Ok(Vec::new())
        }

        async fn send_agent_message(
            &self,
            _session_id: zlogic_protocol::SessionId,
            _task_id: TaskId,
            _message: String,
        ) -> std::result::Result<(), String> {
            Ok(())
        }

        async fn stop(
            &self,
            _session_id: zlogic_protocol::SessionId,
            _task_id: TaskId,
        ) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn background_agent_returns_a_task_id_without_waiting_for_spawner() {
        let spawner = Arc::new(FakeSpawner::default());
        let tasks = Arc::new(FixedTasks {
            id: TaskId::new(),
            starts: AtomicUsize::new(0),
        });
        let mut ctx = ctx_with(spawner.clone());
        ctx.tasks = Some(tasks.clone());

        let out = CreateAgent
            .execute(
                &ctx,
                r#"{"agent":"reviewer","task":"review it","background":true}"#,
            )
            .await
            .unwrap();

        assert_eq!(out.status, ToolExecStatus::Success);
        assert!(out.model_text().contains(&tasks.id.to_string()));
        assert_eq!(tasks.starts.load(Ordering::Relaxed), 1);
        assert_eq!(
            spawner.calls.load(Ordering::Relaxed),
            0,
            "the task host owns the asynchronous spawn"
        );
    }

    /// No agent runtime means an explicit "unsupported", never a pretend success.
    #[tokio::test]
    async fn fails_closed_without_a_spawner() {
        let ctx = test_ctx(std::path::Path::new("/work"));
        let err = CreateAgent
            .execute(&ctx, r#"{"agent":"researcher","task":"x"}"#)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Unsupported(_)));
    }

    #[tokio::test]
    async fn an_unknown_agent_name_lists_the_available_ones() {
        let ctx = ctx_with(Arc::new(FakeSpawner::default()));
        let out = CreateAgent
            .execute(&ctx, r#"{"agent":"nonexistent","task":"x"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("researcher"));
        assert!(out.model_text().contains("reviewer"));
    }

    /// An unknown name is fine when the caller customises the agent — that is the whole point of
    /// the custom-agent path. The request must carry the customisation through.
    #[tokio::test]
    async fn an_unknown_name_with_customisation_is_forwarded() {
        struct Capture(std::sync::Mutex<Option<AgentRequest>>);
        #[async_trait]
        impl AgentSpawner for Capture {
            async fn spawn(
                &self,
                req: AgentRequest,
            ) -> std::result::Result<crate::AgentOutcome, String> {
                *self.0.lock().unwrap() = Some(req);
                Ok(crate::AgentOutcome {
                    session_id: zlogic_protocol::SessionId::new(),
                    answer: "done".into(),
                })
            }
            fn available(&self) -> Vec<String> {
                vec!["researcher".into(), "reviewer".into()]
            }
        }

        let cap = Arc::new(Capture(std::sync::Mutex::new(None)));
        let ctx = ctx_with(cap.clone());
        let out = CreateAgent
            .execute(
                &ctx,
                r#"{"agent":"security-auditor","task":"t","system":"be strict","tools":["read_file","grep"],"model":"provider:m1"}"#,
            )
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Success);

        let got = cap.0.lock().unwrap().clone().unwrap();
        assert_eq!(got.agent, "security-auditor");
        assert_eq!(got.system.as_deref(), Some("be strict"));
        assert_eq!(
            got.tools.as_deref(),
            Some(vec!["read_file".to_string(), "grep".to_string()].as_slice())
        );
        assert_eq!(got.model.as_deref(), Some("provider:m1"));
        assert!(got.is_customised());
    }

    #[tokio::test]
    async fn sub_agent_failure_is_reported_to_the_model() {
        let ctx = ctx_with(Arc::new(FakeSpawner {
            fail: true,
            ..Default::default()
        }));
        let out = CreateAgent
            .execute(&ctx, r#"{"agent":"researcher","task":"x"}"#)
            .await
            .unwrap();
        assert_eq!(out.status, ToolExecStatus::Failed);
        assert!(out.model_text().contains("upstream blew up"));
    }

    /// The parent link and anchor must reach core verbatim — the UI drills down through them.
    #[tokio::test]
    async fn parent_linkage_is_passed_through() {
        struct Capture(std::sync::Mutex<Option<AgentRequest>>);
        #[async_trait]
        impl AgentSpawner for Capture {
            async fn spawn(
                &self,
                req: AgentRequest,
            ) -> std::result::Result<crate::AgentOutcome, String> {
                *self.0.lock().unwrap() = Some(req);
                Ok(crate::AgentOutcome {
                    session_id: zlogic_protocol::SessionId::new(),
                    answer: "done".into(),
                })
            }
            fn available(&self) -> Vec<String> {
                vec!["researcher".into()]
            }
        }

        let cap = Arc::new(Capture(std::sync::Mutex::new(None)));
        let ctx = ctx_with(cap.clone());
        let (sid, tid) = (ctx.session_id, ctx.turn_id);
        CreateAgent
            .execute(&ctx, r#"{"agent":"researcher","task":"t"}"#)
            .await
            .unwrap();

        let got = cap.0.lock().unwrap().clone().unwrap();
        assert_eq!(got.parent_session_id, sid);
        assert_eq!(got.parent_turn_id, tid);
        assert_eq!(got.anchor_call_id.as_str(), "call_test");
    }

    #[tokio::test]
    async fn large_answers_are_offloaded() {
        let s = Arc::new(FakeSpawner {
            answer: "long".repeat(500),
            ..Default::default()
        });
        let ctx = ctx_with(s);
        let out = CreateAgent
            .execute(&ctx, r#"{"agent":"researcher","task":"t"}"#)
            .await
            .unwrap();
        assert!(
            !out.objects.is_empty(),
            "a long answer still goes to the object store"
        );
        // Both cards are present: the expandable output and the agent link.
        assert!(
            out.display
                .iter()
                .any(|d| matches!(d, ToolDisplay::Output { .. }))
        );
        assert!(
            out.display
                .iter()
                .any(|d| matches!(d, ToolDisplay::Agent { .. }))
        );
        assert!(out.dangling_display_objects().is_empty());
    }

    /// The child session id must reach the result, or the sub-agent's transcript is persisted
    /// but unreachable from the parent timeline.
    #[tokio::test]
    async fn the_child_session_id_reaches_the_display() {
        let ctx = ctx_with(Arc::new(FakeSpawner::default()));
        let out = CreateAgent
            .execute(&ctx, r#"{"agent":"researcher","task":"t"}"#)
            .await
            .unwrap();
        match out
            .display
            .iter()
            .find(|d| matches!(d, ToolDisplay::Agent { .. }))
            .unwrap()
        {
            ToolDisplay::Agent { agent, session_id } => {
                assert_eq!(agent, "researcher");
                assert!(session_id.parse::<zlogic_protocol::SessionId>().is_ok());
            }
            other => panic!("expected an agent card, got {other:?}"),
        }
    }
}

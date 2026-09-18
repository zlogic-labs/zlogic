//! Tools for observing and controlling durable task runs.
//! The trait lives here, while its implementation lives in engine. This is the same dependency
//! boundary as [`crate::WorktreeHost`]: tools describe the capability, but never own process-wide
//! state or SQLite connections.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::{Child, ChildStderr, ChildStdout};
use zlogic_protocol::llm::ToolDefinition;
use zlogic_protocol::{CallId, SessionId, TurnId};
use zlogic_task::{ExecutorSpec, ProcessSpec, TaskId, TaskResult, TaskRun, TaskState};

use crate::{
    AgentRequest, AgentSpawner, CancellationToken, Result, Tool, ToolCtx, ToolError,
    ToolExecResult, ToolMeta, ToolRisk, parse_args,
};

/// A shell process that has already passed policy and been spawned by the shell tool.
/// Moving the live handles into TaskHost is what lets `background: true` hand the already-running
/// child over without killing and re-running it.
pub struct ProcessRequest {
    pub spec: ProcessSpec,
    pub parent_session_id: SessionId,
    pub parent_turn_id: TurnId,
    pub anchor_call_id: CallId,
    pub cancel: CancellationToken,
    /// Whether the creating turn should wait for this run before ending.
    /// The shell tool sets it from its long-running heuristic: a command expected to terminate
    /// (compile, test) is `true`; a recognised server / watcher is `false`. Scheduled jobs and
    /// background agents always pass `false`.
    pub turn_scoped: bool,
}

pub struct SpawnedProcess {
    pub child: Child,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// One run as `task_get` reports it: the task row plus the output window that belongs to it.
/// The window is read from the runtime's spool rather than stored on the row, so a *running*
/// process task reports what it has printed so far — which is the question `task_get` is usually
/// asked ("is it still going, and how far did it get").
pub struct TaskReport {
    pub task: TaskRun,
    /// Head+tail of a process task's output; `None` for an agent run (its transcript is the child
    /// session) and for one that has printed nothing yet.
    pub output: Option<String>,
}

#[async_trait]
pub trait TaskHost: Send + Sync {
    /// Adopts an already-running shell process and returns after its durable runtime is registered.
    async fn start_process(
        &self,
        request: ProcessRequest,
        process: SpawnedProcess,
    ) -> std::result::Result<TaskId, String>;

    /// Starts a persisted background agent and returns after the runtime has been registered.
    /// The host combines `request.cancel` with the task's own token. Cancelling the launching
    /// conversation or calling `task_stop` therefore reaches the same agent run.
    async fn start_agent(
        &self,
        request: AgentRequest,
        spawner: Arc<dyn AgentSpawner>,
    ) -> std::result::Result<TaskId, String>;

    async fn get(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> std::result::Result<Option<TaskRun>, String>;

    /// One run with its output window — what a completion notification carries, on demand.
    /// Separate from [`Self::get`] because reading a possibly large spool is not something a
    /// control path (`stop`, `send_agent_message`) should pay for.
    async fn report(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> std::result::Result<Option<TaskReport>, String>;

    async fn list(&self, session_id: SessionId) -> std::result::Result<Vec<TaskRun>, String>;

    /// Injects guidance into a still-running background agent at its next safe checkpoint.
    async fn send_agent_message(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        message: String,
    ) -> std::result::Result<(), String>;

    /// Stops one run. A terminal run is a no-op.
    async fn stop(&self, session_id: SessionId, task_id: TaskId)
    -> std::result::Result<(), String>;
}

pub struct TaskGet;
pub struct TaskStop;
pub struct TaskMessage;

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(TaskGet), Arc::new(TaskStop), Arc::new(TaskMessage)]
}

#[async_trait]
impl Tool for TaskGet {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "task_get".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_get".into(),
            description: "Read one background task: its state, what it is running, and the \
                          output it has produced so far (head and tail). Not a way to wait — a \
                          completion notification arrives on its own, and polling this is not a \
                          substitute for it."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "minLength": 1, "description": "Task id returned when the background work started" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let args: TaskArgs = parse_args(args)?;
        let task_id = parse_task_id(&args.task_id)?;
        let Some(report) = host(ctx)?
            .report(ctx.session_id, task_id)
            .await
            .map_err(ToolError::Failed)?
        else {
            return Ok(ToolExecResult::failed(format!(
                "no task {task_id} in this conversation"
            )));
        };
        Ok(ToolExecResult::success(describe(&report)))
    }
}

/// One task, as the model needs to read it: identity and state first, then the output window.
fn describe(report: &TaskReport) -> String {
    let task = &report.task;
    let mut lines = vec![format!("task: {}", task.task_id)];
    if let Some(job_id) = &task.job_id {
        lines.push(format!("job: {job_id}"));
    }
    lines.push(format!("state: {}", task.state.as_str()));
    lines.push(match &task.executor {
        ExecutorSpec::Process(spec) => {
            let mut command = spec.program.clone();
            if !spec.args.is_empty() {
                command.push(' ');
                command.push_str(&spec.args.join(" "));
            }
            format!("executor: process — {command}")
        }
        ExecutorSpec::Agent(spec) => format!("executor: agent — {}", spec.agent),
    });
    if let ExecutorSpec::Process(spec) = &task.executor
        && let Some(cwd) = &spec.cwd
    {
        lines.push(format!("cwd: {cwd}"));
    }
    lines.push(format!(
        "started: {}",
        match task.started_at {
            Some(at) => at.to_rfc3339(),
            None => "not yet".into(),
        }
    ));
    if let Some(ended) = task.finished_at {
        lines.push(format!("finished: {}", ended.to_rfc3339()));
    }
    match (&task.result, &task.error) {
        (Some(TaskResult::Process(result)), _) => {
            lines.push(format!(
                "exit: {}",
                match result.exit_code {
                    Some(code) => code.to_string(),
                    None => "no exit code (killed by a signal)".into(),
                }
            ));
            lines.push(format!("output: {} characters", result.output_chars));
            if result.output_object_id.is_some() {
                // Said out loud because the model cannot read it: the object store is the UI's
                // ("view full output"), not a tool.
                lines.push(
                    "note: the full transcript is kept for the user; only the window below is \
                     readable here."
                        .into(),
                );
            }
        }
        (Some(TaskResult::Agent(result)), _) => {
            lines.push(format!("child session: {}", result.child_session_id));
            if let Some(conclusion) = &result.conclusion {
                lines.push(format!("\n--- conclusion ---\n{conclusion}"));
            }
        }
        _ => {}
    }
    if let Some(error) = &task.error {
        lines.push(format!("error: {error}"));
    }
    match task.state {
        TaskState::Running => lines.push(
            "note: still running. Do not poll it: the completion notification arrives on its own."
                .into(),
        ),
        TaskState::Queued => lines.push("note: queued; it has not started yet.".into()),
        TaskState::NeedsInput => {
            lines.push("note: it is waiting for an answer only a person can give.".into())
        }
        // Terminal: the result above is the whole answer.
        TaskState::Succeeded
        | TaskState::Failed
        | TaskState::Cancelled
        | TaskState::Interrupted => {}
    }
    let mut body = lines.join("\n");
    match &report.output {
        Some(output) => body.push_str(&format!("\n\n--- output (head and tail) ---\n{output}")),
        None if matches!(task.executor, ExecutorSpec::Process(_)) => {
            body.push_str("\n\n--- output (empty so far) ---");
        }
        None => {}
    }
    body
}

#[async_trait]
impl Tool for TaskStop {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "task_stop".into(),
            source: "builtin",
            risk: ToolRisk::Write,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_stop".into(),
            description: "Stop one background task. A task that already ended is unchanged.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "minLength": 1, "description": "Task id returned when the background work started" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let args: TaskArgs = parse_args(args)?;
        let task_id = parse_task_id(&args.task_id)?;
        host(ctx)?
            .stop(ctx.session_id, task_id)
            .await
            .map_err(ToolError::Failed)?;
        Ok(ToolExecResult::success(format!(
            "Stop requested for background task {task_id}."
        )))
    }
}

#[async_trait]
impl Tool for TaskMessage {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "task_message".into(),
            source: "builtin",
            risk: ToolRisk::Write,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_message".into(),
            description: "Send additional guidance to a running background agent. The message is \
                          injected at the agent's next safe model-round checkpoint."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "minLength": 1 },
                    "message": { "type": "string", "minLength": 1 }
                },
                "required": ["task_id", "message"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let args: TaskMessageArgs = parse_args(args)?;
        let task_id = parse_task_id(&args.task_id)?;
        let message = args.message.trim();
        if message.is_empty() {
            return Ok(ToolExecResult::failed("message is required"));
        }
        host(ctx)?
            .send_agent_message(ctx.session_id, task_id, message.to_string())
            .await
            .map_err(ToolError::Failed)?;
        Ok(ToolExecResult::success(format!(
            "Message queued for background agent task {task_id}."
        )))
    }
}

#[derive(Debug, Deserialize)]
struct TaskArgs {
    task_id: String,
}

#[derive(Debug, Deserialize)]
struct TaskMessageArgs {
    task_id: String,
    message: String,
}

fn host(ctx: &ToolCtx) -> Result<&Arc<dyn TaskHost>> {
    ctx.tasks
        .as_ref()
        .ok_or(ToolError::Unsupported("background task runtime"))
}

fn parse_task_id(value: &str) -> Result<TaskId> {
    value
        .parse()
        .map_err(|_| ToolError::BadArgs(format!("invalid task_id: {value:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use zlogic_protocol::WorkspaceId;
    use zlogic_task::{PermissionPolicy, ProcessResult, TaskTrigger};

    fn process_task(state: TaskState, result: Option<TaskResult>, error: Option<&str>) -> TaskRun {
        let now = Utc::now();
        TaskRun {
            task_id: TaskId::new(),
            job_id: None,
            workspace_id: WorkspaceId::new(),
            executor: ExecutorSpec::Process(ProcessSpec {
                program: "/bin/bash".into(),
                args: vec!["-c".into(), "cargo test --workspace".into()],
                cwd: Some("/repo".into()),
                env: std::collections::BTreeMap::new(),
            }),
            permission_policy: PermissionPolicy::RequireInteraction,
            notification_session_id: None,
            trigger: TaskTrigger::Tool {
                session_id: zlogic_protocol::SessionId::new(),
                turn_id: TurnId::new(),
                call_id: CallId::new("call_test"),
            },
            turn_scoped: true,
            state,
            attempt: 1,
            scheduled_for: None,
            started_at: Some(now),
            finished_at: state.is_terminal().then_some(now),
            result,
            error: error.map(str::to_string),
            created_at: now,
            updated_at: now,
        }
    }

    /// A running task is reported as such, with the output it has produced so far — that is the
    /// whole point of reading one mid-flight, and the model must not be left thinking it finished.
    #[test]
    fn a_running_task_is_reported_with_its_partial_output() {
        let report = TaskReport {
            task: process_task(TaskState::Running, None, None),
            output: Some("Compiling zlogic-store v0.9.9".into()),
        };
        let text = describe(&report);
        assert!(text.contains("state: running"), "{text}");
        assert!(text.contains("cargo test --workspace"), "{text}");
        assert!(text.contains("cwd: /repo"), "{text}");
        assert!(text.contains("Compiling zlogic-store"), "{text}");
        assert!(text.contains("still running"), "{text}");
    }

    /// A finished task leads with the result, and says that the fuller transcript is the user's —
    /// the model cannot open an object, so it must not be sent looking for one.
    #[test]
    fn a_finished_task_reports_its_exit_code_and_what_it_cannot_read() {
        let report = TaskReport {
            task: process_task(
                TaskState::Succeeded,
                Some(TaskResult::Process(ProcessResult {
                    exit_code: Some(0),
                    output_object_id: Some(
                        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                            .parse()
                            .unwrap(),
                    ),
                    output_chars: 4_211,
                })),
                None,
            ),
            output: Some("hello".into()),
        };
        let text = describe(&report);
        assert!(text.contains("state: succeeded"), "{text}");
        assert!(text.contains("exit: 0"), "{text}");
        assert!(text.contains("output: 4211 characters"), "{text}");
        assert!(text.contains("only the window below is"), "{text}");
        assert!(!text.contains("still running"), "{text}");
    }

    /// No output yet is not the same as no output channel: a process task says so instead of
    /// looking like an agent run (which has no spool at all).
    #[test]
    fn an_empty_process_task_says_its_output_is_empty_rather_than_absent() {
        let report = TaskReport {
            task: process_task(TaskState::Running, None, None),
            output: None,
        };
        assert!(describe(&report).contains("output (empty so far)"));
    }

    /// The stop and message tools keep the exact task-id parsing rule, including its message.
    #[test]
    fn a_bad_task_id_is_a_bad_argument_not_a_failure() {
        let error = parse_task_id("not-a-task-id").unwrap_err();
        assert!(matches!(error, ToolError::BadArgs(_)), "{error:?}");
    }
}

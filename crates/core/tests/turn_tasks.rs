//! Turn-end wait for shell background tasks.
//! A compile / test the model deliberately put in the background (`background: true`) is usually
//! something the turn still wants the result of. These tests drive the wait policy through a stub
//! [`TaskHost`]: tasks the turn
//! created (`TaskTrigger::Tool` with its turn id, `turn_scoped`, process executor) are waited on
//! within the budget; what is still running after the budget is left running and reported through
//! a non-persisted notice — no popup asking the user.

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use common::*;
use zlogic_core::Limits;
use zlogic_llm::mock::MockScript;
use zlogic_protocol::interaction::{FormAnswer, InteractionDecision};
use zlogic_protocol::stream::TurnStatus;
use zlogic_protocol::{CallId, SessionId, TurnId, WorkspaceId};
use zlogic_store::EntryKind;
use zlogic_task::{
    ExecutorSpec, PermissionPolicy, ProcessResult, ProcessSpec, TaskId, TaskResult, TaskRun,
    TaskState, TaskTrigger,
};
use zlogic_tools::{AgentRequest, AgentSpawner, ProcessRequest, SpawnedProcess, TaskHost};

/// A scripted task runtime: `list` returns whatever rows the test put in, and records how often
/// it was asked and which tasks were stopped.
struct StubTasks {
    state: Mutex<Vec<TaskRun>>,
    lists: AtomicUsize,
    stopped: Mutex<Vec<TaskId>>,
}

impl StubTasks {
    fn new(tasks: Vec<TaskRun>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(tasks),
            lists: AtomicUsize::new(0),
            stopped: Mutex::new(Vec::new()),
        })
    }

    fn lists(&self) -> usize {
        self.lists.load(Ordering::SeqCst)
    }

    fn stopped(&self) -> Vec<TaskId> {
        self.stopped.lock().unwrap().clone()
    }

    fn finish_all(&self) {
        let mut state = self.state.lock().unwrap();
        let now = Utc::now();
        for task in state.iter_mut() {
            if !task.state.is_terminal() {
                task.state = TaskState::Succeeded;
                task.finished_at = Some(now);
                task.result = Some(TaskResult::Process(ProcessResult {
                    exit_code: Some(0),
                    output_object_id: None,
                    output_chars: 0,
                }));
            }
        }
    }
}

#[async_trait]
impl TaskHost for StubTasks {
    async fn start_process(
        &self,
        _request: ProcessRequest,
        _process: SpawnedProcess,
    ) -> Result<TaskId, String> {
        unreachable!("no tool runs in these tests")
    }

    async fn start_agent(
        &self,
        _request: AgentRequest,
        _spawner: Arc<dyn AgentSpawner>,
    ) -> Result<TaskId, String> {
        unreachable!("no tool runs in these tests")
    }

    async fn get(
        &self,
        _session_id: SessionId,
        task_id: TaskId,
    ) -> Result<Option<TaskRun>, String> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .iter()
            .find(|task| task.task_id == task_id)
            .cloned())
    }

    async fn report(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<Option<zlogic_tools::TaskReport>, String> {
        // These tests never call task_get; the row is enough to satisfy the trait.
        Ok(self
            .get(session_id, task_id)
            .await?
            .map(|task| zlogic_tools::TaskReport { task, output: None }))
    }

    async fn list(&self, _session_id: SessionId) -> Result<Vec<TaskRun>, String> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(self.state.lock().unwrap().clone())
    }

    async fn send_agent_message(
        &self,
        _session_id: SessionId,
        _task_id: TaskId,
        _message: String,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn stop(&self, _session_id: SessionId, task_id: TaskId) -> Result<(), String> {
        self.stopped.lock().unwrap().push(task_id);
        Ok(())
    }
}

/// One still-running process task, created by `turn_id` — the shape the wait policy waits on.
fn running_process_task(session_id: SessionId, turn_id: TurnId, turn_scoped: bool) -> TaskRun {
    let now = Utc::now();
    TaskRun {
        task_id: TaskId::new(),
        job_id: None,
        workspace_id: WorkspaceId::new(),
        executor: ExecutorSpec::Process(ProcessSpec {
            program: "cargo".into(),
            args: vec!["test".into()],
            cwd: None,
            env: BTreeMap::new(),
        }),
        permission_policy: PermissionPolicy::DenyRequests,
        notification_session_id: Some(session_id),
        trigger: TaskTrigger::Tool {
            session_id,
            turn_id,
            call_id: CallId::new("call_bg"),
        },
        turn_scoped,
        state: TaskState::Running,
        attempt: 1,
        scheduled_for: None,
        started_at: Some(now),
        finished_at: None,
        result: None,
        error: None,
        created_at: now,
        updated_at: now,
    }
}

/// The turn waits for its own background task within the budget, and the result stays in the same
/// turn: the model was asked exactly once.
#[tokio::test]
async fn a_turn_waits_for_its_own_background_task() {
    let h = Harness::new().limits(Limits {
        task_wait_secs: 5,
        ..Default::default()
    });
    let turn = TurnId::new();
    let tasks = StubTasks::new(vec![running_process_task(h.session, turn, true)]);
    let h = h.tasks(tasks.clone());

    {
        let tasks = tasks.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tasks.finish_all();
        });
    }

    let client = Scripted::new(vec![MockScript::text("done")]);
    let out = h
        .core()
        .run(turn, h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(
        client.request_count(),
        1,
        "the result landed in the same round, so no extra round may be run"
    );
    assert!(
        tasks.lists() >= 2,
        "the turn really did poll task status as it closed out"
    );
}

/// Budget exhausted while the task is still running: it is left running (never silently killed)
/// and a **non-persisted** notice tells the user it will notify on completion; the turn ends
/// without an extra round and without a popup.
#[tokio::test]
async fn a_task_still_running_after_the_budget_is_left_running_and_reported() {
    let h = Harness::new().limits(Limits {
        task_wait_secs: 1,
        ..Default::default()
    });
    let turn = TurnId::new();
    let tasks = StubTasks::new(vec![running_process_task(h.session, turn, true)]);
    let h = h.tasks(tasks.clone());

    let client = Scripted::new(vec![MockScript::text("first")]);
    let out = h
        .core()
        .run(turn, h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert!(
        tasks.stopped().is_empty(),
        "headless must not silently kill the task"
    );
    assert_eq!(
        client.request_count(),
        1,
        "no popup, and no extra round making the model restate that the task is still running"
    );
    assert!(
        !h.entries()
            .iter()
            .any(|e| e.kind == EntryKind::Steering && e.data.to_string().contains("still running")),
        "the timeline must not carry a persisted still-running notice"
    );
    assert!(
        h.notices()
            .iter()
            .any(|(code, text)| code == "tasks_still_running" && text.contains("still running")),
        "the stream carries a transient still-running notice"
    );
}

#[tokio::test]
async fn no_popup_when_tasks_outlive_the_budget() {
    let answers = Answers::new(InteractionDecision::Submitted(FormAnswer::single_choice(
        "action", "kill",
    )));
    let h = Harness::new()
        .limits(Limits {
            task_wait_secs: 1,
            ..Default::default()
        })
        .interaction(answers.clone());
    let turn = TurnId::new();
    let tasks = StubTasks::new(vec![running_process_task(h.session, turn, true)]);
    let h = h.tasks(tasks.clone());

    let client = Scripted::new(vec![MockScript::text("first")]);
    let out = h
        .core()
        .run(turn, h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(
        answers.count(),
        0,
        "with the budget exhausted, the user must not be asked again"
    );
    assert!(tasks.stopped().is_empty(), "the task stays running");
    assert_eq!(client.request_count(), 1);
}

/// `task_wait_secs: 0` turns the whole feature off: no wait, no poll, no extra round.
#[tokio::test]
async fn task_wait_can_be_turned_off() {
    let h = Harness::new().limits(Limits {
        task_wait_secs: 0,
        ..Default::default()
    });
    let turn = TurnId::new();
    let tasks = StubTasks::new(vec![running_process_task(h.session, turn, true)]);
    let h = h.tasks(tasks.clone());

    let client = Scripted::new(vec![MockScript::text("done")]);
    let out = h
        .core()
        .run(turn, h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(client.request_count(), 1);
    assert_eq!(
        tasks.lists(),
        0,
        "with wait turned off, not even the task list may be queried"
    );
}

/// Servers / watchers (`turn_scoped = false`) are never waited on: the turn ends immediately.
#[tokio::test]
async fn servers_are_never_waited_on() {
    let h = Harness::new().limits(Limits {
        task_wait_secs: 5,
        ..Default::default()
    });
    let turn = TurnId::new();
    let tasks = StubTasks::new(vec![running_process_task(h.session, turn, false)]);
    let h = h.tasks(tasks.clone());

    let client = Scripted::new(vec![MockScript::text("done")]);
    let out = h
        .core()
        .run(turn, h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(client.request_count(), 1);
    assert_eq!(
        tasks.lists(),
        1,
        "one query is enough to know there is nothing to wait for, so no wait loop is entered"
    );
}

/// Tasks created by an earlier turn are not this turn's business.
#[tokio::test]
async fn tasks_from_other_turns_are_not_waited_on() {
    let h = Harness::new().limits(Limits {
        task_wait_secs: 5,
        ..Default::default()
    });
    let tasks = StubTasks::new(vec![running_process_task(h.session, TurnId::new(), true)]);
    let h = h.tasks(tasks.clone());

    let client = Scripted::new(vec![MockScript::text("done")]);
    let out = h
        .core()
        .run(TurnId::new(), h.plan(client.clone()), user("go"), h.token())
        .await
        .unwrap();

    assert_eq!(out.status, TurnStatus::Completed);
    assert_eq!(client.request_count(), 1);
    assert_eq!(
        tasks.lists(),
        1,
        "another turn's tasks are outside the waiting scope"
    );
}
